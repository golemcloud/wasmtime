//! Tests for `Accessor::register_terminal_observer`: observing how (and
//! whether) the guest consumes the terminal event of a concurrent host import
//! call.

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use component_async_tests::Ctx;
use wasmtime::component::{Accessor, Component, Linker, TerminalConsumption, TerminalObserver};
use wasmtime::{Engine, Result, Store, format_err};

use crate::scenario::util::config;

/// Records what happened to a registered terminal observer: which consumption
/// (if any) it was invoked with, and whether it was dropped without being
/// invoked (i.e. suppressed).
#[derive(Default, Debug)]
struct ObserverLog {
    invocations: Vec<TerminalConsumption>,
    dropped_uninvoked: bool,
}

#[derive(Clone, Default)]
struct ObserverProbe(Arc<Mutex<ObserverLog>>);

impl ObserverProbe {
    /// Creates a `TerminalObserver` whose fate is recorded in this probe.
    fn observer(&self) -> TerminalObserver {
        struct Guard(Arc<Mutex<ObserverLog>>, bool);
        impl Drop for Guard {
            fn drop(&mut self) {
                if !self.1 {
                    self.0.lock().unwrap().dropped_uninvoked = true;
                }
            }
        }
        let mut guard = Guard(self.0.clone(), false);
        Box::new(move |consumption| {
            guard.1 = true;
            guard.0.lock().unwrap().invocations.push(consumption);
        })
    }

    fn assert_invoked_once(&self, expected: TerminalConsumption) {
        let log = self.0.lock().unwrap();
        assert_eq!(log.invocations, vec![expected], "unexpected invocations");
        assert!(!log.dropped_uninvoked);
    }

    fn assert_suppressed(&self) {
        let log = self.0.lock().unwrap();
        assert!(log.invocations.is_empty(), "observer was invoked: {log:?}");
        assert!(log.dropped_uninvoked, "observer was not dropped");
    }
}

/// Component whose `run` export async-lowers the imported `f`, expects
/// `STARTED`, waits for its completion via `waitable-set.wait` and then drops
/// the subtask.
const WAIT_COMPONENT: &str = r#"(component
    (import "f" (func $f async))

    (core module $Mem (memory (export "mem") 1))
    (core instance $mem (instantiate $Mem))

    (core module $m
        (import "" "f" (func $f (result i32)))
        (import "" "subtask.drop" (func $subtask.drop (param i32)))
        (import "" "waitable.join" (func $waitable.join (param i32 i32)))
        (import "" "waitable-set.new" (func $waitable-set.new (result i32)))
        (import "" "waitable-set.wait" (func $waitable-set.wait (param i32 i32) (result i32)))
        (import "" "waitable-set.drop" (func $waitable-set.drop (param i32)))
        (func (export "run")
            (local $s i32) (local $ws i32)

            ;; start the subtask, asserting it's `STARTED`
            call $f
            local.tee $s
            i32.const 0xf
            i32.and
            i32.const 1 ;; STARTED
            i32.ne
            if unreachable end

            ;; extract the subtask handle
            (local.set $s (i32.shr_u (local.get $s) (i32.const 4)))

            ;; wait for the subtask's completion
            (local.set $ws (call $waitable-set.new))
            (call $waitable.join (local.get $s) (local.get $ws))
            (drop (call $waitable-set.wait (local.get $ws) (i32.const 0x100)))

            ;; clean up
            (call $subtask.drop (local.get $s))
            (call $waitable-set.drop (local.get $ws))
        )
    )
    (core func $f (canon lower (func $f) async))
    (core func $subtask.drop (canon subtask.drop))
    (core func $waitable.join (canon waitable.join))
    (core func $waitable-set.new (canon waitable-set.new))
    (core func $waitable-set.wait (canon waitable-set.wait (memory $mem "mem")))
    (core func $waitable-set.drop (canon waitable-set.drop))
    (core instance $i (instantiate $m
        (with "" (instance
            (export "f" (func $f))
            (export "subtask.drop" (func $subtask.drop))
            (export "waitable.join" (func $waitable.join))
            (export "waitable-set.new" (func $waitable-set.new))
            (export "waitable-set.wait" (func $waitable-set.wait))
            (export "waitable-set.drop" (func $waitable-set.drop))
        ))
    ))

    (func (export "run") async
        (canon lift (core func $i "run")))
)"#;

