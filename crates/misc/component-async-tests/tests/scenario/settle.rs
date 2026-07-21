use std::future;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use component_async_tests::{Ctx, util::yield_times, yield_};
use wasmtime::component::{Accessor, AccessorTask, Linker};
use wasmtime::{AsContextMut, Engine, Result, Store, Trap};

use crate::scenario::util::{config, make_component};

mod yield_post_return {
    wasmtime::component::bindgen!({
        path: "wit",
        world: "yield-post-return-callee",
    });
}

fn make_store() -> (Engine, Store<Ctx>) {
    let engine = Engine::new(&config()).unwrap();
    let store = Store::new(&engine, Ctx::default());
    (engine, store)
}

/// A host task that stays runnable (yielding) for a few polls, then flips a
/// flag and completes.
struct YieldingTask {
    yields: usize,
    done: Arc<AtomicBool>,
}

impl AccessorTask<Ctx> for YieldingTask {
    async fn run(self, _accessor: &Accessor<Ctx>) -> Result<()> {
        yield_times(self.yields).await;
        self.done.store(true, Ordering::SeqCst);
        Ok(())
    }
}

/// A host task that flips a flag and then parks forever (never completes).
struct ParkingTask {
    parked: Arc<AtomicBool>,
}

impl AccessorTask<Ctx> for ParkingTask {
    async fn run(self, _accessor: &Accessor<Ctx>) -> Result<()> {
        self.parked.store(true, Ordering::SeqCst);
        future::pending::<()>().await;
        Ok(())
    }
}

/// The root future of `run_concurrent_and_settle` completing must not
/// short-circuit still-runnable host tasks: the event loop keeps polling them
/// and consults the settlement predicate only at idle observation points, so a
/// predicate gated on a runnable task's completion sees that completion.
///
/// (With plain `run_concurrent` the root result returns immediately and the
/// spawned task would be left behind un-run.)
#[tokio::test]
async fn settle_runs_runnable_host_tasks_before_predicate() -> Result<()> {
    let (_engine, mut store) = make_store();

    let done = Arc::new(AtomicBool::new(false));
    store.spawn(YieldingTask {
        yields: 10,
        done: done.clone(),
    });

    let done_for_predicate = done.clone();
    let mut settled =
        move |_: wasmtime::StoreContextMut<'_, Ctx>| done_for_predicate.load(Ordering::SeqCst);

    let result = store
        .as_context_mut()
        .run_concurrent_and_settle(async |_| 42u32, &mut settled)
        .await?;

    assert_eq!(result, 42);
    assert!(done.load(Ordering::SeqCst));
    store.assert_concurrent_state_empty();
    Ok(())
}

/// A host task parked forever must not block settlement: once the predicate
/// reports settled at an idle observation point, the captured result is
/// returned and the parked future is left pending in the store. A later event
/// loop scope on the same store still works.
#[tokio::test]
async fn settle_leaves_parked_host_task_behind() -> Result<()> {
    let (_engine, mut store) = make_store();

    let parked = Arc::new(AtomicBool::new(false));
    store.spawn(ParkingTask {
        parked: parked.clone(),
    });

    let parked_for_predicate = parked.clone();
    let mut settled =
        move |_: wasmtime::StoreContextMut<'_, Ctx>| parked_for_predicate.load(Ordering::SeqCst);

    let result = store
        .as_context_mut()
        .run_concurrent_and_settle(async |_| "root", &mut settled)
        .await?;

    assert_eq!(result, "root");
    assert!(parked.load(Ordering::SeqCst));

    // The parked host future stays in the store; a subsequent event-loop scope
    // on the same store must still run normally.
    let done = Arc::new(AtomicBool::new(false));
    store.spawn(YieldingTask {
        yields: 3,
        done: done.clone(),
    });
    let done_for_predicate = done.clone();
    let mut settled =
        move |_: wasmtime::StoreContextMut<'_, Ctx>| done_for_predicate.load(Ordering::SeqCst);
    let result = store
        .as_context_mut()
        .run_concurrent_and_settle(async |_| 7u32, &mut settled)
        .await?;
    assert_eq!(result, 7);
    assert!(done.load(Ordering::SeqCst));
    Ok(())
}

/// If the store goes fully idle (no host futures, no queued work, no guest
/// tasks) while the predicate still returns `false`, nothing can ever make it
/// become `true`, so the loop must trap with `Trap::AsyncDeadlock` rather than
/// hang.
#[tokio::test]
async fn settle_traps_async_deadlock_when_idle_and_unsettled() -> Result<()> {
    let (_engine, mut store) = make_store();

    let mut settled = move |_: wasmtime::StoreContextMut<'_, Ctx>| false;

    let result = store
        .as_context_mut()
        .run_concurrent_and_settle(async |_| (), &mut settled)
        .await;

    let err = result.expect_err("expected an AsyncDeadlock trap");
    assert_eq!(err.downcast_ref::<Trap>(), Some(&Trap::AsyncDeadlock));
    Ok(())
}

/// A guest task spawned by the component (post-return work still tracked as an
/// "interesting" task by the store) must be driven to completion before
/// settlement, even when the embedder predicate is already `true` at the
/// moment the root future completes.
#[tokio::test]
async fn settle_drains_interesting_guest_tasks() -> Result<()> {
    let engine = Engine::new(&config())?;

    let component = make_component(
        &engine,
        &[test_programs_artifacts::ASYNC_YIELD_POST_RETURN_CALLEE_COMPONENT],
    )
    .await?;

    let mut linker = Linker::new(&engine);
    wasmtime_wasi::p2::add_to_linker_async(&mut linker)?;
    yield_::local::local::yield_::add_to_linker::<_, Ctx>(&mut linker, |ctx| ctx)?;

    let mut store = Store::new(&engine, Ctx::default());
    let guest = yield_post_return::YieldPostReturnCallee::instantiate_async(
        &mut store, &component, &linker,
    )
    .await?;

    let mut settled = |_: wasmtime::StoreContextMut<'_, Ctx>| true;
    store
        .as_context_mut()
        .run_concurrent_and_settle(
            async |accessor| {
                guest
                    .local_local_yield_post_return()
                    .call_run(accessor, 100)
                    .await
            },
            &mut settled,
        )
        .await??;

    // The spawned guest task fully drained before settlement returned, so no
    // concurrent state may remain in the store.
    store.assert_concurrent_state_empty();
    Ok(())
}

/// `run_concurrent_and_drain` behavior is unchanged: after the root future
/// completes it drains all remaining runnable host tasks to completion before
/// returning the result.
#[tokio::test]
async fn drain_still_completes_runnable_host_tasks() -> Result<()> {
    let (_engine, mut store) = make_store();

    let done = Arc::new(AtomicBool::new(false));
    store.spawn(YieldingTask {
        yields: 10,
        done: done.clone(),
    });

    let result = store
        .as_context_mut()
        .run_concurrent_and_drain(async |_| 99u32)
        .await?;

    assert_eq!(result, 99);
    assert!(done.load(Ordering::SeqCst));
    store.assert_concurrent_state_empty();
    Ok(())
}
