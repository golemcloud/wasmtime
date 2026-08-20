#![cfg(all(
    feature = "component-model-async",
    feature = "cranelift",
    feature = "wat"
))]

use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use wasmtime::component::{Component, FutureProducer, FutureReader, Linker, TerminalConsumption};
use wasmtime::{Config, Engine, Result, Store, StoreContextMut};

const COMPONENT: &str = r#"
(component
  (core module $libc (memory (export "memory") 1))
  (core instance $libc (instantiate $libc))

  (core module $m
    (import "" "future.read" (func $future.read (param i32 i32) (result i32)))
    (import "" "future.cancel-read" (func $future.cancel-read (param i32) (result i32)))
    (import "" "future.drop-readable" (func $future.drop-readable (param i32)))
    (import "" "waitable.join" (func $waitable.join (param i32 i32)))
    (import "" "waitable-set.new" (func $waitable-set.new (result i32)))
    (import "" "waitable-set.wait" (func $waitable-set.wait (param i32 i32) (result i32)))
    (import "" "waitable-set.drop" (func $waitable-set.drop (param i32)))
    (import "" "thread.yield" (func $thread.yield (result i32)))

    (func (export "delivered") (param $future i32)
      (local $waitable-set i32)

      (call $future.read (local.get $future) (i32.const 0x100))
      i32.const -1 ;; BLOCKED
      i32.ne
      if unreachable end

      (local.set $waitable-set (call $waitable-set.new))
      (call $waitable.join (local.get $future) (local.get $waitable-set))
      (drop (call $waitable-set.wait (local.get $waitable-set) (i32.const 0x200)))
      (call $waitable.join (local.get $future) (i32.const 0))
      (call $future.drop-readable (local.get $future))
      (call $waitable-set.drop (local.get $waitable-set))
    )

    (func (export "not-delivered") (param $future i32)
      (call $future.read (local.get $future) (i32.const 0x100))
      i32.const -1 ;; BLOCKED
      i32.ne
      if unreachable end

      ;; Let the producer queue its completed event, then drop the future without
      ;; consuming that event.
      (drop (call $thread.yield))
      (drop (call $thread.yield))
      (drop (call $thread.yield))
      (call $future.cancel-read (local.get $future))
      i32.const 0 ;; COMPLETED, consumed by cancellation rather than delivery
      i32.ne
      if unreachable end
      (call $future.drop-readable (local.get $future))
    )
  )

  (type $future (future u32))
  (core func $future.read (canon future.read $future async (memory $libc "memory")))
  (core func $future.cancel-read (canon future.cancel-read $future))
  (core func $future.drop-readable (canon future.drop-readable $future))
  (canon waitable.join (core func $waitable.join))
  (canon waitable-set.new (core func $waitable-set.new))
  (canon waitable-set.wait (memory $libc "memory") (core func $waitable-set.wait))
  (canon waitable-set.drop (core func $waitable-set.drop))
  (core func $thread.yield (canon thread.yield))

  (core instance $i (instantiate $m
    (with "" (instance
      (export "future.read" (func $future.read))
      (export "future.cancel-read" (func $future.cancel-read))
      (export "future.drop-readable" (func $future.drop-readable))
      (export "waitable.join" (func $waitable.join))
      (export "waitable-set.new" (func $waitable-set.new))
      (export "waitable-set.wait" (func $waitable-set.wait))
      (export "waitable-set.drop" (func $waitable-set.drop))
      (export "thread.yield" (func $thread.yield))
    ))
  ))

  (func (export "delivered") async (param "future" $future)
    (canon lift (core func $i "delivered") (memory $libc "memory")))
  (func (export "not-delivered") async (param "future" $future)
    (canon lift (core func $i "not-delivered") (memory $libc "memory")))
)
"#;

struct YieldOnce(bool);

impl FutureProducer<()> for YieldOnce {
    type Item = u32;

    fn poll_produce(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        _store: StoreContextMut<()>,
        finish: bool,
    ) -> Poll<Result<Option<Self::Item>>> {
        if finish {
            return Poll::Ready(Ok(None));
        }
        if !self.0 {
            self.0 = true;
            cx.waker().wake_by_ref();
            Poll::Pending
        } else {
            Poll::Ready(Ok(Some(42)))
        }
    }
}

#[tokio::test]
async fn observes_guest_consumption_of_host_future() -> Result<()> {
    let mut config = Config::new();
    config.wasm_component_model_async(true);
    config.wasm_component_model_more_async_builtins(true);
    let engine = Engine::new(&config)?;
    let component = Component::new(&engine, COMPONENT)?;
    let mut store = Store::new(&engine, ());
    let instance = Linker::new(&engine)
        .instantiate_async(&mut store, &component)
        .await?;

    let delivered = Arc::new(Mutex::new(Vec::new()));
    let mut reader = FutureReader::new(&mut store, YieldOnce(false))?;
    let delivered_observer = delivered.clone();
    reader.register_terminal_observer(&mut store, move |consumption| {
        delivered_observer.lock().unwrap().push(consumption);
    })?;
    instance
        .get_typed_func::<(FutureReader<u32>,), ()>(&mut store, "delivered")?
        .call_async(&mut store, (reader,))
        .await?;
    assert_eq!(
        *delivered.lock().unwrap(),
        vec![TerminalConsumption::Delivered]
    );

    let superseded = Arc::new(Mutex::new(Vec::new()));
    let not_delivered = Arc::new(Mutex::new(Vec::new()));
    let mut reader = FutureReader::new(&mut store, YieldOnce(false))?;
    let superseded_observer = superseded.clone();
    reader.register_terminal_observer(&mut store, move |consumption| {
        superseded_observer.lock().unwrap().push(consumption);
    })?;
    let not_delivered_observer = not_delivered.clone();
    reader.register_terminal_observer(&mut store, move |consumption| {
        not_delivered_observer.lock().unwrap().push(consumption);
    })?;
    instance
        .get_typed_func::<(FutureReader<u32>,), ()>(&mut store, "not-delivered")?
        .call_async(&mut store, (reader,))
        .await?;
    assert_eq!(
        *superseded.lock().unwrap(),
        vec![TerminalConsumption::Superseded]
    );
    assert_eq!(
        *not_delivered.lock().unwrap(),
        vec![TerminalConsumption::NotDelivered]
    );

    Ok(())
}