/// Component whose `run` export async-lowers the imported `f`, expects
/// `STARTED`, yields a few times to let the host task complete, then cancels
/// the subtask expecting `RETURNED` (i.e. the completion was consumed by the
/// cancellation rather than delivered) and drops it.
const CANCEL_COMPLETED_COMPONENT: &str = r#"(component
    (import "f" (func $f async))

    (core module $m
        (import "" "f" (func $f (result i32)))
        (import "" "subtask.cancel" (func $subtask.cancel (param i32) (result i32)))
        (import "" "subtask.drop" (func $subtask.drop (param i32)))
        (import "" "thread.yield" (func $thread.yield (result i32)))
        (func (export "run")
            (local $s i32)

            ;; start the subtask, asserting it's `STARTED`
            call $f
            local.tee $s
            i32.const 0xf
            i32.and
            i32.const 1 ;; STARTED
            i32.ne
            if unreachable end

            ;; extract the subtask handle
            (local.set $s (i32.shr_u (local.get $s) (i32.const 4)))

            ;; let the host task run to completion; its `RETURNED` event is
            ;; queued but never delivered because the subtask is not in any
            ;; waitable set
            (drop (call $thread.yield))
            (drop (call $thread.yield))
            (drop (call $thread.yield))

            ;; cancel the subtask, asserting the host task had already
            ;; returned
            (call $subtask.cancel (local.get $s))
            i32.const 2 ;; RETURNED
            i32.ne
            if unreachable end

            (call $subtask.drop (local.get $s))
        )
    )
    (core func $f (canon lower (func $f) async))
    (core func $subtask.cancel (canon subtask.cancel))
    (core func $subtask.drop (canon subtask.drop))
    (core func $thread.yield (canon thread.yield))
    (core instance $i (instantiate $m
        (with "" (instance
            (export "f" (func $f))
            (export "subtask.cancel" (func $subtask.cancel))
            (export "subtask.drop" (func $subtask.drop))
            (export "thread.yield" (func $thread.yield))
        ))
    ))

    (func (export "run") async
        (canon lift (core func $i "run")))
)"#;

/// Component whose `run` export async-lowers the imported `f`, expects
/// `STARTED`, then immediately cancels the still-running subtask expecting
/// `RETURN_CANCELLED` and drops it.
const CANCEL_RUNNING_COMPONENT: &str = r#"(component
    (import "f" (func $f async))

    (core module $m
        (import "" "f" (func $f (result i32)))
        (import "" "subtask.cancel" (func $subtask.cancel (param i32) (result i32)))
        (import "" "subtask.drop" (func $subtask.drop (param i32)))
        (func (export "run")
            (local $s i32)

            ;; start the subtask, asserting it's `STARTED`
            call $f
            local.tee $s
            i32.const 0xf
            i32.and
            i32.const 1 ;; STARTED
            i32.ne
            if unreachable end

            ;; extract the subtask handle
            (local.set $s (i32.shr_u (local.get $s) (i32.const 4)))

            ;; cancel the still-running subtask, asserting it reports
            ;; `RETURN_CANCELLED`
            (call $subtask.cancel (local.get $s))
            i32.const 4 ;; RETURN_CANCELLED
            i32.ne
            if unreachable end

            (call $subtask.drop (local.get $s))
        )
    )
    (core func $f (canon lower (func $f) async))
    (core func $subtask.cancel (canon subtask.cancel))
    (core func $subtask.drop (canon subtask.drop))
    (core instance $i (instantiate $m
        (with "" (instance
            (export "f" (func $f))
            (export "subtask.cancel" (func $subtask.cancel))
            (export "subtask.drop" (func $subtask.drop))
        ))
    ))

    (func (export "run") async
        (canon lift (core func $i "run")))
)"#;

/// Component whose `run` export calls the imported `f` and asserts it
/// completes immediately with `RETURNED` (no subtask handle).
const IMMEDIATE_COMPONENT: &str = r#"(component
    (import "f" (func $f async))

    (core module $m
        (import "" "f" (func $f (result i32)))
        (func (export "run")
            ;; call the subtask, asserting it's `RETURNED` without a handle
            call $f
            i32.const 2 ;; RETURNED
            i32.ne
            if unreachable end
        )
    )
    (core func $f (canon lower (func $f) async))
    (core instance $i (instantiate $m
        (with "" (instance
            (export "f" (func $f))
        ))
    ))

    (func (export "run") async
        (canon lift (core func $i "run")))
)"#;

