use crate::Wizer;
use ::wasmtime::{Result, bail};

mod info;
mod instrument;
mod parse;
mod rewrite;
mod snapshot;
#[cfg(feature = "wasmtime")]
mod wasmtime;
#[cfg(feature = "wasmtime")]
pub use wasmtime::*;

const WIZER_INSTANCE: &str = "wasmtime:wizer/access";

pub use self::info::ComponentContext;

impl Wizer {
    /// Same as [`Wizer::instrument`], except for components.
    pub fn instrument_component<'a>(
        &self,
        wasm: &'a [u8],
    ) -> Result<(ComponentContext<'a>, Vec<u8>)> {
        // Make sure we're given valid Wasm from the get go.
        self.wasm_validate(&wasm)?;

        let mut cx = parse::parse(wasm)?;
        let instrumented_wasm = instrument::instrument(&mut cx)?;
        if !cx.needs_gc_snapshot()? {
            self.debug_assert_valid_wasm(&instrumented_wasm, "instrumented component");
            return Ok((cx, instrumented_wasm));
        }
        let mut replacements = std::collections::BTreeMap::new();
        let mut accessors = Vec::new();
        for (id, module) in cx.core_modules() {
            let Some(&instance) = cx.core_instantiations.get(&id) else {
                continue;
            };
            let mut module = module.clone();
            let bytes = crate::instrument::instrument(&mut module);
            replacements.insert(id, crate::gc::lower::component_module(&bytes, id)?);
            accessors.push((id, instance));
        }
        let instrumented_wasm = crate::gc::component_rewrite::inject(
            &instrumented_wasm,
            &crate::gc::pool(),
            &replacements,
            &accessors,
            "__wizer_gc_state",
        )?;
        self.debug_assert_valid_wasm(&instrumented_wasm, "instrumented component");

        Ok((cx, instrumented_wasm))
    }

    /// Same as [`Wizer::snapshot`], except for components.
    pub async fn snapshot_component(
        &self,
        cx: &ComponentContext<'_>,
        instance: &mut impl ComponentInstanceState,
    ) -> Result<Vec<u8>> {
        if !self.func_renames.is_empty() {
            bail!("components do not support renaming functions");
        }

        let snapshot = snapshot::snapshot(cx, instance).await;
        if !cx.needs_gc_snapshot()? {
            let rewritten_wasm = self.rewrite_component(cx, &snapshot);
            self.debug_assert_valid_wasm(&rewritten_wasm, "rewritten component");
            return Ok(rewritten_wasm);
        }
        let originals: Vec<_> = cx
            .core_modules()
            .filter(|(id, _)| cx.core_instantiations.contains_key(id))
            .map(|(id, module)| (id, crate::gc::original(module)))
            .collect();
        let inputs: Vec<_> = originals
            .iter()
            .map(|(id, bytes)| (*id, bytes.as_slice()))
            .collect();
        let graph = if inputs.is_empty() {
            None
        } else {
            Some(crate::gc::capture::capture_shared(&inputs, &mut *instance).await?)
        };
        let rewritten_wasm = self.rewrite_component(cx, &snapshot);
        let rewritten_wasm = if let Some(graph) = graph {
            let rewritten_modules: Vec<_> = cx
                .core_modules()
                .filter_map(|(id, module)| {
                    let snapshot = &snapshot.modules.iter().find(|(index, _)| *index == id)?.1;
                    Some((
                        id,
                        self.rewrite(module, snapshot, &crate::FuncRenames::default(), false),
                    ))
                })
                .collect();
            let inputs: Vec<_> = rewritten_modules
                .iter()
                .map(|(id, bytes)| (*id, bytes.as_slice()))
                .collect();
            let (modules, state_import) = crate::gc::shared_restore::restore(&inputs, &graph)?;
            let mut modules = modules.into_iter();
            let helper = modules.next().unwrap();
            let replacements = rewritten_modules
                .iter()
                .map(|(id, _)| *id)
                .zip(modules)
                .collect();
            crate::gc::component_rewrite::inject(
                &rewritten_wasm,
                &helper,
                &replacements,
                &[],
                &state_import,
            )?
        } else {
            rewritten_wasm
        };
        self.debug_assert_valid_wasm(&rewritten_wasm, "rewritten component");

        Ok(rewritten_wasm)
    }
}

/// Trait representing the ability to invoke functions on a component to learn
/// about its internal state.
pub trait ComponentInstanceState: Send {
    /// Calls the scalar GC accessor for the specified core module.
    ///
    /// Uses the operations, value encodings, and capture lifetime documented
    /// by [`crate::InstanceState::gc_read`], with these component-specific rules:
    ///
    /// * `module` is the zero-based ordinal of the embedded core module in the
    ///   original component, before instrumentation. Only instantiated modules
    ///   have accessors. It is not a core instance index or an injected helper's
    ///   module index.
    /// * Object IDs belong to one registry shared by all instrumented modules.
    ///   Operations 3 and 6 can use any participating module's accessor.
    ///   Operation 3 returns the owning module ID and that module's original
    ///   type index. Route operations 4 and 5 to the owning module. Route global,
    ///   table, and passive-element reads to the module containing those roots.
    /// * Function payloads identify the module where instrumentation created
    ///   the function proxy and the original function index in that module,
    ///   including imports. A proxy retains this identity when passed between
    ///   modules; it does not use the receiving module's function index space.
    ///
    /// Call the generated component export `wizer-gc-read{module}` with WIT
    /// signature `(op: u32, target: u32, index: u64, part: u32) -> u64`.
    /// Unlike the core backend, forward every part, including function payload
    /// part 2, directly to this export. Component instrumentation uses proxies
    /// for object types and function identities, so the core backend's runtime
    /// type-marker checks and function-handle lookup are not needed here.
    fn gc_read(
        &mut self,
        module: u32,
        op: u32,
        target: u32,
        index: u64,
        part: u32,
    ) -> impl Future<Output = Result<u64>> + Send;

    /// Looks up the exported `instance` which has `func` as an export, calls
    /// it, and returns the `list<u8>` return type.
    fn call_func_ret_list_u8(
        &mut self,
        instance: &str,
        func: &str,
        contents: impl FnOnce(&[u8]) + Send,
    ) -> impl Future<Output = ()> + Send;

    /// Same as [`Self::call_func_ret_list_u8`], but for the `s32` WIT type.
    fn call_func_ret_s32(&mut self, instance: &str, func: &str)
    -> impl Future<Output = i32> + Send;

    /// Same as [`Self::call_func_ret_list_u8`], but for the `s64` WIT type.
    fn call_func_ret_s64(&mut self, instance: &str, func: &str)
    -> impl Future<Output = i64> + Send;

    /// Same as [`Self::call_func_ret_list_u8`], but for the `f32` WIT type.
    fn call_func_ret_f32(&mut self, instance: &str, func: &str)
    -> impl Future<Output = u32> + Send;

    /// Same as [`Self::call_func_ret_list_u8`], but for the `f64` WIT type.
    fn call_func_ret_f64(&mut self, instance: &str, func: &str)
    -> impl Future<Output = u64> + Send;
}
