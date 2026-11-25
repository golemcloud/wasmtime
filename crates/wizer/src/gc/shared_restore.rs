use super::capture::{Graph, Snapshot, Value};
use std::collections::BTreeMap;
use wasm_encoder::{
    self as e,
    reencode::{self, Reencode},
};
use wasmparser::{self as p, Payload, ValType};
use wasmtime::Result;

type Error = reencode::Error<std::convert::Infallible>;

struct Offset(u32);
impl Reencode for Offset {
    type Error = std::convert::Infallible;
    fn type_index(&mut self, index: u32) -> std::result::Result<u32, Error> {
        Ok(index + self.0)
    }
}

struct Plan {
    immutable: Vec<(u32, String)>,
    passive: Vec<(u32, p::RefType, Vec<String>)>,
    globals: Vec<(u32, u32, u32)>,
    tables: Vec<(u32, u64, u32, u32)>,
    functions: Vec<(u32, u32)>,
}

fn function_value(value: &mut Value, functions: &BTreeMap<(u32, u32), u32>) {
    if let Value::Function(module, index) = *value {
        *value = Value::Function(0, functions[&(module, index)]);
    }
}

pub fn restore(originals: &[(u32, &[u8])], graph: &Graph) -> Result<(Vec<Vec<u8>>, String)> {
    let mut imported_modules = std::collections::BTreeSet::new();
    for &(_, bytes) in originals {
        for payload in p::Parser::new(0).parse_all(bytes) {
            if let Payload::ImportSection(reader) = payload? {
                for import in reader.into_imports() {
                    imported_modules.insert(import?.module.to_owned());
                }
            }
        }
    }
    let mut ordinal = 0;
    let state_import = loop {
        let name = format!("wizer-gc-state-{ordinal}");
        if !imported_modules.contains(&name) {
            break name;
        }
        ordinal += 1;
    };
    let mut types = e::TypeSection::new();
    let mut offsets = BTreeMap::new();
    let mut type_count = 0;
    for &(id, bytes) in originals {
        offsets.insert(id, type_count);
        for payload in p::Parser::new(0).parse_all(bytes) {
            if let Payload::TypeSection(reader) = payload? {
                let mut remap = Offset(type_count);
                for group in reader {
                    let group = group?;
                    type_count += group.types().len() as u32;
                    remap.parse_recursive_type_group(types.ty(), group)?;
                }
            }
        }
    }
    let mut type_shell = e::Module::new();
    type_shell.section(&types);
    let mut combined_types = Vec::new();
    for payload in p::Parser::new(0).parse_all(&type_shell.finish()) {
        if let Payload::TypeSection(reader) = payload? {
            for group in reader {
                combined_types.extend(group?.into_types());
            }
        }
    }
    let mut functions = BTreeMap::new();
    for value in graph
        .nodes
        .iter()
        .flat_map(|n| &n.fields)
        .chain(graph.modules.iter().flat_map(|(_, s)| {
            s.globals
                .iter()
                .map(|(_, v)| v)
                .chain(s.tables.iter().flat_map(|(_, v)| v))
                .chain(s.passive.iter().flat_map(|(_, _, v)| v))
        }))
    {
        if let Value::Function(module, index) = value {
            let next = functions.len() as u32;
            functions.entry((*module, *index)).or_insert(next);
        }
    }
    let mut snapshot = Snapshot {
        types: combined_types,
        globals: Vec::new(),
        tables: Vec::new(),
        nodes: graph.nodes.clone(),
        function_types: vec![0; functions.len()],
        imported_globals: 0,
        passive: Vec::new(),
    };
    let mut bodies = (0..functions.len())
        .map(|_| e::Function::new([]))
        .collect::<Vec<_>>();
    for (&(module, index), &slot) in &functions {
        let source = &graph
            .modules
            .iter()
            .find(|(id, _)| *id == module)
            .unwrap()
            .1;
        let ty = source.function_types[index as usize] + offsets[&module];
        snapshot.function_types[slot as usize] = ty;
        let p::CompositeInnerType::Func(sig) = &snapshot.types[ty as usize].composite_type.inner
        else {
            unreachable!()
        };
        let body = &mut bodies[slot as usize];
        for param in 0..sig.params().len() {
            body.instruction(&e::Instruction::LocalGet(param as u32));
        }
        body.instruction(&e::Instruction::I32Const(slot as i32));
        body.instruction(&e::Instruction::TableGet(2));
        body.instruction(&e::Instruction::RefCastNonNull(e::HeapType::Concrete(ty)));
        body.instruction(&e::Instruction::CallRef(ty));
        body.instruction(&e::Instruction::End);
    }
    for node in &mut snapshot.nodes {
        node.type_index += offsets[&node.module_id];
        node.module_id = 0;
        for value in &mut node.fields {
            function_value(value, &functions);
        }
    }
    let mut roots: [Vec<Value>; 2] = [Vec::new(), Vec::new()];
    let mut names = Vec::new();
    let mut plans = Vec::new();
    for &(id, bytes) in originals {
        let source = &graph
            .modules
            .iter()
            .find(|(module, _)| *module == id)
            .unwrap()
            .1;
        let mut plan = Plan {
            immutable: Vec::new(),
            passive: Vec::new(),
            globals: Vec::new(),
            tables: Vec::new(),
            functions: Vec::new(),
        };
        for (&(module, function), &slot) in &functions {
            if module == id {
                plan.functions.push((function, slot));
            }
        }
        for (index, (ty, value)) in source
            .globals
            .iter()
            .enumerate()
            .skip(source.imported_globals)
        {
            if let ValType::Ref(r) = ty.content_type {
                let mut value = value.clone();
                function_value(&mut value, &functions);
                if !ty.mutable {
                    let name = format!("global-{id}-{index}");
                    plan.immutable.push((index as u32, name.clone()));
                    names.push(name);
                    snapshot
                        .globals
                        .push((remap_global(*ty, offsets[&id])?, value));
                } else {
                    let pool = if is_function(source, r) { 1 } else { 0 };
                    let slot = roots[pool].len() as u32;
                    roots[pool].push(value);
                    plan.globals.push((index as u32, pool as u32, slot));
                }
            }
        }
        for (segment, ty, values) in &source.passive {
            let mut imports = Vec::new();
            for (index, value) in values.iter().enumerate() {
                let name = format!("element-{id}-{segment}-{index}");
                imports.push(name.clone());
                names.push(name);
                let mut value = value.clone();
                function_value(&mut value, &functions);
                snapshot.globals.push((
                    remap_global(
                        p::GlobalType {
                            content_type: ValType::Ref(*ty),
                            mutable: false,
                            shared: false,
                        },
                        offsets[&id],
                    )?,
                    value,
                ));
            }
            plan.passive.push((*segment, *ty, imports));
        }
        let mut imported_tables = 0;
        for payload in p::Parser::new(0).parse_all(bytes) {
            if let Payload::ImportSection(reader) = payload? {
                for import in reader.into_imports() {
                    if matches!(import?.ty, p::TypeRef::Table(_)) {
                        imported_tables += 1;
                    }
                }
            }
        }
        for (index, (ty, values)) in source.tables.iter().enumerate().skip(imported_tables) {
            let pool = if is_function(source, ty.element_type) {
                1
            } else {
                0
            };
            for (offset, value) in values.iter().enumerate() {
                let mut value = value.clone();
                function_value(&mut value, &functions);
                let slot = roots[pool].len() as u32;
                roots[pool].push(value);
                plan.tables
                    .push((index as u32, offset as u64, pool as u32, slot));
            }
        }
        plans.push((imported_tables as u32, plan));
    }
    for (pool, values) in roots.into_iter().enumerate() {
        snapshot.tables.push((
            p::TableType {
                element_type: if pool == 0 {
                    p::RefType::ANYREF
                } else {
                    p::RefType::FUNCREF
                },
                table64: false,
                initial: values.len() as u64,
                maximum: None,
                shared: false,
            },
            values,
        ));
    }
    snapshot.tables.push((
        p::TableType {
            element_type: p::RefType::FUNCREF,
            table64: false,
            initial: functions.len() as u64,
            maximum: None,
            shared: false,
        },
        vec![Value::Null; functions.len()],
    ));
    let helper = super::restore::helper(types, &snapshot, &bodies, &names)?;
    let mut output = vec![helper];
    for ((&(id, bytes), (imported_tables, plan)), (_, snapshot)) in
        originals.iter().zip(plans).zip(&graph.modules)
    {
        let _ = id;
        output.push(consumer(
            bytes,
            snapshot,
            imported_tables,
            plan,
            &state_import,
        )?);
    }
    Ok((output, state_import))
}

