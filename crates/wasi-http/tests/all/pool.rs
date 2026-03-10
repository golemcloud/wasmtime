//! Tests for HTTP connection pooling and error classification.
//!
//! These are host-level tests that exercise the Rust APIs directly (no Wasm
//! components). They verify:
//! - Connection pool construction and cloneability
//! - Pooled vs non-pooled request dispatch
//! - Error classification for various failure modes

use http_body_util::{BodyExt, Empty};
use hyper::body::Bytes;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpListener;
use wasmtime::Result;
use wasmtime_wasi_http::bindings::http::types::ErrorCode;
use wasmtime_wasi_http::body::HyperOutgoingBody;
use wasmtime_wasi_http::io::TokioIo;
use wasmtime_wasi_http::types::{
    default_send_request, default_send_request_with_pool, HostFutureIncomingResponse,
    OutgoingRequestConfig,
};
use wasmtime_wasi_http::{HttpConnectionPool, HttpConnectionPoolConfig};

/// Build a minimal outgoing request body (empty).
fn empty_body() -> HyperOutgoingBody {
    Empty::<Bytes>::new()
        .map_err(|_| unreachable!("Infallible error"))
        .boxed_unsync()
}

/// Standard short timeouts for tests.
fn fast_config(use_tls: bool) -> OutgoingRequestConfig {
    OutgoingRequestConfig {
        use_tls,
        connect_timeout: Duration::from_millis(500),
        first_byte_timeout: Duration::from_secs(5),
        between_bytes_timeout: Duration::from_secs(5),
    }
}

/// Helper: resolve a `HostFutureIncomingResponse` to its inner result.
async fn resolve(
    mut future: HostFutureIncomingResponse,
) -> wasmtime::Result<std::result::Result<wasmtime_wasi_http::types::IncomingResponse, ErrorCode>> {
    use wasmtime_wasi::p2::Pollable;
    future.ready().await;
    future.unwrap_ready()
}

/// Start a local HTTP/1.1 server that echoes the request method and URI back.
/// Returns the server address and a `tokio::task::JoinHandle` for cleanup.
async fn start_echo_server() -> Result<(SocketAddr, tokio::task::JoinHandle<()>)> {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
    let addr = listener.local_addr()?;

    let handle = tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                break;
            };
            let io = TokioIo::new(stream);
            tokio::spawn(async move {
                let _ = http1::Builder::new()
                    .keep_alive(true)
                    .serve_connection(
                        io,
                        service_fn(|req: Request<hyper::body::Incoming>| async move {
                            let method = req.method().to_string();
                            let uri = req.uri().to_string();
                            Response::builder()
                                .status(200)
                                .header("x-method", method)
                                .header("x-uri", uri)
                                .body(
                                    Empty::<Bytes>::new()
                                        .map_err(|_| unreachable!())
                                        .boxed(),
                                )
                        }),
                    )
                    .await;
            });
        }
    });

    Ok((addr, handle))
}

/// Start a local HTTP/1.1 server that counts TCP connections.
/// Returns the server address, connection counter, and a `tokio::task::JoinHandle`.
async fn start_counting_echo_server() -> Result<(SocketAddr, Arc<AtomicUsize>, tokio::task::JoinHandle<()>)> {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
    let addr = listener.local_addr()?;
    let conn_count = Arc::new(AtomicUsize::new(0));
    let conn_count_clone = conn_count.clone();

    let handle = tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                break;
            };
            conn_count_clone.fetch_add(1, Ordering::SeqCst);
            let io = TokioIo::new(stream);
            tokio::spawn(async move {
                let _ = http1::Builder::new()
                    .keep_alive(true)
                    .serve_connection(
                        io,
                        service_fn(|req: Request<hyper::body::Incoming>| async move {
                            let method = req.method().to_string();
                            let uri = req.uri().to_string();
                            Response::builder()
                                .status(200)
                                .header("x-method", method)
                                .header("x-uri", uri)
                                .body(
                                    Empty::<Bytes>::new()
                                        .map_err(|_| unreachable!())
                                        .boxed(),
                                )
                        }),
                    )
                    .await;
            });
        }
    });

    Ok((addr, conn_count, handle))
}

