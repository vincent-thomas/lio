//! `wasi:random`, from the operating system's generator.

use crate::WasiCtx;
use crate::bindings::wasi::random::{insecure, insecure_seed, random};

/// The most bytes one call returns. WASI lets a host return fewer than asked.
const MAX_BYTES: u64 = 1 << 20;

fn bytes(max_len: u64) -> wasmtime::Result<Vec<u8>> {
  let mut bytes = vec![0; max_len.min(MAX_BYTES) as usize];
  getrandom::fill(&mut bytes)
    .map_err(|error| wasmtime::Error::msg(error.to_string()))?;
  Ok(bytes)
}

fn word() -> wasmtime::Result<u64> {
  let mut word = [0; 8];
  getrandom::fill(&mut word)
    .map_err(|error| wasmtime::Error::msg(error.to_string()))?;
  Ok(u64::from_le_bytes(word))
}

impl random::Host for WasiCtx {
  fn get_random_bytes(&mut self, max_len: u64) -> wasmtime::Result<Vec<u8>> {
    bytes(max_len)
  }

  fn get_random_u64(&mut self) -> wasmtime::Result<u64> {
    word()
  }
}

impl insecure::Host for WasiCtx {
  fn get_insecure_random_bytes(
    &mut self,
    max_len: u64,
  ) -> wasmtime::Result<Vec<u8>> {
    bytes(max_len)
  }

  fn get_insecure_random_u64(&mut self) -> wasmtime::Result<u64> {
    word()
  }
}

impl insecure_seed::Host for WasiCtx {
  fn get_insecure_seed(&mut self) -> wasmtime::Result<(u64, u64)> {
    Ok((word()?, word()?))
  }
}
