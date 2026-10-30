use crate::{Error, RequestOptions, Result, WasiBody};
use bytes::Bytes;
use http_body::{Body, Frame};
use http_body_util::BodyExt;
use hyper_util::client::legacy::connect::{CaptureConnection, capture_connection};
use std::collections::HashMap;
use std::pin::Pin;
use std::sync::{Arc, Mutex, Weak};
use std::task::{Context, Poll};
use std::time::Duration;
use tokio::sync::{OwnedSemaphorePermit, Semaphore, oneshot};

/// Limits and timeouts for a shared HTTP connection pool.
#[derive(Clone, Debug)]
pub struct HttpConnectionPoolConfig {
    /// Maximum idle connections per host (default 8).
    pub max_idle_per_host: usize,
    /// Idle connection lifetime (default 90 seconds).
    pub idle_timeout: Duration,
    /// TCP connection timeout, independent of per-request permit acquisition.
    pub connect_timeout: Duration,
    /// Maximum active response bodies per host (default 20).
    pub max_connections_per_host: usize,
    /// Maximum active response bodies across hosts (default 200).
    pub max_total_connections: usize,
    /// Threshold for pruning unused host semaphore entries (default 1024).
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

/// Shared TCP/TLS transport for both WASI HTTP versions.
///
/// This implements [`crate::WasiHttpHooks`] and can be used directly as the
/// `hooks` field of [`crate::WasiHttpCtxView`]. That implementation attaches an
/// `Arc<PooledConnection>` to the response extensions for P2 connection poisoning.
/// Custom hooks can instead call [`Self::send_request`] and retain its connection
/// handle separately.
#[derive(Clone)]
pub struct HttpConnectionPool {
    client: hyper_util::client::legacy::Client<
        hyper_rustls::HttpsConnector<hyper_util::client::legacy::connect::HttpConnector>,
        WasiBody,
    >,
    global: Arc<Semaphore>,
    hosts: Arc<Mutex<HashMap<String, Weak<Semaphore>>>>,
    config: HttpConnectionPoolConfig,
}

impl std::fmt::Debug for HttpConnectionPool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HttpConnectionPool").finish_non_exhaustive()
    }
}

/// Connection identity independent of the response body's concurrency permits.
pub struct PooledConnection {
    capture: CaptureConnection,
}

impl std::fmt::Debug for PooledConnection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PooledConnection").finish_non_exhaustive()
    }
}

impl PooledConnection {
    /// Prevent reuse of a connection whose request or response was interrupted.
    pub fn poison(&self) {
        if let Some(connection) = self.capture.connection_metadata().as_ref() {
            connection.poison();
        }
    }
}

impl HttpConnectionPool {
    /// Construct a pool that can be shared across stores and HTTP versions.
    pub fn new(config: HttpConnectionPoolConfig) -> Self {
        let mut tls = rustls::ClientConfig::builder()
            .with_root_certificates(rustls::RootCertStore {
                roots: webpki_roots::TLS_SERVER_ROOTS.into(),
            })
            .with_no_client_auth();
        tls.alpn_protocols = vec![b"http/1.1".to_vec()];
        let mut http = hyper_util::client::legacy::connect::HttpConnector::new();
        http.enforce_http(false);
        http.set_connect_timeout(Some(config.connect_timeout));
        let https = hyper_rustls::HttpsConnector::from((http, Arc::new(tls)));
        let client =
            hyper_util::client::legacy::Client::builder(hyper_util::rt::TokioExecutor::new())
                .pool_idle_timeout(config.idle_timeout)
                .pool_max_idle_per_host(config.max_idle_per_host)
                .build(https);
        Self {
            client,
            global: Arc::new(Semaphore::new(config.max_total_connections)),
            hosts: Arc::new(Mutex::new(HashMap::new())),
            config,
        }
    }

    fn host_semaphore(&self, key: String) -> Arc<Semaphore> {
        let mut hosts = self.hosts.lock().unwrap();
        if let Some(sem) = hosts.get(&key).and_then(Weak::upgrade) {
            return sem;
        }
        let sem = Arc::new(Semaphore::new(self.config.max_connections_per_host));
        hosts.insert(key, Arc::downgrade(&sem));
        if hosts.len() > self.config.max_host_entries {
            hosts.retain(|_, sem| sem.strong_count() > 0);
        }
        sem
    }

