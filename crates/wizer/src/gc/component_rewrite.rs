use std::collections::BTreeMap;
use wasm_encoder::{
    self as e,
    reencode::{self, Reencode, ReencodeComponent},
};
use wasmparser as p;
use wasmtime::Result;

type Error = reencode::Error<std::convert::Infallible>;

pub fn inject(
    source: &[u8],
    helper: &[u8],
    replacements: &BTreeMap<u32, Vec<u8>>,
    accessors: &[(u32, u32)],
    state_import: &str,
) -> Result<Vec<u8>> {
    let mut output = e::Component::new();
    output.section(&e::RawSection {
        id: e::ComponentSectionId::CoreModule as u8,
        data: helper,
    });
    let mut instance = e::InstanceSection::new();
    instance.instantiate(0, [] as [(&str, e::ModuleArg); 0]);
    output.section(&instance);
    let mut r = Rewriter {
        replacements,
        state_import,
        module: 0,
    };
    r.parse_component(&mut output, p::Parser::new(0), source)?;
    if !accessors.is_empty() {
        let (types, funcs, core_funcs) = counts(source)?;
        let mut ty = e::ComponentTypeSection::new();
        ty.function()
            .params([
                (
                    "op",
                    e::ComponentValType::Primitive(e::PrimitiveValType::U32),
                ),
                (
                    "target",
                    e::ComponentValType::Primitive(e::PrimitiveValType::U32),
                ),
                (
                    "index",
                    e::ComponentValType::Primitive(e::PrimitiveValType::U64),
                ),
                (
                    "part",
                    e::ComponentValType::Primitive(e::PrimitiveValType::U32),
                ),
            ])
            .result(Some(e::ComponentValType::Primitive(
                e::PrimitiveValType::U64,
            )));
        output.section(&ty);
        let mut aliases = e::ComponentAliasSection::new();
        let mut canon = e::CanonicalFunctionSection::new();
        let mut exports = e::ComponentExportSection::new();
        for (ordinal, &(module, instance)) in accessors.iter().enumerate() {
            aliases.alias(e::Alias::CoreInstanceExport {
                instance: instance + 1,
                kind: e::ExportKind::Func,
                name: "__wizer_gc_read",
            });
            canon.lift(core_funcs + ordinal as u32, types, []);
            exports.export(
                &format!("wizer-gc-read{module}"),
                e::ComponentExportKind::Func,
                funcs + ordinal as u32,
                None,
            );
        }
        output.section(&aliases);
        output.section(&canon);
        output.section(&exports);
    }
    let bytes = output.finish();
    let bytes = if accessors.is_empty() {
        bytes
    } else {
        super::thread_tables::adapt(&bytes)?
    };
    p::Validator::new_with_features(p::WasmFeatures::all()).validate_all(&bytes)?;
    Ok(bytes)
}

struct Rewriter<'a> {
    replacements: &'a BTreeMap<u32, Vec<u8>>,
    state_import: &'a str,
    module: u32,
}
impl Reencode for Rewriter<'_> {
    type Error = std::convert::Infallible;
}
impl ReencodeComponent for Rewriter<'_> {
    fn module_index(&mut self, index: u32) -> u32 {
        index + 1
    }
    fn instance_index(&mut self, index: u32) -> u32 {
        index + 1
    }
    fn parse_component_submodule(
        &mut self,
        dst: &mut e::Component,
        _: p::Parser,
        bytes: &[u8],
    ) -> std::result::Result<(), Error> {
        let bytes = self
            .replacements
            .get(&self.module)
            .map_or(bytes, Vec::as_slice);
        dst.section(&e::RawSection {
            id: e::ComponentSectionId::CoreModule as u8,
            data: bytes,
        });
        self.module += 1;
        Ok(())
    }
    fn parse_component_subcomponent(
        &mut self,
        dst: &mut e::Component,
        _: p::Parser,
        bytes: &[u8],
        _: &[u8],
    ) -> std::result::Result<(), Error> {
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
    ) -> std::result::Result<(), Error> {
        if let p::Instance::Instantiate { module_index, args } = instance {
            let mut args: Vec<_> = args
                .iter()
                .map(|a| (a.name, e::ModuleArg::Instance(a.index + 1)))
                .collect();
            if self.replacements.contains_key(&module_index) {
                // This name is absent from the original imports, but the
                // component may still supply an unused argument with it.
                args.retain(|(name, _)| *name != self.state_import);
                args.push((self.state_import, e::ModuleArg::Instance(0)));
            }
            dst.instantiate(module_index + 1, args);
            Ok(())
        } else {
            reencode::component_utils::parse_instance(self, dst, instance)
        }
    }
}

fn counts(source: &[u8]) -> Result<(u32, u32, u32)> {
    let (mut types, mut funcs, mut core_funcs) = (0, 0, 0);
    let mut depth = 0;
    for payload in p::Parser::new(0).parse_all(source) {
        let payload = payload?;
        if matches!(
            payload,
            p::Payload::ModuleSection { .. } | p::Payload::ComponentSection { .. }
        ) {
            depth += 1;
            continue;
        }
        if depth != 0 {
            if matches!(payload, p::Payload::End(_)) {
                depth -= 1;
            }
            continue;
        }
        match payload {
            p::Payload::ComponentTypeSection(reader) => types += reader.count(),
            p::Payload::ComponentExportSection(reader) => {
                for export in reader {
                    match export?.kind {
                        p::ComponentExternalKind::Func => funcs += 1,
                        p::ComponentExternalKind::Type => types += 1,
                        _ => {}
                    }
                }
            }
            p::Payload::ComponentImportSection(reader) => {
                for import in reader {
                    match import?.ty {
                        p::ComponentTypeRef::Func(_) => funcs += 1,
                        p::ComponentTypeRef::Type(_) => types += 1,
                        _ => {}
                    }
                }
            }
            p::Payload::ComponentAliasSection(reader) => {
                for alias in reader {
                    match alias? {
                        p::ComponentAlias::CoreInstanceExport {
                            kind: p::ExternalKind::Func | p::ExternalKind::FuncExact,
                            ..
                        } => core_funcs += 1,
                        p::ComponentAlias::InstanceExport {
                            kind: p::ComponentExternalKind::Func,
                            ..
                        } => funcs += 1,
                        p::ComponentAlias::InstanceExport {
                            kind: p::ComponentExternalKind::Type,
                            ..
                        }
                        | p::ComponentAlias::Outer {
                            kind: p::ComponentOuterAliasKind::Type,
                            ..
                        } => types += 1,
                        _ => {}
                    }
                }
            }
            p::Payload::ComponentCanonicalSection(reader) => {
                for function in reader {
                    if matches!(function?, p::CanonicalFunction::Lift { .. }) {
                        funcs += 1;
                    } else {
                        core_funcs += 1;
                    }
                }
            }
            _ => {}
        }
    }
    Ok((types, funcs, core_funcs))
}
