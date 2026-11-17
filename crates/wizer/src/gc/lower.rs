use std::borrow::Cow;
use std::collections::HashMap;
use wasm_encoder::reencode::{self, Reencode};
use wasm_encoder::{self as e, Encode};
use wasmparser::{self as p, CompositeInnerType, Operator, Payload};

type Error = reencode::Error<std::convert::Infallible>;

pub fn preserving_types(bytes: &[u8], module_id: u32) -> wasmtime::Result<Vec<u8>> {
    lower_impl(bytes, module_id, true, false)
}

/// The component initializer uses GC proxies for function-valued references.
/// The raw callable remains in the proxy alongside its module-local source
/// index. This transformation is not applied to the saved module.
pub fn component_module(bytes: &[u8], module_id: u32) -> wasmtime::Result<Vec<u8>> {
    lower_impl(bytes, module_id, false, true)
}

fn lower_impl(
    bytes: &[u8],
    module_id: u32,
    preserve_types: bool,
    shared_pool: bool,
) -> wasmtime::Result<Vec<u8>> {
    let mut groups = Vec::new();
    let mut functions = Vec::new();
    let mut operations = Vec::new();
    let mut operation_indexes = HashMap::new();
    let mut globals = Vec::new();
    let mut tables = Vec::new();
    let mut passive = Vec::new();
    let mut imported_tables = 0;
    for payload in p::Parser::new(0).parse_all(bytes) {
        match payload? {
            Payload::TypeSection(types) => {
                for group in types {
                    let group = group?;
                    groups.push((
                        group.is_explicit_rec_group(),
                        group.into_types().collect::<Vec<_>>(),
                    ));
                }
            }
            Payload::ImportSection(imports) => {
                for import in imports.into_imports() {
                    match import?.ty {
                        p::TypeRef::Func(ty) | p::TypeRef::FuncExact(ty) => functions.push(ty),
                        p::TypeRef::Global(ty) => globals.push(ty),
                        p::TypeRef::Table(ty) => {
                            tables.push(ty);
                            imported_tables += 1;
                        }
                        _ => {}
                    }
                }
            }
            Payload::FunctionSection(types) => {
                functions.extend(types.into_iter().collect::<Result<Vec<_>, _>>()?);
            }
            Payload::GlobalSection(section) => {
                for global in section {
                    globals.push(global?.ty);
                }
            }
            Payload::TableSection(section) => {
                for table in section {
                    tables.push(table?.ty);
                }
            }
            Payload::ElementSection(section) => {
                for (index, segment) in section.into_iter().enumerate() {
                    let segment = segment?;
                    if !matches!(segment.kind, p::ElementKind::Passive) {
                        continue;
                    }
                    let (ty, len) = match segment.items {
                        p::ElementItems::Functions(items) => (p::RefType::FUNCREF, items.count()),
                        p::ElementItems::Expressions(ty, items) => (ty, items.count()),
                    };
                    passive.push((index as u32, ty, len));
                }
            }
            Payload::CodeSectionEntry(body) => {
                for op in body.get_operators_reader()? {
                    let op = op?;
                    if is_gc_access(&op) {
                        let key = format!("{op:?}");
                        if !operation_indexes.contains_key(&key) {
                            operation_indexes.insert(key, operations.len());
                            operations.push(op);
                        }
                    }
                }
            }
            _ => {}
        }
    }
    let mut raw_types = Vec::new();
    let mut proxy_types = Vec::new();
    let mut next = if preserve_types { 0 } else { 4 };
    for (_, types) in &groups {
        for _ in types {
            raw_types.push(next);
            next += 1;
        }
        for ty in types {
            if preserve_types {
                proxy_types.push(Some(raw_types[proxy_types.len()]));
                continue;
            }
            proxy_types.push(
                if !matches!(ty.composite_type.inner, CompositeInnerType::Cont(_)) {
                    let index = next;
                    next += 1;
                    Some(index)
                } else {
                    None
                },
            );
        }
    }
    let mut lowering = Lowering {
        shared_pool,
        imported_tables,
        emitted_imports: false,
        preserve_types,
        module_id,
        groups,
        functions,
        raw_types,
        proxy_types,
        emitted_types: false,
        operation_indexes,
        helper_types: Vec::new(),
        helpers: Vec::new(),
        next_type: next,
        globals,
        tables,
        passive,
        emitted_functions: false,
        emitted_tables: false,
        emitted_exports: false,
        emitted_code: false,
        read_function: 0,
        function_read: None,
        type_markers: Vec::new(),
    };
    for op in operations {
        if preserve_types {
            continue;
        }
        let (ty, code) = lowering.gc_access(op)?;
        lowering.helper_types.push(ty);
        lowering.helpers.push(code);
    }
    lowering.snapshot_accessors()?;
    let mut module = e::Module::new();
    lowering.parse_core_module(&mut module, p::Parser::new(0), bytes)?;
    let output = module.finish();
    p::Validator::new_with_features(p::WasmFeatures::all()).validate_all(&output)?;
    Ok(output)
}

struct Lowering {
    shared_pool: bool,
    imported_tables: u32,
    emitted_imports: bool,
    preserve_types: bool,
    module_id: u32,
    groups: Vec<(bool, Vec<p::SubType>)>,
    functions: Vec<u32>,
    raw_types: Vec<u32>,
    proxy_types: Vec<Option<u32>>,
    emitted_types: bool,
    operation_indexes: HashMap<String, usize>,
    helper_types: Vec<e::FuncType>,
    helpers: Vec<e::Function>,
    next_type: u32,
    globals: Vec<p::GlobalType>,
    tables: Vec<p::TableType>,
    passive: Vec<(u32, p::RefType, u32)>,
    emitted_functions: bool,
    emitted_tables: bool,
    emitted_exports: bool,
    emitted_code: bool,
    read_function: u32,
    function_read: Option<u32>,
    type_markers: Vec<(u32, u32)>,
}

impl Lowering {
    fn function_ref(&self, function: u32, code: &mut Vec<u8>) {
        let id = ((1u64 << 63) | (u64::from(self.module_id) << 32) | u64::from(function)) as i64;
        e::Instruction::I64Const(id).encode(code);
        e::Instruction::RefFunc(function).encode(code);
        e::Instruction::StructNew(
            self.proxy_types[self.functions[function as usize] as usize].unwrap(),
        )
        .encode(code);
    }