    /// Send using the unified HTTP body and options contract.
    ///
    /// The returned future reports request transmission, not response processing.
    /// The caller retains ownership of the response-processing future supplied to
    /// `WasiHttpHooks::send_request`. Permits belong to the response body until
    /// EOF, trailers, error, timeout, or drop for P3. P2 transfers a lease in
    /// response extensions to the incoming-body resource instead. Poisoning
    /// does not release permits.
    pub async fn send_request(
        &self,
        request: http::Request<WasiBody>,
        options: Option<RequestOptions>,
    ) -> Result<
        (
            http::Response<WasiBody>,
            Box<dyn Future<Output = Result<(), Error>> + Send>,
            PooledConnection,
        ),
        Error,
    > {
        #[cfg(feature = "p2")]
        let request = crate::p2::body::prepare_request(request).await?;
        let p2 = request.extensions().get::<crate::P2Request>().is_some();
        let scheme = request
            .uri()
            .scheme_str()
            .ok_or(Error::HttpRequestUriInvalid)?;
        let authority = request
            .uri()
            .authority()
            .ok_or(Error::HttpRequestUriInvalid)?;
        if scheme != "http" && scheme != "https" {
            return Err(Error::HttpProtocolError);
        }
        let port = authority
            .port_u16()
            .unwrap_or(if scheme == "https" { 443 } else { 80 });
        let key = format!(
            "{scheme}://{}:{port}",
            authority.host().to_ascii_lowercase()
        );
        let options = options.unwrap_or_default();
        let deadline = tokio::time::Instant::now()
            + options.connect_timeout.unwrap_or(Duration::from_secs(600));
        let host = tokio::time::timeout_at(deadline, self.host_semaphore(key).acquire_owned())
            .await
            .map_err(|_| Error::ConnectionTimeout)?
            .map_err(|_| Error::ConnectionTimeout)?;
        let global = tokio::time::timeout_at(deadline, self.global.clone().acquire_owned())
            .await
            .map_err(|_| Error::ConnectionTimeout)?
            .map_err(|_| Error::ConnectionTimeout)?;

        let error = Arc::new(Mutex::new(None));
        let (tx, rx) = oneshot::channel();
        let done = if request.body().is_end_stream() {
            let _ = tx.send(());
            None
        } else {
            Some(tx)
        };
        let mut request = request.map(|inner| {
            RequestBody {
                inner,
                error: error.clone(),
                done,
            }
            .boxed_unsync()
        });
        let capture = capture_connection(&mut request);
        let response = tokio::time::timeout(
            options
                .first_byte_timeout
                .unwrap_or(Duration::from_secs(600)),
            self.client.request(request),
        )
        .await
        .map_err(|_| Error::ConnectionReadTimeout)?
        .map_err(|err| {
            error
                .lock()
                .unwrap()
                .take()
                .unwrap_or_else(|| pooled_error(&err))
        })?;
        let timeout = crate::default_send_request::ResponseTimeout::new(
            options
                .between_bytes_timeout
                .unwrap_or(Duration::from_secs(600))
                .max(Duration::from_nanos(1)),
            p2,
        );
        let mut permits = Some((host, global));
        let lease = if p2 {
            permits.take().map(|p| P2ConnectionLease {
                _permits: Arc::new(p),
            })
        } else {
            None
        };
        let mut response = response.map(|body| {
            let permits = if body.is_end_stream() { None } else { permits };
            ResponseBody {
                body,
                timeout,
                permits,
                ended: false,
            }
            .boxed_unsync()
        });
        if let Some(lease) = lease {
            response.extensions_mut().insert(lease);
        }
        Ok((
            response,
            Box::new(async move {
                // A received response makes a dropped completion sender successful.
                let _ = rx.await;
                match error.lock().unwrap().take() {
                    Some(error) => Err(error),
                    None => Ok(()),
                }
            }),
            PooledConnection { capture },
        ))
    }
}

impl crate::WasiHttpHooks for HttpConnectionPool {
    fn send_request(
        &mut self,
        request: http::Request<WasiBody>,
        options: Option<RequestOptions>,
        response_processing: Box<dyn Future<Output = Result<(), Error>> + Send>,
    ) -> Box<
        dyn Future<
                Output = Result<(
                    http::Response<WasiBody>,
                    Box<dyn Future<Output = Result<(), Error>> + Send>,
                )>,
            > + Send,
    > {
        let pool = self.clone();
        drop(response_processing);
        Box::new(async move {
            let (mut response, transmission, connection) =
                pool.send_request(request, options).await?;
            response.extensions_mut().insert(Arc::new(connection));
            Ok((response, transmission))
        })
    }
}