struct Consumer<'a> {
    snapshot: &'a Snapshot,
    state_import: &'a str,
    imported_tables: u32,
    plan: Plan,
    start: e::Function,
    emitted: [bool; 6],
}

impl Consumer<'_> {
    fn extra_globals(&self) -> u32 {
        self.plan.immutable.len() as u32
            + self
                .plan
                .passive
                .iter()
                .map(|(_, _, v)| v.len() as u32)
                .sum::<u32>()
    }
    fn types(&mut self, s: &mut e::TypeSection) {
        s.ty().function([], []);
        self.emitted[0] = true;
    }
    fn imports(&mut self, s: &mut e::ImportSection) -> std::result::Result<(), Error> {
        for (pool, name) in ["roots-any", "roots-func", "functions"]
            .into_iter()
            .enumerate()
        {
            s.import(
                self.state_import,
                name,
                e::TableType {
                    element_type: if pool == 0 {
                        e::RefType::ANYREF
                    } else {
                        e::RefType::FUNCREF
                    },
                    minimum: 0,
                    maximum: None,
                    table64: false,
                    shared: false,
                },
            );
        }
        for &(index, ref name) in &self.plan.immutable {
            s.import(
                self.state_import,
                name,
                reencode::RoundtripReencoder
                    .global_type(self.snapshot.globals[index as usize].0)?,
            );
        }
        for (_, ty, names) in &self.plan.passive {
            for name in names {
                s.import(
                    self.state_import,
                    name,
                    e::GlobalType {
                        val_type: e::ValType::Ref(reencode::RoundtripReencoder.ref_type(*ty)?),
                        mutable: false,
                        shared: false,
                    },
                );
            }
        }
        self.emitted[1] = true;
        Ok(())
    }
    fn functions(&mut self, s: &mut e::FunctionSection) {
        s.function(self.snapshot.types.len() as u32);
        self.emitted[2] = true;
    }
    fn elements(&mut self, s: &mut e::ElementSection) {
        s.declared(e::Elements::Functions(std::borrow::Cow::Owned(
            self.plan.functions.iter().map(|(f, _)| *f).collect(),
        )));
        self.emitted[4] = true;
    }
    fn code(&mut self, s: &mut e::CodeSection) {
        s.function(&self.start);
        self.emitted[5] = true;
    }
    fn root(
        &self,
        body: &mut e::Function,
        pool: u32,
        slot: u32,
        ty: p::RefType,
        value: &Value,
    ) -> Result<()> {
        if matches!(value, Value::Null) {
            body.instruction(&e::Instruction::RefNull(
                reencode::RoundtripReencoder.heap_type(ty.heap_type())?,
            ));
            return Ok(());
        }
        body.instruction(&e::Instruction::I32Const(slot as i32));
        body.instruction(&e::Instruction::TableGet(self.imported_tables + pool));
        if ty.is_extern_ref() {
            body.instruction(&e::Instruction::ExternConvertAny);
        }
        let ty = reencode::RoundtripReencoder.ref_type(ty)?;
        if ty.nullable {
            body.instruction(&e::Instruction::RefCastNullable(ty.heap_type));
        } else {
            body.instruction(&e::Instruction::RefCastNonNull(ty.heap_type));
        }
        Ok(())
    }
    fn clear(&self, body: &mut e::Function, pool: u32, slot: u32) {
        body.instruction(&e::Instruction::I32Const(slot as i32));
        body.instruction(&e::Instruction::RefNull(if pool == 0 {
            e::HeapType::ANY
        } else {
            e::HeapType::FUNC
        }));
        body.instruction(&e::Instruction::TableSet(self.imported_tables + pool));
    }
}

