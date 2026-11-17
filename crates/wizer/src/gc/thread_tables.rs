use std::collections::BTreeMap;
use wasm_encoder::{
    self as e,
    reencode::{self, Reencode, ReencodeComponent},
};
use wasmparser as p;
use wasmtime::Result;

type Error = reencode::Error<wasmtime::Error>;

#[derive(Clone)]
struct Table {
    types: Vec<u8>,
    count: u32,
    ty: p::TableType,
}

pub fn adapt(bytes: &[u8]) -> Result<Vec<u8>> {
    let mut rewriter = Adapter::default();
    let mut output = e::Component::new();
    rewriter
        .parse_component(&mut output, p::Parser::new(0), bytes)
        .map_err(|error| wasmtime::format_err!("{error}"))?;
    Ok(output.finish())
}

#[derive(Default)]
struct Adapter {
    modules: Vec<u32>,
    instances: Vec<u32>,
    functions: Vec<u32>,
    tables: Vec<Table>,
    table_indexes: Vec<u32>,
    module_tables: Vec<BTreeMap<String, Table>>,
    instance_tables: Vec<BTreeMap<String, Table>>,
    next_module: u32,
    next_instance: u32,
    next_function: u32,
    next_table: u32,
}

impl Reencode for Adapter {
    type Error = wasmtime::Error;
    fn function_index(&mut self, index: u32) -> Result<u32, Error> {
        Ok(self.functions[index as usize])
    }
    fn table_index(&mut self, index: u32) -> Result<u32, Error> {
        Ok(self.table_indexes[index as usize])
    }
}

impl ReencodeComponent for Adapter {
    fn module_index(&mut self, index: u32) -> u32 {
        self.modules[index as usize]
    }
    fn instance_index(&mut self, index: u32) -> u32 {
        self.instances[index as usize]
    }

    fn parse_component_submodule(
        &mut self,
        dst: &mut e::Component,
        _: p::Parser,
        bytes: &[u8],
    ) -> Result<(), Error> {
        self.module_tables
            .push(module_tables(bytes).map_err(Error::UserError)?);
        self.modules.push(self.next_module);
        self.next_module += 1;
        dst.section(&e::RawSection {
            id: e::ComponentSectionId::CoreModule as u8,
            data: bytes,
        });
        Ok(())
    }

    fn parse_component_subcomponent(
        &mut self,
        dst: &mut e::Component,
        _: p::Parser,
        bytes: &[u8],
        _: &[u8],
    ) -> Result<(), Error> {
        dst.section(&e::RawSection {
            id: e::ComponentSectionId::Component as u8,
            data: bytes,
        });
        Ok(())
    }

    fn parse_instance(
        &mut self,
        dst: &mut e::InstanceSection,
        instance: p::Instance<'_>,
    ) -> Result<(), Error> {
        let tables = match &instance {
            p::Instance::Instantiate { module_index, .. } => {
                self.module_tables[*module_index as usize].clone()
            }
            p::Instance::FromExports(exports) => exports
                .iter()
                .filter(|export| export.kind == p::ExternalKind::Table)
                .map(|export| {
                    (
                        export.name.to_owned(),
                        self.tables[export.index as usize].clone(),
                    )
                })
                .collect(),
        };
        reencode::component_utils::parse_instance(self, dst, instance)?;
        self.instance_tables.push(tables);
        self.instances.push(self.next_instance);
        self.next_instance += 1;
        Ok(())
    }

    fn parse_component_alias_section(
        &mut self,
        dst: &mut e::ComponentAliasSection,
        section: p::ComponentAliasSectionReader<'_>,
    ) -> Result<(), Error> {
        for alias in section {
            let alias = alias?;
            dst.alias(self.component_alias(alias.clone())?);
            match alias {
                p::ComponentAlias::CoreInstanceExport {
                    kind: p::ExternalKind::Func | p::ExternalKind::FuncExact,
                    ..
                } => {
                    self.functions.push(self.next_function);
                    self.next_function += 1;
                }
                p::ComponentAlias::CoreInstanceExport {
                    kind: p::ExternalKind::Table,
                    instance_index,
                    name,
                } => {
                    self.tables
                        .push(self.instance_tables[instance_index as usize][name].clone());
                    self.table_indexes.push(self.next_table);
                    self.next_table += 1;
                }
                _ => {}
            }
        }
        Ok(())
    }