/// Component whose `run` export sync-lowers the imported (concurrent) `f`.
const SYNC_COMPONENT: &str = r#"(component
    (import "f" (func $f async))

    (core module $m
        (import "" "f" (func $f))
        (func (export "run")
            call $f
        )
    )
    (core func $f (canon lower (func $f)))
    (core instance $i (instantiate $m
        (with "" (instance
            (export "f" (func $f))
        ))
    ))

    (func (export "run") async
        (canon lift (core func $i "run")))
)"#;

/// Like `WAIT_COMPONENT`, but passes an out-of-bounds payload pointer to
/// `waitable-set.wait`, so writing the delivered event's payload into guest
/// memory traps *after* the event was popped for delivery.
const OOB_WAIT_COMPONENT: &str = r#"(component
    (import "f" (func $f async))

    (core module $Mem (memory (export "mem") 1))
    (core instance $mem (instantiate $Mem))

    (core module $m
        (import "" "f" (func $f (result i32)))
        (import "" "waitable.join" (func $waitable.join (param i32 i32)))
        (import "" "waitable-set.new" (func $waitable-set.new (result i32)))
        (import "" "waitable-set.wait" (func $waitable-set.wait (param i32 i32) (result i32)))
        (func (export "run")
            (local $s i32) (local $ws i32)

            ;; start the subtask, asserting it's `STARTED`
            call $f
            local.tee $s
            i32.const 0xf
            i32.and
            i32.const 1 ;; STARTED
            i32.ne
            if unreachable end

            ;; extract the subtask handle
            (local.set $s (i32.shr_u (local.get $s) (i32.const 4)))

            ;; wait for the subtask's completion with an out-of-bounds payload
            ;; pointer (the memory is a single 64 KiB page and the payload is
            ;; an 8-byte pair, so 0xfffc is out of bounds): the wait must trap
            ;; while writing the event payload to guest memory
            (local.set $ws (call $waitable-set.new))
            (call $waitable.join (local.get $s) (local.get $ws))
            (drop (call $waitable-set.wait (local.get $ws) (i32.const 0xfffc)))
            unreachable
        )
    )
    (core func $f (canon lower (func $f) async))
    (core func $waitable.join (canon waitable.join))
    (core func $waitable-set.new (canon waitable-set.new))
    (core func $waitable-set.wait (canon waitable-set.wait (memory $mem "mem")))
    (core instance $i (instantiate $m
        (with "" (instance
            (export "f" (func $f))
            (export "waitable.join" (func $waitable.join))
            (export "waitable-set.new" (func $waitable-set.new))
            (export "waitable-set.wait" (func $waitable-set.wait))
        ))
    ))

    (func (export "run") async
        (canon lift (core func $i "run")))
)"#;

/// Component whose callback-based async-lifted `run` export starts the
/// imported `f` as a subtask, waits for its completion via a callback code,
/// and whose callback traps while processing the delivered event.
const CALLBACK_TRAP_COMPONENT: &str = r#"(component
    (import "f" (func $f async))

    (core module $m
        (import "" "f" (func $f (result i32)))
        (import "" "waitable.join" (func $waitable.join (param i32 i32)))
        (import "" "waitable-set.new" (func $waitable-set.new (result i32)))
        (func (export "run") (result i32)
            (local $s i32) (local $ws i32)

            ;; start the subtask, asserting it's `STARTED`
            call $f
            local.tee $s
            i32.const 0xf
            i32.and
            i32.const 1 ;; STARTED
            i32.ne
            if unreachable end

            ;; extract the subtask handle and join it to a new waitable set
            (local.set $s (i32.shr_u (local.get $s) (i32.const 4)))
            (local.set $ws (call $waitable-set.new))
            (call $waitable.join (local.get $s) (local.get $ws))

            ;; wait on the set; the completion event is dispatched to `cb`
            (i32.or (i32.shl (local.get $ws) (i32.const 4)) (i32.const 2)) ;; WAIT
        )

        ;; the callback traps while processing the delivered event
        (func (export "cb") (param i32 i32 i32) (result i32)
            unreachable
        )
    )
    (core func $f (canon lower (func $f) async))
    (core func $waitable.join (canon waitable.join))
    (core func $waitable-set.new (canon waitable-set.new))
    (core instance $i (instantiate $m
        (with "" (instance
            (export "f" (func $f))
            (export "waitable.join" (func $waitable.join))
            (export "waitable-set.new" (func $waitable-set.new))
        ))
    ))

    (func (export "run") async
        (canon lift (core func $i "run") async (callback (func $i "cb"))))
)"#;

