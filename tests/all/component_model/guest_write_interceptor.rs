#![cfg(not(miri))]

use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use wasmtime::component::*;
use wasmtime::{AsContextMut, Config, Engine, Result, Store, StoreContextMut};

struct Hook {
    pending: bool,
    prefix: bool,
    observations: Arc<Mutex<Vec<TerminalConsumption>>>,
    starts: Arc<Mutex<usize>>,
}

impl GuestWriteInterceptor<(), u8> for Hook {
    fn write_started(self: Pin<&mut Self>, _: StoreContextMut<'_, ()>) -> Result<()> {
        *self.starts.lock().unwrap() += 1;
        Ok(())
    }

    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        mut store: StoreContextMut<'_, ()>,
        mut source: Source<'_, u8>,
        finish: bool,
        native: &mut dyn FnMut(
            &mut Context<'_>,
            StoreContextMut<'_, ()>,
            Source<'_, u8>,
            bool,
        ) -> Poll<Result<StreamResult>>,
    ) -> Poll<Result<StreamResult>> {
        if !self.pending {
            self.pending = true;
            cx.waker().wake_by_ref();
            return Poll::Pending;
        }
        let result = if self.prefix {
            let mut prefix = Vec::with_capacity(16);
            source.read_count(store.as_context_mut(), &mut prefix, 1)?;
            assert_eq!(prefix.len(), 1);
            let mut prefix = VecBuffer::from(prefix);
            std::task::ready!(native(
                cx,
                store.as_context_mut(),
                source.with_buffer(&mut prefix),
                finish
            ))?
        } else {
            std::task::ready!(native(
                cx,
                store.as_context_mut(),
                source.reborrow(),
                finish
            ))?
        };
        let observations = self.observations.clone();
        source.register_write_observer(store, move |result| {
            observations.lock().unwrap().push(result)
        })?;
        self.pending = false;
        Poll::Ready(Ok(result))
    }
}

struct Consumer(Arc<Mutex<Vec<u8>>>, bool, bool);

impl StreamConsumer<()> for Consumer {
    type Item = u8;
    fn poll_consume(
        mut self: Pin<&mut Self>,
        _: &mut Context<'_>,
        mut store: StoreContextMut<'_, ()>,
        source: Source<'_, u8>,
        finish: bool,
    ) -> Poll<Result<StreamResult>> {
        if self.1 {
            return if finish {
                self.1 = false;
                Poll::Ready(Ok(StreamResult::Cancelled))
            } else {
                Poll::Pending
            };
        }
        if self.2 {
            assert_eq!(source.remaining(store.as_context_mut()), 1);
        }
        let mut source = source.as_direct(store);
        let n = if self.2 { source.remaining().len() } else { 1 };
        self.0
            .lock()
            .unwrap()
            .extend_from_slice(&source.remaining()[..n]);
        source.mark_read(n);
        Poll::Ready(Ok(StreamResult::Completed))
    }
}

impl FutureConsumer<()> for Consumer {
    type Item = u8;

    fn poll_consume(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        store: StoreContextMut<'_, ()>,
        source: Source<'_, u8>,
        finish: bool,
    ) -> Poll<Result<()>> {
        StreamConsumer::poll_consume(self, cx, store, source, finish)
            .map(|result| result.map(|_| ()))
    }
}

