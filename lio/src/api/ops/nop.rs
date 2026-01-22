use crate::operation::{Operation, OperationExt};

#[cfg(unix)]
use crate::operation::OpMeta;

#[cfg(windows)]
use crate::operation::IocpStartResult;

pub struct Nop;

assert_op_max_size!(Nop);

impl OperationExt for Nop {
  type Result = ();
}

impl Operation for Nop {
  impl_result!(());

  #[cfg(unix)]
  fn meta(&self) -> OpMeta {
    OpMeta::CAP_NONE
  }

  #[cfg(linux)]
  fn create_entry(&self) -> lio_uring::submission::Entry {
    lio_uring::operation::Nop::new().build()
  }

  fn run_blocking(&self) -> isize {
    0
  }

  // ═══════════════════════════════════════════════════════════════════════════
  // Windows IOCP Support
  // ═══════════════════════════════════════════════════════════════════════════

  #[cfg(windows)]
  fn start_iocp(
    &self,
    _overlapped: *mut windows_sys::Win32::System::IO::OVERLAPPED,
  ) -> IocpStartResult {
    // NOP completes immediately with result 0
    IocpStartResult::Completed(0)
  }
}
