use alloc::boxed::Box;
use anyhow::Result;
use core::any::Any;
use core::future::Future;
use core::pin::Pin;
use std::time::Instant;
use wasmtime::component::{Resource, ResourceTable};

pub type DynFuture<'a> = Pin<Box<dyn Future<Output = ()> + Send + 'a>>;
pub type MakeFuture = for<'a> fn(&'a mut dyn Any) -> DynFuture<'a>;
pub type OverrideSelf = fn(&dyn Any) -> Option<u32>;

/// The host representation of the `wasi:io/poll.pollable` resource.
///
/// A pollable is not the same thing as a Rust Future: the same pollable may be used to
/// repeatedly check for readiness of a given condition, e.g. if a stream is readable
/// or writable. So, rather than containing a Future, which can only become Ready once, a
/// `DynPollable` contains a way to create a Future in each call to `poll`.
pub struct DynPollable {
    pub(crate) index: u32,
    pub(crate) override_self: Option<OverrideSelf>,
    pub(crate) make_future: MakeFuture,
    pub(crate) remove_index_on_delete: Option<fn(&mut ResourceTable, u32) -> Result<()>>,
    pub(crate) supports_suspend: Option<Instant>,
}

/// The trait used to implement [`DynPollable`] to create a `pollable`
/// resource in `wasi:io/poll`.
///
/// This trait is the internal implementation detail of any pollable resource in
/// this crate's implementation of WASI. The `ready` function is an `async fn`
/// which resolves when the implementation is ready. Using native `async` Rust
/// enables this type's readiness to compose with other types' readiness
/// throughout the WASI implementation.
///
/// This trait is used in conjunction with [`subscribe`] to create a `pollable`
/// resource.
///
/// # Example
///
/// This is a simple example of creating a `Pollable` resource from a few
/// parameters.
///
/// ```
/// # // stub out so we don't need a dep to build the doctests:
/// # mod tokio { pub mod time { pub use std::time::{Duration, Instant}; pub async fn sleep_until(_:
/// Instant) {} } }
/// use tokio::time::{self, Duration, Instant};
/// use wasmtime_wasi_io::{IoView, poll::{Pollable, subscribe, DynPollable}, async_trait};
/// use wasmtime::component::Resource;
/// use wasmtime::Result;
///
/// fn sleep(cx: &mut dyn IoView, dur: Duration) -> Result<Resource<DynPollable>> {
///     let end = Instant::now() + dur;
///     let sleep = MySleep { end };
///     let sleep_resource = cx.table().push(sleep)?;
///     subscribe(cx.table(), sleep_resource, None)
/// }
///
/// struct MySleep {
///     end: Instant,
/// }
///
/// #[async_trait]
/// impl Pollable for MySleep {
///     async fn ready(&mut self) {
///         tokio::time::sleep_until(self.end).await;
///     }
/// }
/// ```
#[async_trait::async_trait]
pub trait Pollable: Send + 'static {
    /// An asynchronous function which resolves when this object's readiness
    /// operation is ready.
    ///
    /// This function is invoked as part of `poll` in `wasi:io/poll`. The
    /// meaning of when this function Returns depends on what object this
    /// [`Pollable`] is attached to. When the returned future resolves then the
    /// corresponding call to `wasi:io/poll` will return.
    ///
    /// Note that this method does not return an error. Returning an error
    /// should be done through accessors on the object that this `pollable` is
    /// connected to. The call to `wasi:io/poll` itself does not return errors,
    /// only a list of ready objects.
    async fn ready(&mut self);
}

/// Creates a `wasi:io/poll/pollable` resource which is subscribed to the provided
/// `resource`.
///
/// If `resource` is an owned resource then it will be deleted when the returned
/// resource is deleted. Otherwise the returned resource is considered a "child"
/// of the given `resource` which means that the given resource cannot be
/// deleted while the `pollable` is still alive.
pub fn subscribe<T>(
    table: &mut ResourceTable,
    resource: Resource<T>,
    supports_suspend: Option<Instant>,
) -> Result<Resource<DynPollable>>
where
    T: Pollable,
{
    fn make_future<'a, T>(stream: &'a mut dyn Any) -> DynFuture<'a>
    where
        T: Pollable,
    {
        stream.downcast_mut::<T>().unwrap().ready()
    }

    let pollable = DynPollable {
        index: resource.rep(),
        override_self: None,
        remove_index_on_delete: if resource.owned() {
            Some(|table, idx| {
                let resource = Resource::<T>::new_own(idx);
                table.delete(resource)?;
                Ok(())
            })
        } else {
            None
        },
        make_future: make_future::<T>,
        supports_suspend
    };

    Ok(table.push_child(pollable, &resource)?)
}

/// An advanced version of Pollable supporting dynamically switching the underlying table entry
///
/// This can be used to implement "lazy initialized" pollables.
#[async_trait::async_trait]
pub trait DynamicPollable: Pollable {
    /// Returns the table index to another `Pollable` entry in case the override should happen
    fn override_index(&self) -> Option<u32>;
}

/// Creates a `pollable` resource from a `DynamicSubscribe` implementation
pub fn dynamic_subscribe<T>(
    table: &mut ResourceTable,
    resource: Resource<T>,
    supports_suspend: Option<Instant>,
) -> Result<Resource<DynPollable>>
where
    T: DynamicPollable,
{
    fn make_future<'a, T>(stream: &'a mut dyn Any) -> DynFuture<'a>
    where
        T: DynamicPollable,
    {
        stream.downcast_mut::<T>().unwrap().ready()
    }

    fn override_self<'a, T>(entry: &'a dyn Any) -> Option<u32>
    where
        T: DynamicPollable,
    {
        let entry = entry.downcast_ref::<T>().unwrap();
        entry.override_index()
    }

    let pollable = DynPollable {
        index: resource.rep(),
        override_self: Some(override_self::<T>),
        remove_index_on_delete: if resource.owned() {
            Some(|table, idx| {
                let resource = Resource::<T>::new_own(idx);
                table.delete(resource)?;
                Ok(())
            })
        } else {
            None
        },
        make_future: make_future::<T>,
        supports_suspend,
    };

    Ok(table.push_child(pollable, &resource)?)
}