use super::capture::{Snapshot, Value};
use wasm_encoder::reencode::{self, Reencode};
use wasm_encoder::{self as e, Encode};
use wasmparser::{self as p, CompositeInnerType, ValType};
use wasmtime::{Result, bail, ensure};

type Error = reencode::Error<std::convert::Infallible>;

pub fn restore(source: &[u8], module_id: u32, snapshot: &Snapshot) -> Result<Vec<u8>> {
    if snapshot.tables.is_empty()
        && snapshot.passive.is_empty()
        && snapshot
            .globals
            .iter()
            .all(|(ty, _)| !matches!(ty.content_type, ValType::Ref(_)))
    {
        return Ok(source.to_vec());
    }
    let mut restorer = prepare(module_id, snapshot)?;
    let mut module = e::Module::new();
    restorer.parse_core_module(&mut module, p::Parser::new(0), source)?;
    let bytes = module.finish();
    p::Validator::new_with_features(p::WasmFeatures::all()).validate_all(&bytes)?;
    Ok(bytes)
}

pub fn helper(
    mut types: e::TypeSection,
    snapshot: &Snapshot,
    bodies: &[e::Function],
    roots: &[String],
) -> Result<Vec<u8>> {
    let mut restorer = prepare(0, snapshot)?;
    let mut module = e::Module::new();
    restorer.append_types(&mut types);
    module.section(&types);
    let mut functions = e::FunctionSection::new();
    for &ty in &snapshot.function_types {
        functions.function(ty);
    }
    restorer.append_functions(&mut functions);
    module.section(&functions);
    let mut tables = e::TableSection::new();
    for (ty, values) in &snapshot.tables {
        let mut ty = reencode::RoundtripReencoder.table_type(*ty)?;
        ty.minimum = values.len() as u64;
        tables.table(ty);
    }
    module.section(&tables);
    let mut globals = e::GlobalSection::new();
    restorer.append_globals(&mut globals);
    for ((ty, _), init) in snapshot.globals.iter().zip(&restorer.global_initializers) {
        globals.global(reencode::RoundtripReencoder.global_type(*ty)?, init);
    }
    module.section(&globals);
    let mut exports = e::ExportSection::new();
    for (index, name) in roots.iter().enumerate() {
        exports.export(
            name,
            e::ExportKind::Global,
            restorer.stable_count + index as u32,
        );
    }
    for (index, name) in ["roots-any", "roots-func", "functions"]
        .into_iter()
        .enumerate()
    {
        exports.export(name, e::ExportKind::Table, index as u32);
    }
    module.section(&exports);
    module.section(&e::StartSection {
        function_index: snapshot.function_types.len() as u32,
    });
    let mut elements = e::ElementSection::new();
    elements.declared(e::Elements::Functions(std::borrow::Cow::Owned(
        (0..snapshot.function_types.len() as u32).collect(),
    )));
    restorer.append_elements(&mut elements);
    module.section(&elements);
    let mut code = e::CodeSection::new();
    for body in bodies {
        code.function(body);
    }
    restorer.append_code(&mut code);
    module.section(&code);
    let bytes = module.finish();
    p::Validator::new_with_features(p::WasmFeatures::all()).validate_all(&bytes)?;
    Ok(bytes)
}

