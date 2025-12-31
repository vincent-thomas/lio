use std::sync::{Condvar, Mutex};
use std::time::Duration;

/// Provides blocking operations on top of an ArrayQueue without wrapping it.
///
/// This struct only manages the blocking coordination (Condvar + Mutex).
/// The actual queue is owned and managed externally.
///
/// # Design
///
/// - Does NOT own the queue - only coordinates blocking
/// - Mutex holds no data - just used for Condvar coordination
/// - All actual queue operations happen externally
pub struct BlockingCoordinator {
  condvar: Condvar,
  wait_lock: Mutex<()>,
}

impl BlockingCoordinator {
  /// Creates a new blocking coordinator.
  pub fn new() -> Self {
    Self { condvar: Condvar::new(), wait_lock: Mutex::new(()) }
  }

  /// Notifies one waiting thread that an item may be available.
  ///
  /// CRITICAL: This acquires the wait_lock before notifying to prevent lost wakeups.
  /// The lock ensures that if a waiter is between checking the queue and calling
  /// condvar.wait(), the notification won't be lost.
  ///
  /// Call this after pushing to the queue.
  #[inline]
  pub fn notify_one(&self) {
    // Acquire lock before notifying to prevent lost wakeup race.
    // If we notify without the lock, a waiter might check the queue (empty),
    // then we push + notify, then the waiter calls wait() - missing our notification.
    let _guard = self.wait_lock.lock().unwrap();
    self.condvar.notify_one();
    // Lock released here, allowing waiting thread to proceed
  }

  /// Blocks until `try_pop` succeeds in getting a value from the queue.
  ///
  /// # Arguments
  ///
  /// * `try_pop` - Closure that attempts to pop from the queue. Should return Some(T) on success.
  ///
  /// # Example
  ///
  /// ```ignore
  /// let coordinator = BlockingCoordinator::new();
  /// let queue = ArrayQueue::new(10);
  ///
  /// let value = coordinator.blocking_pop(|| queue.pop());
  /// ```
  pub fn blocking_pop<T, F>(&self, try_pop: F) -> T
  where
    F: Fn() -> Option<T>,
  {
    eprintln!("[BlockingCoordinator::blocking_pop] Fast path check");
    // Fast path: try without locking
    if let Some(value) = try_pop() {
      eprintln!("[BlockingCoordinator::blocking_pop] Fast path succeeded");
      return value;
    }

    eprintln!("[BlockingCoordinator::blocking_pop] Fast path failed, acquiring lock");
    // Slow path: wait for notification
    let mut guard = self.wait_lock.lock().unwrap();
    eprintln!("[BlockingCoordinator::blocking_pop] Lock acquired");

    loop {
      eprintln!("[BlockingCoordinator::blocking_pop] Checking queue while holding lock");
      // Check again after acquiring lock to avoid race
      if let Some(value) = try_pop() {
        eprintln!("[BlockingCoordinator::blocking_pop] Found value, returning");
        return value;
      }

      eprintln!("[BlockingCoordinator::blocking_pop] Queue still empty, waiting on condvar");
      // Wait for notification
      guard = self.condvar.wait(guard).unwrap();
      eprintln!("[BlockingCoordinator::blocking_pop] Woke up from condvar.wait()");
      // Loop handles spurious wakeups
    }
  }

  /// Blocks until `try_pop` succeeds or timeout expires.
  ///
  /// Returns None if timeout expires before getting a value.
  ///
  /// # Arguments
  ///
  /// * `try_pop` - Closure that attempts to pop from the queue
  /// * `timeout` - Maximum time to wait
  pub fn blocking_pop_timeout<T, F>(
    &self,
    try_pop: F,
    timeout: Duration,
  ) -> Option<T>
  where
    F: Fn() -> Option<T>,
  {
    // Fast path: try without locking
    if let Some(value) = try_pop() {
      return Some(value);
    }

    // Slow path: wait with timeout
    let mut guard = self.wait_lock.lock().unwrap();
    let start = std::time::Instant::now();

    loop {
      // Check again after acquiring lock
      if let Some(value) = try_pop() {
        return Some(value);
      }

      // Calculate remaining time
      let elapsed = start.elapsed();
      if elapsed >= timeout {
        // Final check before giving up
        return try_pop();
      }

      let remaining = timeout - elapsed;

      // Wait with remaining timeout
      let (new_guard, wait_result) =
        self.condvar.wait_timeout(guard, remaining).unwrap();

      guard = new_guard;

      // If timed out, do final check and return
      if wait_result.timed_out() {
        return try_pop();
      }
      // Otherwise loop to handle spurious wakeups
    }
  }
}

// SAFETY: BlockingCoordinator can be sent across threads
unsafe impl Send for BlockingCoordinator {}

// SAFETY: BlockingCoordinator can be shared across threads
unsafe impl Sync for BlockingCoordinator {}

#[cfg(test)]
mod tests {
  use super::*;
  use crossbeam_queue::ArrayQueue;
  use std::sync::Arc;
  use std::thread;