/// Callback-based async lift whose callback drops the completed host subtask before returning.
/// The terminal handoff must still be observed: notifying only after callback return loses the
/// observer because `subtask.drop` removes it with the host task.
const CALLBACK_DROP_SUBTASK_COMPONENT: &str = r#"(component
    (import "f" (func $f async))

    (core module $m
        (import "" "f" (func $f (result i32)))
        (import "" "subtask.drop" (func $subtask.drop (param i32)))
        (import "" "waitable.join" (func $waitable.join (param i32 i32)))
        (import "" "waitable-set.new" (func $waitable-set.new (result i32)))
        (func (export "run") (result i32)
            (local $s i32) (local $ws i32)

            call $f
            local.tee $s
            i32.const 0xf
            i32.and
            i32.const 1 ;; STARTED
            i32.ne
            if unreachable end

            (local.set $s (i32.shr_u (local.get $s) (i32.const 4)))
            (local.set $ws (call $waitable-set.new))
            (call $waitable.join (local.get $s) (local.get $ws))
            (i32.or (i32.shl (local.get $ws) (i32.const 4)) (i32.const 2)) ;; WAIT
        )

        ;; The second callback argument is the delivered waitable's guest handle.
        (func (export "cb") (param i32 i32 i32) (result i32)
            (call $subtask.drop (local.get 1))
            i32.const 0 ;; EXIT
        )
    )
    (core func $f (canon lower (func $f) async))
    (core func $subtask.drop (canon subtask.drop))
    (core func $waitable.join (canon waitable.join))
    (core func $waitable-set.new (canon waitable-set.new))
    (core instance $i (instantiate $m
        (with "" (instance
            (export "f" (func $f))
            (export "subtask.drop" (func $subtask.drop))
            (export "waitable.join" (func $waitable.join))
            (export "waitable-set.new" (func $waitable-set.new))
        ))
    ))

    (func (export "run") async
        (canon lift (core func $i "run") async (callback (func $i "cb"))))
)"#;

/// Component whose `run` export sync-lowers the imported (concurrent)
/// string-returning `f` with a trapping `realloc`, so lowering the host's
/// successful result into guest memory fails.
const SYNC_LOWER_TRAP_COMPONENT: &str = r#"(component
    (import "f" (func $f async (result string)))

    (core module $Mem
        (memory (export "mem") 1)
        (func (export "realloc") (param i32 i32 i32 i32) (result i32)
            unreachable))
    (core instance $mem (instantiate $Mem))

    (core module $m
        (import "" "f" (func $f (param i32)))
        (func (export "run")
            (call $f (i32.const 16))
        )
    )
    (core func $f (canon lower (func $f)
        (memory $mem "mem") (realloc (func $mem "realloc"))))
    (core instance $i (instantiate $m
        (with "" (instance
            (export "f" (func $f))
        ))
    ))

    (func (export "run") async
        (canon lift (core func $i "run")))
)"#;

async fn run<F>(component: &str, f: F) -> Result<()>
where
    F: for<'a> Fn((), &'a Accessor<Ctx>) -> Pin<Box<dyn Future<Output = Result<()>> + Send + 'a>>
        + Send
        + Sync
        + 'static,
{
    let engine = Engine::new(&config())?;
    let mut store = Store::new(&engine, Ctx::default());
    let component = Component::new(&engine, component)?;
    let mut linker = Linker::new(&engine);
    linker
        .root()
        .func_wrap_concurrent::<(), (), _>("f", move |accessor: &Accessor<Ctx>, (): ()| {
            f((), accessor)
        })?;
    let instance = linker.instantiate_async(&mut store, &component).await?;
    let func = instance.get_typed_func::<(), ()>(&mut store, "run")?;
    store
        .run_concurrent(async |store| func.call_concurrent(store, ()).await)
        .await??;
    Ok(())
}