    fn parse_component_payload(
        &mut self,
        dst: &mut e::Component,
        payload: p::Payload<'_>,
        whole: &[u8],
    ) -> Result<(), Error> {
        if let p::Payload::ComponentCanonicalSection(section) = payload {
            for function in section {
                let function = function?;
                if let p::CanonicalFunction::ThreadNewIndirect {
                    func_ty_index,
                    table_index,
                } = function
                {
                    self.thread(dst, func_ty_index, table_index)?;
                } else {
                    let lift = matches!(function, p::CanonicalFunction::Lift { .. });
                    let mut section = e::CanonicalFunctionSection::new();
                    self.parse_component_canonical(&mut section, function)?;
                    dst.section(&section);
                    if !lift {
                        self.functions.push(self.next_function);
                        self.next_function += 1;
                    }
                }
            }
            Ok(())
        } else {
            reencode::component_utils::parse_component_payload(self, dst, payload, whole)
        }
    }
}

impl Adapter {
    fn thread(
        &mut self,
        dst: &mut e::Component,
        function_type: u32,
        table: u32,
    ) -> Result<(), Error> {
        let bridge = bridge(&self.tables[table as usize]).map_err(Error::UserError)?;
        dst.section(&e::RawSection {
            id: e::ComponentSectionId::CoreModule as u8,
            data: &bridge,
        });
        let bridge_module = self.next_module;
        self.next_module += 1;
        let mut instances = e::InstanceSection::new();
        instances.export_items([(
            "table",
            e::ExportKind::Table,
            self.table_indexes[table as usize],
        )]);
        instances.instantiate(
            bridge_module,
            [("source", e::ModuleArg::Instance(self.next_instance))],
        );
        let bridge_instance = self.next_instance + 1;
        self.next_instance += 2;
        dst.section(&instances);
        let mut aliases = e::ComponentAliasSection::new();
        aliases.alias(e::Alias::CoreInstanceExport {
            instance: bridge_instance,
            kind: e::ExportKind::Table,
            name: "raw",
        });
        aliases.alias(e::Alias::CoreInstanceExport {
            instance: bridge_instance,
            kind: e::ExportKind::Func,
            name: "set",
        });
        let setter = self.next_function;
        self.next_function += 1;
        dst.section(&aliases);
        let mut canon = e::CanonicalFunctionSection::new();
        canon.thread_new_indirect(function_type, self.next_table);
        self.next_table += 1;
        dst.section(&canon);
        let raw_new = self.next_function;
        self.next_function += 1;
        let wrapper = wrapper();
        dst.section(&e::RawSection {
            id: e::ComponentSectionId::CoreModule as u8,
            data: &wrapper,
        });
        let wrapper_module = self.next_module;
        self.next_module += 1;
        let mut instances = e::InstanceSection::new();
        instances.export_items([
            ("set", e::ExportKind::Func, setter),
            ("new", e::ExportKind::Func, raw_new),
        ]);
        instances.instantiate(
            wrapper_module,
            [("source", e::ModuleArg::Instance(self.next_instance))],
        );
        let wrapper_instance = self.next_instance + 1;
        self.next_instance += 2;
        dst.section(&instances);
        let mut aliases = e::ComponentAliasSection::new();
        aliases.alias(e::Alias::CoreInstanceExport {
            instance: wrapper_instance,
            kind: e::ExportKind::Func,
            name: "new",
        });
        dst.section(&aliases);
        self.functions.push(self.next_function);
        self.next_function += 1;
        Ok(())
    }
}