    fn instructions(&mut self, op: Operator<'_>, code: &mut Vec<u8>) -> Result<(), Error> {
        use e::Instruction as I;
        if self.preserve_types {
            self.instruction(op)?.encode(code);
            return Ok(());
        }
        match op {
            Operator::RefFunc { function_index } => self.function_ref(function_index, code),
            op if is_gc_access(&op) => {
                I::Call(
                    self.functions.len() as u32 + self.operation_indexes[&format!("{op:?}")] as u32,
                )
                .encode(code);
            }
            Operator::CallRef { type_index } | Operator::ReturnCallRef { type_index } => {
                I::StructGet {
                    struct_type_index: self.proxy_types[type_index as usize].unwrap(),
                    field_index: 1,
                }
                .encode(code);
                if matches!(op, Operator::CallRef { .. }) {
                    I::CallRef(self.raw_types[type_index as usize]).encode(code);
                } else {
                    I::ReturnCallRef(self.raw_types[type_index as usize]).encode(code);
                }
            }
            Operator::CallIndirect {
                type_index,
                table_index,
            }
            | Operator::ReturnCallIndirect {
                type_index,
                table_index,
            } => {
                I::TableGet(self.table_index(table_index)?).encode(code);
                I::RefCastNonNull(e::HeapType::Concrete(
                    self.proxy_types[type_index as usize].unwrap(),
                ))
                .encode(code);
                I::StructGet {
                    struct_type_index: self.proxy_types[type_index as usize].unwrap(),
                    field_index: 1,
                }
                .encode(code);
                if matches!(op, Operator::CallIndirect { .. }) {
                    I::CallRef(self.raw_types[type_index as usize]).encode(code);
                } else {
                    I::ReturnCallRef(self.raw_types[type_index as usize]).encode(code);
                }
            }
            _ => self.instruction(op)?.encode(code),
        }
        Ok(())
    }

    fn proxy_type(&self, index: usize, original: &p::SubType) -> e::SubType {
        let root = match original.composite_type.inner {
            CompositeInnerType::Func(_) => 3,
            CompositeInnerType::Struct(_) => 1,
            CompositeInnerType::Array(_) => 2,
            CompositeInnerType::Cont(_) => unreachable!(),
        };
        let parent = original
            .supertype_idxs
            .first()
            .map(|p| self.proxy_types[p.as_module_index().unwrap() as usize].unwrap())
            .unwrap_or(root);
        e::SubType {
            is_final: original.is_final,
            supertype_idxs: vec![parent],
            composite_type: e::CompositeType {
                shared: false,
                descriptor: None,
                describes: None,
                inner: e::CompositeInnerType::Struct(e::StructType {
                    fields: vec![
                        e::FieldType {
                            element_type: e::StorageType::Val(e::ValType::I64),
                            mutable: false,
                        },
                        e::FieldType {
                            element_type: e::StorageType::Val(e::ValType::Ref(e::RefType {
                                nullable: false,
                                heap_type: e::HeapType::Concrete(self.raw_types[index]),
                            })),
                            mutable: false,
                        },
                    ]
                    .into(),
                }),
            },
        }
    }

    fn types(&mut self, types: &mut e::TypeSection) -> Result<(), Error> {
        self.emitted_types = true;
        if self.preserve_types {
            for (explicit, group) in self.groups.clone() {
                let group = group
                    .into_iter()
                    .map(|ty| self.sub_type(ty))
                    .collect::<Result<Vec<_>, _>>()?;
                if explicit {
                    types.ty().rec(group);
                } else {
                    types.ty().subtype(&group[0]);
                }
            }
            for ty in &self.helper_types {
                types.ty().func_type(ty);
            }
            return Ok(());
        }
        types.ty().subtype(&e::SubType {
            is_final: false,
            supertype_idxs: vec![],
            composite_type: e::CompositeType {
                shared: false,
                descriptor: None,
                describes: None,
                inner: e::CompositeInnerType::Struct(e::StructType {
                    fields: vec![e::FieldType {
                        element_type: e::StorageType::Val(e::ValType::I64),
                        mutable: false,
                    }]
                    .into(),
                }),
            },
        });
        for ty in [
            e::AbstractHeapType::Struct,
            e::AbstractHeapType::Array,
            e::AbstractHeapType::Func,
        ] {
            types.ty().subtype(&e::SubType {
                is_final: false,
                supertype_idxs: vec![0],
                composite_type: e::CompositeType {
                    shared: false,
                    descriptor: None,
                    describes: None,
                    inner: e::CompositeInnerType::Struct(e::StructType {
                        fields: vec![
                            e::FieldType {
                                element_type: e::StorageType::Val(e::ValType::I64),
                                mutable: false,
                            },
                            e::FieldType {
                                element_type: e::StorageType::Val(e::ValType::Ref(e::RefType {
                                    nullable: false,
                                    heap_type: e::HeapType::Abstract { shared: false, ty },
                                })),
                                mutable: false,
                            },
                        ]
                        .into(),
                    }),
                },
            });
        }
        let mut base = 0;
        for (explicit, group) in self.groups.clone() {
            // Keep non-recursive host-callable signatures in their original
            // recursive group. Recursive reference-valued groups also need
            // their proxies in that same group.
            let end = base + group.len();
            let mut within_group = false;
            for ty in &group {
                let values: Vec<_> = match &ty.composite_type.inner {
                    CompositeInnerType::Func(f) => {
                        f.params().iter().chain(f.results()).copied().collect()
                    }
                    CompositeInnerType::Struct(s) => s
                        .fields
                        .iter()
                        .filter_map(|f| match f.element_type {
                            p::StorageType::Val(v) => Some(v),
                            _ => None,
                        })
                        .collect(),
                    CompositeInnerType::Array(a) => match a.0.element_type {
                        p::StorageType::Val(v) => vec![v],
                        _ => vec![],
                    },
                    CompositeInnerType::Cont(_) => vec![],
                };
                within_group |= values.iter().any(|v| match v {
                    p::ValType::Ref(r) => r
                        .type_index()
                        .and_then(|i| i.as_module_index())
                        .is_some_and(|i| (base..end).contains(&(i as usize))),
                    _ => false,
                });
            }
            let mut encoded = group
                .iter()
                .cloned()
                .map(|t| self.sub_type(t))
                .collect::<Result<Vec<_>, _>>()?;
            let proxies: Vec<_> = group
                .iter()
                .enumerate()
                .filter(|(_, t)| !matches!(t.composite_type.inner, CompositeInnerType::Cont(_)))
                .map(|(i, t)| self.proxy_type(base + i, t))
                .collect();
            if within_group {
                encoded.extend(proxies);
                types.ty().rec(encoded);
            } else {
                if explicit {
                    types.ty().rec(encoded);
                } else {
                    types.ty().subtype(&encoded[0]);
                }
                for proxy in proxies {
                    types.ty().subtype(&proxy);
                }
            }
            base = end;
        }
        for ty in &self.helper_types {
            types.ty().func_type(ty);
        }
        Ok(())
    }
}

