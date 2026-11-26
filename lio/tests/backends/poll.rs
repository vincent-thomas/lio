//! The backend contract against a poller written outside lio, on `poll(2)`.
//!
//! It shows what a new readiness API takes: the five required methods of
//! [`ReadinessPoll`], and [`Poller::with_poller`] is a backend that queues,
//! cancels and keeps to its capacity like lio's own.

mod on_poll {
  use std::cell::RefCell;
  use std::io;
  use std::os::fd::RawFd;
  use std::time::Duration;

  use lio::backend::impls::{Interest, ReadinessPoll};

  /// One registered descriptor.
  struct Watch {
    fd: RawFd,
    key: u64,
    interest: Interest,
    /// Cleared once an event was reported: a registration is one-shot.
    armed: bool,
  }

  /// What `wait` reports for a descriptor.
  #[derive(Clone, Copy)]
  pub struct Event {
    key: u64,
    ready: Interest,
  }

  /// A readiness poller that keeps its registrations here and asks the
  /// kernel about them with one `poll` call per wait.
  #[derive(Default)]
  pub struct PollPoller {
    watches: RefCell<Vec<Watch>>,
  }

  /// Any of these on a descriptor wakes its readers: a read then returns
  /// the data, the end of the stream, or the error.
  const READ_SIDE: libc::c_short =
    libc::POLLIN | libc::POLLHUP | libc::POLLERR | libc::POLLNVAL;
  /// And these its writers.
  const WRITE_SIDE: libc::c_short =
    libc::POLLOUT | libc::POLLHUP | libc::POLLERR | libc::POLLNVAL;

  impl ReadinessPoll for PollPoller {
    type NativeEvent = Event;

    fn open() -> io::Result<Self> {
      Ok(Self::default())
    }

    fn blank_event() -> Event {
      Event { key: u64::MAX, ready: Interest::NONE }
    }

    fn add(&self, fd: RawFd, key: u64, interest: Interest) -> io::Result<()> {
      let mut watches = self.watches.borrow_mut();
      if watches.iter().any(|watch| watch.fd == fd) {
        return Err(io::Error::from_raw_os_error(libc::EEXIST));
      }
      // SAFETY: `fcntl(F_GETFD)` only inspects the descriptor table.
      if unsafe { libc::fcntl(fd, libc::F_GETFD) } < 0 {
        return Err(io::Error::last_os_error());
      }
      watches.push(Watch { fd, key, interest, armed: true });
      Ok(())
    }

    fn delete(&self, fd: RawFd) -> io::Result<()> {
      let mut watches = self.watches.borrow_mut();
      let Some(index) = watches.iter().position(|watch| watch.fd == fd) else {
        return Err(io::Error::from_raw_os_error(libc::ENOENT));
      };
      watches.swap_remove(index);
      Ok(())
    }

    fn wait(
      &self,
      events: &mut [Event],
      timeout: Option<Duration>,
    ) -> io::Result<usize> {
      let mut watches = self.watches.borrow_mut();
      let mut fds: Vec<libc::pollfd> = watches
        .iter()
        .filter(|watch| watch.armed)
        .map(|watch| {
          let mut asked = 0;
          if watch.interest.is_readable() {
            asked |= libc::POLLIN;
          }
          if watch.interest.is_writable() {
            asked |= libc::POLLOUT;
          }
          libc::pollfd { fd: watch.fd, events: asked, revents: 0 }
        })
        .collect();
      let millis = match timeout {
        None => -1,
        Some(timeout) => {
          timeout.as_nanos().div_ceil(1_000_000).min(i32::MAX as u128) as i32
        }
      };
      // SAFETY: `fds` is a live array of `fds.len()` entries.
      let rc = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as _, millis) };
      if rc < 0 {
        let err = io::Error::last_os_error();
        return if err.kind() == io::ErrorKind::Interrupted {
          Ok(0)
        } else {
          Err(err)
        };
      }

      let mut written = 0;
      for polled in fds.iter().filter(|polled| polled.revents != 0) {
        let Some(slot) = events.get_mut(written) else {
          break; // Reported by the next wait: its watch stays armed.
        };
        let Some(watch) = watches.iter_mut().find(|w| w.fd == polled.fd) else {
          continue;
        };
        let mut ready = Interest::NONE;
        if watch.interest.is_readable() && polled.revents & READ_SIDE != 0 {
          ready |= Interest::READ;
        }
        if watch.interest.is_writable() && polled.revents & WRITE_SIDE != 0 {
          ready |= Interest::WRITE;
        }
        watch.armed = false;
        *slot = Event { key: watch.key, ready };
        written += 1;
      }
      Ok(written)
    }

    fn event_key(event: &Event) -> u64 {
      event.key
    }

    fn event_interest(event: &Event) -> Interest {
      event.ready
    }
  }
}

lio_test::test_io_backend!(lio::backend::impls::Poller::with_poller(
  on_poll::PollPoller::default()
));
