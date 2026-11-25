//! Wizer's snapshot futures are `Send`, so a caller can run them on a
//! multi-threaded executor, such as with `tokio::spawn`. These checks compile
//! the futures without running them.

use std::future::Future;
use wasmtime::{Engine, Linker, Module, Store};
use wasmtime_wizer::Wizer;

fn require_send<F: Future + Send>(_: F) {}

#[test]
fn run_future_is_send() {
    let mut store = Store::new(&Engine::default(), ());
    require_send(async move {
        Wizer::new()
            .run(
                &mut store,
                &[],
                async |store: &mut Store<()>, module: &Module| {
                    Linker::new(store.engine())
                        .instantiate_async(store, module)
                        .await
                },
            )
            .await
    });
}

#[cfg(feature = "component-model")]
#[test]
fn run_component_future_is_send() {
    use wasmtime::component::{Component, Linker};

    let mut store = Store::new(&Engine::default(), ());
    require_send(async move {
        Wizer::new()
            .run_component(
                &mut store,
                &[],
                async |store: &mut Store<()>, component: &Component| {
                    Linker::new(store.engine())
                        .instantiate_async(store, component)
                        .await
                },
            )
            .await
    });
}