#[tokio::test]
async fn partial_write_observed_at_callback_and_dropped_reader_is_intercepted() -> Result<()> {
    for is_future in [false, true] {
        for (drop_reader, cancel, drop_pending, prefix, before_pipe) in [
            (false, false, false, false, false),
            (true, false, false, false, false),
            (false, true, false, false, false),
            (false, false, true, false, false),
            (false, false, false, true, false),
            (false, false, false, false, true),
            (false, true, false, false, true),
            (false, false, false, true, true),
        ] {
            // A cancelled future must be retried before dropping its writer.
            if is_future && cancel {
                continue;
            }
            let engine = Engine::new(
                Config::new()
                    .wasm_component_model_async(true)
                    .wasm_component_model_more_async_builtins(true),
            )?;
            let wat = r#"
                (component
                  (type $s (stream u8))
                  (import "attach" (func $attach (param "s" $s)))
                  (import "observed" (func $observed (param "code" u32)))
                  (import "drop-pending" (func $drop-pending))
                  (core module $mem (memory (export "m") 1) (data (i32.const 0) "abcd"))
                  (core instance $mem (instantiate $mem))
                  (core func $attach (canon lower (func $attach)))
                  (core func $observed (canon lower (func $observed)))
                  (core func $drop-pending (canon lower (func $drop-pending)))
                  (core func $new (canon stream.new $s))
                  (core func $write (canon stream.write $s async (memory (core memory $mem "m"))))
                  (core func $cancel (canon stream.cancel-write $s async))
                  (core func $drop (canon stream.drop-writable $s))
                  (core func $join (canon waitable.join))
                  (core func $set (canon waitable-set.new))
                  (core func $return (canon task.return))
                  (core module $m
                    (import "" "attach" (func $attach (param i32)))
                    (import "" "observed" (func $observed (param i32)))
                    (import "" "drop-pending" (func $drop-pending))
                    (import "" "new" (func $new (result i64)))
                    (import "" "write" (func $write (param i32 i32 i32) (result i32)))
                    (import "" "cancel" (func $cancel (param i32) (result i32)))
                    (import "" "drop" (func $drop (param i32)))
                    (import "" "join" (func $join (param i32 i32)))
                    (import "" "set" (func $set (result i32)))
                    (import "" "return" (func $return))
                    (global $writer (mut i32) (i32.const 0))
                    (func (export "run") (param i32) (result i32)
                      (local $pair i64) (local $set i32)
                      (local.set $pair (call $new))
                      (global.set $writer (i32.wrap_i64 (i64.shr_u (local.get $pair) (i64.const 32))))
                      (call $attach (i32.wrap_i64 (local.get $pair)))
                      (if (i32.ne (call $write (global.get $writer) (i32.const 0) (i32.const 4)) (i32.const -1)) (then unreachable))
                      (call $drop-pending)
                      (if (local.get 0) (then
                        (if (i32.ne (call $cancel (global.get $writer)) (i32.const -1)) (then unreachable))))
                      (local.set $set (call $set))
                      (call $join (global.get $writer) (local.get $set))
                      (i32.or (i32.const 2) (i32.shl (local.get $set) (i32.const 4))))
                    (func (export "cb") (param i32 i32 i32) (result i32)
                      (call $observed (local.get 2))
                      (call $join (global.get $writer) (i32.const 0))
                      (call $drop (global.get $writer))
                      (call $return)
                      (i32.const 0)))
                  (core instance $i (instantiate $m (with "" (instance
                    (export "attach" (func $attach)) (export "observed" (func $observed))
                    (export "drop-pending" (func $drop-pending))
                    (export "new" (func $new)) (export "write" (func $write))
                    (export "cancel" (func $cancel))
                    (export "drop" (func $drop)) (export "join" (func $join))
                    (export "set" (func $set)) (export "return" (func $return))))))
                  (func (export "run") async (param "cancel" u32) (canon lift (core func $i "run") async (callback (core func $i "cb")))))
            "#;
            let wat = if is_future {
                wat.replace("(stream u8)", "(future u8)")
                    .replace("canon stream.", "canon future.")
                    .replace(
                        "(func $write (param i32 i32 i32)",
                        "(func $write (param i32 i32)",
                    )
                    .replace(
                        "(call $write (global.get $writer) (i32.const 0) (i32.const 4))",
                        "(call $write (global.get $writer) (i32.const 0))",
                    )
            } else {
                wat.to_owned()
            };
            let before_install = prefix && before_pipe;
            let wat = if before_install {
                let attach = "(call $attach (i32.wrap_i64 (local.get $pair)))";
                let write = if is_future {
                    "(if (i32.ne (call $write (global.get $writer) (i32.const 0)) (i32.const -1)) (then unreachable))"
                } else {
                    "(if (i32.ne (call $write (global.get $writer) (i32.const 0) (i32.const 4)) (i32.const -1)) (then unreachable))"
                };
                assert_eq!(wat.matches(attach).count(), 1);
                assert_eq!(wat.matches(write).count(), 1);
                wat.replace(attach, "")
                    .replace(write, &format!("{write}\n{attach}"))
            } else {
                wat
            };
            let component = Component::new(&engine, &wat)?;
            let observations = Arc::new(Mutex::new(Vec::new()));
            let starts = Arc::new(Mutex::new(0));
            let bytes = Arc::new(Mutex::new(Vec::new()));
            let pending = Arc::new(Mutex::new(None::<StreamAny>));
            let pending_future = Arc::new(Mutex::new(None::<FutureAny>));
            let mut linker = Linker::new(&engine);
            if is_future {
                linker.root().func_wrap("attach", {
                    let observations = observations.clone();
                    let starts = starts.clone();
                    let bytes = bytes.clone();
                    let pending = pending_future.clone();
                    move |mut store: StoreContextMut<'_, ()>, (mut reader,): (FutureReader<u8>,)| {
                        assert!(!reader.has_guest_write_interceptor(&mut store)?);
                        reader.intercept_guest_writes(
                            &mut store,
                            Hook {
                                pending: false,
                                prefix,
                                observations: observations.clone(),
                                starts: starts.clone(),
                            },
                        )?;
                        assert_eq!(*starts.lock().unwrap(), usize::from(before_install));
                        assert!(reader.has_guest_write_interceptor(&mut store)?);
                        if drop_reader {
                            reader.close(store)?;
                        } else if before_pipe {
                            *pending.lock().unwrap() =
                                Some(reader.try_into_future_any(&mut store).unwrap());
                        } else {
                            if drop_pending {
                                let erased = reader.try_into_future_any(&mut store).unwrap();
                                *pending.lock().unwrap() = Some(erased.clone());
                                reader = erased.try_into_future_reader().unwrap();
                            }
                            reader.pipe(store, Consumer(bytes.clone(), cancel, prefix))?;
                        }
                        Ok(())
                    }
                })?;
            } else {
                linker.root().func_wrap("attach", {
                    let observations = observations.clone();
                    let starts = starts.clone();
                    let bytes = bytes.clone();
                    let pending = pending.clone();
                    move |mut store: StoreContextMut<'_, ()>, (mut reader,): (StreamReader<u8>,)| {
                        assert!(!reader.has_guest_write_interceptor(&mut store)?);
                        reader.intercept_guest_writes(
                            &mut store,
                            Hook {
                                pending: false,
                                prefix,
                                observations: observations.clone(),
                                starts: starts.clone(),
                            },
                        )?;
                        assert_eq!(*starts.lock().unwrap(), usize::from(before_install));
                        assert!(reader.has_guest_write_interceptor(&mut store)?);
                        if drop_reader {
                            reader.close(store)?;
                        } else if before_pipe {
                            *pending.lock().unwrap() =
                                Some(reader.try_into_stream_any(&mut store).unwrap());
                        } else {
                            if drop_pending {
                                let erased = reader.try_into_stream_any(&mut store).unwrap();
                                *pending.lock().unwrap() = Some(erased.clone());
                                reader = erased.try_into_stream_reader().unwrap();
                            }
                            reader.pipe(store, Consumer(bytes.clone(), cancel, prefix))?;
                        }
                        Ok(())
                    }
                })?;
            }
            linker.root().func_wrap("drop-pending", {
                let bytes = bytes.clone();
                let starts = starts.clone();
                move |mut store: StoreContextMut<'_, ()>, ()| {
                    assert_eq!(*starts.lock().unwrap(), 1);
                    if before_pipe && cancel {
                        return Ok(());
                    }
                    if let Some(mut reader) = pending.lock().unwrap().take() {
                        if before_pipe {
                            reader
                                .try_into_stream_reader::<u8>()
                                .unwrap()
                                .pipe(&mut store, Consumer(bytes.clone(), false, false))?;
                        } else {
                            reader.close(&mut store)?;
                        }
                    }
                    if let Some(mut reader) = pending_future.lock().unwrap().take() {
                        if before_pipe {
                            reader
                                .try_into_future_reader::<u8>()
                                .unwrap()
                                .pipe(&mut store, Consumer(bytes.clone(), false, false))?;
                        } else {
                            reader.close(&mut store)?;
                        }
                    }
                    Ok(())
                }
            })?;
            linker.root().func_wrap("observed", {
                let observations = observations.clone();
                let starts = starts.clone();
                move |_: StoreContextMut<'_, ()>, (code,): (u32,)| {
                    assert_eq!(*starts.lock().unwrap(), 1);
                    assert_eq!(
                        code,
                        if cancel {
                            2
                        } else if drop_reader || drop_pending {
                            1
                        } else if is_future {
                            0
                        } else {
                            16
                        }
                    );
                    assert_eq!(
                        *observations.lock().unwrap(),
                        vec![TerminalConsumption::Delivered]
                    );
                    Ok(())
                }
            })?;
            let mut store = Store::new(&engine, ());
            let instance = linker.instantiate_async(&mut store, &component).await?;
            let run = instance.get_typed_func::<(u32,), ()>(&mut store, "run")?;
            tokio::time::timeout(
                std::time::Duration::from_secs(10),
                run.call_async(&mut store, (u32::from(cancel),)),
            )
            .await
            .unwrap()?;
            assert_eq!(
                *bytes.lock().unwrap(),
                if drop_reader || cancel || drop_pending {
                    vec![]
                } else {
                    vec![b'a']
                }
            );
        }
    }
    Ok(())
}

