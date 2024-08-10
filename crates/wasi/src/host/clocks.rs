#![allow(unused_variables)]

use async_trait::async_trait;
use crate::bindings::{
    clocks::monotonic_clock::{self, Duration as WasiDuration, Instant},
    clocks::wall_clock::{self, Datetime},
};
use crate::poll::{subscribe, Subscribe};
use crate::{Pollable, WasiView};
use cap_std::time::SystemTime;
use std::time::Duration;
use wasmtime::component::Resource;

impl TryFrom<SystemTime> for Datetime {
    type Error = anyhow::Error;

    fn try_from(time: SystemTime) -> Result<Self, Self::Error> {
        let duration =
            time.duration_since(SystemTime::from_std(std::time::SystemTime::UNIX_EPOCH))?;

        Ok(Datetime {
            seconds: duration.as_secs(),
            nanoseconds: duration.subsec_nanos(),
        })
    }
}

#[async_trait]
impl<T: WasiView> wall_clock::Host for T {
    async fn now(&mut self) -> anyhow::Result<Datetime> {
        let now = self.ctx().wall_clock.now();
        Ok(Datetime {
            seconds: now.as_secs(),
            nanoseconds: now.subsec_nanos(),
        })
    }

    async fn resolution(&mut self) -> anyhow::Result<Datetime> {
        let res = self.ctx().wall_clock.resolution();
        Ok(Datetime {
            seconds: res.as_secs(),
            nanoseconds: res.subsec_nanos(),
        })
    }
}

fn subscribe_to_duration(
    table: &mut wasmtime::component::ResourceTable,
    duration: tokio::time::Duration,
) -> anyhow::Result<Resource<Pollable>> {
    let (sleep, suspend_until) = if duration.is_zero() {
        (table.push(Deadline::Past)?, None)
    } else if let Some(deadline) = tokio::time::Instant::now().checked_add(duration) {
        // NB: this resource created here is not actually exposed to wasm, it's
        // only an internal implementation detail used to match the signature
        // expected by `subscribe`.
        (
            table.push(Deadline::Instant(deadline))?,
            Some(deadline.into()),
        )
    } else {
        // If the user specifies a time so far in the future we can't
        // represent it, wait forever rather than trap.
        (table.push(Deadline::Never)?, None)
    };
    subscribe(table, sleep, suspend_until)
}

#[async_trait]
impl<T: WasiView> monotonic_clock::Host for T {
    async fn now(&mut self) -> anyhow::Result<Instant> {
        Ok(self.ctx().monotonic_clock.now())
    }

    async fn resolution(&mut self) -> anyhow::Result<Instant> {
        Ok(self.ctx().monotonic_clock.resolution())
    }

    async fn subscribe_instant(&mut self, when: Instant) -> anyhow::Result<Resource<Pollable>> {
        let clock_now = self.ctx().monotonic_clock.now();
        let duration = if when > clock_now {
            Duration::from_nanos(when - clock_now)
        } else {
            Duration::from_nanos(0)
        };
        subscribe_to_duration(&mut self.table(), duration)
    }

    async fn subscribe_duration(
        &mut self,
        duration: WasiDuration,
    ) -> anyhow::Result<Resource<Pollable>> {
        subscribe_to_duration(&mut self.table(), Duration::from_nanos(duration))
    }
}

enum Deadline {
    Past,
    Instant(tokio::time::Instant),
    Never,
}

#[async_trait::async_trait]
impl Subscribe for Deadline {
    async fn ready(&mut self) {
        match self {
            Deadline::Past => {}
            Deadline::Instant(instant) => tokio::time::sleep_until(*instant).await,
            Deadline::Never => std::future::pending().await,
        }
    }
}
