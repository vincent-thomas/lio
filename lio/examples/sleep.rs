//! A simple `sleep` implementation using lio.
//!
//! Usage: cargo run --example sleep <seconds>

use lio::{Lio, api};
use std::env;
use std::time::{Duration, Instant};

fn main() -> std::io::Result<()> {
  let seconds: f64 =
    env::args().nth(1).and_then(|s| s.parse().ok()).unwrap_or_else(|| {
      eprintln!("Usage: sleep <seconds>");
      std::process::exit(1);
    });

  let duration = Duration::from_secs_f64(seconds);
  let lio = Lio::new(1)?;

  let start = Instant::now();
  let rx = api::sleep(duration).with_lio(&lio).send();
  lio.run().expect("lio.try_run()");
  rx.recv()?;
  let elapsed = start.elapsed();

  println!("Slept for {:?} (requested {:?})", elapsed, duration);
  Ok(())
}
