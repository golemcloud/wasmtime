use crate::p3::bindings::http::types::{self, ErrorCode, Method, Scheme};
use crate::{Error, WasiHttpCtxView, WasiHttpHooks};
use core::convert::Infallible;
use core::error::Error as _;
use std::io::ErrorKind;

impl From<Infallible> for ErrorCode {
    fn from(x: Infallible) -> Self {
        match x {}
    }
}

impl From<ErrorCode> for Error {
    fn from(e: ErrorCode) -> Self {
        match e {
            ErrorCode::DnsTimeout => Self::DnsTimeout,
            ErrorCode::DnsError(payload) => Self::DnsError {
                rcode: payload.rcode,
                info_code: payload.info_code,
            },
            ErrorCode::DestinationNotFound => Self::DestinationNotFound,
            ErrorCode::DestinationUnavailable => Self::DestinationUnavailable,
            ErrorCode::DestinationIpProhibited => Self::DestinationIpProhibited,
            ErrorCode::DestinationIpUnroutable => Self::DestinationIpUnroutable,
            ErrorCode::ConnectionRefused => Self::ConnectionRefused,
            ErrorCode::ConnectionTerminated => Self::ConnectionTerminated,
            ErrorCode::ConnectionTimeout => Self::ConnectionTimeout,
            ErrorCode::ConnectionReadTimeout => Self::ConnectionReadTimeout,
            ErrorCode::ConnectionWriteTimeout => Self::ConnectionWriteTimeout,
            ErrorCode::ConnectionLimitReached => Self::ConnectionLimitReached,
            ErrorCode::TlsProtocolError => Self::TlsProtocolError,
            ErrorCode::TlsCertificateError => Self::TlsCertificateError,
            ErrorCode::TlsAlertReceived(payload) => Self::TlsAlertReceived {
                alert_id: payload.alert_id,
                alert_message: payload.alert_message,
            },
            ErrorCode::HttpRequestDenied => Self::HttpRequestDenied,
            ErrorCode::HttpRequestLengthRequired => Self::HttpRequestLengthRequired,
            ErrorCode::HttpRequestBodySize(payload) => Self::HttpRequestBodySize(payload),
            ErrorCode::HttpRequestMethodInvalid => Self::HttpRequestMethodInvalid,
            ErrorCode::HttpRequestUriInvalid => Self::HttpRequestUriInvalid,
            ErrorCode::HttpRequestUriTooLong => Self::HttpRequestUriTooLong,
            ErrorCode::HttpRequestHeaderSectionSize(payload) => {
                Self::HttpRequestHeaderSectionSize(payload)
            }
            ErrorCode::HttpRequestHeaderSize(payload) => {
                Self::HttpRequestHeaderSize(payload.map(|p| crate::FieldSizePayload {
                    field_name: p.field_name,
                    field_size: p.field_size,
                }))
            }
            ErrorCode::HttpRequestTrailerSectionSize(payload) => {
                Self::HttpRequestTrailerSectionSize(payload)
            }
            ErrorCode::HttpRequestTrailerSize(payload) => Self::HttpRequestTrailerSize {
                field_name: payload.field_name,
                field_size: payload.field_size,
            },
            ErrorCode::HttpResponseIncomplete => Self::HttpResponseIncomplete,
            ErrorCode::HttpResponseHeaderSectionSize(payload) => {
                Self::HttpResponseHeaderSectionSize(payload)
            }
            ErrorCode::HttpResponseHeaderSize(payload) => Self::HttpResponseHeaderSize {
                field_name: payload.field_name,
                field_size: payload.field_size,
            },
            ErrorCode::HttpResponseBodySize(payload) => Self::HttpResponseBodySize(payload),
            ErrorCode::HttpResponseTrailerSectionSize(payload) => {
                Self::HttpResponseTrailerSectionSize(payload)
            }
            ErrorCode::HttpResponseTrailerSize(payload) => Self::HttpResponseTrailerSize {
                field_name: payload.field_name,
                field_size: payload.field_size,
            },
            ErrorCode::HttpResponseTransferCoding(payload) => {
                Self::HttpResponseTransferCoding(payload)
            }
            ErrorCode::HttpResponseContentCoding(payload) => {
                Self::HttpResponseContentCoding(payload)
            }
            ErrorCode::HttpResponseTimeout => Self::HttpResponseTimeout,
            ErrorCode::HttpUpgradeFailed => Self::HttpUpgradeFailed,
            ErrorCode::HttpProtocolError => Self::HttpProtocolError,
            ErrorCode::LoopDetected => Self::LoopDetected,
            ErrorCode::ConfigurationError => Self::ConfigurationError,
            ErrorCode::InternalError(payload) => Self::InternalError(payload),
        }
    }
}

