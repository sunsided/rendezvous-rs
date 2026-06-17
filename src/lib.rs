//! # Easier Rendezvous Channels
//!
//! A [`Rendezvous`] lets one thread wait until a group of worker threads have all reached a
//! synchronization point. Each worker holds a [`RendezvousGuard`]; once every guard is dropped,
//! the waiting [`Rendezvous::rendezvous`] call (or its async/timeout variants) proceeds.
//!
//! Internally this is a guard counter protected by a [`Mutex`] and a [`Condvar`] (plus a
//! [`tokio::sync::Notify`] when the `tokio` feature is enabled). The waiter never relinquishes its
//! own handle, so forking a guard always works — even after a timed-out wait — and a timeout never
//! leaves the [`Rendezvous`] in a state where dropping it blocks forever.
//!
//! ## Crate Features
//!
//! * `log` - Enables support for the `log` crate.
//! * `tokio` - Enables the [`Rendezvous::rendezvous_async`] and
//!   [`Rendezvous::rendezvous_timeout_async`] methods to asynchronously wait for the rendezvous
//!   points to be reached.
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

// only enables the `doc_cfg` feature when
// the `docsrs` configuration attribute is defined
#![cfg_attr(docsrs, feature(doc_cfg))]

#[cfg(feature = "log")]
use log::{debug, error, trace};

use std::error::Error;
use std::fmt::{Display, Formatter};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::Duration;

/// State shared between a [`Rendezvous`] and all of its [`RendezvousGuard`]s.
struct Shared {
    /// The number of outstanding [`RendezvousGuard`] instances. The owning [`Rendezvous`] does
    /// *not* count itself; the rendezvous completes once this reaches zero.
    count: Mutex<usize>,
    /// Notifies synchronous waiters when `count` reaches zero.
    cv: Condvar,
    /// Notifies asynchronous waiters when `count` reaches zero.
    #[cfg(feature = "tokio")]
    notify: tokio::sync::Notify,
}

impl Shared {
    /// Locks the guard counter, recovering from poisoning. The counter is a plain `usize` with no
    /// invariant that a panicking guard thread could break, so recovering is always safe and avoids
    /// permanently wedging a waiter.
    fn lock_count(&self) -> MutexGuard<'_, usize> {
        self.count.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// [`Rendezvous`] is a synchronization primitive that allows a thread to wait until a group of
/// worker threads have all reached a certain point in the code before proceeding.
pub struct Rendezvous {
    /// The shared guard counter and notification primitives.
    shared: Arc<Shared>,
    /// Set to `true` once a timeout has been returned. Tells [`Drop`] not to block waiting for
    /// outstanding guards, since the caller already opted into bounded-wait semantics.
    detached: bool,
}

/// A guard forked off a [`Rendezvous`] struct. While it is alive it keeps the owning
/// [`Rendezvous`] from completing; dropping it (or all clones of it) releases the rendezvous.
pub struct RendezvousGuard(Arc<Shared>);

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
        Self {
            shared: Arc::new(Shared {
                count: Mutex::new(0),
                cv: Condvar::new(),
                #[cfg(feature = "tokio")]
                notify: tokio::sync::Notify::new(),
            }),
            detached: false,
        }
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
    /// </div>
    ///
    /// ```no_run
    /// use rendezvous::Rendezvous;
    ///
    /// let rendezvous = Rendezvous::new();
    /// let guard = rendezvous.fork_guard();
    /// rendezvous.rendezvous(); // will deadlock
    /// drop(guard);
    /// ```
    pub fn fork_guard(&self) -> RendezvousGuard {
        #[cfg(feature = "log")]
        {
            trace!("Forking rendezvous guard");
        }
        *self.shared.lock_count() += 1;
        RendezvousGuard(self.shared.clone())
    }

    /// Executes the rendezvous process, blocking until all [`RendezvousGuard`]s are dropped.
    ///
    /// ## Example
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
    /// </div>
    ///
    /// ```no_run
    /// use rendezvous::Rendezvous;
    ///
    /// let rendezvous = Rendezvous::new();
    /// let guard = rendezvous.fork_guard();
    /// rendezvous.rendezvous(); // will deadlock
    /// drop(guard);
    /// ```
    pub fn rendezvous(self) {
        self.wait();
    }

    /// Asynchronously executes the rendezvous process.
    ///
    /// Unlike a blocking wait, this does not occupy a thread: the returned future parks on a
    /// [`tokio::sync::Notify`] and is woken when the last guard is dropped. Dropping the future
    /// cancels the wait cleanly.
    ///
    /// ## Usage notes
    ///
    /// When the rendezvous channel is dropped without a call to [`Rendezvous::rendezvous_async`],
    /// the dropping scope will block until all rendezvous points are reached.
    ///
    /// ## Example
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
    /// # tokio_test::block_on(async {
    /// rendezvous.rendezvous_async().await;
    /// # });
    ///
    /// // The thread finished in time.
    /// assert_eq!(*(value.lock().unwrap()), 42);
    /// ```
    #[cfg(feature = "tokio")]
    #[cfg_attr(docsrs, doc(cfg(feature = "tokio")))]
    pub async fn rendezvous_async(self) {
        self.wait_async().await;
    }

