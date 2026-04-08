/// Shared contract tests for [`IoBackend`] implementations.
///
/// Backends that pass `test_io_backend!` conform to the standard observable
/// contract for the deterministic scenarios covered here.
///
/// Runtime / queue semantics:
/// - empty `wait()` after initialization yields no completions
/// - pushing more than the initialized backend capacity panics
/// - `wait(Duration::ZERO)` never blocks
/// - `wait(None)` blocks until at least one completion is available
/// - `flush()` with an empty backlog is harmless
/// - `flush()` is allowed to produce completions without creating pending work
/// - immediate completions queued during `flush()` are surfaced on the next `wait()`
/// - batched immediate completions are surfaced together with exact results
/// - a completed `wait()` buffer is cleared on the next `wait()`
/// - completions are not duplicated across `wait()` calls
/// - deferred readiness completions are delivered exactly once
/// - mixed pending batches complete each registration exactly once
/// - completions are routed to the correct registration id
///
/// `Op::Nop`:
/// - produces one immediate success completion with exact result `0`
/// - completion is surfaced on the next `wait()` and never replayed
///
/// `Op::Read`:
/// - success may return short positive byte counts, but repeated successful
///   submissions must eventually transfer the exact total byte count
/// - vectored success may return short positive byte counts, but repeated
///   successful submissions must eventually transfer the exact total byte count
/// - pending reads remain pending until data arrives
/// - EOF returns exact `0`
/// - invalid offset returns exact raw `-EINVAL`
/// - unsupported flags return exact raw `-ENOTSUP`
/// - null iovec pointer returns exact raw `-EINVAL`
/// - invalid fd returns exact raw `-EBADF`
///
/// `Op::Write`:
/// - success may return short positive byte counts, but repeated successful
///   submissions must eventually transfer the exact total byte count
/// - vectored success may return short positive byte counts, but repeated
///   successful submissions must eventually transfer the exact total byte count
/// - invalid offset returns exact raw `-EINVAL`
/// - unsupported flags return exact raw `-ENOTSUP`
/// - null iovec pointer returns exact raw `-EINVAL`
/// - invalid fd returns exact raw `-EBADF`
///
/// `Op::Recv`:
/// - success may return short positive byte counts, but repeated successful
///   submissions must eventually transfer the exact total byte count
/// - vectored success may return short positive byte counts, but repeated
///   successful submissions must eventually transfer the exact total byte count
/// - pending receives remain pending until data arrives
/// - EOF returns exact `0`
/// - invalid fd returns exact raw `-EBADF`
///
/// `Op::Send`:
/// - success may return short positive byte counts, but repeated successful
///   submissions must eventually transfer the exact total byte count
/// - vectored success may return short positive byte counts, but repeated
///   successful submissions must eventually transfer the exact total byte count
/// - invalid fd returns exact raw `-EBADF`
///
/// `Op::Accept`:
/// - success produces exactly one non-error completion on Unix platforms where
///   local listener registration is supported
/// - pending accepts remain pending until a client arrives
/// - invalid fd returns exact raw `-EBADF`
///
/// `Op::Connect`:
/// - success returns exact `0` on Unix platforms where local listener
///   registration is supported
/// - missing Unix socket path returns exact raw `-ENOENT`
/// - invalid fd returns exact raw `-EBADF`
///
/// `Op::UnlinkAt`:
/// - success returns exact `0`
/// - missing path returns exact raw `-ENOENT`
///
/// `Op::RenameAt`:
/// - success returns exact `0`
/// - missing source returns exact raw `-ENOENT`
///
/// `Op::MkdirAt`:
/// - success returns exact `0`
/// - existing path returns exact raw `-EEXIST`
///
/// `Op::LinkAt`:
/// - hard-link success returns exact `0`
/// - symbolic-link success returns exact `0`
/// - hard-link with missing source returns exact raw `-ENOENT`
///
/// `Op::ReadlinkAt`:
/// - success returns the exact symlink-target byte count
/// - missing path returns exact raw `-ENOENT`
///
/// The suite intentionally avoids nondeterministic network cases. It only tests
/// scenarios where the expected raw `isize` output is stable for a conforming
/// backend.
#[macro_export]
macro_rules! test_io_backend {
  ($backend_ctor:expr) => {
    #[cfg(test)]
    mod io_backend_contract {
      use super::*;
      use std::{
        time::{Duration, SystemTime, Instant},
        env, fs, mem, path::PathBuf, thread,
        os::{
          unix::{ffi::OsStrExt, net::{UnixListener, UnixStream}},
          fd::{FromRawFd, IntoRawFd, RawFd}
        },
      };

      use $crate::api::resource::Resource;
      use $crate::backend::{
        IoBackend,
        op::{LinkKind, MsgBuf, MsgBufMut, MsgRecv, MsgSend, Op, RawBuf},
      };

      fn new_backend() -> impl IoBackend {
        $backend_ctor
      }

      fn assert_exact_result(
        completed: &$crate::backend::OpCompleted,
        id: u64,
        expected: isize,
      ) {
        assert_eq!(completed.registration_id(), id);
        assert_eq!(
          completed.result(),
          expected,
          "unexpected raw result for registration {}",
          id
        );
      }

      #[test]
      fn init_and_empty_wait() {
        let mut backend = new_backend();
        backend.init(64).unwrap();
        backend.flush().unwrap();

        let completed = backend.wait(Some(Duration::ZERO)).unwrap();
        assert!(
          completed.is_empty(),
          "backend returned completions without submitted operations"
        );
      }

      #[test]
      fn zero_timeout_wait_never_blocks() {
        let mut backend = new_backend();
        backend.init(64).unwrap();

        let start = Instant::now();
        let completed = backend.wait(Some(Duration::ZERO)).unwrap();
        let elapsed = start.elapsed();

        assert!(completed.is_empty(), "zero-timeout wait without work must be empty");
        assert!(
          elapsed < Duration::from_millis(200),
          "wait(Duration::ZERO) blocked for {:?}",
          elapsed
        );
      }

      #[cfg(unix)]
      #[test]
      fn wait_none_blocks_until_completion() {
        use std::os::fd::AsRawFd;

        let mut backend = new_backend();
        backend.init(64).unwrap();

        let (read_res, write_res) = socket_pair();
        let payload = *b"wake";
        let mut buf = [0_u8; 4];
        let mut raw_buf = unsafe { RawBuf::from_raw_parts(buf.as_mut_ptr(), buf.len()) };

        backend.push(
          3,
          Op::Read {
            fd: read_res.clone(),
            iovecs: (&mut raw_buf as *mut RawBuf),
            iov_count: 1,
            offset: -1,
            flags: 0,
          },
        );
        backend.flush().unwrap();

        let writer = thread::spawn(move || {
          std::thread::sleep(Duration::from_millis(20));
          let wrote = unsafe {
            libc::send(
              write_res.as_raw_fd(),
              payload.as_ptr().cast(),
              payload.len(),
              0,
            )
          };
          assert_eq!(wrote, payload.len() as isize);
        });

        let start = Instant::now();
        let completed = backend.wait(None).unwrap();
        let elapsed = start.elapsed();

        assert_eq!(completed.len(), 1);
        assert_exact_result(&completed[0], 3, payload.len() as isize);
        assert!(
          elapsed >= Duration::from_millis(5),
          "wait(None) returned too early to have actually blocked: {:?}",
          elapsed
        );

        writer.join().unwrap();
      }

      #[test]
      fn empty_flush_is_harmless() {
        let mut backend = new_backend();
        backend.init(64).unwrap();

        backend.flush().unwrap();
        backend.flush().unwrap();

        let completed = backend.wait(Some(Duration::ZERO)).unwrap();
        assert!(
          completed.is_empty(),
          "flush() with an empty backlog must not create completions"
        );
      }

      #[cfg(unix)]
      #[test]
      fn flush_can_produce_immediate_completions_without_pending_work() {
        let mut backend = new_backend();
        backend.init(64).unwrap();

        let invalid_fd = unsafe { Resource::from_raw_fd(-1) };
        let mut raw_buf = unsafe { RawBuf::from_raw_parts(std::ptr::null_mut(), 0) };

        backend.push(
          91,
          Op::Read {
            fd: invalid_fd,
            iovecs: (&mut raw_buf as *mut RawBuf),
            iov_count: 1,
            offset: -2,
            flags: 0,
          },
        );

        backend.flush().unwrap();

        let completed = backend.wait(Some(Duration::ZERO)).unwrap();
        assert_eq!(
          completed.len(),
          1,
          "flush-produced completion must be surfaced on the next wait()"
        );
        assert_exact_result(&completed[0], 91, -(libc::EINVAL as isize));

        let completed = backend.wait(Some(Duration::ZERO)).unwrap();
        assert!(
          completed.is_empty(),
          "flush-produced completion must not be replayed on later wait() calls"
        );
      }

      #[test]
      #[should_panic(expected = "IoBackend capacity exceeded")]
      fn pushing_more_than_capacity_panics() {
        let mut backend = new_backend();
        backend.init(1).unwrap();

        backend.push(1, Op::Nop);
        backend.push(2, Op::Nop);
      }

      #[cfg(unix)]
      fn socket_pair() -> (
        $crate::api::resource::Resource,
        $crate::api::resource::Resource,
      ) {
        let mut fds = [0; 2];
        let rc = unsafe {
          libc::socketpair(
            libc::AF_UNIX,
            libc::SOCK_STREAM,
            0,
            fds.as_mut_ptr(),
          )
        };
        assert_eq!(
          rc,
          0,
          "socketpair() failed: {}",
          std::io::Error::last_os_error()
        );

        for &fd in &fds {
          let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
          assert!(
            flags >= 0,
            "fcntl(F_GETFL) failed: {}",
            std::io::Error::last_os_error()
          );

          let rc = unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) };
          assert_eq!(
            rc,
            0,
            "fcntl(F_SETFL, O_NONBLOCK) failed: {}",
            std::io::Error::last_os_error()
          );
        }

        // SAFETY: both fds were just returned by `pipe` and are uniquely owned here.
        let read = unsafe {
          <$crate::api::resource::Resource as std::os::fd::FromRawFd>::from_raw_fd(fds[0])
        };
        // SAFETY: both fds were just returned by `pipe` and are uniquely owned here.
        let write = unsafe {
          <$crate::api::resource::Resource as std::os::fd::FromRawFd>::from_raw_fd(fds[1])
        };

        (read, write)
      }

      #[cfg(unix)]
      fn set_nonblocking(fd: RawFd) {
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        assert!(
          flags >= 0,
          "fcntl(F_GETFL) failed: {}",
          std::io::Error::last_os_error()
        );
        let rc = unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) };
        assert_eq!(
          rc,
          0,
          "fcntl(F_SETFL, O_NONBLOCK) failed: {}",
          std::io::Error::last_os_error()
        );
      }

      #[cfg(unix)]
      fn unix_socket_path(prefix: &str) -> PathBuf {
        let now = SystemTime::now()
          .duration_since(SystemTime::UNIX_EPOCH)
          .expect("system time before UNIX epoch")
          .as_nanos();
        let pid = std::process::id();
        let mut path = env::temp_dir();
        let mut name = format!("lio-{}-{:x}-{:x}", prefix, pid, now);
        let max_name_len = 40;
        if name.len() > max_name_len {
          name.truncate(max_name_len);
        }
        path.push(&name);
        // Ensure it is unique, append a compact suffix if necessary.
        let mut idx = 0u32;
        while path.exists() {
          path.set_file_name(format!("{name}-{:x}", idx));
          idx += 1;
        }
        let bytes = path.as_os_str().as_bytes();
        let mut storage: libc::sockaddr_storage = unsafe { mem::zeroed() };
        let sun = unsafe { &mut *(std::ptr::addr_of_mut!(storage) as *mut libc::sockaddr_un) };
        assert!(
          bytes.len() < sun.sun_path.len(),
          "unix socket test path too long: {} >= {} ({})",
          bytes.len(),
          sun.sun_path.len(),
          path.display()
        );
        path
      }

      #[cfg(unix)]
      fn temp_path(prefix: &str) -> PathBuf {
        let now = SystemTime::now()
          .duration_since(SystemTime::UNIX_EPOCH)
          .expect("system time before UNIX epoch")
          .as_nanos();
        let pid = std::process::id();
        let mut path = env::temp_dir();
        path.push(format!("lio-{}-{:x}-{:x}", prefix, pid, now));
        path
      }

      #[cfg(unix)]
      fn unix_sockaddr_un(path: &PathBuf) -> (libc::sockaddr_storage, libc::socklen_t) {
        let bytes = path.as_os_str().as_bytes();

        let mut storage: libc::sockaddr_storage = unsafe { mem::zeroed() };
        let sun = unsafe { &mut *(std::ptr::addr_of_mut!(storage) as *mut libc::sockaddr_un) };
        assert!(
          bytes.len() < sun.sun_path.len(),
          "path too long for unix socket test"
        );
        sun.sun_family = libc::AF_UNIX as libc::sa_family_t;
        for (i, &byte) in bytes.iter().enumerate() {
          sun.sun_path[i] = byte as libc::c_char;
        }
        sun.sun_path[bytes.len()] = 0;

        let len = (std::mem::size_of::<libc::sa_family_t>() + bytes.len() + 1) as libc::socklen_t;
        (storage, len)
      }

      #[cfg(unix)]
      fn wait_for_single_completion(
        backend: &mut impl IoBackend,
      ) -> $crate::backend::OpCompleted {
        for _ in 0..20 {
          let completed = backend
            .wait(Some(Duration::from_millis(10)))
            .unwrap();
          if let Some(first) = completed.first() {
            return $crate::backend::OpCompleted::new(
              first.registration_id(),
              first.result(),
            );
          }
        }

        panic!("backend did not produce a completion within the expected timeout");
      }

      #[cfg(unix)]
      fn invalid_fd_resource() -> Resource {
        // SAFETY: this intentionally uses an invalid fd to verify exact raw errno output.
        unsafe { Resource::borrow(-1) }
      }

      #[cfg(unix)]
      fn wait_for_positive_completion(
        backend: &mut impl IoBackend,
      ) -> $crate::backend::OpCompleted {
        let completed = wait_for_single_completion(backend);
        assert!(
          completed.result() > 0,
          "expected a positive completion result, got {}",
          completed.result()
        );
        completed
      }

      mod nop {
        use super::*;

        #[test]
        fn nop_produces_immediate_success_completion() {
          let mut backend = new_backend();
          backend.init(64).unwrap();

          backend.push(40, Op::Nop);
          backend.flush().unwrap();

          let completed = backend.wait(Some(Duration::ZERO)).unwrap();
          assert_eq!(completed.len(), 1, "Op::Nop must complete immediately");
          assert_exact_result(&completed[0], 40, 0);

          let completed = backend.wait(Some(Duration::ZERO)).unwrap();
          assert!(
            completed.is_empty(),
            "Op::Nop completion must not be replayed on later wait() calls"
          );
        }
      }

      #[cfg(unix)]
      mod read {
        use super::*;
        use std::os::fd::AsRawFd;

        #[test]
        fn success_reports_raw_result_and_id() {
          let mut backend = new_backend();
          backend.init(64).unwrap();

          let (read_res, write_res) = socket_pair();
          let payload = *b"read";
          let mut buf = [0_u8; 4];
          let wrote = unsafe {
            libc::send(
              write_res.as_raw_fd(),
              payload.as_ptr().cast(),
              payload.len(),
              0,
            )
          };
          assert_eq!(wrote, payload.len() as isize);

          let mut total = 0usize;
          let mut id = 43_u64;
          while total < payload.len() {
            let mut raw_buf = unsafe {
              RawBuf::from_raw_parts(
                buf[total..].as_mut_ptr(),
                buf.len() - total,
              )
            };

            backend.push(
              id,
              Op::Read {
                fd: read_res.clone(),
                iovecs: (&mut raw_buf as *mut RawBuf),
                iov_count: 1,
                offset: -1,
                flags: 0,
              },
            );
            backend.flush().unwrap();

            let completed = wait_for_positive_completion(&mut backend);
            assert_eq!(completed.registration_id(), id);
            let n = completed.result() as usize;
            assert!(
              n <= payload.len() - total,
              "read completed more bytes than remaining: {} > {}",
              n,
              payload.len() - total
            );
            total += n;
            id += 1;
          }

          assert_eq!(total, payload.len());
          assert_eq!(&buf, &payload);
        }

        #[test]
        fn pending_then_success() {
          let mut backend = new_backend();
          backend.init(64).unwrap();

          let (read_res, write_res) = socket_pair();
          let payload = *b"late";
          let mut buf = [0_u8; 4];
          let mut raw_buf = unsafe { RawBuf::from_raw_parts(buf.as_mut_ptr(), buf.len()) };

          backend.push(
            54,
            Op::Read {
              fd: read_res.clone(),
              iovecs: (&mut raw_buf as *mut RawBuf),
              iov_count: 1,
              offset: -1,
              flags: 0,
            },
          );
          backend.flush().unwrap();

          let completed = backend.wait(Some(Duration::ZERO)).unwrap();
          assert!(
            completed.is_empty(),
            "read without available data must remain pending"
          );

          let completed = backend.wait(Some(Duration::ZERO)).unwrap();
          assert!(
            completed.is_empty(),
            "read must remain pending across repeated zero-timeout waits"
          );

          let wrote = unsafe {
            libc::send(
              write_res.as_raw_fd(),
              payload.as_ptr().cast(),
              payload.len(),
              0,
            )
          };
          assert_eq!(wrote, payload.len() as isize);

          let completed = wait_for_positive_completion(&mut backend);
          assert_eq!(completed.registration_id(), 54);
          let mut total = completed.result() as usize;

          let mut id = 55_u64;
          while total < payload.len() {
            let mut raw_buf = unsafe {
              RawBuf::from_raw_parts(
                buf[total..].as_mut_ptr(),
                buf.len() - total,
              )
            };
            backend.push(
              id,
              Op::Read {
                fd: read_res.clone(),
                iovecs: (&mut raw_buf as *mut RawBuf),
                iov_count: 1,
                offset: -1,
                flags: 0,
              },
            );
            backend.flush().unwrap();

            let completed = wait_for_positive_completion(&mut backend);
            assert_eq!(completed.registration_id(), id);
            let n = completed.result() as usize;
            assert!(
              n <= payload.len() - total,
              "read completed more bytes than remaining: {} > {}",
              n,
              payload.len() - total
            );
            total += n;
            id += 1;
          }

          assert_eq!(total, payload.len());
          assert_eq!(&buf, &payload);
        }

        #[test]
        fn deferred_completion_is_delivered_exactly_once() {
          let mut backend = new_backend();
          backend.init(64).unwrap();

          let (read_res, write_res) = socket_pair();
          let payload = *b"once";
          let mut buf = [0_u8; 4];
          let mut raw_buf = unsafe { RawBuf::from_raw_parts(buf.as_mut_ptr(), buf.len()) };

          backend.push(
            541,
            Op::Read {
              fd: read_res.clone(),
              iovecs: (&mut raw_buf as *mut RawBuf),
              iov_count: 1,
              offset: -1,
              flags: 0,
            },
          );
          backend.flush().unwrap();

          assert!(
            backend.wait(Some(Duration::ZERO)).unwrap().is_empty(),
            "read without available data must remain pending"
          );

          let wrote = unsafe {
            libc::send(
              write_res.as_raw_fd(),
              payload.as_ptr().cast(),
              payload.len(),
              0,
            )
          };
          assert_eq!(wrote, payload.len() as isize);

          let completed = wait_for_single_completion(&mut backend);
          assert_exact_result(&completed, 541, payload.len() as isize);
          assert_eq!(&buf, &payload);

          for _ in 0..3 {
            let completed = backend.wait(Some(Duration::ZERO)).unwrap();
            assert!(
              completed.is_empty(),
              "a deferred read completion already returned once must not be returned again"
            );
          }
        }

        #[test]
        fn vectored_success_reports_total_byte_count() {
          let mut backend = new_backend();
          backend.init(64).unwrap();

          let (read_res, write_res) = socket_pair();
          let payload = *b"vector";
          let wrote = unsafe {
            libc::send(
              write_res.as_raw_fd(),
              payload.as_ptr().cast(),
              payload.len(),
              0,
            )
          };
          assert_eq!(wrote, payload.len() as isize);
          let mut left = [0_u8; 3];
          let mut right = [0_u8; 3];
          let mut total = 0usize;
          let mut id = 65_u64;
          while total < payload.len() {
            let left_written = total.min(left.len());
            let right_written = total.saturating_sub(left.len()).min(right.len());
            let mut bufs = [
              unsafe {
                RawBuf::from_raw_parts(
                  left[left_written..].as_mut_ptr(),
                  left.len() - left_written,
                )
              },
              unsafe {
                RawBuf::from_raw_parts(
                  right[right_written..].as_mut_ptr(),
                  right.len() - right_written,
                )
              },
            ];

            backend.push(
              id,
              Op::Read {
                fd: read_res.clone(),
                iovecs: bufs.as_mut_ptr(),
                iov_count: bufs.len(),
                offset: -1,
                flags: 0,
              },
            );
            backend.flush().unwrap();

            let completed = wait_for_positive_completion(&mut backend);
            assert_eq!(completed.registration_id(), id);
            let n = completed.result() as usize;
            assert!(
              n <= payload.len() - total,
              "read completed more bytes than remaining: {} > {}",
              n,
              payload.len() - total
            );
            total += n;
            id += 1;
          }

          assert_eq!(total, payload.len());
          assert_eq!(&left, b"vec");
          assert_eq!(&right, b"tor");
        }

        #[test]
        fn eof_reports_zero() {
          let mut backend = new_backend();
          backend.init(64).unwrap();

          let (read_res, write_res) = socket_pair();
          let mut buf = [0_u8; 4];
          let mut raw_buf = unsafe { RawBuf::from_raw_parts(buf.as_mut_ptr(), buf.len()) };

          backend.push(
            55,
            Op::Read {
              fd: read_res.clone(),
              iovecs: (&mut raw_buf as *mut RawBuf),
              iov_count: 1,
              offset: -1,
              flags: 0,
            },
          );
          backend.flush().unwrap();
          drop(write_res);

          let completed = wait_for_single_completion(&mut backend);
          assert_exact_result(&completed, 55, 0);
        }

        #[test]
        fn invalid_fd_reports_ebadf() {
          let mut backend = new_backend();
          backend.init(64).unwrap();

          let mut buf = [0_u8; 4];
          let mut raw_buf = unsafe { RawBuf::from_raw_parts(buf.as_mut_ptr(), buf.len()) };

          backend.push(
            47,
            Op::Read {
              fd: invalid_fd_resource(),
              iovecs: (&mut raw_buf as *mut RawBuf),
              iov_count: 1,
              offset: -1,
              flags: 0,
            },
          );
          backend.flush().unwrap();

          let completed = wait_for_single_completion(&mut backend);
          assert_exact_result(&completed, 47, -(libc::EBADF as isize));
        }

        #[test]
        fn invalid_offset_reports_einval() {
          let mut backend = new_backend();
          backend.init(64).unwrap();

          let (read_res, _write_res) = socket_pair();
          let mut buf = [0_u8; 4];
          let mut raw_buf = unsafe { RawBuf::from_raw_parts(buf.as_mut_ptr(), buf.len()) };

          backend.push(
            69,
            Op::Read {
              fd: read_res,
              iovecs: (&mut raw_buf as *mut RawBuf),
              iov_count: 1,
              offset: -2,
              flags: 0,
            },
          );
          backend.flush().unwrap();

          let completed = backend.wait(Some(Duration::ZERO)).unwrap();
          assert_eq!(completed.len(), 1);
          assert_exact_result(&completed[0], 69, -(libc::EINVAL as isize));
        }

        #[test]
        fn unsupported_flags_report_enotsup() {
          let mut backend = new_backend();
          backend.init(64).unwrap();

          let (read_res, _write_res) = socket_pair();
          let mut buf = [0_u8; 4];
          let mut raw_buf = unsafe { RawBuf::from_raw_parts(buf.as_mut_ptr(), buf.len()) };

          backend.push(
            70,
            Op::Read {
              fd: read_res,
              iovecs: (&mut raw_buf as *mut RawBuf),
              iov_count: 1,
              offset: -1,
              flags: i32::MIN,
            },
          );
          backend.flush().unwrap();

          let completed = backend.wait(Some(Duration::ZERO)).unwrap();
          assert_eq!(completed.len(), 1);
          assert_exact_result(&completed[0], 70, -(libc::ENOTSUP as isize));
        }

        #[test]
        fn null_iovecs_reports_einval() {
          let mut backend = new_backend();
          backend.init(64).unwrap();

          let (read_res, _write_res) = socket_pair();

          backend.push(
            71,
            Op::Read {
              fd: read_res,
              iovecs: std::ptr::null_mut(),
              iov_count: 1,
              offset: -1,
              flags: 0,
            },
          );
          backend.flush().unwrap();

          let completed = backend.wait(Some(Duration::ZERO)).unwrap();
          assert_eq!(completed.len(), 1);
          assert_exact_result(&completed[0], 71, -(libc::EINVAL as isize));
        }
      }

      #[cfg(unix)]
      mod write {
        use super::*;
        use std::os::fd::AsRawFd;

        #[test]
        fn success_reports_raw_result_and_id() {
          let mut backend = new_backend();
          backend.init(64).unwrap();

          let (read_res, write_res) = socket_pair();
          let mut payload = *b"writ";
          let mut total = 0usize;
          let mut id = 44_u64;
          while total < payload.len() {
            let raw_buf = unsafe {
              RawBuf::from_raw_parts(
                payload[total..].as_mut_ptr(),
                payload.len() - total,
              )
            };

            backend.push(
              id,
              Op::Write {
                fd: write_res.clone(),
                iovecs: (&raw_buf as *const RawBuf),
                iov_count: 1,
                offset: -1,
                flags: 0,
              },
            );
            backend.flush().unwrap();

            let completed = wait_for_positive_completion(&mut backend);
            assert_eq!(completed.registration_id(), id);
            let n = completed.result() as usize;
            assert!(
              n <= payload.len() - total,
              "write completed more bytes than remaining: {} > {}",
              n,
              payload.len() - total
            );
            total += n;
            id += 1;
          }

          assert_eq!(total, payload.len());

          let mut buf = [0_u8; 4];
          let n = unsafe {
            libc::recv(
              read_res.as_raw_fd(),
              buf.as_mut_ptr().cast(),
              buf.len(),
              0,
            )
          };
          assert_eq!(n, payload.len() as isize);
          assert_eq!(&buf, &payload);
        }

        #[test]
        fn vectored_success_reports_total_byte_count() {
          let mut backend = new_backend();
          backend.init(64).unwrap();

          let (read_res, write_res) = socket_pair();
          let left = *b"vec";
          let right = *b"tor";
          let mut total = 0usize;
          let mut id = 66_u64;
          while total < 6 {
            let left_done = total.min(left.len());
            let right_done = total.saturating_sub(left.len()).min(right.len());
            let bufs = [
              RawBuf::new(&left[left_done..]),
              RawBuf::new(&right[right_done..]),
            ];

            backend.push(
              id,
              Op::Write {
                fd: write_res.clone(),
                iovecs: bufs.as_ptr(),
                iov_count: bufs.len(),
                offset: -1,
                flags: 0,
              },
            );
            backend.flush().unwrap();

            let completed = wait_for_positive_completion(&mut backend);
            assert_eq!(completed.registration_id(), id);
            let n = completed.result() as usize;
            assert!(n <= 6 - total, "write completed more bytes than remaining");
            total += n;
            id += 1;
          }

          assert_eq!(total, 6);

          let mut buf = [0_u8; 6];
          let n = unsafe {
            libc::recv(
              read_res.as_raw_fd(),
              buf.as_mut_ptr().cast(),
              buf.len(),
              0,
            )
          };
          assert_eq!(n, 6);
          assert_eq!(&buf, b"vector");
        }

        #[test]
        fn invalid_fd_reports_ebadf() {
          let mut backend = new_backend();
          backend.init(64).unwrap();

          let payload = *b"err!";
          let raw_buf = unsafe { RawBuf::from_raw_parts(payload.as_ptr().cast_mut(), payload.len()) };

          backend.push(
            48,
            Op::Write {
              fd: invalid_fd_resource(),
              iovecs: (&raw_buf as *const RawBuf),
              iov_count: 1,
              offset: -1,
              flags: 0,
            },
          );
          backend.flush().unwrap();

          let completed = wait_for_single_completion(&mut backend);
          assert_exact_result(&completed, 48, -(libc::EBADF as isize));
        }

        #[test]
        fn invalid_offset_reports_einval() {
          let mut backend = new_backend();
          backend.init(64).unwrap();

          let (_read_res, write_res) = socket_pair();
          let payload = *b"arg!";
          let raw_buf = unsafe { RawBuf::from_raw_parts(payload.as_ptr().cast_mut(), payload.len()) };

          backend.push(
            72,
            Op::Write {
              fd: write_res,
              iovecs: (&raw_buf as *const RawBuf),
              iov_count: 1,
              offset: -2,
              flags: 0,
            },
          );
          backend.flush().unwrap();

          let completed = wait_for_single_completion(&mut backend);
          assert_exact_result(&completed, 72, -(libc::EINVAL as isize));
        }

        #[test]
        fn unsupported_flags_report_enotsup() {
          let mut backend = new_backend();
          backend.init(64).unwrap();

          let (_read_res, write_res) = socket_pair();
          let payload = *b"arg!";
          let raw_buf = unsafe { RawBuf::from_raw_parts(payload.as_ptr().cast_mut(), payload.len()) };

          backend.push(
            73,
            Op::Write {
              fd: write_res,
              iovecs: (&raw_buf as *const RawBuf),
              iov_count: 1,
              offset: -1,
              flags: i32::MIN,
            },
          );
          backend.flush().unwrap();

          let completed = wait_for_single_completion(&mut backend);
          assert_exact_result(&completed, 73, -(libc::ENOTSUP as isize));
        }

        #[test]
        fn null_iovecs_reports_einval() {
          let mut backend = new_backend();
          backend.init(64).unwrap();

          let (_read_res, write_res) = socket_pair();

          backend.push(
            74,
            Op::Write {
              fd: write_res,
              iovecs: std::ptr::null(),
              iov_count: 1,
              offset: -1,
              flags: 0,
            },
          );
          backend.flush().unwrap();

          let completed = wait_for_single_completion(&mut backend);
          assert_exact_result(&completed, 74, -(libc::EINVAL as isize));
        }
      }

      #[cfg(unix)]
      mod recv {
        use super::*;
        use std::os::fd::AsRawFd;

        #[test]
        fn success_reports_raw_result_and_id() {
          let mut backend = new_backend();
          backend.init(64).unwrap();

          let (read_res, write_res) = socket_pair();
          let payload = *b"pong";
          let wrote = unsafe {
            libc::send(
              write_res.as_raw_fd(),
              payload.as_ptr().cast(),
              payload.len(),
              0,
            )
          };
          assert_eq!(wrote, payload.len() as isize);

          let mut buf = [0_u8; 4];
          let mut total = 0usize;
          let mut id = 42_u64;
          while total < payload.len() {
            let bufs = [MsgBufMut::from_slice(&mut buf[total..])];

            backend.push(
              id,
              Op::Recv {
                fd: read_res.clone(),
                msg: MsgRecv::new(&bufs, false),
                flags: 0,
              },
            );
            backend.flush().unwrap();

            let completed = wait_for_positive_completion(&mut backend);
            assert_eq!(completed.registration_id(), id);
            let n = completed.result() as usize;
            assert!(n <= payload.len() - total, "recv completed more bytes than remaining");
            total += n;
            id += 1;
          }
          assert_eq!(total, payload.len());
          assert_eq!(&buf, &payload);
        }

        #[test]
        fn pending_then_success() {
          let mut backend = new_backend();
          backend.init(64).unwrap();

          let (read_res, write_res) = socket_pair();
          let payload = *b"wait";
          let mut buf = [0_u8; 4];
          let bufs = [MsgBufMut::from_slice(&mut buf)];

          backend.push(
            56,
            Op::Recv {
              fd: read_res.clone(),
              msg: MsgRecv::new(&bufs, false),
              flags: 0,
            },
          );
          backend.flush().unwrap();

          let completed = backend.wait(Some(Duration::ZERO)).unwrap();
          assert!(
            completed.is_empty(),
            "recv without available data must remain pending"
          );

          let completed = backend.wait(Some(Duration::ZERO)).unwrap();
          assert!(
            completed.is_empty(),
            "recv must remain pending across repeated zero-timeout waits"
          );

          let wrote = unsafe {
            libc::send(
              write_res.as_raw_fd(),
              payload.as_ptr().cast(),
              payload.len(),
              0,
            )
          };
          assert_eq!(wrote, payload.len() as isize);

          let completed = wait_for_positive_completion(&mut backend);
          assert_eq!(completed.registration_id(), 56);
          let mut total = completed.result() as usize;

          let mut id = 57_u64;
          while total < payload.len() {
            let bufs = [MsgBufMut::from_slice(&mut buf[total..])];

            backend.push(
              id,
              Op::Recv {
                fd: read_res.clone(),
                msg: MsgRecv::new(&bufs, false),
                flags: 0,
              },
            );
            backend.flush().unwrap();

            let completed = wait_for_positive_completion(&mut backend);
            assert_eq!(completed.registration_id(), id);
            let n = completed.result() as usize;
            assert!(n <= payload.len() - total, "recv completed more bytes than remaining");
            total += n;
            id += 1;
          }

          assert_eq!(total, payload.len());
          assert_eq!(&buf, &payload);
        }

        #[test]
        fn deferred_completion_is_delivered_exactly_once() {
          let mut backend = new_backend();
          backend.init(64).unwrap();

          let (read_res, write_res) = socket_pair();
          let payload = *b"once";
          let mut buf = [0_u8; 4];
          let bufs = [MsgBufMut::from_slice(&mut buf)];

          backend.push(
            561,
            Op::Recv {
              fd: read_res.clone(),
              msg: MsgRecv::new(&bufs, false),
              flags: 0,
            },
          );
          backend.flush().unwrap();

          assert!(
            backend.wait(Some(Duration::ZERO)).unwrap().is_empty(),
            "recv without available data must remain pending"
          );

          let wrote = unsafe {
            libc::send(
              write_res.as_raw_fd(),
              payload.as_ptr().cast(),
              payload.len(),
              0,
            )
          };
          assert_eq!(wrote, payload.len() as isize);

          let completed = wait_for_single_completion(&mut backend);
          assert_exact_result(&completed, 561, payload.len() as isize);
          assert_eq!(&buf, &payload);

          for _ in 0..3 {
            let completed = backend.wait(Some(Duration::ZERO)).unwrap();
            assert!(
              completed.is_empty(),
              "a deferred recv completion already returned once must not be returned again"
            );
          }
        }

        #[test]
        fn vectored_success_reports_total_byte_count() {
          let mut backend = new_backend();
          backend.init(64).unwrap();

          let (read_res, write_res) = socket_pair();
          let payload = *b"vector";
          let wrote = unsafe {
            libc::send(
              write_res.as_raw_fd(),
              payload.as_ptr().cast(),
              payload.len(),
              0,
            )
          };
          assert_eq!(wrote, payload.len() as isize);

          let mut left = [0_u8; 3];
          let mut right = [0_u8; 3];
          let mut total = 0usize;
          let mut id = 67_u64;
          while total < payload.len() {
            let left_done = total.min(left.len());
            let right_done = total.saturating_sub(left.len()).min(right.len());
            let bufs = [
              MsgBufMut::from_slice(&mut left[left_done..]),
              MsgBufMut::from_slice(&mut right[right_done..]),
            ];

            backend.push(
              id,
              Op::Recv {
                fd: read_res.clone(),
                msg: MsgRecv::new(&bufs, false),
                flags: 0,
              },
            );
            backend.flush().unwrap();

            let completed = wait_for_positive_completion(&mut backend);
            assert_eq!(completed.registration_id(), id);
            let n = completed.result() as usize;
            assert!(n <= payload.len() - total, "recv completed more bytes than remaining");
            total += n;
            id += 1;
          }

          assert_eq!(total, payload.len());
          assert_eq!(&left, b"vec");
          assert_eq!(&right, b"tor");
        }

        #[test]
        fn eof_reports_zero() {
          let mut backend = new_backend();
          backend.init(64).unwrap();

          let (read_res, write_res) = socket_pair();
          let mut buf = [0_u8; 4];
          let bufs = [MsgBufMut::from_slice(&mut buf)];

          backend.push(
            57,
            Op::Recv {
              fd: read_res.clone(),
              msg: MsgRecv::new(&bufs, false),
              flags: 0,
            },
          );
          backend.flush().unwrap();
          drop(write_res);

          let completed = wait_for_single_completion(&mut backend);
          assert_exact_result(&completed, 57, 0);
        }

        #[test]
        fn invalid_fd_reports_ebadf() {
          let mut backend = new_backend();
          backend.init(64).unwrap();

          let mut buf = [0_u8; 4];
          let bufs = [MsgBufMut::from_slice(&mut buf)];

          backend.push(
            49,
            Op::Recv {
              fd: invalid_fd_resource(),
              msg: MsgRecv::new(&bufs, false),
              flags: 0,
            },
          );
          backend.flush().unwrap();

          let completed = wait_for_single_completion(&mut backend);
          assert_exact_result(&completed, 49, -(libc::EBADF as isize));
        }
      }

      #[cfg(unix)]
      mod send {
        use super::*;
        use std::os::fd::AsRawFd;

        #[test]
        fn success_reports_raw_result_and_id() {
          let mut backend = new_backend();
          backend.init(64).unwrap();

          let (read_res, write_res) = socket_pair();
          let payload = *b"ping";
          let mut total = 0usize;
          let mut id = 41_u64;
          while total < payload.len() {
            let bufs = [MsgBuf::from_slice(&payload[total..])];

            backend.push(
              id,
              Op::Send {
                fd: write_res.clone(),
                msg: MsgSend::new(&bufs, None),
                flags: 0,
              },
            );
            backend.flush().unwrap();

            let completed = wait_for_positive_completion(&mut backend);
            assert_eq!(completed.registration_id(), id);
            let n = completed.result() as usize;
            assert!(n <= payload.len() - total, "send completed more bytes than remaining");
            total += n;
            id += 1;
          }

          assert_eq!(total, payload.len());

          let mut buf = [0_u8; 4];
          let n = unsafe {
            libc::recv(
              read_res.as_raw_fd(),
              buf.as_mut_ptr().cast(),
              buf.len(),
              0,
            )
          };
          assert_eq!(n, payload.len() as isize);
          assert_eq!(&buf, &payload);
        }

        #[test]
        fn vectored_success_reports_total_byte_count() {
          let mut backend = new_backend();
          backend.init(64).unwrap();

          let (read_res, write_res) = socket_pair();
          let left = *b"vec";
          let right = *b"tor";
          let mut total = 0usize;
          let mut id = 68_u64;
          while total < 6 {
            let left_done = total.min(left.len());
            let right_done = total.saturating_sub(left.len()).min(right.len());
            let bufs = [
              MsgBuf::from_slice(&left[left_done..]),
              MsgBuf::from_slice(&right[right_done..]),
            ];

            backend.push(
              id,
              Op::Send {
                fd: write_res.clone(),
                msg: MsgSend::new(&bufs, None),
                flags: 0,
              },
            );
            backend.flush().unwrap();

            let completed = wait_for_positive_completion(&mut backend);
            assert_eq!(completed.registration_id(), id);
            let n = completed.result() as usize;
            assert!(n <= 6 - total, "send completed more bytes than remaining");
            total += n;
            id += 1;
          }

          assert_eq!(total, 6);

          let mut buf = [0_u8; 6];
          let n = unsafe {
            libc::recv(
              read_res.as_raw_fd(),
              buf.as_mut_ptr().cast(),
              buf.len(),
              0,
            )
          };
          assert_eq!(n, 6);
          assert_eq!(&buf, b"vector");
        }

        #[test]
        fn invalid_fd_reports_ebadf() {
          let mut backend = new_backend();
          backend.init(64).unwrap();

          let payload = *b"oops";
          let bufs = [MsgBuf::from_slice(&payload)];

          backend.push(
            50,
            Op::Send {
              fd: invalid_fd_resource(),
              msg: MsgSend::new(&bufs, None),
              flags: 0,
            },
          );
          backend.flush().unwrap();

          let completed = wait_for_single_completion(&mut backend);
          assert_exact_result(&completed, 50, -(libc::EBADF as isize));
        }
      }

      #[cfg(unix)]
      mod accept {
        use super::*;

        #[test]
        fn success_reports_success_and_id() {
          let mut backend = new_backend();
          backend.init(64).unwrap();

          let path = unix_socket_path("accept-ok");
          fs::remove_file(&path).ok();
          let listener = UnixListener::bind(&path).unwrap();
          listener.set_nonblocking(true).unwrap();
          let listener_fd = listener.into_raw_fd();
          let listener_res = unsafe { Resource::from_raw_fd(listener_fd) };

          let mut storage: libc::sockaddr_storage = unsafe { mem::zeroed() };
          let mut len = mem::size_of::<libc::sockaddr_storage>() as libc::socklen_t;

          backend.push(
            52,
            Op::Accept {
              fd: listener_res,
              addr: &mut storage,
              len: &mut len,
            },
          );
          backend.flush().unwrap();

          let path_clone = path.clone();
          let client = thread::spawn(move || {
            let stream = UnixStream::connect(&path_clone).unwrap();
            drop(stream);
          });

          let completed = wait_for_single_completion(&mut backend);
          assert_eq!(completed.registration_id(), 52);
          assert!(
            completed.result() >= 0,
            "accept must succeed with a nonnegative fd, got {}",
            completed.result()
          );

          client.join().unwrap();
          unsafe {
            libc::close(completed.result() as libc::c_int);
          }
          fs::remove_file(&path).ok();
        }

        #[test]
        fn pending_then_success() {
          let mut backend = new_backend();
          backend.init(64).unwrap();

          let path = unix_socket_path("accept-pending");
          fs::remove_file(&path).ok();
          let listener = UnixListener::bind(&path).unwrap();
          listener.set_nonblocking(true).unwrap();
          let listener_fd = listener.into_raw_fd();
          let listener_res = unsafe { Resource::from_raw_fd(listener_fd) };

          let mut storage: libc::sockaddr_storage = unsafe { mem::zeroed() };
          let mut len = mem::size_of::<libc::sockaddr_storage>() as libc::socklen_t;

          backend.push(
            58,
            Op::Accept {
              fd: listener_res,
              addr: &mut storage,
              len: &mut len,
            },
          );
          backend.flush().unwrap();

          let completed = backend.wait(Some(Duration::ZERO)).unwrap();
          assert!(
            completed.is_empty(),
            "accept without a pending client must remain pending"
          );

          let path_clone = path.clone();
          let client = thread::spawn(move || {
            let stream = UnixStream::connect(&path_clone).unwrap();
            drop(stream);
          });

          let completed = wait_for_single_completion(&mut backend);
          assert_eq!(completed.registration_id(), 58);
          assert!(
            completed.result() >= 0,
            "accept must succeed with a nonnegative fd, got {}",
            completed.result()
          );

          client.join().unwrap();
          unsafe {
            libc::close(completed.result() as libc::c_int);
          }
          fs::remove_file(&path).ok();
        }

        #[test]
        fn invalid_fd_reports_ebadf() {
          let mut backend = new_backend();
          backend.init(64).unwrap();

          let mut storage: libc::sockaddr_storage = unsafe { mem::zeroed() };
          let mut len = mem::size_of::<libc::sockaddr_storage>() as libc::socklen_t;

          backend.push(
            51,
            Op::Accept {
              fd: invalid_fd_resource(),
              addr: &mut storage,
              len: &mut len,
            },
          );
          backend.flush().unwrap();

          let completed = wait_for_single_completion(&mut backend);
          assert_exact_result(&completed, 51, -(libc::EBADF as isize));
        }
      }

      #[cfg(unix)]
      mod connect {
        use super::*;

        #[test]
        fn missing_path_reports_enoent() {
          let mut backend = new_backend();
          backend.init(64).unwrap();

          let path = unix_socket_path("connect");
          fs::remove_file(&path).ok();

          let client_fd = unsafe {
            let fd = libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0);
            assert!(fd >= 0, "socket() failed: {}", std::io::Error::last_os_error());
            set_nonblocking(fd);
            fd
          };
          let client_res = unsafe { Resource::from_raw_fd(client_fd) };
          let (storage, len) = unix_sockaddr_un(&path);

          backend.push(
            46,
            Op::Connect {
              fd: client_res.clone(),
              addr: &storage,
              len,
            },
          );
          backend.flush().unwrap();

          let completed = backend.wait(Some(Duration::ZERO)).unwrap();
          assert_eq!(completed.len(), 1);
          assert_exact_result(&completed[0], 46, -(libc::ENOENT as isize));
        }

        #[test]
        fn success_reports_zero_and_id() {
          let mut backend = new_backend();
          backend.init(64).unwrap();

          let path = unix_socket_path("connect-ok");
          fs::remove_file(&path).ok();
          let listener = UnixListener::bind(&path).unwrap();

          let acceptor = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            drop(stream);
          });

          let client_fd = unsafe {
            let fd = libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0);
            assert!(fd >= 0, "socket() failed: {}", std::io::Error::last_os_error());
            set_nonblocking(fd);
            fd
          };
          let client_res = unsafe { Resource::from_raw_fd(client_fd) };
          let (storage, len) = unix_sockaddr_un(&path);

          backend.push(
            53,
            Op::Connect {
              fd: client_res,
              addr: &storage,
              len,
            },
          );
          backend.flush().unwrap();

          let completed = wait_for_single_completion(&mut backend);
          assert_exact_result(&completed, 53, 0);

          acceptor.join().unwrap();
          fs::remove_file(&path).ok();
        }

        #[test]
        fn invalid_fd_reports_ebadf() {
          let mut backend = new_backend();
          backend.init(64).unwrap();

          let path = unix_socket_path("connect-ebadf");
          let (storage, len) = unix_sockaddr_un(&path);

          backend.push(
            59,
            Op::Connect {
              fd: invalid_fd_resource(),
              addr: &storage,
              len,
            },
          );
          backend.flush().unwrap();

          let completed = wait_for_single_completion(&mut backend);
          assert_exact_result(&completed, 59, -(libc::EBADF as isize));
        }

        #[test]
        fn immediate_completion_is_surfaced_on_next_wait() {
          let mut backend = new_backend();
          backend.init(64).unwrap();

          let path = unix_socket_path("connect-immediate");
          fs::remove_file(&path).ok();

          let client_fd = unsafe {
            let fd = libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0);
            assert!(fd >= 0, "socket() failed: {}", std::io::Error::last_os_error());
            set_nonblocking(fd);
            fd
          };
          let client_res = unsafe { Resource::from_raw_fd(client_fd) };
          let (storage, len) = unix_sockaddr_un(&path);

          backend.push(
            63,
            Op::Connect {
              fd: client_res,
              addr: &storage,
              len,
            },
          );
          backend.flush().unwrap();

          let completed = backend.wait(Some(Duration::ZERO)).unwrap();
          assert_eq!(completed.len(), 1);
          assert_exact_result(&completed[0], 63, -(libc::ENOENT as isize));
        }
      }

      #[cfg(unix)]
      mod unlinkat {
        use super::*;

        #[test]
        fn success_reports_zero_and_id() {
          let mut backend = new_backend();
          backend.init(64).unwrap();

          let path = temp_path("unlinkat-ok");
          fs::write(&path, b"hello").unwrap();
          let c_path =
            std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();

          backend.push(
            70,
            Op::UnlinkAt {
              dir_fd: Resource::cwd(),
              path: c_path.as_ptr(),
              flags: 0,
            },
          );
          backend.flush().unwrap();

          let completed = wait_for_single_completion(&mut backend);
          assert_exact_result(&completed, 70, 0);
          assert!(!path.exists(), "unlinkat should remove the file");
        }

        #[test]
        fn missing_path_reports_enoent() {
          let mut backend = new_backend();
          backend.init(64).unwrap();

          let path = temp_path("unlinkat-missing");
          let c_path =
            std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();

          backend.push(
            71,
            Op::UnlinkAt {
              dir_fd: Resource::cwd(),
              path: c_path.as_ptr(),
              flags: 0,
            },
          );
          backend.flush().unwrap();

          let completed = wait_for_single_completion(&mut backend);
          assert_exact_result(&completed, 71, -(libc::ENOENT as isize));
        }
      }

      #[cfg(unix)]
      mod renameat {
        use super::*;

        #[test]
        fn success_reports_zero_and_id() {
          let mut backend = new_backend();
          backend.init(64).unwrap();

          let source = temp_path("renameat-source");
          let dest = temp_path("renameat-dest");
          fs::write(&source, b"hello").unwrap();
          fs::remove_file(&dest).ok();
          let c_source =
            std::ffi::CString::new(source.as_os_str().as_bytes()).unwrap();
          let c_dest =
            std::ffi::CString::new(dest.as_os_str().as_bytes()).unwrap();

          backend.push(
            72,
            Op::RenameAt {
              old_dir_fd: Resource::cwd(),
              old_path: c_source.as_ptr(),
              new_dir_fd: Resource::cwd(),
              new_path: c_dest.as_ptr(),
            },
          );
          backend.flush().unwrap();

          let completed = wait_for_single_completion(&mut backend);
          assert_exact_result(&completed, 72, 0);
          assert!(!source.exists(), "renameat should remove the old path");
          assert!(dest.exists(), "renameat should create the new path");
          fs::remove_file(dest).ok();
        }

        #[test]
        fn missing_source_reports_enoent() {
          let mut backend = new_backend();
          backend.init(64).unwrap();

          let source = temp_path("renameat-missing");
          let dest = temp_path("renameat-target");
          fs::remove_file(&dest).ok();
          let c_source =
            std::ffi::CString::new(source.as_os_str().as_bytes()).unwrap();
          let c_dest =
            std::ffi::CString::new(dest.as_os_str().as_bytes()).unwrap();

          backend.push(
            73,
            Op::RenameAt {
              old_dir_fd: Resource::cwd(),
              old_path: c_source.as_ptr(),
              new_dir_fd: Resource::cwd(),
              new_path: c_dest.as_ptr(),
            },
          );
          backend.flush().unwrap();

          let completed = wait_for_single_completion(&mut backend);
          assert_exact_result(&completed, 73, -(libc::ENOENT as isize));
        }
      }

      #[cfg(unix)]
      mod mkdirat {
        use super::*;

        #[test]
        fn success_reports_zero_and_id() {
          let mut backend = new_backend();
          backend.init(64).unwrap();

          let path = temp_path("mkdirat-ok");
          fs::remove_dir(&path).ok();
          let c_path =
            std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();

          backend.push(
            74,
            Op::MkdirAt {
              dir_fd: Resource::cwd(),
              path: c_path.as_ptr(),
              mode: 0o755,
            },
          );
          backend.flush().unwrap();

          let completed = wait_for_single_completion(&mut backend);
          assert_exact_result(&completed, 74, 0);
          assert!(path.is_dir(), "mkdirat should create the directory");
          fs::remove_dir(path).ok();
        }

        #[test]
        fn existing_path_reports_eexist() {
          let mut backend = new_backend();
          backend.init(64).unwrap();

          let path = temp_path("mkdirat-exists");
          fs::create_dir(&path).unwrap();
          let c_path =
            std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();

          backend.push(
            75,
            Op::MkdirAt {
              dir_fd: Resource::cwd(),
              path: c_path.as_ptr(),
              mode: 0o755,
            },
          );
          backend.flush().unwrap();

          let completed = wait_for_single_completion(&mut backend);
          assert_exact_result(&completed, 75, -(libc::EEXIST as isize));
          fs::remove_dir(path).ok();
        }
      }

      #[cfg(unix)]
      mod linkat {
        use super::*;

        #[test]
        fn hard_link_success_reports_zero_and_id() {
          let mut backend = new_backend();
          backend.init(64).unwrap();

          let source = temp_path("linkat-hard-source");
          let dest = temp_path("linkat-hard-dest");
          fs::write(&source, b"hello").unwrap();
          fs::remove_file(&dest).ok();
          let c_source =
            std::ffi::CString::new(source.as_os_str().as_bytes()).unwrap();
          let c_dest =
            std::ffi::CString::new(dest.as_os_str().as_bytes()).unwrap();

          backend.push(
            76,
            Op::LinkAt {
              kind: LinkKind::Hard,
              source_dir_fd: Resource::cwd(),
              source_path: c_source.as_ptr(),
              new_dir_fd: Resource::cwd(),
              new_path: c_dest.as_ptr(),
            },
          );
          backend.flush().unwrap();

          let completed = wait_for_single_completion(&mut backend);
          assert_exact_result(&completed, 76, 0);
          assert!(dest.exists(), "hard link should create the destination");
          fs::remove_file(dest).ok();
          fs::remove_file(source).ok();
        }

        #[test]
        fn soft_link_success_reports_zero_and_id() {
          let mut backend = new_backend();
          backend.init(64).unwrap();

          let source = temp_path("linkat-soft-source");
          let dest = temp_path("linkat-soft-dest");
          fs::write(&source, b"hello").unwrap();
          fs::remove_file(&dest).ok();
          let c_source =
            std::ffi::CString::new(source.as_os_str().as_bytes()).unwrap();
          let c_dest =
            std::ffi::CString::new(dest.as_os_str().as_bytes()).unwrap();

          backend.push(
            77,
            Op::LinkAt {
              kind: LinkKind::Soft,
              source_dir_fd: Resource::cwd(),
              source_path: c_source.as_ptr(),
              new_dir_fd: Resource::cwd(),
              new_path: c_dest.as_ptr(),
            },
          );
          backend.flush().unwrap();

          let completed = wait_for_single_completion(&mut backend);
          assert_exact_result(&completed, 77, 0);
          assert!(
            fs::symlink_metadata(&dest).unwrap().file_type().is_symlink(),
            "soft link should create a symlink destination"
          );
          fs::remove_file(dest).ok();
          fs::remove_file(source).ok();
        }

        #[test]
        fn hard_link_missing_source_reports_enoent() {
          let mut backend = new_backend();
          backend.init(64).unwrap();

          let source = temp_path("linkat-missing-source");
          let dest = temp_path("linkat-missing-dest");
          fs::remove_file(&dest).ok();
          let c_source =
            std::ffi::CString::new(source.as_os_str().as_bytes()).unwrap();
          let c_dest =
            std::ffi::CString::new(dest.as_os_str().as_bytes()).unwrap();

          backend.push(
            78,
            Op::LinkAt {
              kind: LinkKind::Hard,
              source_dir_fd: Resource::cwd(),
              source_path: c_source.as_ptr(),
              new_dir_fd: Resource::cwd(),
              new_path: c_dest.as_ptr(),
            },
          );
          backend.flush().unwrap();

          let completed = wait_for_single_completion(&mut backend);
          assert_exact_result(&completed, 78, -(libc::ENOENT as isize));
        }
      }

      #[cfg(unix)]
      mod readlinkat {
        use super::*;

        #[test]
        fn success_reports_target_length_and_id() {
          let mut backend = new_backend();
          backend.init(64).unwrap();

          let target = temp_path("readlinkat-target");
          let link = temp_path("readlinkat-link");
          fs::write(&target, b"hello").unwrap();
          std::os::unix::fs::symlink(&target, &link).unwrap();
          let c_link =
            std::ffi::CString::new(link.as_os_str().as_bytes()).unwrap();
          let mut buf = [0_u8; 512];

          backend.push(
            79,
            Op::ReadlinkAt {
              dir_fd: Resource::cwd(),
              path: c_link.as_ptr(),
              buf: buf.as_mut_ptr(),
              buf_len: buf.len(),
            },
          );
          backend.flush().unwrap();

          let completed = wait_for_single_completion(&mut backend);
          assert_exact_result(&completed, 79, target.as_os_str().as_bytes().len() as isize);
          assert_eq!(
            &buf[..target.as_os_str().as_bytes().len()],
            target.as_os_str().as_bytes()
          );

          fs::remove_file(link).ok();
          fs::remove_file(target).ok();
        }

        #[test]
        fn missing_path_reports_enoent() {
          let mut backend = new_backend();
          backend.init(64).unwrap();

          let link = temp_path("readlinkat-missing");
          let c_link =
            std::ffi::CString::new(link.as_os_str().as_bytes()).unwrap();
          let mut buf = [0_u8; 64];

          backend.push(
            80,
            Op::ReadlinkAt {
              dir_fd: Resource::cwd(),
              path: c_link.as_ptr(),
              buf: buf.as_mut_ptr(),
              buf_len: buf.len(),
            },
          );
          backend.flush().unwrap();

          let completed = wait_for_single_completion(&mut backend);
          assert_exact_result(&completed, 80, -(libc::ENOENT as isize));
        }
      }

      #[cfg(unix)]
      mod batching {
        use super::*;

        #[test]
        fn multiple_immediate_completions_are_returned_together() {
          let mut backend = new_backend();
          backend.init(64).unwrap();

          let path = unix_socket_path("batch-connect");
          let (storage, len) = unix_sockaddr_un(&path);

          let mut buf = [0_u8; 4];
          let mut raw_buf = unsafe { RawBuf::from_raw_parts(buf.as_mut_ptr(), buf.len()) };

          backend.push(
            60,
            Op::Connect {
              fd: invalid_fd_resource(),
              addr: &storage,
              len,
            },
          );
          backend.push(
            61,
            Op::Read {
              fd: invalid_fd_resource(),
              iovecs: (&mut raw_buf as *mut RawBuf),
              iov_count: 1,
              offset: -1,
              flags: 0,
            },
          );
          backend.flush().unwrap();

          let mut completed = backend
            .wait(Some(Duration::ZERO))
            .unwrap()
            .iter()
            .map(|item| {
              $crate::backend::OpCompleted::new(
                item.registration_id(),
                item.result(),
              )
            })
            .collect::<Vec<_>>();
          completed.sort_by_key(|item| item.registration_id());

          assert_eq!(completed.len(), 2);
          assert_exact_result(&completed[0], 60, -(libc::EBADF as isize));
          assert_exact_result(&completed[1], 61, -(libc::EBADF as isize));
        }

        #[test]
        fn wait_buffer_is_cleared_after_consumption() {
          let mut backend = new_backend();
          backend.init(64).unwrap();

          let path = unix_socket_path("buffer-reuse");
          let (storage, len) = unix_sockaddr_un(&path);

          backend.push(
            62,
            Op::Connect {
              fd: invalid_fd_resource(),
              addr: &storage,
              len,
            },
          );
          backend.flush().unwrap();

          let completed = backend.wait(Some(Duration::ZERO)).unwrap();
          assert_eq!(completed.len(), 1);
          assert_exact_result(&completed[0], 62, -(libc::EBADF as isize));

          let completed = backend.wait(Some(Duration::ZERO)).unwrap();
          assert!(
            completed.is_empty(),
            "completed wait buffer must not retain stale completions"
          );
        }

        #[test]
        fn completions_are_not_duplicated_across_waits() {
          let mut backend = new_backend();
          backend.init(64).unwrap();

          let path = unix_socket_path("no-dup");
          let (storage, len) = unix_sockaddr_un(&path);

          backend.push(
            64,
            Op::Connect {
              fd: invalid_fd_resource(),
              addr: &storage,
              len,
            },
          );
          backend.flush().unwrap();

          let completed = backend.wait(Some(Duration::ZERO)).unwrap();
          assert_eq!(completed.len(), 1);
          assert_exact_result(&completed[0], 64, -(libc::EBADF as isize));

          let completed = backend.wait(Some(Duration::ZERO)).unwrap();
          assert!(
            completed.is_empty(),
            "a completion already returned once must not be returned again"
          );
        }

        #[test]
        fn mixed_pending_batch_completes_each_registration_exactly_once() {
          use std::os::fd::AsRawFd;

          let mut backend = new_backend();
          backend.init(64).unwrap();

          let (read_a, write_a) = socket_pair();
          let (read_b, write_b) = socket_pair();
          let payload_a = *b"ab";
          let payload_b = *b"cd";
          let mut buf_a = [0_u8; 2];
          let mut buf_b = [0_u8; 2];
          let mut raw_a = unsafe { RawBuf::from_raw_parts(buf_a.as_mut_ptr(), buf_a.len()) };
          let mut raw_b = unsafe { RawBuf::from_raw_parts(buf_b.as_mut_ptr(), buf_b.len()) };

          backend.push(
            701,
            Op::Read {
              fd: read_a.clone(),
              iovecs: (&mut raw_a as *mut RawBuf),
              iov_count: 1,
              offset: -1,
              flags: 0,
            },
          );
          backend.push(
            702,
            Op::Read {
              fd: read_b.clone(),
              iovecs: (&mut raw_b as *mut RawBuf),
              iov_count: 1,
              offset: -1,
              flags: 0,
            },
          );
          backend.flush().unwrap();

          assert!(
            backend.wait(Some(Duration::ZERO)).unwrap().is_empty(),
            "pending reads must not complete before data arrives"
          );

          let wrote_a = unsafe {
            libc::send(
              write_a.as_raw_fd(),
              payload_a.as_ptr().cast(),
              payload_a.len(),
              0,
            )
          };
          let wrote_b = unsafe {
            libc::send(
              write_b.as_raw_fd(),
              payload_b.as_ptr().cast(),
              payload_b.len(),
              0,
            )
          };
          assert_eq!(wrote_a, payload_a.len() as isize);
          assert_eq!(wrote_b, payload_b.len() as isize);

          let mut seen = std::collections::BTreeMap::new();
          for _ in 0..10 {
            let completed = backend.wait(Some(Duration::from_millis(10))).unwrap();
            for item in completed {
              let previous = seen.insert(item.registration_id(), item.result());
              assert!(
                previous.is_none(),
                "registration {} completed more than once",
                item.registration_id()
              );
            }
            if seen.len() == 2 {
              break;
            }
          }

          assert_eq!(seen.len(), 2, "expected exactly two completions");
          assert_eq!(seen.get(&701), Some(&(payload_a.len() as isize)));
          assert_eq!(seen.get(&702), Some(&(payload_b.len() as isize)));
          assert_eq!(&buf_a, &payload_a);
          assert_eq!(&buf_b, &payload_b);

          for _ in 0..3 {
            let completed = backend.wait(Some(Duration::ZERO)).unwrap();
            assert!(
              completed.is_empty(),
              "completed pending batch registrations must not be returned again"
            );
          }
        }
      }
    }
  };
}