#[tokio::test]
async fn cancelled_future_write_can_retry_before_and_after_consumer_installation() -> Result<()> {
    for before_pipe in [false, true] {
        let engine = Engine::new(
            Config::new()
                .wasm_component_model_async(true)
                .wasm_component_model_more_async_builtins(true),
        )?;
        let component = Component::new(
            &engine,
            r#"
                (component
                  (type $f (future u8))
                  (import "attach" (func $attach (param "f" $f)))
                  (import "observed" (func $observed (param "code" u32)))
                  (core module $mem (memory (export "m") 1) (data (i32.const 0) "a"))
                  (core instance $mem (instantiate $mem))
                  (core func $attach (canon lower (func $attach)))
                  (core func $observed (canon lower (func $observed)))
                  (core func $new (canon future.new $f))
                  (core func $write (canon future.write $f async (memory (core memory $mem "m"))))
                  (core func $cancel (canon future.cancel-write $f async))
                  (core func $drop (canon future.drop-writable $f))
                  (core func $join (canon waitable.join))
                  (core func $set (canon waitable-set.new))
                  (core func $return (canon task.return))
                  (core module $m
                    (import "" "attach" (func $attach (param i32)))
                    (import "" "observed" (func $observed (param i32)))
                    (import "" "new" (func $new (result i64)))
                    (import "" "write" (func $write (param i32 i32) (result i32)))
                    (import "" "cancel" (func $cancel (param i32) (result i32)))
                    (import "" "drop" (func $drop (param i32)))
                    (import "" "join" (func $join (param i32 i32)))
                    (import "" "set" (func $set (result i32)))
                    (import "" "return" (func $return))
                    (global $writer (mut i32) (i32.const 0))
                    (global $set (mut i32) (i32.const 0))
                    (global $retry (mut i32) (i32.const 0))
                    (func (export "run") (result i32)
                      (local $pair i64)
                      (local.set $pair (call $new))
                      (global.set $writer (i32.wrap_i64 (i64.shr_u (local.get $pair) (i64.const 32))))
                      (call $attach (i32.wrap_i64 (local.get $pair)))
                      (if (i32.ne (call $write (global.get $writer) (i32.const 0)) (i32.const -1)) (then unreachable))
                      (if (i32.ne (call $cancel (global.get $writer)) (i32.const -1)) (then unreachable))
                      (global.set $set (call $set))
                      (call $join (global.get $writer) (global.get $set))
                      (i32.or (i32.const 2) (i32.shl (global.get $set) (i32.const 4))))
                    (func (export "cb") (param i32 i32 i32) (result i32)
                      (call $observed (local.get 2))
                      (if (i32.eqz (global.get $retry)) (then
                        (global.set $retry (i32.const 1))
                        (if (i32.ne (call $write (global.get $writer) (i32.const 0)) (i32.const -1)) (then unreachable))
                        (return (i32.or (i32.const 2) (i32.shl (global.get $set) (i32.const 4))))))
                      (call $join (global.get $writer) (i32.const 0))
                      (call $drop (global.get $writer))
                      (call $return)
                      (i32.const 0)))
                  (core instance $i (instantiate $m (with "" (instance
                    (export "attach" (func $attach)) (export "observed" (func $observed))
                    (export "new" (func $new)) (export "write" (func $write))
                    (export "cancel" (func $cancel)) (export "drop" (func $drop))
                    (export "join" (func $join)) (export "set" (func $set))
                    (export "return" (func $return))))))
                  (func (export "run") async (canon lift (core func $i "run") async (callback (core func $i "cb")))))
            "#,
        )?;
        let observations = Arc::new(Mutex::new(Vec::new()));
        let codes = Arc::new(Mutex::new(Vec::new()));
        let bytes = Arc::new(Mutex::new(Vec::new()));
        let pending = Arc::new(Mutex::new(None::<FutureReader<u8>>));
        let mut linker = Linker::new(&engine);
        linker.root().func_wrap("attach", {
            let observations = observations.clone();
            let pending = pending.clone();
            let bytes = bytes.clone();
            move |mut store: StoreContextMut<'_, ()>, (mut reader,): (FutureReader<u8>,)| {
                reader.intercept_guest_writes(
                    &mut store,
                    Hook {
                        pending: false,
                        prefix: false,
                        observations: observations.clone(),
                        starts: Arc::default(),
                    },
                )?;
                if before_pipe {
                    *pending.lock().unwrap() = Some(reader);
                    Ok(())
                } else {
                    reader.pipe(store, Consumer(bytes.clone(), true, false))
                }
            }
        })?;
        linker.root().func_wrap("observed", {
            let codes = codes.clone();
            let pending = pending.clone();
            let bytes = bytes.clone();
            move |mut store: StoreContextMut<'_, ()>, (code,): (u32,)| {
                codes.lock().unwrap().push(code);
                if let Some(reader) = pending.lock().unwrap().take() {
                    reader.pipe(&mut store, Consumer(bytes.clone(), false, false))?;
                }
                Ok(())
            }
        })?;
        let mut store = Store::new(&engine, ());
        let instance = linker.instantiate_async(&mut store, &component).await?;
        let run = instance.get_typed_func::<(), ()>(&mut store, "run")?;
        tokio::time::timeout(
            std::time::Duration::from_secs(10),
            run.call_async(&mut store, ()),
        )
        .await??;
        assert_eq!(*codes.lock().unwrap(), vec![2, 0]);
        assert_eq!(*bytes.lock().unwrap(), vec![b'a']);
        assert_eq!(
            *observations.lock().unwrap(),
            vec![TerminalConsumption::Delivered; 2]
        );
    }
    Ok(())
}