impl Reencode for Lowering {
    type Error = std::convert::Infallible;

    fn table_index(&mut self, index: u32) -> Result<u32, Error> {
        Ok(self.table_index_value(index))
    }

    fn parse_import_section(
        &mut self,
        imports: &mut e::ImportSection,
        reader: p::ImportSectionReader<'_>,
    ) -> Result<(), Error> {
        reencode::utils::parse_import_section(self, imports, reader)?;
        self.append_imports(imports);
        Ok(())
    }

    fn type_index(&mut self, ty: u32) -> Result<u32, Error> {
        Ok(self.raw_types[ty as usize])
    }

    fn heap_type(&mut self, ty: p::HeapType) -> Result<e::HeapType, Error> {
        if self.preserve_types {
            return reencode::utils::heap_type(self, ty);
        }
        Ok(match ty {
            p::HeapType::Abstract {
                shared: false,
                ty: p::AbstractHeapType::Func,
            } => e::HeapType::Concrete(3),
            p::HeapType::Abstract {
                shared: false,
                ty: p::AbstractHeapType::Struct,
            } => e::HeapType::Concrete(1),
            p::HeapType::Abstract {
                shared: false,
                ty: p::AbstractHeapType::Array,
            } => e::HeapType::Concrete(2),
            p::HeapType::Abstract {
                shared: false,
                ty: p::AbstractHeapType::NoFunc,
            } => e::HeapType::Abstract {
                shared: false,
                ty: e::AbstractHeapType::None,
            },
            p::HeapType::Concrete(index) => {
                let index = index.as_module_index().unwrap() as usize;
                e::HeapType::Concrete(self.proxy_types[index].unwrap_or(self.raw_types[index]))
            }
            p::HeapType::Exact(index) => {
                let index = index.as_module_index().unwrap() as usize;
                e::HeapType::Exact(self.proxy_types[index].unwrap_or(self.raw_types[index]))
            }
            ty => reencode::utils::heap_type(self, ty)?,
        })
    }

    fn parse_type_section(
        &mut self,
        types: &mut e::TypeSection,
        _: p::TypeSectionReader<'_>,
    ) -> Result<(), Error> {
        self.types(types)
    }

    fn intersperse_section_hook(
        &mut self,
        module: &mut e::Module,
        _: Option<e::SectionId>,
        before: Option<e::SectionId>,
    ) -> Result<(), Error> {
        use e::SectionId as S;
        let rank = match before {
            Some(S::Type) => 1,
            Some(S::Import) => 2,
            Some(S::Function) => 3,
            Some(S::Table) => 4,
            Some(S::Memory) => 5,
            Some(S::Tag) => 6,
            Some(S::Global) => 7,
            Some(S::Export) => 8,
            Some(S::Start) => 9,
            Some(S::Element) => 10,
            Some(S::DataCount) => 11,
            Some(S::Code) => 12,
            Some(S::Data) => 13,
            None => 14,
            Some(S::Custom) => unreachable!(),
        };
        if !self.emitted_types && rank > 1 {
            let mut types = e::TypeSection::new();
            self.types(&mut types)?;
            module.section(&types);
        }
        if self.shared_pool && !self.emitted_imports && rank > 2 {
            let mut section = e::ImportSection::new();
            self.append_imports(&mut section);
            module.section(&section);
        }
        if !self.emitted_functions && rank > 3 {
            let mut section = e::FunctionSection::new();
            self.append_functions(&mut section);
            module.section(&section);
        }
        if !self.emitted_tables && rank > 4 {
            let mut section = e::TableSection::new();
            self.append_pool(&mut section);
            module.section(&section);
        }
        if !self.emitted_exports && rank > 8 {
            let mut section = e::ExportSection::new();
            self.append_exports(&mut section);
            module.section(&section);
        }
        if !self.emitted_code && rank > 12 {
            let mut section = e::CodeSection::new();
            self.append_code(&mut section);
            module.section(&section);
        }
        Ok(())
    }

    fn const_expr(&mut self, expr: p::ConstExpr<'_>) -> Result<e::ConstExpr, Error> {
        if self.preserve_types {
            return reencode::utils::const_expr(self, expr);
        }
        let mut stack: Vec<Vec<u8>> = Vec::new();
        let mut reader = expr.get_operators_reader();
        while !reader.eof() {
            let op = reader.read()?;
            if matches!(op, Operator::End) {
                break;
            }
            let (pops, pushes) = op.operator_arity(self).unwrap();
            assert_eq!(pushes, 1);
            let operands = stack.split_off(stack.len() - pops as usize);
            let constructor_type = match op {
                Operator::StructNew { struct_type_index }
                | Operator::StructNewDefault { struct_type_index } => Some(struct_type_index),
                Operator::ArrayNew { array_type_index }
                | Operator::ArrayNewDefault { array_type_index }
                | Operator::ArrayNewFixed {
                    array_type_index, ..
                } => Some(array_type_index),
                _ => None,
            };
            let mut code = Vec::new();
            if let Some(ty) = constructor_type {
                e::Instruction::I64Const(
                    ((u64::from(self.module_id) << 32) | u64::from(ty)) as i64,
                )
                .encode(&mut code);
            }
            for operand in operands {
                code.extend(operand);
            }
            if let Some(ty) = constructor_type {
                self.instruction(op)?.encode(&mut code);
                e::Instruction::StructNew(self.proxy_types[ty as usize].unwrap()).encode(&mut code);
            } else {
                self.instructions(op, &mut code)?;
            }
            stack.push(code);
        }
        assert_eq!(stack.len(), 1);
        Ok(e::ConstExpr::raw(stack.pop().unwrap()))
    }

