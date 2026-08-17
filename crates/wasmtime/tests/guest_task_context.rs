use std::sync::Arc;
use wasmtime::component::*;
use wasmtime::{Config, Engine, Result, Store, StoreContextMut};

#[tokio::test]
#[cfg_attr(miri, ignore)]
async fn guest_task_context_direct_and_accessor() -> Result<()> {
    let mut config = Config::new();
    config.wasm_component_model_async(true);
    let engine = Engine::new(&config)?;
    let component = Component::new(
        &engine,
        r#"
        (component
            (import "setup" (func $setup))
            (import "check" (func $check async))
            (import "check-wrap-async" (func $check-wrap-async))
            (import "check-new-async" (func $check-new-async))
            (core func $setup (canon lower (func $setup)))
            (core func $check (canon lower (func $check)))
            (core func $check-wrap-async (canon lower (func $check-wrap-async)))
            (core func $check-new-async (canon lower (func $check-new-async)))
            (core module $m
                (import "" "setup" (func $setup))
                (import "" "check" (func $check))
                (import "" "check-wrap-async" (func $check-wrap-async))
                (import "" "check-new-async" (func $check-new-async))
                (func (export "run")
                    call $setup
                    call $check
                    call $check-wrap-async
                    call $check-new-async)
            )
            (core instance $i (instantiate $m
                (with "" (instance
                    (export "setup" (func $setup))
                    (export "check" (func $check))
                    (export "check-wrap-async" (func $check-wrap-async))
                    (export "check-new-async" (func $check-new-async))
                ))))
            (func (export "run") async (canon lift (core func $i "run")))
        )
        "#,
    )?;
    let callback_component = Component::new(
        &engine,
        r#"
        (component
            (import "observe" (func $observe))
            (core func $observe (canon lower (func $observe)))
            (core module $m
                (import "" "observe" (func $observe))
                (func (export "callback") call $observe)
            )
            (core instance $i (instantiate $m
                (with "" (instance (export "observe" (func $observe))))))
            (func (export "callback") (canon lift (core func $i "callback")))
        )
        "#,
    )?;

    type State = Option<Instance>;
    let mut linker = Linker::new(&engine);
    linker
        .root()
        .func_wrap("setup", |mut store: StoreContextMut<State>, (): ()| {
            assert!(store.guest_task_context::<u32>()?.is_none());
            store.set_guest_task_context(Arc::new(1_u32))?;
            assert_eq!(*store.guest_task_context::<u32>()?.unwrap(), 1);
            assert!(store.guest_task_context::<String>().is_err());
            store.clear_guest_task_context()?;
            assert!(store.guest_task_context::<u32>()?.is_none());
            store.set_guest_task_context(Arc::new(7_u32))?;
            Ok(())
        })?;
    linker
        .root()
        .func_wrap_concurrent("check", |accessor: &Accessor<State>, (): ()| {
            Box::pin(async move {
                tokio::task::yield_now().await;
                assert_eq!(*accessor.guest_task_context::<u32>()?.unwrap(), 7);
                assert!(accessor.guest_task_context::<String>().is_err());
                let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    accessor.with(|_| panic!("test panic while accessing store"))
                }));
                assert!(panic.is_err());
                assert_eq!(*accessor.guest_task_context::<u32>()?.unwrap(), 7);
                let callback = accessor.with(|mut store| {
                    let instance = *store.data_mut().as_ref().unwrap();
                    instance.get_typed_func::<(), ()>(&mut store, "callback")
                })?;
                let call =
                    accessor.with(|mut store| callback.start_call_concurrent(&mut store, ()))?;
                callback.finish_call_concurrent(accessor, call).await?;
                Ok(())
            })
        })?;
    linker.root().func_wrap_async(
        "check-wrap-async",
        |_store: StoreContextMut<State>, (): ()| {
            assert_eq!(*current_guest_task_context::<u32>().unwrap().unwrap(), 7);
            Box::new(async move {
                tokio::task::yield_now().await;
                assert_eq!(*current_guest_task_context::<u32>()?.unwrap(), 7);
                Ok(())
            })
        },
    )?;
    linker.root().func_new_async(
        "check-new-async",
        |_store: StoreContextMut<State>, _ty, params, results| {
            assert!(params.is_empty());
            assert!(results.is_empty());
            assert_eq!(*current_guest_task_context::<u32>().unwrap().unwrap(), 7);
            Box::new(async move {
                tokio::task::yield_now().await;
                assert_eq!(*current_guest_task_context::<u32>()?.unwrap(), 7);
                Ok(())
            })
        },
    )?;
    linker
        .root()
        .func_wrap("observe", |mut store: StoreContextMut<State>, (): ()| {
            assert_eq!(*store.guest_task_context::<u32>()?.unwrap(), 7);
            assert_eq!(*current_guest_task_context::<u32>()?.unwrap(), 7);
            Ok(())
        })?;

    let mut store = Store::new(&engine, None);
    let callback_instance = linker
        .instantiate_async(&mut store, &callback_component)
        .await?;
    let instance = linker.instantiate_async(&mut store, &component).await?;
    *store.data_mut() = Some(callback_instance);
    let run = instance.get_typed_func::<(), ()>(&mut store, "run")?;
    store
        .run_concurrent(async |accessor| -> wasmtime::Result<()> {
            assert!(accessor.guest_task_context::<u32>()?.is_none());
            let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                accessor.with(|_| panic!("test panic from root accessor"))
            }));
            assert!(panic.is_err());
            assert!(accessor.guest_task_context::<u32>()?.is_none());
            let call = accessor.with(|mut store| run.start_call_concurrent(&mut store, ()))?;
            run.finish_call_concurrent(accessor, call).await
        })
        .await??;
    Ok(())
}