// ---------------------------------------------------------------------------
// Pool construction
// ---------------------------------------------------------------------------

#[test_log::test(tokio::test)]
async fn pool_default_config_constructs() {
    let pool = HttpConnectionPool::new(HttpConnectionPoolConfig::default());
    // Pool should be cloneable (Arc-based internally).
    let _pool2 = pool.clone();
}

#[test_log::test(tokio::test)]
async fn pool_custom_config_constructs() {
    let config = HttpConnectionPoolConfig {
        max_idle_per_host: 2,
        idle_timeout: Duration::from_secs(10),
        connect_timeout: Duration::from_secs(1),
    };
    let pool = HttpConnectionPool::new(config);
    let _pool2 = pool.clone();
}

// ---------------------------------------------------------------------------
// Successful requests — non-pooled
// ---------------------------------------------------------------------------

#[test_log::test(tokio::test(flavor = "multi_thread"))]
async fn non_pooled_request_succeeds() -> Result<()> {
    let (addr, server) = start_echo_server().await?;

    let request = hyper::Request::builder()
        .method(http::Method::GET)
        .uri(format!("http://localhost:{}/hello", addr.port()))
        .body(empty_body())
        .unwrap();

    let future = default_send_request(request, fast_config(false));
    let resp = resolve(future).await?.expect("request should succeed");
    assert_eq!(resp.resp.status(), StatusCode::OK);
    assert_eq!(
        resp.resp.headers().get("x-uri").unwrap().to_str().unwrap(),
        "/hello"
    );

    server.abort();
    Ok(())
}

// ---------------------------------------------------------------------------
// Successful requests — pooled
// ---------------------------------------------------------------------------

#[test_log::test(tokio::test(flavor = "multi_thread"))]
async fn pooled_request_succeeds() -> Result<()> {
    let (addr, server) = start_echo_server().await?;
    let pool = HttpConnectionPool::new(HttpConnectionPoolConfig::default());

    let request = hyper::Request::builder()
        .method(http::Method::GET)
        .uri(format!("http://localhost:{}/pooled", addr.port()))
        .body(empty_body())
        .unwrap();

    let future = default_send_request_with_pool(request, fast_config(false), Some(pool.clone()));
    let resp = resolve(future).await?.expect("request should succeed");
    assert_eq!(resp.resp.status(), StatusCode::OK);
    assert_eq!(
        resp.resp.headers().get("x-uri").unwrap().to_str().unwrap(),
        "/pooled"
    );

    server.abort();
    Ok(())
}

#[test_log::test(tokio::test(flavor = "multi_thread"))]
async fn pooled_connection_reuse() -> Result<()> {
    // Verify that two sequential requests through the same pool reuse the
    // same TCP connection (keep-alive). We use a server-side connection
    // counter to assert this.
    let (addr, conn_count, server) = start_counting_echo_server().await?;
    let pool = HttpConnectionPool::new(HttpConnectionPoolConfig::default());

    for path in &["/first", "/second"] {
        let request = hyper::Request::builder()
            .method(http::Method::GET)
            .uri(format!("http://localhost:{}{}", addr.port(), path))
            .body(empty_body())
            .unwrap();

        let future =
            default_send_request_with_pool(request, fast_config(false), Some(pool.clone()));
        let resp = resolve(future).await?.expect("request should succeed");
        assert_eq!(resp.resp.status(), StatusCode::OK);
        assert_eq!(
            resp.resp.headers().get("x-uri").unwrap().to_str().unwrap(),
            *path
        );
        // Drain the response body so the connection is returned to the pool
        // for reuse (HTTP/1.1 keep-alive requires the body to be consumed).
        let _ = resp.resp.into_body().collect().await;
    }

    // With connection pooling and keep-alive, both requests should have
    // used the same TCP connection.
    let connections = conn_count.load(Ordering::SeqCst);
    assert_eq!(
        connections, 1,
        "expected 1 TCP connection (reuse), got {connections}"
    );

    server.abort();
    Ok(())
}

