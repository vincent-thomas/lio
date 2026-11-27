//! Runs a `wasi:cli/command` component on `wasmtime-wasi`, the host this
//! crate is measured against. Same engine, same flags.

mod common;

use std::process::ExitCode;

use wasmtime::Store;
use wasmtime::component::{Component, Linker, ResourceTable};
use wasmtime_wasi::p3::bindings::Command;
use wasmtime_wasi::{FsPerms, I32Exit, WasiCtx, WasiCtxView, WasiView};

struct Host {
  ctx: WasiCtx,
  table: ResourceTable,
}

impl WasiView for Host {
  fn ctx(&mut self) -> WasiCtxView<'_> {
    WasiCtxView { ctx: &mut self.ctx, table: &mut self.table }
  }
}

fn main() -> ExitCode {
  let runtime =
    match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
      Ok(runtime) => runtime,
      Err(error) => {
        eprintln!("Error: {error:?}");
        return ExitCode::FAILURE;
      }
    };
  match runtime.block_on(run()) {
    Ok(code) => code,
    Err(error) => {
      eprintln!("Error: {error:?}");
      ExitCode::FAILURE
    }
  }
}

async fn run() -> wasmtime::Result<ExitCode> {
  let args = common::parse().map_err(wasmtime::Error::msg)?;

  let engine = wasmtime::Engine::new(&common::config()?)?;
  let component = Component::from_file(&engine, &args.wasm)?;
  let mut linker = Linker::<Host>::new(&engine);
  wasmtime_wasi::p3::add_to_linker(&mut linker)?;

  let mut ctx = WasiCtx::builder();
  ctx.inherit_stdio().args(&args.guest);
  ctx.allow_blocking_current_thread(args.blocking);
  if args.network {
    ctx.inherit_network().allow_tcp(true).allow_udp(true);
  }
  ctx.allow_ip_name_lookup(args.name_lookup);
  for (name, value) in &args.env {
    ctx.env(name, value);
  }
  for (host, guest) in &args.dirs {
    ctx.preopened_dir(host, guest, FsPerms::ReadWrite)?;
  }
  let host = Host { ctx: ctx.build(), table: ResourceTable::new() };
  let mut store = Store::new(&engine, host);

  let command =
    Command::instantiate_async(&mut store, &component, &linker).await?;
  let result = store
    .run_concurrent(async |store| command.wasi_cli_run().call_run(store).await)
    .await;
  match result {
    Ok(Ok(Ok(()))) => Ok(ExitCode::SUCCESS),
    Ok(Ok(Err(()))) => Ok(ExitCode::FAILURE),
    Ok(Err(error)) | Err(error) => match error.downcast_ref::<I32Exit>() {
      Some(I32Exit(code)) => Ok(ExitCode::from(*code as u8)),
      None => Err(error),
    },
  }
}
