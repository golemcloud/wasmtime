use crate::IoData;
use crate::bindings::wasi::io::{error, poll, streams};
use crate::poll::{DynFuture, DynPollable, MakeFuture, subscribe};
use crate::streams::{DynInputStream, DynOutputStream, StreamError, StreamResult};
use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;
use bytes::Bytes;
use core::future::Future;
use core::pin::Pin;
use core::task::{Context, Poll};
use std::time::Instant;
use wasmtime::component::{Resource, ResourceTable};
use wasmtime::{Result, format_err};

const MAX_POLLABLE_OVERRIDE_CHAIN: usize = 64;

fn get_pollable_following_overrides<'a>(
    table: &'a ResourceTable,
    pollable: &Resource<DynPollable>,
) -> Result<&'a DynPollable> {
    let mut pollable = table.get(pollable)?;
    for _ in 0..MAX_POLLABLE_OVERRIDE_CHAIN {
        if let Some(override_self) = &pollable.override_self {
            let entry = table.get_any(pollable.index)?;
            let pollable_override = override_self(entry);
            if let Some(overridden_idx) = pollable_override {
                pollable = table
                    .get_any(overridden_idx)?
                    .downcast_ref()
                    .ok_or_else(|| format_err!("Pollable override does not point to a Pollable"))?;
            } else {
                return Ok(pollable);
            }
        } else {
            return Ok(pollable);
        }
    }
    Err(format_err!(
        "Pollable override chain exceeded maximum depth of {MAX_POLLABLE_OVERRIDE_CHAIN}"
    ))
}

impl poll::Host for IoData<'_> {
    async fn poll(&mut self, pollables: Vec<Resource<DynPollable>>) -> Result<Vec<u32>> {
        type ReadylistIndex = u32;

        if pollables.is_empty() {
            return Err(format_err!("empty poll list"));
        }

        let mut table_futures: BTreeMap<u32, (MakeFuture, Vec<ReadylistIndex>)> = BTreeMap::new();
        let mut all_supports_suspend = Some(None);
        let mut yield_on_immediate_return = false;

        for (ix, p) in pollables.iter().enumerate() {
            let ix: u32 = ix.try_into()?;

            let pollable = get_pollable_following_overrides(self.table, p)?;

            let (_, list) = table_futures
                .entry(pollable.index)
                .or_insert((pollable.make_future, Vec::new()));
            list.push(ix);

            if pollable.yield_on_immediate_return {
                yield_on_immediate_return = true;
            }

            match pollable.supports_suspend {
                None => {
                    all_supports_suspend = None;
                }
                Some(maximum_suspend_time) => {
                    all_supports_suspend = all_supports_suspend.map(|maybe_max| match maybe_max {
                        None => Some(maximum_suspend_time),
                        Some(max) => Some(std::cmp::min(max, maximum_suspend_time)),
                    });
                }
            }
        }

        if let Some(Some(deadline)) = all_supports_suspend {
            let duration = deadline.duration_since(Instant::now());
            if duration >= self.io_ctx.suspend_threshold {
                return Err((self.io_ctx.suspend_signal)(duration));
            }
        }

        let mut futures: Vec<(Option<DynFuture<'_>>, Vec<ReadylistIndex>)> = Vec::new();
        for (entry, (make_future, readylist_indices)) in self.table.iter_entries(table_futures) {
            let entry = entry?;
            futures.push((Some(make_future(entry)), readylist_indices));
        }

        struct PollList<'a> {
            /// Futures that have already resolved to `Ready` are replaced with
            /// `None` so we don't poll a completed `async fn` again (which
            /// would panic with `async fn resumed after completion`). The
            /// readylist indices of completed futures are still reported in
            /// the output.
            futures: Vec<(Option<DynFuture<'a>>, Vec<ReadylistIndex>)>,
            /// Indices accumulated from futures that have already resolved.
            ready_indices: Vec<u32>,
            /// If `true` and the futures all resolve on the very first poll,
            /// the host injects a single cooperative yield to the async
            /// runtime before returning, so other tasks (e.g. `mio`-driven
            /// sockets in the host runtime) get a chance to make progress.
            ///
            /// See `wasmtime-wasi-io::poll::DynPollable::yield_on_immediate_return`
            /// and upstream wasmtime issue #13040 for context.
            yield_on_immediate_return: bool,
            /// Tracks whether we've already yielded to the async runtime at
            /// least once during this `poll` call. Returning `Pending`
            /// naturally also counts, so we never inject more than one yield.
            fairness_yielded: bool,
        }
        impl<'a> Future for PollList<'a> {
            type Output = Vec<u32>;

            fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
                let mut newly_ready: Vec<u32> = Vec::new();
                for (fut_slot, readylist_indices) in self.futures.iter_mut() {
                    let Some(fut) = fut_slot.as_mut() else {
                        continue;
                    };
                    match fut.as_mut().poll(cx) {
                        Poll::Ready(()) => {
                            // Drop the future so we never poll it again on a
                            // subsequent re-poll (Rust panics if an already
                            // completed `async fn` future is polled again).
                            newly_ready.extend_from_slice(readylist_indices);
                            *fut_slot = None;
                        }
                        Poll::Pending => {}
                    }
                }
                let any_newly_ready = !newly_ready.is_empty();
                self.ready_indices.append(&mut newly_ready);
                let any_ready = !self.ready_indices.is_empty();
                if any_ready {
                    if self.yield_on_immediate_return
                        && !self.fairness_yielded
                        && any_newly_ready
                    {
                        // Force a single yield to the async runtime, then
                        // re-poll on the next wake to allow any other futures
                        // that may become ready in the meantime to be
                        // collected before returning to the guest.
                        self.fairness_yielded = true;
                        cx.waker().wake_by_ref();
                        Poll::Pending
                    } else {
                        let results = std::mem::take(&mut self.ready_indices);
                        Poll::Ready(results)
                    }
                } else {
                    // Returning `Pending` naturally yields to the runtime, so
                    // mark fairness as satisfied to avoid an additional
                    // injected yield on subsequent polls.
                    self.fairness_yielded = true;
                    Poll::Pending
                }
            }
        }

        Ok(PollList {
            futures,
            ready_indices: Vec::new(),
            yield_on_immediate_return,
            fairness_yielded: false,
        }
        .await)
    }
}

