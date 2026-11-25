use wasmparser::{self as p, CompositeInnerType, Payload, ValType};
use wasmtime::Result;

/// Reads scalar GC state from one core module, as [`crate::InstanceState::gc_read`] does.
///
/// Capture takes this trait rather than an async closure so that its futures are `Send`:
/// rustc cannot prove that the future returned by an `AsyncFnMut` call is `Send`.
pub trait GcReader {
    fn read(
        &mut self,
        op: u32,
        target: u32,
        index: u64,
        part: u32,
    ) -> impl Future<Output = Result<u64>> + Send;
}

/// Reads scalar GC state from any core module of a component, as
/// [`crate::ComponentInstanceState::gc_read`] does.
#[cfg(feature = "component-model")]
pub trait SharedGcReader {
    fn read(
        &mut self,
        module: u32,
        op: u32,
        target: u32,
        index: u64,
        part: u32,
    ) -> impl Future<Output = Result<u64>> + Send;
}

impl<S: crate::InstanceState> GcReader for S {
    fn read(
        &mut self,
        op: u32,
        target: u32,
        index: u64,
        part: u32,
    ) -> impl Future<Output = Result<u64>> + Send {
        self.gc_read(op, target, index, part)
    }
}

#[cfg(feature = "component-model")]
impl<S: crate::ComponentInstanceState> SharedGcReader for S {
    fn read(
        &mut self,
        module: u32,
        op: u32,
        target: u32,
        index: u64,
        part: u32,
    ) -> impl Future<Output = Result<u64>> + Send {
        self.gc_read(module, op, target, index, part)
    }
}

/// Reads one module's state through a component-level reader.
#[cfg(feature = "component-model")]
struct ModuleReader<'a, R> {
    shared: &'a mut R,
    module: u32,
}

#[cfg(feature = "component-model")]
impl<R: SharedGcReader> GcReader for ModuleReader<'_, R> {
    fn read(
        &mut self,
        op: u32,
        target: u32,
        index: u64,
        part: u32,
    ) -> impl Future<Output = Result<u64>> + Send {
        self.shared.read(self.module, op, target, index, part)
    }
}

#[derive(Clone, Debug)]
pub enum Value {
    Bits(u128),
    Null,
    I31(i32),
    Object(u32),
    Function(u32, u32),
}

#[derive(Clone, Debug)]
pub struct Node {
    pub module_id: u32,
    pub type_index: u32,
    pub fields: Vec<Value>,
}

#[derive(Clone)]
pub struct Snapshot {
    pub types: Vec<p::SubType>,
    pub globals: Vec<(p::GlobalType, Value)>,
    pub tables: Vec<(p::TableType, Vec<Value>)>,
    pub nodes: Vec<Node>,
    pub function_types: Vec<u32>,
    pub imported_globals: usize,
    pub passive: Vec<(u32, p::RefType, Vec<Value>)>,
}

async fn value(
    ty: ValType,
    op: u32,
    target: u32,
    index: u64,
    read: &mut impl GcReader,
) -> Result<Value> {
    let low = read.read(op, target, index, 0).await?;
    Ok(match ty {
        ValType::Ref(_) => {
            let bits = read
                .read(op, target, index, if low == 3 { 2 } else { 1 })
                .await?;
            match low {
                0 => Value::Null,
                1 => Value::I31(bits as i32),
                2 => Value::Object(bits as u32),
                3 => Value::Function((bits >> 32) as u32, bits as u32),
                _ => wasmtime::bail!(
                    "snapshot contains an unsupported host-owned reference at operation {op}, root {target}, field {index}"
                ),
            }
        }
        ValType::V128 => Value::Bits(
            u128::from(low) | (u128::from(read.read(op, target, index, 1).await?) << 64),
        ),
        _ => Value::Bits(u128::from(low)),
    })
}

pub async fn capture(
    original: &[u8],
    module_id: u32,
    read: &mut impl GcReader,
) -> Result<Snapshot> {
    let mut snapshot = capture_roots(original, read).await?;
    while (snapshot.nodes.len() as u64) < read.read(6, 0, 0, 0).await? {
        let index = snapshot.nodes.len() as u32;
        let owner = read.read(3, index, 0, 0).await?;
        wasmtime::ensure!(
            (owner >> 32) as u32 == module_id,
            "cross-module object needs its owning module's accessor"
        );
        let type_index = owner as u32;
        let len = read.read(4, index, 0, 0).await?;
        let mut fields = Vec::new();
        for i in 0..len {
            let ty = field_type(&snapshot.types[type_index as usize], i)?;
            fields.push(value(ty, 5, index, i, read).await?);
        }
        snapshot.nodes.push(Node {
            module_id,
            type_index,
            fields,
        });
    }
    Ok(snapshot)
}

