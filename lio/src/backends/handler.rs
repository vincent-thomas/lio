use std::{io, sync::Arc};

use crate::{backends::OpCompleted, store::OpStore};

pub trait IoHandler {
  fn try_tick(
    &mut self,
    state: *const (),
    store: &OpStore,
  ) -> io::Result<Vec<OpCompleted>>;
  fn tick(
    &mut self,
    state: *const (),
    store: &OpStore,
  ) -> io::Result<Vec<OpCompleted>>;
}

pub struct Handler {
  io: Box<dyn IoHandler>,
  state: *const (),
  store: Arc<OpStore>,
}

unsafe impl Send for Handler {}

impl Handler {
  pub fn new(
    io: Box<dyn IoHandler>,
    store: Arc<OpStore>,
    state: *const (),
  ) -> Self {
    Self { io, store, state }
  }

  pub fn tick(&mut self) -> io::Result<()> {
    let result = self.io.tick(self.state, &self.store)?;

    for one in result {
      eprintln!(
        "[Handler::tick] Setting op_id={} as done with result={}",
        one.op_id, one.result
      );
      let exists =
        self.store.get_mut(one.op_id, |reg| reg.set_done(one.result));

      eprintln!(
        "[Handler::tick] Op {} exists in store: {:?}",
        one.op_id,
        exists.is_some()
      );
    }
    Ok(())
  }

  #[cfg(test)]
  pub fn try_tick(&mut self) -> io::Result<()> {
    let result = self.io.try_tick(self.state, &self.store)?;
    // assert!(!result.is_empty());

    for one in result {
      let _exists = self.store.get_mut(one.op_id, |reg| {
        reg.set_done(one.result);
      });
    }

    Ok(())
  }
}
