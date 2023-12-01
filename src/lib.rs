//! # Easier Rendezvous Channels
//!
//! In rust, [mpsc::channel](https://doc.rust-lang.org/std/sync/mpsc/fn.channel.html) can be used as a synchronization
//! primitive between threads by utilizing the fact that we can block on the receiver's `recv()` function until all senders
//! are dropped.
//!
//! This crate aims at giving the concept an expressive name and at reducing some classes of race conditions, namely those
//! where the original sender was not dropped before the call to `recv()`.
//!
//! This version of the crate only supports synchronous code due to the dropping semantics.
//!
//! ## Example usage
//!
//! ```rust
//! use std::sync::{Arc, Mutex};
//! use std::thread;
//! use std::time::Duration;
//! use rendezvous::{Rendezvous, RendezvousGuard};
//!
//! /// A slow worker function. Sleeps, then mutates a value.
//! fn slow_worker_fn(_guard: RendezvousGuard, mut value: Arc<Mutex<u32>>) {
//!     thread::sleep(Duration::from_millis(400));
//!     let mut value = value.lock().unwrap();
//!     *value = 42;
//! }
//!
//! fn example() {
//!     // The guard that ensures synchronization across threads.
//!     // Rendezvous itself acts as a guard: If not explicitly dropped, it will block the current
//!     // scope until all rendezvous points are reached.
//!     let rendezvous = Rendezvous::new();
//!
//!     // A value to mutate in a different thread.
//!     let value = Arc::new(Mutex::new(0u32));
//!
//!     // Run the worker in a thread.
//!     thread::spawn({
//!         let guard = rendezvous.fork_guard();
//!         let value = value.clone();
//!         move || slow_worker_fn(guard, value)
//!     });
//!
//!     // Block until the thread has finished its work.
//!     rendezvous.rendezvous();
//!
//!     // The thread finished in time.
//!     assert_eq!(*(value.lock().unwrap()), 42);
//! }
//! ```

#[cfg(feature = "log")]
use log::{debug, error, trace};

use std::error::Error;
use std::fmt::{Display, Formatter};
use std::sync::mpsc;
use std::sync::mpsc::RecvTimeoutError;
use std::time::Duration;

/// [`Rendezvous`] is a synchronization primitive that allows two threads to rendezvous
/// at a certain point in the code before proceeding.
pub struct Rendezvous {
    /// The receiver used for the rendezvous process. If all senders are dropped, the
    /// receiver allows the [`Rendezvous::rendezvous`] method to pass.
    rx: mpsc::Receiver<()>,
    /// The original sender for the rendezvous process. Will be forked using [`Rendezvous::fork_guard`]
    /// or transiently forked from [`RendezvousGuard::fork`]. If all senders are dropped,
    /// [`Rendezvous::rendezvous`] can proceed.
    tx: Option<mpsc::Sender<()>>,
}

/// A guard forked off a [`Rendezvous`] struct.
pub struct RendezvousGuard(mpsc::Sender<()>);

impl Rendezvous {
    /// Create a new instance of a [`Rendezvous`] channel.
    ///
    /// # Returns
    ///
    /// The newly created rendezvous channel.
    ///
    /// # Examples
    ///
    /// ```
    /// use rendezvous::Rendezvous;
    ///
    /// let rendezvous = Rendezvous::new();
    /// ```
    pub fn new() -> Self {
        let (tx, rx) = mpsc::channel();
        Self { tx: Some(tx), rx }
    }

    /// Forks a guard off the [`Rendezvous`] channel.
    ///
    /// When all guards are dropped, [`Rendezvous::rendezvous`] will proceed; until then, that
    /// call blocks.
    ///
    /// ## Example
    ///
    /// See [`Rendezvous::new`] for a usage example.
    ///
    /// <div class="warning">
    /// Note that forking and not dropping a guard in the same thread is a deadlock:
    ///
    /// ```no_run
    /// use rendezvous::Rendezvous;
    ///
    /// let mut rendezvous = Rendezvous::new();
    /// let guard = rendezvous.fork_guard();
    /// rendezvous.rendezvous(); // will deadlock
    /// drop(guard);
    /// ```
    /// </div>
    pub fn fork_guard(&self) -> RendezvousGuard {
        if let Some(tx) = &self.tx {
            #[cfg(feature = "log")]
            {
                trace!("Forking rendezvous guard");
            }
            RendezvousGuard(tx.clone())
        } else {
            unreachable!("Fork called after Rendezvous is dropped")
        }
    }