fn prepare(module_id: u32, snapshot: &Snapshot) -> Result<Restorer<'_>> {
    let mut stable = vec![false; snapshot.nodes.len()];
    let mut pending = Vec::new();
    for (ty, value) in &snapshot.globals {
        if !ty.mutable {
            if let Value::Object(id) = value {
                pending.push(*id);
            }
        }
    }
    for (_, _, values) in &snapshot.passive {
        for value in values {
            if let Value::Object(id) = value {
                pending.push(*id);
            }
        }
    }
    while let Some(id) = pending.pop() {
        if std::mem::replace(&mut stable[id as usize], true) {
            continue;
        }
        for (field, value) in fields(snapshot, id) {
            if !field.mutable {
                if let Value::Object(id) = value {
                    pending.push(*id);
                }
            }
        }
    }
    let mut order = Vec::new();
    let mut states = vec![0u8; snapshot.nodes.len()];
    for id in 0..snapshot.nodes.len() as u32 {
        let mut stack = vec![(id, false)];
        while let Some((id, leave)) = stack.pop() {
            if leave {
                states[id as usize] = 2;
                order.push(id);
                continue;
            }
            if states[id as usize] == 2 {
                continue;
            }
            ensure!(
                states[id as usize] == 0,
                "snapshot contains an immutable reference cycle"
            );
            states[id as usize] = 1;
            stack.push((id, true));
            for (field, value) in fields(snapshot, id) {
                if !field.mutable {
                    if let Value::Object(id) = value {
                        stack.push((*id, false));
                    }
                }
            }
        }
    }
    let mut stable_globals = vec![None; snapshot.nodes.len()];
    let mut node_locals = vec![None; snapshot.nodes.len()];
    let mut stable_count = 0;
    let mut local_types = Vec::new();
    for &id in &order {
        if stable[id as usize] {
            stable_globals[id as usize] = Some(snapshot.imported_globals as u32 + stable_count);
            stable_count += 1;
        } else {
            node_locals[id as usize] = Some(local_types.len() as u32);
            local_types.push((
                1,
                e::ValType::Ref(e::RefType {
                    nullable: true,
                    heap_type: e::HeapType::Concrete(snapshot.nodes[id as usize].type_index),
                }),
            ));
        }
    }
    let mut restorer = Restorer {
        snapshot,
        module_id,
        stable_count,
        stable_globals,
        node_locals,
        stable_initializers: Vec::new(),
        global_initializers: Vec::new(),
        start: e::Function::new(local_types),
        dummy_functions: Vec::new(),
        emitted_types: false,
        emitted_functions: false,
        emitted_globals: false,
        emitted_start: false,
        emitted_elements: false,
        emitted_code: false,
    };
    let mut body = Vec::new();
    for &id in &order {
        let mut code = Vec::new();
        restorer.allocate(id, &mut code)?;
        if let Some(global) = restorer.stable_globals[id as usize] {
            assert_eq!(
                global,
                snapshot.imported_globals as u32 + restorer.stable_initializers.len() as u32
            );
            restorer.stable_initializers.push((
                snapshot.nodes[id as usize].type_index,
                e::ConstExpr::raw(code),
            ));
        } else {
            body.extend(code);
            e::Instruction::LocalSet(restorer.node_locals[id as usize].unwrap()).encode(&mut body);
        }
    }
    for id in 0..snapshot.nodes.len() as u32 {
        let node = &snapshot.nodes[id as usize];
        let is_array = matches!(
            snapshot.types[node.type_index as usize]
                .composite_type
                .inner,
            CompositeInnerType::Array(_)
        );
        for (index, (field, value)) in fields(snapshot, id).into_iter().enumerate() {
            if !field.mutable
                || (!is_array
                    && !matches!(
                        value,
                        Value::Object(_) | Value::I31(_) | Value::Null | Value::Function(..)
                    ))
            {
                continue;
            }
            restorer.node_ref(id, &mut body);
            if is_array {
                e::Instruction::I32Const(index as i32).encode(&mut body);
            }
            restorer.value(value, field.element_type.unpack(), &mut body)?;
            if is_array {
                e::Instruction::ArraySet(node.type_index).encode(&mut body);
            } else {
                e::Instruction::StructSet {
                    struct_type_index: node.type_index,
                    field_index: index as u32,
                }
                .encode(&mut body);
            }
        }
    }
    for (index, (ty, value)) in snapshot
        .globals
        .iter()
        .enumerate()
        .skip(snapshot.imported_globals)
    {
        let mut init = Vec::new();
        if ty.mutable && matches!(ty.content_type, ValType::Ref(_)) {
            restorer.default(ty.content_type, &mut Vec::new(), &mut init)?;
            restorer.value(value, ty.content_type, &mut body)?;
            e::Instruction::GlobalSet(restorer.global_index(index as u32)?).encode(&mut body);
        } else {
            restorer.value(value, ty.content_type, &mut init)?;
        }
        restorer.global_initializers.push(e::ConstExpr::raw(init));
    }
    for (index, (ty, values)) in snapshot.tables.iter().enumerate() {
        for (offset, value) in values.iter().enumerate() {
            if ty.table64 {
                e::Instruction::I64Const(offset as i64).encode(&mut body);
            } else {
                e::Instruction::I32Const(offset as i32).encode(&mut body);
            }
            restorer.value(value, ValType::Ref(ty.element_type), &mut body)?;
            e::Instruction::TableSet(index as u32).encode(&mut body);
        }
    }
    e::Instruction::End.encode(&mut body);
    restorer.start.raw(body);
    Ok(restorer)
}

fn fields(snapshot: &Snapshot, id: u32) -> Vec<(p::FieldType, &Value)> {
    let node = &snapshot.nodes[id as usize];
    match &snapshot.types[node.type_index as usize]
        .composite_type
        .inner
    {
        CompositeInnerType::Struct(s) => s.fields.iter().copied().zip(&node.fields).collect(),
        CompositeInnerType::Array(a) => node.fields.iter().map(|v| (a.0, v)).collect(),
        _ => unreachable!(),
    }
}