/// A host task whose successful completion is received by the guest via
/// `waitable-set.wait` reports `Delivered`.
#[tokio::test]
async fn terminal_observer_delivered_via_wait() -> Result<()> {
    let probe = ObserverProbe::default();
    let probe2 = probe.clone();
    run(WAIT_COMPONENT, move |(), accessor| {
        let probe = probe2.clone();
        Box::pin(async move {
            accessor.register_terminal_observer(probe.observer())?;
            tokio::task::yield_now().await;
            Ok(())
        })
    })
    .await?;
    probe.assert_invoked_once(TerminalConsumption::Delivered);
    Ok(())
}

/// A host task which completes on its first poll returns its result directly
/// to the guest caller and reports `Delivered`.
#[tokio::test]
async fn terminal_observer_delivered_immediately() -> Result<()> {
    let probe = ObserverProbe::default();
    let probe2 = probe.clone();
    run(IMMEDIATE_COMPONENT, move |(), accessor| {
        let probe = probe2.clone();
        Box::pin(async move {
            accessor.register_terminal_observer(probe.observer())?;
            Ok(())
        })
    })
    .await?;
    probe.assert_invoked_once(TerminalConsumption::Delivered);
    Ok(())
}

/// A sync-lowered concurrent host import which blocks and later completes
/// hands its result back to the blocked guest caller and reports `Delivered`.
#[tokio::test]
async fn terminal_observer_delivered_sync_lower() -> Result<()> {
    let probe = ObserverProbe::default();
    let probe2 = probe.clone();
    run(SYNC_COMPONENT, move |(), accessor| {
        let probe = probe2.clone();
        Box::pin(async move {
            accessor.register_terminal_observer(probe.observer())?;
            tokio::task::yield_now().await;
            Ok(())
        })
    })
    .await?;
    probe.assert_invoked_once(TerminalConsumption::Delivered);
    Ok(())
}

/// A host task which completes successfully but whose queued `RETURNED` event
/// is consumed by `subtask.cancel` (i.e. the guest abandoned the call) reports
/// `Discarded`.
#[tokio::test]
async fn terminal_observer_discarded_on_cancel_after_completion() -> Result<()> {
    let probe = ObserverProbe::default();
    let probe2 = probe.clone();
    run(CANCEL_COMPLETED_COMPONENT, move |(), accessor| {
        let probe = probe2.clone();
        Box::pin(async move {
            accessor.register_terminal_observer(probe.observer())?;
            tokio::task::yield_now().await;
            Ok(())
        })
    })
    .await?;
    probe.assert_invoked_once(TerminalConsumption::Discarded);
    Ok(())
}

/// A host task cancelled by the guest before it completes reports `Cancelled`.
#[tokio::test]
async fn terminal_observer_cancelled_before_completion() -> Result<()> {
    let probe = ObserverProbe::default();
    let probe2 = probe.clone();
    run(CANCEL_RUNNING_COMPONENT, move |(), accessor| {
        let probe = probe2.clone();
        Box::pin(async move {
            accessor.register_terminal_observer(probe.observer())?;
            std::future::pending::<()>().await;
            unreachable!()
        })
    })
    .await?;
    probe.assert_invoked_once(TerminalConsumption::Cancelled);
    Ok(())
}

/// A host task which fails after registering an observer never produces a
/// successful terminal, so the observer is dropped without being invoked. (A
/// failure to lower the result is reported through the same error path and
/// behaves identically.)
#[tokio::test]
async fn terminal_observer_suppressed_on_host_error() -> Result<()> {
    let probe = ObserverProbe::default();
    let probe2 = probe.clone();
    let result = run(WAIT_COMPONENT, move |(), accessor| {
        let probe = probe2.clone();
        Box::pin(async move {
            accessor.register_terminal_observer(probe.observer())?;
            tokio::task::yield_now().await;
            Err(format_err!("deliberate host failure"))
        })
    })
    .await;
    assert!(result.is_err());
    probe.assert_suppressed();
    Ok(())
}