#[tokio::test]
async fn unwritten_stream_can_close_before_its_consumer_is_installed() -> Result<()> {
    for (close_before_registration, close_reader) in [(false, false), (true, false), (false, true)]
    {
        let engine = Engine::new(
            Config::new()
                .wasm_component_model_async(true)
                .wasm_component_model_more_async_builtins(true),
        )?;
        let close = "(call $drop (i32.wrap_i64 (i64.shr_u (local.get $pair) (i64.const 32))))";
        let attach = "(call $attach (i32.wrap_i64 (local.get $pair)))";
        let operations = if close_before_registration {
            format!("{close} {attach}")
        } else {
            format!("{attach} {close}")
        };
        let component = Component::new(
            &engine,
            format!(
                r#"
            (component
              (type $s (stream u8))
              (import "attach" (func $attach (param "s" $s)))
              (core func $attach (canon lower (func $attach)))
              (core func $new (canon stream.new $s))
              (core func $drop (canon stream.drop-writable $s))
              (core module $m
                (import "" "attach" (func $attach (param i32)))
                (import "" "new" (func $new (result i64)))
                (import "" "drop" (func $drop (param i32)))
                (func (export "run") (local $pair i64)
                  (local.set $pair (call $new))
                  {operations}))
              (core instance $i (instantiate $m (with "" (instance
                (export "attach" (func $attach)) (export "new" (func $new))
                (export "drop" (func $drop))))))
              (func (export "run") async (canon lift (core func $i "run"))))
        "#
            ),
        )?;
        let observations = Arc::new(Mutex::new(Vec::new()));
        let pending = Arc::new(Mutex::new(None::<StreamReader<u8>>));
        let mut linker = Linker::new(&engine);
        linker.root().func_wrap("attach", {
            let interceptor = Mutex::new(Some(Hook {
                pending: false,
                prefix: false,
                observations: observations.clone(),
                starts: Arc::default(),
            }));
            let pending = pending.clone();
            move |mut store: StoreContextMut<'_, ()>, (mut reader,): (StreamReader<u8>,)| {
                reader.intercept_guest_writes(
                    &mut store,
                    interceptor.lock().unwrap().take().unwrap(),
                )?;
                if close_reader {
                    reader.close(store)?;
                } else {
                    *pending.lock().unwrap() = Some(reader);
                }
                Ok(())
            }
        })?;
        let mut store = Store::new(&engine, ());
        let instance = linker.instantiate_async(&mut store, &component).await?;
        instance
            .get_typed_func::<(), ()>(&mut store, "run")?
            .call_async(&mut store, ())
            .await?;
        let bytes = Arc::new(Mutex::new(Vec::new()));
        if !close_reader {
            pending
                .lock()
                .unwrap()
                .take()
                .unwrap()
                .pipe(&mut store, Consumer(bytes.clone(), false, false))?;
        }
        assert!(bytes.lock().unwrap().is_empty());
        assert!(observations.lock().unwrap().is_empty());
        assert_eq!(Arc::strong_count(&bytes), 1);
        assert_eq!(Arc::strong_count(&observations), 1);
    }
    Ok(())
}

