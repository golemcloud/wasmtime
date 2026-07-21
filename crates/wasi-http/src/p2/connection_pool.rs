//! HTTP connection pool support for outgoing requests.
//!
//! This module provides [`HttpConnectionPool`], a `Clone`-able pool backed by
//! `hyper-util`'s legacy client and `hyper-rustls` for TLS. It enforces both
//! per-host and global concurrency limits and reuses TCP/TLS connections
//! across requests.
//!
//! It also provides the underlying [`default_send_request_handler`] (which
//! creates a fresh connection per request) and
//! [`pooled_send_request_handler`] (which uses the pool), along with
//! [`default_send_request_with_pool`], which dispatches to one of the two
//! based on whether a pool is configured. Both paths map TLS, connection,
//! and DNS errors to specific WASI [`ErrorCode`](crate::p2::bindings::http::types::ErrorCode)
//! variants instead of using a catch-all.
//!
//! All items in this module are gated on the `default-send-request` feature.

#![cfg(feature = "default-send-request")]

use crate::io::TokioIo;
use crate::p2::bindings::http::types;
use crate::p2::body::HyperOutgoingBody;
use crate::p2::error::{dns_error, hyper_request_error};
use crate::p2::types::{
    ConnectionPermits, HostFutureIncomingResponse, IncomingResponse, OutgoingRequestConfig,
};
use http_body_util::BodyExt;
use hyper_util::client::legacy::connect::{CaptureConnection, capture_connection};
use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, Weak};
use std::time::Duration;
use tokio::net::TcpStream;
use tokio::sync::{Semaphore, watch};
use tokio::time::timeout;

/// Configuration for the HTTP connection pool.
#[derive(Clone, Debug)]
pub struct HttpConnectionPoolConfig {
    /// Maximum number of idle connections per host. Default: 8.
    pub max_idle_per_host: usize,
    /// How long idle connections remain in the pool before being closed. Default: 90 seconds.
    pub idle_timeout: Duration,
    /// Timeout for establishing new TCP connections. Default: 30 seconds.
    ///
    /// This is a global setting applied to all connections created by the pool.
    /// Per-request `connect_timeout` from `OutgoingRequestConfig` is not applied
    /// when using the pool.
    pub connect_timeout: Duration,
    /// Maximum number of concurrent in-flight connections per host. Default: 20.
    ///
    /// When this limit is reached, new requests to the same host will wait
    /// for an existing request to complete before proceeding. This prevents
    /// overwhelming targets (e.g. Cloudflare) with too many simultaneous
    /// TCP/TLS handshakes.
    pub max_connections_per_host: usize,
    /// Maximum total number of concurrent in-flight connections across all hosts. Default: 200.
    ///
    /// This prevents exhausting OS resources (file descriptors, ports, memory)
    /// when connecting to many different hosts simultaneously.
    pub max_total_connections: usize,
    /// Maximum number of distinct host entries tracked in the per-host semaphore map. Default: 1024.
    ///
    /// When this limit is exceeded, stale entries (hosts with no active connections)
    /// are opportunistically cleaned up.
    pub max_host_entries: usize,
}

impl Default for HttpConnectionPoolConfig {
    fn default() -> Self {
        Self {
            max_idle_per_host: 8,
            idle_timeout: Duration::from_secs(90),
            connect_timeout: Duration::from_secs(30),
            max_connections_per_host: 20,
            max_total_connections: 200,
            max_host_entries: 1024,
        }
    }
}

/// A shared HTTP connection pool backed by `hyper-util`'s legacy client.
///
/// This pool reuses TCP and TLS connections across requests to the same host,
/// reducing connection establishment overhead. It is `Clone`-able and can be
/// shared across multiple workers or contexts.
///
/// In addition to connection reuse, the pool enforces concurrency limits:
/// - A per-host limit prevents overwhelming individual targets with too many
///   simultaneous connections (e.g. triggering Cloudflare rate limiting).
/// - A global limit prevents exhausting OS resources (file descriptors, ports).
#[derive(Clone)]
pub struct HttpConnectionPool {
    client: hyper_util::client::legacy::Client<
        hyper_rustls::HttpsConnector<hyper_util::client::legacy::connect::HttpConnector>,
        HyperOutgoingBody,
    >,
    pub(crate) global_semaphore: Arc<Semaphore>,
    host_semaphores: Arc<tokio::sync::Mutex<HashMap<String, Weak<Semaphore>>>>,
    max_connections_per_host: usize,
    max_host_entries: usize,
}

/// Captured pooled connection returned by the p3 pooled send path.
///
/// Call [`Self::poison`] before dropping a response whose connection is unsafe
/// to reuse, for example after a retryable HTTP status response where the
/// server may not have drained the request body.
#[cfg(feature = "p3")]
pub struct P3PooledConnection {
    capture: CaptureConnection,
}

#[cfg(feature = "p3")]
impl P3PooledConnection {
    /// Marks the captured pooled connection as unsafe for reuse.
    pub fn poison(&self) {
        let meta = self.capture.connection_metadata();
        if let Some(connection) = meta.as_ref() {
            connection.poison();
        }
    }
}

