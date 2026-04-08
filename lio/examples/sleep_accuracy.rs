//! Measure `api::sleep()` timing accuracy for one requested duration.
//!
//! Usage:
//! - `cargo run --release --example sleep_accuracy -- 1.0`

use lio::{Lio, api};
use std::io;
use std::time::{Duration, Instant};

fn main() -> io::Result<()> {
  let seconds = std::env::args()
    .nth(1)
    .unwrap_or_else(|| {
      eprintln!("Usage: sleep_accuracy <seconds>");
      std::process::exit(1);
    })
    .parse::<f64>()
    .unwrap_or_else(|_| {
      eprintln!("Usage: sleep_accuracy <seconds>");
      std::process::exit(1);
    });

  if !seconds.is_finite() || seconds < 0.0 {
    eprintln!("seconds must be a finite non-negative number");
    std::process::exit(1);
  }

  let requested = Duration::from_secs_f64(seconds);
  let lio = Lio::new(64)?;

  let start = Instant::now();
  let result = run(&lio, api::sleep(requested).with_lio(&lio).send());
  let elapsed = start.elapsed();
  result?;

  let drift = elapsed.abs_diff(requested);
  let signed_drift_ms =
    elapsed.as_secs_f64() * 1_000.0 - requested.as_secs_f64() * 1_000.0;

  println!("requested: {:.6}s", requested.as_secs_f64());
  println!("elapsed:   {:.6}s", elapsed.as_secs_f64());
  println!("drift:     {signed_drift_ms:+.3} ms");
  println!("accuracy:  {:.3} ms", drift.as_secs_f64() * 1_000.0);

  Ok(())
}

fn run<T>(lio: &Lio, mut rx: api::Receiver<T>) -> T {
  loop {
    if let Some(result) = rx.try_recv() {
      return result;
    }
    if lio.try_run().expect("lio.try_run()") == 0 {
      lio.run().expect("lio.run()");
    }
  }
}
