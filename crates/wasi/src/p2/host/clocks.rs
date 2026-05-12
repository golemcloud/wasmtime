use crate::clocks::WasiClocksCtxView;
use crate::p2::DynPollable;
use crate::p2::bindings::{
    clocks::monotonic_clock::{self, Duration as WasiDuration, Instant},
    clocks::wall_clock::{self, Datetime},
};
use cap_std::time::SystemTime;
use std::time::Duration;
use wasmtime::component::Resource;
use wasmtime_wasi_io::poll::{Pollable, subscribe};

impl TryFrom<crate::clocks::Datetime> for Datetime {
    type Error = crate::clocks::DatetimeError;

    fn try_from(
        crate::clocks::Datetime {
            seconds,
            nanoseconds,
        }: crate::clocks::Datetime,
    ) -> Result<Self, Self::Error> {
        Ok(Self {
            seconds: seconds.try_into()?,
            nanoseconds,
        })
    }
}

impl TryFrom<Datetime> for crate::clocks::Datetime {
    type Error = crate::clocks::DatetimeError;

    fn try_from(
        Datetime {
            seconds,
            nanoseconds,
        }: Datetime,
    ) -> Result<Self, Self::Error> {
        Ok(Self {
            seconds: seconds.try_into()?,
            nanoseconds,
        })
    }
}

impl TryFrom<SystemTime> for Datetime {
    type Error = crate::clocks::DatetimeError;

    fn try_from(time: SystemTime) -> Result<Self, Self::Error> {
        let time = crate::clocks::Datetime::try_from(time)?;
        time.try_into()
    }
}

impl wall_clock::Host for WasiClocksCtxView<'_> {
    async fn now(&mut self) -> wasmtime::Result<Datetime> {
        let now = self.ctx.wall_clock.now();
        Ok(Datetime {
            seconds: now.as_secs(),
            nanoseconds: now.subsec_nanos(),
        })
    }

    async fn resolution(&mut self) -> wasmtime::Result<Datetime> {
        let res = self.ctx.wall_clock.resolution();
        Ok(Datetime {
            seconds: res.as_secs(),
            nanoseconds: res.subsec_nanos(),
        })
    }
}

fn subscribe_to_duration(
    table: &mut wasmtime::component::ResourceTable,
    duration: tokio::time::Duration,
) -> wasmtime::Result<Resource<DynPollable>> {
    let supports_suspend = if duration.is_zero() {
        None
    } else {
        std::time::Instant::now().checked_add(duration)
    };
    let is_past = duration.is_zero();
    let sleep = if is_past {
        table.push(Deadline::Past)?
    } else if let Some(deadline) = tokio::time::Instant::now().checked_add(duration) {
        // NB: this resource created here is not actually exposed to wasm, it's
        // only an internal implementation detail used to match the signature
        // expected by `subscribe`.
        table.push(Deadline::Instant(deadline))?
    } else {
        // If the user specifies a time so far in the future we can't
        // represent it, wait forever rather than trap.
        table.push(Deadline::Never)?
    };
    let pollable_resource = subscribe(table, sleep, supports_suspend)?;
    if is_past {
        // For zero-duration deadlines we want `pollable.ready()` to report
        // `true` synchronously (so guests using a past pollable as an "is
        // ready now?" probe behave correctly), while still ensuring that a
        // guest using a zero-duration timer as a cooperative yield via
        // `wasi:io/poll#poll` (see upstream wasmtime issue #13040) actually
        // yields control to the host async runtime so that other tasks
        // (e.g. `mio`-driven sockets) can make progress.
        let pollable: &mut DynPollable = table.get_mut(&pollable_resource)?;
        pollable.set_yield_on_immediate_return(true);
    }
    Ok(pollable_resource)
}

impl monotonic_clock::Host for WasiClocksCtxView<'_> {
    async fn now(&mut self) -> wasmtime::Result<Instant> {
        Ok(self.ctx.monotonic_clock.now())
    }

    async fn resolution(&mut self) -> wasmtime::Result<Instant> {
        Ok(self.ctx.monotonic_clock.resolution())
    }

    async fn subscribe_instant(&mut self, when: Instant) -> wasmtime::Result<Resource<DynPollable>> {
        let clock_now = self.ctx.monotonic_clock.now();
        let duration = if when > clock_now {
            Duration::from_nanos(when - clock_now)
        } else {
            Duration::from_nanos(0)
        };
        subscribe_to_duration(self.table, duration)
    }

    async fn subscribe_duration(
        &mut self,
        duration: WasiDuration,
    ) -> wasmtime::Result<Resource<DynPollable>> {
        subscribe_to_duration(self.table, Duration::from_nanos(duration))
    }
}

enum Deadline {
    Past,
    Instant(tokio::time::Instant),
    Never,
}

#[async_trait::async_trait]
impl Pollable for Deadline {
    async fn ready(&mut self) {
        match self {
            // Past deadlines resolve immediately. Cooperative yielding for the
            // zero-duration `wasi:io/poll#poll` case (upstream wasmtime issue
            // #13040) is handled in `wasmtime-wasi-io`'s `poll`/`block` impls
            // via the `yield_on_immediate_return` flag set when constructing
            // `Deadline::Past` pollables. Yielding here would also poison
            // `pollable.ready()` (which uses `poll_immediate`), making it
            // erroneously report `false` for an already-ready pollable.
            Deadline::Past => {}
            Deadline::Instant(instant) => tokio::time::sleep_until(*instant).await,
            Deadline::Never => std::future::pending().await,
        }
    }
}
