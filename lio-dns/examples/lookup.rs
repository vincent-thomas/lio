//! Looks up the addresses of the hosts named on the command line with the
//! system's resolver configuration.
//!
//! ```sh
//! cargo run -p lio-dns --example lookup -- example.com localhost
//! ```

use std::{
  future::IntoFuture,
  pin::pin,
  task::{Context, Poll, Waker},
};

use lio::{Lio, install_global};
use lio_dns::Resolver;

fn main() -> Result<(), Box<dyn std::error::Error>> {
  let lio = Lio::new(64)?;
  let _guard = install_global(lio.clone());

  let resolver = block_on(&lio, Resolver::system())?;
  for host in std::env::args().skip(1) {
    match block_on(&lio, resolver.lookup_ip(&host)) {
      Ok(addrs) => {
        for addr in addrs {
          println!("{host}\t{addr}");
        }
      }
      Err(err) => println!("{host}\t{err}"),
    }
  }
  Ok(())
}

/// Runs the driver until `fut` is done. Lio has no executor of its own.
fn block_on<F: IntoFuture>(lio: &Lio, fut: F) -> F::Output {
  let mut cx = Context::from_waker(Waker::noop());
  let mut fut = pin!(fut.into_future());
  loop {
    if let Poll::Ready(value) = fut.as_mut().poll(&mut cx) {
      return value;
    }
    if let Err(err) = lio.run() {
      panic!("the driver failed: {err}");
    }
  }
}
