use crate::preview2::bindings::sockets::ip_name_lookup::{Host, HostResolveAddressStream};
use crate::preview2::bindings::sockets::network::{ErrorCode, IpAddress, Network};
use crate::preview2::host::network::util;
use crate::preview2::poll::{subscribe, Pollable, Subscribe};
use crate::preview2::{spawn_blocking, AbortOnDropJoinHandle, SocketError, WasiView};
use anyhow::Result;
use std::mem;
use std::net::{Ipv6Addr, ToSocketAddrs};
use std::pin::Pin;
use std::str::FromStr;
use std::vec;
use wasmtime::component::Resource;

use super::network::{from_ipv4_addr, from_ipv6_addr};

pub enum ResolveAddressStream {
    Waiting(AbortOnDropJoinHandle<Result<Vec<IpAddress>, SocketError>>),
    Done(Result<vec::IntoIter<IpAddress>, SocketError>),
}

#[async_trait::async_trait]
impl<T: WasiView> Host for T {
    fn resolve_addresses(
        &mut self,
        network: Resource<Network>,
        name: String,
    ) -> Result<Resource<ResolveAddressStream>, SocketError> {
        let network = self.table().get(&network)?;

        let host = parse(&name)?;

        if !network.allow_ip_name_lookup {
            return Err(ErrorCode::PermanentResolverFailure.into());
        }

        let task = spawn_blocking(move || blocking_resolve(&host));
        let resource = self.table_mut().push(ResolveAddressStream::Waiting(task))?;
        Ok(resource)
    }
}

#[async_trait::async_trait]
impl<T: WasiView> HostResolveAddressStream for T {
    fn resolve_next_address(
        &mut self,
        resource: Resource<ResolveAddressStream>,
    ) -> Result<Option<IpAddress>, SocketError> {
        let stream = self.table_mut().get_mut(&resource)?;
        loop {
            match stream {
                ResolveAddressStream::Waiting(future) => {
                    match crate::preview2::poll_noop(Pin::new(future)) {
                        Some(result) => {
                            *stream = ResolveAddressStream::Done(result.map(|v| v.into_iter()));
                        }
                        None => return Err(ErrorCode::WouldBlock.into()),
                    }
                }
                ResolveAddressStream::Done(slot @ Err(_)) => {
                    mem::replace(slot, Ok(Vec::new().into_iter()))?;
                    unreachable!();
                }
                ResolveAddressStream::Done(Ok(iter)) => return Ok(iter.next()),
            }
        }
    }

    fn subscribe(
        &mut self,
        resource: Resource<ResolveAddressStream>,
    ) -> Result<Resource<Pollable>> {
        subscribe(self.table_mut(), resource, None)
    }

    fn drop(&mut self, resource: Resource<ResolveAddressStream>) -> Result<()> {
        self.table_mut().delete(resource)?;
        Ok(())
    }
}

#[async_trait::async_trait]
impl Subscribe for ResolveAddressStream {
    async fn ready(&mut self) {
        if let ResolveAddressStream::Waiting(future) = self {
            *self = ResolveAddressStream::Done(future.await.map(|v| v.into_iter()));
        }
    }
}

fn parse(name: &str) -> Result<url::Host, SocketError> {
    // `url::Host::parse` serves us two functions:
    // 1. validate the input is a valid domain name or IP,
    // 2. convert unicode domains to punycode.
    match url::Host::parse(&name) {
        Ok(host) => Ok(host),

        // `url::Host::parse` doesn't understand bare IPv6 addresses without [brackets]
        Err(_) => {
            if let Ok(addr) = Ipv6Addr::from_str(name) {
                Ok(url::Host::Ipv6(addr))
            } else {
                Err(ErrorCode::InvalidArgument.into())
            }
        }
    }
}

fn blocking_resolve(host: &url::Host) -> Result<Vec<IpAddress>, SocketError> {
    match host {
        url::Host::Ipv4(v4addr) => Ok(vec![IpAddress::Ipv4(from_ipv4_addr(*v4addr))]),
        url::Host::Ipv6(v6addr) => Ok(vec![IpAddress::Ipv6(from_ipv6_addr(*v6addr))]),
        url::Host::Domain(domain) => {
            // For now use the standard library to perform actual resolution through
            // the usage of the `ToSocketAddrs` trait. This is only
            // resolving names, not ports, so force the port to be 0.
            let addresses = (domain.as_str(), 0)
                .to_socket_addrs()
                .map_err(|_| ErrorCode::NameUnresolvable)? // If/when we use `getaddrinfo` directly, map the error properly.
                .map(|addr| util::to_canonical(&addr.ip()).into())
                .collect();

            Ok(addresses)
        }
    }
}
