use crate::bindings::wasi::io::poll::{Host, HostPollable};
use crate::poll::{DynamicPollable, Pollable, dynamic_subscribe, subscribe};
use crate::{IoCtx, IoData};
use alloc::boxed::Box;
use core::future::Future;
use core::pin::pin;
use core::task::{Context, Poll};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use wasmtime::component::{Resource, ResourceTable};

struct Ready;

#[async_trait::async_trait]
impl Pollable for Ready {
    async fn ready(&mut self) {}
}

struct Redirect(u32);

#[async_trait::async_trait]
impl Pollable for Redirect {
    async fn ready(&mut self) {
        core::future::pending().await
    }
}

#[async_trait::async_trait]
impl DynamicPollable for Redirect {
    fn override_index(&self) -> Option<u32> {
        Some(self.0)
    }
}

fn io_ctx() -> IoCtx {
    IoCtx {
        suspend_threshold: Duration::MAX,
        suspend_signal: Box::new(|_| panic!("unexpected suspension")),
    }
}

#[test]
fn ready_probe_and_poll_preserve_duplicates_and_yield_once() {
    let mut table = ResourceTable::new();
    let source = table.push(Ready).unwrap();
    let pollable = subscribe(&mut table, source, None).unwrap();
    table
        .get_mut(&pollable)
        .unwrap()
        .set_yield_on_immediate_return(true);
    let mut ctx = io_ctx();
    let mut io = IoData {
        table: &mut table,
        io_ctx: &mut ctx,
    };
    let mut cx = Context::from_waker(core::task::Waker::noop());
    for _ in 0..2 {
        let mut ready = pin!(io.ready(Resource::new_borrow(pollable.rep())));
        assert!(matches!(
            ready.as_mut().poll(&mut cx),
            Poll::Ready(Ok(true))
        ));
    }
    for _ in 0..2 {
        let mut poll = pin!(io.poll(vec![
            Resource::new_borrow(pollable.rep()),
            Resource::new_borrow(pollable.rep()),
        ]));
        assert!(poll.as_mut().poll(&mut cx).is_pending());
        match poll.as_mut().poll(&mut cx) {
            Poll::Ready(Ok(indices)) => assert_eq!(indices, vec![0, 1]),
            other => panic!("expected readiness after one yield, got {other:?}"),
        }
    }
    let mut block = pin!(io.block(Resource::new_borrow(pollable.rep())));
    assert!(block.as_mut().poll(&mut cx).is_pending());
    assert!(matches!(block.as_mut().poll(&mut cx), Poll::Ready(Ok(()))));
}

#[test]
fn suspension_requires_every_pollable_and_uses_earliest_deadline() {
    let mut table = ResourceTable::new();
    let now = Instant::now();
    let a = table.push(Ready).unwrap();
    let a = subscribe(&mut table, a, Some(now + Duration::from_secs(120))).unwrap();
    let b = table.push(Ready).unwrap();
    let b = subscribe(&mut table, b, Some(now + Duration::from_secs(60))).unwrap();
    let c = table.push(Ready).unwrap();
    let c = subscribe(&mut table, c, None).unwrap();
    let observed = Arc::new(Mutex::new(None));
    let captured = observed.clone();
    let mut ctx = IoCtx {
        suspend_threshold: Duration::from_secs(1),
        suspend_signal: Box::new(move |duration| {
            *captured.lock().unwrap() = Some(duration);
            wasmtime::format_err!("suspended")
        }),
    };
    let mut io = IoData {
        table: &mut table,
        io_ctx: &mut ctx,
    };
    let mut cx = Context::from_waker(core::task::Waker::noop());
    {
        let mut poll = pin!(io.poll(vec![
            Resource::new_borrow(a.rep()),
            Resource::new_borrow(b.rep())
        ]));
        assert!(matches!(poll.as_mut().poll(&mut cx), Poll::Ready(Err(_))));
    }
    let duration = observed.lock().unwrap().take().unwrap();
    assert!(duration <= Duration::from_secs(60));
    assert!(duration > Duration::from_secs(1));
    let mut poll = pin!(io.poll(vec![
        Resource::new_borrow(a.rep()),
        Resource::new_borrow(c.rep())
    ]));
    assert!(matches!(poll.as_mut().poll(&mut cx), Poll::Ready(Ok(_))));
    assert!(observed.lock().unwrap().is_none());
}

#[test]
fn dynamic_pollables_follow_target_readiness_and_reject_cycles() {
    let mut table = ResourceTable::new();
    let source = table.push(Ready).unwrap();
    let target = subscribe(&mut table, source, None).unwrap();
    let redirect = table.push(Redirect(target.rep())).unwrap();
    let redirect_index = redirect.rep();
    let dynamic = dynamic_subscribe(&mut table, redirect, None).unwrap();
    let mut ctx = io_ctx();
    let mut cx = Context::from_waker(core::task::Waker::noop());
    {
        let mut io = IoData {
            table: &mut table,
            io_ctx: &mut ctx,
        };
        let mut ready = pin!(io.ready(Resource::new_borrow(dynamic.rep())));
        assert!(matches!(
            ready.as_mut().poll(&mut cx),
            Poll::Ready(Ok(true))
        ));
    }
    table
        .get_mut(&Resource::<Redirect>::new_borrow(redirect_index))
        .unwrap()
        .0 = dynamic.rep();
    let mut io = IoData {
        table: &mut table,
        io_ctx: &mut ctx,
    };
    let mut ready = pin!(io.ready(Resource::new_borrow(dynamic.rep())));
    assert!(matches!(ready.as_mut().poll(&mut cx), Poll::Ready(Err(_))));
}
