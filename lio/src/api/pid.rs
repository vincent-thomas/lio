/// Opaque process identifier.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Pid(i64);

impl Pid {
  // Only the Unix spawn model creates pids so far.
  #[cfg_attr(windows, allow(dead_code))]
  pub(crate) const fn from_raw(raw: i64) -> Self {
    Self(raw)
  }

  pub const fn as_raw(self) -> i64 {
    self.0
  }
}
