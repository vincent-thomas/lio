use lio::{
  Lio,
  api::{resource::Resource, truncate},
};
use std::{ffi::CString, os::fd::FromRawFd, sync::mpsc};

#[test]
fn test_truncate_shrink_file() {
  let mut lio = Lio::new(256).unwrap();

  let path = CString::new("/tmp/lio_test_truncate_shrink.txt").unwrap();

  // Create file with data
  let fd = unsafe {
    let fd = libc::open(
      path.as_ptr(),
      libc::O_CREAT | libc::O_RDWR | libc::O_TRUNC,
      0o644,
    );
    libc::write(fd, b"0123456789ABCDEF".as_ptr() as *const libc::c_void, 16);
    fd
  };

  let (sender, receiver) = mpsc::channel();
  let sender1 = sender.clone();

  // Truncate to 5 bytes
  truncate(&unsafe { Resource::from_raw_fd(fd) }, 5)
    .with_lio(&mut lio)
    .send_with(sender1);

  // assert_eq!(receiver.try_recv().unwrap_err(), TryRecvError::Empty);
  lio.try_run().unwrap();

  receiver.recv().unwrap().expect("Failed to truncate file");

  // Verify size
  unsafe {
    let mut stat: libc::stat = std::mem::zeroed();
    libc::fstat(fd, &mut stat);
    assert_eq!(stat.st_size, 5);

    // Verify content
    let mut buf = vec![0u8; 10];
    libc::lseek(fd, 0, libc::SEEK_SET);
    let read_bytes = libc::read(fd, buf.as_mut_ptr() as *mut libc::c_void, 10);
    assert_eq!(read_bytes, 5);
    assert_eq!(&buf[..5], b"01234");

    libc::close(fd);
    libc::unlink(path.as_ptr());
  }
}

#[test]
fn test_truncate_extend_file() {
  let mut lio = Lio::new(256).unwrap();

  let path = CString::new("/tmp/lio_test_truncate_extend.txt").unwrap();

  // Create file with data
  let fd = unsafe {
    let fd = libc::open(
      path.as_ptr(),
      libc::O_CREAT | libc::O_RDWR | libc::O_TRUNC,
      0o644,
    );
    libc::write(fd, b"Hello".as_ptr() as *const libc::c_void, 5);
    fd
  };

  let (sender, receiver) = mpsc::channel();
  let sender1 = sender.clone();

  // Extend to 20 bytes
  truncate(&unsafe { Resource::from_raw_fd(fd) }, 20)
    .with_lio(&mut lio)
    .send_with(sender1);

  lio.try_run().unwrap();

  // assert_eq!(receiver.try_recv().unwrap_err(), TryRecvError::Empty);

  receiver.try_recv().unwrap().expect("Failed to truncate file");

  // Verify size
  unsafe {
    let mut stat: libc::stat = std::mem::zeroed();
    libc::fstat(fd, &mut stat);
    assert_eq!(stat.st_size, 20);

    // Verify content (extended part should be zeros)
    let mut buf = vec![0u8; 20];
    libc::lseek(fd, 0, libc::SEEK_SET);
    let read_bytes = libc::read(fd, buf.as_mut_ptr() as *mut libc::c_void, 20);
    assert_eq!(read_bytes, 20);
    assert_eq!(&buf[..5], b"Hello");
    assert_eq!(&buf[5..20], &[0u8; 15]);

    libc::close(fd);
    libc::unlink(path.as_ptr());
  }
}

#[test]
fn test_truncate_to_zero() {
  let mut lio = Lio::new(256).unwrap();

  let path = CString::new("/tmp/lio_test_truncate_zero.txt").unwrap();

  // Create file with data
  let fd = unsafe {
    let fd = libc::open(
      path.as_ptr(),
      libc::O_CREAT | libc::O_RDWR | libc::O_TRUNC,
      0o644,
    );
    libc::write(fd, b"Some data here".as_ptr() as *const libc::c_void, 14);
    fd
  };

  let (sender, receiver) = mpsc::channel();
  let sender1 = sender.clone();

  // Truncate to 0 bytes
  truncate(&unsafe { Resource::from_raw_fd(fd) }, 0)
    .with_lio(&mut lio)
    .send_with(sender1);

  // assert_eq!(receiver.try_recv().unwrap_err(), TryRecvError::Empty);
  lio.try_run().unwrap();

  receiver.try_recv().unwrap().expect("Failed to truncate file to zero");

  // Verify size
  unsafe {
    let mut stat: libc::stat = std::mem::zeroed();
    libc::fstat(fd, &mut stat);
    assert_eq!(stat.st_size, 0);

    // Try to read (should get 0 bytes)
    let mut buf = vec![0u8; 10];
    libc::lseek(fd, 0, libc::SEEK_SET);
    let read_bytes = libc::read(fd, buf.as_mut_ptr() as *mut libc::c_void, 10);
    assert_eq!(read_bytes, 0);

    libc::close(fd);
    libc::unlink(path.as_ptr());
  }
}