impl crate::bindings::wasi::io::poll::HostPollable for IoData<'_> {
    async fn block(&mut self, pollable: Resource<DynPollable>) -> Result<()> {
        // `block` is defined as equivalent to `poll([self])`, so delegate to
        // `poll::Host::poll` to share the cooperative-yield handling for
        // `yield_on_immediate_return` pollables (see upstream wasmtime
        // issue #13040).
        let _ = <Self as poll::Host>::poll(self, vec![pollable]).await?;
        Ok(())
    }
    async fn ready(&mut self, pollable: Resource<DynPollable>) -> Result<bool> {
        let pollable = get_pollable_following_overrides(self.table, &pollable)?;
        let ready = (pollable.make_future)(self.table.get_any_mut(pollable.index)?);
        futures::pin_mut!(ready);
        Ok(matches!(
            futures::future::poll_immediate(ready).await,
            Some(())
        ))
    }
    fn drop(&mut self, pollable: Resource<DynPollable>) -> Result<()> {
        let pollable = self.table.delete(pollable)?;
        if let Some(delete) = pollable.remove_index_on_delete {
            delete(self.table, pollable.index)?;
        }
        Ok(())
    }
}

impl error::Host for ResourceTable {}

impl streams::Host for ResourceTable {
    fn convert_stream_error(&mut self, err: StreamError) -> Result<streams::StreamError> {
        match err {
            StreamError::Closed => Ok(streams::StreamError::Closed),
            StreamError::LastOperationFailed(e) => {
                Ok(streams::StreamError::LastOperationFailed(self.push(e)?))
            }
            StreamError::Trap(e) => Err(e),
        }
    }
}

impl error::HostError for ResourceTable {
    fn drop(&mut self, err: Resource<streams::Error>) -> Result<()> {
        self.delete(err)?;
        Ok(())
    }

    fn to_debug_string(&mut self, err: Resource<streams::Error>) -> Result<String> {
        Ok(alloc::format!("{:?}", self.get(&err)?))
    }
}

impl streams::HostOutputStream for ResourceTable {
    async fn drop(&mut self, stream: Resource<DynOutputStream>) -> Result<()> {
        self.delete(stream)?.cancel().await;
        Ok(())
    }

    async fn check_write(&mut self, stream: Resource<DynOutputStream>) -> StreamResult<u64> {
        let bytes = self.get_mut(&stream)?.check_write()?;
        Ok(bytes as u64)
    }

    async fn write(&mut self, stream: Resource<DynOutputStream>, bytes: Vec<u8>) -> StreamResult<()> {
        self.get_mut(&stream)?.write(bytes.into())?;
        Ok(())
    }

    fn subscribe(&mut self, stream: Resource<DynOutputStream>) -> Result<Resource<DynPollable>> {
        subscribe(self, stream, None)
    }

    async fn blocking_write_and_flush(
        &mut self,
        stream: Resource<DynOutputStream>,
        bytes: Vec<u8>,
    ) -> StreamResult<()> {
        if bytes.len() > 4096 {
            return Err(StreamError::trap(
                "Buffer too large for blocking-write-and-flush (expected at most 4096)",
            ));
        }

        self.get_mut(&stream)?
            .blocking_write_and_flush(bytes.into())
            .await
    }

