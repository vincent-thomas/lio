//! Worker threads for managing I/O submission and completion.
//!
//! The worker system consists of two threads:
//! - **Submitter thread**: Receives operations via channel, submits them to the backend
//! - **Handler thread**: Polls for completions, processes results, invokes callbacks/wakers

use crate::backends::{Driver, SubmitErr, Submitter};
use crate::registration::StoredOp;
use std::{
  sync::mpsc::{self, Receiver, Sender},
  thread::{self, JoinHandle},
};

/// Message sent to the submitter thread
pub enum SubmitMsg {
  Submit { op: StoredOp, response_tx: Sender<Result<u64, SubmitErr>> },
  Shutdown,
}

/// Manages the submitter and handler worker threads
pub struct Worker {
  submit_tx: Sender<SubmitMsg>,
  submitter_thread: Option<JoinHandle<()>>,
  handler_thread: Option<JoinHandle<()>>,
  shutdown_tx: Option<Sender<()>>,
}

impl Worker {
  /// Spawns submitter and handler threads
  pub fn spawn(mut submitter: Submitter, mut handler: Driver) -> Self {
    let (submit_tx, submit_rx) = mpsc::channel::<SubmitMsg>();
    let (shutdown_tx, shutdown_rx) = mpsc::channel::<()>();

    // Spawn submitter thread
    let submitter_thread = thread::Builder::new()
      .name("lio-submitter".into())
      .spawn(move || Self::submitter_loop(submit_rx, &mut submitter))
      .expect("failed to spawn submitter thread");

    // Spawn handler thread
    let shutdown_rx_clone = shutdown_rx;
    let handler_thread = thread::Builder::new()
      .name("lio-handler".into())
      .spawn(move || Self::handler_loop(shutdown_rx_clone, &mut handler))
      .expect("failed to spawn handler thread");

    Worker {
      submit_tx,
      submitter_thread: Some(submitter_thread),
      handler_thread: Some(handler_thread),
      shutdown_tx: Some(shutdown_tx),
    }
  }

  /// Submit an operation (blocks until submitter thread processes it and returns result)
  pub fn submit(&self, op: StoredOp) -> Result<u64, SubmitErr> {
    let (response_tx, response_rx) = mpsc::channel();

    if self.submit_tx.send(SubmitMsg::Submit { op, response_tx }).is_err() {
      return Err(SubmitErr::DriverShutdown);
    }

    // Wait for submitter thread to process and respond
    response_rx.recv().unwrap_or(Err(SubmitErr::DriverShutdown))
  }

  /// Submitter thread loop
  fn submitter_loop(rx: Receiver<SubmitMsg>, submitter: &mut Submitter) {
    loop {
      match rx.recv() {
        Ok(SubmitMsg::Submit { op, response_tx }) => {
          // Submit to backend and send result back
          let result = submitter.submit(op);
          // Notify handler thread to wake up from blocking tick() and process the operation
          let _ = response_tx.send(result); // Ignore if receiver dropped
        }
        Ok(SubmitMsg::Shutdown) => {
          let _ = submitter.notify();
          // Channel closed or shutdown requested
          break;
        }
        Err(_) => {
          panic!("lio error: invalid shutdown logic");
        }
      }
    }
  }

  /// Handler thread loop
  fn handler_loop(shutdown_rx: Receiver<()>, handler: &mut Driver) {
    // eprintln!("[Worker::handler_loop] Handler thread started");
    loop {
      // Check for shutdown signal (non-blocking)
      match shutdown_rx.try_recv() {
        Ok(_) => {
          break;
        }
        Err(mpsc::TryRecvError::Disconnected) => {
          break;
        }
        Err(mpsc::TryRecvError::Empty) => {} // No shutdown yet, continue
      }

      // eprintln!("[Worker::handler_loop] Calling handler.tick()");
      // Poll for completions
      match handler.tick() {
        Ok(()) => {}
        Err(e) => {
          // Consider: should we break on error or continue?
          // eprintln!("[Worker::handler_loop] handler.tick() error: {:?}", e);
          panic!("lio: handler tick error: {:?}", e);
        }
      }
    }
  }