    fn element_items<'a>(&mut self, items: p::ElementItems<'a>) -> Result<e::Elements<'a>, Error> {
        if self.preserve_types {
            return reencode::utils::element_items(self, items);
        }
        if let p::ElementItems::Functions(functions) = items {
            let mut exprs = Vec::new();
            for function in functions {
                let mut code = Vec::new();
                self.function_ref(function?, &mut code);
                exprs.push(e::ConstExpr::raw(code));
            }
            Ok(e::Elements::Expressions(
                e::RefType {
                    nullable: true,
                    heap_type: e::HeapType::Concrete(3),
                },
                Cow::Owned(exprs),
            ))
        } else {
            reencode::utils::element_items(self, items)
        }
    }

    fn parse_function_body(
        &mut self,
        code: &mut e::CodeSection,
        body: p::FunctionBody<'_>,
    ) -> Result<(), Error> {
        let mut function = self.new_function_with_parsed_locals(&body)?;
        let mut bytes = Vec::new();
        for op in body.get_operators_reader()? {
            self.instructions(op?, &mut bytes)?;
        }
        function.raw(bytes);
        code.function(&function);
        Ok(())
    }

    fn parse_function_section(
        &mut self,
        functions: &mut e::FunctionSection,
        reader: p::FunctionSectionReader<'_>,
    ) -> Result<(), Error> {
        reencode::utils::parse_function_section(self, functions, reader)?;
        self.append_functions(functions);
        Ok(())
    }

    fn parse_code_section(
        &mut self,
        code: &mut e::CodeSection,
        reader: p::CodeSectionReader<'_>,
    ) -> Result<(), Error> {
        reencode::utils::parse_code_section(self, code, reader)?;
        self.append_code(code);
        Ok(())
    }

    fn parse_table_section(
        &mut self,
        tables: &mut e::TableSection,
        reader: p::TableSectionReader<'_>,
    ) -> Result<(), Error> {
        reencode::utils::parse_table_section(self, tables, reader)?;
        self.append_pool(tables);
        Ok(())
    }

    fn parse_export_section(
        &mut self,
        exports: &mut e::ExportSection,
        reader: p::ExportSectionReader<'_>,
    ) -> Result<(), Error> {
        reencode::utils::parse_export_section(self, exports, reader)?;
        self.append_exports(exports);
        Ok(())
    }
}

macro_rules! emit {
    ($function:expr; $($instruction:expr),* $(,)?) => { $( $function.instruction(&$instruction); )* };
}

impl Lowering {
    fn table_index_value(&self, index: u32) -> u32 {
        index + u32::from(self.shared_pool && index >= self.imported_tables)
    }

    fn pool_index(&self) -> u32 {
        if self.shared_pool {
            self.imported_tables
        } else {
            self.tables.len() as u32
        }
    }

    fn append_imports(&mut self, imports: &mut e::ImportSection) {
        self.emitted_imports = true;
        if self.shared_pool {
            imports.import(
                "__wizer_gc_state",
                "objects",
                e::EntityType::Table(e::TableType {
                    element_type: e::RefType::EQREF,
                    table64: false,
                    minimum: 0,
                    maximum: None,
                    shared: false,
                }),
            );
        }
    }

    fn append_functions(&mut self, functions: &mut e::FunctionSection) {
        self.emitted_functions = true;
        for index in 0..self.helpers.len() {
            functions.function(self.next_type + index as u32);
        }
    }

    fn append_pool(&mut self, tables: &mut e::TableSection) {
        self.emitted_tables = true;
        if !self.shared_pool {
            tables.table(e::TableType {
                element_type: e::RefType::EQREF,
                table64: false,
                minimum: 0,
                maximum: None,
                shared: false,
            });
        }
        for (_, ty, len) in self.passive.clone() {
            let mut ty = self.ref_type(ty).unwrap();
            ty.nullable = true;
            tables.table(e::TableType {
                element_type: ty,
                table64: false,
                minimum: u64::from(len),
                maximum: None,
                shared: false,
            });
        }
    }

    fn passive_table(&self, ordinal: usize) -> u32 {
        self.tables.len() as u32 + 1 + ordinal as u32
    }

    fn passive_ref(&self, code: &mut e::Function, ordinal: usize, len: u32, segment: u32) {
        use e::Instruction::*;
        emit!(code; I32Const(0), I32Const(0), I32Const(len as i32),
            TableInit { elem_index: segment, table: self.passive_table(ordinal) },
            LocalGet(2), I32WrapI64, TableGet(self.passive_table(ordinal)));
    }

    fn append_exports(&mut self, exports: &mut e::ExportSection) {
        self.emitted_exports = true;
        exports.export("__wizer_gc_read", e::ExportKind::Func, self.read_function);
        if let Some(function) = self.function_read {
            exports.export("__wizer_gc_function", e::ExportKind::Func, function);
            exports.export(
                "__wizer_gc_objects",
                e::ExportKind::Table,
                self.pool_index(),
            );
            for &(ty, function) in &self.type_markers {
                exports.export(
                    &format!("__wizer_gc_type_{ty}"),
                    e::ExportKind::Func,
                    function,
                );
            }
            for index in 0..self.functions.len() {
                exports.export(
                    &format!("__wizer_function_{index}"),
                    e::ExportKind::Func,
                    index as u32,
                );
            }
        }
    }

    fn append_code(&mut self, code: &mut e::CodeSection) {
        self.emitted_code = true;
        for helper in &self.helpers {
            code.function(helper);
        }
    }

    fn helper(
        &mut self,
        params: Vec<e::ValType>,
        results: Vec<e::ValType>,
        body: e::Function,
    ) -> u32 {
        let index = self.functions.len() as u32 + self.helpers.len() as u32;
        self.helper_types.push(e::FuncType::new(params, results));
        self.helpers.push(body);
        index
    }