impl fmt::Debug for HttpConnectionPool {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HttpConnectionPool").finish_non_exhaustive()
    }
}

impl HttpConnectionPool {
    /// Create a new connection pool with the given configuration.
    pub fn new(config: HttpConnectionPoolConfig) -> Self {
        let root_cert_store = rustls::RootCertStore {
            roots: webpki_roots::TLS_SERVER_ROOTS.into(),
        };
        let mut tls_config = rustls::ClientConfig::builder()
            .with_root_certificates(root_cert_store)
            .with_no_client_auth();
        tls_config.alpn_protocols = vec![b"http/1.1".to_vec()];

        let mut http_connector = hyper_util::client::legacy::connect::HttpConnector::new();
        http_connector.enforce_http(false);
        http_connector.set_connect_timeout(Some(config.connect_timeout));

        // Construct the HttpsConnector directly via From<(H, C)> instead of
        // using HttpsConnectorBuilder. The builder's enable_http1() leaves
        // alpn_protocols empty, which causes HandshakeFailure with servers that
        // require ALPN. The builder also asserts alpn_protocols is empty on
        // input, so we cannot pre-set it. The From impl is equivalent to
        // .https_or_http().wrap_connector() (force_https=false) but lets us
        // keep the ALPN we configured above.
        let https: hyper_rustls::HttpsConnector<_> = (http_connector, Arc::new(tls_config)).into();

        let client =
            hyper_util::client::legacy::Client::builder(hyper_util::rt::TokioExecutor::new())
                .pool_idle_timeout(config.idle_timeout)
                .pool_max_idle_per_host(config.max_idle_per_host)
                .build(https);

        Self {
            client,
            global_semaphore: Arc::new(Semaphore::new(config.max_total_connections)),
            host_semaphores: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
            max_connections_per_host: config.max_connections_per_host,
            max_host_entries: config.max_host_entries,
        }
    }

    /// Get or create a semaphore for the given host key.
    ///
    /// Uses `Weak` references so that semaphores are naturally cleaned up when
    /// all permits are released and no one holds an `Arc` to the semaphore.
    async fn host_semaphore(&self, key: &str) -> Arc<Semaphore> {
        let mut map = self.host_semaphores.lock().await;

        if let Some(weak) = map.get(key) {
            if let Some(strong) = weak.upgrade() {
                return strong;
            }
        }

        let sem = Arc::new(Semaphore::new(self.max_connections_per_host));
        map.insert(key.to_string(), Arc::downgrade(&sem));

        // Opportunistic cleanup when the map grows too large
        if map.len() > self.max_host_entries {
            map.retain(|_, w| w.strong_count() > 0);
        }

        sem
    }