fn consumer(
    bytes: &[u8],
    snapshot: &Snapshot,
    imported_tables: u32,
    plan: Plan,
    state_import: &str,
) -> Result<Vec<u8>> {
    let mut r = Consumer {
        snapshot,
        state_import,
        imported_tables,
        plan,
        start: e::Function::new([]),
        emitted: [false; 6],
    };
    let mut start = e::Function::new([]);
    for &(function, slot) in &r.plan.functions {
        start.instruction(&e::Instruction::I32Const(slot as i32));
        start.instruction(&e::Instruction::RefFunc(function));
        start.instruction(&e::Instruction::TableSet(imported_tables + 2));
    }
    for &(index, pool, slot) in &r.plan.globals {
        let ValType::Ref(ty) = snapshot.globals[index as usize].0.content_type else {
            unreachable!()
        };
        r.root(
            &mut start,
            pool,
            slot,
            ty,
            &snapshot.globals[index as usize].1,
        )?;
        let global = index + r.extra_globals();
        start.instruction(&e::Instruction::GlobalSet(global));
        r.clear(&mut start, pool, slot);
    }
    for &(table, offset, pool, slot) in &r.plan.tables {
        let ty = snapshot.tables[table as usize].0;
        if ty.table64 {
            start.instruction(&e::Instruction::I64Const(offset as i64));
        } else {
            start.instruction(&e::Instruction::I32Const(offset as i32));
        }
        r.root(
            &mut start,
            pool,
            slot,
            ty.element_type,
            &snapshot.tables[table as usize].1[offset as usize],
        )?;
        start.instruction(&e::Instruction::TableSet(table + 3));
        r.clear(&mut start, pool, slot);
    }
    start.instruction(&e::Instruction::End);
    r.start = start;
    let mut module = e::Module::new();
    r.parse_core_module(&mut module, p::Parser::new(0), bytes)?;
    let bytes = module.finish();
    p::Validator::new_with_features(p::WasmFeatures::all()).validate_all(&bytes)?;
    Ok(bytes)
}

