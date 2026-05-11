//! Raw bindings to the `wasi:http` package.

#[expect(missing_docs, reason = "bindgen-generated code")]
mod generated {
    use crate::p2::body;
    use crate::p2::types;

    wasmtime::component::bindgen!({
        path: "wit",
        world: "wasi:http/proxy",
        imports: {
            "wasi:http/outgoing-handler.handle": async | tracing | trappable,
            "wasi:http/types.[method]future-incoming-response.get": async | tracing | trappable,
            "wasi:http/types.[method]future-trailers.get": async | tracing | trappable,
            "wasi:http/types.[static]incoming-body.finish": async | tracing | trappable,
            "wasi:http/types.[drop]incoming-body": async | tracing | trappable,
            "wasi:http/types.[drop]incoming-response": async | tracing | trappable,
            "wasi:http/types.[drop]future-incoming-response": async | tracing | trappable,
            default: tracing | trappable,
        },
        exports: { default: async },
        require_store_data_send: true,
        with: {
            // Upstream package dependencies
            "wasi:io": wasmtime_wasi::p2::bindings::io,

            // Configure all WIT http resources to be defined types in this
            // crate to use the `ResourceTable` helper methods.
            "wasi:http/types.outgoing-body": body::HostOutgoingBody,
            "wasi:http/types.future-incoming-response": types::HostFutureIncomingResponse,
            "wasi:http/types.outgoing-response": types::HostOutgoingResponse,
            "wasi:http/types.future-trailers": body::HostFutureTrailers,
            "wasi:http/types.incoming-body": body::HostIncomingBody,
            "wasi:http/types.incoming-response": types::HostIncomingResponse,
            "wasi:http/types.response-outparam": types::HostResponseOutparam,
            "wasi:http/types.outgoing-request": types::HostOutgoingRequest,
            "wasi:http/types.incoming-request": types::HostIncomingRequest,
            "wasi:http/types.fields": crate::FieldMap,
            "wasi:http/types.request-options": types::HostRequestOptions,
        },
        trappable_error_type: {
            "wasi:http/types.error-code" => crate::p2::HttpError,
            "wasi:http/types.header-error" => crate::p2::HeaderError,
        },
    });
}

pub use self::generated::wasi::*;

/// Raw bindings to the `wasi:http/proxy` exports.
pub use self::generated::exports;

/// Bindings to the `wasi:http/proxy` world.
pub use self::generated::{LinkOptions, Proxy, ProxyIndices, ProxyPre};