    fn snapshot_accessors(&mut self) -> Result<(), Error> {
        use e::Instruction::*;
        use e::ValType::*;
        let pool = self.pool_index();
        // The registry is populated only after the initializer has returned.
        // In particular it does not keep temporary allocations alive during it.
        let mut id = e::Function::new([(1, I32)]);
        emit!(id;
            Block(e::BlockType::Empty), Loop(e::BlockType::Empty),
            LocalGet(1), TableSize(pool), I32Eq, BrIf(1),
            LocalGet(0), LocalGet(1), TableGet(pool), RefEq,
            If(e::BlockType::Empty), LocalGet(1), Return, End,
            LocalGet(1), I32Const(1), I32Add, LocalSet(1), Br(0), End, End,
            LocalGet(0), I32Const(1), TableGrow(pool), LocalTee(1), I32Const(-1), I32Eq,
            If(e::BlockType::Empty), Unreachable, End, LocalGet(1), End);
        let id_function = self.helper(vec![Ref(e::RefType::EQREF)], vec![I32], id);
        let type_function = if self.preserve_types {
            let mut body = e::Function::new([]);
            let types: Vec<_> = self
                .groups
                .iter()
                .flat_map(|(_, group)| group)
                .cloned()
                .collect();
            for (index, ty) in types.into_iter().enumerate().rev() {
                if !matches!(
                    ty.composite_type.inner,
                    CompositeInnerType::Struct(_) | CompositeInnerType::Array(_)
                ) {
                    continue;
                }
                let mut marker = e::Function::new([]);
                marker.instruction(&End);
                let function = self.helper(
                    vec![Ref(e::RefType {
                        nullable: true,
                        heap_type: e::HeapType::Concrete(index as u32),
                    })],
                    vec![],
                    marker,
                );
                self.type_markers.push((index as u32, function));
                emit!(body; LocalGet(0), RefTestNonNull(e::HeapType::Concrete(index as u32)), If(e::BlockType::Empty),
                    I64Const(((u64::from(self.module_id) << 32) | index as u64) as i64), Return, End);
            }
            emit!(body; Unreachable, End);
            Some(self.helper(vec![Ref(e::RefType::EQREF)], vec![I64], body))
        } else {
            None
        };
        let mut value = e::Function::new([(1, I64)]);
        emit!(value; LocalGet(0), RefIsNull, If(e::BlockType::Empty), I64Const(0), Return, End,
            LocalGet(0), RefTestNonNull(e::HeapType::I31), If(e::BlockType::Empty),
            LocalGet(1), If(e::BlockType::Result(I64)), LocalGet(0), RefCastNonNull(e::HeapType::I31), I31GetS, I64ExtendI32S,
            Else, I64Const(1), End, Return, End);
        if self.preserve_types {
            emit!(value; LocalGet(0), RefTestNonNull(e::HeapType::Abstract { shared: false, ty: e::AbstractHeapType::Eq }),
                I32Eqz, If(e::BlockType::Empty), I64Const(4), Return, End,
                LocalGet(1), If(e::BlockType::Result(I64)), LocalGet(0),
                RefCastNonNull(e::HeapType::Abstract { shared: false, ty: e::AbstractHeapType::Eq }),
                Call(id_function), I64ExtendI32U, Else, I64Const(2), End, End);
        } else {
            emit!(value;
            LocalGet(0), RefTestNonNull(e::HeapType::Concrete(0)), I32Eqz,
            If(e::BlockType::Empty), I64Const(4), Return, End,
            LocalGet(0), RefCastNonNull(e::HeapType::Concrete(0)), StructGet { struct_type_index: 0, field_index: 0 }, LocalSet(2),
            LocalGet(2), I64Const(0), I64LtS, If(e::BlockType::Empty),
            LocalGet(1), If(e::BlockType::Result(I64)), LocalGet(2), I64Const(i64::MAX), I64And,
            Else, I64Const(3), End, Return, End,
            LocalGet(1), If(e::BlockType::Result(I64)), LocalGet(0), RefCastNonNull(e::HeapType::Abstract { shared: false, ty: e::AbstractHeapType::Eq }), Call(id_function), I64ExtendI32U,
            Else, I64Const(2), End, End);
        }
        let value_function = self.helper(vec![Ref(e::RefType::ANYREF), I32], vec![I64], value);
        if self.preserve_types {
            self.function_accessor(type_function)?;
        }
        let needs_v128 = self
            .globals
            .iter()
            .any(|ty| ty.content_type == p::ValType::V128)
            || self.groups.iter().flat_map(|(_, types)| types).any(|ty| {
                let vector = p::StorageType::Val(p::ValType::V128);
                match &ty.composite_type.inner {
                    CompositeInnerType::Struct(s) => {
                        s.fields.iter().any(|field| field.element_type == vector)
                    }
                    CompositeInnerType::Array(a) => a.0.element_type == vector,
                    _ => false,
                }
            });
        let mut read = e::Function::new(needs_v128.then_some((1, V128)));
        // Opcodes: global, table length, table entry, object owner/type,
        // object length, object field, and registry length.
        for (index, ty) in self.globals.clone().into_iter().enumerate() {
            emit!(read; LocalGet(0), I32Eqz, LocalGet(1), I32Const(index as i32), I32Eq, I32And, If(e::BlockType::Empty), GlobalGet(index as u32));
            self.encode_value(&mut read, ty.content_type, value_function)?;
            emit!(read; Return, End);
        }
        for (index, table) in self.tables.clone().into_iter().enumerate() {
            emit!(read; LocalGet(1), I32Const(index as i32), I32Eq, If(e::BlockType::Empty),
                LocalGet(0), I32Const(1), I32Eq, If(e::BlockType::Empty), TableSize(self.table_index_value(index as u32)));
            if !table.table64 {
                read.instruction(&I64ExtendI32U);
            }
            emit!(read; Return, End, LocalGet(0), I32Const(2), I32Eq, If(e::BlockType::Empty), LocalGet(2));
            if !table.table64 {
                read.instruction(&I32WrapI64);
            }
            read.instruction(&TableGet(self.table_index_value(index as u32)));
            self.encode_value(
                &mut read,
                p::ValType::Ref(table.element_type),
                value_function,
            )?;
            emit!(read; Return, End, End);
        }
        for (ordinal, (segment, ty, len)) in self.passive.clone().into_iter().enumerate() {
            emit!(read; LocalGet(0), I32Const(7), I32Eq, LocalGet(1), I32Const(segment as i32), I32Eq,
                I32And, If(e::BlockType::Empty));
            self.passive_ref(&mut read, ordinal, len, segment);
            self.encode_value(&mut read, p::ValType::Ref(ty), value_function)?;
            emit!(read; Return, End);
        }
        emit!(read; LocalGet(0), I32Const(3), I32Eq, If(e::BlockType::Empty));
        self.read_object_type(&mut read, type_function);
        emit!(read; Return, End,
            LocalGet(0), I32Const(6), I32Eq, If(e::BlockType::Empty), TableSize(pool), I64ExtendI32U, Return, End);
        for (index, ty) in self
            .groups
            .clone()
            .into_iter()
            .flat_map(|(_, types)| types)
            .enumerate()
        {
            if matches!(
                ty.composite_type.inner,
                CompositeInnerType::Func(_) | CompositeInnerType::Cont(_)
            ) {
                continue;
            }
            let owner = ((u64::from(self.module_id) << 32) | index as u64) as i64;
            emit!(read; LocalGet(0), I32Const(4), I32Eq, LocalGet(0), I32Const(5), I32Eq, I32Or, If(e::BlockType::Empty));
            self.read_object_type(&mut read, type_function);
            emit!(read; I64Const(owner), I64Eq, If(e::BlockType::Empty));
            let raw = self.raw_types[index];
            emit!(read; LocalGet(0), I32Const(4), I32Eq, If(e::BlockType::Empty));
            match &ty.composite_type.inner {
                CompositeInnerType::Struct(s) => {
                    read.instruction(&I64Const(s.fields.len() as i64));
                }
                CompositeInnerType::Array(_) => {
                    self.read_object(&mut read, index);
                    emit!(read; ArrayLen, I64ExtendI32U);
                }
                _ => unreachable!(),
            }
            emit!(read; Return, End);
            match &ty.composite_type.inner {
                CompositeInnerType::Struct(s) => {
                    for (field_index, field) in s.fields.iter().enumerate() {
                        emit!(read; LocalGet(2), I64Const(field_index as i64), I64Eq, If(e::BlockType::Empty));
                        self.read_object(&mut read, index);
                        read.instruction(&match field.element_type {
                            p::StorageType::I8 | p::StorageType::I16 => StructGetU {
                                struct_type_index: raw,
                                field_index: field_index as u32,
                            },
                            _ => StructGet {
                                struct_type_index: raw,
                                field_index: field_index as u32,
                            },
                        });
                        self.encode_value(&mut read, field.element_type.unpack(), value_function)?;
                        emit!(read; Return, End);
                    }
                }
                CompositeInnerType::Array(a) => {
                    self.read_object(&mut read, index);
                    emit!(read; LocalGet(2), I32WrapI64);
                    read.instruction(&match a.0.element_type {
                        p::StorageType::I8 | p::StorageType::I16 => ArrayGetU(raw),
                        _ => ArrayGet(raw),
                    });
                    self.encode_value(&mut read, a.0.element_type.unpack(), value_function)?;
                    read.instruction(&Return);
                }
                _ => unreachable!(),
            }
            emit!(read; Unreachable, End, End);
        }
        emit!(read; Unreachable, End);
        self.read_function = self.helper(vec![I32, I32, I64, I32], vec![I64], read);
        Ok(())
    }

