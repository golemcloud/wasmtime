pub(crate) mod capture;
#[cfg(feature = "component-model")]
pub(crate) mod component_rewrite;
pub(crate) mod lower;
pub(crate) mod restore;
#[cfg(feature = "component-model")]
pub(crate) mod shared_restore;
#[cfg(feature = "component-model")]
mod thread_tables;

pub(crate) fn needs_snapshot(module: &crate::ModuleContext<'_>) -> wasmtime::Result<bool> {
    use wasmparser::{ElementKind, TypeRef, ValType};
    if module
        .defined_globals()
        .any(|(_, ty, _)| matches!(ty.content_type, ValType::Ref(_)))
        || module.imports().iter().any(|import| match import.ty {
            TypeRef::Global(ty) => matches!(ty.content_type, ValType::Ref(_)),
            TypeRef::Table(_) => true,
            _ => false,
        })
    {
        return Ok(true);
    }
    for section in module.raw_sections() {
        if section.id == wasm_encoder::SectionId::Table as u8
            && wasmparser::TableSectionReader::new(wasmparser::BinaryReader::new(section.data, 0))?
                .count()
                != 0
        {
            return Ok(true);
        }
        if section.id == wasm_encoder::SectionId::Element as u8 {
            let reader = wasmparser::ElementSectionReader::new(wasmparser::BinaryReader::new(
                section.data,
                0,
            ))?;
            for element in reader {
                if matches!(element?.kind, ElementKind::Passive) {
                    return Ok(true);
                }
            }
        }
    }
    Ok(false)
}

pub(crate) fn original(module: &crate::ModuleContext<'_>) -> Vec<u8> {
    let mut bytes = wasm_encoder::Module::new();
    for section in module.raw_sections() {
        bytes.section(section);
    }
    bytes.finish()
}

#[cfg(feature = "component-model")]
pub(crate) fn pool() -> Vec<u8> {
    let mut module = wasm_encoder::Module::new();
    let mut table = wasm_encoder::TableSection::new();
    table.table(wasm_encoder::TableType {
        element_type: wasm_encoder::RefType::EQREF,
        minimum: 0,
        maximum: None,
        shared: false,
        table64: false,
    });
    module.section(&table);
    let mut export = wasm_encoder::ExportSection::new();
    export.export("objects", wasm_encoder::ExportKind::Table, 0);
    module.section(&export);
    module.finish()
}
