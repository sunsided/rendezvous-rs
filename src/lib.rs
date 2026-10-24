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
//! ## Drop semantics
//!
//! A [`Rendezvous`] that is dropped without being waited on acts as an implicit rendezvous: the
//! destructor **blocks** until every outstanding guard is released. This is convenient in
//! synchronous code but a hazard in async code, where it parks an executor thread. To opt out:
//!
//! * use [`Rendezvous::rendezvous_timeout`] (or its async variant) — a rendezvous handed back on
//!   timeout is detached and drops without blocking;
//! * use [`Rendezvous::rendezvous_async`] (or `.await` the rendezvous directly) — the rendezvous
//!   is detached as soon as the future is created, so cancelling the future never blocks;
//! * call [`Rendezvous::detach`] to give up on the outstanding guards explicitly, e.g. on an
//!   early-return error path.
//!
//! ## Crate Features
//!
//! * `log` - Emits diagnostics through the `log` crate.
//! * `tracing` - Emits diagnostics through the `tracing` crate (takes precedence over `log`).
//! * `tokio` - Enables the [`Rendezvous::rendezvous_async`] and
//!   [`Rendezvous::rendezvous_timeout_async`] methods (and `.await` on a [`Rendezvous`]) to
//!   asynchronously wait for the rendezvous points to be reached.
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
//! fn slow_worker_fn(_guard: RendezvousGuard, value: Arc<Mutex<u32>>) {
//!     thread::sleep(Duration::from_millis(400));
//!     let mut value = value.lock().unwrap();
//!     *value = 42;
//! }
//!
//! // The guard that ensures synchronization across threads.
//! // Rendezvous itself acts as a guard: If not explicitly dropped, it will block the current
//! // scope until all rendezvous points are reached.
//! let rendezvous = Rendezvous::new();
//!
//! // A value to mutate in a different thread.
//! let value = Arc::new(Mutex::new(0u32));
//!
//! // Run the worker in a thread.
//! thread::spawn({
//!     let guard = rendezvous.fork_guard();
//!     let value = value.clone();
//!     move || slow_worker_fn(guard, value)
//! });
//!
//! // Block until the thread has finished its work.
//! rendezvous.rendezvous();
//!
//! // The thread finished in time.
//! assert_eq!(*(value.lock().unwrap()), 42);
//! ```

// only enables the `doc_cfg` feature when
// the `docsrs` configuration attribute is defined
#![cfg_attr(docsrs, feature(doc_cfg))]

use std::error::Error;
use std::fmt::{Debug, Display, Formatter};

// Under `--cfg loom` the synchronization primitives are swapped for loom's instrumented versions
// so the model checker can explore every thread interleaving of the sync core.
#[cfg(loom)]
use loom::sync::{Arc, Condvar, Mutex, MutexGuard};
#[cfg(not(loom))]
use std::sync::{Arc, Condvar, Mutex, MutexGuard};

// `Duration` is only needed by the timeout APIs, which are excluded from loom builds (loom does not
// model time).
#[cfg(any(not(loom), feature = "tokio"))]
use std::time::Duration;

/// Emits a diagnostic through `tracing` if enabled, falling back to `log`; a no-op otherwise.
macro_rules! emit {
    ($level:ident, $($arg:tt)+) => {{
        #[cfg(feature = "tracing")]
        tracing::$level!($($arg)+);
        #[cfg(all(feature = "log", not(feature = "tracing")))]
        log::$level!($($arg)+);
    }};
}

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
///
/// See the [crate-level documentation](crate#drop-semantics) for what happens when it is dropped.
pub struct Rendezvous {
    /// The shared guard counter and notification primitives.
    shared: Arc<Shared>,
    /// Set to `true` once the caller opted out of an implicit rendezvous (timeout, async wait or
    /// [`Rendezvous::detach`]). Tells [`Drop`] not to block waiting for outstanding guards.
    detached: bool,
}