#[test]
fn host_producers_bypass_interception() -> Result<()> {
    struct Producer(Vec<u8>);

    impl StreamProducer<()> for Producer {
        type Item = u8;
        type Buffer = VecBuffer<u8>;

        fn poll_produce<'a>(
            self: Pin<&mut Self>,
            _: &mut Context<'_>,
            _: StoreContextMut<'a, ()>,
            mut destination: Destination<'a, u8, VecBuffer<u8>>,
            _: bool,
        ) -> Poll<Result<StreamResult>> {
            destination.set_buffer(std::mem::take(&mut self.get_mut().0).into());
            Poll::Ready(Ok(StreamResult::Dropped))
        }

        fn try_into(
            me: Pin<Box<Self>>,
            ty: std::any::TypeId,
        ) -> Result<Box<dyn std::any::Any>, Pin<Box<Self>>> {
            if ty == std::any::TypeId::of::<Self>() {
                let producer: Box<Self> = Pin::into_inner(me);
                Ok(producer)
            } else {
                Err(me)
            }
        }
    }

    let engine = Engine::new(Config::new().wasm_component_model_async(true))?;
    let mut store = Store::new(&engine, ());
    let observations = Arc::new(Mutex::new(Vec::new()));
    let hook = || Hook {
        pending: false,
        prefix: false,
        observations: observations.clone(),
        starts: Arc::default(),
    };
    let mut reader = StreamReader::new(&mut store, Producer(vec![19, 7, 3]))?;
    assert!(!reader.is_guest_produced(&mut store)?);
    reader.intercept_guest_writes(&mut store, hook())?;
    assert!(!reader.has_guest_write_interceptor(&mut store)?);
    assert_eq!(
        reader.try_into::<Producer>(&mut store).ok().map(|p| p.0),
        Some(vec![19, 7, 3])
    );

    let mut future = FutureReader::new(&mut store, async { Ok::<_, wasmtime::Error>(17u8) })?;
    assert!(!future.is_guest_produced(&mut store)?);
    future.intercept_guest_writes(&mut store, hook())?;
    assert!(!future.has_guest_write_interceptor(&mut store)?);
    future.close(&mut store)?;
    assert_eq!(Arc::strong_count(&observations), 1);
    Ok(())
}