#[tokio::test]
#[cfg_attr(miri, ignore)]
async fn legacy_async_recursive_child_clear_uses_child_context() -> Result<()> {
    let mut config = Config::new();
    config.wasm_component_model_async(true);
    let engine = Engine::new(&config)?;
    let component = Component::new(
        &engine,
        r#"
        (component
            (import "setup" (func $setup))
            (import "recurse" (func $recurse))
            (core func $setup (canon lower (func $setup)))
            (core func $recurse (canon lower (func $recurse)))
            (core module $m
                (import "" "setup" (func $setup))
                (import "" "recurse" (func $recurse))
                (func (export "run")
                    call $setup
                    call $recurse)
            )
            (core instance $i (instantiate $m
                (with "" (instance
                    (export "setup" (func $setup))
                    (export "recurse" (func $recurse))
                ))))
            (func (export "run") async (canon lift (core func $i "run")))
        )
        "#,
    )?;
    let callback_component = Component::new(
        &engine,
        r#"
        (component
            (import "clear" (func $clear))
            (import "observe" (func $observe))
            (import "observe-new" (func $observe-new))
            (core func $clear (canon lower (func $clear)))
            (core func $observe (canon lower (func $observe)))
            (core func $observe-new (canon lower (func $observe-new)))
            (core module $m
                (import "" "clear" (func $clear))
                (import "" "observe" (func $observe))
                (import "" "observe-new" (func $observe-new))
                (func (export "callback")
                    call $clear
                    call $observe
                    call $observe-new)
            )
            (core instance $i (instantiate $m
                (with "" (instance
                    (export "clear" (func $clear))
                    (export "observe" (func $observe))
                    (export "observe-new" (func $observe-new))
                ))))
            (func (export "callback") (canon lift (core func $i "callback")))
        )
        "#,
    )?;

    type State = Option<Instance>;
    let mut linker = Linker::new(&engine);
    linker
        .root()
        .func_wrap("setup", |mut store: StoreContextMut<State>, (): ()| {
            store.set_guest_task_context(Arc::new(7_u32))
        })?;
    linker
        .root()
        .func_wrap("clear", |mut store: StoreContextMut<State>, (): ()| {
            assert_eq!(*store.guest_task_context::<u32>()?.unwrap(), 7);
            store.clear_guest_task_context()
        })?;
    linker
        .root()
        .func_wrap("observe", |mut store: StoreContextMut<State>, (): ()| {
            assert!(store.guest_task_context::<u32>()?.is_none());
            assert_eq!(current_guest_task_context::<u32>()?.as_deref(), None);
            Ok(())
        })?;
    linker.root().func_new(
        "observe-new",
        |mut store: StoreContextMut<State>, _ty, params, results| {
            assert!(params.is_empty());
            assert!(results.is_empty());
            assert!(store.guest_task_context::<u32>()?.is_none());
            assert_eq!(current_guest_task_context::<u32>()?.as_deref(), None);
            Ok(())
        },
    )?;
    linker
        .root()
        .func_wrap_async("recurse", |mut store: StoreContextMut<State>, (): ()| {
            assert_eq!(*current_guest_task_context::<u32>().unwrap().unwrap(), 7);
            Box::new(async move {
                let instance = *store.data().as_ref().unwrap();
                let callback = instance.get_typed_func::<(), ()>(&mut store, "callback")?;
                callback.call_async(&mut store, ()).await
            })
        })?;

    let mut store = Store::new(&engine, None);
    let callback_instance = linker
        .instantiate_async(&mut store, &callback_component)
        .await?;
    let instance = linker.instantiate_async(&mut store, &component).await?;
    *store.data_mut() = Some(callback_instance);
    let run = instance.get_typed_func::<(), ()>(&mut store, "run")?;
    run.call_async(&mut store, ()).await
}