/// A guard forked off a [`Rendezvous`] struct. While it is alive it keeps the owning
/// [`Rendezvous`] from completing; dropping it (or all clones of it) releases the rendezvous.
#[must_use = "a guard that is dropped immediately does not hold the rendezvous; \
              bind it or move it into the worker"]
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
    #[must_use]
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
        emit!(trace, "Forking rendezvous guard");
        *self.shared.lock_count() += 1;
        RendezvousGuard(self.shared.clone())
    }

    /// Returns the number of guards that have not been released yet.
    ///
    /// The value is a snapshot; guards on other threads may be forked or dropped concurrently.
    #[must_use]
    pub fn outstanding(&self) -> usize {
        *self.shared.lock_count()
    }

    /// Gives up on the rendezvous without waiting for the outstanding guards.
    ///
    /// Unlike a plain [`drop`], this never blocks. The guards stay valid and can still be forked
    /// and dropped; nobody waits for them anymore. Use this on early-return paths (e.g. a startup
    /// error) where the workers holding guards will never be told to finish.
    ///
    /// ## Example
    ///
    /// ```
    /// use rendezvous::Rendezvous;
    ///
    /// let rendezvous = Rendezvous::new();
    /// let guard = rendezvous.fork_guard();
    /// rendezvous.detach(); // returns immediately despite the outstanding guard
    /// drop(guard);
    /// ```
    pub fn detach(mut self) {
        emit!(debug, "Detaching rendezvous");
        self.detached = true;
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
    /// fn slow_worker_fn(_guard: RendezvousGuard, value: Arc<Mutex<u32>>) {
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
    /// [`tokio::sync::Notify`] and is woken when the last guard is dropped.
    ///
    /// The rendezvous is detached as soon as this method is called, so dropping the returned
    /// future — before or during the wait — cancels the wait without blocking.
    ///
    /// Awaiting the [`Rendezvous`] directly (`rendezvous.await`) is equivalent.
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
    /// fn slow_worker_fn(_guard: RendezvousGuard, value: Arc<Mutex<u32>>) {
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
    pub fn rendezvous_async(mut self) -> impl Future<Output = ()> + Send {
        // Detach eagerly (not inside the future) so that a future dropped before its first poll
        // does not fall back to the blocking destructor either.
        self.detached = true;
        async move { self.wait_async().await }
    }

    /// Executes the rendezvous process with a timeout.
    ///
    /// On success the rendezvous is consumed. On a timeout the [`Rendezvous`] is handed back inside
    /// the error (see [`RendezvousTimeoutError::into_rendezvous`]) so the caller can retry (e.g.
    /// with a longer timeout) or fork additional guards. The returned [`Rendezvous`] is detached:
    /// dropping it, or the error holding it, does **not** block.
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
    /// fn slow_worker_fn(_guard: RendezvousGuard, value: Arc<Mutex<u32>>) {
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
    /// let err = rendezvous
    ///     .rendezvous_timeout(Duration::from_millis(10))
    ///     .expect_err("the worker is still sleeping");
    /// assert_eq!(err.outstanding(), 1);
    /// let rendezvous = err.into_rendezvous();
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
    /// use rendezvous::Rendezvous;
    ///
    /// let rendezvous = Rendezvous::new();
    /// let guard = rendezvous.fork_guard();
    /// let result = rendezvous.rendezvous_timeout(Duration::from_millis(10));
    /// assert!(result.is_err());
    /// drop(guard);
    /// ```
    #[cfg(not(loom))]
    pub fn rendezvous_timeout(mut self, timeout: Duration) -> Result<(), RendezvousTimeoutError> {
        match self.wait_timeout(timeout) {
            Ok(()) => Ok(()),
            Err(outstanding) => {
                self.detached = true;
                Err(RendezvousTimeoutError::new(self, outstanding))
            }
        }
    }

    /// Asynchronously executes the rendezvous process with a timeout.
    ///
    /// Behaves like [`Rendezvous::rendezvous_timeout`] but parks on a [`tokio::sync::Notify`]
    /// instead of blocking a thread. On a timeout the [`Rendezvous`] is handed back for retry.
    ///
    /// Like [`Rendezvous::rendezvous_async`], the rendezvous is detached as soon as this method is
    /// called, so dropping the returned future cancels the wait without blocking.
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
    /// fn slow_worker_fn(_guard: RendezvousGuard, value: Arc<Mutex<u32>>) {
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
    /// let rendezvous = rendezvous
    ///     .rendezvous_timeout_async(Duration::from_millis(10))
    ///     .await
    ///     .expect_err("the worker is still sleeping")
    ///     .into_rendezvous();
    ///
    /// // Now wait long enough for the worker to finish.
    /// assert!(rendezvous.rendezvous_timeout_async(Duration::from_secs(1)).await.is_ok());
    /// # });
    ///
    /// assert_eq!(*(value.lock().unwrap()), 42);
    /// ```
    #[cfg(feature = "tokio")]
    pub fn rendezvous_timeout_async(
        mut self,
        timeout: Duration,
    ) -> impl Future<Output = Result<(), RendezvousTimeoutError>> + Send {
        // See `rendezvous_async`: detach before the future exists so cancellation never blocks.
        self.detached = true;
        async move {
            match tokio::time::timeout(timeout, self.wait_async()).await {
                Ok(()) => Ok(()),
                Err(_elapsed) => {
                    let outstanding = self.outstanding();
                    Err(RendezvousTimeoutError::new(self, outstanding))
                }
            }
        }
    }

    /// Blocks until the outstanding guard count reaches zero.
    ///
    /// Uses a manual predicate loop (rather than `Condvar::wait_while`) because loom's `Condvar`
    /// only exposes `wait`; the loop re-checks the count to absorb spurious wakeups.
    fn wait(&self) {
        let mut count = self.shared.lock_count();
        while *count > 0 {
            count = self
                .shared
                .cv
                .wait(count)
                .unwrap_or_else(|e| e.into_inner());
        }
    }

    /// Blocks until the outstanding guard count reaches zero or the timeout elapses.
    ///
    /// Returns the number of outstanding guards on timeout.
    #[cfg(not(loom))]
    fn wait_timeout(&self, timeout: Duration) -> Result<(), usize> {
        let count = self.shared.lock_count();
        let (count, _) = self
            .shared
            .cv
            .wait_timeout_while(count, timeout, |c| *c > 0)
            .unwrap_or_else(|e| e.into_inner());
        match *count {
            0 => Ok(()),
            outstanding => Err(outstanding),
        }
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

impl Debug for Rendezvous {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Rendezvous")
            .field("outstanding", &self.outstanding())
            .field("detached", &self.detached)
            .finish()
    }
}

/// Awaiting a [`Rendezvous`] is shorthand for [`Rendezvous::rendezvous_async`].
///
/// ```
/// use rendezvous::Rendezvous;
///
/// # tokio_test::block_on(async {
/// let rendezvous = Rendezvous::new();
/// let guard = rendezvous.fork_guard();
/// std::thread::spawn(move || drop(guard));
/// rendezvous.await;
/// # });
/// ```
#[cfg(feature = "tokio")]
impl IntoFuture for Rendezvous {
    type Output = ();
    type IntoFuture = std::pin::Pin<Box<dyn Future<Output = ()> + Send>>;

    fn into_future(self) -> Self::IntoFuture {
        Box::pin(self.rendezvous_async())
    }
}

impl RendezvousGuard {
    /// Creates a guard that is not attached to any waiting [`Rendezvous`].
    ///
    /// Useful where an API requires a guard but no shutdown coordination is needed (e.g. in
    /// tests), instead of threading an `Option<RendezvousGuard>` through. Forks of a detached
    /// guard are detached as well.
    ///
    /// ## Example
    ///
    /// ```
    /// use rendezvous::RendezvousGuard;
    ///
    /// let guard = RendezvousGuard::detached();
    /// let fork = guard.fork();
    /// drop(guard);
    /// drop(fork);
    /// ```
    pub fn detached() -> Self {
        let rendezvous = Rendezvous::new();
        let guard = rendezvous.fork_guard();
        rendezvous.detach();
        guard
    }

    /// Forks a guard off the owning [`Rendezvous`] channel.
    ///
    /// When all guards are dropped, [`Rendezvous::rendezvous`] will proceed; until then, that
    /// call blocks.
    pub fn fork(&self) -> RendezvousGuard {
        emit!(trace, "Forking nested rendezvous guard");
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

impl Debug for RendezvousGuard {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RendezvousGuard")
            .field("outstanding", &*self.0.lock_count())
            .finish()
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
        // The caller opted out of the implicit rendezvous; do not block.
        if self.detached {
            return;
        }

        let outstanding = self.outstanding();
        if outstanding > 0 {
            emit!(
                warn,
                "Rendezvous dropped with {} outstanding guard(s); blocking until they are released",
                outstanding
            );
        }

        // Acts as an implicit guard: block until all rendezvous points are reached.
        self.wait();
    }
}

/// A rendezvous timed out while guards were still outstanding.
///
/// Carries the (detached) [`Rendezvous`] so the caller can retry; dropping the error does not
/// block.
pub struct RendezvousTimeoutError {
    rendezvous: Rendezvous,
    outstanding: usize,
}

impl RendezvousTimeoutError {
    // Only the timeout APIs construct this, and loom builds exclude the synchronous one.
    #[cfg(any(not(loom), feature = "tokio"))]
    fn new(rendezvous: Rendezvous, outstanding: usize) -> Self {
        emit!(
            debug,
            "Rendezvous timed out with {} outstanding guard(s)",
            outstanding
        );
        Self {
            rendezvous,
            outstanding,
        }
    }

    /// The number of guards that were still outstanding when the timeout occurred.
    #[must_use]
    pub fn outstanding(&self) -> usize {
        self.outstanding
    }

    /// Recovers the [`Rendezvous`] to retry the wait or fork additional guards.
    ///
    /// The rendezvous stays detached: dropping it does not block.
    #[must_use]
    pub fn into_rendezvous(self) -> Rendezvous {
        self.rendezvous
    }
}

impl Debug for RendezvousTimeoutError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RendezvousTimeoutError")
            .field("outstanding", &self.outstanding)
            .finish_non_exhaustive()
    }
}

