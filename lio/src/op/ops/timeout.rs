use crate::op::{DetachSafe, OpMeta};
use std::os::fd::RawFd;
#[cfg(target_os = "linux")]
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::time::Duration;

#[cfg(linux)]
use io_uring::{opcode, squeue, types::Timespec};

use crate::op::{Operation, OperationExt};

pub struct Timeout {
  duration: Duration,
  #[cfg(linux)]
  timespec: Timespec,
  #[cfg(target_os = "linux")]
  timer_fd: OwnedFd,
  #[cfg(all(unix, not(target_os = "linux")))]
  timer_id: u64,
}

// wtf are you doing waiting and then not waiting??
unsafe impl DetachSafe for Timeout {}

impl Timeout {
  pub(crate) fn new(duration: Duration) -> Self {
    Self::new_with_id(duration, 0)
  }

  pub(crate) fn new_with_id(
    duration: Duration,
    #[allow(unused)] id: u64,
  ) -> Self {
    #[cfg(target_os = "linux")]
    let timer_fd =
      Self::create_timer_fd(duration).expect("Failed to create timerfd");

    Self {
      duration,
      #[cfg(linux)]
      timespec: Timespec::new()
        .sec(duration.as_secs())
        .nsec(duration.subsec_nanos()),
      #[cfg(target_os = "linux")]
      timer_fd: unsafe { OwnedFd::from_raw_fd(timer_fd) },
      #[cfg(kqueue)]
      timer_id: id,
    }
  }

  #[cfg(target_os = "linux")]
  fn create_timer_fd(duration: Duration) -> std::io::Result<RawFd> {
    use std::mem::MaybeUninit;

    // Create timerfd
    let fd = syscall!(timerfd_create(
      libc::CLOCK_MONOTONIC,
      libc::TFD_NONBLOCK | libc::TFD_CLOEXEC
    ))?;

    // Set the timeout
    let mut new_value: libc::itimerspec =
      unsafe { MaybeUninit::zeroed().assume_init() };
    new_value.it_value.tv_sec = duration.as_secs() as libc::time_t;
    new_value.it_value.tv_nsec = duration.subsec_nanos() as libc::c_long;
    // it_interval is zero (no repeat)

    syscall!(timerfd_settime(
      fd,
      0,
      &new_value as *const libc::itimerspec,
      std::ptr::null_mut(),
    ))?;

    Ok(fd)
  }

  pub fn duration(&self) -> Duration {
    self.duration
  }

  #[cfg(kqueue)]
  pub fn timer_id(&self) -> u64 {
    self.timer_id
  }
}

impl OperationExt for Timeout {
  type Result = std::io::Result<()>;
}

impl Operation for Timeout {
  impl_result!(|_this, res: std::io::Result<i32>| -> std::io::Result<()> {
    match res {
      Ok(v) => {
        assert!(v == 0);
        Ok(())
      }
      Err(err) => {
        if let Some(inner_err) = err.raw_os_error() {
          // io_uring returns -ETIME, pollingv2 returns ETIME
          match inner_err.abs() {
            libc::ETIME => Ok(()),
            _ => Err(err),
          }
        } else {
          Err(err)
        }
      }
    }
  });

  #[cfg(kqueue)]
  fn meta(&self) -> OpMeta {
    OpMeta::CAP_TIMER
  }

  #[cfg(not(kqueue))]
  fn meta(&self) -> OpMeta {
    OpMeta::CAP_FD | OpMeta::FD_READ
  }

  // #[cfg(linux)]
  // const OPCODE: u8 = 11;

  #[cfg(linux)]
  fn create_entry(&mut self) -> squeue::Entry {
    opcode::Timeout::new(&self.timespec as *const _).build()
  }

  /// Very special case here for kqueue.
  /// Return duration here.
  #[cfg(unix)]
  fn cap(&self) -> RawFd {
    #[cfg(linux)]
    {
      Some(self.timer_fd.as_raw_fd())
    }
    #[cfg(kqueue)]
    {
      // For kqueue, encode duration as milliseconds in the "fd"
      self.duration.as_millis() as RawFd
    }
  }

  fn run_blocking(&self) -> std::io::Result<i32> {
    #[cfg(linux)]
    {
      // When the timer expires, read from the timerfd to clear it
      let mut buf = [0u8; 8];
      syscall!(read(
        self.timer_fd.as_raw_fd(),
        buf.as_mut_ptr() as *mut libc::c_void,
        8
      ))?;
      return Ok(0);
    }

    #[cfg(kqueue)]
    {
      // For kqueue timers, run_blocking is called AFTER the timer fires
      // from EVFILT_TIMER, so we just return success immediately
      return Ok(0);
    }
  }
}