    /// Send `request` through the pool, shaped for the wasi-http p3 host.
    ///
    /// Unlike [`pooled_send_request_handler`], which returns a p2
    /// [`IncomingResponse`], this returns the response directly with its body
    /// already mapped to the p3
    /// [`ErrorCode`](crate::p3::bindings::http::types::ErrorCode). The per-host
    /// and global concurrency permits are held by the returned response body and
    /// released only when that body is dropped (fully drained or aborted),
    /// mirroring the p2 permit lifecycle.
    ///
    /// Connection reuse, keep-alive, and TLS are handled by the same pooled
    /// `hyper-util` client used by the p2 path. The returned capture handle
    /// lets callers poison the underlying connection before dropping a response
    /// that is unsafe to keep alive.
    #[cfg(feature = "p3")]
    pub async fn pooled_send_request_p3(
        &self,
        request: http::Request<
            http_body_util::combinators::UnsyncBoxBody<
                bytes::Bytes,
                crate::p3::bindings::http::types::ErrorCode,
            >,
        >,
        options: Option<crate::p3::RequestOptions>,
    ) -> Result<
        (
            http::Response<
                http_body_util::combinators::UnsyncBoxBody<
                    bytes::Bytes,
                    crate::p3::bindings::http::types::ErrorCode,
                >,
            >,
            Box<
                dyn std::future::Future<
                        Output = Result<(), crate::p3::bindings::http::types::ErrorCode>,
                    > + Send,
            >,
            P3PooledConnection,
        ),
        crate::p3::bindings::http::types::ErrorCode,
    > {
        use crate::p3::bindings::http::types::ErrorCode as P3ErrorCode;
        use http_body::Body as _;

        let scheme = request
            .uri()
            .scheme_str()
            .ok_or(P3ErrorCode::HttpRequestUriInvalid)?;
        let authority = request
            .uri()
            .authority()
            .ok_or(P3ErrorCode::HttpRequestUriInvalid)?
            .clone();
        let scheme_is_http = scheme.eq_ignore_ascii_case("http");
        let scheme_is_https = scheme.eq_ignore_ascii_case("https");
        if !scheme_is_http && !scheme_is_https {
            return Err(P3ErrorCode::HttpProtocolError);
        }

        let connect_timeout = options
            .and_then(|o| o.connect_timeout)
            .unwrap_or(Duration::from_secs(600));
        let first_byte_timeout = options
            .and_then(|o| o.first_byte_timeout)
            .unwrap_or(Duration::from_secs(600));
        let between_bytes_timeout = options
            .and_then(|o| o.between_bytes_timeout)
            .unwrap_or(Duration::from_secs(600));

        // Use a single deadline for both semaphore acquisitions so the total
        // wait never exceeds connect_timeout (rather than 2x connect_timeout).
        let acquire_deadline = tokio::time::Instant::now() + connect_timeout;

        // Per-host first avoids global permit hoarding where a burst to one host
        // grabs all global permits while waiting on per-host, starving others.
        let host_key = make_host_key(scheme, &authority);
        let host_sem = self.host_semaphore(&host_key).await;
        let host_permit = tokio::time::timeout_at(acquire_deadline, host_sem.acquire_owned())
            .await
            .map_err(|_| P3ErrorCode::ConnectionTimeout)?
            .map_err(|_| P3ErrorCode::ConnectionTimeout)?;
        let global_permit = tokio::time::timeout_at(
            acquire_deadline,
            self.global_semaphore.clone().acquire_owned(),
        )
        .await
        .map_err(|_| P3ErrorCode::ConnectionTimeout)?
        .map_err(|_| P3ErrorCode::ConnectionTimeout)?;

        // The pool client is typed for the p2 outgoing body error, so the guest
        // request body error must be adapted to that type. Formatting it loses
        // the structured p3 `ErrorCode`, which step 3 records into the durable
        // oplog (e.g. content-length `HttpRequestBodySize` validation), so the
        // original error is captured out-of-band and preferred when the send
        // fails because of it.
        //
        // The returned request-transmission future must mirror the non-pooled
        // path: it stays pending while the request body is still being sent and
        // resolves with the transmission result. The pooled `hyper-util` client
        // owns connection I/O internally and exposes no per-request connection
        // driver, so completion is signalled off the outgoing request body
        // reaching a terminal state (EOF or error).
        let captured_body_error: Arc<std::sync::Mutex<Option<P3ErrorCode>>> =
            Arc::new(std::sync::Mutex::new(None));
        let (body_done_tx, body_done_rx) =
            tokio::sync::oneshot::channel::<Result<(), P3ErrorCode>>();
        // A body that is already end-of-stream (e.g. an empty body for GET) may
        // never be polled by the client, so signal transmission completion up
        // front; otherwise hand the sender to the body wrapper.
        let body_done_tx = if request.body().is_end_stream() {
            let _ = body_done_tx.send(Ok(()));
            None
        } else {
            Some(body_done_tx)
        };
        let mut request = request.map(|body| {
            OutgoingRequestBodyP3 {
                inner: body,
                captured_error: Arc::clone(&captured_body_error),
                done: body_done_tx,
            }
            .boxed_unsync()
        });
        let pooled_connection = capture_connection(&mut request);

        let resp = timeout(first_byte_timeout, self.client.request(request))
            .await
            .map_err(|_| P3ErrorCode::ConnectionReadTimeout)?
            .map_err(|e| {
                captured_body_error
                    .lock()
                    .expect("p3 pooled body error mutex poisoned")
                    .clone()
                    .unwrap_or_else(|| map_pooled_client_error_p3(&e))
            })?;

        let (parts, incoming) = resp.into_parts();
        let mut between_bytes = tokio::time::interval(between_bytes_timeout);
        between_bytes.reset();
        let body = PooledResponseBodyP3 {
            incoming,
            timeout: between_bytes,
            host_permit: Some(host_permit),
            global_permit: Some(global_permit),
        };
        // If the body wrapper is dropped before signalling (e.g. the client
        // tears down the request after receiving the response without fully
        // draining the request body), report a successful transmission: a
        // response was received, matching the non-pooled path's behaviour when
        // its connection driver has already completed.
        let io: Box<dyn std::future::Future<Output = Result<(), P3ErrorCode>> + Send> =
            Box::new(async move { body_done_rx.await.unwrap_or(Ok(())) });
        Ok((
            http::Response::from_parts(parts, body.boxed_unsync()),
            io,
            P3PooledConnection {
                capture: pooled_connection,
            },
        ))
    }
}

/// Outgoing request body wrapper for [`HttpConnectionPool::pooled_send_request_p3`].
///
/// Adapts the guest p3 request body to the pool client's p2 body error type,
/// captures the original p3 [`ErrorCode`](crate::p3::bindings::http::types::ErrorCode)
/// on failure so it can be reported as the send result, and signals
/// transmission completion (or failure) once the body reaches a terminal state.
#[cfg(feature = "p3")]
struct OutgoingRequestBodyP3 {
    inner: http_body_util::combinators::UnsyncBoxBody<
        bytes::Bytes,
        crate::p3::bindings::http::types::ErrorCode,
    >,
    captured_error: Arc<std::sync::Mutex<Option<crate::p3::bindings::http::types::ErrorCode>>>,
    done: Option<
        tokio::sync::oneshot::Sender<Result<(), crate::p3::bindings::http::types::ErrorCode>>,
    >,
}

#[cfg(feature = "p3")]
impl http_body::Body for OutgoingRequestBodyP3 {
    type Data = bytes::Bytes;
    type Error = types::ErrorCode;