struct Restorer<'a> {
    snapshot: &'a Snapshot,
    module_id: u32,
    stable_count: u32,
    stable_globals: Vec<Option<u32>>,
    node_locals: Vec<Option<u32>>,
    stable_initializers: Vec<(u32, e::ConstExpr)>,
    global_initializers: Vec<e::ConstExpr>,
    start: e::Function,
    dummy_functions: Vec<u32>,
    emitted_types: bool,
    emitted_functions: bool,
    emitted_globals: bool,
    emitted_start: bool,
    emitted_elements: bool,
    emitted_code: bool,
}

impl Restorer<'_> {
    fn node_ref(&self, id: u32, code: &mut Vec<u8>) {
        if let Some(global) = self.stable_globals[id as usize] {
            e::Instruction::GlobalGet(global).encode(code);
        } else {
            e::Instruction::LocalGet(self.node_locals[id as usize].unwrap()).encode(code);
            e::Instruction::RefAsNonNull.encode(code);
        }
    }

    fn value(&self, value: &Value, ty: ValType, code: &mut Vec<u8>) -> Result<()> {
        use e::Instruction as I;
        match value {
            Value::Bits(bits) => match ty {
                ValType::I32 => I::I32Const(*bits as i32).encode(code),
                ValType::I64 => I::I64Const(*bits as i64).encode(code),
                ValType::F32 => I::F32Const(e::Ieee32::new(*bits as u32)).encode(code),
                ValType::F64 => I::F64Const(e::Ieee64::new(*bits as u64)).encode(code),
                ValType::V128 => I::V128Const(*bits as i128).encode(code),
                _ => bail!("scalar snapshot value has reference type"),
            },
            Value::Null => {
                let ValType::Ref(ty) = ty else {
                    bail!("null scalar")
                };
                I::RefNull(reencode::RoundtripReencoder.heap_type(ty.heap_type())?).encode(code);
            }
            Value::I31(value) => {
                I::I32Const(*value).encode(code);
                I::RefI31.encode(code);
            }
            Value::Object(id) => self.node_ref(*id, code),
            Value::Function(module, function) => {
                ensure!(
                    *module == self.module_id,
                    "cross-module function restoration needs a dispatch proxy"
                );
                I::RefFunc(*function).encode(code);
            }
        }
        if !matches!(value, Value::Null)
            && matches!(ty, ValType::Ref(r) if matches!(r.heap_type(), p::HeapType::Abstract { ty: p::AbstractHeapType::Extern | p::AbstractHeapType::NoExtern, .. }))
        {
            I::ExternConvertAny.encode(code);
        }
        Ok(())
    }

    fn allocate(&mut self, id: u32, code: &mut Vec<u8>) -> Result<()> {
        let node = &self.snapshot.nodes[id as usize];
        let ty = node.type_index;
        match &self.snapshot.types[ty as usize].composite_type.inner {
            CompositeInnerType::Struct(_) => {
                for (field, value) in fields(self.snapshot, id) {
                    if field.mutable
                        && matches!(field.element_type, p::StorageType::Val(ValType::Ref(_)))
                    {
                        self.default(field.element_type.unpack(), &mut Vec::new(), code)?;
                    } else {
                        self.value(value, field.element_type.unpack(), code)?;
                    }
                }
                e::Instruction::StructNew(ty).encode(code);
            }
            CompositeInnerType::Array(a) => {
                if a.0.mutable && !node.fields.is_empty() {
                    self.default(a.0.element_type.unpack(), &mut Vec::new(), code)?;
                    e::Instruction::I32Const(node.fields.len() as i32).encode(code);
                    e::Instruction::ArrayNew(ty).encode(code);
                } else {
                    for value in &node.fields {
                        self.value(value, a.0.element_type.unpack(), code)?;
                    }
                    e::Instruction::ArrayNewFixed {
                        array_type_index: ty,
                        array_size: node.fields.len() as u32,
                    }
                    .encode(code);
                }
            }
            _ => unreachable!(),
        }
        Ok(())
    }

    fn default(&mut self, ty: ValType, path: &mut Vec<u32>, code: &mut Vec<u8>) -> Result<()> {
        use e::Instruction as I;
        let ValType::Ref(r) = ty else {
            return self.value(&Value::Bits(0), ty, code);
        };
        if r.is_nullable() {
            I::RefNull(reencode::RoundtripReencoder.heap_type(r.heap_type())?).encode(code);
            return Ok(());
        }
        match r.heap_type() {
            p::HeapType::Concrete(index) | p::HeapType::Exact(index) => {
                let index = index.as_module_index().unwrap();
                match &self.snapshot.types[index as usize].composite_type.inner {
                    CompositeInnerType::Func(_) => {
                        let dummy = if let Some(i) =
                            self.dummy_functions.iter().position(|t| *t == index)
                        {
                            i
                        } else {
                            self.dummy_functions.push(index);
                            self.dummy_functions.len() - 1
                        };
                        I::RefFunc(self.snapshot.function_types.len() as u32 + 1 + dummy as u32)
                            .encode(code);
                    }
                    CompositeInnerType::Array(_) => I::ArrayNewFixed {
                        array_type_index: index,
                        array_size: 0,
                    }
                    .encode(code),
                    CompositeInnerType::Struct(s) => {
                        ensure!(
                            !path.contains(&index),
                            "cannot construct a finite placeholder for non-nullable type {index}"
                        );
                        path.push(index);
                        for field in &s.fields {
                            self.default(field.element_type.unpack(), path, code)?;
                        }
                        path.pop();
                        I::StructNew(index).encode(code);
                    }
                    _ => bail!("unsupported non-nullable placeholder type"),
                }
            }
            p::HeapType::Abstract {
                ty: p::AbstractHeapType::Any | p::AbstractHeapType::Eq | p::AbstractHeapType::I31,
                ..
            } => {
                I::I32Const(0).encode(code);
                I::RefI31.encode(code);
            }
            p::HeapType::Abstract {
                ty: p::AbstractHeapType::Func,
                ..
            } => I::RefFunc(self.snapshot.function_types.len() as u32).encode(code),
            p::HeapType::Abstract {
                ty: p::AbstractHeapType::Extern,
                ..
            } => {
                I::I32Const(0).encode(code);
                I::RefI31.encode(code);
                I::ExternConvertAny.encode(code);
            }
            p::HeapType::Abstract {
                ty: p::AbstractHeapType::Struct | p::AbstractHeapType::Array,
                ..
            } => {
                let want_array = matches!(
                    r.heap_type(),
                    p::HeapType::Abstract {
                        ty: p::AbstractHeapType::Array,
                        ..
                    }
                );
                let candidates = self
                    .snapshot
                    .types
                    .iter()
                    .enumerate()
                    .filter(|(_, t)| {
                        if want_array {
                            matches!(t.composite_type.inner, CompositeInnerType::Array(_))
                        } else {
                            matches!(t.composite_type.inner, CompositeInnerType::Struct(_))
                        }
                    })
                    .map(|(i, _)| i as u32)
                    .collect::<Vec<_>>();
                for index in candidates {
                    let mut trial = Vec::new();
                    let dummy = self.dummy_functions.len();
                    if self
                        .default(
                            ValType::Ref(
                                p::RefType::new(
                                    false,
                                    p::HeapType::Concrete(p::UnpackedIndex::Module(index)),
                                )
                                .unwrap(),
                            ),
                            &mut path.clone(),
                            &mut trial,
                        )
                        .is_ok()
                    {
                        code.extend(trial);
                        return Ok(());
                    }
                    self.dummy_functions.truncate(dummy);
                }
                bail!("cannot construct an abstract non-nullable placeholder");
            }
            _ => bail!("unsupported non-nullable placeholder"),
        }
        Ok(())
    }

    fn append_types(&mut self, types: &mut e::TypeSection) {
        self.emitted_types = true;
        types.ty().function([], []);
    }
    fn append_functions(&mut self, functions: &mut e::FunctionSection) {
        self.emitted_functions = true;
        functions.function(self.snapshot.types.len() as u32);
        for &ty in &self.dummy_functions {
            functions.function(ty);
        }
    }
    fn append_globals(&mut self, globals: &mut e::GlobalSection) {
        self.emitted_globals = true;
        for (ty, init) in &self.stable_initializers {
            globals.global(
                e::GlobalType {
                    val_type: e::ValType::Ref(e::RefType {
                        nullable: false,
                        heap_type: e::HeapType::Concrete(*ty),
                    }),
                    mutable: false,
                    shared: false,
                },
                init,
            );
        }
    }
    fn append_elements(&mut self, elements: &mut e::ElementSection) {
        self.emitted_elements = true;
        elements.declared(e::Elements::Functions(std::borrow::Cow::Owned(
            (self.snapshot.function_types.len() as u32
                ..=self.snapshot.function_types.len() as u32 + self.dummy_functions.len() as u32)
                .collect(),
        )));
    }
    fn append_code(&mut self, code: &mut e::CodeSection) {
        self.emitted_code = true;
        code.function(&self.start);
        for _ in &self.dummy_functions {
            let mut function = e::Function::new([]);
            function.instruction(&e::Instruction::Unreachable);
            function.instruction(&e::Instruction::End);
            code.function(&function);
        }
    }
}

