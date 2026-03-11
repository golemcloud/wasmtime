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

/// Returns `true` for HTTP methods where the server expects a request body
/// (POST, PUT, PATCH). For all other methods (GET, HEAD, DELETE, CONNECT,
/// OPTIONS, TRACE, and custom/unknown methods), the server may not wait for
/// a body and could close the connection early.
#[cfg(feature = "default-send-request")]
fn method_expects_body(method: &Method) -> bool {
    method == Method::POST || method == Method::PUT || method == Method::PATCH
}

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

        #[cfg(feature = "default-send-request")]
        let mut req = self.table().delete(request_id)?;
        #[cfg(not(feature = "default-send-request"))]
        let req = self.table().delete(request_id)?;

        // Extract body_completion before consuming the request fields.
        #[cfg(feature = "default-send-request")]
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

        // For methods that don't expect a body (GET, HEAD, DELETE, etc.),
        // if the guest has created an OutgoingBody, defer sending the request
        // until the body is finished. This prevents the server from closing
        // the connection before the client is done.
        // For body-expected methods (POST, PUT, PATCH), send immediately and
        // stream the body concurrently as today.
        #[cfg(feature = "default-send-request")]
        if !method_expects_body(&method) && body_completion.is_some() {
            let body_completion = body_completion.unwrap();
            let (parts, body) = request.into_parts();

            let body_collection: crate::types::BodyCollectionHandle =
                wasmtime_wasi::runtime::spawn(async move {
                    // Drain the body and wait for completion concurrently.
                    // We must drain immediately (not wait for completion first)
                    // because the body channel is bounded — if we wait, the guest
                    // may fill the channel and block before reaching finish().
                    let completion_fut = async {
                        match body_completion.await {
                            Ok(Ok(())) => Ok(()),
                            Ok(Err(e)) => Err(e),
                            Err(_) => Err(types::ErrorCode::HttpProtocolError),
                        }
                    };

                    let collect_fut = async {
                        // Collect all body frames (including trailers) using
                        // BodyExt::collect, which preserves trailers.
                        BodyExt::collect(body).await.map(|collected| {
                            collected
                                .map_err(|_: std::convert::Infallible| {
                                    unreachable!("Infallible error")
                                })
                                .boxed_unsync()
                        })
                    };

                    let (completion, collected) =
                        futures::future::join(completion_fut, collect_fut).await;

                    // Check completion first — it carries specific errors like
                    // content-length mismatch or abort.
                    completion?;
                    collected
                });

            let future = HostFutureIncomingResponse::DeferredCollectingBody {
                body_collection,
                request_parts: parts,
                config,
            };
            return Ok(self.table().push(future)?);
        }

        // Immediate send path: for body-expected methods, or when body() was never called
        let future = self.send_request(request, config)?;
        Ok(self.table().push(future)?)
    }
}