#[tokio::test]
async fn synchronous_write_observes_immediate_and_scheduled_results() -> Result<()> {
    for is_future in [false, true] {
        for immediate in [false, true] {
            let engine = Engine::new(
                Config::new()
                    .wasm_component_model_async(true)
                    .wasm_component_model_more_async_builtins(true),
            )?;
            let wat = r#"
                    (component
                      (type $s (stream u8))
                      (import "attach" (func $attach (param "s" $s)))
                      (import "observed" (func $observed (param "code" u32)))
                      (core module $mem (memory (export "m") 1) (data (i32.const 0) "abcd"))
                      (core instance $mem (instantiate $mem))
                      (core func $attach (canon lower (func $attach)))
                      (core func $observed (canon lower (func $observed)))
                      (core func $new (canon stream.new $s))
                      (core func $write (canon stream.write $s (memory (core memory $mem "m"))))
                      (core func $drop (canon stream.drop-writable $s))
                      (core module $m
                        (import "" "attach" (func $attach (param i32)))
                        (import "" "observed" (func $observed (param i32)))
                        (import "" "new" (func $new (result i64)))
                        (import "" "write" (func $write (param i32 i32 i32) (result i32)))
                        (import "" "drop" (func $drop (param i32)))
                        (func (export "run")
                          (local $pair i64) (local $writer i32)
                          (local.set $pair (call $new))
                          (local.set $writer (i32.wrap_i64 (i64.shr_u (local.get $pair) (i64.const 32))))
                          (call $attach (i32.wrap_i64 (local.get $pair)))
                          (call $observed (call $write (local.get $writer) (i32.const 0) (i32.const 4)))
                          (call $drop (local.get $writer))))
                      (core instance $i (instantiate $m (with "" (instance
                        (export "attach" (func $attach)) (export "observed" (func $observed))
                        (export "new" (func $new)) (export "write" (func $write)) (export "drop" (func $drop))))))
                      (func (export "run") async (canon lift (core func $i "run"))))
                "#;
            let wat = if is_future {
                wat.replace("(stream u8)", "(future u8)")
                    .replace("canon stream.", "canon future.")
                    .replace(
                        "(func $write (param i32 i32 i32)",
                        "(func $write (param i32 i32)",
                    )
                    .replace(
                        "(call $write (local.get $writer) (i32.const 0) (i32.const 4))",
                        "(call $write (local.get $writer) (i32.const 0))",
                    )
            } else {
                let write = "(call $observed (call $write (local.get $writer) (i32.const 0) (i32.const 4)))";
                assert_eq!(wat.matches(write).count(), 1);
                wat.replace(write, &format!("{write}\n{write}"))
            };
            let component = Component::new(&engine, &wat)?;
            let observations = Arc::new(Mutex::new(Vec::new()));
            let bytes = Arc::new(Mutex::new(Vec::new()));
            let starts = Arc::new(Mutex::new(0));
            let mut linker = Linker::new(&engine);
            if is_future {
                linker.root().func_wrap("attach", {
                    let observations = observations.clone();
                    let bytes = bytes.clone();
                    let starts = starts.clone();
                    move |mut store: StoreContextMut<'_, ()>, (mut reader,): (FutureReader<u8>,)| {
                        reader.intercept_guest_writes(
                            &mut store,
                            Hook {
                                pending: immediate,
                                prefix: false,
                                observations: observations.clone(),
                                starts: starts.clone(),
                            },
                        )?;
                        reader.pipe(store, Consumer(bytes.clone(), false, false))
                    }
                })?;
            } else {
                linker.root().func_wrap("attach", {
                    let observations = observations.clone();
                    let bytes = bytes.clone();
                    let starts = starts.clone();
                    move |mut store: StoreContextMut<'_, ()>, (mut reader,): (StreamReader<u8>,)| {
                        reader.intercept_guest_writes(
                            &mut store,
                            Hook {
                                pending: immediate,
                                prefix: false,
                                observations: observations.clone(),
                                starts: starts.clone(),
                            },
                        )?;
                        reader.pipe(store, Consumer(bytes.clone(), false, false))
                    }
                })?;
            }
            linker.root().func_wrap("observed", {
                let observations = observations.clone();
                let starts = starts.clone();
                move |_: StoreContextMut<'_, ()>, (code,): (u32,)| {
                    assert_eq!(code, if is_future { 0 } else { 16 });
                    assert_eq!(
                        *observations.lock().unwrap(),
                        vec![TerminalConsumption::Delivered; *starts.lock().unwrap()]
                    );
                    Ok(())
                }
            })?;
            let mut store = Store::new(&engine, ());
            let instance = linker.instantiate_async(&mut store, &component).await?;
            let run = instance.get_typed_func::<(), ()>(&mut store, "run")?;
            tokio::time::timeout(
                std::time::Duration::from_secs(10),
                run.call_async(&mut store, ()),
            )
            .await
            .unwrap()?;
            let writes = if is_future { 1 } else { 2 };
            assert_eq!(*starts.lock().unwrap(), writes);
            assert_eq!(*bytes.lock().unwrap(), vec![b'a'; writes]);
        }
    }
    Ok(())
}

