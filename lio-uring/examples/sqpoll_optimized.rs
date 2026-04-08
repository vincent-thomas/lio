//! Minimal example demonstrating the current SQPOLL configuration API.

use lio_uring::{LioUring, Params, SqeFlags, operation::*};
use std::io;

fn main() -> io::Result<()> {
  let params = Params::default().sqpoll(1000);
  let mut ring = match LioUring::with_params(params) {
    Ok(ring) => ring,
    Err(e) if e.raw_os_error() == Some(libc::EPERM) => return Ok(()),
    Err(e) => return Err(e),
  };

  let nop1 = Nop::new();
  let nop2 = Nop::new();

  unsafe { ring.push_with_flags(nop1.build(), 1, SqeFlags::IO_LINK)? };
  unsafe { ring.push(nop2.build(), 2)? };
  ring.submit()?;

  let completion = ring.wait()?;
  assert!(completion.is_ok());
  let completion = ring.wait()?;
  assert!(completion.is_ok());

  println!("sqpoll_optimized: submitted two linked NOPs via SQPOLL");
  Ok(())
}