    fn read_object_type(&self, read: &mut e::Function, type_function: Option<u32>) {
        use e::Instruction::*;
        emit!(read; LocalGet(1), TableGet(self.pool_index()));
        if let Some(function) = type_function {
            read.instruction(&Call(function));
        } else {
            emit!(read; RefCastNonNull(e::HeapType::Concrete(0)), StructGet { struct_type_index: 0, field_index: 0 });
        }
    }

    fn function_accessor(&mut self, type_function: Option<u32>) -> Result<(), Error> {
        use e::Instruction::*;
        let mut read = e::Function::new([]);
        for (index, ty) in self.globals.iter().enumerate() {
            if !matches!(ty.content_type, p::ValType::Ref(r) if self.function_reference(r)) {
                continue;
            }
            emit!(read; LocalGet(0), I32Eqz, LocalGet(1), I32Const(index as i32), I32Eq, I32And,
                If(e::BlockType::Empty), GlobalGet(index as u32), Return, End);
        }
        for (index, table) in self.tables.iter().enumerate() {
            if !self.function_reference(table.element_type) {
                continue;
            }
            emit!(read; LocalGet(0), I32Const(2), I32Eq, LocalGet(1), I32Const(index as i32), I32Eq, I32And,
                If(e::BlockType::Empty), LocalGet(2));
            if !table.table64 {
                read.instruction(&I32WrapI64);
            }
            emit!(read; TableGet(self.table_index_value(index as u32)), Return, End);
        }
        for (ordinal, &(segment, ty, len)) in self.passive.iter().enumerate() {
            if !self.function_reference(ty) {
                continue;
            }
            emit!(read; LocalGet(0), I32Const(7), I32Eq, LocalGet(1), I32Const(segment as i32), I32Eq,
                I32And, If(e::BlockType::Empty));
            self.passive_ref(&mut read, ordinal, len, segment);
            emit!(read; Return, End);
        }
        for (index, ty) in self.groups.iter().flat_map(|(_, group)| group).enumerate() {
            let fields: Vec<_> = match &ty.composite_type.inner {
                CompositeInnerType::Struct(s) => s
                    .fields
                    .iter()
                    .enumerate()
                    .map(|(i, f)| (Some(i as u32), f))
                    .collect(),
                CompositeInnerType::Array(a) => vec![(None, &a.0)],
                _ => continue,
            };
            for (field_index, field) in fields {
                if !matches!(field.element_type, p::StorageType::Val(p::ValType::Ref(r)) if self.function_reference(r))
                {
                    continue;
                }
                emit!(read; LocalGet(0), I32Const(5), I32Eq, If(e::BlockType::Empty));
                self.read_object_type(&mut read, type_function);
                emit!(read; I64Const(((u64::from(self.module_id) << 32) | index as u64) as i64), I64Eq, If(e::BlockType::Empty));
                if let Some(field_index) = field_index {
                    emit!(read; LocalGet(2), I64Const(i64::from(field_index)), I64Eq, If(e::BlockType::Empty));
                    self.read_object(&mut read, index);
                    emit!(read; StructGet { struct_type_index: index as u32, field_index }, Return, End);
                } else {
                    self.read_object(&mut read, index);
                    emit!(read; LocalGet(2), I32WrapI64, ArrayGet(index as u32), Return);
                }
                emit!(read; End, End);
            }
        }
        emit!(read; Unreachable, End);
        self.function_read = Some(self.helper(
            vec![e::ValType::I32, e::ValType::I32, e::ValType::I64],
            vec![e::ValType::Ref(e::RefType::FUNCREF)],
            read,
        ));
        Ok(())
    }

