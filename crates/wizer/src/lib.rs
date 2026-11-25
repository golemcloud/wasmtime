//! Wizer: the WebAssembly pre-initializer!
//!
//! See the [`Wizer`] struct for details.

#![deny(missing_docs)]
#![cfg_attr(docsrs, feature(doc_cfg))]

mod gc;
mod info;
mod instrument;
mod parse;
mod rewrite;
mod snapshot;

#[cfg(feature = "wasmtime")]
mod wasmtime;
#[cfg(feature = "wasmtime")]
pub use wasmtime::*;
#[cfg(feature = "component-model")]
mod component;
#[cfg(feature = "component-model")]
pub use component::*;
#[cfg(not(feature = "rayon"))]
mod rayoff;

pub use crate::info::ModuleContext;
pub use crate::snapshot::SnapshotVal;
use ::wasmtime::{Result, bail, error::Context as _};
use std::collections::{HashMap, HashSet};
pub use wasmparser::ValType;

const DEFAULT_KEEP_INIT_FUNC: bool = false;

/// Wizer: the WebAssembly pre-initializer!
///
/// Don't wait for your Wasm module to initialize itself, pre-initialize it!
/// Wizer instantiates your WebAssembly module, executes its initialization
/// function, and then serializes the instance's initialized state out into a
/// new WebAssembly module. Now you can use this new, pre-initialized
/// WebAssembly module to hit the ground running, without making your users wait
/// for that first-time set up code to complete.
///
/// ## Caveats
///
/// * The initialization function may not call any imported functions. Doing so
///   will trigger a trap and `wizer` will exit.
///
/// * The Wasm module may not import globals, tables, or memories.
///
/// * GC structs, arrays, reference globals, and tables are snapshotted together
///   to preserve shared references and cycles. Non-null external, exception,
///   and continuation references, and unknown host-owned function references
///   or GC subtypes cannot be snapshotted.
#[derive(Clone, Debug)]
#[cfg_attr(feature = "clap", derive(clap::Parser))]
pub struct Wizer {
    /// The Wasm export name of the function that should be executed to
    /// initialize the Wasm module.
    ///
    /// When used with components, this can be either the item name of a
    /// function (e.g. `wasi:cli/run.run`) or a complete wave-encoded function
    /// call (e.g. `your:package/iface.func("hello world")`)
    #[cfg_attr(
        feature = "clap",
        arg(short = 'f', long, default_value = "wizer-initialize")
    )]
    init_func: String,

    /// Any function renamings to perform.
    ///
    /// A renaming specification `dst=src` renames a function export `src` to
    /// `dst`, overwriting any previous `dst` export.
    ///
    /// Multiple renamings can be specified. It is an error to specify more than
    /// one source to rename to a destination name, or to specify more than one
    /// renaming destination for one source.
    ///
    /// This option can be used, for example, to replace a `_start` entry point
    /// in an initialized module with an alternate entry point.
    ///
    /// When module linking is enabled, these renames are only applied to the
    /// outermost module.
    #[cfg_attr(
        feature = "clap",
        arg(
            short = 'r',
            long = "rename-func",
            alias = "func-rename",
            value_name = "dst=src",
            value_parser = parse_rename,
        ),
    )]
    func_renames: Vec<(String, String)>,

    /// After initialization, should the Wasm module still export the
    /// initialization function?
    ///
    /// This is `false` by default, meaning that the initialization function is
    /// no longer exported from the Wasm module.
    #[cfg_attr(
        feature = "clap",
        arg(long, require_equals = true, value_name = "true|false")
    )]
    keep_init_func: Option<Option<bool>>,
}

#[cfg(feature = "clap")]
fn parse_rename(s: &str) -> Result<(String, String)> {
    let parts: Vec<&str> = s.splitn(2, '=').collect();
    if parts.len() != 2 {
        bail!("must contain exactly one equals character ('=')");
    }
    Ok((parts[0].into(), parts[1].into()))
}

#[derive(Default)]
struct FuncRenames {
    /// For a given export name that we encounter in the original module, a map
    /// to a new name, if any, to emit in the output module.
    rename_src_to_dst: HashMap<String, String>,
    /// A set of export names that we ignore in the original module (because
    /// they are overwritten by renamings).
    rename_dsts: HashSet<String>,
}

