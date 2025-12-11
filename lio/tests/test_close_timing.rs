#![cfg(feature = "high")]
#![cfg(linux)]

use std::time::Instant;

#[test]
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
  lio::tick();
  tee_recv.try_recv().unwrap().unwrap();
  println!("Tee took: {:?}", tee_start.elapsed());

  // Close all file descriptors
  let mut close1_recv = lio::close(pipe1_fds[0]).send();
  let mut close2_recv = lio::close(pipe1_fds[1]).send();
  let mut close3_recv = lio::close(pipe2_fds[0]).send();
  let mut close4_recv = lio::close(pipe2_fds[1]).send();

  lio::tick();

  close1_recv.try_recv().unwrap().unwrap();
  close2_recv.try_recv().unwrap().unwrap();
  close3_recv.try_recv().unwrap().unwrap();
  close4_recv.try_recv().unwrap().unwrap();

  println!("Total execution took: {:?}", start.elapsed());
}
