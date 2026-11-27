//! A WASI 0.3 host for wasmtime whose I/O runs on [`lio`].
//!
//! wasmtime runs the guest and the component model's async machinery. This
//! crate is what the guest's WASI imports reach: every one that can wait is
//! a lio operation, awaited on the thread that drives lio.
//!
//! ```no_run
//! use lio_wasi_host::{Engine, Store, WasiCtx, bindings::Command};
//! use wasmtime::component::{Component, Linker};
//!
//! # fn main() -> wasmtime::Result<()> {
//! let lio = lio::Lio::new(1024)?;
//! let _installed = lio::install_global(lio.clone());
//!
//! let engine = Engine::new(wasmtime::Config::new())?;
//! let mut linker = Linker::<WasiCtx>::new(&engine);
//! lio_wasi_host::add_to_linker(&mut linker, |ctx| ctx)?;
//!
//! let component = Component::from_file(&engine, "guest.wasm")?;
//! let ctx = WasiCtx::builder().preopen_dir(".", "/")?.build();
//! let mut store = Store::new(&engine, ctx, &lio, |ctx| ctx);
//!
//! let result = lio_wasi_host::block_on(&lio, async {
//!   let command =
//!     Command::instantiate_async(&mut *store, &component, &linker).await?;
//!   store
//!     .run_concurrent(async |store| command.wasi_cli_run().call_run(store).await)
//!     .await?
//! })??;
//! # let _ = result;
//! # Ok(())
//! # }
//! ```
//!
//! # Reads go straight into the guest
//!
//! On a [`Store`], a guest's read hands lio the guest's own buffer, so the
//! kernel writes where the guest will look. [`Engine`] and [`Store`] are
//! what keep that memory in place until the kernel is done. A plain
//! `wasmtime::Store` works too, with a host buffer and a copy per read.
//!
//! # One thread
//!
//! lio's driver and its futures belong to one thread. The store, the guest
//! and every host call run there, under [`block_on`].
//!
//! # Not a sandbox yet
//!
//! Paths are checked lexically: an absolute path, or one that climbs above
//! its directory with `..`, is refused. A symbolic link inside a preopened
//! directory that points outside it is followed.

pub mod bindings;
pub mod cli;
mod clocks;
mod exec;
pub mod filesystem;
mod io;
mod lend;
mod local;
mod random;
pub mod sockets;
mod status;
mod stream;

use std::fs::File;
use std::os::fd::OwnedFd;
use std::path::Path;
use std::time::Instant;

use lio::api::resource::Resource;
use wasmtime::component::{HasData, Linker, ResourceTable};

pub use exec::block_on;
pub use lend::{Engine, Stats, Store, stats};

use filesystem::Descriptor;

/// What the guest's WASI imports work on: its arguments, environment,
/// preopened directories and open resources.
pub struct WasiCtx {
  pub(crate) table: ResourceTable,
  pub(crate) args: Vec<String>,
  pub(crate) env: Vec<(String, String)>,
  pub(crate) preopens: Vec<(Descriptor, String)>,
  pub(crate) network: bool,
  pub(crate) name_lookup: bool,
  /// The monotonic clock's zero.
  pub(crate) started: Instant,
  /// Set by [`Store::new`]: the guest's own memory may be a read's buffer.
  pub(crate) lending: Option<lend::LendingRef>,
}

impl WasiCtx {
  pub fn builder() -> WasiCtxBuilder {
    WasiCtxBuilder {
      args: Vec::new(),
      env: Vec::new(),
      preopens: Vec::new(),
      network: false,
      name_lookup: false,
    }
  }
}

/// Builds a [`WasiCtx`].
pub struct WasiCtxBuilder {
  args: Vec<String>,
  env: Vec<(String, String)>,
  preopens: Vec<(Descriptor, String)>,
  network: bool,
  name_lookup: bool,
}

impl WasiCtxBuilder {
  /// The guest's arguments, the program's name first.
  pub fn args(mut self, args: impl IntoIterator<Item = String>) -> Self {
    self.args.extend(args);
    self
  }

  pub fn env(
    mut self,
    name: impl Into<String>,
    value: impl Into<String>,
  ) -> Self {
    self.env.push((name.into(), value.into()));
    self
  }

  /// Gives the guest the host directory `host` under the name `guest`.
  ///
  /// # Errors
  ///
  /// When the directory cannot be opened.
  pub fn preopen_dir(
    mut self,
    host: impl AsRef<Path>,
    guest: impl Into<String>,
  ) -> std::io::Result<Self> {
    let dir = File::open(host)?;
    if !dir.metadata()?.is_dir() {
      return Err(std::io::Error::from(std::io::ErrorKind::NotADirectory));
    }
    let resource = Resource::from(OwnedFd::from(dir));
    self.preopens.push((Descriptor::preopen(resource), guest.into()));
    Ok(self)
  }

  /// Lets the guest open sockets.
  pub fn allow_network(mut self, allow: bool) -> Self {
    self.network = allow;
    self
  }

  /// Lets the guest resolve host names.
  pub fn allow_name_lookup(mut self, allow: bool) -> Self {
    self.name_lookup = allow;
    self
  }

  pub fn build(self) -> WasiCtx {
    WasiCtx {
      table: ResourceTable::new(),
      args: self.args,
      env: self.env,
      preopens: self.preopens,
      network: self.network,
      name_lookup: self.name_lookup,
      started: Instant::now(),
      lending: None,
    }
  }
}

/// Names [`WasiCtx`] as what the host functions get from the store's data.
pub struct LioWasi;

impl HasData for LioWasi {
  type Data<'a> = &'a mut WasiCtx;
}

/// Adds every interface of `wasi:cli/command@0.3` to `linker`.
///
/// # Errors
///
/// When an interface is already defined in `linker`.
pub fn add_to_linker<T: Send + 'static>(
  linker: &mut Linker<T>,
  ctx: fn(&mut T) -> &mut WasiCtx,
) -> wasmtime::Result<()> {
  let options = bindings::LinkOptions::default();
  bindings::Command::add_to_linker::<T, LioWasi>(linker, &options, ctx)
}

/// The guest called `wasi:cli/exit`. It comes back as the error of the call
/// that was running.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Exit(pub u8);

impl std::fmt::Display for Exit {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    write!(f, "the guest exited with status {}", self.0)
  }
}

impl std::error::Error for Exit {}