impl FuncRenames {
    fn parse(renames: &[(String, String)]) -> Result<FuncRenames> {
        let mut ret = FuncRenames {
            rename_src_to_dst: HashMap::new(),
            rename_dsts: HashSet::new(),
        };
        if renames.is_empty() {
            return Ok(ret);
        }

        for (dst, src) in renames {
            if ret.rename_dsts.contains(dst) {
                bail!("Duplicated function rename dst {dst}");
            }
            if ret.rename_src_to_dst.contains_key(src) {
                bail!("Duplicated function rename src {src}");
            }
            ret.rename_dsts.insert(dst.clone());
            ret.rename_src_to_dst.insert(src.clone(), dst.clone());
        }

        Ok(ret)
    }
}

impl Wizer {
    /// Construct a new `Wizer` builder.
    pub fn new() -> Self {
        Wizer {
            init_func: "wizer-initialize".to_string(),
            func_renames: vec![],
            keep_init_func: None,
        }
    }

    /// The export name of the initializer function.
    ///
    /// Defaults to `"wizer-initialize"`.
    pub fn init_func(&mut self, init_func: impl Into<String>) -> &mut Self {
        self.init_func = init_func.into();
        self
    }

    /// Returns the initialization function that will be run for wizer.
    pub fn get_init_func(&self) -> &str {
        &self.init_func
    }

    /// Add a function rename to perform.
    pub fn func_rename(&mut self, new_name: &str, old_name: &str) -> &mut Self {
        self.func_renames
            .push((new_name.to_string(), old_name.to_string()));
        self
    }

    /// After initialization, should the Wasm module still export the
    /// initialization function?
    ///
    /// This is `false` by default, meaning that the initialization function is
    /// no longer exported from the Wasm module.
    pub fn keep_init_func(&mut self, keep: bool) -> &mut Self {
        self.keep_init_func = Some(Some(keep));
        self
    }

