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

        for (ix, p) in pollables.iter().enumerate() {
            let ix: u32 = ix.try_into()?;

            let pollable = get_pollable_following_overrides(self.table, p)?;

            let (_, list) = table_futures
                .entry(pollable.index)
                .or_insert((pollable.make_future, Vec::new()));
            list.push(ix);

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

        let mut futures: Vec<(DynFuture<'_>, Vec<ReadylistIndex>)> = Vec::new();
        for (entry, (make_future, readylist_indices)) in self.table.iter_entries(table_futures) {
            let entry = entry?;
            futures.push((make_future(entry), readylist_indices));
        }

        struct PollList<'a> {
            futures: Vec<(DynFuture<'a>, Vec<ReadylistIndex>)>,
        }
        impl<'a> Future for PollList<'a> {
            type Output = Vec<u32>;

            fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
                let mut any_ready = false;
                let mut results = Vec::new();
                for (fut, readylist_indices) in self.futures.iter_mut() {
                    match fut.as_mut().poll(cx) {
                        Poll::Ready(()) => {
                            results.extend_from_slice(readylist_indices);
                            any_ready = true;
                        }
                        Poll::Pending => {}
                    }
                }
                if any_ready {
                    Poll::Ready(results)
                } else {
                    Poll::Pending
                }
            }
        }

        Ok(PollList { futures }.await)
    }
}

impl crate::bindings::wasi::io::poll::HostPollable for IoData<'_> {
    async fn block(&mut self, pollable: Resource<DynPollable>) -> Result<()> {
        let pollable = get_pollable_following_overrides(self.table, &pollable)?;
        let ready = (pollable.make_future)(self.table.get_any_mut(pollable.index)?);
        ready.await;
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