  /// Shutdown the worker threads gracefully
  pub fn shutdown(&mut self) {
    // Signal handler to shutdown
    if let Some(tx) = self.shutdown_tx.take() {
      tx.send(()).unwrap();
    }
    // Signal submitter to shutdown
    let _ = self.submit_tx.send(SubmitMsg::Shutdown);

    // // Wait for threads to finish
    if let Some(handle) = self.submitter_thread.take() {
      let _ = handle.join();
    }
    //
    if let Some(handle) = self.handler_thread.take() {
      let _ = handle.join();
    }
  }
}

impl Drop for Worker {
  fn drop(&mut self) {
    self.shutdown();
  }
}

// #[cfg(test)]
// mod tests {
//   use super::*;
//   use crate::{
//     backends::{IoDriver, dummy::DummyDriver},
//     op::Nop,
//     registration::StoredOp,
//     store::OpStore,
//   };
//   use std::{
//     sync::{
//       Arc,
//       atomic::{AtomicBool, AtomicUsize, Ordering},
//     },
//     thread,
//     time::Duration,
//   };
//
//   /// Helper to create a Worker with DummyDriver
//   fn setup_worker() -> (Worker, Arc<OpStore>) {
//     let state = Box::into_raw(Box::new(DummyDriver::new_state().unwrap()));
//     let (submitter_impl, handler_impl) =
//       DummyDriver::new(unsafe { &mut *state }).unwrap();
//
//     let store = Arc::new(OpStore::with_capacity(1024));
//
//     let submitter = Submitter::new(
//       Box::new(submitter_impl),
//       Arc::clone(&store),
//       state as *const (),
//     );
//     let handler = Handler::new(
//       Box::new(handler_impl),
//       Arc::clone(&store),
//       state as *const (),
//     );
//
//     let worker = Worker::spawn(submitter, handler);
//
//     (worker, store)
//   }
//
//   #[test]
//   fn test_worker_basic_submit() {
//     let (worker, _store) = setup_worker();
//
//     let (sender, recv) = mpsc::channel();
//
//     let op = StoredOp::new_callback(Box::new(Nop), |_| {
//       sender.send(()).unwrap();
//     });
//     let result = worker.submit(op);
//
//     let _id = result.expect("Submit should succeed");
//
//     // Give handler thread time to process
//     thread::sleep(Duration::from_millis(50));
//
//     recv.try_recv().unwrap();
//   }
//
//   #[test]
//   fn test_storedop_completes_with_timeout() {
//     let (mut worker, _store) = setup_worker();
//
//     let (sender, recv) = mpsc::channel();
//
//     let op = StoredOp::new_callback(Box::new(Nop), move |res| {
//       sender.send(res).unwrap();
//     });
//
//     let result = worker.submit(op);
//     assert!(result.is_ok(), "Submit should succeed");
//
//     // Use recv_timeout to detect if operation completes
//     // If notify is not called, handler won't process and this will timeout
//     let completion = recv.recv_timeout(Duration::from_secs(2));
//
//     assert!(
//       completion.is_ok(),
//       "StoredOp should complete within 2 seconds. If this fails, handler may not be notified after submit."
//     );
//
//     // Shutdown worker to cleanup threads
//     worker.shutdown();
//   }
//
//   #[test]
//   fn test_worker_multiple_submits() {
//     let (worker, _store) = setup_worker();
//
//     for _ in 0..100 {
//       let op = StoredOp::new_waker(Nop);
//       let result = worker.submit(op);
//       assert!(result.is_ok(), "All submits should succeed");
//     }
//
//     // Give handler thread time to process
//     thread::sleep(Duration::from_millis(100));
//   }
//
//   #[test]
//   fn test_worker_concurrent_submits_from_multiple_threads() {
//     let worker = Arc::new(setup_worker().0);
//     let num_threads = 8;
//     let ops_per_thread = 50;
//
//     let handles: Vec<_> = (0..num_threads)
//       .map(|_| {
//         let worker = Arc::clone(&worker);
//         thread::spawn(move || {
//           for _ in 0..ops_per_thread {
//             let op = StoredOp::new_waker(Nop);
//             let result = worker.submit(op);
//             assert!(result.is_ok(), "Submit should succeed");
//           }
//         })
//       })
//       .collect();
//
//     for handle in handles {
//       handle.join().unwrap();
//     }
//
//     // Give handler thread time to process
//     thread::sleep(Duration::from_millis(100));
//   }
//
//   #[test]
//   fn test_worker_shutdown_graceful() {
//     let (mut worker, _store) = setup_worker();
//
//     // Submit some operations
//     for _ in 0..10 {
//       let op = StoredOp::new_waker(Nop);
//       assert!(worker.submit(op).is_ok());
//     }
//
//     // Shutdown should complete without hanging
//     worker.shutdown();
//
//     // Submitting after shutdown should fail
//     let op = StoredOp::new_waker(Nop);
//     let result = worker.submit(op);
//     assert!(result.is_err(), "Submit after shutdown should fail");
//   }
//
//   #[test]
//   fn test_worker_drop_triggers_shutdown() {
//     let (worker, _store) = setup_worker();
//
//     // Submit some operations
//     for _ in 0..5 {
//       let op = StoredOp::new_waker(Nop);
//       assert!(worker.submit(op).is_ok());
//     }
//
//     // Drop should trigger shutdown
//     drop(worker);
//
//     // Test passes if drop completes without hanging
//   }
//
//   #[test]
//   fn test_worker_rapid_shutdown() {
//     let (mut worker, _store) = setup_worker();
//
//     // Shutdown immediately without submitting anything
//     worker.shutdown();
//
//     // Should complete cleanly
//   }
//
//   #[test]
//   fn test_worker_shutdown_while_submitting() {
//     let (worker, _store) = setup_worker();
//     let worker = Arc::new(worker);
//     let worker_clone = Arc::clone(&worker);
//     let keep_submitting = Arc::new(AtomicBool::new(true));
//     let keep_submitting_clone = Arc::clone(&keep_submitting);
//
//     // Spawn thread that continuously submits
//     let submit_thread = thread::spawn(move || {
//       while keep_submitting_clone.load(Ordering::Relaxed) {
//         let op = StoredOp::new_waker(Nop);
//         let _ = worker_clone.submit(op);
//       }
//     });
//
//     // Let it submit for a bit
//     thread::sleep(Duration::from_millis(50));
//
//     // Signal to stop and shutdown
//     keep_submitting.store(false, Ordering::Relaxed);
//     submit_thread.join().unwrap();
//
//     // Get mutable access by unwrapping Arc
//     let mut worker = match Arc::try_unwrap(worker) {
//       Ok(w) => w,
//       Err(_) => panic!("Failed to unwrap Arc"),
//     };
//     worker.shutdown();
//   }
//
//   #[test]
//   fn test_worker_high_throughput() {
//     let (worker, _store) = setup_worker();
//     let num_ops = 1000;
//
//     let start = std::time::Instant::now();
//
//     for _ in 0..num_ops {
//       let op = StoredOp::new_waker(Nop);
//       worker.submit(op).unwrap();
//     }
//
//     let elapsed = start.elapsed();
//     println!(
//       "Submitted {} operations in {:?} ({:.2} ops/sec)",
//       num_ops,
//       elapsed,
//       num_ops as f64 / elapsed.as_secs_f64()
//     );
//
//     // Give handler time to process
//     thread::sleep(Duration::from_millis(200));
//   }
//
//   #[test]
//   fn test_worker_interleaved_submit_shutdown() {
//     for _ in 0..10 {
//       let (mut worker, _store) = setup_worker();
//
//       // Submit a few ops
//       for _ in 0..5 {
//         let op = StoredOp::new_waker(Nop);
//         assert!(worker.submit(op).is_ok());
//       }
//
//       // Shutdown
//       worker.shutdown();
//     }
//   }
//
//   #[test]
//   fn test_worker_submit_returns_unique_ids() {
//     let (worker, _store) = setup_worker();
//     let mut ids = std::collections::HashSet::new();
//
//     for _ in 0..100 {
//       let op = StoredOp::new_waker(Nop);
//       let id = match worker.submit(op) {
//         Ok(v) => v,
//         Err(_) => panic!(),
//       };
//       assert!(ids.insert(id), "ID {} was duplicated", id);
//     }
//
//     assert_eq!(ids.len(), 100);
//   }
//
//   #[test]
//   fn test_worker_stress_concurrent_operations() {
//     let (worker, _store) = setup_worker();
//     let worker = Arc::new(worker);
//     let num_threads = 16;
//     let ops_per_thread = 100;
//     let total_ops = num_threads * ops_per_thread;
//     let completed = Arc::new(AtomicUsize::new(0));
//
//     let handles: Vec<_> = (0..num_threads)
//       .map(|_| {
//         let worker: Arc<Worker> = Arc::clone(&worker);
//         let completed = Arc::clone(&completed);
//         thread::spawn(move || {
//           for _ in 0..ops_per_thread {
//             let op = StoredOp::new_waker(Nop);
//             if worker.submit(op).is_ok() {
//               completed.fetch_add(1, Ordering::Relaxed);
//             }
//           }
//         })
//       })
//       .collect();
//
//     for handle in handles {
//       handle.join().unwrap();
//     }
//
//     assert_eq!(
//       completed.load(Ordering::Relaxed),
//       total_ops,
//       "All operations should complete"
//     );
//
//     // Give handler time to process
//     thread::sleep(Duration::from_millis(200));
//   }
//
//   #[test]
//   fn test_worker_submit_after_partial_shutdown() {
//     let (mut worker, _store) = setup_worker();
//
//     // Submit some ops
//     for _ in 0..5 {
//       let op = StoredOp::new_waker(Nop);
//       worker.submit(op).unwrap();
//     }
//
//     // Trigger shutdown
//     worker.shutdown();
//
//     // Try to submit after shutdown - should fail
//     let op = StoredOp::new_waker(Nop);
//     let result = worker.submit(op);
//     assert!(result.is_err());
//   }
//
//   #[test]
//   fn test_worker_multiple_shutdowns_idempotent() {
//     let (mut worker, _store) = setup_worker();
//
//     // First shutdown
//     worker.shutdown();
//
//     // Second shutdown should be safe (no-op)
//     worker.shutdown();
//
//     // Third shutdown
//     worker.shutdown();
//   }
//
//   #[test]
//   fn test_worker_empty_worker_lifecycle() {
//     // Create and immediately drop without submitting anything
//     let (worker, _store) = setup_worker();
//     drop(worker);
//   }
//
//   #[test]
//   fn test_worker_handler_processes_operations() {
//     let (worker, store) = setup_worker();
//
//     // Submit operations
//     let ids: Vec<_> = (0..10)
//       .map(|_| {
//         let op = StoredOp::new_waker(Nop);
//         worker.submit(op).unwrap()
//       })
//       .collect();
//
//     // Give handler time to process
//     thread::sleep(Duration::from_millis(100));
//
//     // Check if operations were processed (they should be removed from store or marked complete)
//     for id in ids {
//       let exists = store.get_mut(id, |_reg| {});
//       let _ = exists;
//     }
//   }
//
//   #[test]
//   fn test_worker_burst_traffic() {
//     let (worker, _store) = setup_worker();
//
//     // Submit bursts of operations with pauses
//     for burst in 0..5 {
//       for _ in 0..50 {
//         let op = StoredOp::new_waker(Nop);
//         worker.submit(op).unwrap();
//       }
//
//       println!("Completed burst {}", burst + 1);
//       thread::sleep(Duration::from_millis(20));
//     }
//
//     // Give handler time to drain
//     thread::sleep(Duration::from_millis(100));
//   }
//
//   #[test]
//   fn test_worker_sequential_workers() {
//     // Create multiple workers sequentially
//     for i in 0..5 {
//       let (mut worker, _store) = setup_worker();
//
//       for _ in 0..10 {
//         let op = StoredOp::new_waker(Nop);
//         worker.submit(op).unwrap();
//       }
//
//       worker.shutdown();
//       println!("Worker {} completed", i + 1);
//     }
//   }
//
//   #[test]
//   fn test_worker_channel_communication() {
//     let (worker, _store) = setup_worker();
//
//     // Verify we can submit and the channel doesn't block indefinitely
//     let op = StoredOp::new_waker(Nop);
//     let start = std::time::Instant::now();
//     worker.submit(op).unwrap();
//     let elapsed = start.elapsed();
//
//     // Submit should be fast (not blocking on I/O, just channel send)
//     assert!(
//       elapsed < Duration::from_millis(100),
//       "Submit took too long: {:?}",
//       elapsed
//     );
//   }
//
//   #[test]
//   fn test_worker_memory_safety() {
//     // This test verifies no memory leaks or crashes under heavy load
//     let (worker, _store) = setup_worker();
//
//     for _ in 0..1000 {
//       let op = StoredOp::new_waker(Nop);
//       worker.submit(op).unwrap();
//     }
//
//     thread::sleep(Duration::from_millis(200));
//
//     // If we get here without crashing, memory safety is maintained
//   }
//
//   #[test]
//   fn test_worker_thread_join_timeout() {
//     let (worker, _store) = setup_worker();
//
//     // Submit some operations
//     for _ in 0..10 {
//       let op = StoredOp::new_waker(Nop);
//       worker.submit(op).unwrap();
//     }
//
//     let start = std::time::Instant::now();
//     drop(worker); // Should trigger shutdown
//     let elapsed = start.elapsed();
//
//     // Shutdown should complete reasonably quickly
//     assert!(
//       elapsed < Duration::from_secs(5),
//       "Worker shutdown took too long: {:?}",
//       elapsed
//     );
//   }
//
//   #[test]
//   fn test_worker_concurrent_submit_and_shutdown() {
//     let (worker, _store) = setup_worker();
//     let worker = Arc::new(worker);
//     let worker_submit: Arc<Worker> = Arc::clone(&worker);
//
//     // Spawn thread that submits
//     let handle = thread::spawn(move || {
//       for _ in 0..100 {
//         let op = StoredOp::new_waker(Nop);
//         let _ = worker_submit.submit(op);
//         thread::sleep(Duration::from_micros(100));
//       }
//     });
//
//     // Wait a bit then shutdown
//     thread::sleep(Duration::from_millis(5));
//
//     // Unwrap Arc and shutdown
//     handle.join().unwrap();
//     let mut worker = match Arc::try_unwrap(worker) {
//       Ok(w) => w,
//       Err(_) => panic!("Failed to unwrap Arc"),
//     };
//     worker.shutdown();
//   }
//
//   #[test]
//   fn test_worker_zero_operations() {
//     let (mut worker, _store) = setup_worker();
//     // Shutdown immediately without any operations
//     worker.shutdown();
//   }
//
//   #[test]
//   fn test_worker_single_operation() {
//     let (worker, _store) = setup_worker();
//
//     let op = StoredOp::new_waker(Nop);
//     let result = worker.submit(op);
//
//     assert!(result.is_ok());
//     thread::sleep(Duration::from_millis(50));
//   }
//
//   #[test]
//   fn test_worker_handler_tick_loop() {
//     let (worker, _store) = setup_worker();
//
//     // Submit operations over time to ensure handler's tick loop is running
//     for i in 0..10 {
//       let op = StoredOp::new_waker(Nop);
//       worker.submit(op).unwrap();
//       thread::sleep(Duration::from_millis(10));
//       println!("Submitted operation {}", i + 1);
//     }
//
//     // Give handler time to process all
//     thread::sleep(Duration::from_millis(100));
//   }
//
//   #[test]
//   fn test_worker_submitter_loop_responsiveness() {
//     let (worker, _store) = setup_worker();
//
//     // Submit operations rapidly to test submitter responsiveness
//     let start = std::time::Instant::now();
//
//     for _ in 0..100 {
//       let op = StoredOp::new_waker(Nop);
//       worker.submit(op).unwrap();
//     }
//
//     let elapsed = start.elapsed();
//
//     // Should be able to handle 100 submits very quickly
//     assert!(
//       elapsed < Duration::from_secs(1),
//       "Submitter was too slow: {:?}",
//       elapsed
//     );
//   }
//
//   // #[test]
//   // fn test_worker_with_store_capacity() {
//   //   // Test with different store capacities
//   //   for capacity in [16, 128, 1024] {
//   //     let state = DummyDriver::new_state().unwrap();
//   //     let (submitter_impl, handler_impl) = DummyDriver::new(state).unwrap();
//   //
//   //     let store = Arc::new(OpStore::with_capacity(capacity));
//   //
//   //     let submitter =
//   //       Submitter::new(Box::new(submitter_impl), Arc::clone(&store), state);
//   //     let handler =
//   //       Handler::new(Box::new(handler_impl), Arc::clone(&store), state);
//   //
//   //     let worker = Worker::spawn(submitter, handler);
//   //
//   //     // Submit some operations
//   //     for _ in 0..(capacity / 2) {
//   //       let op = StoredOp::new_waker(Nop);
//   //       worker.submit(op).unwrap();
//   //     }
//   //
//   //     thread::sleep(Duration::from_millis(50));
//   //   }
//   // }
//
//   #[test]
//   fn test_worker_mixed_pace_submissions() {
//     let (worker, _store) = setup_worker();
//
//     // Fast burst
//     for _ in 0..50 {
//       let op = StoredOp::new_waker(Nop);
//       worker.submit(op).unwrap();
//     }
//
//     thread::sleep(Duration::from_millis(100));
//
//     // Slow submissions
//     for _ in 0..10 {
//       let op = StoredOp::new_waker(Nop);
//       worker.submit(op).unwrap();
//       thread::sleep(Duration::from_millis(10));
//     }
//
//     // Another fast burst
//     for _ in 0..50 {
//       let op = StoredOp::new_waker(Nop);
//       worker.submit(op).unwrap();
//     }
//
//     thread::sleep(Duration::from_millis(100));
//   }
//
//   #[test]
//   fn test_worker_resource_cleanup_on_drop() {
//     // Create worker in a scope to ensure it's dropped
//     {
//       let (worker, _store) = setup_worker();
//
//       for _ in 0..20 {
//         let op = StoredOp::new_waker(Nop);
//         worker.submit(op).unwrap();
//       }
//
//       // Worker and store will be dropped here
//     }
//
//     // If we reach here without crash, cleanup was successful
//   }
//
//   #[test]
//   fn test_worker_creation_and_destruction_loop() {
//     // Repeatedly create and destroy workers
//     for _ in 0..10 {
//       let (worker, _store) = setup_worker();
//
//       let op = StoredOp::new_waker(Nop);
//       worker.submit(op).unwrap();
//
//       drop(worker);
//     }
//   }
// }