    /// First half of [`Self::run`] which instruments the provided `wasm` and
    /// produces a new wasm module which should be run by a runtime.
    ///
    /// After the returned wasm is executed the context returned here and the
    /// state of the instance should be passed to [`Self::snapshot`].
    pub fn instrument<'a>(&self, wasm: &'a [u8]) -> Result<(ModuleContext<'a>, Vec<u8>)> {
        // Make sure we're given valid Wasm from the get go.
        self.wasm_validate(&wasm)?;

        let mut cx = parse::parse(wasm)?;

        // When wizening core modules directly some imports aren't supported,
        // so check for those here.
        for import in cx.imports() {
            match import.ty {
                wasmparser::TypeRef::Global(_) => {
                    bail!("imported globals are not supported")
                }
                wasmparser::TypeRef::Table(_) => {
                    bail!("imported tables are not supported")
                }
                wasmparser::TypeRef::Memory(_) => {
                    bail!("imported memories are not supported")
                }
                wasmparser::TypeRef::Func(_) => {}
                wasmparser::TypeRef::FuncExact(_) => {}
                wasmparser::TypeRef::Tag(_) => {}
            }
        }

        let instrumented_wasm = instrument::instrument(&mut cx);
        let instrumented_wasm = if gc::needs_snapshot(&cx)? {
            gc::lower::preserving_types(&instrumented_wasm, 0)?
        } else {
            instrumented_wasm
        };
        self.debug_assert_valid_wasm(&instrumented_wasm, "instrumented module");

        Ok((cx, instrumented_wasm))
    }

    /// Second half of [`Self::run`] which takes the [`ModuleContext`] returned
    /// by [`Self::instrument`] and the state of the `instance` after it has
    /// possibly executed its initialization function.
    ///
    /// This returns a new WebAssembly binary which has all state
    /// pre-initialized.
    pub async fn snapshot(
        &self,
        cx: &ModuleContext<'_>,
        instance: &mut impl InstanceState,
    ) -> Result<Vec<u8>> {
        // Parse rename spec.
        let renames = FuncRenames::parse(&self.func_renames)?;

        let snapshot = snapshot::snapshot(cx, instance).await;
        let rewritten_wasm = self.rewrite(cx, &snapshot, &renames, true);
        let rewritten_wasm = if gc::needs_snapshot(cx)? {
            let original = gc::original(cx);
            let gc_snapshot = gc::capture::capture(&original, 0, &mut *instance).await?;
            gc::restore::restore(&rewritten_wasm, 0, &gc_snapshot)?
        } else {
            rewritten_wasm
        };

        self.debug_assert_valid_wasm(&rewritten_wasm, "rewritten module");

        Ok(rewritten_wasm)
    }

    fn debug_assert_valid_wasm(&self, wasm: &[u8], context: &str) {
        if !cfg!(debug_assertions) {
            return;
        }
        if let Err(error) = self.wasm_validate(&wasm) {
            #[cfg(feature = "wasmprinter")]
            let wat = wasmprinter::print_bytes(&wasm)
                .unwrap_or_else(|e| format!("Disassembling to WAT failed: {e}"));
            #[cfg(not(feature = "wasmprinter"))]
            let wat = "`wasmprinter` cargo feature is not enabled".to_string();

            let wat = if wat.len() > 16 * 1024 {
                std::fs::write("invalid.wat", wat).expect("writing to invalid.wat");
                "written to invalid.wat"
            } else {
                &wat
            };
            panic!("{context} is not valid wasm: {error:?}\n\nWAT:\n{wat}");
        }
    }

    fn wasm_validate(&self, wasm: &[u8]) -> Result<()> {
        log::debug!("Validating input Wasm");

        wasmparser::Validator::new_with_features(wasmparser::WasmFeatures::all())
            .validate_all(wasm)
            .context("wasm validation failed")?;

        for payload in wasmparser::Parser::new(0).parse_all(wasm) {
            match payload? {
                wasmparser::Payload::CodeSectionEntry(code) => {
                    let mut ops = code.get_operators_reader()?;
                    while !ops.eof() {
                        match ops.read()? {
                            // Wizer has no way of dynamically determining which
                            // element or data segments were dropped during
                            // execution so instead disallow these instructions
                            // entirely. Like above it'd be nice to allow them
                            // but just forbid their execution during the
                            // initialization function, but that can't be done
                            // easily at this time.
                            wasmparser::Operator::ElemDrop { .. } => {
                                bail!("unsupported `elem.drop` instruction")
                            }
                            wasmparser::Operator::DataDrop { .. } => {
                                bail!("unsupported `data.drop` instruction")
                            }

                            _ => continue,
                        }
                    }
                }
                _ => {}
            }
        }

        Ok(())
    }

    fn get_keep_init_func(&self) -> bool {
        match self.keep_init_func {
            Some(keep) => keep.unwrap_or(true),
            None => DEFAULT_KEEP_INIT_FUNC,
        }
    }
}