impl WasiHttpCtxView<'_> {
    /// Convert a transport error with this view's P3 error-classification hooks.
    pub fn error_to_p3(&mut self, e: &Error) -> ErrorCode {
        error_to_p3(self.hooks, e)
    }
}

/// Convert a transport error using P3 error-classification hooks without a store.
pub fn error_to_p3(hooks: &mut dyn WasiHttpHooks, e: &Error) -> ErrorCode {
    match e {
        Error::Hyper(err) => {
            // If there's a source, we might be able to extract a wasi-http
            // error from it.
            if let Some(cause) = err.source() {
                if let Some(err) = cause.downcast_ref::<ErrorCode>() {
                    return err.clone();
                }
                if let Some(err) = cause.downcast_ref::<Error>() {
                    return error_to_p3(hooks, err);
                }
            }

            hooks.p3_error_from_hyper(err)
        }
        Error::Connect(err) => {
            if err.kind() == ErrorKind::AddrNotAvailable {
                return ErrorCode::DnsError(types::DnsErrorPayload {
                    rcode: Some("address not available".to_string()),
                    info_code: None,
                });
            }

            if err
                .to_string()
                .starts_with("failed to lookup address information")
            {
                return ErrorCode::DnsError(types::DnsErrorPayload {
                    rcode: Some("address not available".to_string()),
                    info_code: None,
                });
            }

            hooks.p3_error_from_connect(err)
        }
        Error::Tls(err) => hooks.p3_error_from_tls(err),
        #[cfg(feature = "default-send-request")]
        Error::InvalidDnsNameError(err) => hooks.p3_error_from_dns(err),
        Error::DnsTimeout => ErrorCode::DnsTimeout,
        Error::DnsError { rcode, info_code } => ErrorCode::DnsError(types::DnsErrorPayload {
            rcode: rcode.clone(),
            info_code: *info_code,
        }),
        Error::DestinationNotFound => ErrorCode::DestinationNotFound,
        Error::DestinationUnavailable => ErrorCode::DestinationUnavailable,
        Error::DestinationIpProhibited => ErrorCode::DestinationIpProhibited,
        Error::DestinationIpUnroutable => ErrorCode::DestinationIpUnroutable,
        Error::ConnectionRefused => ErrorCode::ConnectionRefused,
        Error::ConnectionTerminated => ErrorCode::ConnectionTerminated,
        Error::ConnectionTimeout => ErrorCode::ConnectionTimeout,
        Error::ConnectionReadTimeout => ErrorCode::ConnectionReadTimeout,
        Error::ConnectionWriteTimeout => ErrorCode::ConnectionWriteTimeout,
        Error::ConnectionLimitReached => ErrorCode::ConnectionLimitReached,
        Error::TlsProtocolError => ErrorCode::TlsProtocolError,
        Error::TlsCertificateError => ErrorCode::TlsCertificateError,
        Error::TlsAlertReceived {
            alert_id,
            alert_message,
        } => ErrorCode::TlsAlertReceived(types::TlsAlertReceivedPayload {
            alert_id: *alert_id,
            alert_message: alert_message.clone(),
        }),
        Error::HttpRequestDenied => ErrorCode::HttpRequestDenied,
        Error::HttpRequestLengthRequired => ErrorCode::HttpRequestLengthRequired,
        Error::HttpRequestBodySize(payload) => ErrorCode::HttpRequestBodySize(*payload),
        Error::HttpRequestMethodInvalid => ErrorCode::HttpRequestMethodInvalid,
        Error::HttpRequestUriInvalid => ErrorCode::HttpRequestUriInvalid,
        Error::HttpRequestUriTooLong => ErrorCode::HttpRequestUriTooLong,
        Error::HttpRequestHeaderSectionSize(payload) => {
            ErrorCode::HttpRequestHeaderSectionSize(*payload)
        }
        Error::HttpRequestHeaderSize(payload) => {
            ErrorCode::HttpRequestHeaderSize(payload.as_ref().map(|p| types::FieldSizePayload {
                field_name: p.field_name.clone(),
                field_size: p.field_size,
            }))
        }
        Error::HttpRequestTrailerSectionSize(payload) => {
            ErrorCode::HttpRequestTrailerSectionSize(*payload)
        }
        Error::HttpRequestTrailerSize {
            field_name,
            field_size,
        } => ErrorCode::HttpRequestTrailerSize(types::FieldSizePayload {
            field_name: field_name.clone(),
            field_size: *field_size,
        }),
        Error::HttpResponseIncomplete => ErrorCode::HttpResponseIncomplete,
        Error::HttpResponseHeaderSectionSize(payload) => {
            ErrorCode::HttpResponseHeaderSectionSize(*payload)
        }
        Error::HttpResponseHeaderSize {
            field_name,
            field_size,
        } => ErrorCode::HttpResponseHeaderSize(types::FieldSizePayload {
            field_name: field_name.clone(),
            field_size: *field_size,
        }),
        Error::HttpResponseBodySize(payload) => ErrorCode::HttpResponseBodySize(*payload),
        Error::HttpResponseTrailerSectionSize(payload) => {
            ErrorCode::HttpResponseTrailerSectionSize(*payload)
        }
        Error::HttpResponseTrailerSize {
            field_name,
            field_size,
        } => ErrorCode::HttpResponseTrailerSize(types::FieldSizePayload {
            field_name: field_name.clone(),
            field_size: *field_size,
        }),
        Error::HttpResponseTransferCoding(payload) => {
            ErrorCode::HttpResponseTransferCoding(payload.clone())
        }
        Error::HttpResponseContentCoding(payload) => {
            ErrorCode::HttpResponseContentCoding(payload.clone())
        }
        Error::HttpResponseTimeout => ErrorCode::HttpResponseTimeout,
        Error::HttpUpgradeFailed => ErrorCode::HttpUpgradeFailed,
        Error::HttpProtocolError => ErrorCode::HttpProtocolError,
        Error::LoopDetected => ErrorCode::LoopDetected,
        Error::ConfigurationError => ErrorCode::ConfigurationError,
        Error::InternalError(payload) => ErrorCode::InternalError(payload.clone()),
    }
}

