use crate::p2::SocketError;
use crate::p2::bindings::sockets::ip_name_lookup::{Host, HostResolveAddressStream};
use crate::p2::bindings::sockets::network::{ErrorCode, IpAddress, Network};
use crate::runtime::poll_now;
use crate::sockets::ip_name_lookup::resolve_addresses;
use crate::sockets::{MaybeSpawned, WasiSocketsCtxView};
use std::net::IpAddr;
use std::vec;
use wasmtime::Result;
use wasmtime::component::Resource;
use wasmtime_wasi_io::poll::{DynPollable, Pollable, subscribe};

pub struct ResolveAddressStream(MaybeSpawned<Result<vec::IntoIter<IpAddr>, ErrorCode>>);

impl ResolveAddressStream {
    /// Creates an immediately ready stream from addresses that are already resolved.
    pub fn from_addresses(addresses: Vec<IpAddr>) -> Self {
        Self(MaybeSpawned::Ready(Ok(addresses.into_iter())))
    }
}

impl Host for WasiSocketsCtxView<'_> {
    async fn resolve_addresses(
        &mut self,
        network: Resource<Network>,
        name: String,
    ) -> Result<Resource<ResolveAddressStream>, SocketError> {
        // The network resource itself represents the capability to use this
        // method, so we need to check its validity. Other than that, we have no
        // use for it.
        _ = self.table.get(&network)?;

        let fut = resolve_addresses(&self.ctx, name);
        let stream = ResolveAddressStream(MaybeSpawned::poll_or_spawn(async move {
            Ok(fut.await?.into_iter())
        }));

        // Attempt to surface errors immediately.
        if let MaybeSpawned::Ready(Err(err)) = &stream.0 {
            return Err((*err).into());
        }
        Ok(self.table.push(stream)?)
    }
}

impl HostResolveAddressStream for WasiSocketsCtxView<'_> {
    fn resolve_next_address(
        &mut self,
        resource: Resource<ResolveAddressStream>,
    ) -> Result<Option<IpAddress>, SocketError> {
        let stream: &mut ResolveAddressStream = self.table.get_mut(&resource)?;
        let Some(result) = poll_now(|cx| stream.0.poll_ready(cx)) else {
            return Err(ErrorCode::WouldBlock.into());
        };

        match result {
            Ok(iter) => Ok(iter.next().map(|addr| addr.into())),
            Err(err) => Err((*err).into()),
        }
    }

    fn subscribe(
        &mut self,
        resource: Resource<ResolveAddressStream>,
    ) -> Result<Resource<DynPollable>> {
        subscribe(self.table, resource, None)
    }

    async fn drop(&mut self, resource: Resource<ResolveAddressStream>) -> Result<()> {
        let stream = self.table.delete(resource)?;
        if let MaybeSpawned::Pending(fut) = stream.0 {
            fut.cancel().await;
        }
        Ok(())
    }
}

#[async_trait::async_trait]
impl Pollable for ResolveAddressStream {
    async fn ready(&mut self) {
        std::future::poll_fn(|cx| self.0.poll_ready(cx).map(|_| ())).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sockets::WasiSocketsCtx;
    use std::net::{Ipv4Addr, Ipv6Addr};
    use wasmtime::component::ResourceTable;

    #[tokio::test]
    async fn resolved_addresses_preserve_order_and_exhaustion() -> Result<()> {
        for addresses in [
            vec![],
            vec![
                IpAddr::V4(Ipv4Addr::LOCALHOST),
                IpAddr::V6(Ipv6Addr::LOCALHOST),
                IpAddr::V4(Ipv4Addr::LOCALHOST),
            ],
        ] {
            let mut stream = ResolveAddressStream::from_addresses(addresses.clone());
            assert!(poll_now(|cx| stream.ready().as_mut().poll(cx)).is_some());
            let mut table = ResourceTable::new();
            let resource = table.push(stream)?;
            let mut ctx = WasiSocketsCtx::default();
            let mut view = WasiSocketsCtxView {
                ctx: &mut ctx,
                table: &mut table,
            };
            for expected in addresses {
                let actual = view
                    .resolve_next_address(Resource::new_borrow(resource.rep()))?
                    .map(|address| match address {
                        IpAddress::Ipv4((a, b, c, d)) => IpAddr::from([a, b, c, d]),
                        IpAddress::Ipv6((a, b, c, d, e, f, g, h)) => {
                            IpAddr::from([a, b, c, d, e, f, g, h])
                        }
                    });
                assert_eq!(actual, Some(expected));
            }
            for _ in 0..2 {
                assert!(
                    view.resolve_next_address(Resource::new_borrow(resource.rep()))?
                        .is_none()
                );
            }
            HostResolveAddressStream::drop(&mut view, resource).await?;
        }
        Ok(())
    }
}

mod named {
    use crate::p2::SocketError;
    use crate::p2::bindings::named_imports::wasi::sockets::ip_name_lookup;
    use crate::p2::bindings::sockets::network::ErrorCode;
    use crate::p2::bindings::sockets::network::{IpAddress, Network};
    use crate::p2::ip_name_lookup::ResolveAddressStream;
    use crate::sockets::WasiSocketsNamedView;
    use crate::{NamedId, WasiCtxNamedView};
    use wasmtime::Result;
    use wasmtime::component::Resource;
    use wasmtime_wasi_io::poll::DynPollable;

    impl<T> ip_name_lookup::Host for WasiCtxNamedView<'_, T>
    where
        T: WasiSocketsNamedView,
    {
        fn convert_error_code(&mut self, err: SocketError) -> wasmtime::Result<ErrorCode> {
            err.downcast()
        }

        async fn resolve_addresses(
            &mut self,
            id: NamedId,
            network: Resource<Network>,
            name: String,
        ) -> Result<Resource<ResolveAddressStream>, SocketError> {
            super::Host::resolve_addresses(&mut self.0.sockets(id), network, name).await
        }
    }

    impl<T> ip_name_lookup::HostResolveAddressStream for WasiCtxNamedView<'_, T>
    where
        T: WasiSocketsNamedView,
    {
        fn resolve_next_address(
            &mut self,
            id: NamedId,
            resource: Resource<ResolveAddressStream>,
        ) -> Result<Option<IpAddress>, SocketError> {
            super::HostResolveAddressStream::resolve_next_address(&mut self.0.sockets(id), resource)
        }

        fn subscribe(
            &mut self,
            id: NamedId,
            resource: Resource<ResolveAddressStream>,
        ) -> Result<Resource<DynPollable>> {
            super::HostResolveAddressStream::subscribe(&mut self.0.sockets(id), resource)
        }

        async fn drop(
            &mut self,
            id: NamedId,
            resource: Resource<ResolveAddressStream>,
        ) -> Result<()> {
            super::HostResolveAddressStream::drop(&mut self.0.sockets(id), resource).await
        }
    }
}