    /// Executes the rendezvous process.
    ///
    /// # Example
    ///
    /// ```
    /// use std::sync::{Arc, Mutex};
    /// use std::thread;
    /// use std::time::Duration;
    /// use rendezvous::{Rendezvous, RendezvousGuard};
    ///
    /// // A slow worker function. Sleeps, then mutates a value.
    /// fn slow_worker_fn(_guard: RendezvousGuard, mut value: Arc<Mutex<u32>>) {
    ///     thread::sleep(Duration::from_millis(400));
    ///     let mut value = value.lock().unwrap();
    ///     *value = 42;
    /// }
    ///
    /// // The guard that ensures synchronization across threads.
    /// let rendezvous = Rendezvous::new();
    ///
    /// // A value to mutate in a different thread.
    /// let value = Arc::new(Mutex::new(0u32));
    ///
    /// // Run the worker in a thread.
    /// thread::spawn({
    ///     let guard = rendezvous.fork_guard();
    ///     let value = value.clone();
    ///     move || slow_worker_fn(guard, value)
    /// });
    ///
    /// // Block until the thread has finished its work.
    /// rendezvous.rendezvous();
    ///
    /// // The thread finished in time.
    /// assert_eq!(*(value.lock().unwrap()), 42);
    /// ```
    ///
    /// <div class="warning">
    /// Note that forking and not dropping a guard in the same thread is a deadlock:
    ///
    /// ```no_run
    /// use rendezvous::Rendezvous;
    ///
    /// let mut rendezvous = Rendezvous::new();
    /// let guard = rendezvous.fork_guard();
    /// rendezvous.rendezvous(); // will deadlock
    /// drop(guard);
    /// ```
    /// </div>
    pub fn rendezvous(mut self) {
        self.rendezvous_internal();
    }

    /// Executes the rendezvous process with a timeout..
    ///
    /// # Example
    ///
    /// ```
    /// use std::sync::{Arc, Mutex};
    /// use std::thread;
    /// use std::time::Duration;
    /// use rendezvous::{Rendezvous, RendezvousGuard, RendezvousTimeoutError};
    ///
    /// // A slow worker function. Sleeps, then mutates a value.
    /// fn slow_worker_fn(_guard: RendezvousGuard, mut value: Arc<Mutex<u32>>) {
    ///     thread::sleep(Duration::from_millis(400));
    ///     let mut value = value.lock().unwrap();
    ///     *value = 42;
    /// }
    ///
    /// // The guard that ensures synchronization across threads.
    /// let mut rendezvous = Rendezvous::new();
    ///
    /// // A value to mutate in a different thread.
    /// let value = Arc::new(Mutex::new(0u32));
    ///
    /// // Run the worker in a thread.
    /// thread::spawn({
    ///     let guard = rendezvous.fork_guard();
    ///     let value = value.clone();
    ///     move || slow_worker_fn(guard, value)
    /// });
    ///
    /// // Wait briefly - this will fail.
    /// let result = rendezvous.rendezvous_timeout(Duration::from_millis(10));
    /// assert_eq!(result, Err(RendezvousTimeoutError::Timeout));
    ///
    /// // Block until the thread has finished its work, or the timeout occurs.
    /// let result = rendezvous.rendezvous_timeout(Duration::from_secs(1));
    /// assert_eq!(result, Ok(()));
    ///
    /// // The thread finished in time.
    /// assert_eq!(*(value.lock().unwrap()), 42);
    /// ```
    ///
    /// <div class="warning">
    /// Note that forking and not dropping a guard is generally a deadlock, and a timeout will occur:
    ///
    /// ```
    /// use std::time::Duration;
    /// use rendezvous::{Rendezvous, RendezvousTimeoutError};
    ///
    /// let mut rendezvous = Rendezvous::new();
    /// let guard = rendezvous.fork_guard();
    /// assert_eq!(rendezvous.rendezvous_timeout(Duration::from_millis(10)), Err(RendezvousTimeoutError::Timeout));
    /// drop(guard);
    /// ```
    /// </div>
    pub fn rendezvous_timeout(&mut self, timeout: Duration) -> Result<(), RendezvousTimeoutError> {
        if let Some(tx) = self.tx.take() {
            drop(tx);
        } else {
            #[cfg(feature = "log")]
            {
                trace!("Rendezvous was called previously, attempting again");
            }
        }
        match self.rx.recv_timeout(timeout) {
            Ok(_) => Ok(()),
            Err(err) => match err {
                RecvTimeoutError::Timeout => {
                    #[cfg(feature = "log")]
                    {
                        debug!("A timeout occurred during a rendezvous");
                    }
                    Err(RendezvousTimeoutError::Timeout)
                }
                RecvTimeoutError::Disconnected => Ok(()),
            },
        }
    }