  #[test]
  fn test_blocking_pop_fast_path() {
    let coordinator = BlockingCoordinator::new();
    let queue = ArrayQueue::new(5);
    queue.push(42).unwrap();

    let start = std::time::Instant::now();
    let value = coordinator.blocking_pop(|| queue.pop());
    let elapsed = start.elapsed();

    assert_eq!(value, 42);
    assert!(elapsed < Duration::from_millis(1));
  }

  #[test]
  fn test_blocking_pop_waits_then_wakes() {
    let coordinator = Arc::new(BlockingCoordinator::new());
    let queue = Arc::new(ArrayQueue::new(5));

    let coordinator_clone = Arc::clone(&coordinator);
    let queue_clone = Arc::clone(&queue);

    let handle = thread::spawn(move || {
      thread::sleep(Duration::from_millis(50));
      queue_clone.push(42).unwrap();
      coordinator_clone.notify_one();
    });

    let start = std::time::Instant::now();
    let value = coordinator.blocking_pop(|| queue.pop());
    let elapsed = start.elapsed();

    assert_eq!(value, 42);
    assert!(elapsed >= Duration::from_millis(50));
    assert!(elapsed < Duration::from_millis(200));

    handle.join().unwrap();
  }

  #[test]
  fn test_blocking_pop_timeout_success() {
    let coordinator = Arc::new(BlockingCoordinator::new());
    let queue = Arc::new(ArrayQueue::new(5));

    let coordinator_clone = Arc::clone(&coordinator);
    let queue_clone = Arc::clone(&queue);

    let handle = thread::spawn(move || {
      thread::sleep(Duration::from_millis(50));
      queue_clone.push(42).unwrap();
      coordinator_clone.notify_one();
    });

    let value = coordinator
      .blocking_pop_timeout(|| queue.pop(), Duration::from_millis(200));
    assert_eq!(value, Some(42));

    handle.join().unwrap();
  }

  #[test]
  fn test_blocking_pop_timeout_expires() {
    let coordinator = BlockingCoordinator::new();
    let queue = ArrayQueue::<i32>::new(5);

    let start = std::time::Instant::now();
    let value = coordinator
      .blocking_pop_timeout(|| queue.pop(), Duration::from_millis(100));
    let elapsed = start.elapsed();

    assert_eq!(value, None);
    assert!(elapsed >= Duration::from_millis(100));
    assert!(elapsed < Duration::from_millis(200));
  }

  #[test]
  fn test_blocking_pop_timeout_fast_path() {
    let coordinator = BlockingCoordinator::new();
    let queue = ArrayQueue::new(5);
    queue.push(42).unwrap();

    let start = std::time::Instant::now();
    let value =
      coordinator.blocking_pop_timeout(|| queue.pop(), Duration::from_secs(1));
    let elapsed = start.elapsed();

    assert_eq!(value, Some(42));
    assert!(elapsed < Duration::from_millis(1));
  }

  #[test]
  fn test_multiple_waiters() {
    let coordinator = Arc::new(BlockingCoordinator::new());
    let queue = Arc::new(ArrayQueue::new(10));
    let num_waiters = 5;

    let handles: Vec<_> = (0..num_waiters)
      .map(|i| {
        let coordinator = Arc::clone(&coordinator);
        let queue = Arc::clone(&queue);
        thread::spawn(move || {
          let value = coordinator.blocking_pop(|| queue.pop());
          (i, value)
        })
      })
      .collect();

    thread::sleep(Duration::from_millis(50));

    // Push values and notify
    for i in 0..num_waiters {
      queue.push(i * 10).unwrap();
      coordinator.notify_one();
      thread::sleep(Duration::from_millis(10));
    }

    let mut results: Vec<_> =
      handles.into_iter().map(|h| h.join().unwrap()).collect();
    results.sort_by_key(|(i, _)| *i);

    assert_eq!(results.len(), num_waiters);

    let mut values: Vec<_> = results.iter().map(|(_, v)| *v).collect();
    values.sort();
    let expected: Vec<_> = (0..num_waiters).map(|i| i * 10).collect();
    assert_eq!(values, expected);
  }

  #[test]
  fn test_concurrent_operations() {
    let coordinator = Arc::new(BlockingCoordinator::new());
    let queue = Arc::new(ArrayQueue::new(50));
    let num_items = 1000;
    let num_producers = 4;
    let num_consumers = 4;

    let producer_handles: Vec<_> = (0..num_producers)
      .map(|producer_id| {
        let coordinator = Arc::clone(&coordinator);
        let queue = Arc::clone(&queue);
        thread::spawn(move || {
          let items_per_producer = num_items / num_producers;
          for i in 0..items_per_producer {
            while queue.push(producer_id * items_per_producer + i).is_err() {
              thread::yield_now();
            }
            coordinator.notify_one();
            if i % 10 == 0 {
              thread::yield_now();
            }
          }
        })
      })
      .collect();

    let consumer_handles: Vec<_> = (0..num_consumers)
      .map(|_| {
        let coordinator = Arc::clone(&coordinator);
        let queue = Arc::clone(&queue);
        thread::spawn(move || {
          let mut consumed = Vec::new();
          let items_per_consumer = num_items / num_consumers;
          for _ in 0..items_per_consumer {
            let value = coordinator.blocking_pop(|| queue.pop());
            consumed.push(value);
          }
          consumed
        })
      })
      .collect();

    for handle in producer_handles {
      handle.join().unwrap();
    }

    let mut all_consumed = Vec::new();
    for handle in consumer_handles {
      all_consumed.extend(handle.join().unwrap());
    }

    all_consumed.sort();
    let expected: Vec<_> = (0..num_items).collect();
    assert_eq!(all_consumed, expected);
  }

