//! `wasi:clocks`. Waiting is a lio timer.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use lio::api;
use wasmtime::component::Accessor;

use crate::bindings::wasi::clocks::{
  monotonic_clock, system_clock, timezone, types,
};
use crate::io::op_waits;
use crate::{LioWasi, WasiCtx};

impl types::Host for WasiCtx {}

impl WasiCtx {
  /// Nanoseconds since this context was built.
  fn mark(&self) -> u64 {
    u64::try_from(self.started.elapsed().as_nanos()).unwrap_or(u64::MAX)
  }
}

async fn sleep(nanoseconds: u64) -> wasmtime::Result<()> {
  if nanoseconds > 0 {
    op_waits(api::sleep(Duration::from_nanos(nanoseconds))).await?;
  }
  Ok(())
}

impl monotonic_clock::Host for WasiCtx {
  fn now(&mut self) -> wasmtime::Result<monotonic_clock::Mark> {
    Ok(self.mark())
  }

  fn get_resolution(&mut self) -> wasmtime::Result<monotonic_clock::Duration> {
    Ok(1)
  }
}

impl<T: Send + 'static> monotonic_clock::HostWithStore<T> for LioWasi {
  async fn wait_until(
    accessor: &Accessor<T, Self>,
    when: monotonic_clock::Mark,
  ) -> wasmtime::Result<()> {
    let now = accessor.with(|mut store| store.get().mark());
    sleep(when.saturating_sub(now)).await
  }

  async fn wait_for(
    _accessor: &Accessor<T, Self>,
    how_long: monotonic_clock::Duration,
  ) -> wasmtime::Result<()> {
    sleep(how_long).await
  }
}

impl system_clock::Host for WasiCtx {
  fn now(&mut self) -> wasmtime::Result<system_clock::Instant> {
    Ok(match SystemTime::now().duration_since(UNIX_EPOCH) {
      Ok(since) => system_clock::Instant {
        seconds: i64::try_from(since.as_secs()).unwrap_or(i64::MAX),
        nanoseconds: since.subsec_nanos(),
      },
      // A clock set before 1970.
      Err(before) => {
        let before = before.duration();
        let seconds = i64::try_from(before.as_secs()).unwrap_or(i64::MAX);
        if before.subsec_nanos() == 0 {
          system_clock::Instant { seconds: -seconds, nanoseconds: 0 }
        } else {
          system_clock::Instant {
            seconds: -seconds - 1,
            nanoseconds: 1_000_000_000 - before.subsec_nanos(),
          }
        }
      }
    })
  }

  fn get_resolution(&mut self) -> wasmtime::Result<system_clock::Duration> {
    Ok(1)
  }
}

impl timezone::Host for WasiCtx {
  fn iana_id(&mut self) -> wasmtime::Result<Option<String>> {
    Ok(None)
  }

  fn utc_offset(
    &mut self,
    _when: system_clock::Instant,
  ) -> wasmtime::Result<Option<i64>> {
    Ok(None)
  }

  fn to_debug_string(&mut self) -> wasmtime::Result<String> {
    Ok(String::from("UTC"))
  }
}
