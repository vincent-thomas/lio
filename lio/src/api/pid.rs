/// Opaque process identifier.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Pid(i64);

impl Pid {
  pub(crate) const fn from_raw(raw: i64) -> Self {
    Self(raw)
  }

  pub const fn as_raw(self) -> i64 {
    self.0
  }
}