pub struct Graph {
    pub modules: Vec<(u32, Snapshot)>,
    pub nodes: Vec<Node>,
}

#[cfg(feature = "component-model")]
pub async fn capture_shared(
    originals: &[(u32, &[u8])],
    read: &mut impl SharedGcReader,
) -> Result<Graph> {
    let mut modules = Vec::new();
    for &(module_id, original) in originals {
        let mut module = ModuleReader {
            shared: &mut *read,
            module: module_id,
        };
        let snapshot = capture_roots(original, &mut module).await?;
        modules.push((module_id, snapshot));
    }
    let registry = originals
        .first()
        .ok_or_else(|| wasmtime::format_err!("no modules to capture"))?
        .0;
    let mut nodes = Vec::new();
    while (nodes.len() as u64) < read.read(registry, 6, 0, 0, 0).await? {
        let index = nodes.len() as u32;
        let owner = read.read(registry, 3, index, 0, 0).await?;
        let module_id = (owner >> 32) as u32;
        let type_index = owner as u32;
        let module = &modules
            .iter()
            .find(|(id, _)| *id == module_id)
            .ok_or_else(|| {
                wasmtime::format_err!("object {index} has unknown owning module {module_id}")
            })?
            .1;
        let len = read.read(module_id, 4, index, 0, 0).await?;
        let mut fields = Vec::new();
        for i in 0..len {
            let ty = field_type(&module.types[type_index as usize], i)?;
            fields.push(
                value(
                    ty,
                    5,
                    index,
                    i,
                    &mut ModuleReader {
                        shared: &mut *read,
                        module: module_id,
                    },
                )
                .await?,
            );
        }
        nodes.push(Node {
            module_id,
            type_index,
            fields,
        });
    }
    Ok(Graph { modules, nodes })
}

fn field_type(ty: &p::SubType, index: u64) -> Result<ValType> {
    match &ty.composite_type.inner {
        CompositeInnerType::Struct(s) => Ok(s.fields[index as usize].element_type.unpack()),
        CompositeInnerType::Array(a) => Ok(a.0.element_type.unpack()),
        _ => wasmtime::bail!("object registry entry is not a struct or array"),
    }
}

async fn capture_roots(original: &[u8], read: &mut impl GcReader) -> Result<Snapshot> {
    let mut types = Vec::new();
    let mut global_types = Vec::new();
    let mut table_types = Vec::new();
    let mut function_types = Vec::new();
    let mut imported_globals = 0;
    let mut passive_types = Vec::new();
    for payload in p::Parser::new(0).parse_all(original) {
        match payload? {
            Payload::TypeSection(section) => {
                for group in section {
                    types.extend(group?.into_types());
                }
            }
            Payload::ImportSection(section) => {
                for import in section.into_imports() {
                    match import?.ty {
                        p::TypeRef::Func(ty) | p::TypeRef::FuncExact(ty) => function_types.push(ty),
                        p::TypeRef::Global(ty) => {
                            global_types.push(ty);
                            imported_globals += 1;
                        }
                        p::TypeRef::Table(ty) => table_types.push(ty),
                        _ => {}
                    }
                }
            }
            Payload::FunctionSection(section) => {
                function_types.extend(
                    section
                        .into_iter()
                        .collect::<std::result::Result<Vec<_>, _>>()?,
                );
            }
            Payload::GlobalSection(section) => {
                for global in section {
                    global_types.push(global?.ty);
                }
            }
            Payload::TableSection(section) => {
                for table in section {
                    table_types.push(table?.ty);
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
                    passive_types.push((index as u32, ty, len));
                }
            }
            _ => {}
        }
    }
    let mut globals = Vec::new();
    for (index, ty) in global_types.into_iter().enumerate() {
        globals.push((ty, value(ty.content_type, 0, index as u32, 0, read).await?));
    }
    let mut tables = Vec::new();
    for (index, ty) in table_types.into_iter().enumerate() {
        let len = read.read(1, index as u32, 0, 0).await?;
        let mut elements = Vec::new();
        for i in 0..len {
            elements.push(value(ValType::Ref(ty.element_type), 2, index as u32, i, read).await?);
        }
        tables.push((ty, elements));
    }
    let mut passive = Vec::new();
    for (segment, ty, len) in passive_types {
        let mut values = Vec::new();
        for index in 0..len {
            values.push(value(ValType::Ref(ty), 7, segment, u64::from(index), read).await?);
        }
        passive.push((segment, ty, values));
    }
    Ok(Snapshot {
        types,
        globals,
        tables,
        nodes: Vec::new(),
        function_types,
        imported_globals,
        passive,
    })
}