fn module_tables(bytes: &[u8]) -> Result<BTreeMap<String, Table>> {
    let (mut types, mut count, mut tables, mut exports) = (Vec::new(), 0, Vec::new(), Vec::new());
    for payload in p::Parser::new(0).parse_all(bytes) {
        match payload? {
            p::Payload::TypeSection(reader) => {
                let range = reader.range();
                types = bytes[range.start as usize..range.end as usize].to_vec();
                for group in reader {
                    count += group?.types().len() as u32;
                }
            }
            p::Payload::ImportSection(reader) => {
                for import in reader.into_imports() {
                    if let p::TypeRef::Table(ty) = import?.ty {
                        tables.push(ty);
                    }
                }
            }
            p::Payload::TableSection(reader) => {
                for table in reader {
                    tables.push(table?.ty);
                }
            }
            p::Payload::ExportSection(reader) => {
                for export in reader {
                    let export = export?;
                    if export.kind == p::ExternalKind::Table {
                        exports.push((export.name.to_owned(), export.index));
                    }
                }
            }
            _ => {}
        }
    }
    Ok(exports
        .into_iter()
        .map(|(name, index)| {
            (
                name,
                Table {
                    types: types.clone(),
                    count,
                    ty: tables[index as usize],
                },
            )
        })
        .collect())
}

fn bridge(table: &Table) -> Result<Vec<u8>> {
    use e::Instruction as I;
    let mut module = e::Module::new();
    let reader = p::TypeSectionReader::new(p::BinaryReader::new(&table.types, 0))?;
    let mut reencoder = reencode::RoundtripReencoder;
    let mut types = e::TypeSection::new();
    reencoder.parse_type_section(&mut types, reader)?;
    types.ty().function([e::ValType::I32], []);
    module.section(&types);
    let mut imports = e::ImportSection::new();
    imports.import(
        "source",
        "table",
        e::EntityType::Table(reencoder.table_type(table.ty)?),
    );
    module.section(&imports);
    let mut functions = e::FunctionSection::new();
    functions.function(table.count);
    module.section(&functions);
    let mut tables = e::TableSection::new();
    tables.table(e::TableType {
        element_type: e::RefType::FUNCREF,
        minimum: 1,
        maximum: Some(1),
        table64: false,
        shared: false,
    });
    module.section(&tables);
    let mut exports = e::ExportSection::new();
    exports.export("set", e::ExportKind::Func, 0);
    exports.export("raw", e::ExportKind::Table, 1);
    module.section(&exports);
    let mut function = e::Function::new([]);
    for instruction in [
        I::I32Const(0),
        I::LocalGet(0),
        I::TableGet(0),
        I::RefCastNullable(e::HeapType::Concrete(3)),
        I::StructGet {
            struct_type_index: 3,
            field_index: 1,
        },
        I::TableSet(1),
        I::End,
    ] {
        function.instruction(&instruction);
    }
    let mut code = e::CodeSection::new();
    code.function(&function);
    module.section(&code);
    Ok(module.finish())
}

fn wrapper() -> Vec<u8> {
    use e::Instruction as I;
    let mut module = e::Module::new();
    let mut types = e::TypeSection::new();
    types.ty().function([e::ValType::I32], []);
    types
        .ty()
        .function([e::ValType::I32, e::ValType::I32], [e::ValType::I32]);
    module.section(&types);
    let mut imports = e::ImportSection::new();
    imports.import("source", "set", e::EntityType::Function(0));
    imports.import("source", "new", e::EntityType::Function(1));
    module.section(&imports);
    let mut functions = e::FunctionSection::new();
    functions.function(1);
    module.section(&functions);
    let mut exports = e::ExportSection::new();
    exports.export("new", e::ExportKind::Func, 2);
    module.section(&exports);
    let mut function = e::Function::new([]);
    for instruction in [
        I::LocalGet(0),
        I::Call(0),
        I::I32Const(0),
        I::LocalGet(1),
        I::Call(1),
        I::End,
    ] {
        function.instruction(&instruction);
    }
    let mut code = e::CodeSection::new();
    code.function(&function);
    module.section(&code);
    module.finish()
}