impl Reencode for Consumer<'_> {
    type Error = std::convert::Infallible;
    fn global_index(&mut self, index: u32) -> std::result::Result<u32, Error> {
        Ok(index
            + if index as usize >= self.snapshot.imported_globals {
                self.extra_globals()
            } else {
                0
            })
    }
    fn table_index(&mut self, index: u32) -> std::result::Result<u32, Error> {
        Ok(index + if index >= self.imported_tables { 3 } else { 0 })
    }
    fn start_section(&mut self, _: u32) -> std::result::Result<u32, Error> {
        self.emitted[3] = true;
        Ok(self.snapshot.function_types.len() as u32)
    }
    fn intersperse_section_hook(
        &mut self,
        module: &mut e::Module,
        _: Option<e::SectionId>,
        before: Option<e::SectionId>,
    ) -> std::result::Result<(), Error> {
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
        if !self.emitted[0] && rank > 1 {
            let mut s = e::TypeSection::new();
            self.types(&mut s);
            module.section(&s);
        }
        if !self.emitted[1] && rank > 2 {
            let mut s = e::ImportSection::new();
            self.imports(&mut s)?;
            module.section(&s);
        }
        if !self.emitted[2] && rank > 3 {
            let mut s = e::FunctionSection::new();
            self.functions(&mut s);
            module.section(&s);
        }
        if !self.emitted[3] && rank > 9 {
            module.section(&e::StartSection {
                function_index: self.snapshot.function_types.len() as u32,
            });
            self.emitted[3] = true;
        }
        if !self.emitted[4] && rank > 10 {
            let mut s = e::ElementSection::new();
            self.elements(&mut s);
            module.section(&s);
        }
        if !self.emitted[5] && rank > 12 {
            let mut s = e::CodeSection::new();
            self.code(&mut s);
            module.section(&s);
        }
        Ok(())
    }
    fn parse_type_section(
        &mut self,
        s: &mut e::TypeSection,
        reader: p::TypeSectionReader<'_>,
    ) -> std::result::Result<(), Error> {
        reencode::utils::parse_type_section(self, s, reader)?;
        self.types(s);
        Ok(())
    }
    fn parse_import_section(
        &mut self,
        s: &mut e::ImportSection,
        reader: p::ImportSectionReader<'_>,
    ) -> std::result::Result<(), Error> {
        reencode::utils::parse_import_section(self, s, reader)?;
        self.imports(s)
    }
    fn parse_function_section(
        &mut self,
        s: &mut e::FunctionSection,
        reader: p::FunctionSectionReader<'_>,
    ) -> std::result::Result<(), Error> {
        reencode::utils::parse_function_section(self, s, reader)?;
        self.functions(s);
        Ok(())
    }
    fn parse_code_section(
        &mut self,
        s: &mut e::CodeSection,
        reader: p::CodeSectionReader<'_>,
    ) -> std::result::Result<(), Error> {
        reencode::utils::parse_code_section(self, s, reader)?;
        self.code(s);
        Ok(())
    }
    fn parse_element_section(
        &mut self,
        s: &mut e::ElementSection,
        reader: p::ElementSectionReader<'_>,
    ) -> std::result::Result<(), Error> {
        let mut imported = self.snapshot.imported_globals as u32 + self.plan.immutable.len() as u32;
        for (index, element) in reader.into_iter().enumerate() {
            let mut element = element?;
            if let Some((_, ty, names)) = self
                .plan
                .passive
                .iter()
                .find(|(segment, _, _)| *segment == index as u32)
            {
                let exprs = (imported..imported + names.len() as u32)
                    .map(e::ConstExpr::global_get)
                    .collect();
                imported += names.len() as u32;
                s.passive(e::Elements::Expressions(
                    reencode::RoundtripReencoder.ref_type(*ty)?,
                    std::borrow::Cow::Owned(exprs),
                ));
                continue;
            }
            if matches!(element.kind, p::ElementKind::Active { .. }) {
                element.kind = p::ElementKind::Declared;
            }
            self.parse_element(s, element)?;
        }
        self.elements(s);
        Ok(())
    }
    fn parse_global_section(
        &mut self,
        s: &mut e::GlobalSection,
        reader: p::GlobalSectionReader<'_>,
    ) -> std::result::Result<(), Error> {
        for (defined, global) in reader.into_iter().enumerate() {
            let global = global?;
            let index = self.snapshot.imported_globals as u32 + defined as u32;
            if let Some(import) = self
                .plan
                .immutable
                .iter()
                .position(|(original, _)| *original == index)
            {
                s.global(
                    self.global_type(global.ty)?,
                    &e::ConstExpr::global_get(
                        self.snapshot.imported_globals as u32 + import as u32,
                    ),
                );
            } else {
                self.parse_global(s, global)?;
            }
        }
        Ok(())
    }
    fn parse_table_section(
        &mut self,
        s: &mut e::TableSection,
        reader: p::TableSectionReader<'_>,
    ) -> std::result::Result<(), Error> {
        for (defined, table) in reader.into_iter().enumerate() {
            let mut table = table?;
            table.ty.initial = self.snapshot.tables[self.imported_tables as usize + defined]
                .1
                .len() as u64;
            self.parse_table(s, table)?;
        }
        Ok(())
    }
}

fn remap_global(ty: p::GlobalType, offset: u32) -> Result<p::GlobalType> {
    let mut ty = ty;
    if let ValType::Ref(r) = ty.content_type {
        let heap = match r.heap_type() {
            p::HeapType::Concrete(index) => p::HeapType::Concrete(p::UnpackedIndex::Module(
                index.as_module_index().unwrap() + offset,
            )),
            p::HeapType::Exact(index) => p::HeapType::Exact(p::UnpackedIndex::Module(
                index.as_module_index().unwrap() + offset,
            )),
            heap => heap,
        };
        ty.content_type = ValType::Ref(p::RefType::new(r.is_nullable(), heap).unwrap());
    }
    Ok(ty)
}

fn is_function(snapshot: &Snapshot, ty: p::RefType) -> bool {
    match ty.heap_type() {
        p::HeapType::Concrete(index) | p::HeapType::Exact(index) => matches!(
            snapshot.types[index.as_module_index().unwrap() as usize]
                .composite_type
                .inner,
            p::CompositeInnerType::Func(_)
        ),
        p::HeapType::Abstract {
            ty: p::AbstractHeapType::Func | p::AbstractHeapType::NoFunc,
            ..
        } => true,
        _ => false,
    }
}