impl Reencode for Restorer<'_> {
    type Error = std::convert::Infallible;
    fn global_index(&mut self, index: u32) -> std::result::Result<u32, Error> {
        Ok(index
            + if index as usize >= self.snapshot.imported_globals {
                self.stable_count
            } else {
                0
            })
    }
    fn start_section(&mut self, _: u32) -> std::result::Result<u32, Error> {
        self.emitted_start = true;
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
        if !self.emitted_types && rank > 1 {
            let mut s = e::TypeSection::new();
            self.append_types(&mut s);
            module.section(&s);
        }
        if !self.emitted_functions && rank > 3 {
            let mut s = e::FunctionSection::new();
            self.append_functions(&mut s);
            module.section(&s);
        }
        if !self.emitted_globals && rank > 7 {
            let mut s = e::GlobalSection::new();
            self.append_globals(&mut s);
            module.section(&s);
        }
        if !self.emitted_start && rank > 9 {
            module.section(&e::StartSection {
                function_index: self.snapshot.function_types.len() as u32,
            });
            self.emitted_start = true;
        }
        if !self.emitted_elements && rank > 10 {
            let mut s = e::ElementSection::new();
            self.append_elements(&mut s);
            module.section(&s);
        }
        if !self.emitted_code && rank > 12 {
            let mut s = e::CodeSection::new();
            self.append_code(&mut s);
            module.section(&s);
        }
        Ok(())
    }
    fn parse_type_section(
        &mut self,
        types: &mut e::TypeSection,
        reader: p::TypeSectionReader<'_>,
    ) -> std::result::Result<(), Error> {
        reencode::utils::parse_type_section(self, types, reader)?;
        self.append_types(types);
        Ok(())
    }
    fn parse_function_section(
        &mut self,
        functions: &mut e::FunctionSection,
        reader: p::FunctionSectionReader<'_>,
    ) -> std::result::Result<(), Error> {
        reencode::utils::parse_function_section(self, functions, reader)?;
        self.append_functions(functions);
        Ok(())
    }
    fn parse_code_section(
        &mut self,
        code: &mut e::CodeSection,
        reader: p::CodeSectionReader<'_>,
    ) -> std::result::Result<(), Error> {
        reencode::utils::parse_code_section(self, code, reader)?;
        self.append_code(code);
        Ok(())
    }
    fn parse_element_section(
        &mut self,
        elements: &mut e::ElementSection,
        reader: p::ElementSectionReader<'_>,
    ) -> std::result::Result<(), Error> {
        for (index, element) in reader.into_iter().enumerate() {
            let element = element?;
            if let Some((_, ty, values)) = self
                .snapshot
                .passive
                .iter()
                .find(|(segment, _, _)| *segment == index as u32)
            {
                let mut exprs = Vec::new();
                for value in values {
                    let mut code = Vec::new();
                    self.value(value, ValType::Ref(*ty), &mut code).unwrap();
                    exprs.push(e::ConstExpr::raw(code));
                }
                elements.passive(e::Elements::Expressions(
                    self.ref_type(*ty)?,
                    std::borrow::Cow::Owned(exprs),
                ));
            } else {
                self.parse_element(elements, element)?;
            }
        }
        self.append_elements(elements);
        Ok(())
    }
    fn parse_global_section(
        &mut self,
        globals: &mut e::GlobalSection,
        _: p::GlobalSectionReader<'_>,
    ) -> std::result::Result<(), Error> {
        self.append_globals(globals);
        for ((ty, _), init) in self
            .snapshot
            .globals
            .iter()
            .skip(self.snapshot.imported_globals)
            .zip(&self.global_initializers)
        {
            globals.global(reencode::RoundtripReencoder.global_type(*ty)?, init);
        }
        Ok(())
    }
    fn parse_table_section(
        &mut self,
        tables: &mut e::TableSection,
        reader: p::TableSectionReader<'_>,
    ) -> std::result::Result<(), Error> {
        for (index, table) in reader.into_iter().enumerate() {
            let mut table = table?;
            table.ty.initial = self.snapshot.tables[index].1.len() as u64;
            self.parse_table(tables, table)?;
        }
        Ok(())
    }
}
