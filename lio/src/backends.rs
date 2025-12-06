use crate::{OperationProgress, driver::OpStore, op::Operation};

#[cfg(linux)]
mod io_uring;
#[cfg(linux)]
pub use io_uring::*;
#[cfg(not(linux))]
mod polling;
#[cfg(not(linux))]
pub use polling::*;
mod threading;
#[allow(unused_imports)]
pub use threading::*;

pub trait IoBackend {
  fn tick(&self, store: &OpStore, can_wait: bool);
  fn submit<O>(&self, op: O, store: &OpStore) -> OperationProgress<O>
  where
    O: Operation + Sized;
  fn notify(&self);
}
