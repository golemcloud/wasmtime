//! Implementation of the `wasi:http/outgoing-handler` interface.

use crate::{
    WasiHttpImpl, WasiHttpView,
    bindings::http::{
        outgoing_handler,
        types::{self, Scheme},
    },
    error::internal_error,
    http_request_error,
    types::{HostFutureIncomingResponse, HostOutgoingRequest, OutgoingRequestConfig},
};
use bytes::Bytes;
use http_body_util::{BodyExt, Empty};
use hyper::Method;
use wasmtime::component::Resource;

impl<T> outgoing_handler::Host for WasiHttpImpl<T>
where
    T: WasiHttpView + Send,
{
    async fn handle(
        &mut self,
        request_id: Resource<HostOutgoingRequest>,
        options: Option<Resource<types::RequestOptions>>,
    ) -> crate::HttpResult<Resource<HostFutureIncomingResponse>> {
        let opts = options.map(|opts| self.table().get(&opts)).transpose()?;

        let connect_timeout = opts
            .and_then(|opts| opts.connect_timeout)
            .unwrap_or(std::time::Duration::from_secs(600));

        let first_byte_timeout = opts
            .and_then(|opts| opts.first_byte_timeout)
            .unwrap_or(std::time::Duration::from_secs(600));

        let between_bytes_timeout = opts
            .and_then(|opts| opts.between_bytes_timeout)
            .unwrap_or(std::time::Duration::from_secs(600));

        let mut req = self.table().delete(request_id)?;

        let body_completion = req.body_completion.take();

        let method = match req.method {
            types::Method::Get => Method::GET,
            types::Method::Head => Method::HEAD,
            types::Method::Post => Method::POST,
            types::Method::Put => Method::PUT,
            types::Method::Delete => Method::DELETE,
            types::Method::Connect => Method::CONNECT,
            types::Method::Options => Method::OPTIONS,
            types::Method::Trace => Method::TRACE,
            types::Method::Patch => Method::PATCH,
            types::Method::Other(m) => match hyper::Method::from_bytes(m.as_bytes()) {
                Ok(method) => method,
                Err(_) => return Err(types::ErrorCode::HttpRequestMethodInvalid.into()),
            },
        };

        let mut builder = hyper::Request::builder();
        builder = builder.method(method.clone());

        let (use_tls, scheme) = match req.scheme.unwrap_or(Scheme::Https) {
            Scheme::Http => (false, http::uri::Scheme::HTTP),
            Scheme::Https => (true, http::uri::Scheme::HTTPS),
            Scheme::Other(_) => return Err(types::ErrorCode::HttpProtocolError.into()),
        };

        let authority = match req.authority {
            Some(a) if !a.is_empty() => a,
            _ => return Err(types::ErrorCode::HttpRequestUriInvalid.into()),
        };

        builder = builder.header(hyper::header::HOST, &authority);

        let mut uri = http::Uri::builder()
            .scheme(scheme)
            .authority(authority.clone());

        if let Some(path) = req.path_with_query {
            uri = uri.path_and_query(path);
        }

        builder = builder.uri(uri.build().map_err(http_request_error)?);

        for (k, v) in req.headers.as_ref().iter() {
            builder = builder.header(k, v);
        }

        let body = req.body.unwrap_or_else(|| {
            Empty::<Bytes>::new()
                .map_err(|_| unreachable!("Infallible error"))
                .boxed_unsync()
        });

        let request = builder
            .body(body)
            .map_err(|err| internal_error(err.to_string()))?;

        let config = OutgoingRequestConfig {
            use_tls,
            connect_timeout,
            first_byte_timeout,
            between_bytes_timeout,
        };

        // Always delegate to send_request, which allows the implementor
        // (e.g. Golem) to decide whether to defer or send immediately.
        // The default implementation handles body-collection for non-body
        // methods internally.
        let future = self.send_request(request, config, body_completion)?;
        Ok(self.table().push(future)?)
    }
}