#[test_log::test(tokio::test(flavor = "multi_thread"))]
async fn pool_none_falls_back_to_non_pooled() -> Result<()> {
    // Passing None for the pool should still work (falls back to direct connect).
    let (addr, server) = start_echo_server().await?;

    let request = hyper::Request::builder()
        .method(http::Method::GET)
        .uri(format!("http://localhost:{}/fallback", addr.port()))
        .body(empty_body())
        .unwrap();

    let future = default_send_request_with_pool(request, fast_config(false), None);
    let resp = resolve(future).await?.expect("request should succeed");
    assert_eq!(resp.resp.status(), StatusCode::OK);

    server.abort();
    Ok(())
}

// ---------------------------------------------------------------------------
// Error classification — connection refused (non-pooled)
// ---------------------------------------------------------------------------

#[test_log::test(tokio::test(flavor = "multi_thread"))]
async fn non_pooled_connection_refused() -> Result<()> {
    // Bind a port, get the addr, then drop the listener so nothing is listening.
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
    let addr = listener.local_addr()?;
    drop(listener);

    let request = hyper::Request::builder()
        .method(http::Method::GET)
        .uri(format!("http://localhost:{}/refused", addr.port()))
        .body(empty_body())
        .unwrap();

    let future = default_send_request(request, fast_config(false));
    let result = resolve(future).await?;
    match result {
        Err(ErrorCode::ConnectionRefused) => {} // expected
        other => panic!("expected ConnectionRefused, got: {other:?}"),
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Error classification — connection refused (pooled)
// ---------------------------------------------------------------------------

#[test_log::test(tokio::test(flavor = "multi_thread"))]
async fn pooled_connection_refused() -> Result<()> {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
    let addr = listener.local_addr()?;
    drop(listener);

    let pool = HttpConnectionPool::new(HttpConnectionPoolConfig::default());

    let request = hyper::Request::builder()
        .method(http::Method::GET)
        .uri(format!("http://localhost:{}/refused", addr.port()))
        .body(empty_body())
        .unwrap();

    let future = default_send_request_with_pool(request, fast_config(false), Some(pool));
    let result = resolve(future).await?;
    match result {
        Err(ErrorCode::ConnectionRefused) => {}    // expected
        Err(ErrorCode::DestinationUnavailable) => {} // may occur on some platforms
        other => panic!("expected ConnectionRefused or DestinationUnavailable, got: {other:?}"),
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Error classification — connection timeout (non-pooled)
// ---------------------------------------------------------------------------

#[test_log::test(tokio::test(flavor = "multi_thread"))]
async fn non_pooled_connection_timeout() -> Result<()> {
    // Bind but never accept — the TCP handshake will eventually time out.
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
    let addr = listener.local_addr()?;
    // Keep listener alive but never accept.

    let request = hyper::Request::builder()
        .method(http::Method::GET)
        .uri(format!("http://localhost:{}/timeout", addr.port()))
        .body(empty_body())
        .unwrap();

    let config = OutgoingRequestConfig {
        use_tls: false,
        // Very short connect timeout to trigger quickly.
        // Note: on localhost, TCP connect to a bound-but-not-accepted port
        // may actually succeed at the TCP level (the kernel accepts the SYN
        // into the backlog). So this test might see a *first_byte_timeout*
        // instead. We use a non-routable address to ensure a real timeout.
        connect_timeout: Duration::from_millis(200),
        first_byte_timeout: Duration::from_millis(200),
        between_bytes_timeout: Duration::from_secs(5),
    };

    // Use a non-routable IP to guarantee a connect timeout
    let request_timeout = hyper::Request::builder()
        .method(http::Method::GET)
        .uri("http://192.0.2.1:12345/timeout") // TEST-NET-1, non-routable
        .body(empty_body())
        .unwrap();

    let future = default_send_request(request_timeout, config);
    let result = resolve(future).await?;
    match result {
        Err(ErrorCode::ConnectionTimeout) => {}
        // On some systems, non-routable addresses may be immediately rejected
        Err(ErrorCode::ConnectionRefused) => {}
        Err(ErrorCode::DestinationUnavailable) => {}
        other => panic!("expected ConnectionTimeout, ConnectionRefused, or DestinationUnavailable, got: {other:?}"),
    }

    drop(listener);
    drop(request);
    Ok(())
}

// ---------------------------------------------------------------------------
// Error classification — connection timeout (pooled)
// ---------------------------------------------------------------------------

#[test_log::test(tokio::test(flavor = "multi_thread"))]
async fn pooled_connection_timeout() -> Result<()> {
    let pool = HttpConnectionPool::new(HttpConnectionPoolConfig {
        connect_timeout: Duration::from_millis(200),
        ..Default::default()
    });

    let request = hyper::Request::builder()
        .method(http::Method::GET)
        .uri("http://192.0.2.1:12345/timeout") // TEST-NET-1, non-routable
        .body(empty_body())
        .unwrap();

    let config = OutgoingRequestConfig {
        use_tls: false,
        connect_timeout: Duration::from_millis(200),
        first_byte_timeout: Duration::from_millis(500),
        between_bytes_timeout: Duration::from_secs(5),
    };

    let future = default_send_request_with_pool(request, config, Some(pool));
    let result = resolve(future).await?;
    match result {
        Err(ErrorCode::ConnectionTimeout) => {}
        // On some systems or depending on hyper-util wrapping
        Err(ErrorCode::DestinationUnavailable) => {}
        Err(ErrorCode::ConnectionRefused) => {}
        other => panic!("expected ConnectionTimeout, DestinationUnavailable, or ConnectionRefused, got: {other:?}"),
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Error classification — DNS / invalid host
// ---------------------------------------------------------------------------

#[test_log::test(tokio::test(flavor = "multi_thread"))]
async fn non_pooled_dns_error() -> Result<()> {
    let request = hyper::Request::builder()
        .method(http::Method::GET)
        .uri("http://this-host-does-not-exist.invalid:8080/dns")
        .body(empty_body())
        .unwrap();

    let future = default_send_request(request, fast_config(false));
    let result = resolve(future).await?;
    match result {
        Err(ErrorCode::DnsError(_)) => {}         // expected
        Err(ErrorCode::ConnectionRefused) => {}    // also acceptable on some platforms
        other => panic!("expected DnsError or ConnectionRefused, got: {other:?}"),
    }

    Ok(())
}

#[test_log::test(tokio::test(flavor = "multi_thread"))]
async fn pooled_dns_error() -> Result<()> {
    let pool = HttpConnectionPool::new(HttpConnectionPoolConfig::default());

    let request = hyper::Request::builder()
        .method(http::Method::GET)
        .uri("http://this-host-does-not-exist.invalid:8080/dns")
        .body(empty_body())
        .unwrap();

    let future = default_send_request_with_pool(request, fast_config(false), Some(pool));
    let result = resolve(future).await?;
    match result {
        Err(ErrorCode::DnsError(_)) => {}            // expected
        Err(ErrorCode::DestinationUnavailable) => {}  // pooled path may report this
        Err(ErrorCode::ConnectionRefused) => {}       // also acceptable on some platforms
        other => panic!(
            "expected DnsError, DestinationUnavailable, or ConnectionRefused, got: {other:?}"
        ),
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Error classification — TLS to a plain HTTP server (non-pooled)
// ---------------------------------------------------------------------------

#[test_log::test(tokio::test(flavor = "multi_thread"))]
async fn non_pooled_tls_to_plain_http() -> Result<()> {
    let (addr, server) = start_echo_server().await?;

    // Try to connect with TLS to a plain HTTP server — should fail with a TLS error.
    let request = hyper::Request::builder()
        .method(http::Method::GET)
        .uri(format!("https://localhost:{}/tls-fail", addr.port()))
        .body(empty_body())
        .unwrap();

    let future = default_send_request(request, fast_config(true));
    let result = resolve(future).await?;
    match result {
        Err(ErrorCode::TlsProtocolError) => {}
        Err(ErrorCode::TlsCertificateError) => {}
        Err(ErrorCode::TlsAlertReceived(_)) => {}
        Err(ErrorCode::ConnectionTerminated) => {}
        other => panic!("expected a TLS error variant, got: {other:?}"),
    }

    server.abort();
    Ok(())
}

// ---------------------------------------------------------------------------
// Error classification — TLS to a plain HTTP server (pooled)
// ---------------------------------------------------------------------------

#[test_log::test(tokio::test(flavor = "multi_thread"))]
async fn pooled_tls_to_plain_http() -> Result<()> {
    let (addr, server) = start_echo_server().await?;
    let pool = HttpConnectionPool::new(HttpConnectionPoolConfig::default());

    let request = hyper::Request::builder()
        .method(http::Method::GET)
        .uri(format!("https://localhost:{}/tls-fail", addr.port()))
        .body(empty_body())
        .unwrap();

    let future = default_send_request_with_pool(request, fast_config(true), Some(pool));
    let result = resolve(future).await?;
    match result {
        Err(ErrorCode::TlsProtocolError) => {}
        Err(ErrorCode::TlsCertificateError) => {}
        Err(ErrorCode::TlsAlertReceived(_)) => {}
        Err(ErrorCode::ConnectionTerminated) => {}
        Err(ErrorCode::DestinationUnavailable) => {}
        other => panic!("expected a TLS error variant or DestinationUnavailable, got: {other:?}"),
    }

    server.abort();
    Ok(())
}

// ---------------------------------------------------------------------------
// Error classification — missing URI authority
// ---------------------------------------------------------------------------

#[test_log::test(tokio::test(flavor = "multi_thread"))]
async fn non_pooled_missing_authority() -> Result<()> {
    let request = hyper::Request::builder()
        .method(http::Method::GET)
        .uri("/no-authority")
        .body(empty_body())
        .unwrap();

    let future = default_send_request(request, fast_config(false));
    let result = resolve(future).await?;
    match result {
        Err(ErrorCode::HttpRequestUriInvalid) => {} // expected
        other => panic!("expected HttpRequestUriInvalid, got: {other:?}"),
    }

    Ok(())
}

#[test_log::test(tokio::test(flavor = "multi_thread"))]
async fn pooled_missing_authority() -> Result<()> {
    let pool = HttpConnectionPool::new(HttpConnectionPoolConfig::default());

    let request = hyper::Request::builder()
        .method(http::Method::GET)
        .uri("/no-authority")
        .body(empty_body())
        .unwrap();

    let future = default_send_request_with_pool(request, fast_config(false), Some(pool));
    let result = resolve(future).await?;
    match result {
        // The pooled path requires an absolute URI (scheme + authority).
        // A relative URI like "/no-authority" is rejected as invalid.
        Err(ErrorCode::HttpRequestUriInvalid) => {}
        other => panic!("expected HttpRequestUriInvalid, got: {other:?}"),
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// WasiHttpCtx pool wiring
// ---------------------------------------------------------------------------

#[test_log::test(tokio::test)]
async fn wasi_http_ctx_pool_default_is_none() {
    let ctx = wasmtime_wasi_http::WasiHttpCtx::new();
    assert!(ctx.connection_pool.is_none());
}

#[test_log::test(tokio::test)]
async fn wasi_http_ctx_pool_can_be_set() {
    let mut ctx = wasmtime_wasi_http::WasiHttpCtx::new();
    let pool = HttpConnectionPool::new(HttpConnectionPoolConfig::default());
    ctx.connection_pool = Some(pool);
    assert!(ctx.connection_pool.is_some());
}
