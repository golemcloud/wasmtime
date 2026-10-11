#![cfg(all(
    feature = "component-model-async",
    feature = "cranelift",
    feature = "wat"
))]

use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use wasmtime::component::{
    Accessor, AccessorTask, Component, Destination, Linker, Lower, StreamProducer,
    StreamReadBoundary, StreamReadProgress, StreamReader, StreamResult, TerminalConsumption,
};
use wasmtime::{Config, Engine, Result, Store, StoreContextMut};

const COMPONENT: &str = r#"
(component
  (import "check" (func $check (param "consumed" bool)))
  (core module $libc (memory (export "memory") 1))
  (core instance $libc (instantiate $libc))
  (core module $m
    (import "" "read" (func $read (param i32 i32 i32) (result i32)))
    (import "" "cancel" (func $cancel (param i32) (result i32)))
    (import "" "drop" (func $drop (param i32)))
    (import "" "join" (func $join (param i32 i32)))
    (import "" "new-set" (func $new-set (result i32)))
    (import "" "wait" (func $wait (param i32 i32) (result i32)))
    (import "" "drop-set" (func $drop-set (param i32)))
    (import "" "check" (func $check (param i32)))
    (import "" "yield" (func $yield (result i32)))
    (func (export "run") (param $reader i32)
      (local $set i32)
      $BODY
    )
  )
  (type $s (stream u8))
  (core func $read (canon stream.read $s async (memory $libc "memory")))
  (core func $cancel (canon stream.cancel-read $s))
  (core func $drop (canon stream.drop-readable $s))
  (canon waitable.join (core func $join))
  (canon waitable-set.new (core func $new-set))
  (canon waitable-set.wait (memory $libc "memory") (core func $wait))
  (canon waitable-set.drop (core func $drop-set))
  (core func $check (canon lower (func $check)))
  (core func $yield (canon thread.yield))
  (core instance $i (instantiate $m
    (with "" (instance
      (export "read" (func $read)) (export "cancel" (func $cancel))
      (export "drop" (func $drop)) (export "join" (func $join))
      (export "new-set" (func $new-set)) (export "wait" (func $wait))
      (export "drop-set" (func $drop-set)) (export "check" (func $check))
      (export "yield" (func $yield))
    ))
  ))
  (func (export "run") async (param "reader" $s)
    (canon lift (core func $i "run") (memory $libc "memory")))
)
"#;

struct Producer {
    pending: bool,
    terminal: bool,
    retain_on_cancel: bool,
    registered: bool,
    observed: Arc<Mutex<Vec<TerminalConsumption>>>,
}

impl StreamProducer<()> for Producer {
    type Item = u8;
    type Buffer = Option<u8>;

    fn poll_produce<'a>(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        mut store: StoreContextMut<'a, ()>,
        mut destination: Destination<'a, u8, Option<u8>>,
        finish: bool,
    ) -> Poll<Result<StreamResult>> {
        if !self.registered {
            self.registered = true;
            let observed = self.observed.clone();
            destination.register_read_observer(&mut store, move |event| {
                observed.lock().unwrap().push(event);
            })?;
        }
        if finish && !self.retain_on_cancel {
            return Poll::Ready(Ok(StreamResult::Cancelled));
        }
        if self.pending && !finish {
            if !self.retain_on_cancel {
                self.pending = false;
                cx.waker().wake_by_ref();
            }
            return Poll::Pending;
        }
        if self.terminal {
            return Poll::Ready(Ok(StreamResult::Dropped));
        }
        if destination.remaining(&mut store) != Some(0) {
            destination.set_buffer(Some(42));
        }
        Poll::Ready(Ok(StreamResult::Completed))
    }
}

