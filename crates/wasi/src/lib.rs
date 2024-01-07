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
pub use self::filesystem::{
    DirPerms, FileInputStream, FilePerms, FsError, FsResult, ReaddirIterator,
};
pub use self::network::{Network, SocketAddrUse, SocketError, SocketResult};
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

/// Add all WASI interfaces from this crate into the `linker` provided.
///
/// This function will add the `async` variant of all interfaces into the
/// [`Linker`] provided. By `async` this means that this function is only
/// compatible with [`Config::async_support(true)`][async]. For embeddings with
/// async support disabled see [`add_to_linker_sync`] instead.
///
/// This function will add all interfaces implemented by this crate to the
/// [`Linker`], which corresponds to the `wasi:cli/imports` world supported by
/// this crate.
///
/// [async]: wasmtime::Config::async_support
///
/// # Example
///
/// ```
/// use wasmtime::{Engine, Result, Store, Config};
/// use wasmtime::component::{ResourceTable, Linker};
/// use wasmtime_wasi::{IoView, WasiCtx, WasiView, WasiCtxBuilder};
///
/// fn main() -> Result<()> {
///     let mut config = Config::new();
///     config.async_support(true);
///     let engine = Engine::new(&config)?;
///
///     let mut linker = Linker::<MyState>::new(&engine);
///     wasmtime_wasi::add_to_linker_async(&mut linker)?;
///     // ... add any further functionality to `linker` if desired ...
///
///     let mut builder = WasiCtxBuilder::new();
///
///     // ... configure `builder` more to add env vars, args, etc ...
///
///     let mut store = Store::new(
///         &engine,
///         MyState {
///             ctx: builder.build(),
///             table: ResourceTable::new(),
///             io_ctx:
///         },
///     );
///
///     // ... use `linker` to instantiate within `store` ...
///
///     Ok(())
/// }
///
/// struct MyState {
///     ctx: WasiCtx,
///     table: ResourceTable,
///     io_ctx: IoCtx
/// }
///
/// impl IoView for MyState {
///     fn table(&mut self) -> &mut ResourceTable { &mut self.table }
///     fn ctx(&mut self) -> &mut IoCtx { &mut self.io_ctx }
/// }
/// impl WasiView for MyState {
///     fn ctx(&mut self) -> &mut WasiCtx { &mut self.ctx }
/// }
/// ```
pub fn add_to_linker_async<T: WasiView>(linker: &mut Linker<T>) -> anyhow::Result<()> {
    let options = crate::bindings::LinkOptions::default();
    add_to_linker_with_options_async(linker, &options)
}

/// Similar to [`add_to_linker_async`], but with the ability to enable unstable features.
pub fn add_to_linker_with_options_async<T: WasiView>(
    linker: &mut Linker<T>,
    options: &crate::bindings::LinkOptions,
) -> anyhow::Result<()> {
    let l = linker;
    wasmtime_wasi_io::add_to_linker_async(l)?;

    let closure = type_annotate::<T, _>(|t| WasiImpl(IoImpl(t)));

    crate::bindings::clocks::wall_clock::add_to_linker_get_host(l, closure)?;
    crate::bindings::clocks::monotonic_clock::add_to_linker_get_host(l, closure)?;
    crate::bindings::filesystem::types::add_to_linker_get_host(l, closure)?;
    crate::bindings::filesystem::preopens::add_to_linker_get_host(l, closure)?;
    crate::bindings::random::random::add_to_linker_get_host(l, closure)?;
    crate::bindings::random::insecure::add_to_linker_get_host(l, closure)?;
    crate::bindings::random::insecure_seed::add_to_linker_get_host(l, closure)?;
    crate::bindings::cli::exit::add_to_linker_get_host(l, &options.into(), closure)?;
    crate::bindings::cli::environment::add_to_linker_get_host(l, closure)?;
    crate::bindings::cli::stdin::add_to_linker_get_host(l, closure)?;
    crate::bindings::cli::stdout::add_to_linker_get_host(l, closure)?;
    crate::bindings::cli::stderr::add_to_linker_get_host(l, closure)?;
    crate::bindings::cli::terminal_input::add_to_linker_get_host(l, closure)?;
    crate::bindings::cli::terminal_output::add_to_linker_get_host(l, closure)?;
    crate::bindings::cli::terminal_stdin::add_to_linker_get_host(l, closure)?;
    crate::bindings::cli::terminal_stdout::add_to_linker_get_host(l, closure)?;
    crate::bindings::cli::terminal_stderr::add_to_linker_get_host(l, closure)?;
    crate::bindings::sockets::tcp::add_to_linker_get_host(l, closure)?;
    crate::bindings::sockets::tcp_create_socket::add_to_linker_get_host(l, closure)?;
    crate::bindings::sockets::udp::add_to_linker_get_host(l, closure)?;
    crate::bindings::sockets::udp_create_socket::add_to_linker_get_host(l, closure)?;
    crate::bindings::sockets::instance_network::add_to_linker_get_host(l, closure)?;
    crate::bindings::sockets::network::add_to_linker_get_host(l, &options.into(), closure)?;
    crate::bindings::sockets::ip_name_lookup::add_to_linker_get_host(l, closure)?;
    Ok(())
}

// NB: workaround some rustc inference - a future refactoring may make this
// obsolete.
#[allow(dead_code)]
fn io_type_annotate<T: IoView, F>(val: F) -> F
where
    F: Fn(&mut T) -> IoImpl<&mut T>,
{
    val
}
fn type_annotate<T: WasiView, F>(val: F) -> F
where
    F: Fn(&mut T) -> WasiImpl<&mut T>,
{
    val
}
