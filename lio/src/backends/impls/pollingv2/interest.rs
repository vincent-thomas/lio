/// Interest/Event flags for I/O readiness
///
/// This type is used for both:
/// - Registering interest (what you want to be notified about)
/// - Receiving events (what actually happened)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Interest {
  bits: u8,
}

impl Interest {
  pub const NONE: Self = Self { bits: 0 };
  pub const READ: Self = Self { bits: 1 << 0 };
  pub const WRITE: Self = Self { bits: 1 << 1 };
  pub const TIMER: Self = Self { bits: 1 << 2 };
  pub const READ_AND_WRITE: Self =
    Self { bits: Self::READ.bits | Self::WRITE.bits };

  pub const fn is_readable(self) -> bool {
    self.bits & Self::READ.bits != 0
  }

  pub const fn is_writable(self) -> bool {
    self.bits & Self::WRITE.bits != 0
  }

  pub const fn is_timer(self) -> bool {
    self.bits & Self::TIMER.bits != 0
  }

  pub const fn is_none(self) -> bool {
    self.bits == 0
  }

  /// Combine interests using bitwise OR
  pub const fn or(self, other: Self) -> Self {
    Self { bits: self.bits | other.bits }
  }

  /// Check if this interest contains all bits from another
  pub const fn contains(self, other: Self) -> bool {
    (self.bits & other.bits) == other.bits
  }
}

impl std::ops::BitOr for Interest {
  type Output = Self;

  fn bitor(self, rhs: Self) -> Self::Output {
    self.or(rhs)
  }
}

impl std::ops::BitOrAssign for Interest {
  fn bitor_assign(&mut self, rhs: Self) {
    *self = self.or(rhs);
  }
}