    /// Performs a rendezvous operation internally.
    ///
    /// This function borrows `self` and drops the `tx` channel if it exists.
    /// It then blocks on the `rx` channel, waiting for all [`RendezvousGuard`] instances to be
    /// dropped, and discards any error that may occur.
    fn rendezvous_internal(&mut self) {
        if let Some(tx) = self.tx.take() {
            drop(tx);
        }
        self.rx.recv().ok();
    }
}

impl Default for Rendezvous {
    fn default() -> Self {
        Rendezvous::new()
    }
}

impl RendezvousGuard {
    /// Forks a guard off the owning [`Rendezvous`] channel.
    ///
    /// When all guards are dropped, [`Rendezvous::rendezvous`] will proceed; until then, that
    /// call blocks.
    pub fn fork(&self) -> RendezvousGuard {
        #[cfg(feature = "log")]
        {
            trace!("Forking nested rendezvous guard");
        }
        RendezvousGuard(self.0.clone())
    }

    /// A no-operation that consumes self, marking a rendezvous point.
    ///
    /// ## Example
    ///
    /// ```
    /// use rendezvous::Rendezvous;
    ///
    /// let mut rendezvous = Rendezvous::new();
    /// let guard = rendezvous.fork_guard();
    /// guard.completed();
    /// rendezvous.rendezvous();
    /// ```
    pub fn completed(self) {}
}

impl Drop for Rendezvous {
    fn drop(&mut self) {
        #[cfg(feature = "log")]
        if self.tx.is_some() {
            error!("Implementation error: Rendezvous method not invoked")
        }
        self.rendezvous_internal()
    }
}

/// Timeout error that may occur during a rendezvous process.
///
/// This error is used to indicate that a timeout has occurred while waiting for a rendezvous.
#[derive(Debug, Eq, PartialEq)]
pub enum RendezvousTimeoutError {
    /// A timeout occurred that may occur during a rendezvous process. Forks have not disconnected
    /// yet, so the work might not have been completed.
    Timeout,
}

impl Display for RendezvousTimeoutError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            RendezvousTimeoutError::Timeout => write!(f, "Timeout"),
        }
    }
}

impl Error for RendezvousTimeoutError {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

    #[test]
    fn rendezvous_can_pass_away() {
        let rendezvous = Rendezvous::new();
        rendezvous.rendezvous();
    }

    #[test]
    fn rendezvous_can_be_dropped_right_away() {
        let rendezvous = Rendezvous::new();
        drop(rendezvous);
    }

    #[test]
    fn test_timeout() {
        let mut rendezvous = Rendezvous::new();
        let guard = rendezvous.fork_guard();

        let result = rendezvous.rendezvous_timeout(Duration::from_millis(100));
        assert_eq!(result, Err(RendezvousTimeoutError::Timeout));
        drop(guard);
    }

    #[test]
    fn test_background_forks() {
        let rendezvous = Rendezvous::new();

        let guard = rendezvous.fork_guard();
        thread::spawn(move || {
            let _guard = guard;
            thread::sleep(Duration::from_millis(400))
        });

        rendezvous.rendezvous();
    }
}
