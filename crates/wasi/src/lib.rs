#![cfg_attr(docsrs, feature(doc_auto_cfg))]

//! # Wasmtime's WASI Implementation
//!
//! This crate provides a Wasmtime host implementations of different versions of WASI.
//! WASI is implemented with the Rust crates [`tokio`] and [`cap-std`](cap_std) primarily, meaning that
//! operations are implemented in terms of their native platform equivalents by
//! default.
//!
//! For components and WASIp2, see [`p2`].
//! For WASIp1 and core modules, see the [`preview1`] module documentation.

mod clocks;
mod error;
mod fs;
mod net;
pub mod p2;
mod random;
pub mod runtime;

pub use self::clocks::{HostMonotonicClock, HostWallClock};
pub use self::error::{I32Exit, TrappableError};
pub use self::fs::{DirPerms, FilePerms, OpenMode};
pub use self::net::{Network, SocketAddrUse};
pub use self::random::{thread_rng, Deterministic};
#[doc(no_inline)]
pub use async_trait::async_trait;
#[doc(no_inline)]
pub use cap_fs_ext::SystemTimeSpec;
#[doc(no_inline)]
pub use cap_rand::RngCore;
#[doc(no_inline)]
pub use wasmtime::component::{ResourceTable, ResourceTableError};
// These contents of wasmtime-wasi-io are re-exported by this crate for compatibility:
// they were originally defined in this crate before being factored out, and many
// users of this crate depend on them at these names.
pub use wasmtime_wasi_io::poll::{
    dynamic_subscribe, subscribe, DynFuture, DynPollable, DynamicPollable, MakeFuture,
    OverrideSelf, Pollable,
};
pub use wasmtime_wasi_io::streams::{
    DynInputStream, DynOutputStream, Error as IoError, InputStream, OutputStream, StreamError,
    StreamResult,
};
pub use wasmtime_wasi_io::{IoCtx, IoImpl, IoView};
