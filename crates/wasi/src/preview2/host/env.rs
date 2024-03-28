use crate::preview2::bindings::cli::environment;
use crate::preview2::WasiView;
use async_trait::async_trait;

#[async_trait]
impl<T: WasiView> environment::Host for T {
    async fn get_environment(&mut self) -> anyhow::Result<Vec<(String, String)>> {
        Ok(self.ctx().env.clone())
    }
    async fn get_arguments(&mut self) -> anyhow::Result<Vec<String>> {
        Ok(self.ctx().args.clone())
    }
    async fn initial_cwd(&mut self) -> anyhow::Result<Option<String>> {
        // FIXME: expose cwd in builder and save in ctx
        Ok(None)
    }
}