  #[test]
  fn test_spurious_wakeup_handling() {
    let coordinator = Arc::new(BlockingCoordinator::new());
    let queue = Arc::new(ArrayQueue::new(5));

    let coordinator_clone = Arc::clone(&coordinator);
    let queue_clone = Arc::clone(&queue);

    let handle = thread::spawn(move || {
      thread::sleep(Duration::from_millis(100));
      queue_clone.push(42).unwrap();
      coordinator_clone.notify_one();
    });

    let value = coordinator
      .blocking_pop_timeout(|| queue.pop(), Duration::from_millis(200));
    assert_eq!(value, Some(42));

    handle.join().unwrap();
  }

  #[test]
  fn test_stress_single_producer_consumer() {
    let coordinator = Arc::new(BlockingCoordinator::new());
    let queue = Arc::new(ArrayQueue::new(10));
    let num_items = 10000;

    let coordinator_clone = Arc::clone(&coordinator);
    let queue_clone = Arc::clone(&queue);

    let producer = thread::spawn(move || {
      for i in 0..num_items {
        while queue_clone.push(i).is_err() {
          thread::yield_now();
        }
        coordinator_clone.notify_one();
      }
    });

    let coordinator_clone = Arc::clone(&coordinator);
    let queue_clone = Arc::clone(&queue);

    let consumer = thread::spawn(move || {
      let mut received = Vec::new();
      for _ in 0..num_items {
        let value = coordinator_clone.blocking_pop(|| queue_clone.pop());
        received.push(value);
      }
      received
    });

    producer.join().unwrap();
    let received = consumer.join().unwrap();

    assert_eq!(received.len(), num_items);
    for (i, &value) in received.iter().enumerate() {
      assert_eq!(value, i);
    }
  }

  #[test]
  fn test_notify_without_waiters() {
    let coordinator = BlockingCoordinator::new();
    // Should not panic or block
    coordinator.notify_one();
    coordinator.notify_one();
    coordinator.notify_one();
  }

  #[test]
  fn test_mixed_blocking_nonblocking() {
    let coordinator = Arc::new(BlockingCoordinator::new());
    let queue = Arc::new(ArrayQueue::new(5));

    let coordinator_clone = Arc::clone(&coordinator);
    let queue_clone = Arc::clone(&queue);

    let handle = thread::spawn(move || {
      let mut results = Vec::new();
      for _ in 0..3 {
        results.push(coordinator_clone.blocking_pop(|| queue_clone.pop()));
      }
      results
    });

    thread::sleep(Duration::from_millis(20));

    // Direct non-blocking access to queue
    assert_eq!(queue.pop(), None);

    // Push items
    queue.push(1).unwrap();
    coordinator.notify_one();
    queue.push(2).unwrap();
    coordinator.notify_one();
    queue.push(3).unwrap();
    coordinator.notify_one();

    let results = handle.join().unwrap();
    assert_eq!(results, vec![1, 2, 3]);
  }

  #[test]
  fn test_high_contention() {
    let coordinator = Arc::new(BlockingCoordinator::new());
    let queue = Arc::new(ArrayQueue::new(10));
    let num_threads = 16;
    let operations_per_thread = 500;

    let handles: Vec<_> = (0..num_threads)
      .map(|thread_id| {
        let coordinator = Arc::clone(&coordinator);
        let queue = Arc::clone(&queue);
        thread::spawn(move || {
          for i in 0..operations_per_thread {
            if i % 2 == 0 {
              while queue.push(thread_id * operations_per_thread + i).is_err() {
                thread::yield_now();
              }
              coordinator.notify_one();
            } else {
              let _ = queue.pop();
            }
          }
        })
      })
      .collect();

    for handle in handles {
      handle.join().unwrap();
    }

    // No panics or deadlocks = success
  }

  #[test]
  fn test_timeout_precision() {
    let coordinator = BlockingCoordinator::new();
    let queue = ArrayQueue::<i32>::new(5);

    let timeouts = [
      Duration::from_millis(10),
      Duration::from_millis(50),
      Duration::from_millis(100),
    ];

    for timeout in timeouts {
      let start = std::time::Instant::now();
      let value = coordinator.blocking_pop_timeout(|| queue.pop(), timeout);
      let elapsed = start.elapsed();

      assert_eq!(value, None);
      assert!(
        elapsed >= timeout,
        "elapsed {:?} should be >= timeout {:?}",
        elapsed,
        timeout
      );
      assert!(
        elapsed < timeout + Duration::from_millis(100),
        "elapsed {:?} should be < timeout + 100ms",
        elapsed
      );
    }
  }
}
