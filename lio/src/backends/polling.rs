#[cfg(feature = "high")]
use crate::op_registration::OpNotification;
use crate::sync::Mutex;
use std::io::ErrorKind;
use std::os::fd::BorrowedFd;
use std::{collections::HashMap, io, os::fd::RawFd};

use crate::{
  OperationProgress,
  backends::IoBackend,
  driver::OpStore,
  op::{EventType, Operation},
};

pub struct Polling {
  inner: polling::Poller,
  // inner_io: Threading,
  fd_map: Mutex<HashMap<u64, RawFd>>,
}

impl IoBackend for Polling {
  fn submit<O>(&self, op: O, store: &OpStore) -> OperationProgress<O>
  where
    O: Operation + Sized,
  {
    if O::EVENT_TYPE.is_none() {
      return OperationProgress::FromResult {
        res: Some(op.run_blocking()),
        operation: op,
      };
    };

    if !O::IS_CONNECT {
      return self.new_polling(op, store);
    };

    let result = op.run_blocking();

    if result
      .as_ref()
      .is_err_and(|err| err.raw_os_error() == Some(libc::EINPROGRESS))
    {
      self.new_polling(op, store)
    } else {
      OperationProgress::<O>::new_from_result(op, result)
    }
  }
  fn notify(&self) {
    self.inner.notify().unwrap();
  }
  fn tick(&self, store: &OpStore, can_wait: bool) {
    let events =
      self.interest_wait(can_wait).expect("background thread failed");

    for event in events.iter() {
      let operation_id = event.key as u64;

      // Look up fd from our internal map
      let entry_fd = *self
        .fd_map
        .lock()
        .get(&operation_id)
        .expect("couldn't find fd for operation");

      let result = store
        .get_mut(operation_id, |entry| entry.run_blocking())
        .expect("couldn't find entry");

      match result {
        // Special-case.
        Err(err)
          if err.kind() == ErrorKind::WouldBlock
            || err.raw_os_error() == Some(libc::EINPROGRESS) =>
        {
          self.modify_interest(entry_fd, event).expect("fd sure exists");

          continue;
        }
        _ => {
          // Clean up fd from our internal map

          self.delete_interest(entry_fd).unwrap();
          self.fd_map.lock().remove(&operation_id);

          let set_done_result = store
            .get_mut(operation_id, |reg| {
              // if should keep.
              reg.set_done(result)
            })
            .expect("Cannot find matching operation");
          match set_done_result {
            None => {}
            Some(value) => match value {
              #[cfg(feature = "high")]
              OpNotification::Waker(waker) => waker.wake(),
              OpNotification::Callback(callback) => {
                store.get_mut(operation_id, |entry| callback.call(entry));
                assert!(store.remove(operation_id));
              }
            },
          }
        }
      };
    }
  }
}

impl Polling {
  pub fn new() -> Self {
    Self {
      inner: polling::Poller::new().unwrap(),
      // inner_io: Threading::new(),
      fd_map: Mutex::new(HashMap::new()),
    }
  }
  fn new_polling<T>(&self, op: T, store: &OpStore) -> OperationProgress<T>
  where
    T: Operation,
  {
    let fd = op.fd().expect("not provided fd");

    assert!(fd > 0, "reserve_driver_entry: invalid fd {}", fd);
    let id = store.next_id();

    // Store fd in Polling's internal map
    self.fd_map.lock().insert(id, fd);

    store.insert(id, Box::new(op));

    self
      .add_interest(
        fd,
        id,
        T::EVENT_TYPE.expect("op is event but no event_type??"),
      )
      .expect("fd sure exists");
    OperationProgress::<T>::new_store_tracked(id)
  }

  pub(crate) fn add_interest(
    &self,
    fd: RawFd,
    key: u64,
    _event: EventType,
  ) -> io::Result<()> {
    let event = match _event {
      EventType::Read => polling::Event::readable(key as usize),
      EventType::Write => polling::Event::writable(key as usize),
    };

    let result = unsafe { self.inner.add(&BorrowedFd::borrow_raw(fd), event) };

    if let Err(err) = result {
      match err.kind() {
        // If a interest already exists. Expected if multiple accept calls
        // to the same fd is called.
        ErrorKind::AlreadyExists => {
          println!("fd: {fd} already has interest");
          return Ok(());
        }
        _ => return Err(err),
      };
    };

    Ok(())
  }
  pub(crate) fn modify_interest(
    &self,
    fd: RawFd,
    event: polling::Event,
  ) -> io::Result<()> {
    unsafe {
      use std::os::fd::BorrowedFd;

      self.inner.modify(BorrowedFd::borrow_raw(fd), event)
    }
  }

  pub(crate) fn delete_interest(&self, fd: RawFd) -> io::Result<()> {
    unsafe {
      use std::os::fd::BorrowedFd;

      self.inner.delete(BorrowedFd::borrow_raw(fd))
    }
  }
  pub(crate) fn interest_wait(
    &self,
    can_wait: bool,
  ) -> io::Result<polling::Events> {
    let mut events = polling::Events::new();
    let _ = self.inner.wait(
      &mut events,
      if can_wait {
        None
      } else {
        use std::time::Duration;
        // TODO: is it really 1ms?.
        Some(Duration::from_millis(1))
      },
    )?;
    Ok(events)
  }
}
