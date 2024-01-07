use crate::runtime::in_tokio;
use wasmtime_wasi_io::{bindings::wasi::io::poll as async_poll, poll::DynPollable, IoImpl, IoView};

use anyhow::Result;
use wasmtime::component::Resource;

impl<T> crate::bindings::sync::io::poll::Host for IoImpl<T>
where
    T: IoView,
{
    fn poll(&mut self, pollables: Vec<Resource<DynPollable>>) -> Result<Vec<u32>> {
        in_tokio(async { async_poll::Host::poll(self, pollables).await })
    }
}

impl<T> crate::bindings::sync::io::poll::HostPollable for IoImpl<T>
where
    T: IoView,
{
    fn ready(&mut self, pollable: Resource<DynPollable>) -> Result<bool> {
        in_tokio(async { async_poll::HostPollable::ready(self, pollable).await })
    }
    fn block(&mut self, pollable: Resource<DynPollable>) -> Result<()> {
        in_tokio(async { async_poll::HostPollable::block(self, pollable).await })
    }
    fn drop(&mut self, pollable: Resource<DynPollable>) -> Result<()> {
        async_poll::HostPollable::drop(self, pollable)
    }
}

// pub struct Pollable {
//     index: u32,
//     make_future: MakeFuture,
//     remove_index_on_delete: Option<fn(&mut ResourceTable, u32) -> Result<()>>,
//     pub supports_suspend: Option<Instant>,
// }

// pub fn subscribe<T>(
//     table: &mut ResourceTable,
//     resource: Resource<T>,
//     supports_suspend: Option<Instant>,
// ) -> Result<Resource<Pollable>>
//     where
//         T: Subscribe,
// {
//     fn make_future<'a, T>(stream: &'a mut dyn Any) -> PollableFuture<'a>
//         where
//             T: Subscribe,
//     {
//         stream.downcast_mut::<T>().unwrap().ready()
//     }
//
//     let pollable = Pollable {
//         index: resource.rep(),
//         remove_index_on_delete: if resource.owned() {
//             Some(|table, idx| {
//                 let resource = Resource::<T>::new_own(idx);
//                 table.delete(resource)?;
//                 Ok(())
//             })
//         } else {
//             None
//         },
//         make_future: make_future::<T>,
//         supports_suspend,
//     };
//
//     Ok(table.push_child(pollable, &resource)?)
// }

// #[async_trait::async_trait]
// impl<T> poll::Host for WasiImpl<T>
// where
//     T: WasiView,
// {
//     async fn poll(&mut self, pollables: Vec<Resource<Pollable>>) -> Result<Vec<u32>> {
//         type ReadylistIndex = u32;
//
//         if pollables.is_empty() {
//             return Err(anyhow!("empty poll list"));
//         }
//
//         let mut table_futures: HashMap<u32, (MakeFuture, Vec<ReadylistIndex>)> = HashMap::new();
//         let mut all_supports_suspend = Some(None);
//
//         for (ix, p) in pollables.iter().enumerate() {
//             let ix: u32 = ix.try_into()?;
//
//             let pollable = self.table().get(p)?;
//             let (_, list) = table_futures
//                 .entry(pollable.index)
//                 .or_insert((pollable.make_future, Vec::new()));
//             list.push(ix);
//
//             match pollable.supports_suspend {
//                 None => {
//                     all_supports_suspend = None;
//                 }
//                 Some(maximum_suspend_time) => {
//                     all_supports_suspend = all_supports_suspend.map(|maybe_max| match maybe_max {
//                         None => Some(maximum_suspend_time),
//                         Some(max) => Some(std::cmp::min(max, maximum_suspend_time)),
//                     });
//                 }
//             }
//         }
//
//         if let Some(Some(deadline)) = all_supports_suspend {
//             let duration = deadline.duration_since(Instant::now());
//             if duration >= self.ctx().suspend_threshold {
//                 return Err((self.ctx().suspend_signal)(duration));
//             }
//         }
//         let mut futures: Vec<(PollableFuture<'_>, Vec<ReadylistIndex>)> = Vec::new();
//         for (entry, (make_future, readylist_indices)) in
//         self.table().iter_entries(table_futures)
//         {
//             let entry = entry?;
//             futures.push((make_future(entry), readylist_indices));
//         }
//
//         struct PollList<'a> {
//             futures: Vec<(PollableFuture<'a>, Vec<ReadylistIndex>)>,
//         }
//         impl<'a> Future for PollList<'a> {
//             type Output = Vec<u32>;
//
//             fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
//                 let mut any_ready = false;
//                 let mut results = Vec::new();
//                 for (fut, readylist_indicies) in self.futures.iter_mut() {
//                     match fut.as_mut().poll(cx) {
//                         Poll::Ready(()) => {
//                             results.extend_from_slice(readylist_indicies);
//                             any_ready = true;
//                         }
//                         Poll::Pending => {}
//                     }
//                 }
//                 if any_ready {
//                     Poll::Ready(results)
//                 } else {
//                     Poll::Pending
//                 }
//             }
//         }
//
//         Ok(PollList { futures }.await)
//     }
// }