struct RequestBody {
    inner: WasiBody,
    error: Arc<Mutex<Option<Error>>>,
    done: Option<oneshot::Sender<()>>,
}

impl Body for RequestBody {
    type Data = Bytes;
    type Error = Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Error>>> {
        let result = Pin::new(&mut self.inner).poll_frame(cx);
        let result = match result {
            Poll::Ready(Some(Err(err))) => {
                self.error.lock().unwrap().get_or_insert(err);
                Poll::Ready(Some(Err(Error::HttpProtocolError)))
            }
            other => other,
        };
        if matches!(&result, Poll::Ready(None | Some(Err(_))))
            || matches!(&result, Poll::Ready(Some(Ok(frame))) if frame.is_trailers() || self.inner.is_end_stream())
        {
            if let Some(done) = self.done.take() {
                let _ = done.send(());
            }
        }
        result
    }
    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }
    fn size_hint(&self) -> http_body::SizeHint {
        self.inner.size_hint()
    }
}

#[derive(Clone, Debug)]
pub(crate) struct P2ConnectionLease {
    _permits: Arc<(OwnedSemaphorePermit, OwnedSemaphorePermit)>,
}

struct ResponseBody<B> {
    body: B,
    timeout: crate::default_send_request::ResponseTimeout,
    permits: Option<(OwnedSemaphorePermit, OwnedSemaphorePermit)>,
    ended: bool,
}

impl<B: Body<Data = Bytes> + Unpin> Body for ResponseBody<B>
where
    Error: From<B::Error>,
{
    type Data = Bytes;
    type Error = Error;
    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Error>>> {
        if self.ended {
            return Poll::Ready(None);
        }
        if self.timeout.before_frame(cx) {
            self.ended = true;
            self.permits.take();
            return Poll::Ready(Some(Err(Error::ConnectionReadTimeout)));
        }
        let result = match Pin::new(&mut self.body).poll_frame(cx) {
            Poll::Pending => match self.timeout.poll_pending(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(_) => Poll::Ready(Some(Err(Error::ConnectionReadTimeout))),
            },
            Poll::Ready(Some(Err(err))) => Poll::Ready(Some(Err(match err.into() {
                Error::Hyper(err) if err.is_timeout() => Error::HttpResponseTimeout,
                err => err,
            }))),
            Poll::Ready(Some(Ok(frame))) => {
                self.timeout.delivered();
                Poll::Ready(Some(Ok(frame)))
            }
            Poll::Ready(None) => Poll::Ready(None),
        };
        if matches!(&result, Poll::Ready(None | Some(Err(_))))
            || matches!(&result, Poll::Ready(Some(Ok(frame))) if frame.is_trailers() || self.body.is_end_stream())
        {
            self.ended = true;
            self.permits.take();
        }
        result
    }
    fn is_end_stream(&self) -> bool {
        self.ended || self.body.is_end_stream()
    }
    fn size_hint(&self) -> http_body::SizeHint {
        self.body.size_hint()
    }
}

pub(crate) fn find_rustls_error<'a>(
    mut err: &'a (dyn std::error::Error + 'static),
) -> Option<&'a rustls::Error> {
    for _ in 0..32 {
        if let Some(err) = err.downcast_ref::<rustls::Error>() {
            return Some(err);
        }
        err = match err
            .downcast_ref::<std::io::Error>()
            .and_then(std::io::Error::get_ref)
        {
            Some(inner) => inner,
            None => err.source()?,
        };
    }
    None
}

