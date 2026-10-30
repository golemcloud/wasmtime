use crate::{LookupItem, lookup_keys};
use anyhow::Result;
use wit_parser::{Function, FunctionKind, Resolve, WorldKey};

bitflags::bitflags! {
    #[derive(Default, Copy, Clone, Debug)]
    pub struct FunctionFlags: u8 {
        const ASYNC = 1 << 0;
        const TRAPPABLE = 1 << 1;
        const STORE = 1 << 2;
        const TRACING = 1 << 3;
        const VERBOSE_TRACING = 1 << 4;
        const EXACT = 1 << 5;
        /// Use a concurrent host handler even for a synchronous WIT import.
        /// Implies `ASYNC | STORE`; not supported for exports.
        const CONCURRENT = 1 << 6;
    }
}

#[derive(Default, Debug, Clone)]
pub struct FunctionConfig {
    rules: Vec<FunctionRule>,
    pub(crate) default: FunctionFlags,
}

#[derive(Debug, Clone)]
struct FunctionRule {
    filter: String,
    flags: FunctionFlags,
    used: bool,
}

#[derive(Debug, Clone)]
pub enum FunctionFilter {
    Name(String),
    Default,
}

impl FunctionConfig {
    /// Creates a blank set of configuration.
    pub fn new() -> FunctionConfig {
        FunctionConfig::default()
    }

    /// Adds a new rule to this configuration.
    ///
    /// Note that the order rules are added is significant as only the first
    /// matching rule is used for a function.
    pub fn push(&mut self, filter: FunctionFilter, mut flags: FunctionFlags) {
        if flags.contains(FunctionFlags::CONCURRENT) {
            flags |= FunctionFlags::ASYNC | FunctionFlags::STORE;
        }
        match filter {
            FunctionFilter::Name(filter) => {
                self.rules.push(FunctionRule {
                    filter,
                    flags,
                    used: false,
                });
            }
            FunctionFilter::Default => {
                self.default = flags;
            }
        }
    }

    /// Returns the set of configuration flags associated with `func`.
    ///
    /// The `name` provided should include the full name of the function
    /// including its interface. The `kind` is the classification of the
    /// function in WIT which affects the default set of flags.
    pub(crate) fn flags(
        &mut self,
        resolve: &Resolve,
        ns: Option<&WorldKey>,
        func: &Function,
    ) -> FunctionFlags {
        let mut wit_flags = FunctionFlags::empty();

        // If the kind is async, then set the async/store flags as that's a
        // concurrent function which requires access to both.
        match &func.kind {
            FunctionKind::Freestanding
            | FunctionKind::Method(_)
            | FunctionKind::Static(_)
            | FunctionKind::Constructor(_)
            | FunctionKind::Getter
            | FunctionKind::Setter
            | FunctionKind::MethodGetter(_)
            | FunctionKind::MethodSetter(_)
            | FunctionKind::StaticGetter(_)
            | FunctionKind::StaticSetter(_) => {}

            FunctionKind::AsyncFreestanding
            | FunctionKind::AsyncMethod(_)
            | FunctionKind::AsyncStatic(_) => {
                wit_flags |= FunctionFlags::ASYNC | FunctionFlags::STORE;
            }
        }

        let mut ret = FunctionFlags::empty();
        self.add_function_flags(resolve, ns, &func.name, &mut ret);
        ret |= wit_flags;
        ret
    }

    pub(crate) fn resource_drop_flags(
        &mut self,
        resolve: &Resolve,
        ns: Option<&WorldKey>,
        resource_name: &str,
    ) -> FunctionFlags {
        let mut ret = FunctionFlags::empty();
        self.add_function_flags(resolve, ns, &format!("[drop]{resource_name}"), &mut ret);
        ret
    }

    fn add_function_flags(
        &mut self,
        resolve: &Resolve,
        key: Option<&WorldKey>,
        name: &str,
        base: &mut FunctionFlags,
    ) {
        let mut apply_rules = |name: &str, is_exact: bool| {
            for rule in self.rules.iter_mut() {
                if name != rule.filter {
                    continue;
                }
                if !is_exact && rule.flags.contains(FunctionFlags::EXACT) {
                    continue;
                }
                rule.used = true;
                *base |= rule.flags;

                // only the first rule is used.
                return true;
            }

            false
        };
        match key {
            Some(key) => {
                for (lookup, projection) in lookup_keys(resolve, key, LookupItem::Name(name)) {
                    if apply_rules(&lookup, projection.is_empty()) {
                        return;
                    }
                }
            }
            None => {
                if apply_rules(name, true) {
                    return;
                }
            }
        }

        *base |= self.default;
    }

