//! `wasi:cli`: arguments, environment, exit and the standard streams.

use std::io::{self, IsTerminal};

use lio::api::resource::Resource;
use wasmtime::component::{
  Access, FutureReader, Resource as Handle, StreamReader,
};

use crate::bindings::wasi::cli::types::ErrorCode;
use crate::bindings::wasi::cli::{
  environment, exit, stderr, stdin, stdout, terminal_input, terminal_output,
  terminal_stderr, terminal_stdin, terminal_stdout, types,
};
use crate::stream::{ByteReader, ByteWriter, ReadFrom, WriteTo};
use crate::{Exit, LioWasi, WasiCtx};

/// Standard input, when it is a terminal.
pub struct TerminalInput;

/// Standard output or error, when it is a terminal.
pub struct TerminalOutput;

fn error(error: &io::Error) -> ErrorCode {
  match error.raw_os_error() {
    Some(libc::EPIPE) => ErrorCode::Pipe,
    Some(libc::EILSEQ) => ErrorCode::IllegalByteSequence,
    _ => ErrorCode::Io,
  }
}

fn nothing(_: &Resource) {}

impl environment::Host for WasiCtx {
  fn get_environment(&mut self) -> wasmtime::Result<Vec<(String, String)>> {
    Ok(self.env.clone())
  }

  fn get_arguments(&mut self) -> wasmtime::Result<Vec<String>> {
    Ok(self.args.clone())
  }

  fn get_initial_cwd(&mut self) -> wasmtime::Result<Option<String>> {
    Ok(None)
  }
}

impl exit::Host for WasiCtx {
  fn exit(&mut self, status: Result<(), ()>) -> wasmtime::Result<()> {
    Err(Exit(u8::from(status.is_err())).into())
  }

  fn exit_with_code(&mut self, status_code: u8) -> wasmtime::Result<()> {
    Err(Exit(status_code).into())
  }
}

impl types::Host for WasiCtx {}
impl stdin::Host for WasiCtx {}
impl stdout::Host for WasiCtx {}
impl stderr::Host for WasiCtx {}

impl<T: Send + 'static> stdin::HostWithStore<T> for LioWasi {
  fn read_via_stream(
    mut host: Access<T, Self>,
  ) -> wasmtime::Result<(StreamReader<u8>, FutureReader<Result<(), ErrorCode>>)>
  {
    let from = ReadFrom::Cursor(Resource::stdin());
    let lending = host.get().lending.clone();
    let (reader, status) = ByteReader::new(from, error, nothing, lending);
    Ok((
      StreamReader::new(&mut host, reader)?,
      FutureReader::new(&mut host, status)?,
    ))
  }
}

fn write<T: Send + 'static>(
  mut host: Access<T, LioWasi>,
  to: Resource,
  data: StreamReader<u8>,
) -> wasmtime::Result<FutureReader<Result<(), ErrorCode>>> {
  let lending = host.get().lending.clone();
  let (writer, status) =
    ByteWriter::new(WriteTo::Cursor(to), error, nothing, lending);
  data.pipe(&mut host, writer)?;
  FutureReader::new(&mut host, status)
}

impl<T: Send + 'static> stdout::HostWithStore<T> for LioWasi {
  fn write_via_stream(
    host: Access<T, Self>,
    data: StreamReader<u8>,
  ) -> wasmtime::Result<FutureReader<Result<(), ErrorCode>>> {
    write(host, Resource::stdout(), data)
  }
}

impl<T: Send + 'static> stderr::HostWithStore<T> for LioWasi {
  fn write_via_stream(
    host: Access<T, Self>,
    data: StreamReader<u8>,
  ) -> wasmtime::Result<FutureReader<Result<(), ErrorCode>>> {
    write(host, Resource::stderr(), data)
  }
}

impl terminal_input::Host for WasiCtx {}
impl terminal_output::Host for WasiCtx {}

impl terminal_input::HostTerminalInput for WasiCtx {
  fn drop(&mut self, rep: Handle<TerminalInput>) -> wasmtime::Result<()> {
    self.table.delete(rep)?;
    Ok(())
  }
}

impl terminal_output::HostTerminalOutput for WasiCtx {
  fn drop(&mut self, rep: Handle<TerminalOutput>) -> wasmtime::Result<()> {
    self.table.delete(rep)?;
    Ok(())
  }
}

impl terminal_stdin::Host for WasiCtx {
  fn get_terminal_stdin(
    &mut self,
  ) -> wasmtime::Result<Option<Handle<TerminalInput>>> {
    if !io::stdin().is_terminal() {
      return Ok(None);
    }
    Ok(Some(self.table.push(TerminalInput)?))
  }
}

impl terminal_stdout::Host for WasiCtx {
  fn get_terminal_stdout(
    &mut self,
  ) -> wasmtime::Result<Option<Handle<TerminalOutput>>> {
    if !io::stdout().is_terminal() {
      return Ok(None);
    }
    Ok(Some(self.table.push(TerminalOutput)?))
  }
}

impl terminal_stderr::Host for WasiCtx {
  fn get_terminal_stderr(
    &mut self,
  ) -> wasmtime::Result<Option<Handle<TerminalOutput>>> {
    if !io::stderr().is_terminal() {
      return Ok(None);
    }
    Ok(Some(self.table.push(TerminalOutput)?))
  }
}