    /// Executes the rendezvous process with a timeout.
    ///
    /// On success the rendezvous is consumed. On a timeout the [`Rendezvous`] is handed back inside
    /// the error so the caller can retry (e.g. with a longer timeout) or fork additional guards.
    /// Dropping the returned [`Rendezvous`] after a timeout does **not** block.
    ///
    /// ## Example
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
    /// // Wait briefly - this will time out and hand the rendezvous back.
    /// let rendezvous = match rendezvous.rendezvous_timeout(Duration::from_millis(10)) {
    ///     Ok(()) => unreachable!("the worker is still sleeping"),
    ///     Err((rendezvous, err)) => {
    ///         assert_eq!(err, RendezvousTimeoutError::Timeout);
    ///         rendezvous
    ///     }
    /// };
    ///
    /// // Block until the thread has finished its work, or the timeout occurs.
    /// assert!(rendezvous.rendezvous_timeout(Duration::from_secs(1)).is_ok());
    ///
    /// // The thread finished in time.
    /// assert_eq!(*(value.lock().unwrap()), 42);
    /// ```
    ///
    /// <div class="warning">
    /// Note that forking and not dropping a guard is generally a deadlock, and a timeout will occur:
    /// </div>
    ///
    /// ```
    /// use std::time::Duration;
    /// use rendezvous::{Rendezvous, RendezvousTimeoutError};
    ///
    /// let rendezvous = Rendezvous::new();
    /// let guard = rendezvous.fork_guard();
    /// let result = rendezvous.rendezvous_timeout(Duration::from_millis(10));
    /// assert!(matches!(result, Err((_, RendezvousTimeoutError::Timeout))));
    /// drop(guard);
    /// ```
    pub fn rendezvous_timeout(
        mut self,
        timeout: Duration,
    ) -> Result<(), (Self, RendezvousTimeoutError)> {
        if self.wait_timeout(timeout) {
            Ok(())
        } else {
            #[cfg(feature = "log")]
            {
                debug!("A timeout occurred during a rendezvous");
            }
            self.detached = true;
            Err((self, RendezvousTimeoutError::Timeout))
        }
    }

    /// Asynchronously executes the rendezvous process with a timeout.
    ///
    /// Behaves like [`Rendezvous::rendezvous_timeout`] but parks on a [`tokio::sync::Notify`]
    /// instead of blocking a thread. On a timeout the [`Rendezvous`] is handed back for retry.
    ///
    /// ## Example
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
    /// let rendezvous = Rendezvous::new();
    /// let value = Arc::new(Mutex::new(0u32));
    ///
    /// thread::spawn({
    ///     let guard = rendezvous.fork_guard();
    ///     let value = value.clone();
    ///     move || slow_worker_fn(guard, value)
    /// });
    ///
    /// # tokio_test::block_on(async {
    /// // Wait briefly - this will time out and hand the rendezvous back.
    /// let rendezvous = match rendezvous.rendezvous_timeout_async(Duration::from_millis(10)).await {
    ///     Ok(()) => unreachable!("the worker is still sleeping"),
    ///     Err((rendezvous, err)) => {
    ///         assert_eq!(err, RendezvousTimeoutError::Timeout);
    ///         rendezvous
    ///     }
    /// };
    ///
    /// // Now wait long enough for the worker to finish.
    /// assert!(rendezvous.rendezvous_timeout_async(Duration::from_secs(1)).await.is_ok());
    /// # });
    ///
    /// assert_eq!(*(value.lock().unwrap()), 42);
    /// ```
    #[cfg(feature = "tokio")]
    #[cfg_attr(docsrs, doc(cfg(feature = "tokio")))]
    pub async fn rendezvous_timeout_async(
        mut self,
        timeout: Duration,
    ) -> Result<(), (Self, RendezvousTimeoutError)> {
        match tokio::time::timeout(timeout, self.wait_async()).await {
            Ok(()) => Ok(()),
            Err(_elapsed) => {
                #[cfg(feature = "log")]
                {
                    debug!("A timeout occurred during a rendezvous");
                }
                self.detached = true;
                Err((self, RendezvousTimeoutError::Timeout))
            }
        }
    }

    /// Blocks until the outstanding guard count reaches zero.
    fn wait(&self) {
        let count = self.shared.lock_count();
        drop(self.shared.cv.wait_while(count, |c| *c > 0));
    }

    /// Blocks until the outstanding guard count reaches zero or the timeout elapses.
    ///
    /// Returns `true` if the count reached zero, `false` on timeout.
    fn wait_timeout(&self, timeout: Duration) -> bool {
        let count = self.shared.lock_count();
        let (count, result) = self
            .shared
            .cv
            .wait_timeout_while(count, timeout, |c| *c > 0)
            .unwrap_or_else(|e| e.into_inner());
        let _ = result;
        *count == 0
    }