#[test]
fn test_truncate_same_size() {
  let mut lio = Lio::new(256).unwrap();

  let path = CString::new("/tmp/lio_test_truncate_same.txt").unwrap();

  let test_data = b"ExactSize";

  // Create file with data
  let fd = unsafe {
    let fd = libc::open(
      path.as_ptr(),
      libc::O_CREAT | libc::O_RDWR | libc::O_TRUNC,
      0o644,
    );
    libc::write(fd, test_data.as_ptr() as *const libc::c_void, test_data.len());
    fd
  };

  let (sender, receiver) = mpsc::channel();
  let sender1 = sender.clone();

  // Truncate to same size
  truncate(&unsafe { Resource::from_raw_fd(fd) }, test_data.len() as u64)
    .with_lio(&mut lio)
    .send_with(sender1);

  // assert_eq!(receiver.try_recv().unwrap_err(), TryRecvError::Empty);
  lio.try_run().unwrap();

  receiver.try_recv().unwrap().expect("Failed to truncate file");

  // Verify size and content unchanged
  unsafe {
    let mut stat: libc::stat = std::mem::zeroed();
    libc::fstat(fd, &mut stat);
    assert_eq!(stat.st_size as usize, test_data.len());

    let mut buf = vec![0u8; test_data.len()];
    libc::lseek(fd, 0, libc::SEEK_SET);
    let read_bytes =
      libc::read(fd, buf.as_mut_ptr() as *mut libc::c_void, test_data.len());
    assert_eq!(read_bytes as usize, test_data.len());
    assert_eq!(&buf, test_data);

    libc::close(fd);
    libc::unlink(path.as_ptr());
  }
}

#[test]
fn test_truncate_then_write() {
  let mut lio = Lio::new(256).unwrap();

  let path = CString::new("/tmp/lio_test_truncate_write.txt").unwrap();

  // Create file with data
  let fd = unsafe {
    let fd = libc::open(
      path.as_ptr(),
      libc::O_CREAT | libc::O_RDWR | libc::O_TRUNC,
      0o644,
    );
    libc::write(fd, b"Old data here".as_ptr() as *const libc::c_void, 13);
    fd
  };

  let (sender, receiver) = mpsc::channel();
  let sender1 = sender.clone();

  // Truncate to 5 bytes
  truncate(&unsafe { Resource::from_raw_fd(fd) }, 5)
    .with_lio(&mut lio)
    .send_with(sender1);

  // assert_eq!(receiver.try_recv().unwrap_err(), TryRecvError::Empty);
  lio.try_run().unwrap();

  receiver.try_recv().unwrap().expect("Failed to truncate file");

  // Write new data
  unsafe {
    libc::lseek(fd, 5, libc::SEEK_SET);
    libc::write(fd, b"New".as_ptr() as *const libc::c_void, 3);
  }

  // Verify
  unsafe {
    let mut buf = vec![0u8; 10];
    libc::lseek(fd, 0, libc::SEEK_SET);
    let read_bytes = libc::read(fd, buf.as_mut_ptr() as *mut libc::c_void, 10);
    assert_eq!(read_bytes, 8);
    assert_eq!(&buf[..8], b"Old dNew");

    libc::close(fd);
    libc::unlink(path.as_ptr());
  }
}

#[test]
fn test_truncate_large_file() {
  let mut lio = lio::Lio::new(256).unwrap();

  let path = CString::new("/tmp/lio_test_truncate_large.txt").unwrap();

  // Create file with large data (1MB)
  let large_data: Vec<u8> = (0..1024 * 1024).map(|i| (i % 256) as u8).collect();
  let fd = unsafe {
    let fd = libc::open(
      path.as_ptr(),
      libc::O_CREAT | libc::O_RDWR | libc::O_TRUNC,
      0o644,
    );
    libc::write(
      fd,
      large_data.as_ptr() as *const libc::c_void,
      large_data.len(),
    );
    fd
  };

  let (sender, receiver) = mpsc::channel();
  let sender1 = sender.clone();

  // Truncate to 1KB
  truncate(&unsafe { Resource::from_raw_fd(fd) }, 1024)
    .with_lio(&mut lio)
    .send_with(sender1);

  // assert_eq!(receiver.try_recv().unwrap_err(), TryRecvError::Empty);
  lio.try_run().unwrap();

  receiver.try_recv().unwrap().expect("Failed to truncate large file");

  // Verify size
  unsafe {
    let mut stat: libc::stat = std::mem::zeroed();
    libc::fstat(fd, &mut stat);
    assert_eq!(stat.st_size, 1024);

    // Verify content
    let mut buf = vec![0u8; 1024];
    libc::lseek(fd, 0, libc::SEEK_SET);
    let read_bytes =
      libc::read(fd, buf.as_mut_ptr() as *mut libc::c_void, 1024);
    assert_eq!(read_bytes, 1024);
    assert_eq!(&buf, &large_data[..1024]);

    libc::close(fd);
    libc::unlink(path.as_ptr());
  }
}

