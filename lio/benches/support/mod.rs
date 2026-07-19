use std::env;
use std::time::{Duration, Instant};

pub struct Harness {
  warmup: Duration,
  sample_time: Duration,
  samples: usize,
  json: bool,
}

impl Harness {
  pub fn from_args() -> Self {
    let mut harness = Self {
      warmup: Duration::from_millis(500),
      sample_time: Duration::from_millis(50),
      samples: 30,
      json: false,
    };
    let mut args = env::args().skip(1);
    while let Some(arg) = args.next() {
      match arg.as_str() {
        // Cargo injects this for custom benchmark targets (`harness = false`).
        "--bench" => {}
        "--json" => harness.json = true,
        "--warmup-ms" => {
          harness.warmup = Duration::from_millis(parse_value(&mut args, &arg));
        }
        "--sample-ms" => {
          harness.sample_time =
            Duration::from_millis(parse_value(&mut args, &arg));
        }
        "--samples" => harness.samples = parse_value(&mut args, &arg),
        "--help" | "-h" => {
          eprintln!(
            "Usage: bookkeeping [--json] [--warmup-ms N] [--sample-ms N] [--samples N]"
          );
          std::process::exit(0);
        }
        _ => panic!("unknown benchmark argument: {arg}"),
      }
    }
    assert!(harness.samples > 0, "--samples must be greater than zero");
    harness
  }

  pub fn bench(
    &self,
    name: &str,
    operations_per_iteration: u64,
    mut f: impl FnMut(),
  ) {
    assert!(operations_per_iteration > 0);

    let warmup_deadline = Instant::now() + self.warmup;
    while Instant::now() < warmup_deadline {
      f();
    }

    let mut iterations = 1u64;
    loop {
      let started = Instant::now();
      for _ in 0..iterations {
        f();
      }
      let elapsed = started.elapsed();
      if elapsed >= self.sample_time || iterations >= u64::MAX / 2 {
        break;
      }
      let elapsed_ns = elapsed.as_nanos().max(1);
      let target_ns = self.sample_time.as_nanos().max(1);
      let scale = (target_ns / elapsed_ns).clamp(2, 16) as u64;
      iterations = iterations.saturating_mul(scale);
    }

    let operations_per_sample = operations_per_iteration
      .checked_mul(iterations)
      .expect("operation count overflow");
    let mut ns_per_operation = Vec::with_capacity(self.samples);
    for _ in 0..self.samples {
      let started = Instant::now();
      for _ in 0..iterations {
        f();
      }
      ns_per_operation.push(
        started.elapsed().as_secs_f64() * 1_000_000_000.0
          / operations_per_sample as f64,
      );
    }

    ns_per_operation.sort_by(f64::total_cmp);
    let mean = ns_per_operation.iter().sum::<f64>() / self.samples as f64;
    let variance = ns_per_operation
      .iter()
      .map(|sample| (sample - mean).powi(2))
      .sum::<f64>()
      / self.samples as f64;
    let p50 = percentile(&ns_per_operation, 0.50);
    let p95 = percentile(&ns_per_operation, 0.95);
    let stddev = variance.sqrt();

    eprintln!(
      "{name:<48} {mean:>10.2} ns/op  {:>12.0} ops/s  p95 {p95:.2}  ±{:.1}%",
      1_000_000_000.0 / mean,
      stddev / mean * 100.0,
    );
    if self.json {
      println!(
        "{{\"name\":\"{name}\",\"operations_per_iteration\":{operations_per_iteration},\"iterations_per_sample\":{iterations},\"samples\":{},\"mean_ns_per_op\":{mean:.6},\"p50_ns_per_op\":{p50:.6},\"p95_ns_per_op\":{p95:.6},\"stddev_ns_per_op\":{stddev:.6},\"operations_per_second\":{:.3}}}",
        self.samples,
        1_000_000_000.0 / mean,
      );
    }
  }
}

fn parse_value<T: std::str::FromStr>(
  args: &mut impl Iterator<Item = String>,
  flag: &str,
) -> T {
  args
    .next()
    .unwrap_or_else(|| panic!("{flag} requires a value"))
    .parse()
    .unwrap_or_else(|_| panic!("invalid value for {flag}"))
}

fn percentile(sorted: &[f64], percentile: f64) -> f64 {
  let index = ((sorted.len() - 1) as f64 * percentile).round() as usize;
  sorted[index]
}