    /// Asynchronously waits until the outstanding guard count reaches zero.
    ///
    /// The [`tokio::sync::Notify`] future is created *before* the count is checked so a guard
    /// dropping between the check and the await cannot be missed.
    #[cfg(feature = "tokio")]
    async fn wait_async(&self) {
        loop {
            let notified = self.shared.notify.notified();
            if *self.shared.lock_count() == 0 {
                return;
            }
            notified.await;
        }
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
        *self.0.lock_count() += 1;
        RendezvousGuard(self.0.clone())
    }

    /// A no-operation that consumes self, marking a rendezvous point.
    ///
    /// ## Example
    ///
    /// ```
    /// use rendezvous::Rendezvous;
    ///
    /// let rendezvous = Rendezvous::new();
    /// let guard = rendezvous.fork_guard();
    /// guard.completed();
    /// rendezvous.rendezvous();
    /// ```
    pub fn completed(self) {}
}

impl Clone for RendezvousGuard {
    fn clone(&self) -> Self {
        self.fork()
    }
}

impl Drop for RendezvousGuard {
    fn drop(&mut self) {
        let mut count = self.0.lock_count();
        *count -= 1;
        if *count == 0 {
            // Decrement and notify happen under the lock, so a waiter cannot miss the wakeup.
            self.0.cv.notify_all();
            #[cfg(feature = "tokio")]
            self.0.notify.notify_waiters();
        }
    }
}

impl Drop for Rendezvous {
    fn drop(&mut self) {
        // After a timeout the caller already accepted a bounded wait; do not block.
        if self.detached {
            return;
        }

        #[cfg(all(debug_assertions, feature = "log"))]
        if *self.shared.lock_count() > 0 {
            error!("Implementation error: Rendezvous method not invoked")
        }

        // Acts as an implicit guard: block until all rendezvous points are reached.
        self.wait();
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
    use std::time::Instant;

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
        let rendezvous = Rendezvous::new();
        let guard = rendezvous.fork_guard();

        let result = rendezvous.rendezvous_timeout(Duration::from_millis(100));
        assert!(matches!(result, Err((_, RendezvousTimeoutError::Timeout))));
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

    #[test]
    fn fork_after_timeout_does_not_panic() {
        let rendezvous = Rendezvous::new();
        let first = rendezvous.fork_guard();

        // First attempt times out and hands the rendezvous back.
        let rendezvous = match rendezvous.rendezvous_timeout(Duration::from_millis(10)) {
            Ok(()) => panic!("guard still held, should have timed out"),
            Err((rendezvous, err)) => {
                assert_eq!(err, RendezvousTimeoutError::Timeout);
                rendezvous
            }
        };

        // Forking after a timeout must not panic.
        let second = rendezvous.fork_guard();

        // Drop both guards on a background thread, then wait again with enough time.
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(50));
            drop(first);
            drop(second);
        });

        assert!(rendezvous
            .rendezvous_timeout(Duration::from_secs(1))
            .is_ok());
    }

    #[test]
    fn drop_after_timeout_is_bounded() {
        let rendezvous = Rendezvous::new();
        let guard = rendezvous.fork_guard();

        // Hold the guard alive far longer than the test should take.
        let handle = thread::spawn(move || {
            let _guard = guard;
            thread::sleep(Duration::from_secs(5))
        });

        let rendezvous = match rendezvous.rendezvous_timeout(Duration::from_millis(10)) {
            Ok(()) => panic!("guard still held, should have timed out"),
            Err((rendezvous, _)) => rendezvous,
        };

        // Dropping after a timeout must return promptly, not block on the outstanding guard.
        let start = Instant::now();
        drop(rendezvous);
        assert!(
            start.elapsed() < Duration::from_secs(1),
            "drop after timeout blocked"
        );

        // Let the worker thread finish so we do not leak it past the test.
        handle.join().unwrap();
    }

    #[cfg(feature = "tokio")]
    #[test]
    fn async_rendezvous_completes() {
        tokio_test::block_on(async {
            let rendezvous = Rendezvous::new();
            let guard = rendezvous.fork_guard();
            thread::spawn(move || {
                let _guard = guard;
                thread::sleep(Duration::from_millis(100))
            });
            rendezvous.rendezvous_async().await;
        });
    }

    #[cfg(feature = "tokio")]
    #[test]
    fn async_timeout_returns_rendezvous() {
        tokio_test::block_on(async {
            let rendezvous = Rendezvous::new();
            let guard = rendezvous.fork_guard();

            let rendezvous = match rendezvous
                .rendezvous_timeout_async(Duration::from_millis(10))
                .await
            {
                Ok(()) => panic!("guard still held, should have timed out"),
                Err((rendezvous, err)) => {
                    assert_eq!(err, RendezvousTimeoutError::Timeout);
                    rendezvous
                }
            };

            drop(guard);
            assert!(rendezvous
                .rendezvous_timeout_async(Duration::from_secs(1))
                .await
                .is_ok());
        });
    }
}
