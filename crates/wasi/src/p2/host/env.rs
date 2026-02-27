use crate::cli::WasiCliCtxView;
use crate::p2::bindings::cli::environment;

impl environment::Host for WasiCliCtxView<'_> {
    async fn get_environment(&mut self) -> wasmtime::Result<Vec<(String, String)>> {
        Ok(self.ctx.environment.clone())
    }
    async fn get_arguments(&mut self) -> wasmtime::Result<Vec<String>> {
        Ok(self.ctx.arguments.clone())
    }
    async fn initial_cwd(&mut self) -> wasmtime::Result<Option<String>> {
        Ok(self.ctx.initial_cwd.clone())
    }
}