/// A host task which completes successfully but whose terminal event cannot
/// be written to guest memory (out-of-bounds `waitable-set.wait` payload
/// pointer) traps without the guest ever receiving the event: the observer is
/// dropped without being invoked, never reporting `Delivered`.
#[tokio::test]
async fn terminal_observer_suppressed_on_event_payload_trap() -> Result<()> {
    let probe = ObserverProbe::default();
    let probe2 = probe.clone();
    let result = run(OOB_WAIT_COMPONENT, move |(), accessor| {
        let probe = probe2.clone();
        Box::pin(async move {
            accessor.register_terminal_observer(probe.observer())?;
            tokio::task::yield_now().await;
            Ok(())
        })
    })
    .await;
    assert!(result.is_err());
    probe.assert_suppressed();
    Ok(())
}

/// A host task whose completion event is dispatched to a guest callback is
/// treated as delivered when the callback is entered, even if the callback
/// subsequently traps while processing it.
#[tokio::test]
async fn terminal_observer_delivered_on_callback_entry_before_trap() -> Result<()> {
    let probe = ObserverProbe::default();
    let probe2 = probe.clone();
    let result = run(CALLBACK_TRAP_COMPONENT, move |(), accessor| {
        let probe = probe2.clone();
        Box::pin(async move {
            accessor.register_terminal_observer(probe.observer())?;
            tokio::task::yield_now().await;
            Ok(())
        })
    })
    .await;
    assert!(result.is_err());
    probe.assert_invoked_once(TerminalConsumption::Delivered);
    Ok(())
}

#[tokio::test]
async fn terminal_observer_delivered_before_callback_drops_subtask() -> Result<()> {
    let probe = ObserverProbe::default();
    let probe2 = probe.clone();
    let result = run(CALLBACK_DROP_SUBTASK_COMPONENT, move |(), accessor| {
        let probe = probe2.clone();
        Box::pin(async move {
            accessor.register_terminal_observer(probe.observer())?;
            tokio::task::yield_now().await;
            Ok(())
        })
    })
    .await;
    assert!(result.is_err());
    probe.assert_invoked_once(TerminalConsumption::Delivered);
    Ok(())
}

/// A sync-lowered concurrent host import whose successful result fails to
/// lower into the guest's memory (trapping `realloc`) is not treated as
/// delivered: the observer is dropped without being invoked.
#[tokio::test]
async fn terminal_observer_suppressed_on_sync_lowering_trap() -> Result<()> {
    let probe = ObserverProbe::default();
    let probe2 = probe.clone();

    let engine = Engine::new(&config())?;
    let mut store = Store::new(&engine, Ctx::default());
    let component = Component::new(&engine, SYNC_LOWER_TRAP_COMPONENT)?;
    let mut linker = Linker::new(&engine);
    linker.root().func_wrap_concurrent::<(), (String,), _>(
        "f",
        move |accessor: &Accessor<Ctx>, (): ()| {
            let probe = probe2.clone();
            Box::pin(async move {
                accessor.register_terminal_observer(probe.observer())?;
                tokio::task::yield_now().await;
                Ok(("hello".to_string(),))
            })
        },
    )?;
    let instance = linker.instantiate_async(&mut store, &component).await?;
    let func = instance.get_typed_func::<(), ()>(&mut store, "run")?;
    let result = store
        .run_concurrent(async |store| func.call_concurrent(store, ()).await)
        .await
        .and_then(|result| result);
    assert!(result.is_err());
    drop(store);

    probe.assert_suppressed();
    Ok(())
}

/// A store torn down while a host task is still running drops the observer
/// without invoking it.
#[tokio::test]
async fn terminal_observer_suppressed_on_store_teardown() -> Result<()> {
    let probe = ObserverProbe::default();
    let probe2 = probe.clone();
    let registered = Arc::new(AtomicBool::new(false));
    let registered2 = registered.clone();

    let engine = Engine::new(&config())?;
    let mut store = Store::new(&engine, Ctx::default());
    let component = Component::new(&engine, WAIT_COMPONENT)?;
    let mut linker = Linker::new(&engine);
    linker.root().func_wrap_concurrent::<(), (), _>(
        "f",
        move |accessor: &Accessor<Ctx>, (): ()| {
            let probe = probe2.clone();
            let registered = registered2.clone();
            Box::pin(async move {
                accessor.register_terminal_observer(probe.observer())?;
                registered.store(true, Ordering::SeqCst);
                std::future::pending::<()>().await;
                unreachable!()
            })
        },
    )?;
    let instance = linker.instantiate_async(&mut store, &component).await?;
    let func = instance.get_typed_func::<(), ()>(&mut store, "run")?;

    // Drive the call just far enough for the host task to start (and register
    // its observer), then abandon it and tear down the store.
    let mut future =
        Box::pin(store.run_concurrent(async |store| func.call_concurrent(store, ()).await));
    for _ in 0..1000 {
        if registered.load(Ordering::SeqCst) {
            break;
        }
        assert!(futures::poll!(&mut future).is_pending());
        tokio::task::yield_now().await;
    }
    assert!(
        registered.load(Ordering::SeqCst),
        "observer never registered"
    );
    drop(future);
    drop(store);

    probe.assert_suppressed();
    Ok(())
}