#[tokio::test]
async fn observes_each_guest_read_at_consumption_not_production() -> Result<()> {
    let mut config = Config::new();
    config.wasm_component_model_async(true);
    config.wasm_component_model_more_async_builtins(true);
    let engine = Engine::new(&config)?;
    for (body, pending, terminal, expected) in [
        (
            r#"
          (drop (call $read (local.get $reader) (i32.const 0x100) (i32.const 1)))
          (call $check (i32.const 1))
          (call $drop (local.get $reader))
        "#,
            false,
            false,
            TerminalConsumption::Delivered,
        ),
        (
            r#"
          (call $read (local.get $reader) (i32.const 0x100) (i32.const 1))
          i32.const -1 i32.ne if unreachable end
          (call $check (i32.const 0))
          (local.set $set (call $new-set))
          (call $join (local.get $reader) (local.get $set))
          (drop (call $wait (local.get $set) (i32.const 0x200)))
          (call $check (i32.const 1))
          (call $join (local.get $reader) (i32.const 0))
          (call $drop-set (local.get $set))
          (call $drop (local.get $reader))
        "#,
            true,
            false,
            TerminalConsumption::Delivered,
        ),
        (
            r#"
          (drop (call $read (local.get $reader) (i32.const 0x100) (i32.const 1)))
          (call $check (i32.const 0))
          (drop (call $cancel (local.get $reader)))
          (call $check (i32.const 1))
          (call $drop (local.get $reader))
        "#,
            true,
            false,
            TerminalConsumption::NotDelivered,
        ),
        (
            r#"
          (drop (call $read (local.get $reader) (i32.const 0x100) (i32.const 1)))
          (call $check (i32.const 1))
          (call $drop (local.get $reader))
        "#,
            false,
            true,
            TerminalConsumption::Delivered,
        ),
        (
            r#"
          (drop (call $read (local.get $reader) (i32.const 0x100) (i32.const 0)))
          (call $check (i32.const 1))
          (call $drop (local.get $reader))
        "#,
            false,
            false,
            TerminalConsumption::Delivered,
        ),
        (
            r#"
          (drop (call $read (local.get $reader) (i32.const 0x100) (i32.const 1)))
          (drop (call $yield)) (drop (call $yield)) (drop (call $yield))
          (call $check (i32.const 0))
          (call $cancel (local.get $reader))
          i32.const 18 i32.ne if unreachable end
          (call $check (i32.const 1))
          (call $drop (local.get $reader))
        "#,
            true,
            false,
            TerminalConsumption::Delivered,
        ),
        (
            r#"
          (drop (call $read (local.get $reader) (i32.const 0x100) (i32.const 0)))
          (drop (call $yield)) (drop (call $yield)) (drop (call $yield))
          (call $check (i32.const 0))
          (call $cancel (local.get $reader))
          i32.const 2 i32.ne if unreachable end
          (call $check (i32.const 1))
          (call $drop (local.get $reader))
        "#,
            true,
            false,
            TerminalConsumption::NotDelivered,
        ),
    ] {
        let observed = Arc::new(Mutex::new(Vec::new()));
        let checked = observed.clone();
        let mut linker = Linker::<()>::new(&engine);
        linker
            .root()
            .func_wrap("check", move |_, (consumed,): (bool,)| {
                assert_eq!(
                    *checked.lock().unwrap(),
                    if consumed { vec![expected] } else { vec![] }
                );
                Ok(())
            })?;
        let component = Component::new(&engine, COMPONENT.replace("$BODY", body))?;
        let mut store = Store::new(&engine, ());
        let instance = linker.instantiate_async(&mut store, &component).await?;
        let reader = StreamReader::new(
            &mut store,
            Producer {
                pending,
                terminal,
                retain_on_cancel: false,
                registered: false,
                observed: observed.clone(),
            },
        )?;
        instance
            .get_typed_func::<(StreamReader<u8>,), ()>(&mut store, "run")?
            .call_async(&mut store, (reader,))
            .await?;
        assert_eq!(*observed.lock().unwrap(), vec![expected]);
    }
    Ok(())
}

#[tokio::test]
async fn pending_cancellation_preserves_partial_count_and_cancelled_tag() -> Result<()> {
    let mut config = Config::new();
    config.wasm_component_model_async(true);
    config.wasm_component_model_more_async_builtins(true);
    let engine = Engine::new(&config)?;
    for async_cancel in [false, true] {
        for count in [0, 1] {
            let observed = Arc::new(Mutex::new(Vec::new()));
            let mut linker = Linker::<()>::new(&engine);
            linker.root().func_wrap("check", |_, _: (bool,)| Ok(()))?;
            let body = if async_cancel {
                format!(
                    r#"
                    (call $read (local.get $reader) (i32.const 0x100) (i32.const {count}))
                    i32.const -1 i32.ne if unreachable end
                    (call $cancel (local.get $reader))
                    i32.const -1 i32.ne if unreachable end
                    (local.set $set (call $new-set))
                    (call $join (local.get $reader) (local.get $set))
                    (drop (call $wait (local.get $set) (i32.const 0x200)))
                    (call $join (local.get $reader) (i32.const 0))
                    (call $drop-set (local.get $set))
                    (call $drop (local.get $reader))
                    "#
                )
            } else {
                format!(
                    r#"
                    (call $read (local.get $reader) (i32.const 0x100) (i32.const {count}))
                    i32.const -1 i32.ne if unreachable end
                    (call $cancel (local.get $reader))
                    i32.const {} i32.ne if unreachable end
                    (call $drop (local.get $reader))
                    "#,
                    count * 16 + 2,
                )
            };
            let source = COMPONENT
                .replace("$BODY", &body)
                .replace(
                    "(core module $m",
                    "(core module $m (import \"\" \"memory\" (memory 1))",
                )
                .replace(
                    "(export \"read\" (func $read))",
                    "(export \"memory\" (memory $libc \"memory\")) (export \"read\" (func $read))",
                );
            let source = if async_cancel {
                source
                    .replace("(canon stream.cancel-read $s)", "(canon stream.cancel-read $s async)")
                    .replace(
                        "(call $join (local.get $reader) (i32.const 0))",
                        &format!(
                            "(i32.load (i32.const 0x204)) i32.const {} i32.ne if unreachable end (call $join (local.get $reader) (i32.const 0))",
                            count * 16 + 2,
                        ),
                    )
            } else {
                source
            };
            let component = Component::new(&engine, source)?;
            let mut store = Store::new(&engine, ());
            let instance = linker.instantiate_async(&mut store, &component).await?;
            let reader = StreamReader::new(
                &mut store,
                Producer {
                    pending: true,
                    terminal: false,
                    retain_on_cancel: true,
                    registered: false,
                    observed: observed.clone(),
                },
            )?;
            instance
                .get_typed_func::<(StreamReader<u8>,), ()>(&mut store, "run")?
                .call_async(&mut store, (reader,))
                .await
                .map_err(|error| {
                    wasmtime::Error::msg(format!(
                        "async_cancel={async_cancel}, count={count}: {error:?}"
                    ))
                })?;
            assert_eq!(
                *observed.lock().unwrap(),
                vec![if count == 0 {
                    TerminalConsumption::NotDelivered
                } else {
                    TerminalConsumption::Delivered
                }],
                "async_cancel={async_cancel}, count={count}",
            );
        }
    }
    Ok(())
}