impl From<http::Method> for Method {
    fn from(method: http::Method) -> Self {
        Self::from(&method)
    }
}

impl From<&http::Method> for Method {
    fn from(method: &http::Method) -> Self {
        if method == http::Method::GET {
            Self::Get
        } else if method == http::Method::HEAD {
            Self::Head
        } else if method == http::Method::POST {
            Self::Post
        } else if method == http::Method::PUT {
            Self::Put
        } else if method == http::Method::DELETE {
            Self::Delete
        } else if method == http::Method::CONNECT {
            Self::Connect
        } else if method == http::Method::OPTIONS {
            Self::Options
        } else if method == http::Method::TRACE {
            Self::Trace
        } else if method == http::Method::PATCH {
            Self::Patch
        } else {
            Self::Other(method.as_str().into())
        }
    }
}

impl TryFrom<Method> for http::Method {
    type Error = http::method::InvalidMethod;

    fn try_from(method: Method) -> Result<Self, Self::Error> {
        Self::try_from(&method)
    }
}

impl TryFrom<&Method> for http::Method {
    type Error = http::method::InvalidMethod;

    fn try_from(method: &Method) -> Result<Self, Self::Error> {
        match method {
            Method::Get => Ok(Self::GET),
            Method::Head => Ok(Self::HEAD),
            Method::Post => Ok(Self::POST),
            Method::Put => Ok(Self::PUT),
            Method::Delete => Ok(Self::DELETE),
            Method::Connect => Ok(Self::CONNECT),
            Method::Options => Ok(Self::OPTIONS),
            Method::Trace => Ok(Self::TRACE),
            Method::Patch => Ok(Self::PATCH),
            Method::Other(s) => s.parse(),
        }
    }
}

impl From<http::uri::Scheme> for Scheme {
    fn from(scheme: http::uri::Scheme) -> Self {
        Self::from(&scheme)
    }
}