    fn read_object(&self, read: &mut e::Function, index: usize) {
        use e::Instruction::*;
        let proxy = self.proxy_types[index].unwrap();
        emit!(read; LocalGet(1), TableGet(self.pool_index()), RefCastNonNull(e::HeapType::Concrete(proxy)));
        if !self.preserve_types {
            read.instruction(&StructGet {
                struct_type_index: proxy,
                field_index: 1,
            });
        }
    }

    fn encode_value(
        &mut self,
        function: &mut e::Function,
        ty: p::ValType,
        encode_function: u32,
    ) -> Result<(), Error> {
        use e::Instruction::*;
        match ty {
            p::ValType::I32 => {
                function.instruction(&I64ExtendI32U);
            }
            p::ValType::I64 => {}
            p::ValType::F32 => {
                emit!(function; I32ReinterpretF32, I64ExtendI32U);
            }
            p::ValType::F64 => {
                function.instruction(&I64ReinterpretF64);
            }
            p::ValType::V128 => {
                emit!(function; LocalSet(4), LocalGet(3), If(e::BlockType::Result(e::ValType::I64)), LocalGet(4), I64x2ExtractLane(1), Else, LocalGet(4), I64x2ExtractLane(0), End);
            }
            p::ValType::Ref(ty) => {
                if self.preserve_types && self.function_reference(ty) {
                    emit!(function; RefIsNull, If(e::BlockType::Result(e::ValType::I64)), I64Const(0), Else, I64Const(3), End);
                    return Ok(());
                }
                let unsupported = match ty.heap_type() {
                    p::HeapType::Concrete(index) | p::HeapType::Exact(index) => matches!(
                        self.original_type(index.as_module_index().unwrap())
                            .composite_type
                            .inner,
                        CompositeInnerType::Cont(_)
                    ),
                    p::HeapType::Abstract { ty, .. } => matches!(
                        ty,
                        p::AbstractHeapType::Exn
                            | p::AbstractHeapType::NoExn
                            | p::AbstractHeapType::Cont
                            | p::AbstractHeapType::NoCont
                    ),
                };
                if unsupported {
                    emit!(function; RefIsNull, If(e::BlockType::Result(e::ValType::I64)),
                        I64Const(0), Else, I64Const(4), End);
                    return Ok(());
                }
                if matches!(
                    ty.heap_type(),
                    p::HeapType::Abstract {
                        ty: p::AbstractHeapType::Extern | p::AbstractHeapType::NoExtern,
                        ..
                    }
                ) {
                    function.instruction(&AnyConvertExtern);
                }
                emit!(function; LocalGet(3), Call(encode_function));
            }
        }
        Ok(())
    }

    fn function_reference(&self, ty: p::RefType) -> bool {
        match ty.heap_type() {
            p::HeapType::Abstract {
                ty: p::AbstractHeapType::Func | p::AbstractHeapType::NoFunc,
                ..
            } => true,
            p::HeapType::Concrete(index) | p::HeapType::Exact(index) => matches!(
                self.original_type(index.as_module_index().unwrap())
                    .composite_type
                    .inner,
                CompositeInnerType::Func(_)
            ),
            _ => false,
        }
    }
}

fn is_gc_access(op: &Operator<'_>) -> bool {
    matches!(
        op,
        Operator::StructNew { .. }
            | Operator::StructNewDefault { .. }
            | Operator::StructGet { .. }
            | Operator::StructGetS { .. }
            | Operator::StructGetU { .. }
            | Operator::StructSet { .. }
            | Operator::ArrayNew { .. }
            | Operator::ArrayNewDefault { .. }
            | Operator::ArrayNewFixed { .. }
            | Operator::ArrayNewData { .. }
            | Operator::ArrayNewElem { .. }
            | Operator::ArrayGet { .. }
            | Operator::ArrayGetS { .. }
            | Operator::ArrayGetU { .. }
            | Operator::ArraySet { .. }
            | Operator::ArrayLen
            | Operator::ArrayFill { .. }
            | Operator::ArrayCopy { .. }
            | Operator::ArrayInitData { .. }
            | Operator::ArrayInitElem { .. }
    )
}

impl Lowering {
    fn original_type(&self, index: u32) -> &p::SubType {
        self.groups
            .iter()
            .flat_map(|(_, types)| types)
            .nth(index as usize)
            .unwrap()
    }