#[tokio::test]
async fn store_teardown_does_not_report_guest_cancellation() -> Result<()> {
    let mut config = Config::new();
    config.wasm_component_model_async(true);
    config.wasm_component_model_more_async_builtins(true);
    let engine = Engine::new(&config)?;
    let observed = Arc::new(Mutex::new(Vec::new()));
    let checked = observed.clone();
    let mut linker = Linker::<()>::new(&engine);
    linker.root().func_wrap("check", move |_, _: (bool,)| {
        assert!(checked.lock().unwrap().is_empty());
        Ok(())
    })?;
    let component = Component::new(
        &engine,
        COMPONENT.replace(
            "$BODY",
            r#"
          (drop (call $read (local.get $reader) (i32.const 0x100) (i32.const 1)))
          (call $check (i32.const 0))
          unreachable
        "#,
        ),
    )?;
    let mut store = Store::new(&engine, ());
    let instance = linker.instantiate_async(&mut store, &component).await?;
    let reader = StreamReader::new(
        &mut store,
        Producer {
            pending: true,
            terminal: false,
            retain_on_cancel: false,
            registered: false,
            observed: observed.clone(),
        },
    )?;
    assert!(
        instance
            .get_typed_func::<(StreamReader<u8>,), ()>(&mut store, "run")?
            .call_async(&mut store, (reader,))
            .await
            .is_err()
    );
    drop(store);
    assert!(observed.lock().unwrap().is_empty());
    Ok(())
}

#[derive(Default)]
struct ControlState {
    ready: bool,
    boundaries: Vec<StreamReadBoundary>,
    controls: usize,
    guest_steps: usize,
    reallocs: usize,
    cancel_count: usize,
    control_drops: Arc<AtomicUsize>,
    side_ready: Vec<Option<tokio::sync::oneshot::Sender<()>>>,
    // A deterministic externally driven wait, used to cancel the driver.
    park_at: Option<StreamReadBoundary>,
    parked: Option<tokio::sync::oneshot::Receiver<()>>,
    entered: Option<tokio::sync::oneshot::Sender<()>>,
}

struct ControlTask(StreamReadBoundary);

impl AccessorTask<ControlState> for ControlTask {
    async fn run(self, accessor: &Accessor<ControlState>) -> Result<()> {
        struct OnDrop(Arc<AtomicUsize>);
        impl Drop for OnDrop {
            fn drop(&mut self) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }
        let _drop = OnDrop(accessor.with(|mut store| store.data_mut().control_drops.clone()));
        let (guest_steps, reallocs, parked) = accessor.with(|mut store| {
            let state = store.data_mut();
            state.controls += 1;
            let parked = if state.park_at == Some(self.0) {
                if let Some(entered) = state.entered.take() {
                    entered.send(()).unwrap();
                }
                state.parked.take()
            } else {
                None
            };
            (state.guest_steps, state.reallocs, parked)
        });
        if let Some(parked) = parked {
            parked.await?;
        } else {
            tokio::task::yield_now().await;
        }
        accessor.with(|mut store| {
            let state = store.data_mut();
            assert_eq!(state.guest_steps, guest_steps, "guest overtook control");
            assert_eq!(state.reallocs, reallocs, "realloc overtook control");
            state.boundaries.push(self.0);
        });
        Ok(())
    }
}

struct ControlledProducer<T> {
    item: T,
    registered: bool,
    eof: bool,
    zero: bool,
    progress: StreamReadProgress,
    blocked_cancels: usize,
    observed: Arc<Mutex<Vec<TerminalConsumption>>>,
}