fn pooled_error(err: &hyper_util::client::legacy::Error) -> Error {
    if !err.is_connect() {
        return Error::HttpProtocolError;
    }
    if let Some(tls) = find_rustls_error(err) {
        return match tls {
            rustls::Error::InvalidCertificate(_) => Error::TlsCertificateError,
            rustls::Error::AlertReceived(alert) => Error::TlsAlertReceived {
                alert_id: Some(u8::from(*alert)),
                alert_message: Some(format!("{alert:?}")),
            },
            _ => Error::TlsProtocolError,
        };
    }
    let mut source: &(dyn std::error::Error + 'static) = err;
    for _ in 0..32 {
        if let Some(io) = source.downcast_ref::<std::io::Error>() {
            use std::io::ErrorKind::*;
            return match io.kind() {
                ConnectionRefused => Error::ConnectionRefused,
                ConnectionReset | ConnectionAborted | BrokenPipe | UnexpectedEof => {
                    Error::ConnectionTerminated
                }
                TimedOut => Error::ConnectionTimeout,
                AddrNotAvailable => Error::DnsError {
                    rcode: Some("address not available".into()),
                    info_code: Some(0),
                },
                _ if io
                    .to_string()
                    .starts_with("failed to lookup address information") =>
                {
                    Error::DnsError {
                        rcode: Some("address not available".into()),
                        info_code: Some(0),
                    }
                }
                _ => Error::DestinationUnavailable,
            };
        }
        if source.is::<tokio::time::error::Elapsed>() {
            return Error::ConnectionTimeout;
        }
        let Some(next) = source.source() else {
            break;
        };
        source = next;
    }
    Error::DestinationUnavailable
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::{FutureExt, stream};
    use http_body_util::{Empty, StreamBody};

    #[tokio::test(start_paused = true)]
    async fn p2_frame_timeout_excludes_guest_processing() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let stream = stream::poll_fn(move |cx| rx.poll_recv(cx));
        let (mut body, _) = response_body(StreamBody::new(stream).boxed_unsync()).await;
        body.timeout =
            crate::default_send_request::ResponseTimeout::new(Duration::from_secs(1), true);
        tokio::time::advance(Duration::from_secs(10)).await;
        assert!(body.frame().now_or_never().is_none());
        tx.send(Ok(Frame::data(Bytes::from_static(b"first"))))
            .unwrap();
        assert!(body.frame().await.unwrap().is_ok());
        tokio::time::advance(Duration::from_secs(10)).await;
        assert!(body.frame().now_or_never().is_none());
        tokio::time::advance(Duration::from_millis(500)).await;
        tx.send(Ok(Frame::data(Bytes::from_static(b"second"))))
            .unwrap();
        assert!(body.frame().await.unwrap().is_ok());
        assert!(body.frame().now_or_never().is_none());
        tokio::time::advance(Duration::from_secs(2)).await;
        assert!(matches!(
            body.frame().await,
            Some(Err(Error::ConnectionReadTimeout))
        ));
    }

    #[cfg(feature = "p2")]
    #[tokio::test]
    async fn p2_second_request_waits_for_resource_drop_after_eof() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        for content in ["", "hello"] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let server = tokio::spawn(async move {
                for _ in 0..2 {
                    let (mut socket, _) = listener.accept().await.unwrap();
                    let mut header = Vec::new();
                    while !header.ends_with(b"\r\n\r\n") {
                        header.push(socket.read_u8().await.unwrap());
                    }
                    socket.write_all(format!("HTTP/1.1 200 OK\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{content}", content.len()).as_bytes()).await.unwrap();
                }
            });
            let pool = HttpConnectionPool::new(HttpConnectionPoolConfig {
                max_total_connections: 1,
                ..Default::default()
            });
            let request = || {
                let mut request = http::Request::builder()
                    .uri(format!("http://{address}/"))
                    .body(Empty::new().map_err(Error::from).boxed_unsync())
                    .unwrap();
                request.extensions_mut().insert(crate::P2Request);
                request
            };
            let (response, sent, _) = pool.send_request(request(), None).await.unwrap();
            Pin::from(sent).await.unwrap();
            let (mut parts, body) = response.into_parts();
            let mut resource = crate::p2::body::HostIncomingBody::new(body);
            resource.connection_lease = parts.extensions.remove::<P2ConnectionLease>();
            drop(parts);
            let mut stream = resource.take_stream().unwrap();
            let mut received = Vec::new();
            loop {
                stream.ready().await;
                match stream.read(1024) {
                    Ok(bytes) => received.extend_from_slice(&bytes),
                    Err(wasmtime_wasi::p2::StreamError::Closed) => break,
                    Err(err) => panic!("unexpected stream error: {err:?}"),
                }
            }
            assert_eq!(received, content.as_bytes());
            drop(stream);
            let mut second = Box::pin(pool.send_request(request(), None));
            assert!(second.as_mut().now_or_never().is_none());
            assert_eq!(pool.global.available_permits(), 0);
            drop(resource);
            let (response, sent, _) = tokio::time::timeout(Duration::from_secs(5), second)
                .await
                .unwrap()
                .unwrap();
            Pin::from(sent).await.unwrap();
            drop(response);
            server.await.unwrap();
        }
    }

    #[tokio::test]
    async fn empty_transmission_and_poison_are_independent_of_body_permits() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            for _ in 0..2 {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut header = Vec::new();
                while !header.ends_with(b"\r\n\r\n") {
                    header.push(socket.read_u8().await.unwrap());
                }
                socket
                    .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhello")
                    .await
                    .unwrap();
                // Wait for poisoning to close the connection, rather than closing
                // from the server and accidentally testing a non-reusable response.
                let mut buffer = [0; 1];
                assert_eq!(socket.read(&mut buffer).await.unwrap(), 0);
            }
        });
        let mut pool = HttpConnectionPool::new(HttpConnectionPoolConfig {
            max_total_connections: 1,
            ..Default::default()
        });
        tokio::time::timeout(Duration::from_secs(3), async {
            for i in 0..2 {
                let req = http::Request::builder()
                    .uri(format!("http://{address}/"))
                    .body(Empty::new().map_err(Error::from).boxed_unsync())
                    .unwrap();
                let (response, transmitted, connection) = if i == 0 {
                    let (response, transmitted, connection) =
                        pool.send_request(req, None).await.unwrap();
                    (response, transmitted, Arc::new(connection))
                } else {
                    let send = crate::WasiHttpHooks::send_request(
                        &mut pool,
                        req,
                        None,
                        Box::new(std::future::pending()),
                    );
                    let (response, transmitted) = Pin::from(send).await.unwrap();
                    let connection = response
                        .extensions()
                        .get::<Arc<PooledConnection>>()
                        .unwrap()
                        .clone();
                    (response, transmitted, connection)
                };
                Pin::from(transmitted).await.unwrap();
                assert_eq!(pool.global.available_permits(), 0);
                connection.poison();
                assert_eq!(pool.global.available_permits(), 0);
                assert_eq!(
                    response.into_body().collect().await.unwrap().to_bytes(),
                    "hello"
                );
                assert_eq!(pool.global.available_permits(), 1);
            }
            drop(pool);
            server.await.unwrap();
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn early_response_with_dropped_upload_is_successful_transmission() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut header = Vec::new();
            while !header.ends_with(b"\r\n\r\n") {
                header.push(socket.read_u8().await.unwrap());
            }
            socket
                .write_all(b"HTTP/1.1 302 Found\r\nConnection: close\r\nContent-Length: 0\r\n\r\n")
                .await
                .unwrap();
        });
        let pool = HttpConnectionPool::new(Default::default());
        let body = StreamBody::new(stream::pending::<Result<Frame<Bytes>, Error>>()).boxed_unsync();
        let req = http::Request::builder()
            .method("POST")
            .uri(format!("http://{address}/"))
            .body(body)
            .unwrap();
        tokio::time::timeout(Duration::from_secs(3), async {
            let (response, transmitted, _) = pool.send_request(req, None).await.unwrap();
            assert_eq!(response.status(), 302);
            Pin::from(transmitted).await.unwrap();
            server.await.unwrap();
        })
        .await
        .unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn host_wait_does_not_hoard_global_and_deadline_is_shared() {
        let pool = HttpConnectionPool::new(HttpConnectionPoolConfig {
            max_connections_per_host: 1,
            max_total_connections: 1,
            ..Default::default()
        });
        let host = pool.host_semaphore("http://example.com:80".into());
        let held_host = host.clone().acquire_owned().await.unwrap();
        let request = http::Request::builder()
            .uri("http://example.com/")
            .body(Empty::new().map_err(Error::from).boxed_unsync())
            .unwrap();
        let mut send = Box::pin(pool.send_request(
            request,
            Some(RequestOptions {
                connect_timeout: Some(Duration::from_millis(100)),
                ..Default::default()
            }),
        ));
        assert!(send.as_mut().now_or_never().is_none());
        assert_eq!(pool.global.available_permits(), 1);
        let held_global = pool.global.clone().acquire_owned().await.unwrap();
        tokio::time::advance(Duration::from_millis(70)).await;
        drop(held_host);
        assert!(send.as_mut().now_or_never().is_none());
        tokio::time::advance(Duration::from_millis(31)).await;
        assert!(matches!(
            send.as_mut().now_or_never(),
            Some(Err(Error::ConnectionTimeout))
        ));
        assert_eq!(host.available_permits(), 1);
        drop(held_global);
    }

    #[test]
    fn nested_tls_errors_are_classified() {
        let error = std::io::Error::other(std::io::Error::other(
            rustls::Error::InvalidCertificate(rustls::CertificateError::Expired),
        ));
        assert!(matches!(
            find_rustls_error(&error),
            Some(rustls::Error::InvalidCertificate(_))
        ));
        #[cfg(feature = "p2")]
        assert!(matches!(
            crate::default_hooks().p2_error_from_tls(&error),
            crate::p2::bindings::http::types::ErrorCode::TlsCertificateError
        ));
        #[cfg(feature = "p3")]
        assert!(matches!(
            crate::default_hooks().p3_error_from_tls(&error),
            crate::p3::bindings::http::types::ErrorCode::TlsCertificateError
        ));
    }

    async fn response_body(body: WasiBody) -> (ResponseBody<WasiBody>, Arc<Semaphore>) {
        let semaphore = Arc::new(Semaphore::new(2));
        let host = semaphore.clone().acquire_owned().await.unwrap();
        let global = semaphore.clone().acquire_owned().await.unwrap();
        let timeout =
            crate::default_send_request::ResponseTimeout::new(Duration::from_millis(1), false);
        (
            ResponseBody {
                body,
                timeout,
                permits: Some((host, global)),
                ended: false,
            },
            semaphore,
        )
    }

    #[tokio::test]
    async fn permits_release_on_every_terminal_and_drop() {
        let bodies: Vec<WasiBody> = vec![
            Empty::new().map_err(Error::from).boxed_unsync(),
            StreamBody::new(stream::iter([Ok(Frame::trailers(http::HeaderMap::new()))]))
                .boxed_unsync(),
            StreamBody::new(stream::iter([Err(Error::HttpProtocolError)])).boxed_unsync(),
            StreamBody::new(stream::pending::<Result<Frame<Bytes>, Error>>()).boxed_unsync(),
        ];
        for body in bodies {
            let (mut body, semaphore) = response_body(body).await;
            assert_eq!(semaphore.available_permits(), 0);
            let _ = body.frame().await;
            assert_eq!(semaphore.available_permits(), 2);
            assert!(body.frame().await.is_none());
        }
        let (body, semaphore) = response_body(
            StreamBody::new(stream::pending::<Result<Frame<Bytes>, Error>>()).boxed_unsync(),
        )
        .await;
        assert_eq!(semaphore.available_permits(), 0);
        drop(body);
        assert_eq!(semaphore.available_permits(), 2);
    }

    #[tokio::test]
    async fn failed_send_preserves_original_body_error() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let pool = HttpConnectionPool::new(Default::default());
        let body = StreamBody::new(stream::iter([Err(Error::HttpRequestBodySize(Some(17)))]))
            .boxed_unsync();
        let request = http::Request::builder()
            .method("POST")
            .uri(format!("http://{address}/"))
            .body(body)
            .unwrap();
        let result = tokio::time::timeout(Duration::from_secs(3), pool.send_request(request, None))
            .await
            .unwrap();
        assert!(matches!(result, Err(Error::HttpRequestBodySize(Some(17)))));
        assert_eq!(
            pool.global.available_permits(),
            pool.config.max_total_connections
        );
    }

    #[tokio::test]
    async fn transmission_waits_and_keeps_structured_error() {
        let error = Arc::new(Mutex::new(None));
        let (done, mut receiver) = oneshot::channel();
        let (tx, rx) = futures::channel::mpsc::unbounded();
        let mut body = RequestBody {
            inner: StreamBody::new(rx).boxed_unsync(),
            error: error.clone(),
            done: Some(done),
        };
        assert!(body.frame().now_or_never().is_none());
        assert!(receiver.try_recv().is_err());
        tx.unbounded_send(Err(Error::HttpRequestBodySize(Some(17))))
            .unwrap();
        assert!(matches!(body.frame().await, Some(Err(_))));
        assert!(receiver.await.is_ok());
        assert!(matches!(
            error.lock().unwrap().take(),
            Some(Error::HttpRequestBodySize(Some(17)))
        ));
    }
}