impl From<&http::uri::Scheme> for Scheme {
    fn from(scheme: &http::uri::Scheme) -> Self {
        if *scheme == http::uri::Scheme::HTTP {
            Self::Http
        } else if *scheme == http::uri::Scheme::HTTPS {
            Self::Https
        } else {
            Self::Other(scheme.as_str().into())
        }
    }
}

impl TryFrom<Scheme> for http::uri::Scheme {
    type Error = http::uri::InvalidUri;

    fn try_from(scheme: Scheme) -> Result<Self, Self::Error> {
        Self::try_from(&scheme)
    }
}

impl TryFrom<&Scheme> for http::uri::Scheme {
    type Error = http::uri::InvalidUri;

    fn try_from(scheme: &Scheme) -> Result<Self, Self::Error> {
        match scheme {
            Scheme::Http => Ok(Self::HTTP),
            Scheme::Https => Ok(Self::HTTPS),
            Scheme::Other(s) => s.parse(),
        }
    }
}

#[cfg(all(test, feature = "default-send-request"))]
mod tests {
    use super::*;

    #[test]
    fn field_size_errors_roundtrip_without_losing_optional_payloads() {
        let absent = Error::from(ErrorCode::HttpRequestHeaderSize(None));
        assert!(matches!(absent, Error::HttpRequestHeaderSize(None)));
        assert!(matches!(
            crate::p3::error_to_p3(crate::default_hooks(), &absent),
            ErrorCode::HttpRequestHeaderSize(None)
        ));
        for field_name in [None, Some(String::new()), Some("x-large".into())] {
            for field_size in [None, Some(0), Some(1234)] {
                let payload = types::FieldSizePayload {
                    field_name: field_name.clone(),
                    field_size,
                };
                for original in [
                    ErrorCode::HttpRequestHeaderSize(Some(payload.clone())),
                    ErrorCode::HttpRequestTrailerSize(payload.clone()),
                    ErrorCode::HttpResponseHeaderSize(payload.clone()),
                    ErrorCode::HttpResponseTrailerSize(payload),
                ] {
                    let expected = format!("{original:?}");
                    let error = Error::from(original);
                    let actual = crate::p3::error_to_p3(crate::default_hooks(), &error);
                    assert_eq!(format!("{actual:?}"), expected);
                }
            }
        }
    }

    #[test]
    fn hooks_only_classifier_and_view_share_hook_dispatch() {
        struct Hooks(usize);
        impl WasiHttpHooks for Hooks {
            fn p3_error_from_connect(&mut self, _: &std::io::Error) -> ErrorCode {
                self.0 += 1;
                ErrorCode::ConfigurationError
            }
        }
        let error = Error::Connect(std::io::Error::other("custom transport"));
        let mut hooks = Hooks(0);
        assert!(matches!(
            crate::p3::error_to_p3(&mut hooks, &error),
            ErrorCode::ConfigurationError
        ));
        let mut ctx = crate::WasiHttpCtx::new();
        let mut table = wasmtime::component::ResourceTable::new();
        let mut view = WasiHttpCtxView {
            ctx: &mut ctx,
            table: &mut table,
            hooks: &mut hooks,
        };
        assert!(matches!(
            view.error_to_p3(&error),
            ErrorCode::ConfigurationError
        ));
        assert_eq!(hooks.0, 2);
    }

    #[test]
    fn response_header_size_keeps_its_variant_and_payload() {
        let mut ctx = crate::WasiHttpCtx::new();
        let mut table = wasmtime::component::ResourceTable::new();
        let mut view = WasiHttpCtxView {
            ctx: &mut ctx,
            table: &mut table,
            hooks: crate::default_hooks(),
        };
        for (field_name, field_size) in
            [(Some("large-header".to_owned()), Some(1234)), (None, None)]
        {
            let original = ErrorCode::HttpResponseHeaderSize(types::FieldSizePayload {
                field_name: field_name.clone(),
                field_size,
            });
            let error = Error::from(original);
            assert!(matches!(&error, Error::HttpResponseHeaderSize { .. }));
            let ErrorCode::HttpResponseHeaderSize(payload) = view.error_to_p3(&error) else {
                panic!("response header error changed variant");
            };
            assert_eq!(payload.field_name, field_name);
            assert_eq!(payload.field_size, field_size);
        }
    }
}
