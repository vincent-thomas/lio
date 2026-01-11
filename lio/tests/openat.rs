#![cfg(linux)]
use lio::api::openat;
use std::{
  ffi::CString,
  sync::mpsc::{self, TryRecvError},
};

// #[test]
// #[ignore = "Problematic"]
// fn test_openat_create_file() {
//   lio::init();
//
//   let path = CString::new("/tmp/lio_test_openat_create.txt").unwrap();
//
//   let (sender, receiver) = mpsc::channel();
//
//   // Open/create file for writing
//   openat(
//     libc::AT_FDCWD,
//     path.clone(),
//     libc::O_CREAT | libc::O_WRONLY | libc::O_TRUNC | libc::O_RDONLY,
//   )
//   .send_with(sender.clone());
//
//   assert_eq!(receiver.try_recv().unwrap_err(), TryRecvError::Empty);
//
//   lio::tick();
//
//   let fd = receiver.recv().unwrap().expect("Failed to create file");
//
//   assert!(fd >= 0, "File descriptor should be valid");
//
//   // Close and cleanup
//   unsafe {
//     libc::close(fd);
//     libc::unlink(path.as_ptr());
//   }
// }
//
// #[test]
// #[ignore = "need to figure out permission stuff"]
// fn test_openat_read_only() {
//   lio::init();
//
//   let path = CString::new("./test_openat_readonly_testfile.txt").unwrap();
//
//   let (sender, receiver) = mpsc::channel();
//
//   // Create file first
//   openat(
//     libc::AT_FDCWD,
//     path.clone(),
//     libc::O_CREAT | libc::O_WRONLY | libc::O_TRUNC,
//   )
//   .send_with(sender.clone());
//
//   assert_eq!(receiver.try_recv().unwrap_err(), TryRecvError::Empty);
//
//   lio::tick();
//
//   let fd_create = receiver.recv().unwrap().expect("Failed to create file");
//   unsafe { libc::close(fd_create) };
//
//   let (sender2, receiver2) = mpsc::channel();
//
//   // Open for reading
//   openat(libc::AT_FDCWD, path.clone(), libc::O_RDONLY)
//     .send_with(sender2.clone());
//
//   assert_eq!(receiver2.try_recv().unwrap_err(), TryRecvError::Empty);
//
//   lio::tick();
//
//   let fd = receiver2.recv().unwrap().expect("Failed to open file for reading");
//
//   assert!(fd >= 0, "File descriptor should be valid");
//
//   // Cleanup
//   unsafe {
//     libc::close(fd);
//     libc::unlink(path.as_ptr());
//   }
// }
//
// #[test]
// #[ignore]
// fn test_openat_read_write() {
//   lio::init();
//
//   let path = CString::new("/tmp/lio_test_openat_rdwr.txt").unwrap();
//
//   let (sender, receiver) = mpsc::channel();
//
//   // Open for reading and writing
//   openat(
//     libc::AT_FDCWD,
//     path.clone(),
//     libc::O_CREAT | libc::O_RDWR | libc::O_TRUNC,
//   )
//   .send_with(sender.clone());
//
//   assert_eq!(receiver.try_recv().unwrap_err(), TryRecvError::Empty);
//
//   lio::tick();
//
//   let fd =
//     receiver.recv().unwrap().expect("Failed to open file for read/write");
//
//   assert!(fd >= 0, "File descriptor should be valid");
//
//   // Cleanup
//   unsafe {
//     libc::close(fd);
//     libc::unlink(path.as_ptr());
//   }
// }
//
// #[test]
// #[ignore]
// fn test_openat_nonexistent_file() {
//   lio::init();
//
//   let path = CString::new("/tmp/lio_test_nonexistent_file_12345.txt").unwrap();
//
//   let (sender, receiver) = mpsc::channel();
//
//   // Try to open non-existent file without O_CREAT
//   openat(libc::AT_FDCWD, path, libc::O_RDONLY).send_with(sender.clone());
//
//   assert_eq!(receiver.try_recv().unwrap_err(), TryRecvError::Empty);
//
//   lio::tick();
//
//   let result = receiver.recv().unwrap();
//
//   assert!(result.is_err(), "Should fail to open non-existent file");
// }
//
// #[ignore]
// #[test]
// fn test_openat_with_directory_fd() {
//   lio::init();
//
//   // Open /tmp directory
//   let tmp_path = CString::new("/tmp").unwrap();
//   let dir_fd = unsafe {
//     libc::open(tmp_path.as_ptr(), libc::O_RDONLY | libc::O_DIRECTORY)
//   };
//   assert!(dir_fd >= 0, "Failed to open /tmp directory");
//
//   let (sender, receiver) = mpsc::channel();
//
//   // Open file relative to directory fd
//   let file_path = CString::new("lio_test_openat_dirfd.txt").unwrap();
//   openat(
//     dir_fd,
//     file_path.clone(),
//     libc::O_CREAT | libc::O_RDWR | libc::O_TRUNC,
//   )
//   .send_with(sender.clone());
//
//   // assert_eq!(receiver.try_recv().unwrap_err(), TryRecvError::Empty);
//
//   lio::tick();
//
//   let fd =
//     receiver.recv().unwrap().expect("Failed to open file with directory fd");
//
//   assert!(fd >= 0, "File descriptor should be valid");
//
//   // Cleanup
//   unsafe {
//     libc::close(fd);
//     libc::close(dir_fd);
//     // Full path for cleanup
//     let full_path = CString::new("/tmp/lio_test_openat_dirfd.txt").unwrap();
//     libc::unlink(full_path.as_ptr());
//   }
// }
//
// #[test]
// #[ignore]
// fn test_openat_concurrent() {
//   lio::init();
//
//   let (sender, receiver) = mpsc::channel();
//
//   // Test multiple sequential openat operations
//   for i in 0..10 {
//     let path =
//       CString::new(format!("/tmp/lio_test_openat_concurrent_{}.txt", i))
//         .unwrap();
//     let sender_clone = sender.clone();
//
//     openat(
//       libc::AT_FDCWD,
//       path.clone(),
//       libc::O_CREAT | libc::O_RDWR | libc::O_TRUNC,
//     )
//     .when_done(move |result| {
//       sender_clone.send((result, path)).unwrap();
//     });
//   }
//
//   // assert_eq!(receiver.try_recv().unwrap_err(), TryRecvError::Empty);
//
//   lio::tick();
//
//   for _ in 0..10 {
//     let (result, path) = receiver.recv().unwrap();
//     let fd = result.expect("Failed to open file");
//
//     assert!(fd >= 0);
//
//     unsafe {
//       libc::close(fd);
//       libc::unlink(path.as_ptr());
//     }
//   }
// }