    async fn blocking_write_zeroes_and_flush(
        &mut self,
        stream: Resource<DynOutputStream>,
        len: u64,
    ) -> StreamResult<()> {
        if len > 4096 {
            return Err(StreamError::trap(
                "Buffer too large for blocking-write-zeroes-and-flush (expected at most 4096)",
            ));
        }

        // TODO: We could optimize this to not allocate one big zeroed buffer, and instead write
        // repeatedly from a 'static buffer of zeros.
        let bs = Bytes::from_iter(core::iter::repeat(0).take(len as usize));
        self.get_mut(&stream)?.blocking_write_and_flush(bs).await
    }

    async fn write_zeroes(&mut self, stream: Resource<DynOutputStream>, len: u64) -> StreamResult<()> {
        self.get_mut(&stream)?.write_zeroes(len as usize)?;
        Ok(())
    }

    async fn flush(&mut self, stream: Resource<DynOutputStream>) -> StreamResult<()> {
        self.get_mut(&stream)?.flush()?;
        Ok(())
    }

    async fn blocking_flush(&mut self, stream: Resource<DynOutputStream>) -> StreamResult<()> {
        let s = self.get_mut(&stream)?;
        s.flush()?;
        s.write_ready().await?;
        Ok(())
    }

    async fn splice(
        &mut self,
        dest: Resource<DynOutputStream>,
        src: Resource<DynInputStream>,
        len: u64,
    ) -> StreamResult<u64> {
        let len = len.try_into().unwrap_or(usize::MAX);

        let permit = {
            let output = self.get_mut(&dest)?;
            output.check_write()?
        };
        let len = len.min(permit);
        if len == 0 {
            return Ok(0);
        }

        let contents = self.get_mut(&src)?.read(len)?;

        let len = contents.len();
        if len == 0 {
            return Ok(0);
        }

        let output = self.get_mut(&dest)?;
        output.write(contents)?;
        Ok(len.try_into().expect("usize can fit in u64"))
    }

    async fn blocking_splice(
        &mut self,
        dest: Resource<DynOutputStream>,
        src: Resource<DynInputStream>,
        len: u64,
    ) -> StreamResult<u64> {
        let len = len.try_into().unwrap_or(usize::MAX);

        let permit = {
            let output = self.get_mut(&dest)?;
            output.write_ready().await?
        };
        let len = len.min(permit);
        if len == 0 {
            return Ok(0);
        }

        let contents = self.get_mut(&src)?.blocking_read(len).await?;

        let len = contents.len();
        if len == 0 {
            return Ok(0);
        }

        let output = self.get_mut(&dest)?;
        output.blocking_write_and_flush(contents).await?;
        Ok(len.try_into().expect("usize can fit in u64"))
    }

}

impl streams::HostInputStream for ResourceTable {
    async fn drop(&mut self, stream: Resource<DynInputStream>) -> Result<()> {
        self.delete(stream)?.cancel().await;
        Ok(())
    }

    async fn read(&mut self, stream: Resource<DynInputStream>, len: u64) -> StreamResult<Vec<u8>> {
        let len = len.try_into().unwrap_or(usize::MAX);
        let bytes = self.get_mut(&stream)?.read(len)?;
        debug_assert!(bytes.len() <= len);
        Ok(bytes.into())
    }

    async fn blocking_read(
        &mut self,
        stream: Resource<DynInputStream>,
        len: u64,
    ) -> StreamResult<Vec<u8>> {
        let len = len.try_into().unwrap_or(usize::MAX);
        let bytes = self.get_mut(&stream)?.blocking_read(len).await?;
        debug_assert!(bytes.len() <= len);
        Ok(bytes.into())
    }

    async fn skip(&mut self, stream: Resource<DynInputStream>, len: u64) -> StreamResult<u64> {
        let len = len.try_into().unwrap_or(usize::MAX);
        let written = self.get_mut(&stream)?.skip(len)?;
        Ok(written.try_into().expect("usize always fits in u64"))
    }

    async fn blocking_skip(
        &mut self,
        stream: Resource<DynInputStream>,
        len: u64,
    ) -> StreamResult<u64> {
        let len = len.try_into().unwrap_or(usize::MAX);
        let written = self.get_mut(&stream)?.blocking_skip(len).await?;
        Ok(written.try_into().expect("usize always fits in u64"))
    }

    fn subscribe(&mut self, stream: Resource<DynInputStream>) -> Result<Resource<DynPollable>> {
        crate::poll::subscribe(self, stream, None)
    }
}
