#![cfg(not(miri))]

use std::sync::Arc;
use wasmtime::component::{Access, Accessor, Component, HasSelf, Linker, RuntimeObservation};
use wasmtime::{AsContextMut, Config, Engine, Result, Store};

mod resource_and_error_bindings {
    wasmtime::component::bindgen!({
        inline: "
            package test:errors;
            interface api {
                enum failure { denied }
                resource item;
                run: func() -> result<u32, failure>;
            }
            world bindings { import api; }
        ",
        imports: { default: concurrent | trappable | tracing },
        trappable_error_type: { "test:errors/api.failure" => wasmtime::Error },
    });
}

wasmtime::component::bindgen!({
    inline: "
        package test:concurrent;
        world bindings {
            import setup: func();
            import wait: func() -> u32;
            export run: async func();
        }
    ",
    imports: {
        "setup": async | store | trappable,
        "wait": concurrent | trappable,
    },
});

#[derive(Default)]
struct State {
    started: bool,
    released: bool,
    finished: bool,
}

impl BindingsImports for State {}

impl BindingsImportsWithStore<State> for HasSelf<State> {
    async fn setup(mut access: Access<'_, State, Self>) -> Result<()> {
        access
            .as_context_mut()
            .set_guest_task_context(Arc::new(42_u32))?;
        Ok(())
    }

    async fn wait(accessor: &Accessor<State, Self>) -> Result<u32> {
        let context = accessor.guest_task_context::<u32>()?.unwrap();
        assert_eq!(*context, 42);
        let activity = accessor.runtime_activity().expect("import activity");
        accessor.with(|mut access| access.get().started = true);
        let (tx, rx) = tokio::sync::oneshot::channel();
        accessor.spawn(async move |other| {
            other.with(|mut access| {
                assert!(access.get().started);
                assert!(!access.get().finished);
                access.get().released = true;
            });
            tx.send(()).unwrap();
            Ok(())
        })?;
        // Only the independently scheduled task above can release this wait.
        rx.await.unwrap();
        assert_eq!(accessor.runtime_activity(), Some(activity));
        assert!(Arc::ptr_eq(
            &context,
            &accessor.guest_task_context::<u32>()?.unwrap()
        ));
        assert_eq!(
            *wasmtime::component::current_guest_task_context::<u32>()?.unwrap(),
            42
        );
        accessor.with(|mut access| {
            assert!(access.get().released);
            access.get().finished = true;
        });
        Ok(7)
    }
}

fn component(engine: &Engine, async_typed: bool) -> Result<Component> {
    Component::new(
        engine,
        format!(
            r#"
        (component
            (import "setup" (func $setup))
            (import "wait" (func $wait (result u32)))
            (core func $setup (canon lower (func $setup)))
            (core func $wait (canon lower (func $wait)))
            (core module $m
                (import "" "setup" (func $setup))
                (import "" "wait" (func $wait (result i32)))
                (func (export "run")
                    call $setup
                    call $wait
                    i32.const 7
                    i32.ne
                    if unreachable end))
            (core instance $i (instantiate $m
                (with "" (instance
                    (export "setup" (func $setup))
                    (export "wait" (func $wait))))))
            (func (export "run") {} (canon lift (core func $i "run"))))
    "#,
            if async_typed { "async" } else { "" }
        ),
    )
}

#[tokio::test]
async fn pending_sync_lowering_releases_store_and_preserves_context() -> Result<()> {
    let mut config = Config::new();
    config
        .wasm_component_model_async(true)
        .concurrency_support(true);
    let engine = Engine::new(&config)?;
    let mut linker = Linker::new(&engine);
    Bindings::add_to_linker::<_, HasSelf<_>>(&mut linker, |state| state)?;
    let mut store = Store::new(&engine, State::default());
    store
        .as_context_mut()
        .set_runtime_observer(Arc::new(|_: RuntimeObservation| {}))?;
    let bindings =
        Bindings::instantiate_async(&mut store, &component(&engine, true)?, &linker).await?;
    tokio::time::timeout(
        std::time::Duration::from_secs(10),
        store.run_concurrent(async |accessor| bindings.call_run(accessor).await),
    )
    .await???;
    assert!(store.data().finished);
    Ok(())
}

#[tokio::test]
async fn pending_sync_typed_guest_still_traps() -> Result<()> {
    let mut config = Config::new();
    config
        .wasm_component_model_async(true)
        .concurrency_support(true);
    let engine = Engine::new(&config)?;
    let mut linker = Linker::new(&engine);
    Bindings::add_to_linker::<_, HasSelf<_>>(&mut linker, |state| state)?;
    let mut store = Store::new(&engine, State::default());
    store
        .as_context_mut()
        .set_runtime_observer(Arc::new(|_: RuntimeObservation| {}))?;
    let instance = linker
        .instantiate_async(&mut store, &component(&engine, false)?)
        .await?;
    let run = instance.get_typed_func::<(), ()>(&mut store, "run")?;
    let error = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        run.call_async(&mut store, ()),
    )
    .await?
    .unwrap_err();
    assert_eq!(
        error.downcast_ref::<wasmtime::Trap>(),
        Some(&wasmtime::Trap::CannotBlockSyncTask)
    );
    assert!(!store.data().finished);
    Ok(())
}

#[test]
fn registration_requires_concurrency_support() -> Result<()> {
    let mut config = Config::new();
    config.concurrency_support(false);
    let engine = Engine::new(&config)?;
    let mut linker = Linker::<State>::new(&engine);
    let error = Bindings::add_to_linker::<_, HasSelf<_>>(&mut linker, |state| state).unwrap_err();
    assert!(error.to_string().contains("concurrency_support"));
    Ok(())
}
