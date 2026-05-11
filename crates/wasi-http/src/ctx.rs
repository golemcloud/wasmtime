/// Default maximum size for the contents of a fields resource.
///
/// Typically, HTTP proxies limit headers to 8k. This number is higher than that
/// because it not only includes the wire-size of headers but it additionally
/// includes factors for the in-memory representation of `HeaderMap`. This is in
/// theory high enough that no one runs into it but low enough such that a
/// completely full `HeaderMap` doesn't break the bank in terms of memory
/// consumption.
const DEFAULT_FIELD_SIZE_LIMIT: usize = 128 * 1024;

/// Capture the state necessary for use in the wasi-http API implementation.
#[derive(Debug, Clone)]
pub struct WasiHttpCtx {
    /// Maximum size for the contents of a fields resource.
    pub field_size_limit: usize,
    /// Optional connection pool used for outgoing HTTP requests.
    #[cfg(feature = "default-send-request")]
    pub connection_pool: Option<crate::p2::HttpConnectionPool>,
}

impl WasiHttpCtx {
    /// Create a new context.
    pub fn new() -> Self {
        Self {
            field_size_limit: DEFAULT_FIELD_SIZE_LIMIT,
            #[cfg(feature = "default-send-request")]
            connection_pool: None,
        }
    }

    /// Set the maximum size for any fields resources created by this context.
    ///
    /// The limit specified here is roughly a byte limit for the size of the
    /// in-memory representation of headers. This means that the limit needs to
    /// be larger than the literal representation of headers on the wire to
    /// account for in-memory Rust-side data structures representing the header
    /// names/values/etc.
    pub fn set_field_size_limit(&mut self, limit: usize) {
        self.field_size_limit = limit;
    }
}

impl Default for WasiHttpCtx {
    fn default() -> Self {
        Self::new()
    }
}
