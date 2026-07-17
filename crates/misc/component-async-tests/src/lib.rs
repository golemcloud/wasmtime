#![expect(clippy::allow_attributes_without_reason)]

use wasmtime::component::{HasData, ResourceTable};
use wasmtime_wasi::{IoCtx, WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};

pub mod borrowing_host;
pub mod closed_streams;
pub mod resource_stream;
pub mod round_trip;
pub mod round_trip_direct;
pub mod round_trip_many;
pub mod transmit;
pub mod util;
pub mod yield_;
pub mod yield_runner;

/// Host implementation, usable primarily by tests
pub struct Ctx {
    pub wasi: WasiCtx,
    pub io_ctx: IoCtx,
    pub table: ResourceTable,
    pub continue_: bool,
}

impl Default for Ctx {
    fn default() -> Self {
        let (wasi, io_ctx) = WasiCtxBuilder::new().inherit_stdio().build();
        Self {
            wasi,
            io_ctx,
            table: ResourceTable::default(),
            continue_: false,
        }
    }
}

impl WasiView for Ctx {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.wasi,
            table: &mut self.table,
            io_ctx: &mut self.io_ctx,
        }
    }
}

impl HasData for Ctx {
    type Data<'a> = &'a mut Self;
}
