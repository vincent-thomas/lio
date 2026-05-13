#![allow(
  clippy::duplicate_mod,
  clippy::unnecessary_mut_passed,
  clippy::expect_fun_call
)]

use std::time::{Duration, Instant};

use lio::{
  Lio, api,
  backend::ds::{DSBackend, DSConfig},
};

fn new_ds_lio() -> Lio {
  Lio::new_with_backend(
    DSBackend::with_config(DSConfig { fault_every: 0, ..DSConfig::default() }),
    64,
  )
  .unwrap()
}

fn run_until_recv<T>(
  lio: &mut Lio,
  recv: &api::StreamReceiver<T>,
  timeout: Duration,
) -> T {
  let start = Instant::now();
  loop {
    match recv.try_recv() {
      Ok(item) => return item,
      Err(std::sync::mpsc::TryRecvError::Empty) => {
        if start.elapsed() > timeout {
          panic!("timed out waiting for interval item after {:?}", timeout);
        }
        lio.run_timeout(Duration::from_millis(5)).unwrap();
      }
      Err(std::sync::mpsc::TryRecvError::Disconnected) => {
        panic!("interval stream disconnected unexpectedly");
      }
    }
  }
}

#[test]
fn basic() {
  let mut lio = new_ds_lio();
  let recv = api::interval(Duration::from_millis(20)).with_lio(&lio).send();

  let first = run_until_recv(&mut lio, &recv, Duration::from_secs(1));
  assert!(first.is_ok(), "first interval tick should succeed: {first:?}");

  let second = run_until_recv(&mut lio, &recv, Duration::from_secs(1));
  assert!(second.is_ok(), "second interval tick should succeed: {second:?}");
}

#[test]
fn spacing_is_roughly_periodic() {
  let mut lio = new_ds_lio();
  let period = Duration::from_millis(30);
  let recv = api::interval(period).with_lio(&lio).send();

  let start = Instant::now();
  let first = run_until_recv(&mut lio, &recv, Duration::from_secs(1));
  let first_elapsed = start.elapsed();
  assert!(first.is_ok(), "first interval tick should succeed: {first:?}");

  let second = run_until_recv(&mut lio, &recv, Duration::from_secs(1));
  let second_elapsed = start.elapsed();
  assert!(second.is_ok(), "second interval tick should succeed: {second:?}");

  assert!(
    first_elapsed >= period,
    "first tick should not arrive before period: {:?} >= {:?}",
    first_elapsed,
    period,
  );
  assert!(
    second_elapsed >= period * 2,
    "second tick should not arrive before two periods: {:?} >= {:?}",
    second_elapsed,
    period * 2,
  );
}

#[test]
fn drop_stream_stops_delivery() {
  let mut lio = new_ds_lio();
  let recv = {
    let stream = api::interval(Duration::from_millis(10)).with_lio(&lio);
    stream.send()
  };

  let first = run_until_recv(&mut lio, &recv, Duration::from_secs(1));
  assert!(first.is_ok(), "first interval tick should succeed: {first:?}");

  drop(recv);

  for _ in 0..3 {
    lio.run_timeout(Duration::from_millis(20)).unwrap();
  }
}

#[test]
fn pause_resume_stops_and_restores_ticks() {
  let mut lio = new_ds_lio();
  let recv = api::interval(Duration::from_millis(40)).with_lio(&lio).send();

  let first = run_until_recv(&mut lio, &recv, Duration::from_secs(1));
  assert!(first.is_ok(), "first interval tick should succeed: {first:?}");

  lio::time::pause(&lio);
  std::thread::sleep(Duration::from_millis(120));
  lio.run_timeout(Duration::from_millis(10)).unwrap();
  assert!(
    matches!(recv.try_recv(), Err(std::sync::mpsc::TryRecvError::Empty)),
    "interval should not tick while lio time is paused"
  );

  lio::time::resume(&lio);
  let second = run_until_recv(&mut lio, &recv, Duration::from_secs(1));
  assert!(
    second.is_ok(),
    "interval tick after resume should succeed: {second:?}"
  );
}
