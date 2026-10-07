#![cfg(feature = "component-model-async")]

use std::future::{Future, pending};
use std::pin::Pin;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering::SeqCst},
};
use std::task::{Context, Poll, Wake, Waker};
use wasmtime::component::{
    Accessor, AccessorTask, Component, Destination, FutureProducer, FutureReader, Linker,
    RuntimeActivityId, RuntimeActivityKind, RuntimeInvalidation, RuntimeObservation, Source,
    StreamConsumer, StreamProducer, StreamReader, StreamResult,
};
use wasmtime::{AsContextMut, Config, Engine, Store, StoreContextMut};

#[derive(Default)]
struct WakeCount(AtomicUsize);

impl Wake for WakeCount {
    fn wake(self: Arc<Self>) {
        self.0.fetch_add(1, SeqCst);
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.0.fetch_add(1, SeqCst);
    }
}

fn poll<F: Future>(future: Pin<&mut F>, wakes: &Arc<WakeCount>) -> Poll<F::Output> {
    future.poll(&mut Context::from_waker(&Waker::from(wakes.clone())))
}

struct Background(Arc<AtomicUsize>, bool);

impl AccessorTask<()> for Background {
    async fn run(self, _: &Accessor<()>) -> wasmtime::Result<()> {
        self.0.fetch_add(1, SeqCst);
        if self.1 {
            let mut first = true;
            std::future::poll_fn(|cx| {
                if std::mem::take(&mut first) {
                    cx.waker().wake_by_ref();
                    Poll::Pending
                } else {
                    Poll::Ready(())
                }
            })
            .await;
        }
        pending().await
    }
}

struct Producer(Arc<AtomicUsize>);

impl StreamProducer<()> for Producer {
    type Item = u8;
    type Buffer = Option<u8>;

    fn poll_produce<'a>(
        self: Pin<&mut Self>,
        _: &mut Context<'_>,
        _: StoreContextMut<'a, ()>,
        _: Destination<'a, u8, Option<u8>>,
        finish: bool,
    ) -> Poll<wasmtime::Result<StreamResult>> {
        self.0.fetch_add(1, SeqCst);
        if finish {
            Poll::Ready(Ok(StreamResult::Cancelled))
        } else {
            Poll::Pending
        }
    }
}

struct Consumer;

impl StreamConsumer<()> for Consumer {
    type Item = u8;

    fn poll_consume(
        self: Pin<&mut Self>,
        _: &mut Context<'_>,
        _: StoreContextMut<'_, ()>,
        _: Source<'_, u8>,
        finish: bool,
    ) -> Poll<wasmtime::Result<StreamResult>> {
        if finish {
            Poll::Ready(Ok(StreamResult::Cancelled))
        } else {
            Poll::Pending
        }
    }
}

const GUEST_FUTURE_COMPONENT: &str = r#"
(component
  (core module $libc (memory (export "memory") 1))
  (core instance $libc (instantiate $libc))
  (core module $m
    (import "" "future.read" (func $read (param i32 i32) (result i32)))
    (import "" "waitable.join" (func $join (param i32 i32)))
    (import "" "waitable-set.new" (func $new (result i32)))
    (import "" "waitable-set.wait" (func $wait (param i32 i32) (result i32)))
    (func (export "run") (param $future i32) (local $set i32)
      (call $read (local.get $future) (i32.const 0x100))
      i32.const -1 i32.ne if unreachable end
      (local.set $set (call $new))
      (call $join (local.get $future) (local.get $set))
      (drop (call $wait (local.get $set) (i32.const 0x200)))))
  (type $future (future u32))
  (core func $read (canon future.read $future async (memory $libc "memory")))
  (canon waitable.join (core func $join))
  (canon waitable-set.new (core func $new))
  (canon waitable-set.wait (memory $libc "memory") (core func $wait))
  (core instance $i (instantiate $m (with "" (instance
    (export "future.read" (func $read))
    (export "waitable.join" (func $join))
    (export "waitable-set.new" (func $new))
    (export "waitable-set.wait" (func $wait))))))
  (func (export "run") async (param "future" $future)
    (canon lift (core func $i "run") (memory $libc "memory"))))
"#;

struct YieldFuture(bool);

impl FutureProducer<()> for YieldFuture {
    type Item = u32;

    fn poll_produce(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        _: StoreContextMut<'_, ()>,
        finish: bool,
    ) -> Poll<wasmtime::Result<Option<Self::Item>>> {
        if finish {
            Poll::Ready(Ok(None))
        } else if std::mem::replace(&mut self.0, true) {
            Poll::Ready(Ok(Some(42)))
        } else {
            cx.waker().wake_by_ref();
            Poll::Pending
        }
    }
}