    fn poll_frame(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Result<http_body::Frame<Self::Data>, Self::Error>>> {
        use std::task::Poll;

        match std::pin::Pin::new(&mut self.inner).poll_frame(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(None) => {
                if let Some(tx) = self.done.take() {
                    let _ = tx.send(Ok(()));
                }
                Poll::Ready(None)
            }
            Poll::Ready(Some(Ok(frame))) => Poll::Ready(Some(Ok(frame))),
            Poll::Ready(Some(Err(err))) => {
                self.captured_error
                    .lock()
                    .expect("p3 pooled body error mutex poisoned")
                    .get_or_insert_with(|| err.clone());
                if let Some(tx) = self.done.take() {
                    let _ = tx.send(Err(err.clone()));
                }
                Poll::Ready(Some(Err(types::ErrorCode::InternalError(Some(format!(
                    "{err:?}"
                ))))))
            }
        }
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> http_body::SizeHint {
        self.inner.size_hint()
    }
}

/// Response body returned by [`HttpConnectionPool::pooled_send_request_p3`].
///
/// Wraps the hyper response body, applies the configured between-bytes timeout,
/// and holds the pool concurrency permits. The permits are released as soon as
/// the body reaches a terminal state (EOF, trailers, error, or read timeout),
/// and otherwise on drop (covering an aborted body that is never fully read).
#[cfg(feature = "p3")]
struct PooledResponseBodyP3 {
    incoming: hyper::body::Incoming,
    timeout: tokio::time::Interval,
    host_permit: Option<tokio::sync::OwnedSemaphorePermit>,
    global_permit: Option<tokio::sync::OwnedSemaphorePermit>,
}

#[cfg(feature = "p3")]
impl PooledResponseBodyP3 {
    /// Release the pool concurrency permits, allowing queued requests to the
    /// same host (and globally) to proceed. Idempotent.
    fn release_permits(&mut self) {
        self.host_permit.take();
        self.global_permit.take();
    }
}

#[cfg(feature = "p3")]
impl Drop for PooledResponseBodyP3 {
    fn drop(&mut self) {
        self.release_permits();
    }
}

#[cfg(feature = "p3")]
impl http_body::Body for PooledResponseBodyP3 {
    type Data = bytes::Bytes;
    type Error = crate::p3::bindings::http::types::ErrorCode;

    fn poll_frame(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Result<http_body::Frame<Self::Data>, Self::Error>>> {
        use crate::p3::bindings::http::types::ErrorCode as P3ErrorCode;
        use std::task::{Poll, ready};

        match std::pin::Pin::new(&mut self.incoming).poll_frame(cx) {
            Poll::Ready(None) => {
                self.release_permits();
                Poll::Ready(None)
            }
            Poll::Ready(Some(Err(err))) => {
                self.release_permits();
                Poll::Ready(Some(Err(P3ErrorCode::from_hyper_response_error(err))))
            }
            Poll::Ready(Some(Ok(frame))) => {
                // Trailers are the terminal frame of a body; the p3 stream
                // producer treats them as terminal and may not poll again, so
                // release permits now rather than waiting for an EOF poll.
                if frame.is_trailers() {
                    self.release_permits();
                } else {
                    self.timeout.reset();
                }
                Poll::Ready(Some(Ok(frame)))
            }
            Poll::Pending => {
                ready!(self.timeout.poll_tick(cx));
                self.release_permits();
                Poll::Ready(Some(Err(P3ErrorCode::ConnectionReadTimeout)))
            }
        }
    }

    fn is_end_stream(&self) -> bool {
        self.incoming.is_end_stream()
    }

    fn size_hint(&self) -> http_body::SizeHint {
        self.incoming.size_hint()
    }
}

/// Map a pooled `hyper-util` client error to a p3 [`ErrorCode`], mirroring the
/// p2 [`pooled_send_request_handler`] error classification.
#[cfg(feature = "p3")]
fn map_pooled_client_error_p3(
    e: &hyper_util::client::legacy::Error,
) -> crate::p3::bindings::http::types::ErrorCode {
    use crate::p3::bindings::http::types::ErrorCode as P3ErrorCode;

    if e.is_connect() {
        if let Some(io_err) = find_io_error(e) {
            match io_err.kind() {
                std::io::ErrorKind::ConnectionRefused => return P3ErrorCode::ConnectionRefused,
                std::io::ErrorKind::ConnectionReset
                | std::io::ErrorKind::ConnectionAborted
                | std::io::ErrorKind::BrokenPipe
                | std::io::ErrorKind::UnexpectedEof => return P3ErrorCode::ConnectionTerminated,
                std::io::ErrorKind::TimedOut => return P3ErrorCode::ConnectionTimeout,
                _ => {}
            }
            if io_err.kind() == std::io::ErrorKind::AddrNotAvailable
                || io_err
                    .to_string()
                    .starts_with("failed to lookup address information")
            {
                return P3ErrorCode::DnsError(crate::p3::bindings::http::types::DnsErrorPayload {
                    rcode: Some("address not available".to_string()),
                    info_code: Some(0),
                });
            }
        }
        if find_elapsed_error(e).is_some() {
            return P3ErrorCode::ConnectionTimeout;
        }
        if let Some(rustls_err) = find_rustls_error(e) {
            match rustls_err {
                rustls::Error::InvalidCertificate(_) => return P3ErrorCode::TlsCertificateError,
                rustls::Error::AlertReceived(alert) => {
                    return P3ErrorCode::TlsAlertReceived(
                        crate::p3::bindings::http::types::TlsAlertReceivedPayload {
                            alert_id: Some(u8::from(*alert)),
                            alert_message: Some(format!("{alert:?}")),
                        },
                    );
                }
                _ => return P3ErrorCode::TlsProtocolError,
            }
        }
        P3ErrorCode::DestinationUnavailable
    } else {
        P3ErrorCode::HttpProtocolError
    }
}

/// A oneshot receiver used to signal completion of an outgoing body before
/// the request is sent for methods that do not normally carry a body.
pub use crate::p2::BodyCompletionReceiver;

/// Returns `true` for HTTP methods where the server expects a request body
/// (POST, PUT, PATCH). For all other methods (GET, HEAD, DELETE, CONNECT,
/// OPTIONS, TRACE, and custom/unknown methods), the server may not wait for
/// a body and could close the connection early.
fn method_expects_body(method: &hyper::Method) -> bool {
    method == hyper::Method::POST || method == hyper::Method::PUT || method == hyper::Method::PATCH
}

/// Like [`default_send_request`], but optionally uses a connection pool.
///
/// When `connection_pool` is `Some`, connections are reused across requests
/// to the same host. When `None`, falls back to creating a new connection
/// per request via [`default_send_request_handler`].
///
/// For methods that don't expect a body (GET, HEAD, DELETE, etc.), if the
/// request has a `body_completion` signal, the body is collected in full
/// before sending to prevent the server from closing the connection early.
pub fn default_send_request_with_pool(
    request: hyper::Request<HyperOutgoingBody>,
    config: OutgoingRequestConfig,
    body_completion: Option<BodyCompletionReceiver>,
    connection_pool: Option<HttpConnectionPool>,
) -> HostFutureIncomingResponse {
    // For methods that don't expect a body, if we have a body_completion
    // signal, we need to collect the body first before sending.
    if !method_expects_body(request.method()) && body_completion.is_some() {
        let body_completion = body_completion.unwrap();
        let (parts, body) = request.into_parts();

        let handle = wasmtime_wasi::runtime::spawn(async move {
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
                BodyExt::collect(body).await.map(|collected| {
                    collected
                        .map_err(|_: std::convert::Infallible| unreachable!("Infallible error"))
                        .boxed_unsync()
                })
            };

            let (completion, collected) = futures::future::join(completion_fut, collect_fut).await;

            // Check completion first — it carries specific errors like
            // content-length mismatch or abort.
            completion?;
            let collected_body = collected?;

            let request = hyper::Request::from_parts(parts, collected_body);
            if let Some(pool) = connection_pool {
                Ok(pooled_send_request_handler(request, config, pool).await)
            } else {
                Ok(default_send_request_handler(request, config).await)
            }
        });
        HostFutureIncomingResponse::pending(handle)
    } else {
        let handle = wasmtime_wasi::runtime::spawn(async move {
            if let Some(pool) = connection_pool {
                Ok(pooled_send_request_handler(request, config, pool).await)
            } else {
                Ok(default_send_request_handler(request, config).await)
            }
        });
        HostFutureIncomingResponse::pending(handle)
    }
}

/// Maximum depth to walk error source chains to avoid pathological cycles.
const MAX_ERROR_CHAIN_DEPTH: usize = 32;

/// Walk the error source chain to find a `rustls::Error`, if one exists.
fn find_rustls_error<'a>(err: &'a (dyn std::error::Error + 'static)) -> Option<&'a rustls::Error> {
    let mut cur: &(dyn std::error::Error + 'static) = err;
    for _ in 0..MAX_ERROR_CHAIN_DEPTH {
        if let Some(r) = cur.downcast_ref::<rustls::Error>() {
            return Some(r);
        }
        // `io::Error::source()` returns the *wrapped* error's source, skipping
        // the wrapped error itself, so a rustls error inside (possibly nested)
        // `io::Error` wrappers — as produced by tokio-rustls handshake failures
        // going through the hyper-rustls connector — is invisible to a plain
        // source() walk. Descend into the io::Error's payload instead.
        if let Some(io_err) = cur.downcast_ref::<std::io::Error>()
            && let Some(inner) = io_err.get_ref()
        {
            cur = inner;
            continue;
        }
        cur = cur.source()?;
    }
    None
}

/// Walk the error source chain to find a `std::io::Error`, if one exists.
fn find_io_error<'a>(err: &'a (dyn std::error::Error + 'static)) -> Option<&'a std::io::Error> {
    let mut cur: &(dyn std::error::Error + 'static) = err;
    for _ in 0..MAX_ERROR_CHAIN_DEPTH {
        if let Some(io_err) = cur.downcast_ref::<std::io::Error>() {
            return Some(io_err);
        }
        cur = cur.source()?;
    }
    None
}

/// Walk the error source chain to find a `tokio::time::error::Elapsed`, if one exists.
fn find_elapsed_error<'a>(
    err: &'a (dyn std::error::Error + 'static),
) -> Option<&'a tokio::time::error::Elapsed> {
    let mut cur: &(dyn std::error::Error + 'static) = err;
    for _ in 0..MAX_ERROR_CHAIN_DEPTH {
        if let Some(elapsed) = cur.downcast_ref::<tokio::time::error::Elapsed>() {
            return Some(elapsed);
        }
        cur = cur.source()?;
    }
    None
}

/// The underlying implementation of how an outgoing request is sent. This
/// should likely be spawned in a task. Maps connection and TLS errors to
/// specific WASI [`types::ErrorCode`] variants.
pub async fn default_send_request_handler(
    mut request: hyper::Request<HyperOutgoingBody>,
    OutgoingRequestConfig {
        use_tls,
        connect_timeout,
        first_byte_timeout,
        between_bytes_timeout,
        ..
    }: OutgoingRequestConfig,
) -> Result<IncomingResponse, types::ErrorCode> {
    let authority = if let Some(authority) = request.uri().authority() {
        if authority.port().is_some() {
            authority.to_string()
        } else {
            let port = if use_tls { 443 } else { 80 };
            format!("{}:{port}", authority.to_string())
        }
    } else {
        return Err(types::ErrorCode::HttpRequestUriInvalid);
    };
    let tcp_stream = timeout(connect_timeout, TcpStream::connect(&authority))
        .await
        .map_err(|_| types::ErrorCode::ConnectionTimeout)?
        .map_err(|e| match e.kind() {
            std::io::ErrorKind::AddrNotAvailable => {
                dns_error("address not available".to_string(), 0)
            }

            _ => {
                if e.to_string()
                    .starts_with("failed to lookup address information")
                {
                    dns_error("address not available".to_string(), 0)
                } else {
                    types::ErrorCode::ConnectionRefused
                }
            }
        })?;

    let (mut sender, worker, worker_err_rx) = if use_tls {
        use rustls::pki_types::ServerName;

        // derived from https://github.com/rustls/rustls/blob/main/examples/src/bin/simpleclient.rs
        let root_cert_store = rustls::RootCertStore {
            roots: webpki_roots::TLS_SERVER_ROOTS.into(),
        };
        let config = rustls::ClientConfig::builder()
            .with_root_certificates(root_cert_store)
            .with_no_client_auth();
        let connector = tokio_rustls::TlsConnector::from(std::sync::Arc::new(config));
        let mut parts = authority.split(":");
        let host = parts.next().unwrap_or(&authority);
        let domain = ServerName::try_from(host)
            .map_err(|e| {
                tracing::warn!("dns lookup error: {e:?}");
                dns_error("invalid dns name".to_string(), 0)
            })?
            .to_owned();
        let stream = connector.connect(domain, tcp_stream).await.map_err(|e| {
            // Check the io::Error kind directly first
            match e.kind() {
                std::io::ErrorKind::ConnectionRefused => {
                    tracing::warn!("tls connection refused: {e:?}");
                    return types::ErrorCode::ConnectionRefused;
                }
                std::io::ErrorKind::ConnectionReset
                | std::io::ErrorKind::ConnectionAborted
                | std::io::ErrorKind::BrokenPipe
                | std::io::ErrorKind::UnexpectedEof => {
                    tracing::warn!("tls connection terminated: {e:?}");
                    return types::ErrorCode::ConnectionTerminated;
                }
                std::io::ErrorKind::TimedOut => {
                    tracing::warn!("tls connection timed out: {e:?}");
                    return types::ErrorCode::ConnectionTimeout;
                }
                _ => {}
            }
            // Walk the error chain to find a rustls-specific error
            if let Some(rustls_err) = find_rustls_error(&e) {
                match rustls_err {
                    rustls::Error::InvalidCertificate(_) => {
                        tracing::warn!("tls certificate error: {e:?}");
                        return types::ErrorCode::TlsCertificateError;
                    }
                    rustls::Error::AlertReceived(alert) => {
                        tracing::warn!("tls alert received: {e:?}");
                        return types::ErrorCode::TlsAlertReceived(
                            crate::p2::bindings::http::types::TlsAlertReceivedPayload {
                                alert_id: Some(u8::from(*alert)),
                                alert_message: Some(format!("{alert:?}")),
                            },
                        );
                    }
                    _ => {}
                }
            }
            tracing::warn!("tls protocol error: {e:?}");
            types::ErrorCode::TlsProtocolError
        })?;
        let stream = TokioIo::new(stream);

        let (sender, conn) = timeout(
            connect_timeout,
            hyper::client::conn::http1::handshake(stream),
        )
        .await
        .map_err(|_| types::ErrorCode::ConnectionTimeout)?
        .map_err(hyper_request_error)?;

        let (err_tx, err_rx) = watch::channel(None);
        let worker = wasmtime_wasi::runtime::spawn(async move {
            if let Err(e) = conn.await {
                tracing::debug!("hyper connection worker error: {e:?}");
                let _ = err_tx.send(Some(Arc::new(hyper_request_error(e))));
            }
        });

        (sender, worker, err_rx)
    } else {
        let tcp_stream = TokioIo::new(tcp_stream);
        let (sender, conn) = timeout(
            connect_timeout,
            // TODO: we should plumb the builder through the http context, and use it here
            hyper::client::conn::http1::handshake(tcp_stream),
        )
        .await
        .map_err(|_| types::ErrorCode::ConnectionTimeout)?
        .map_err(hyper_request_error)?;

        let (err_tx, err_rx) = watch::channel(None);
        let worker = wasmtime_wasi::runtime::spawn(async move {
            if let Err(e) = conn.await {
                tracing::debug!("hyper connection worker error: {e:?}");
                let _ = err_tx.send(Some(Arc::new(hyper_request_error(e))));
            }
        });

        (sender, worker, err_rx)
    };

    // at this point, the request contains the scheme and the authority, but
    // the http packet should only include those if addressing a proxy, so
    // remove them here, since SendRequest::send_request does not do it for us
    *request.uri_mut() = http::Uri::builder()
        .path_and_query(
            request
                .uri()
                .path_and_query()
                .map(|p| p.as_str())
                .unwrap_or("/"),
        )
        .build()
        .expect("comes from valid request");

    let resp = timeout(first_byte_timeout, sender.send_request(request))
        .await
        .map_err(|_| types::ErrorCode::ConnectionReadTimeout)?
        .map_err(hyper_request_error)?
        .map(|body| body.map_err(hyper_request_error).boxed_unsync());

    Ok(IncomingResponse {
        resp,
        worker: Some(worker),
        between_bytes_timeout,
        worker_error_receiver: Some(worker_err_rx),
        connection_permits: None,
        pooled_connection: None,
    })
}

/// Construct a normalized host key for per-host semaphore lookup.
///
/// The key is `"{scheme}://{host}:{port}"` with:
/// - Scheme canonicalized to lowercase `"http"` or `"https"`
/// - Host lowercased (DNS names are case-insensitive)
/// - Default ports (80 for HTTP, 443 for HTTPS) filled in
///
/// This ensures that `Example.com`, `example.com:443`, and `HTTPS://example.com`
/// all map to the same semaphore.
fn make_host_key(scheme: &str, authority: &http::uri::Authority) -> String {
    let scheme = if scheme.eq_ignore_ascii_case("https") {
        "https"
    } else {
        "http"
    };
    let host = authority.host().to_ascii_lowercase();
    let port = authority
        .port_u16()
        .unwrap_or(if scheme == "https" { 443 } else { 80 });
    format!("{scheme}://{host}:{port}")
}

/// Send a request using a pooled connection via `hyper-util`'s legacy client.
///
/// The client handles TCP connection, TLS handshake, connection pooling,
/// keep-alive, and connection health checks automatically.
///
/// Concurrency is limited by acquiring per-host and global semaphore permits
/// before sending the request. Permits are held until the response (including
/// body) is dropped.
pub(crate) async fn pooled_send_request_handler(
    mut request: hyper::Request<HyperOutgoingBody>,
    config: OutgoingRequestConfig,
    pool: HttpConnectionPool,
) -> Result<IncomingResponse, types::ErrorCode> {
    // Validate that the request URI is absolute (has scheme + authority).
    // The pooled connector requires an absolute URI to determine host and TLS.
    let scheme = request
        .uri()
        .scheme_str()
        .ok_or(types::ErrorCode::HttpRequestUriInvalid)?;
    let authority = request
        .uri()
        .authority()
        .ok_or(types::ErrorCode::HttpRequestUriInvalid)?
        .clone();
    let scheme_is_http = scheme.eq_ignore_ascii_case("http");
    let scheme_is_https = scheme.eq_ignore_ascii_case("https");
    if !scheme_is_http && !scheme_is_https {
        return Err(types::ErrorCode::HttpProtocolError);
    }
    // Validate that use_tls matches the URI scheme. The pooled connector uses the
    // URI scheme to decide whether to use TLS, so a mismatch would lead to silent
    // behavioral divergence from the non-pooled path.
    let scheme_is_tls = scheme_is_https;
    if config.use_tls != scheme_is_tls {
        tracing::warn!(
            "pooled request use_tls={} but URI scheme is {scheme:?}",
            config.use_tls
        );
        return Err(types::ErrorCode::HttpProtocolError);
    }

    let between_bytes_timeout = config.between_bytes_timeout;
    let first_byte_timeout = config.first_byte_timeout;

    // Use a single deadline for both semaphore acquisitions so the total wait
    // never exceeds connect_timeout (rather than 2× connect_timeout).
    let acquire_deadline = tokio::time::Instant::now() + config.connect_timeout;

    // Acquire concurrency permits: per-host first, then global.
    // Per-host first avoids global permit hoarding where a burst to one host
    // grabs all global permits while waiting on per-host, starving other hosts.
    let host_key = make_host_key(scheme, &authority);
    let host_sem = pool.host_semaphore(&host_key).await;

    tracing::debug!(
        host_key = %host_key,
        "pooled: acquiring per-host permit"
    );
    let host_permit = tokio::time::timeout_at(acquire_deadline, host_sem.acquire_owned())
        .await
        .map_err(|_| {
            tracing::warn!(host_key = %host_key, "pooled: timed out waiting for per-host permit");
            types::ErrorCode::ConnectionTimeout
        })?
        .map_err(|_| {
            tracing::warn!(host_key = %host_key, "pooled: per-host semaphore closed");
            types::ErrorCode::ConnectionTimeout
        })?;

    tracing::debug!("pooled: acquiring global permit");
    let global_permit = tokio::time::timeout_at(
        acquire_deadline,
        pool.global_semaphore.clone().acquire_owned(),
    )
    .await
    .map_err(|_| {
        tracing::warn!("pooled: timed out waiting for global permit");
        types::ErrorCode::ConnectionTimeout
    })?
    .map_err(|_| {
        tracing::warn!("pooled: global semaphore closed");
        types::ErrorCode::ConnectionTimeout
    })?;

    let uri = request.uri().clone();
    tracing::debug!(
        %uri,
        use_tls = config.use_tls,
        "pooled: sending request"
    );

    // Capture a handle to the underlying pooled connection so the caller
    // can poison it (via `Connected::poison()`) if it later determines the
    // connection is in an unsafe state for reuse — for example when the
    // server returned a non-2xx response without draining the request body.
    // The pool will then refuse to hand the same connection back on
    // subsequent requests.
    let pooled_connection: CaptureConnection = capture_connection(&mut request);

    let resp = timeout(first_byte_timeout, pool.client.request(request))
        .await
        .map_err(|_| types::ErrorCode::ConnectionReadTimeout)?
        .map_err(|e| {
            // hyper_util::client::legacy::Error wraps hyper errors and
            // connector errors. Try to extract a more specific ErrorCode.
            if e.is_connect() {
                // Connection-phase error: could be DNS, TCP, or TLS.
                // Walk the full error chain since hyper-util wraps errors
                // in multiple layers.
                if let Some(io_err) = find_io_error(&e) {
                    match io_err.kind() {
                        std::io::ErrorKind::ConnectionRefused => {
                            tracing::warn!("pooled connection refused: {e:?}");
                            return types::ErrorCode::ConnectionRefused;
                        }
                        std::io::ErrorKind::ConnectionReset
                        | std::io::ErrorKind::ConnectionAborted
                        | std::io::ErrorKind::BrokenPipe
                        | std::io::ErrorKind::UnexpectedEof => {
                            tracing::warn!("pooled connection terminated: {e:?}");
                            return types::ErrorCode::ConnectionTerminated;
                        }
                        std::io::ErrorKind::TimedOut => {
                            tracing::warn!("pooled connection timed out: {e:?}");
                            return types::ErrorCode::ConnectionTimeout;
                        }
                        _ => {}
                    }
                    // Check for DNS-related errors (matches non-pooled path logic)
                    if io_err.kind() == std::io::ErrorKind::AddrNotAvailable
                        || io_err
                            .to_string()
                            .starts_with("failed to lookup address information")
                    {
                        tracing::warn!("pooled dns error: {e:?}");
                        return dns_error("address not available".to_string(), 0);
                    }
                }
                // Check for tokio timeout errors that may be wrapped
                // in the hyper-util error chain (e.g. pool connect timeout).
                if find_elapsed_error(&e).is_some() {
                    tracing::warn!("pooled connection timed out (elapsed): {e:?}");
                    return types::ErrorCode::ConnectionTimeout;
                }
                if let Some(rustls_err) = find_rustls_error(&e) {
                    match rustls_err {
                        rustls::Error::InvalidCertificate(_) => {
                            tracing::warn!("pooled tls certificate error: {e:?}");
                            return types::ErrorCode::TlsCertificateError;
                        }
                        rustls::Error::AlertReceived(alert) => {
                            tracing::warn!("pooled tls alert received: {e:?}");
                            return types::ErrorCode::TlsAlertReceived(
                                crate::p2::bindings::http::types::TlsAlertReceivedPayload {
                                    alert_id: Some(u8::from(*alert)),
                                    alert_message: Some(format!("{alert:?}")),
                                },
                            );
                        }
                        _ => {
                            tracing::warn!("pooled tls protocol error: {e:?}");
                            return types::ErrorCode::TlsProtocolError;
                        }
                    }
                }
                tracing::warn!(%uri, "pooled connection error: {e:?}");
                types::ErrorCode::DestinationUnavailable
            } else {
                tracing::warn!("pooled request error: {e:?}");
                types::ErrorCode::HttpProtocolError
            }
        })?
        .map(|body| body.map_err(hyper_request_error).boxed_unsync());

    Ok(IncomingResponse {
        resp,
        worker: None,
        between_bytes_timeout,
        worker_error_receiver: None,
        connection_permits: Some(ConnectionPermits {
            _host: host_permit,
            _global: global_permit,
        }),
        pooled_connection: Some(pooled_connection),
    })
}
