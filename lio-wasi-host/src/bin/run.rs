//! Runs a `wasi:cli/command` component on lio.

mod common;

use std::process::ExitCode;

use lio_wasi_host::bindings::Command;
use lio_wasi_host::{Engine, Exit, Store, WasiCtx};
use wasmtime::component::{Component, Linker};

fn main() -> ExitCode {
  match run() {
    Ok(code) => code,
    Err(error) => {
      eprintln!("Error: {error:?}");
      ExitCode::FAILURE
    }
  }
}

fn run() -> wasmtime::Result<ExitCode> {
  let args = common::parse().map_err(wasmtime::Error::msg)?;
  let stats = args.stats;

  let lio = lio::Lio::new(4096)?;
  let _installed = lio::install_global(lio.clone());

  let engine = Engine::new(common::config()?)?;
  let component = Component::from_file(&engine, &args.wasm)?;
  let mut linker = Linker::<WasiCtx>::new(&engine);
  lio_wasi_host::add_to_linker(&mut linker, |ctx| ctx)?;

  let mut ctx = WasiCtx::builder()
    .args(args.guest)
    .allow_network(args.network)
    .allow_name_lookup(args.name_lookup);
  for (name, value) in args.env {
    ctx = ctx.env(name, value);
  }
  for (host, guest) in args.dirs {
    ctx = ctx.preopen_dir(host, guest)?;
  }
  let mut store = Store::new(&engine, ctx.build(), &lio, |ctx| ctx);

  let result = lio_wasi_host::block_on(&lio, async {
    let command =
      Command::instantiate_async(&mut *store, &component, &linker).await?;
    store
      .run_concurrent(async |store| {
        command.wasi_cli_run().call_run(store).await
      })
      .await?
  })?;
  if stats {
    eprintln!("{:?}", lio_wasi_host::stats());
  }
  match result {
    Ok(Ok(())) => Ok(ExitCode::SUCCESS),
    Ok(Err(())) => Ok(ExitCode::FAILURE),
    Err(error) => match error.downcast_ref::<Exit>() {
      Some(Exit(code)) => Ok(ExitCode::from(*code)),
      None => Err(error),
    },
  }
}