    pub(crate) fn assert_all_rules_used(&self, kind: &str) -> Result<()> {
        let mut unused = Vec::new();
        for rule in self.rules.iter().filter(|r| !r.used) {
            unused.push(format!("{:?}: {:?}", rule.filter, rule.flags));
        }

        if unused.is_empty() {
            return Ok(());
        }

        anyhow::bail!("unused `{kind}` rules found: {unused:?}");
    }

    pub(crate) fn has_concurrent(&self) -> bool {
        self.default.contains(FunctionFlags::CONCURRENT)
            || self
                .rules
                .iter()
                .any(|rule| rule.flags.contains(FunctionFlags::CONCURRENT))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Opts;

    fn world() -> (Resolve, wit_parser::WorldId) {
        let mut resolve = Resolve::default();
        let pkg = resolve
            .push_str(
                "test.wit",
                r#"
            package test:binding-options;
            world bindings {
                import api: interface {
                    resource item;
                    run: func();
                }
                export run: func();
            }
        "#,
            )
            .unwrap();
        let world = resolve.select_world(&[pkg], Some("bindings")).unwrap();
        (resolve, world)
    }

    #[test]
    fn concurrent_export_rules_are_rejected() {
        for filter in [FunctionFilter::Default, FunctionFilter::Name("run".into())] {
            let (mut resolve, world) = world();
            let mut opts = Opts::default();
            opts.exports.push(filter, FunctionFlags::CONCURRENT);
            assert!(
                opts.generate(&mut resolve, world)
                    .unwrap_err()
                    .to_string()
                    .contains("only supported for imports")
            );
        }
    }

    #[test]
    fn concurrent_programmatic_rules_imply_async_store() {
        let (mut resolve, world) = world();
        let mut opts = Opts::default();
        opts.imports
            .push(FunctionFilter::Default, FunctionFlags::CONCURRENT);
        assert!(
            opts.imports
                .default
                .contains(FunctionFlags::ASYNC | FunctionFlags::STORE)
        );
        let generated = opts.generate(&mut resolve, world);
        if cfg!(feature = "component-model-async") {
            let generated = generated.unwrap();
            assert!(generated.contains("func_wrap_concurrent"));
            assert!(generated.contains("resource_concurrent"));
            assert!(generated.contains("fn run(accessor:"));
            assert!(generated.contains("fn drop(accessor:"));
            assert!(generated.contains("Box::pin(async move"));
        } else {
            assert!(
                generated
                    .unwrap_err()
                    .to_string()
                    .contains("component-model-async")
            );
        }
    }

    #[test]
    fn async_store_function_keeps_borrowed_access() {
        let (mut resolve, world) = world();
        let mut opts = Opts::default();
        opts.imports.push(
            FunctionFilter::Name("api.run".into()),
            FunctionFlags::ASYNC | FunctionFlags::STORE,
        );
        let generated = opts.generate(&mut resolve, world).unwrap();
        assert!(generated.contains("func_wrap_async"));
        assert!(!generated.contains("func_wrap_concurrent"));
        assert!(generated.contains("fn run(host:"));
        assert!(generated.contains("component::Access<T, Self>"));
    }

    #[test]
    #[cfg(feature = "component-model-async")]
    fn concurrent_trappable_error_conversion_uses_accessor() {
        let mut resolve = Resolve::default();
        let pkg = resolve
            .push_str(
                "errors.wit",
                r#"
            package test:errors;
            interface api {
                enum failure { denied }
                run: func() -> result<u32, failure>;
                resource item;
                native: async func();
            }
            world bindings { import api; }
        "#,
            )
            .unwrap();
        let world = resolve.select_world(&[pkg], Some("bindings")).unwrap();
        let mut opts = Opts::default();
        opts.imports.push(
            FunctionFilter::Name("test:errors/api.run".into()),
            FunctionFlags::CONCURRENT | FunctionFlags::TRAPPABLE | FunctionFlags::TRACING,
        );
        opts.imports.push(
            FunctionFilter::Name("test:errors/api.[drop]item".into()),
            FunctionFlags::CONCURRENT,
        );
        opts.trappable_error_type.push(crate::TrappableError {
            wit_path: "test:errors/api.failure".into(),
            rust_type_name: "MyError".into(),
        });
        let generated = opts.generate(&mut resolve, world).unwrap();
        assert!(generated.contains("caller.with(|mut host| Host::convert_failure"));
        assert!(generated.contains("fn run(accessor:"));
        assert!(generated.contains("fn native(accessor:"));
        assert!(generated.contains("resource_concurrent"));
        assert!(generated.contains("}.instrument(span)"));
    }
}
