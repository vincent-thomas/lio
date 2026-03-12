use std::io;
use std::time::Duration;

#[cfg(linux)]
use crate::api::resource::Resource;
use crate::typed_op::TypedOp;
#[cfg(target_os = "linux")]
use std::os::fd::{AsRawFd, FromRawFd, RawFd};

pub struct Timeout {
  duration: Duration,
  #[cfg(linux)]
  timespec: libc::timespec,
  #[cfg(target_os = "linux")]
  timer_res: Resource,
  #[cfg(all(unix, not(target_os = "linux")))]
  timer_id: u64,
}

assert_op_max_size!(Timeout);

// wtf are you doing waiting and then not waiting??

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
      timespec: libc::timespec {
        tv_sec: duration.as_secs() as libc::time_t,
        tv_nsec: duration.subsec_nanos() as libc::c_long,
      },
      #[cfg(target_os = "linux")]
      timer_res: unsafe { Resource::from_raw_fd(timer_fd) },
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

impl TypedOp for Timeout {
  type Result = std::io::Result<()>;

  fn into_op(&mut self) -> crate::op::Op {
    crate::op::Op::Timeout {
      duration: self.duration,
      #[cfg(target_os = "linux")]
      timer_fd: self.timer_res.clone(),
      #[cfg(target_os = "linux")]
      timespec: &self.timespec as *const libc::timespec,
    }
  }

  fn extract_result(self, res: isize) -> Self::Result {
    if res == 0 {
      Ok(())
    } else {
      match res.abs() as i32 {
        #[cfg(target_os = "linux")]
        libc::ETIME => Ok(()),
        #[cfg(any(target_os = "freebsd", target_os = "macos"))]
        libc::ETIMEDOUT => Ok(()),
        _ => Err(io::Error::last_os_error()),
      }
    }
  }

  // #[cfg(unix)]
  // fn meta(&self) -> crate::operation::OpMeta {
  //   crate::operation::OpMeta::CAP_TIMER
  // }

  // #[cfg(unix)]
  // fn cap(&self) -> i32 {
  //   #[cfg(target_os = "linux")]
  //   return self.timer_res.as_raw_fd();
  //   #[cfg(not(target_os = "linux"))]
  //   return -1;
  // }

  // fn run_blocking(&self) -> isize {
  //   std::thread::sleep(self.duration);
  //   0
  // }
}
