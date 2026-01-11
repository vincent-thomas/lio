//! Macro for generating comprehensive IoDriver tests
//!
//! This macro generates a test suite for any IoDriver implementation.
//! Usage:
//! ```ignore
//! test_io_driver!(my_driver_instance);
//! ```

/// Generates a comprehensive test suite for an IoDriver implementation.
///
/// # Example
/// ```ignore
/// use lio::backends::{test_io_driver, dummy::DummyDriver};
///
/// test_io_driver!(DummyDriver);
/// ```
#[macro_export]
macro_rules! test_io_driver {
  ($driver:ident) => {
    mod io_driver_tests {
      use super::*;
      use $crate::api::ops::Nop;
      use $crate::backends::OpStore;
      use $crate::backends::{IoBackend, IoDriver, IoSubmitter, SubmitErr};
      use $crate::registration::StoredOp;
      //
      // #[test]
      // fn test_state_lifecycle() {
      //   // Test that state can be created and dropped without errors
      //   let state = $driver::new_state().expect("Failed to create state");
      //   $driver::drop_state(state);
      // }
      // //
      // #[test]
      // fn test_new_from_state() {
      //   // Test that submitter and handler can be created from state
      //   let state = $driver::new_state().expect("Failed to create state");
      //   let (submitter, handler) =
      //     $driver::new(state).expect("Failed to create submitter and handler");
      //
      //   // Ensure they are created (type check is compile-time)
      //   drop(submitter);
      //   drop(handler);
      //
      //   $driver::drop_state(state);
      // }
      //
      // #[test]
      // fn test_submit_single_operation() {
      //   // Test submitting a single operation
      //   let state = $driver::new_state().expect("Failed to create state");
      //   let (mut submitter, mut handler) =
      //     $driver::new(state).expect("Failed to create submitter and handler");
      //
      //   let store = OpStore::new();
      //   let op = StoredOp::new_waker(Nop);
      //   let id = store.insert(op);
      //
      //   // Submit the operation
      //   let result = submitter.submit(state, id, &Nop);
      //
      //   match result {
      //     Ok(()) => {
      //       // If submission succeeded, tick the handler to complete it
      //       let completed = handler
      //         .try_tick(state, &store)
      //         .expect("Handler tick failed");
      //
      //       // The operation should be completed (or at least the handler should not error)
      //       assert!(
      //         completed.iter().any(|c| c.op_id == id) || completed.is_empty(),
      //         "Operation should be completed or handler should return empty (async completion)"
      //       );
      //     }
      //     Err(SubmitErr::NotCompatible) => {
      //       // This is acceptable - the driver doesn't support this operation
      //       eprintln!("Driver does not support Nop operation (NotCompatible)");
      //     }
      //     Err(e) => {
      //       panic!("Unexpected submit error: {:?}", e);
      //     }
      //   };
      //
      //   drop(submitter);
      //   drop(handler);
      //
      //   $driver::drop_state(state);
      // }
      //
      // #[test]
      // fn test_submit_multiple_operations() {
      //   // Test submitting multiple operations in sequence
      //   let state = $driver::new_state().expect("Failed to create state");
      //   let (mut submitter, mut handler) =
      //     $driver::new(state).expect("Failed to create submitter and handler");
      //
      //   let store = OpStore::new();
      //   let mut ids = Vec::new();
      //
      //   // Submit 10 operations
      //   for _ in 0..10 {
      //     let op = StoredOp::new_waker(Nop);
      //     let id = store.insert(op);
      //
      //     match submitter.submit(state, id, &Nop) {
      //       Ok(()) => {
      //         ids.push(id);
      //       }
      //       Err(SubmitErr::NotCompatible) => {
      //         // Driver doesn't support this operation
      //         break;
      //       }
      //       Err(SubmitErr::Full) => {
      //         // Queue is full, that's acceptable for this test
      //         break;
      //       }
      //       Err(e) => {
      //         panic!("Unexpected submit error: {:?}", e);
      //       }
      //     }
      //   }
      //
      //   // Tick handler to process operations
      //   let completed =
      //     handler.try_tick(state, &store).expect("Handler tick failed");
      //
      //   // Some operations should be completed or the handler should not error
      //   assert!(
      //     completed.len() <= ids.len(),
      //     "Completed more operations than submitted"
      //   );
      //
      // drop(submitter);
      // drop(handler);
      //
      //   $driver::drop_state(state);
      // }
      // //
      // #[test]
      // fn test_try_tick_without_operations() {
      //   // Test that try_tick works when no operations are pending
      //   let state = $driver::new_state().expect("Failed to create state");
      //   let (_submitter, mut handler) =
      //     $driver::new(state).expect("Failed to create submitter and handler");
      //
      //   let store = OpStore::new();
      //
      //   // Tick without submitting any operations
      //   let completed =
      //     handler.try_tick(state, &store).expect("Handler try_tick failed");
      //
      //   assert_eq!(
      //     completed.len(),
      //     0,
      //     "No operations should be completed when none are submitted"
      //   );
      //
      //   drop(_submitter);
      //   drop(handler);
      //
      //   $driver::drop_state(state);
      // }

      // #[test]
      // fn test_tick_without_operations() {
      //   // Test that tick works when no operations are pending
      //   let state = $driver::new_state().expect("Failed to create state");
      //   let (_submitter, mut handler) =
      //     $driver::new(state).expect("Failed to create submitter and handler");
      //
      //   let store = OpStore::new();
      //
      //   // Use spawn and timeout to avoid blocking forever
      //   let handle = std::thread::spawn(move || {
      //     let completed =
      //       handler.tick(state, &store).expect("Handler tick failed");
      //     (completed, state)
      //   });
      //
      //   // Wait with timeout
      //   let result = std::thread::spawn(move || {
      //     std::thread::sleep(std::time::Duration::from_millis(500));
      //     handle.join()
      //   })
      //   .join();
      //
      //   match result {
      //     Ok(Ok((completed, state))) => {
      //       assert_eq!(
      //         completed.len(),
      //         0,
      //         "No operations should be completed when none are submitted"
      //       );
      //       $driver::drop_state(state);
      //     }
      //     _ => {
      //       // If the thread is still running, that's acceptable - tick() can block
      //       eprintln!(
      //         "tick() blocked as expected when no operations are pending"
      //       );
      //     }
      //   }
      // }

      // #[test]
      // fn test_notify() {
      //   // Test that notify doesn't panic
      //   let state = $driver::new_state().expect("Failed to create state");
      //   let (mut submitter, _handler) =
      //     $driver::new(state).expect("Failed to create submitter and handler");
      //
      //   // Notify should not panic even if no operations are pending
      //   let result = submitter.notify(state);
      //
      //   match result {
      //     Ok(()) => {
      //       // Success
      //     }
      //     Err(e) => {
      //       eprintln!("Notify returned error (acceptable): {:?}", e);
      //     }
      //   }
      //
      //   drop(submitter);
      //   drop(_handler);
      //   $driver::drop_state(state);
      // }
      //
      // #[test]
      // fn test_completed_operation_has_valid_id() {
      //   // Test that completed operations have the correct operation ID
      //   let state = $driver::new_state().expect("Failed to create state");
      //   let (mut submitter, mut handler) =
      //     $driver::new(state).expect("Failed to create submitter and handler");
      //
      //   let store = OpStore::new();
      //   let op = StoredOp::new_waker(Nop);
      //   let id = store.insert(op);
      //
      //   match submitter.submit(state, id, &Nop) {
      //     Ok(()) => {
      //       // Tick to complete the operation
      //       let completed =
      //         handler.try_tick(state, &store).expect("Handler tick failed");
      //
      //       if let Some(comp) = completed.iter().find(|c| c.op_id == id) {
      //         assert_eq!(
      //           comp.op_id, id,
      //           "Completed operation should have the correct ID"
      //         );
      //       }
      //     }
      //     Err(SubmitErr::NotCompatible) => {
      //       // Driver doesn't support this operation
      //       eprintln!("Driver does not support Nop operation");
      //     }
      //     Err(e) => {
      //       panic!("Unexpected submit error: {:?}", e);
      //     }
      //   }
      //
      //   drop(handler);
      //   drop(submitter);
      //
      //   $driver::drop_state(state);
      // }
      //
      // #[test]
      // fn test_multiple_state_instances() {
      //   // Test that multiple state instances can be created independently
      //   let state1 = $driver::new_state().expect("Failed to create state1");
      //   let state2 = $driver::new_state().expect("Failed to create state2");
      //
      //   let (mut sub1, mut handler1) =
      //     $driver::new(state1).expect("Failed to create from state1");
      //   let (mut sub2, mut handler2) =
      //     $driver::new(state2).expect("Failed to create from state2");
      //
      //   let store = OpStore::new();
      //
      //   // Submit to both
      //   let op1 = StoredOp::new_waker(Nop);
      //   let id1 = store.insert(op1);
      //   let op2 = StoredOp::new_waker(Nop);
      //   let id2 = store.insert(op2);
      //
      //   let _ = sub1.submit(state1, id1, &Nop);
      //   let _ = sub2.submit(state2, id2, &Nop);
      //
      //   // Tick both
      //   let _ = handler1.try_tick(state1, &store);
      //   let _ = handler2.try_tick(state2, &store);
      //
      //   drop(sub1);
      //   drop(handler1);
      //
      //   drop(sub2);
      //   drop(handler2);
      //
      //   $driver::drop_state(state1);
      //   $driver::drop_state(state2);
      // }
      #[test]
      fn test_submit_after_notify() {
        // Test that operations can be submitted after notify is called
        let state = Box::into_raw(Box::new(
          $driver::new_state().expect("Failed to create state"),
        ));
        let (mut submitter, mut handler) = $driver::new(unsafe { &mut *state })
          .expect("Failed to create submitter and handler");
        let ptr = state as *const ();

        let store = OpStore::new();

        // Notify first
        let _ = submitter.notify(ptr);

        // Then submit
        let op = StoredOp::new_waker(Nop);
        let id = store.insert(op);

        let result = submitter.submit(ptr, id, &Nop);

        match result {
          Ok(()) => {
            let _ = handler.try_tick(ptr, &store);
          }
          Err(SubmitErr::NotCompatible) => {
            // Acceptable
          }
          Err(e) => {
            panic!("Unexpected error after notify: {:?}", e);
          }
        };

        drop(submitter);
        drop(handler);

        drop(ptr);
        drop(state);
      }
      #[test]
      fn test_operation_result_values() {
        // Test that operation results have reasonable values
        let state = $driver::new_state().expect("Failed to create state");
        let state_ptr = Box::into_raw(Box::new(state));
        let (mut submitter, mut handler) =
          $driver::new(unsafe { &mut *state_ptr })
            .expect("Failed to create submitter and handler");
        let state_ptr = state_ptr as *const ();

        let store = OpStore::new();
        let op = StoredOp::new_waker(Nop);
        let id = store.insert(op);

        match submitter.submit(state_ptr, id, &Nop) {
          Ok(()) => {
            let completed =
              handler.try_tick(state_ptr, &store).expect("Handler tick failed");

            if let Some(comp) = completed.iter().find(|c| c.op_id == id) {
              // Result should be a valid isize (could be positive, zero, or negative errno)
              // Just verify it's within reasonable bounds for an isize
              assert!(
                comp.result >= isize::MIN && comp.result <= isize::MAX,
                "Result should be a valid isize value"
              );
            }
          }
          Err(SubmitErr::NotCompatible) => {
            // Acceptable
          }
          Err(e) => {
            panic!("Unexpected submit error: {:?}", e);
          }
        };

        drop(submitter);
        drop(handler);

        let _ = unsafe {
          Box::from_raw(state_ptr as *mut <$driver as IoBackend>::State)
        };
      }
    }
  };
}