impl<T: Lower + Clone + Unpin + Send + 'static> StreamProducer<ControlState>
    for ControlledProducer<T>
{
    type Item = T;
    type Buffer = Option<T>;

    fn poll_produce<'a>(
        mut self: Pin<&mut Self>,
        _: &mut Context<'_>,
        mut store: StoreContextMut<'a, ControlState>,
        mut destination: Destination<'a, T, Option<T>>,
        finish: bool,
    ) -> Poll<Result<StreamResult>> {
        if !self.registered {
            self.registered = true;
            let progress = self.progress;
            let blocked_cancels = self.blocked_cancels;
            let mut cancels = 0;
            destination.register_read_control(&mut store, move |boundary| {
                let progress = if boundary == StreamReadBoundary::BeforeCancel {
                    cancels += 1;
                    if cancels <= blocked_cancels {
                        StreamReadProgress::Continue
                    } else {
                        progress
                    }
                } else {
                    StreamReadProgress::Continue
                };
                (progress, Some(ControlTask(boundary)))
            })?;
            let observed = self.observed.clone();
            destination.register_read_observer(&mut store, move |event| {
                observed.lock().unwrap().push(event);
            })?;
            // Even an already-prepared value must reproduce original Blocked.
            return Poll::Pending;
        }
        if finish && self.zero {
            store.data_mut().cancel_count += 1;
            self.registered = false;
            return Poll::Ready(Ok(StreamResult::Cancelled));
        }
        if !store.data().ready {
            return Poll::Pending;
        }
        self.registered = false;
        if self.eof {
            return Poll::Ready(Ok(StreamResult::Dropped));
        }
        if destination.remaining(&mut store) != Some(0) {
            destination.set_buffer(Some(self.item.clone()));
            self.eof = true;
        }
        Poll::Ready(Ok(StreamResult::Completed))
    }
}

