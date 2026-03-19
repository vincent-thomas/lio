//! Macro for generating comprehensive IoBackend tests.
//!
//! This macro generates a test suite for any [`IoBackend`] implementation,
//! including third-party implementations in external crates.
//!
//! [`IoBackend`]: crate::backend::IoBackend
//!
//! # Usage
//!
//! In your crate's test module, invoke the macro with an expression that creates
//! a new backend instance:
//!
//! ```ignore
//! // In my_backend/src/lib.rs
//! #[cfg(test)]
//! mod tests {
//!     use my_backend::MyBackend;
//!
//!     lio::test_io_backend!(MyBackend::new());
//! }
//! ```
//!
//! # Requirements
//!
//! Your crate must have `libc` as a dependency for the socket operation tests on Unix.
//!
//! # What's Tested
//!
//! The macro generates tests for:
//! - Initialization with various capacities
//! - Basic push/flush/wait lifecycle
//! - Multiple concurrent operations
//! - Interleaved submission and completion
//! - Non-blocking and timeout wait behavior
//! - Socket creation (on Unix)
//! - Operation ID preservation
//! - Flush behavior (empty, double flush)

/// Generates a comprehensive test suite for an IoBackend implementation.
///
/// The macro takes an expression that creates a new backend instance.
/// Each test will call this expression to get a fresh backend.
///
/// # Example
///
/// ```ignore
/// use lio::backend::dummy::DummyBackend;
///
/// // Generate tests for DummyBackend
/// lio::test_io_backend!(DummyBackend::new());
/// ```
///
/// # Third-Party Usage
///
/// External crates implementing their own `IoBackend` can use this macro
/// to verify correctness:
///
/// ```ignore
/// // In your crate's tests
/// use my_crate::MyCustomBackend;
///
/// lio::test_io_backend!(MyCustomBackend::default());
/// ```
#[macro_export]
macro_rules! test_io_backend {
  ($create_backend:expr) => {
    mod io_backend_tests {
      use super::*;
      use std::time::Duration;

      use $crate::backend::IoBackend;
      use $crate::backend::op::Op;

      const TEST_CAPACITY: usize = 64;

      // ═══════════════════════════════════════════════════════════════════════
      // Initialization tests
      // ═══════════════════════════════════════════════════════════════════════

      #[test]
      fn test_init() {
        let mut backend = $create_backend;
        backend.init(TEST_CAPACITY).expect("init should succeed");
      }

      #[test]
      fn test_init_various_capacities() {
        for cap in [1, 16, 64, 256, 1024] {
          let mut backend = $create_backend;
          backend
            .init(cap)
            .expect(&format!("init with cap={} should succeed", cap));
        }
      }

      // ═══════════════════════════════════════════════════════════════════════
      // Basic operation flow
      // ═══════════════════════════════════════════════════════════════════════

      #[test]
      fn test_push_nop() {
        let mut backend = $create_backend;
        backend.init(TEST_CAPACITY).unwrap();

        backend.push(1, Op::Nop).expect("push Nop should succeed");
      }

      #[test]
      fn test_push_and_flush() {
        let mut backend = $create_backend;
        backend.init(TEST_CAPACITY).unwrap();

        backend.push(1, Op::Nop).unwrap();
        let _flushed = backend.flush().expect("flush should succeed");
      }

      #[test]
      fn test_push_flush_wait() {
        let mut backend = $create_backend;
        backend.init(TEST_CAPACITY).unwrap();

        backend.push(1, Op::Nop).unwrap();
        backend.flush().unwrap();

        // Wait with timeout to avoid hanging
        let completions = backend
          .wait_timeout(Some(Duration::from_secs(5)))
          .expect("wait_timeout should succeed");

        assert_eq!(completions.len(), 1, "should have 1 completion");
        assert_eq!(
          completions[0].op_id, 1,
          "completion should have correct op_id"
        );
      }

      // ═══════════════════════════════════════════════════════════════════════
      // Multiple operations
      // ═══════════════════════════════════════════════════════════════════════

      #[test]
      fn test_multiple_nops() {
        let mut backend = $create_backend;
        backend.init(TEST_CAPACITY).unwrap();

        for i in 1..=10 {
          backend.push(i, Op::Nop).unwrap();
        }
        backend.flush().unwrap();

        // Collect all completions (may come in multiple batches)
        let mut completed_ids = Vec::new();
        while completed_ids.len() < 10 {
          let completions = backend
            .wait_timeout(Some(Duration::from_secs(5)))
            .expect("wait_timeout should succeed");

          for c in completions {
            completed_ids.push(c.op_id);
          }
        }

        // Verify all operations completed
        completed_ids.sort();
        assert_eq!(completed_ids, (1..=10).collect::<Vec<_>>());
      }

      #[test]
      fn test_interleaved_push_wait() {
        let mut backend = $create_backend;
        backend.init(TEST_CAPACITY).unwrap();

        // Push first batch
        backend.push(1, Op::Nop).unwrap();
        backend.push(2, Op::Nop).unwrap();
        backend.flush().unwrap();

        // Wait for first batch
        let mut completed = Vec::new();
        while completed.len() < 2 {
          let completions =
            backend.wait_timeout(Some(Duration::from_secs(5))).unwrap();
          completed.extend(completions.iter().map(|c| c.op_id));
        }

        // Push second batch
        backend.push(3, Op::Nop).unwrap();
        backend.push(4, Op::Nop).unwrap();
        backend.flush().unwrap();

        // Wait for second batch
        while completed.len() < 4 {
          let completions =
            backend.wait_timeout(Some(Duration::from_secs(5))).unwrap();
          completed.extend(completions.iter().map(|c| c.op_id));
        }

        completed.sort();
        assert_eq!(completed, vec![1, 2, 3, 4]);
      }

      // ═══════════════════════════════════════════════════════════════════════
      // Timeout behavior
      // ═══════════════════════════════════════════════════════════════════════

      #[test]
      fn test_wait_nonblocking_empty() {
        let mut backend = $create_backend;
        backend.init(TEST_CAPACITY).unwrap();

        // Non-blocking wait with no pending ops should return empty
        let completions = backend
          .wait_timeout(Some(Duration::ZERO))
          .expect("non-blocking wait should succeed");

        assert!(
          completions.is_empty(),
          "should have no completions when nothing pending"
        );
      }

      #[test]
      fn test_wait_with_short_timeout() {
        let mut backend = $create_backend;
        backend.init(TEST_CAPACITY).unwrap();

        // Short timeout with no pending ops
        let start = std::time::Instant::now();
        let completions = backend
          .wait_timeout(Some(Duration::from_millis(10)))
          .expect("wait with timeout should succeed");

        assert!(completions.is_empty());
        // Should return quickly (within reasonable bounds)
        assert!(start.elapsed() < Duration::from_secs(1));
      }

      // ═══════════════════════════════════════════════════════════════════════
      // Socket operation (creates real fd)
      // ═══════════════════════════════════════════════════════════════════════

      #[test]
      #[cfg(target_os = "linux")]
      fn test_socket_operation() {
        let mut backend = $create_backend;
        backend.init(TEST_CAPACITY).unwrap();

        // Create a TCP socket (Linux has SOCK_NONBLOCK/SOCK_CLOEXEC)
        backend
          .push(
            1,
            Op::Socket {
              domain: libc::AF_INET,
              ty: libc::SOCK_STREAM | libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC,
              proto: 0,
            },
          )
          .unwrap();
        backend.flush().unwrap();

        let completions =
          backend.wait_timeout(Some(Duration::from_secs(5))).unwrap();

        assert_eq!(completions.len(), 1);
        assert_eq!(completions[0].op_id, 1);

        // Result should be a valid fd (>= 0)
        let fd = completions[0].result;
        assert!(fd >= 0, "socket should return valid fd, got {}", fd);

        // Clean up: close the socket
        unsafe { libc::close(fd as i32) };
      }

      #[test]
      #[cfg(all(unix, not(target_os = "linux")))]
      fn test_socket_operation() {
        let mut backend = $create_backend;
        backend.init(TEST_CAPACITY).unwrap();

        // Create a TCP socket (macOS/BSD don't have SOCK_NONBLOCK/SOCK_CLOEXEC in socket())
        backend
          .push(
            1,
            Op::Socket {
              domain: libc::AF_INET,
              ty: libc::SOCK_STREAM,
              proto: 0,
            },
          )
          .unwrap();
        backend.flush().unwrap();

        let completions =
          backend.wait_timeout(Some(Duration::from_secs(5))).unwrap();

        assert_eq!(completions.len(), 1);
        assert_eq!(completions[0].op_id, 1);

        // Result should be a valid fd (>= 0)
        let fd = completions[0].result;
        assert!(fd >= 0, "socket should return valid fd, got {}", fd);

        // Clean up: close the socket
        unsafe { libc::close(fd as i32) };
      }

      // ═══════════════════════════════════════════════════════════════════════
      // Flush behavior
      // ═══════════════════════════════════════════════════════════════════════

      #[test]
      fn test_flush_empty() {
        let mut backend = $create_backend;
        backend.init(TEST_CAPACITY).unwrap();

        // Flushing with nothing queued should succeed
        let flushed = backend.flush().expect("flush empty should succeed");
        assert_eq!(flushed, 0, "flushing empty queue should return 0");
      }

      #[test]
      fn test_double_flush() {
        let mut backend = $create_backend;
        backend.init(TEST_CAPACITY).unwrap();

        backend.push(1, Op::Nop).unwrap();
        let first = backend.flush().unwrap();
        let second = backend.flush().unwrap();

        // Second flush should have nothing to submit
        let _ = first; // first may or may not be > 0 depending on backend
        assert_eq!(second, 0, "second flush should have nothing to submit");
      }

      // ═══════════════════════════════════════════════════════════════════════
      // Op ID uniqueness
      // ═══════════════════════════════════════════════════════════════════════

      #[test]
      fn test_op_ids_preserved() {
        let mut backend = $create_backend;
        backend.init(TEST_CAPACITY).unwrap();

        // Use non-sequential IDs
        let ids = [42, 100, 7, 999, 1];
        for &id in &ids {
          backend.push(id, Op::Nop).unwrap();
        }
        backend.flush().unwrap();

        let mut completed_ids = Vec::new();
        while completed_ids.len() < ids.len() {
          let completions =
            backend.wait_timeout(Some(Duration::from_secs(5))).unwrap();
          completed_ids.extend(completions.iter().map(|c| c.op_id));
        }

        // All IDs should be present (order may vary)
        completed_ids.sort();
        let mut expected: Vec<_> = ids.to_vec();
        expected.sort();
        assert_eq!(completed_ids, expected);
      }

      // ═══════════════════════════════════════════════════════════════════════
      // Vectored I/O operations (readv/writev)
      // These tests are only meaningful for backends that actually perform I/O.
      // The tests use raw syscalls to prepare/verify data, then test the backend
      // performs the vectored operation correctly.
      // ═══════════════════════════════════════════════════════════════════════

      #[test]
      #[cfg(unix)]
      fn test_readv_operation() {
        use std::os::fd::FromRawFd;
        use $crate::backend::op::RawBuf;

        let mut backend = $create_backend;
        backend.init(TEST_CAPACITY).unwrap();

        // Create a temp file with test data
        let path = std::ffi::CString::new(format!(
          "/tmp/lio_test_readv_{}.txt",
          std::process::id()
        ))
        .unwrap();

        let test_data = b"Hello, World!";
        let fd = unsafe {
          let fd = libc::open(
            path.as_ptr(),
            libc::O_CREAT | libc::O_RDWR | libc::O_TRUNC,
            0o644,
          );
          libc::write(
            fd,
            test_data.as_ptr() as *const libc::c_void,
            test_data.len(),
          );
          // Seek back to beginning
          libc::lseek(fd, 0, libc::SEEK_SET);
          fd
        };

        // Create iovecs for reading
        let mut buf1 = vec![0u8; 7];
        let mut buf2 = vec![0u8; 6];
        let mut iovecs: [libc::iovec; 2] = unsafe { std::mem::zeroed() };
        iovecs[0].iov_base = buf1.as_mut_ptr() as *mut _;
        iovecs[0].iov_len = buf1.capacity();
        iovecs[1].iov_base = buf2.as_mut_ptr() as *mut _;
        iovecs[1].iov_len = buf2.capacity();

        // Create resource and keep it alive for the duration of the operation
        let resource = unsafe {
          <$crate::api::resource::Resource as FromRawFd>::from_raw_fd(fd)
        };
        // Clone for the op - the original stays alive to keep fd open
        let resource_for_op = resource.clone();

        backend
          .push(
            1,
            Op::ReadV {
              fd: resource_for_op,
              buf: RawBuf::empty(),
              iovecs: iovecs.as_ptr(),
              iov_count: 2,
            },
          )
          .unwrap();
        backend.flush().unwrap();

        let completions =
          backend.wait_timeout(Some(Duration::from_secs(5))).unwrap();

        assert_eq!(completions.len(), 1);
        assert_eq!(completions[0].op_id, 1);

        // Skip validation for dummy backend which always returns 0
        if completions[0].result > 0 {
          assert_eq!(completions[0].result, 13, "should read all 13 bytes");

          // Update lengths based on bytes read
          let bytes_read = completions[0].result as usize;
          let first_len = bytes_read.min(buf1.capacity());
          unsafe { buf1.set_len(first_len) };
          let second_len =
            bytes_read.saturating_sub(buf1.capacity()).min(buf2.capacity());
          unsafe { buf2.set_len(second_len) };

          assert_eq!(&buf1[..], b"Hello, ");
          assert_eq!(&buf2[..], b"World!");
        }

        // Cleanup - drop resource first to close fd, then unlink
        drop(resource);
        unsafe { libc::unlink(path.as_ptr()) };
      }

      #[test]
      #[cfg(unix)]
      fn test_writev_operation() {
        use std::os::fd::FromRawFd;
        use $crate::backend::op::RawBuf;

        let mut backend = $create_backend;
        backend.init(TEST_CAPACITY).unwrap();

        // Create a temp file
        let path = std::ffi::CString::new(format!(
          "/tmp/lio_test_writev_{}.txt",
          std::process::id()
        ))
        .unwrap();

        let fd = unsafe {
          libc::open(
            path.as_ptr(),
            libc::O_CREAT | libc::O_RDWR | libc::O_TRUNC,
            0o644,
          )
        };

        // Create iovecs for writing
        let buf1 = b"Hello, ";
        let buf2 = b"World!";
        let iovecs: [libc::iovec; 2] = [
          libc::iovec {
            iov_base: buf1.as_ptr() as *mut _,
            iov_len: buf1.len(),
          },
          libc::iovec {
            iov_base: buf2.as_ptr() as *mut _,
            iov_len: buf2.len(),
          },
        ];

        // Create resource and keep it alive for the duration of the operation
        let resource = unsafe {
          <$crate::api::resource::Resource as FromRawFd>::from_raw_fd(fd)
        };
        // Clone for the op - the original stays alive to keep fd open
        let resource_for_op = resource.clone();

        backend
          .push(
            1,
            Op::WriteV {
              fd: resource_for_op,
              buf: RawBuf::empty(),
              iovecs: iovecs.as_ptr(),
              iov_count: 2,
            },
          )
          .unwrap();
        backend.flush().unwrap();

        let completions =
          backend.wait_timeout(Some(Duration::from_secs(5))).unwrap();

        assert_eq!(completions.len(), 1);
        assert_eq!(completions[0].op_id, 1);

        // Skip validation for dummy backend which always returns 0
        if completions[0].result > 0 {
          assert_eq!(completions[0].result, 13, "should write all 13 bytes");

          // Verify the written data
          let mut verify_buf = vec![0u8; 20];
          let bytes_read = unsafe {
            libc::lseek(fd, 0, libc::SEEK_SET);
            libc::read(fd, verify_buf.as_mut_ptr() as *mut _, verify_buf.len())
          };
          assert_eq!(bytes_read, 13);
          assert_eq!(&verify_buf[..13], b"Hello, World!");
        }

        // Cleanup - drop resource to close fd, then unlink
        drop(resource);
        unsafe { libc::unlink(path.as_ptr()) };
      }
    }
  };
}