struct CancelGate {
    started: bool,
    queued: bool,
    result: Option<StreamResult>,
    accepted: Arc<std::sync::atomic::AtomicBool>,
    observations: Arc<Mutex<Vec<TerminalConsumption>>>,
}

impl GuestWriteInterceptor<(), u8> for CancelGate {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        mut store: StoreContextMut<'_, ()>,
        mut source: Source<'_, u8>,
        finish: bool,
        native: &mut dyn FnMut(
            &mut Context<'_>,
            StoreContextMut<'_, ()>,
            Source<'_, u8>,
            bool,
        ) -> Poll<Result<StreamResult>>,
    ) -> Poll<Result<StreamResult>> {
        let this = self.get_mut();
        if !this.started {
            this.started = true;
            cx.waker().wake_by_ref();
            return Poll::Pending;
        }
        if this.result.is_none() {
            this.result = Some(std::task::ready!(native(
                cx,
                store.as_context_mut(),
                source.reborrow(),
                finish
            ))?);
            this.accepted
                .store(true, std::sync::atomic::Ordering::SeqCst);
        }
        if !this.queued && !finish {
            return Poll::Pending;
        }
        let observations = this.observations.clone();
        source.register_write_observer(store, move |value| {
            observations.lock().unwrap().push(value)
        })?;
        Poll::Ready(Ok(this.result.take().unwrap()))
    }
}

