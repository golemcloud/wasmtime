use async_trait::async_trait;
use crate::bindings::cli::environment;
use crate::{WasiImpl, WasiView};

#[async_trait]
impl<T> environment::Host for WasiImpl<T>
where
    T: WasiView,
{
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