#[test]
fn test_truncate_multiple_times() {
  let mut lio = lio::Lio::new(256).unwrap();

  let path = CString::new("/tmp/lio_test_truncate_multiple.txt").unwrap();

  // Create file
  let fd = unsafe {
    let fd = libc::open(
      path.as_ptr(),
      libc::O_CREAT | libc::O_RDWR | libc::O_TRUNC,
      0o644,
    );
    libc::write(fd, b"0123456789".as_ptr() as *const libc::c_void, 10);
    fd
  };

  let (sender, receiver) = mpsc::channel();

  // Truncate multiple times
  let sender1 = sender.clone();
  truncate(&unsafe { Resource::from_raw_fd(fd) }, 8)
    .with_lio(&mut lio)
    .send_with(sender1);
  // assert_eq!(receiver.try_recv().unwrap_err(), TryRecvError::Empty);
  lio.try_run().unwrap();
  receiver.try_recv().unwrap().expect("First truncate failed");

  let sender2 = sender.clone();
  truncate(&unsafe { Resource::from_raw_fd(fd) }, 5)
    .with_lio(&mut lio)
    .when_done(move |res| {
      sender2.send(res).unwrap();
    });
  // assert_eq!(receiver.try_recv().unwrap_err(), TryRecvError::Empty);
  lio.try_run().unwrap();
  receiver.recv().unwrap().expect("Second truncate failed");

  let sender3 = sender.clone();
  truncate(&unsafe { Resource::from_raw_fd(fd) }, 10)
    .with_lio(&mut lio)
    .when_done(move |res| {
      sender3.send(res).unwrap();
    });
  // assert_eq!(receiver.try_recv().unwrap_err(), TryRecvError::Empty);
  lio.try_run().unwrap();
  receiver.recv().unwrap().expect("Third truncate failed");

  let sender4 = sender.clone();
  truncate(&unsafe { Resource::from_raw_fd(fd) }, 3)
    .with_lio(&mut lio)
    .when_done(move |res| {
      sender4.send(res).unwrap();
    });
  // assert_eq!(receiver.try_recv().unwrap_err(), TryRecvError::Empty);
  lio.try_run().unwrap();
  receiver.recv().unwrap().expect("Fourth truncate failed");

  // Verify final size
  unsafe {
    let mut stat: libc::stat = std::mem::zeroed();
    libc::fstat(fd, &mut stat);
    assert_eq!(stat.st_size, 3);

    let mut buf = vec![0u8; 10];
    libc::lseek(fd, 0, libc::SEEK_SET);
    let read_bytes = libc::read(fd, buf.as_mut_ptr() as *mut libc::c_void, 10);
    assert_eq!(read_bytes, 3);
    assert_eq!(&buf[..3], b"012");

    libc::close(fd);
    libc::unlink(path.as_ptr());
  }
}

#[test]
fn test_truncate_concurrent() {
  let mut lio = lio::Lio::new(256).unwrap();

  // Test truncating multiple files concurrently
  for i in 0..10 {
    let path =
      CString::new(format!("/tmp/lio_test_truncate_concurrent_{}.txt", i))
        .unwrap();

    let fd = unsafe {
      let fd = libc::open(
        path.as_ptr(),
        libc::O_CREAT | libc::O_RDWR | libc::O_TRUNC,
        0o644,
      );
      libc::write(fd, b"0123456789ABCDEF".as_ptr() as *const libc::c_void, 16);
      fd
    };

    let (sender, receiver) = mpsc::channel();

    truncate(&unsafe { Resource::from_raw_fd(fd) }, 5)
      .with_lio(&mut lio)
      .send_with(sender.clone());

    // assert_eq!(receiver.try_recv().unwrap_err(), TryRecvError::Empty);
    // lio::tick();
    lio.try_run().unwrap();

    receiver.try_recv().unwrap().expect("Failed to truncate");

    unsafe {
      let mut stat: libc::stat = std::mem::zeroed();
      libc::fstat(fd, &mut stat);
      assert_eq!(stat.st_size, 5);

      libc::close(fd);
      libc::unlink(path.as_ptr());
    }
  }
}
