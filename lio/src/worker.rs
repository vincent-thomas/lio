//! Worker threads for managing I/O submission and completion.
//!
//! The worker system consists of two threads:
//! - **Submitter thread**: Receives operations via channel, submits them to the backend
//! - **Handler thread**: Polls for completions, processes results, invokes callbacks/wakers

use crate::backends::{Handler, SubmitErr, Submitter};
use crate::registration::StoredOp;
use std::sync::mpsc::{self, Receiver, Sender, SyncSender};
use std::thread::{self, JoinHandle};

/// Message sent to the submitter thread
pub enum SubmitMsg {
  Submit { op: StoredOp, response_tx: SyncSender<Result<u64, SubmitErr>> },
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
  pub fn spawn(mut submitter: Submitter, mut handler: Handler) -> Self {
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

  /// Submit an operation (blocks until submitter thread processes it and returns ID)
  pub fn submit(&self, op: StoredOp) -> Result<u64, SubmitErr> {
    let (response_tx, response_rx) = mpsc::sync_channel(1);

    self
      .submit_tx
      .send(SubmitMsg::Submit { op, response_tx })
      .map_err(|_| SubmitErr::DriverShutdown)?;

    // Wait for submitter thread to process and respond
    response_rx.recv().map_err(|_| SubmitErr::DriverShutdown)?
  }

  /// Submitter thread loop
  fn submitter_loop(rx: Receiver<SubmitMsg>, submitter: &mut Submitter) {
    loop {
      match rx.recv() {
        Ok(SubmitMsg::Submit { op, response_tx }) => {
          // Submit to backend and send result back
          let result = submitter.submit(op);
          let _ = response_tx.send(result); // Ignore if receiver dropped
        }
        Ok(SubmitMsg::Shutdown) | Err(_) => {
          let _ = submitter.notify();
          // Channel closed or shutdown requested
          break;
        }
      }
    }
  }

  /// Handler thread loop
  fn handler_loop(shutdown_rx: Receiver<()>, handler: &mut Handler) {
    loop {
      // Check for shutdown signal (non-blocking)
      match shutdown_rx.try_recv() {
        Ok(_) => break, // Shutdown requested
        Err(mpsc::TryRecvError::Disconnected) => break, // Channel closed
        Err(mpsc::TryRecvError::Empty) => {} // No shutdown yet, continue
      }

      // Poll for completions
      match handler.tick() {
        Ok(()) => {}
        Err(e) => {
          panic!("lio: handler tick error: {:?}", e);
          // Consider: should we break on error or continue?
          std::thread::sleep(std::time::Duration::from_millis(10));
        }
      }
    }
  }

  /// Shutdown the worker threads gracefully
  pub fn shutdown(&mut self) {
    // Signal submitter to shutdown
    let _ = self.submit_tx.send(SubmitMsg::Shutdown);

    // Signal handler to shutdown
    if let Some(tx) = self.shutdown_tx.take() {
      let _ = tx.send(());
    }

    // Wait for threads to finish
    if let Some(handle) = self.submitter_thread.take() {
      let _ = handle.join();
    }

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