fn controlled_source(body: &str, strings: bool, async_cancel: bool) -> String {
    let source = COMPONENT
        .replace("$BODY", body)
        .replace(
            "(core module $m",
            "(core module $m (import \"\" \"memory\" (memory 1))",
        )
        .replace(
            "(export \"read\" (func $read))",
            "(export \"memory\" (memory $libc \"memory\")) (export \"read\" (func $read))",
        );
    let source = if async_cancel {
        source.replace(
            "(canon stream.cancel-read $s)",
            "(canon stream.cancel-read $s async)",
        )
    } else {
        source
    };
    if strings {
        source.replace("(stream u8)", "(stream string)")
            .replace("(core module $libc (memory (export \"memory\") 1))", r#"
                (import "realloc-check" (func $realloc-check))
                (core func $realloc-check (canon lower (func $realloc-check)))
                (core module $libc
                    (import "" "check" (func $check))
                    (memory (export "memory") 1)
                    (func (export "realloc") (param i32 i32 i32 i32) (result i32)
                        call $check i32.const 0x400))
            "#)
            .replace("(core instance $libc (instantiate $libc))", "(core instance $libc (instantiate $libc (with \"\" (instance (export \"check\" (func $realloc-check))))))")
            .replace("(canon stream.read $s async (memory $libc \"memory\"))", "(canon stream.read $s async (memory $libc \"memory\") (realloc (func $libc \"realloc\")))")
    } else {
        source
    }
}

fn control_engine() -> Result<Engine> {
    let mut config = Config::new();
    config.wasm_component_model_async(true);
    config.wasm_component_model_more_async_builtins(true);
    Engine::new(&config)
}

#[tokio::test]
async fn controlled_cancel_preserves_real_results_and_observer_order() -> Result<()> {
    let engine = control_engine()?;
    // Positive, EOF, zero cancellation, and zero-length readiness are distinct.
    for (eof, zero, count, result, consumption) in [
        (false, false, 1, 18, TerminalConsumption::Delivered),
        (true, false, 1, 1, TerminalConsumption::Delivered),
        (false, true, 1, 2, TerminalConsumption::NotDelivered),
        (false, false, 0, 2, TerminalConsumption::NotDelivered),
    ] {
        for async_cancel in [false, true] {
            let observed = Arc::new(Mutex::new(Vec::new()));
            let checked = observed.clone();
            let mut linker = Linker::<ControlState>::new(&engine);
            linker.root().func_wrap(
                "check",
                move |mut store: StoreContextMut<ControlState>, (consumed,): (bool,)| {
                    if consumed {
                        assert_eq!(*checked.lock().unwrap(), vec![consumption]);
                        assert_eq!(store.data().controls, 4);
                    } else {
                        assert!(checked.lock().unwrap().is_empty());
                        store.data_mut().ready = true;
                    }
                    store.data_mut().guest_steps += 1;
                    Ok(())
                },
            )?;
            let body = format!(
                r#"
                (call $read (local.get $reader) (i32.const 0x100) (i32.const {count}))
                i32.const -1 i32.ne if unreachable end
                (call $check (i32.const 0))
                (call $cancel (local.get $reader))
                i32.const {result} i32.ne if unreachable end
                (call $check (i32.const 1))
                (call $drop (local.get $reader))
            "#
            );
            let component = Component::new(&engine, controlled_source(&body, false, async_cancel))?;
            let mut store = Store::new(&engine, ControlState::default());
            let instance = linker.instantiate_async(&mut store, &component).await?;
            let reader = StreamReader::new(
                &mut store,
                ControlledProducer {
                    item: 57u8,
                    registered: false,
                    eof,
                    zero,
                    progress: if zero {
                        StreamReadProgress::Cancel
                    } else {
                        StreamReadProgress::Complete
                    },
                    blocked_cancels: 0,
                    observed: observed.clone(),
                },
            )?;
            instance
                .get_typed_func::<(StreamReader<u8>,), ()>(&mut store, "run")?
                .call_async(&mut store, (reader,))
                .await?;
            assert_eq!(
                store.data().boundaries,
                vec![
                    StreamReadBoundary::ReadReturn { result: u32::MAX },
                    StreamReadBoundary::BeforeCancel,
                    StreamReadBoundary::CancelReturn { result },
                    StreamReadBoundary::Settled {
                        result,
                        consumption
                    },
                ]
            );
            assert_eq!(store.data().guest_steps, 2);
            assert_eq!(store.data().cancel_count, usize::from(zero));
        }
    }
    Ok(())
}

#[tokio::test]
async fn repeated_busy_cancellation_and_later_settlement() -> Result<()> {
    let engine = control_engine()?;
    for wait in [false, true] {
        for (eof, zero, result, consumption) in [
            (false, false, 18, TerminalConsumption::Delivered),
            (true, false, 1, TerminalConsumption::Delivered),
            (false, true, 2, TerminalConsumption::NotDelivered),
        ] {
            let observed = Arc::new(Mutex::new(Vec::new()));
            let checked = observed.clone();
            let mut linker = Linker::<ControlState>::new(&engine);
            linker.root().func_wrap(
                "check",
                move |mut store: StoreContextMut<ControlState>, (consumed,): (bool,)| {
                    assert_eq!(
                        *checked.lock().unwrap(),
                        if consumed { vec![consumption] } else { vec![] }
                    );
                    if !consumed {
                        store.data_mut().ready = true;
                    }
                    Ok(())
                },
            )?;
            let settle = if wait {
                format!(
                    r#"
                    (local.set $set (call $new-set))
                    (call $join (local.get $reader) (local.get $set))
                    (drop (call $wait (local.get $set) (i32.const 0x200)))
                    (i32.load (i32.const 0x204)) i32.const {result} i32.ne if unreachable end
                    (call $join (local.get $reader) (i32.const 0))
                    (call $drop-set (local.get $set))
                "#
                )
            } else {
                format!(
                    r#"
                    (drop (call $yield)) (drop (call $yield)) (drop (call $yield))
                    (call $cancel (local.get $reader))
                    i32.const {result} i32.ne if unreachable end
                "#
                )
            };
            let body = format!(
                r#"
                (call $read (local.get $reader) (i32.const 0x100) (i32.const 1))
                i32.const -1 i32.ne if unreachable end
                ;; Prepared payload is still withheld by the replay gate even
                ;; when the ordinary event loop polls this transfer.
                (drop (call $yield)) (drop (call $yield)) (drop (call $yield))
                (call $cancel (local.get $reader)) i32.const -1 i32.ne if unreachable end
                (call $cancel (local.get $reader)) i32.const -1 i32.ne if unreachable end
                (call $check (i32.const 0))
                {settle}
                (call $check (i32.const 1))
                (call $drop (local.get $reader))
            "#
            );
            let component = Component::new(&engine, controlled_source(&body, false, true))?;
            let mut store = Store::new(&engine, ControlState::default());
            let instance = linker.instantiate_async(&mut store, &component).await?;
            let reader = StreamReader::new(
                &mut store,
                ControlledProducer {
                    item: 73u8,
                    registered: false,
                    eof,
                    zero,
                    progress: StreamReadProgress::Continue,
                    blocked_cancels: usize::MAX,
                    observed: observed.clone(),
                },
            )?;
            instance
                .get_typed_func::<(StreamReader<u8>,), ()>(&mut store, "run")?
                .call_async(&mut store, (reader,))
                .await?;
            let mut expected = vec![
                StreamReadBoundary::ReadReturn { result: u32::MAX },
                StreamReadBoundary::BeforeCancel,
                StreamReadBoundary::CancelReturn { result: u32::MAX },
                StreamReadBoundary::BeforeCancel,
                StreamReadBoundary::CancelReturn { result: u32::MAX },
            ];
            if !wait {
                expected.extend([
                    StreamReadBoundary::BeforeCancel,
                    StreamReadBoundary::CancelReturn { result },
                ]);
            }
            expected.push(StreamReadBoundary::Settled {
                result,
                consumption,
            });
            assert_eq!(store.data().boundaries, expected);
        }
    }
    Ok(())
}

#[tokio::test]
async fn zero_cancellation_retains_producer_for_next_read() -> Result<()> {
    let engine = control_engine()?;
    let observed = Arc::new(Mutex::new(Vec::new()));
    let mut linker = Linker::<ControlState>::new(&engine);
    linker.root().func_wrap(
        "check",
        |mut store: StoreContextMut<ControlState>, _: (bool,)| {
            store.data_mut().ready = true;
            Ok(())
        },
    )?;
    let component = Component::new(
        &engine,
        controlled_source(
            r#"
        (call $read (local.get $reader) (i32.const 0x100) (i32.const 1))
        i32.const -1 i32.ne if unreachable end
        (call $cancel (local.get $reader)) i32.const 2 i32.ne if unreachable end
        (call $check (i32.const 0))
        (call $read (local.get $reader) (i32.const 0x100) (i32.const 1))
        i32.const -1 i32.ne if unreachable end
        (local.set $set (call $new-set))
        (call $join (local.get $reader) (local.get $set))
        (drop (call $wait (local.get $set) (i32.const 0x200)))
        (i32.load (i32.const 0x204)) i32.const 16 i32.ne if unreachable end
        (i32.load8_u (i32.const 0x100)) i32.const 91 i32.ne if unreachable end
        (call $join (local.get $reader) (i32.const 0))
        (call $drop-set (local.get $set))
        (call $drop (local.get $reader))
    "#,
            false,
            true,
        ),
    )?;
    let mut store = Store::new(&engine, ControlState::default());
    let instance = linker.instantiate_async(&mut store, &component).await?;
    let reader = StreamReader::new(
        &mut store,
        ControlledProducer {
            item: 91u8,
            registered: false,
            eof: false,
            zero: true,
            progress: StreamReadProgress::Cancel,
            blocked_cancels: 0,
            observed: observed.clone(),
        },
    )?;
    instance
        .get_typed_func::<(StreamReader<u8>,), ()>(&mut store, "run")?
        .call_async(&mut store, (reader,))
        .await?;
    assert_eq!(
        *observed.lock().unwrap(),
        vec![
            TerminalConsumption::NotDelivered,
            TerminalConsumption::Delivered
        ]
    );
    assert_eq!(store.data().cancel_count, 1);
    Ok(())
}

#[tokio::test]
async fn control_failure_or_driver_cancellation_disposes_fiber_without_consumption() -> Result<()> {
    let engine = control_engine()?;
    for (index, boundary) in [
        StreamReadBoundary::ReadReturn { result: u32::MAX },
        StreamReadBoundary::BeforeCancel,
        StreamReadBoundary::CancelReturn { result: 18 },
        StreamReadBoundary::Settled {
            result: 18,
            consumption: TerminalConsumption::Delivered,
        },
    ]
    .into_iter()
    .enumerate()
    {
        for fail_control in [false, true] {
            let observed = Arc::new(Mutex::new(Vec::new()));
            let mut linker = Linker::<ControlState>::new(&engine);
            linker
                .root()
                .func_wrap("check", |_, _: (bool,)| -> Result<()> {
                    panic!("guest resumed after cancelled control wait")
                })?;
            let component = Component::new(
                &engine,
                controlled_source(
                    r#"
        (drop (call $read (local.get $reader) (i32.const 0x100) (i32.const 1)))
        (drop (call $cancel (local.get $reader)))
        (call $check (i32.const 0))
    "#,
                    false,
                    true,
                ),
            )?;
            let (release, parked) = tokio::sync::oneshot::channel();
            let (entered, wait_entered) = tokio::sync::oneshot::channel();
            let mut store = Store::new(
                &engine,
                ControlState {
                    ready: true,
                    park_at: Some(boundary),
                    parked: Some(parked),
                    entered: Some(entered),
                    ..Default::default()
                },
            );
            let instance = linker.instantiate_async(&mut store, &component).await?;
            let reader = StreamReader::new(
                &mut store,
                ControlledProducer {
                    item: 12u8,
                    registered: false,
                    eof: false,
                    zero: false,
                    progress: StreamReadProgress::Complete,
                    blocked_cancels: 0,
                    observed: observed.clone(),
                },
            )?;
            let run = instance.get_typed_func::<(StreamReader<u8>,), ()>(&mut store, "run")?;
            let mut call = Box::pin(run.call_async(&mut store, (reader,)));
            tokio::select! {
                result = &mut call => panic!("control wait completed: {result:?}"),
                result = wait_entered => result?,
            }
            if fail_control {
                drop(release);
                assert!(call.await.is_err());
            } else {
                drop(call);
            }
            assert_eq!(store.data().controls, index + 1);
            assert_eq!(store.data().control_drops.load(Ordering::SeqCst), index + 1);
            assert_eq!(store.data().boundaries.len(), index);
            drop(store);
            assert!(observed.lock().unwrap().is_empty());
        }
    }
    Ok(())
}

#[tokio::test]
async fn designated_lowering_does_not_let_runnable_fibers_or_callbacks_overtake() -> Result<()> {
    let engine = control_engine()?;
    let observed = Arc::new(Mutex::new(Vec::new()));
    let checked = observed.clone();
    let mut linker = Linker::<ControlState>::new(&engine);
    linker.root().func_wrap(
        "check",
        move |mut store: StoreContextMut<ControlState>, (consumed,): (bool,)| {
            // Both unrelated continuations are runnable on the low-priority queue.
            assert_eq!(store.data().guest_steps, 0);
            if consumed {
                assert_eq!(
                    *checked.lock().unwrap(),
                    vec![TerminalConsumption::Delivered]
                );
                assert_eq!(store.data().reallocs, 1);
            } else {
                store.data_mut().ready = true;
            }
            Ok(())
        },
    )?;
    linker.root().func_wrap(
        "realloc-check",
        |mut store: StoreContextMut<ControlState>, (): ()| {
            // ReadReturn and BeforeCancel host phases have completed. Neither
            // CancelReturn nor Settled has started, and neither side task ran.
            assert_eq!(store.data().controls, 2);
            assert_eq!(store.data().boundaries.len(), 2);
            assert_eq!(store.data().guest_steps, 0);
            store.data_mut().reallocs += 1;
            Ok(())
        },
    )?;
    linker.root().func_wrap(
        "mark",
        |mut store: StoreContextMut<ControlState>, (phase,): (u32,)| {
            if phase < 2 {
                store.data_mut().side_ready[phase as usize]
                    .take()
                    .unwrap()
                    .send(())
                    .unwrap();
            } else {
                store.data_mut().guest_steps += 1;
            }
            Ok(())
        },
    )?;
    let side = Component::new(
        &engine,
        r#"
        (component
            (import "mark" (func $mark (param "phase" u32)))
            (core func $mark (canon lower (func $mark)))
            (core func $yield (canon thread.yield))
            (core func $return (canon task.return))
            (core module $m
                (import "" "mark" (func $mark (param i32)))
                (import "" "yield" (func $yield (result i32)))
                (import "" "return" (func $return))
                (func (export "fiber")
                    i32.const 0 call $mark (drop (call $yield)) i32.const 2 call $mark)
                (func (export "stackless") (result i32)
                    i32.const 1 call $mark i32.const 1)
                (func (export "callback") (param i32 i32 i32) (result i32)
                    i32.const 3 call $mark call $return i32.const 0))
            (core instance $i (instantiate $m (with "" (instance
                (export "mark" (func $mark)) (export "yield" (func $yield))
                (export "return" (func $return))))))
            (func (export "fiber") async (canon lift (core func $i "fiber")))
            (func (export "stackless") async
                (canon lift (core func $i "stackless") async (callback (func $i "callback")))))
    "#,
    )?;
    let component = Component::new(
        &engine,
        controlled_source(
            r#"
        (call $read (local.get $reader) (i32.const 0x100) (i32.const 1))
        i32.const -1 i32.ne if unreachable end
        (call $check (i32.const 0))
        (call $cancel (local.get $reader)) i32.const 18 i32.ne if unreachable end
        (i32.load (i32.const 0x100)) i32.const 0x400 i32.ne if unreachable end
        (i32.load (i32.const 0x104)) i32.const 5 i32.ne if unreachable end
        (i32.load (i32.const 0x400)) i32.const 0x6c6c6568 i32.ne if unreachable end
        (i32.load8_u (i32.const 0x404)) i32.const 111 i32.ne if unreachable end
        (call $check (i32.const 1))
        (call $drop (local.get $reader))
    "#,
            true,
            true,
        ),
    )?;
    let (tx0, rx0) = tokio::sync::oneshot::channel();
    let (tx1, rx1) = tokio::sync::oneshot::channel();
    let mut store = Store::new(
        &engine,
        ControlState {
            side_ready: vec![Some(tx0), Some(tx1)],
            ..Default::default()
        },
    );
    let fiber_instance = linker.instantiate_async(&mut store, &side).await?;
    let callback_instance = linker.instantiate_async(&mut store, &side).await?;
    let instance = linker.instantiate_async(&mut store, &component).await?;
    let fiber = fiber_instance.get_typed_func::<(), ()>(&mut store, "fiber")?;
    let callback = callback_instance.get_typed_func::<(), ()>(&mut store, "stackless")?;
    let run = instance.get_typed_func::<(StreamReader<String>,), ()>(&mut store, "run")?;
    let reader = StreamReader::new(
        &mut store,
        ControlledProducer {
            item: "hello".to_owned(),
            registered: false,
            eof: false,
            zero: false,
            progress: StreamReadProgress::Complete,
            blocked_cancels: 0,
            observed: observed.clone(),
        },
    )?;
    store
        .run_concurrent_and_drain(async |accessor| -> Result<()> {
            // Poll side calls concurrently until both have yielded and queued
            // continuations, then admit the high-priority cancelling task.
            let main = async {
                rx0.await?;
                rx1.await?;
                run.call_concurrent(accessor, (reader,)).await
            };
            let side_fiber = fiber.call_concurrent(accessor, ());
            let side_callback = callback.call_concurrent(accessor, ());
            tokio::try_join!(main, side_fiber, side_callback)?;
            Ok(())
        })
        .await??;
    assert_eq!(store.data().guest_steps, 2);
    assert_eq!(store.data().reallocs, 1, "stale lowering proxy ran twice");
    assert_eq!(store.data().controls, 4);
    assert_eq!(store.data().control_drops.load(Ordering::SeqCst), 4);
    assert_eq!(
        *observed.lock().unwrap(),
        vec![TerminalConsumption::Delivered]
    );
    Ok(())
}

#[tokio::test]
async fn positive_cancellation_is_not_redelivered_on_next_read() -> Result<()> {
    let engine = control_engine()?;
    let observed = Arc::new(Mutex::new(Vec::new()));
    let mut linker = Linker::<ControlState>::new(&engine);
    linker.root().func_wrap(
        "check",
        |mut store: StoreContextMut<ControlState>, _: (bool,)| {
            store.data_mut().ready = true;
            Ok(())
        },
    )?;
    let component = Component::new(
        &engine,
        controlled_source(
            r#"
        (call $read (local.get $reader) (i32.const 0x100) (i32.const 1))
        i32.const -1 i32.ne if unreachable end
        (call $check (i32.const 0))
        (drop (call $yield)) (drop (call $yield)) (drop (call $yield))
        (call $cancel (local.get $reader)) i32.const 18 i32.ne if unreachable end
        (i32.load8_u (i32.const 0x100)) i32.const 67 i32.ne if unreachable end
        (call $read (local.get $reader) (i32.const 0x101) (i32.const 1))
        i32.const -1 i32.ne if unreachable end
        (local.set $set (call $new-set))
        (call $join (local.get $reader) (local.get $set))
        (drop (call $wait (local.get $set) (i32.const 0x200)))
        (i32.load (i32.const 0x204)) i32.const 1 i32.ne if unreachable end
        (i32.load8_u (i32.const 0x101)) i32.const 0 i32.ne if unreachable end
        (call $join (local.get $reader) (i32.const 0))
        (call $drop-set (local.get $set))
        (call $drop (local.get $reader))
    "#,
            false,
            true,
        ),
    )?;
    let mut store = Store::new(&engine, ControlState::default());
    let instance = linker.instantiate_async(&mut store, &component).await?;
    let reader = StreamReader::new(
        &mut store,
        ControlledProducer {
            item: 67u8,
            registered: false,
            eof: false,
            zero: false,
            progress: StreamReadProgress::Cancel,
            blocked_cancels: 0,
            observed: observed.clone(),
        },
    )?;
    instance
        .get_typed_func::<(StreamReader<u8>,), ()>(&mut store, "run")?
        .call_async(&mut store, (reader,))
        .await?;
    assert_eq!(
        *observed.lock().unwrap(),
        vec![
            TerminalConsumption::Delivered,
            TerminalConsumption::Delivered
        ]
    );
    Ok(())
}

#[tokio::test]
async fn cancellation_after_inline_read_is_illegal() -> Result<()> {
    let engine = control_engine()?;
    for async_cancel in [false, true] {
        let mut linker = Linker::<()>::new(&engine);
        linker.root().func_wrap("check", |_, _: (bool,)| Ok(()))?;
        let component = Component::new(
            &engine,
            controlled_source(
                r#"
            (call $read (local.get $reader) (i32.const 0x100) (i32.const 1))
            i32.const 16 i32.ne if unreachable end
            (drop (call $cancel (local.get $reader)))
        "#,
                false,
                async_cancel,
            ),
        )?;
        let mut store = Store::new(&engine, ());
        let instance = linker.instantiate_async(&mut store, &component).await?;
        let reader = StreamReader::new(
            &mut store,
            Producer {
                pending: false,
                terminal: false,
                retain_on_cancel: false,
                registered: false,
                observed: Arc::new(Mutex::new(Vec::new())),
            },
        )?;
        let error = instance
            .get_typed_func::<(StreamReader<u8>,), ()>(&mut store, "run")?
            .call_async(&mut store, (reader,))
            .await
            .unwrap_err();
        assert!(format!("{error:?}").contains("cancelled when no read is pending"));
    }
    Ok(())
}

#[tokio::test]
async fn late_control_registration_completes_existing_transfer() -> Result<()> {
    struct LateProducer(usize);
    impl StreamProducer<ControlState> for LateProducer {
        type Item = u8;
        type Buffer = Option<u8>;
        fn poll_produce<'a>(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            mut store: StoreContextMut<'a, ControlState>,
            mut destination: Destination<'a, u8, Option<u8>>,
            _: bool,
        ) -> Poll<Result<StreamResult>> {
            self.0 += 1;
            if self.0 == 1 {
                cx.waker().wake_by_ref();
                return Poll::Pending;
            }
            if self.0 == 2 {
                destination.register_read_control(&mut store, |boundary| {
                    (
                        if boundary == StreamReadBoundary::BeforeCancel {
                            StreamReadProgress::Complete
                        } else {
                            StreamReadProgress::Continue
                        },
                        None::<ControlTask>,
                    )
                })?;
                store.data_mut().ready = true;
                return Poll::Pending;
            }
            destination.set_buffer(Some(42));
            Poll::Ready(Ok(StreamResult::Completed))
        }
    }
    let engine = control_engine()?;
    let mut linker = Linker::<ControlState>::new(&engine);
    linker.root().func_wrap(
        "check",
        |store: StoreContextMut<ControlState>, _: (bool,)| {
            assert!(
                store.data().ready,
                "control must be registered before cancel"
            );
            Ok(())
        },
    )?;
    let component = Component::new(
        &engine,
        controlled_source(
            r#"
        (call $read (local.get $reader) (i32.const 0x100) (i32.const 1))
        i32.const -1 i32.ne if unreachable end
        (drop (call $yield)) (drop (call $yield)) (drop (call $yield))
        (call $check (i32.const 0))
        (call $cancel (local.get $reader))
        i32.const 18 i32.ne if unreachable end
        (call $drop (local.get $reader))
    "#,
            false,
            true,
        ),
    )?;
    let mut store = Store::new(&engine, ControlState::default());
    let instance = linker.instantiate_async(&mut store, &component).await?;
    let reader = StreamReader::new(&mut store, LateProducer(0))?;
    instance
        .get_typed_func::<(StreamReader<u8>,), ()>(&mut store, "run")?
        .call_async(&mut store, (reader,))
        .await?;
    Ok(())
}