#[test]
fn background_activity_outlives_driver_until_store_drop() -> wasmtime::Result<()> {
    struct PendingTask(Arc<Mutex<Option<RuntimeActivityId>>>);
    impl AccessorTask<()> for PendingTask {
        async fn run(self, accessor: &Accessor<()>) -> wasmtime::Result<()> {
            *self.0.lock().unwrap() = accessor.runtime_activity();
            pending().await
        }
    }

    let mut config = Config::new();
    config.wasm_component_model_async(true);
    let engine = Engine::new(&config)?;
    let mut store = Store::new(&engine, ());
    let events = Arc::new(Mutex::new(Vec::new()));
    let observed = events.clone();
    store
        .as_context_mut()
        .set_runtime_observer(Arc::new(move |event| {
            observed.lock().unwrap().push(event);
        }));
    let seen = Arc::new(Mutex::new(None));
    store.as_context_mut().spawn(PendingTask(seen.clone()));
    let activity = match events.lock().unwrap().as_slice() {
        [
            RuntimeObservation::ActivityStarted {
                activity,
                kind: RuntimeActivityKind::Background,
            },
        ] => *activity,
        events => panic!("background task must remain live after admission: {events:?}"),
    };
    let finishes = || {
        events.lock().unwrap().iter().filter(|event| matches!(event,
        RuntimeObservation::ActivityFinished { activity: finished } if *finished == activity
    )).count()
    };
    let wakes = Arc::new(WakeCount::default());
    let mut driver = Box::pin(store.run_concurrent(async |_| pending::<()>().await));
    assert!(poll(driver.as_mut(), &wakes).is_pending());
    assert_eq!(*seen.lock().unwrap(), Some(activity));
    assert_eq!(finishes(), 0);
    drop(driver);
    assert_eq!(finishes(), 0, "driver loss does not remove the stored task");
    drop(store);
    assert_eq!(finishes(), 1);
    Ok(())
}

#[test]
fn provisional_transfer_activities_outlive_driver_until_store_drop() -> wasmtime::Result<()> {
    let mut config = Config::new();
    config.wasm_component_model_async(true);
    let engine = Engine::new(&config)?;
    let mut store = Store::new(&engine, ());
    let events = Arc::new(Mutex::new(Vec::new()));
    let observed = events.clone();
    store
        .as_context_mut()
        .set_runtime_observer(Arc::new(move |event| {
            observed.lock().unwrap().push(event);
        }));

    StreamReader::new(&mut store, Producer(Arc::new(AtomicUsize::new(0))))?
        .pipe(&mut store, Consumer)?;
    let transfers = || {
        events
            .lock()
            .unwrap()
            .iter()
            .filter_map(|event| match event {
                RuntimeObservation::ActivityStarted {
                    activity,
                    kind: RuntimeActivityKind::Transfer,
                } => Some(*activity),
                _ => None,
            })
            .collect::<Vec<_>>()
    };
    let finishes = |activities: &[RuntimeActivityId]| {
        events
            .lock()
            .unwrap()
            .iter()
            .filter(|event| match event {
                RuntimeObservation::ActivityFinished { activity } => activities.contains(activity),
                _ => false,
            })
            .count()
    };
    let transfers = transfers();
    assert!(!transfers.is_empty());
    assert_eq!(finishes(&transfers), 0);

    let wakes = Arc::new(WakeCount::default());
    let mut driver = Box::pin(store.run_concurrent(async |_| pending::<()>().await));
    assert!(poll(driver.as_mut(), &wakes).is_pending());
    assert_eq!(finishes(&transfers), 0);
    drop(driver);
    assert_eq!(finishes(&transfers), 0);
    drop(store);
    assert_eq!(finishes(&transfers), transfers.len());
    Ok(())
}