/// Abstract ability to load state from a WebAssembly instance after it's been
/// instantiated and some exports have run.
pub trait InstanceState {
    /// Calls the GC state accessor added by [`Wizer::instrument`].
    ///
    /// References stay in the initialized instance; only scalar values and
    /// instance-local identities cross this boundary. Wizer calls this after
    /// initialization, only when reference state needs snapshotting. The guest
    /// must not mutate that state during capture, including between reads of
    /// different parts of one value.
    ///
    /// # Operations and indices
    ///
    /// All indices are zero-based. Global, table, function, element-segment,
    /// and type indices refer to the original module, before instrumentation.
    /// Imported entries count in their respective index spaces. A type index
    /// counts individual types, not recursive groups. Object IDs instead index
    /// the capture registry described below.
    ///
    /// | `op` | Operation | `target` | `index` | Result |
    /// | --- | --- | --- | --- | --- |
    /// | 0 | Read global | Global index | 0 | Encoded value |
    /// | 1 | Read table length | Table index | 0 | Current element count |
    /// | 2 | Read table entry | Table index | Element index | Encoded reference |
    /// | 3 | Read object owner/type | Object ID | 0 | Module ID in high 32 bits, type index in low 32 bits |
    /// | 4 | Read object length | Object ID | 0 | Struct field count or array element count |
    /// | 5 | Read object field | Object ID | Field or array element index | Encoded value |
    /// | 6 | Read registry length | 0 | 0 | Current object count |
    /// | 7 | Read passive element | Original element-segment index | Element index | Encoded reference |
    ///
    /// Length and owner/type queries use `part = 0`. Value queries select the
    /// encoding below using the original global, field, or element type.
    /// Unsupported operations and out-of-range operands have no defined result;
    /// the generated accessor can trap. Propagate accessor and runtime errors.
    ///
    /// # Value encoding
    ///
    /// For `i32` and `f32`, `part = 0` returns the raw 32 bits, zero-extended
    /// to `u64`. For `i64` and `f64`, it returns the raw 64 bits. Floating-point
    /// values are not numerically converted. Packed `i8` and `i16` fields are
    /// read unsigned and zero-extended. For `v128`, `part = 0` returns bits
    /// 0..64 and `part = 1` returns bits 64..128.
    ///
    /// For references, first read the tag with `part = 0`, then read the same
    /// location with the payload part specified here:
    ///
    /// | Tag | Meaning | Payload part | Payload |
    /// | --- | --- | --- | --- |
    /// | 0 | Null | 1 | 0 |
    /// | 1 | `i31ref` | 1 | Signed 31-bit value sign-extended to 64 bits |
    /// | 2 | Struct or array | 1 | Object ID, zero-extended to 64 bits |
    /// | 3 | Function | 2 | Module ID in high 32 bits, original function index in low 32 bits |
    /// | 4 | Unsupported reference | 1 | Not used; capture fails |
    ///
    /// Other tags are invalid and cause capture to fail. Non-null exception
    /// and continuation references produce the unsupported tag; their nulls
    /// use tag 0. Payloads for unsupported tags have no meaning. Parts other
    /// than those specified above are not part of the protocol.
    ///
    /// # Registry and ownership
    ///
    /// Reading an object payload registers that object if necessary. Repeated
    /// reads of aliases must return the same ID; distinct objects must have
    /// distinct IDs. IDs are consecutive from zero, stable for the capture,
    /// and keep their objects rooted in the instance. Reading fields can
    /// register more objects, so capture rechecks operation 6 while traversing
    /// the graph. IDs are not addresses and cannot be reused across instances
    /// or captures. No guest reference or runtime pointer is serialized.
    ///
    /// Operation 3 must identify the object's exact original heap type, not
    /// merely a known supertype. Return an error for an unknown host-created
    /// subtype rather than truncating its fields. Function identities must
    /// resolve to an original module function index, including imports, without
    /// invoking the function; return an error for an unknown host-owned
    /// function. Core-module capture uses module ID 0 for objects and functions.
    ///
    /// # Core backend implementation
    ///
    /// The generated `__wizer_gc_read` export has core signature
    /// `(i32, i32, i64, i32) -> i64`, with the same argument order and raw bits
    /// as this method. It handles queries other than function payloads. For
    /// `part = 2`, use `__wizer_gc_function`, whose signature is
    /// `(i32, i32, i64) -> funcref`, and match the returned handle against
    /// `__wizer_function_{index}` exports to produce the original function index.
    /// Do not forward `part = 2` to the scalar export.
    ///
    /// After operation 3, validate the registry entry in the exported
    /// `__wizer_gc_objects` table against the parameter heap type of
    /// `__wizer_gc_type_{type_index}` using exact runtime type equality. The
    /// generated type query alone can match a supertype of a host-created
    /// object. The marker function is for type inspection, not invocation.
    fn gc_read(
        &mut self,
        op: u32,
        target: u32,
        index: u64,
        part: u32,
    ) -> impl Future<Output = Result<u64>> + Send;

    /// Loads the global specified by `name`, returning a `SnapshotVal`.
    ///
    /// # Panics
    ///
    /// This function panics if `name` isn't an exported global or if the type
    /// of the global doesn't fit in `SnapshotVal`.
    fn global_get(
        &mut self,
        name: &str,
        type_hint: ValType,
    ) -> impl Future<Output = SnapshotVal> + Send;

    /// Loads the contents of the memory specified by `name`, returning the
    /// entier contents as a `Vec<u8>`.
    ///
    /// # Panics
    ///
    /// This function panics if `name` isn't an exported memory.
    fn memory_contents(
        &mut self,
        name: &str,
        contents: impl FnOnce(&[u8]) + Send,
    ) -> impl Future<Output = ()> + Send;
}
