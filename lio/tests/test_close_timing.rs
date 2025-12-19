#![cfg(feature = "high")]
#![cfg(linux)]

use std::time::Instant;

#[test]
#[ignore]
fn test_close_timing() {
  lio::init();

  let start = Instant::now();
  let mut pipe1_fds = [0i32; 2];
  let mut pipe2_fds = [0i32; 2];
  unsafe {
    assert_eq!(libc::pipe(pipe1_fds.as_mut_ptr()), 0);
    assert_eq!(libc::pipe(pipe2_fds.as_mut_ptr()), 0);
  }

  // Write some data
  let test_data = b"Hello!";
  unsafe {
    libc::write(
      pipe1_fds[1],
      test_data.as_ptr() as *const libc::c_void,
      test_data.len(),
    );
  }

  // Do a tee operation
  let tee_start = Instant::now();
  let mut tee_recv =
    lio::tee(pipe1_fds[0], pipe2_fds[1], test_data.len() as u32).send();

  // Try multiple ticks - some operations need more than one
  for i in 0..10 {
    lio::tick();
    if let Some(result) = tee_recv.try_recv() {
      result.expect("test_close_timing: tee operation failed");
      break;
    }
    if i == 9 {
      panic!(
        "test_close_timing: tee operation did not complete after 10 ticks"
      );
    }
  }
  println!("Tee took: {:?}", tee_start.elapsed());

  // Close all file descriptors
  let mut close1_recv = lio::close(pipe1_fds[0]).send();
  let mut close2_recv = lio::close(pipe1_fds[1]).send();
  let mut close3_recv = lio::close(pipe2_fds[0]).send();
  let mut close4_recv = lio::close(pipe2_fds[1]).send();

  // Try multiple ticks for close operations too
  for i in 0..10 {
    lio::tick();

    let close1_done = close1_recv.try_recv().is_some();
    let close2_done = close2_recv.try_recv().is_some();
    let close3_done = close3_recv.try_recv().is_some();
    let close4_done = close4_recv.try_recv().is_some();

    if close1_done && close2_done && close3_done && close4_done {
      break;
    }

    if i == 9 {
      panic!(
        "test_close_timing: not all close operations completed after 10 ticks"
      );
    }
  }

  println!("Total execution took: {:?}", start.elapsed());
}