#[tokio::test]
async fn cancellation_preserves_inline_results_for_queued_writes() -> Result<()> {
    for is_future in [false, true] {
        for async_cancel in [false, true] {
            for (queued, callback) in [(false, false), (true, false), (false, true)] {
                let engine = Engine::new(
                    Config::new()
                        .wasm_component_model_async(true)
                        .wasm_component_model_more_async_builtins(true),
                )?;
                let wat = r#"
                      (component
                        (type $s (stream u8))
                        (import "attach" (func $attach (param "s" $s)))
                        (import "turn" (func $turn))
                        (import "observed" (func $observed (param "stage" u32) (param "code" u32)))
                        (core module $mem (memory (export "m") 1) (data (i32.const 0) "abcd"))
                        (core instance $mem (instantiate $mem))
                        (core func $attach (canon lower (func $attach)))
                        (core func $turn (canon lower (func $turn)))
                        (core func $observed (canon lower (func $observed)))
                        (core func $new (canon stream.new $s))
                        (core func $write (canon stream.write $s async (memory (core memory $mem "m"))))
                        (core func $cancel (canon stream.cancel-write $s async))
                        (core func $drop (canon stream.drop-writable $s))
                        (core func $join (canon waitable.join))
                        (core func $set (canon waitable-set.new))
                        (core func $wait (canon waitable-set.wait (memory (core memory $mem "m"))))
                        (core module $m
                          (import "" "mem" (memory 1))
                          (import "" "attach" (func $attach (param i32)))
                          (import "" "turn" (func $turn))
                          (import "" "observed" (func $observed (param i32 i32)))
                          (import "" "new" (func $new (result i64)))
                          (import "" "write" (func $write (param i32 i32 i32) (result i32)))
                          (import "" "cancel" (func $cancel (param i32) (result i32)))
                          (import "" "drop" (func $drop (param i32)))
                          (import "" "join" (func $join (param i32 i32)))
                          (import "" "set" (func $set (result i32)))
                          (import "" "wait" (func $wait (param i32 i32) (result i32)))
                          (func (export "run")
                            (local $pair i64) (local $writer i32) (local $set i32) (local $code i32)
                            (local.set $pair (call $new))
                            (local.set $writer (i32.wrap_i64 (i64.shr_u (local.get $pair) (i64.const 32))))
                            (call $attach (i32.wrap_i64 (local.get $pair)))
                            (if (i32.ne (call $write (local.get $writer) (i32.const 0) (i32.const 4)) (i32.const -1)) (then unreachable))
                            (call $turn)
                            (local.set $code (call $cancel (local.get $writer)))
                            (call $observed (i32.const 0) (local.get $code))
                            (if (i32.eq (local.get $code) (i32.const -1)) (then
                              (local.set $set (call $set))
                              (call $join (local.get $writer) (local.get $set))
                              (drop (call $wait (local.get $set) (i32.const 64)))
                              (local.set $code (i32.load (i32.const 68)))
                              (call $join (local.get $writer) (i32.const 0))))
                            (call $observed (i32.const 1) (local.get $code))
                            (call $drop (local.get $writer))))
                        (core instance $i (instantiate $m (with "" (instance
                          (export "mem" (memory $mem "m"))
                          (export "attach" (func $attach)) (export "turn" (func $turn))
                          (export "observed" (func $observed)) (export "new" (func $new))
                          (export "write" (func $write)) (export "cancel" (func $cancel))
                          (export "drop" (func $drop)) (export "join" (func $join))
                          (export "set" (func $set)) (export "wait" (func $wait))))))
                        (func (export "run") async (canon lift (core func $i "run"))))
                    "#;
                let mut wat = if is_future {
                    wat.replace("(stream u8)", "(future u8)")
                        .replace("canon stream.", "canon future.")
                        .replace(
                            "(func $write (param i32 i32 i32)",
                            "(func $write (param i32 i32)",
                        )
                        .replace(
                            "(call $write (local.get $writer) (i32.const 0) (i32.const 4))",
                            "(call $write (local.get $writer) (i32.const 0))",
                        )
                } else {
                    wat.to_owned()
                };
                if !async_cancel {
                    wat = wat.replace("cancel-write $s async", "cancel-write $s");
                }
                if callback {
                    wat = wat
                        .replace("(call $turn)", "")
                        .replace("(core func $join (canon waitable.join))", "(core func $return (canon task.return)) (core func $join (canon waitable.join))")
                        .replace("(import \"\" \"join\" (func $join", "(import \"\" \"return\" (func $return)) (import \"\" \"join\" (func $join")
                        .replace("(export \"set\" (func $set)) (export \"wait\"", "(export \"return\" (func $return)) (export \"set\" (func $set)) (export \"wait\"")
                        .replace("(func (export \"run\")\n", "(func (export \"run\") (result i32)\n")
                        .replace("(call $drop (local.get $writer))))", "(call $drop (local.get $writer)) (call $return) (i32.const 0)) (func (export \"cb\") (param i32 i32 i32) (result i32) (i32.const 0)))")
                        .replace("(canon lift (core func $i \"run\"))", "(canon lift (core func $i \"run\") async (callback (core func $i \"cb\")))");
                }
                let component = Component::new(&engine, &wat)?;
                let observations = Arc::new(Mutex::new(Vec::new()));
                let bytes = Arc::new(Mutex::new(Vec::new()));
                let accepted = Arc::new(std::sync::atomic::AtomicBool::new(false));
                let mut linker = Linker::new(&engine);
                if is_future {
                    linker.root().func_wrap("attach", {
                        let observations = observations.clone();
                        let bytes = bytes.clone();
                        let accepted = accepted.clone();
                        move |mut store: StoreContextMut<'_, ()>,
                              (mut reader,): (FutureReader<u8>,)| {
                            reader.intercept_guest_writes(
                                &mut store,
                                CancelGate {
                                    started: false,
                                    queued,
                                    result: None,
                                    accepted: accepted.clone(),
                                    observations: observations.clone(),
                                },
                            )?;
                            reader.pipe(store, Consumer(bytes.clone(), false, false))
                        }
                    })?;
                } else {
                    linker.root().func_wrap("attach", {
                        let observations = observations.clone();
                        let bytes = bytes.clone();
                        let accepted = accepted.clone();
                        move |mut store: StoreContextMut<'_, ()>,
                              (mut reader,): (StreamReader<u8>,)| {
                            reader.intercept_guest_writes(
                                &mut store,
                                CancelGate {
                                    started: false,
                                    queued,
                                    result: None,
                                    accepted: accepted.clone(),
                                    observations: observations.clone(),
                                },
                            )?;
                            reader.pipe(store, Consumer(bytes.clone(), false, false))
                        }
                    })?;
                }
                linker.root().func_wrap_concurrent("turn", move |_, ()| {
                    let accepted = accepted.clone();
                    Box::pin(async move {
                        while !accepted.load(std::sync::atomic::Ordering::SeqCst) {
                            tokio::task::yield_now().await;
                        }
                        for _ in 0..5 {
                            tokio::task::yield_now().await;
                        }
                        Ok(())
                    })
                })?;
                linker.root().func_wrap("observed", {
                    let observations = observations.clone();
                    move |_: StoreContextMut<'_, ()>, (stage, code): (u32, u32)| {
                        if stage == 0 && async_cancel && !queued {
                            assert_eq!(
                                code,
                                u32::MAX,
                                "a gated acknowledgement is still pending; future={is_future}"
                            );
                            assert!(observations.lock().unwrap().is_empty());
                        } else {
                            assert_eq!(
                                code,
                                if is_future { 0 } else { 18 },
                                "future={is_future}, queued={queued}, async={async_cancel}"
                            );
                            assert_eq!(
                                *observations.lock().unwrap(),
                                vec![TerminalConsumption::Delivered]
                            );
                        }
                        Ok(())
                    }
                })?;
                let mut store = Store::new(&engine, ());
                let instance = linker.instantiate_async(&mut store, &component).await?;
                let run = instance.get_typed_func::<(), ()>(&mut store, "run")?;
                tokio::time::timeout(
                    std::time::Duration::from_secs(10),
                    run.call_async(&mut store, ()),
                )
                .await
                .unwrap()?;
                assert_eq!(*bytes.lock().unwrap(), vec![b'a']);
            }
        }
    }
    Ok(())
}
