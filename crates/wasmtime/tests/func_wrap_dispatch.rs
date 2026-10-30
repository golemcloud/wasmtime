use std::future::poll_fn;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::Poll;
use wasmtime::component::{Accessor, Component, Linker};
use wasmtime::{Config, Engine, Result, Store, StoreContextMut};

#[derive(Default)]
struct State {
    selected: AtomicUsize,
    borrowed_created: AtomicUsize,
    borrowed_polls: AtomicUsize,
    concurrent_created: AtomicUsize,
}

fn engine() -> Result<Engine> {
    let mut config = Config::new();
    config
        .wasm_component_model_async(true)
        .concurrency_support(true);
    Engine::new(&config)
}

fn component(
    engine: &Engine,
    parameter: &str,
    core_parameter: &str,
    async_export: bool,
) -> Result<Component> {
    let async_ = if async_export { "async " } else { "" };
    Component::new(
        engine,
        format!(
            r#"
            (component
                (import "dispatch" (func $dispatch (param "value" {parameter}) (result u32)))
                (core func $dispatch (canon lower (func $dispatch)))
                (core module $m
                    (import "" "dispatch" (func $dispatch (param {core_parameter}) (result i32)))
                    (func (export "run") (param {core_parameter}) (result i32)
                        local.get 0 call $dispatch))
                (core instance $i (instantiate $m
                    (with "" (instance (export "dispatch" (func $dispatch))))))
                (func (export "run") {async_}(param "value" {parameter}) (result u32)
                    (canon lift (core func $i "run"))))
            "#
        ),
    )
}

fn linker(engine: &Engine) -> Result<Linker<State>> {
    let mut linker = Linker::new(engine);
    linker.root().func_wrap_dispatch(
        "dispatch",
        |state: &mut State, (value,): &(u32,)| {
            state.selected.fetch_add(1, Ordering::SeqCst);
            match value {
                3 => Err(wasmtime::Error::msg("selector rejected invocation")),
                value => Ok(*value == 1),
            }
        },
        |store: StoreContextMut<'_, State>, (value,): (u32,)| {
            store.data().borrowed_created.fetch_add(1, Ordering::SeqCst);
            let mut first_poll = value != 0;
            Box::new(poll_fn(move |cx| {
                store.data().borrowed_polls.fetch_add(1, Ordering::SeqCst);
                if first_poll {
                    first_poll = false;
                    cx.waker().wake_by_ref();
                    Poll::Pending
                } else {
                    Poll::Ready(Ok((value + 20,)))
                }
            }))
        },
        |accessor: &Accessor<State>, (value,): (u32,)| {
            accessor.with(|mut store| {
                store
                    .data_mut()
                    .concurrent_created
                    .fetch_add(1, Ordering::SeqCst);
            });
            Box::pin(async move {
                tokio::task::yield_now().await;
                Ok((value + 10,))
            })
        },
    )?;
    Ok(linker)
}

#[tokio::test]
#[cfg_attr(miri, ignore)]
async fn dispatch_selects_before_creating_and_retains_borrowed_future() -> Result<()> {
    let engine = engine()?;
    let component = component(&engine, "u32", "i32", true)?;
    let linker = linker(&engine)?;
    let mut store = Store::new(&engine, State::default());
    let instance = linker.instantiate_async(&mut store, &component).await?;
    let run = instance.get_typed_func::<(u32,), (u32,)>(&mut store, "run")?;

    assert_eq!(run.call_async(&mut store, (1,)).await?, (11,));
    assert_eq!(run.call_async(&mut store, (2,)).await?, (22,));
    let error = run.call_async(&mut store, (3,)).await.unwrap_err();
    assert!(format!("{error:#}").contains("selector rejected invocation"));

    let state = store.data();
    assert_eq!(state.selected.load(Ordering::SeqCst), 3);
    assert_eq!(state.concurrent_created.load(Ordering::SeqCst), 1);
    assert_eq!(state.borrowed_created.load(Ordering::SeqCst), 1);
    assert_eq!(state.borrowed_polls.load(Ordering::SeqCst), 2);
    Ok(())
}

#[tokio::test]
#[cfg_attr(miri, ignore)]
async fn synchronous_export_falls_back_to_borrowed_handler() -> Result<()> {
    let engine = engine()?;
    let component = component(&engine, "u32", "i32", false)?;
    let linker = linker(&engine)?;
    let mut store = Store::new(&engine, State::default());
    let instance = linker.instantiate_async(&mut store, &component).await?;
    let run = instance.get_typed_func::<(u32,), (u32,)>(&mut store, "run")?;

    assert_eq!(run.call_async(&mut store, (0,)).await?, (20,));
    assert_eq!(run.call_async(&mut store, (2,)).await?, (22,));
    assert_eq!(run.call_async(&mut store, (1,)).await?, (21,));

    let state = store.data();
    assert_eq!(state.selected.load(Ordering::SeqCst), 3);
    assert_eq!(state.concurrent_created.load(Ordering::SeqCst), 0);
    assert_eq!(state.borrowed_created.load(Ordering::SeqCst), 3);
    assert_eq!(state.borrowed_polls.load(Ordering::SeqCst), 5);
    Ok(())
}

#[tokio::test]
#[cfg_attr(miri, ignore)]
async fn dispatch_preserves_import_abi_typechecking() -> Result<()> {
    let engine = engine()?;
    let linker = linker(&engine)?;
    let mismatched = component(&engine, "u64", "i64", true)?;
    let mut store = Store::new(&engine, State::default());
    let error = linker
        .instantiate_async(&mut store, &mismatched)
        .await
        .unwrap_err();
    assert!(format!("{error:#}").contains("type mismatch with parameters"));
    Ok(())
}