#[tokio::test]
#[cfg_attr(miri, ignore)]
async fn concurrent_resource_destructor_inherits_guest_task_context() -> Result<()> {
    let mut config = Config::new();
    config.wasm_component_model_async(true);
    let engine = Engine::new(&config)?;
    let component = Component::new(
        &engine,
        r#"
        (component
            (import "resource" (type $resource (sub resource)))
            (import "setup" (func $setup))
            (core func $setup (canon lower (func $setup)))
            (core func $drop (canon resource.drop $resource))
            (core module $m
                (import "" "setup" (func $setup))
                (import "" "drop" (func $drop (param i32)))
                (func (export "run") (param i32)
                    call $setup
                    local.get 0
                    call $drop)
            )
            (core instance $i (instantiate $m
                (with "" (instance
                    (export "setup" (func $setup))
                    (export "drop" (func $drop))
                ))))
            (func (export "run") async (param "resource" (own $resource))
                (canon lift (core func $i "run")))
        )
        "#,
    )?;

    let mut linker = Linker::new(&engine);
    linker
        .root()
        .func_wrap("setup", |mut store: StoreContextMut<()>, (): ()| {
            store.set_guest_task_context(Arc::new(7_u32))
        })?;
    linker
        .root()
        .resource_concurrent("resource", ResourceType::host::<u32>(), |accessor, _| {
            Box::pin(async move {
                assert_eq!(accessor.guest_task_context::<u32>()?.as_deref(), Some(&7));
                Ok(())
            })
        })?;

    let mut store = Store::new(&engine, ());
    let instance = linker.instantiate_async(&mut store, &component).await?;
    let run = instance.get_typed_func::<(Resource<u32>,), ()>(&mut store, "run")?;
    run.call_async(&mut store, (Resource::new_own(1),)).await
}

#[tokio::test]
#[cfg_attr(miri, ignore)]
async fn legacy_async_resource_destructor_keeps_ambient_guest_task_context() -> Result<()> {
    let mut config = Config::new();
    config.wasm_component_model_async(true);
    let engine = Engine::new(&config)?;
    let component = Component::new(
        &engine,
        r#"
        (component
            (import "resource" (type $resource (sub resource)))
            (import "setup" (func $setup))
            (core func $setup (canon lower (func $setup)))
            (core func $drop (canon resource.drop $resource))
            (core module $m
                (import "" "setup" (func $setup))
                (import "" "drop" (func $drop (param i32)))
                (func (export "run") (param i32)
                    call $setup
                    local.get 0
                    call $drop)
            )
            (core instance $i (instantiate $m
                (with "" (instance
                    (export "setup" (func $setup))
                    (export "drop" (func $drop))
                ))))
            (func (export "run") async (param "resource" (own $resource))
                (canon lift (core func $i "run")))
        )
        "#,
    )?;

    let mut linker = Linker::new(&engine);
    linker
        .root()
        .func_wrap("setup", |mut store: StoreContextMut<()>, (): ()| {
            store.set_guest_task_context(Arc::new(7_u32))
        })?;
    linker
        .root()
        .resource_async("resource", ResourceType::host::<u32>(), |_store, _| {
            Box::new(async move {
                tokio::task::yield_now().await;
                assert_eq!(current_guest_task_context::<u32>()?.as_deref(), Some(&7));
                Ok(())
            })
        })?;

    let mut store = Store::new(&engine, ());
    let instance = linker.instantiate_async(&mut store, &component).await?;
    let run = instance.get_typed_func::<(Resource<u32>,), ()>(&mut store, "run")?;
    run.call_async(&mut store, (Resource::new_own(1),)).await
}