impl Display for RendezvousTimeoutError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "rendezvous timed out with {} outstanding guard(s)",
            self.outstanding
        )
    }
}

impl Error for RendezvousTimeoutError {}

/// Compile-fail checks for `#[must_use]`; never part of the public API.
///
/// ```compile_fail
/// #![deny(unused_must_use)]
/// let rendezvous = rendezvous::Rendezvous::new();
/// rendezvous.fork_guard();
/// ```
///
/// ```compile_fail
/// #![deny(unused_must_use)]
/// rendezvous::Rendezvous::new().outstanding();
/// ```
#[cfg(doctest)]
struct MustUseDoctests;

/// Runs the README examples as doctests. The async example needs the `tokio` feature.
#[cfg(all(doctest, feature = "tokio"))]
#[doc = include_str!("../README.md")]
struct ReadmeDoctests;

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;
    use std::time::Instant;

    /// Many threads concurrently fork and drop guards while the main thread waits. A lost wakeup
    /// would hang the test; a counter underflow would panic. Run with `--release` for more schedules.
    #[test]
    fn stress_sync_concurrent_fork_drop() {
        for _ in 0..200 {
            let rendezvous = Rendezvous::new();
            let mut handles = Vec::new();
            for _ in 0..64 {
                let guard = rendezvous.fork_guard();
                handles.push(thread::spawn(move || drop(guard)));
            }
            // Returns only once every guard has dropped (the count reached zero).
            rendezvous.rendezvous();
            for h in handles {
                h.join().unwrap();
            }
        }
    }

    /// Stresses concurrent increments (`fork`) racing decrements (`drop`) and the waiter.
    #[test]
    fn stress_sync_nested_fork() {
        for _ in 0..100 {
            let rendezvous = Rendezvous::new();
            let root = rendezvous.fork_guard();
            let mut handles = Vec::new();
            for _ in 0..16 {
                let g = root.clone();
                handles.push(thread::spawn(move || {
                    let sub = g.fork();
                    drop(g);
                    drop(sub);
                }));
            }
            drop(root);
            rendezvous.rendezvous();
            for h in handles {
                h.join().unwrap();
            }
        }
    }

    /// Exercises the `Notify` cross-thread wakeup and the create-`notified`-before-check loop under
    /// contention: guards are dropped from OS threads while an async task awaits the rendezvous.
    #[cfg(feature = "tokio")]
    #[test]
    fn stress_async_concurrent_drop() {
        tokio_test::block_on(async {
            for _ in 0..100 {
                let rendezvous = Rendezvous::new();
                let mut handles = Vec::new();
                for _ in 0..32 {
                    let guard = rendezvous.fork_guard();
                    handles.push(thread::spawn(move || drop(guard)));
                }
                rendezvous.rendezvous_async().await;
                for h in handles {
                    h.join().unwrap();
                }
            }
        });
    }

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

        let err = rendezvous
            .rendezvous_timeout(Duration::from_millis(100))
            .expect_err("guard still held, should have timed out");
        assert_eq!(err.outstanding(), 1);
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
        let rendezvous = rendezvous
            .rendezvous_timeout(Duration::from_millis(10))
            .expect_err("guard still held, should have timed out")
            .into_rendezvous();

        // Forking after a timeout must not panic.
        let second = rendezvous.fork_guard();

        // Drop both guards on a background thread, then wait again with enough time.
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(50));
            drop(first);
            drop(second);
        });

        assert!(
            rendezvous
                .rendezvous_timeout(Duration::from_secs(1))
                .is_ok()
        );
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

        let err = rendezvous
            .rendezvous_timeout(Duration::from_millis(10))
            .expect_err("guard still held, should have timed out");

        // Dropping after a timeout must return promptly, not block on the outstanding guard.
        let start = Instant::now();
        drop(err);
        assert!(
            start.elapsed() < Duration::from_secs(1),
            "drop after timeout blocked"
        );

        // Let the worker thread finish so we do not leak it past the test.
        handle.join().unwrap();
    }

    /// Dropping a rendezvous that was never waited on blocks until the last guard is released.
    #[test]
    fn implicit_drop_waits_for_guards() {
        use std::sync::atomic::{AtomicBool, Ordering};

        let rendezvous = Rendezvous::new();
        let guard = rendezvous.fork_guard();
        let released = std::sync::Arc::new(AtomicBool::new(false));

        let handle = thread::spawn({
            let released = released.clone();
            move || {
                thread::sleep(Duration::from_millis(50));
                released.store(true, Ordering::SeqCst);
                drop(guard);
            }
        });

        drop(rendezvous);
        assert!(released.load(Ordering::SeqCst), "drop returned early");
        handle.join().unwrap();
    }

    #[test]
    fn rendezvous_returned_by_timeout_is_detached() {
        let rendezvous = Rendezvous::new();
        let guard = rendezvous.fork_guard();
        let rendezvous = rendezvous
            .rendezvous_timeout(Duration::from_millis(1))
            .expect_err("guard still held, should have timed out")
            .into_rendezvous();
        assert_eq!(
            format!("{rendezvous:?}"),
            "Rendezvous { outstanding: 1, detached: true }"
        );
        drop(guard);
    }

    #[test]
    fn detach_does_not_block() {
        let rendezvous = Rendezvous::new();
        let guard = rendezvous.fork_guard();

        let start = Instant::now();
        rendezvous.detach();
        assert!(start.elapsed() < Duration::from_secs(1), "detach blocked");

        // The guard stays usable after the rendezvous is gone.
        let fork = guard.fork();
        drop(guard);
        drop(fork);
    }

    #[test]
    fn detached_guard_is_standalone() {
        let guard = RendezvousGuard::detached();
        let fork = guard.clone();
        drop(guard);
        drop(fork);
    }

    #[test]
    fn outstanding_tracks_guards() {
        let rendezvous = Rendezvous::new();
        assert_eq!(rendezvous.outstanding(), 0);
        let a = rendezvous.fork_guard();
        let b = a.fork();
        assert_eq!(rendezvous.outstanding(), 2);
        drop(a);
        assert_eq!(rendezvous.outstanding(), 1);
        drop(b);
        rendezvous.rendezvous();
    }

    #[test]
    fn debug_output() {
        let rendezvous = Rendezvous::new();
        let guard = rendezvous.fork_guard();
        assert_eq!(
            format!("{rendezvous:?}"),
            "Rendezvous { outstanding: 1, detached: false }"
        );
        assert_eq!(format!("{guard:?}"), "RendezvousGuard { outstanding: 1 }");

        let err = rendezvous
            .rendezvous_timeout(Duration::from_millis(1))
            .expect_err("guard still held, should have timed out");
        assert_eq!(
            format!("{err:?}"),
            "RendezvousTimeoutError { outstanding: 1, .. }"
        );
        assert_eq!(
            err.to_string(),
            "rendezvous timed out with 1 outstanding guard(s)"
        );
        drop(guard);
    }

    #[test]
    fn timeout_error_is_a_std_error() {
        fn assert_error<E: Error + Send + Sync + 'static>() {}
        assert_error::<RendezvousTimeoutError>();
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
    fn rendezvous_is_awaitable() {
        tokio_test::block_on(async {
            let rendezvous = Rendezvous::new();
            let guard = rendezvous.fork_guard();
            thread::spawn(move || {
                let _guard = guard;
                thread::sleep(Duration::from_millis(50))
            });
            rendezvous.await;
        });
    }

    /// Dropping an async wait (e.g. the losing branch of a `select!`) must not fall back to the
    /// blocking destructor, which would park the executor thread.
    #[cfg(feature = "tokio")]
    #[test]
    fn dropping_async_wait_does_not_block() {
        let rendezvous = Rendezvous::new();
        let guard = rendezvous.fork_guard();

        let start = Instant::now();
        drop(rendezvous.rendezvous_async());
        assert!(start.elapsed() < Duration::from_secs(1), "drop blocked");

        let rendezvous = Rendezvous::new();
        let other = rendezvous.fork_guard();
        drop(rendezvous.rendezvous_timeout_async(Duration::from_secs(60)));
        assert!(start.elapsed() < Duration::from_secs(1), "drop blocked");

        let rendezvous = Rendezvous::new();
        let third = rendezvous.fork_guard();
        drop(rendezvous.into_future());
        assert!(start.elapsed() < Duration::from_secs(1), "drop blocked");

        drop(guard);
        drop(other);
        drop(third);
    }

    /// Drops a rendezvous with one outstanding guard that a background thread releases shortly
    /// after, triggering the blocking-drop warning.
    #[cfg(any(feature = "log", feature = "tracing"))]
    fn drop_with_outstanding_guard() {
        let rendezvous = Rendezvous::new();
        let guard = rendezvous.fork_guard();
        let handle = thread::spawn(move || {
            thread::sleep(Duration::from_millis(20));
            drop(guard);
        });
        drop(rendezvous);
        handle.join().unwrap();
    }

    #[cfg(any(feature = "log", feature = "tracing"))]
    const DROP_WARNING: &str =
        "Rendezvous dropped with 1 outstanding guard(s); blocking until they are released";

    #[cfg(all(feature = "log", not(feature = "tracing")))]
    #[test]
    fn blocking_drop_warns_via_log() {
        use std::sync::Mutex;

        struct Capture(Mutex<Vec<String>>);

        impl log::Log for Capture {
            fn enabled(&self, _: &log::Metadata<'_>) -> bool {
                true
            }

            fn log(&self, record: &log::Record<'_>) {
                let line = format!("{} {}", record.level(), record.args());
                self.0.lock().unwrap().push(line);
            }

            fn flush(&self) {}
        }

        // The logger is process-global, so other tests may add records; only check for ours.
        static CAPTURE: Capture = Capture(Mutex::new(Vec::new()));
        log::set_logger(&CAPTURE).expect("no other test installs a logger");
        log::set_max_level(log::LevelFilter::Trace);

        drop_with_outstanding_guard();

        let expected = format!("WARN {DROP_WARNING}");
        assert!(CAPTURE.0.lock().unwrap().contains(&expected));
    }

    /// Also covers precedence: with both `log` and `tracing` enabled, events go to `tracing`.
    #[cfg(feature = "tracing")]
    #[test]
    fn blocking_drop_warns_via_tracing() {
        use std::sync::Mutex;
        use tracing::field::{Field, Visit};
        use tracing::span::{Attributes, Id, Record};

        struct Capture(std::sync::Arc<Mutex<Vec<String>>>);

        struct Message(String);

        impl Visit for Message {
            fn record_debug(&mut self, field: &Field, value: &dyn Debug) {
                if field.name() == "message" {
                    self.0 = format!("{value:?}");
                }
            }
        }

        impl tracing::Subscriber for Capture {
            fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
                true
            }

            fn new_span(&self, _: &Attributes<'_>) -> Id {
                Id::from_u64(1)
            }

            fn record(&self, _: &Id, _: &Record<'_>) {}

            fn record_follows_from(&self, _: &Id, _: &Id) {}

            fn event(&self, event: &tracing::Event<'_>) {
                let mut message = Message(String::new());
                event.record(&mut message);
                let line = format!("{} {}", event.metadata().level(), message.0);
                self.0.lock().unwrap().push(line);
            }

            fn enter(&self, _: &Id) {}

            fn exit(&self, _: &Id) {}
        }

        // Thread-local subscriber; the warning is emitted by `drop` on this thread.
        let lines = std::sync::Arc::new(Mutex::new(Vec::new()));
        tracing::subscriber::with_default(Capture(lines.clone()), drop_with_outstanding_guard);

        let expected = format!("WARN {DROP_WARNING}");
        assert!(lines.lock().unwrap().contains(&expected), "{lines:?}");
    }

    #[cfg(feature = "tokio")]
    #[test]
    fn async_timeout_returns_rendezvous() {
        tokio_test::block_on(async {
            let rendezvous = Rendezvous::new();
            let guard = rendezvous.fork_guard();

            let err = rendezvous
                .rendezvous_timeout_async(Duration::from_millis(10))
                .await
                .expect_err("guard still held, should have timed out");
            assert_eq!(err.outstanding(), 1);
            let rendezvous = err.into_rendezvous();

            drop(guard);
            assert!(
                rendezvous
                    .rendezvous_timeout_async(Duration::from_secs(1))
                    .await
                    .is_ok()
            );
        });
    }
}