/// Registering a second observer replaces the first: the superseded observer
/// is dropped without being invoked, and only the last registered observer
/// sees the terminal consumption.
#[tokio::test]
async fn terminal_observer_registration_replaces_previous() -> Result<()> {
    let first = ObserverProbe::default();
    let second = ObserverProbe::default();
    let (first2, second2) = (first.clone(), second.clone());
    run(IMMEDIATE_COMPONENT, move |(), accessor| {
        let (first, second) = (first2.clone(), second2.clone());
        Box::pin(async move {
            accessor.register_terminal_observer(first.observer())?;
            accessor.register_terminal_observer(second.observer())?;
            Ok(())
        })
    })
    .await?;
    first.assert_suppressed();
    second.assert_invoked_once(TerminalConsumption::Delivered);
    Ok(())
}

/// Clearing a registered observer suppresses it: it is dropped without being
/// invoked, and no observer sees the terminal consumption.
#[tokio::test]
async fn terminal_observer_cleared_before_terminal() -> Result<()> {
    let probe = ObserverProbe::default();
    let probe2 = probe.clone();
    run(WAIT_COMPONENT, move |(), accessor| {
        let probe = probe2.clone();
        Box::pin(async move {
            accessor.register_terminal_observer(probe.observer())?;
            accessor.clear_terminal_observer()?;
            tokio::task::yield_now().await;
            Ok(())
        })
    })
    .await?;
    probe.assert_suppressed();
    Ok(())
}

/// Clearing when no observer is registered is a no-op.
#[tokio::test]
async fn terminal_observer_clear_without_observer() -> Result<()> {
    run(IMMEDIATE_COMPONENT, move |(), accessor| {
        Box::pin(async move {
            accessor.clear_terminal_observer()?;
            Ok(())
        })
    })
    .await?;
    Ok(())
}

/// Clearing from an accessor which does not belong to a host import call is a
/// no-op rather than an error.
#[tokio::test]
async fn terminal_observer_clear_without_host_task() -> Result<()> {
    let engine = Engine::new(&config())?;
    let mut store = Store::new(&engine, Ctx::default());
    store
        .run_concurrent(async |accessor| {
            accessor.clear_terminal_observer()?;
            Ok::<_, wasmtime::Error>(())
        })
        .await??;
    Ok(())
}

/// After clearing an earlier observer, a replacement registered afterwards
/// receives the eventual terminal verdict; the cleared observer stays
/// suppressed.
#[tokio::test]
async fn terminal_observer_replacement_after_clear() -> Result<()> {
    let first = ObserverProbe::default();
    let second = ObserverProbe::default();
    let (first2, second2) = (first.clone(), second.clone());
    run(WAIT_COMPONENT, move |(), accessor| {
        let (first, second) = (first2.clone(), second2.clone());
        Box::pin(async move {
            accessor.register_terminal_observer(first.observer())?;
            accessor.clear_terminal_observer()?;
            accessor.register_terminal_observer(second.observer())?;
            tokio::task::yield_now().await;
            Ok(())
        })
    })
    .await?;
    first.assert_suppressed();
    second.assert_invoked_once(TerminalConsumption::Delivered);
    Ok(())
}

/// Accessors which do not belong to a host import call cannot register
/// terminal observers.
#[tokio::test]
async fn terminal_observer_requires_host_task() -> Result<()> {
    let engine = Engine::new(&config())?;
    let mut store = Store::new(&engine, Ctx::default());
    store
        .run_concurrent(async |accessor| {
            assert!(
                accessor
                    .register_terminal_observer(Box::new(|_| {}))
                    .is_err()
            );
            Ok::<_, wasmtime::Error>(())
        })
        .await??;
    Ok(())
}