#[test]
fn observation_distinguishes_runnable_work_from_blocking() -> wasmtime::Result<()> {
    let mut config = Config::new();
    config.wasm_component_model(true);
    config.wasm_component_model_async(true);
    let engine = Engine::new(&config)?;
    let events = Arc::new(Mutex::new(Vec::new()));
    let wakes = Arc::new(WakeCount::default());

    let mut store = Store::new(&engine, ());
    let observed = events.clone();
    store
        .as_context_mut()
        .set_runtime_observer(Arc::new(move |event| observed.lock().unwrap().push(event)));

    // The root queues work during the final poll after queue inspection. The driver must take
    // another turn instead of publishing blocked and relying on a wake which does not exist.
    let started = Arc::new(AtomicUsize::new(0));
    let count = started.clone();
    let mut root_polls = 0;
    let mut driver = Box::pin(store.run_concurrent(async move |accessor| {
        std::future::poll_fn(|_| {
            root_polls += 1;
            if root_polls == 2 {
                accessor.spawn(Background(count.clone(), false));
            }
            Poll::<()>::Pending
        })
        .await
    }));
    assert!(poll(driver.as_mut(), &wakes).is_pending());
    assert_eq!(started.load(SeqCst), 1);
    drop(driver);
    drop(store);

    // A yielding batch returns Pending for fairness while admitted tasks remain runnable. No
    // blocked event may describe that poll; once all tasks park, a later poll may publish one.
    let mut store = Store::new(&engine, ());
    let observed = events.clone();
    store
        .as_context_mut()
        .set_runtime_observer(Arc::new(move |event| observed.lock().unwrap().push(event)));
    let started = Arc::new(AtomicUsize::new(0));
    for _ in 0..128 {
        store
            .as_context_mut()
            .spawn(Background(started.clone(), true));
    }
    let before = events.lock().unwrap().len();
    let mut driver = Box::pin(store.run_concurrent(async |_| pending::<()>().await));
    assert!(poll(driver.as_mut(), &wakes).is_pending());
    assert!(started.load(SeqCst) < 128);
    assert!(
        !events.lock().unwrap()[before..]
            .iter()
            .any(|event| matches!(event, RuntimeObservation::DriverBlocked { .. }))
    );
    while started.load(SeqCst) < 128 {
        assert!(poll(driver.as_mut(), &wakes).is_pending());
    }
    for _ in 0..16 {
        let before = wakes.0.load(SeqCst);
        assert!(poll(driver.as_mut(), &wakes).is_pending());
        if wakes.0.load(SeqCst) == before {
            break;
        }
    }
    let blocked = events
        .lock()
        .unwrap()
        .iter()
        .rev()
        .find_map(|event| match event {
            RuntimeObservation::DriverBlocked { run, generation } => Some((*run, *generation)),
            _ => None,
        })
        .expect("parked runtime publishes blocked");
    drop(driver);
    drop(store);
    assert!(events.lock().unwrap().iter().any(|event| matches!(event,
        RuntimeObservation::DriverInvalidated { run, generation, reason: RuntimeInvalidation::DriverDrop }
        if *run == blocked.0 && *generation > blocked.1)));

    // Transfers are runtime activity even when no host import exists.
    let mut store = Store::new(&engine, ());
    let observed = events.clone();
    store
        .as_context_mut()
        .set_runtime_observer(Arc::new(move |event| observed.lock().unwrap().push(event)));
    let produced = Arc::new(AtomicUsize::new(0));
    StreamReader::new(&mut store, Producer(produced.clone()))?.pipe(&mut store, Consumer)?;
    let mut driver = Box::pin(store.run_concurrent(async |_| pending::<()>().await));
    assert!(poll(driver.as_mut(), &wakes).is_pending());
    assert!(produced.load(SeqCst) > 0);
    drop(driver);
    drop(store);

    let events = events.lock().unwrap();
    let starts = events
        .iter()
        .filter(|event| matches!(event, RuntimeObservation::ActivityStarted { .. }))
        .count();
    let finishes = events
        .iter()
        .filter(|event| matches!(event, RuntimeObservation::ActivityFinished { .. }))
        .count();
    assert_eq!(starts, finishes);
    assert!(events.iter().any(|event| matches!(
        event,
        RuntimeObservation::ActivityStarted {
            kind: RuntimeActivityKind::Root,
            ..
        }
    )));
    assert!(events.iter().any(|event| matches!(
        event,
        RuntimeObservation::ActivityStarted {
            kind: RuntimeActivityKind::Background,
            ..
        }
    )));
    assert!(events.iter().any(|event| matches!(
        event,
        RuntimeObservation::ActivityStarted {
            kind: RuntimeActivityKind::Transfer,
            ..
        }
    )));
    Ok(())
}

#[tokio::test]
async fn guest_future_transfer_is_observed_from_admission_through_dispatch() -> wasmtime::Result<()>
{
    let mut config = Config::new();
    config.wasm_component_model_async(true);
    config.wasm_component_model_more_async_builtins(true);
    let engine = Engine::new(&config)?;
    let component = Component::new(&engine, GUEST_FUTURE_COMPONENT)?;
    let events = Arc::new(Mutex::new(Vec::new()));
    let mut store = Store::new(&engine, ());
    let observed = events.clone();
    store
        .as_context_mut()
        .set_runtime_observer(Arc::new(move |event| observed.lock().unwrap().push(event)));
    let instance = Linker::new(&engine)
        .instantiate_async(&mut store, &component)
        .await?;
    let reader = FutureReader::new(&mut store, YieldFuture(false))?;
    instance
        .get_typed_func::<(FutureReader<u32>,), ()>(&mut store, "run")?
        .call_async(&mut store, (reader,))
        .await?;
    drop(store);

    let events = events.lock().unwrap();
    let transfer = events.iter().find_map(|event| match event {
        RuntimeObservation::ActivityStarted {
            activity,
            kind: RuntimeActivityKind::Transfer,
        } => Some(*activity),
        _ => None,
    });
    let transfer = transfer.expect("guest future read registers transfer activity");
    assert!(events.iter().any(|event| matches!(
        event,
        RuntimeObservation::ActivityFinished { activity }
            if *activity == transfer
    )));
    assert!(events.iter().any(|event| matches!(
        event,
        RuntimeObservation::ActivityStarted {
            kind: RuntimeActivityKind::Unknown,
            ..
        }
    )));
    Ok(())
}
