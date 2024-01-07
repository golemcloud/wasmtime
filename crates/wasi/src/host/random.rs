use crate::bindings::random::{insecure, insecure_seed, random};
use async_trait::async_trait;
use crate::{WasiImpl, WasiView};
use cap_rand::{distributions::Standard, Rng};

#[async_trait]
impl<T> random::Host for WasiImpl<T>
where
    T: WasiView,
{
    async fn get_random_bytes(&mut self, len: u64) -> anyhow::Result<Vec<u8>> {
        Ok((&mut self.ctx().random)
            .sample_iter(Standard)
            .take(len as usize)
            .collect())
    }

    async fn get_random_u64(&mut self) -> anyhow::Result<u64> {
        Ok(self.ctx().random.sample(Standard))
    }
}

#[async_trait]
impl<T> insecure::Host for WasiImpl<T>
where
    T: WasiView,
{
    async fn get_insecure_random_bytes(&mut self, len: u64) -> anyhow::Result<Vec<u8>> {
        Ok((&mut self.ctx().insecure_random)
            .sample_iter(Standard)
            .take(len as usize)
            .collect())
    }

    async fn get_insecure_random_u64(&mut self) -> anyhow::Result<u64> {
        Ok(self.ctx().insecure_random.sample(Standard))
    }
}

#[async_trait]
impl<T> insecure_seed::Host for WasiImpl<T>
where
    T: WasiView,
{
    async fn insecure_seed(&mut self) -> anyhow::Result<(u64, u64)> {
        let seed: u128 = self.ctx().insecure_random_seed;
        Ok((seed as u64, (seed >> 64) as u64))
    }
}