    fn gc_access(&mut self, op: Operator<'_>) -> Result<(e::FuncType, e::Function), Error> {
        use p::ValType as V;
        let mut params = Vec::new();
        let mut results = Vec::new();
        let mut wrapped_params = Vec::new();
        let mut constructor = None;
        let reference = |ty| {
            V::Ref(
                p::RefType::new(true, p::HeapType::Concrete(p::UnpackedIndex::Module(ty))).unwrap(),
            )
        };
        match op {
            Operator::StructNew { struct_type_index }
            | Operator::StructNewDefault { struct_type_index } => {
                if matches!(op, Operator::StructNew { .. }) {
                    let CompositeInnerType::Struct(s) =
                        &self.original_type(struct_type_index).composite_type.inner
                    else {
                        unreachable!()
                    };
                    params.extend(s.fields.iter().map(|f| f.element_type.unpack()));
                }
                results.push(reference(struct_type_index));
                constructor = Some(struct_type_index);
            }
            Operator::StructGet {
                struct_type_index,
                field_index,
            }
            | Operator::StructGetS {
                struct_type_index,
                field_index,
            }
            | Operator::StructGetU {
                struct_type_index,
                field_index,
            }
            | Operator::StructSet {
                struct_type_index,
                field_index,
            } => {
                let CompositeInnerType::Struct(s) =
                    &self.original_type(struct_type_index).composite_type.inner
                else {
                    unreachable!()
                };
                let field = s.fields[field_index as usize].element_type.unpack();
                params.push(reference(struct_type_index));
                wrapped_params.push((0, self.proxy_types[struct_type_index as usize].unwrap()));
                if matches!(op, Operator::StructSet { .. }) {
                    params.push(field);
                } else {
                    results.push(field);
                }
            }
            Operator::ArrayLen => {
                params.push(V::Ref(p::RefType::ARRAYREF));
                results.push(V::I32);
                wrapped_params.push((0, 2));
            }
            Operator::ArrayCopy {
                array_type_index_dst,
                array_type_index_src,
            } => {
                params.extend([
                    reference(array_type_index_dst),
                    V::I32,
                    reference(array_type_index_src),
                    V::I32,
                    V::I32,
                ]);
                wrapped_params.extend([
                    (0, self.proxy_types[array_type_index_dst as usize].unwrap()),
                    (2, self.proxy_types[array_type_index_src as usize].unwrap()),
                ]);
            }
            _ => {
                let ty = match op {
                    Operator::ArrayNew { array_type_index }
                    | Operator::ArrayNewDefault { array_type_index }
                    | Operator::ArrayNewFixed {
                        array_type_index, ..
                    }
                    | Operator::ArrayNewData {
                        array_type_index, ..
                    }
                    | Operator::ArrayNewElem {
                        array_type_index, ..
                    }
                    | Operator::ArrayGet { array_type_index }
                    | Operator::ArrayGetS { array_type_index }
                    | Operator::ArrayGetU { array_type_index }
                    | Operator::ArraySet { array_type_index }
                    | Operator::ArrayFill { array_type_index }
                    | Operator::ArrayInitData {
                        array_type_index, ..
                    }
                    | Operator::ArrayInitElem {
                        array_type_index, ..
                    } => array_type_index,
                    _ => unreachable!(),
                };
                let CompositeInnerType::Array(a) = &self.original_type(ty).composite_type.inner
                else {
                    unreachable!()
                };
                let element = a.0.element_type.unpack();
                match op {
                    Operator::ArrayNew { .. } => {
                        params.extend([element, V::I32]);
                        constructor = Some(ty);
                    }
                    Operator::ArrayNewDefault { .. } => {
                        params.push(V::I32);
                        constructor = Some(ty);
                    }
                    Operator::ArrayNewFixed { array_size, .. } => {
                        params.extend(std::iter::repeat_n(element, array_size as usize));
                        constructor = Some(ty);
                    }
                    Operator::ArrayNewData { .. } | Operator::ArrayNewElem { .. } => {
                        params.extend([V::I32, V::I32]);
                        constructor = Some(ty);
                    }
                    _ => {
                        params.extend([reference(ty), V::I32]);
                        wrapped_params.push((0, self.proxy_types[ty as usize].unwrap()));
                        match op {
                            Operator::ArrayGet { .. }
                            | Operator::ArrayGetS { .. }
                            | Operator::ArrayGetU { .. } => results.push(element),
                            Operator::ArraySet { .. } => params.push(element),
                            Operator::ArrayFill { .. } => params.extend([element, V::I32]),
                            Operator::ArrayInitData { .. } | Operator::ArrayInitElem { .. } => {
                                params.extend([V::I32, V::I32])
                            }
                            _ => unreachable!(),
                        }
                    }
                }
                if constructor.is_some() {
                    results.push(reference(ty));
                }
            }
        }
        if constructor.is_some() {
            let V::Ref(r) = &mut results[0] else {
                unreachable!()
            };
            *r = r.as_non_null();
        }
        let param_types = params
            .iter()
            .copied()
            .map(|t| self.val_type(t))
            .collect::<Result<Vec<_>, _>>()?;
        let result_types = results
            .iter()
            .copied()
            .map(|t| self.val_type(t))
            .collect::<Result<Vec<_>, _>>()?;
        let signature = e::FuncType::new(param_types, result_types);
        let mut function = e::Function::new([]);
        if let Some(ty) = constructor {
            function.instruction(&e::Instruction::I64Const(
                ((u64::from(self.module_id) << 32) | u64::from(ty)) as i64,
            ));
        }
        for index in 0..params.len() as u32 {
            function.instruction(&e::Instruction::LocalGet(index));
            if let Some((_, ty)) = wrapped_params.iter().find(|(i, _)| *i == index) {
                function.instruction(&e::Instruction::StructGet {
                    struct_type_index: *ty,
                    field_index: 1,
                });
            }
        }
        function.instruction(&self.instruction(op)?);
        if let Some(ty) = constructor {
            function.instruction(&e::Instruction::StructNew(
                self.proxy_types[ty as usize].unwrap(),
            ));
        }
        function.instruction(&e::Instruction::End);
        Ok((signature, function))
    }
}

impl p::ModuleArity for Lowering {
    fn sub_type_at(&self, type_idx: u32) -> Option<&p::SubType> {
        Some(self.original_type(type_idx))
    }
    fn tag_type_arity(&self, _: u32) -> Option<(u32, u32)> {
        None
    }
    fn type_index_of_function(&self, index: u32) -> Option<u32> {
        self.functions.get(index as usize).copied()
    }
    fn func_type_of_cont_type(&self, _: &p::ContType) -> Option<&p::FuncType> {
        None
    }
    fn sub_type_of_ref_type(&self, r: &p::RefType) -> Option<&p::SubType> {
        self.sub_type_at(r.type_index()?.as_module_index()?)
    }
    fn control_stack_height(&self) -> u32 {
        0
    }
    fn label_block(&self, _: u32) -> Option<(p::BlockType, p::FrameKind)> {
        None
    }
}
