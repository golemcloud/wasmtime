#![cfg(all(
    feature = "component-model-async",
    feature = "cranelift",
    feature = "wat"
))]

use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use wasmtime::component::{
    Component, Destination, Linker, StreamProducer, StreamReader, StreamResult, TerminalConsumption,
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
        if finish {
            return Poll::Ready(Ok(StreamResult::Cancelled));
        }
        if self.pending {
            self.pending = false;
            cx.waker().wake_by_ref();
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
