//! # Easier Rendezvous Channels
//!
//! A [`Rendezvous`] lets one thread or task wait until a group of workers have all reached a
//! synchronization point. Each worker holds a [`RendezvousGuard`]; once every guard is dropped,
//! the waiting [`Rendezvous::rendezvous`] call (or its async/timeout variants) proceeds. The
//! typical use is a graceful-shutdown barrier: wait, within a time budget, until every background
//! worker has drained.
//!
//! Internally this is a registry of live guards protected by a [`Mutex`], with a [`Condvar`] for
//! blocking waits and a list of task wakers for async waits, so async waits run on any executor
//! without depending on a particular runtime. The waiter never relinquishes its own handle, so
//! forking a guard always works — even after a timed-out wait — and a timeout never leaves the
//! [`Rendezvous`] in a state where dropping it blocks forever.
//!
//! ## When to use this crate
//!
//! |                                     | `rendezvous`     | [`std::sync::Barrier`] | `crossbeam` `WaitGroup` | `tokio-util` `TaskTracker` |
//! |-------------------------------------|------------------|------------------------|-------------------------|----------------------------|
//! | Who waits                           | one owner        | every participant      | every clone             | any holder                 |
//! | Participant count                   | dynamic          | fixed up front         | dynamic                 | dynamic                    |
//! | Blocking wait                       | yes              | yes                    | yes                     | no                         |
//! | Async wait                          | any executor     | no                     | no                      | yes                        |
//! | Bounded wait that can be retried    | yes              | no                     | no                      | via `tokio::time::timeout` |
//! | Reports *which* participants hang   | yes              | no                     | no                      | no                         |
//!
//! Reach for `rendezvous` when one coordinator must wait for workers that may live on OS threads
//! *or* async tasks, under a deadline, and you want to know who is late when the deadline passes.
//! If everything is a tokio task and you never need a blocking wait, `TaskTracker` is a fine
//! choice too.
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
//!
//! ## Graceful shutdown in async code
//!
//! A [`Rendezvous`] only *waits*; it does not tell workers to stop. Pair it with a cancellation
//! signal such as `tokio_util::sync::CancellationToken` or a `tokio::sync::watch` channel, and tie
//! each guard to its task with [`RendezvousGuard::wrap`] so it is released when the task ends:
//!
//! ```rust
//! # async fn example() {
//! use std::time::Duration;
//! use rendezvous::Rendezvous;
//!
//! let rendezvous = Rendezvous::new();
//! let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
//!
//! for id in 0..4 {
//!     let mut stop = stop_rx.clone();
//!     let worker = async move {
//!         // ... do work until asked to stop, then drain ...
//!         let _ = stop.wait_for(|stop| *stop).await;
//!     };
//!     // Named guards show up in the timeout error if this worker hangs.
//!     tokio::spawn(rendezvous.fork_named(format!("worker-{id}")).wrap(worker));
//! }
//!
//! // ... later, on SIGTERM:
//! let _ = stop_tx.send(true);
//! let budget = tokio::time::sleep(Duration::from_secs(15));
//! if let Err(err) = rendezvous.rendezvous_or(budget).await {
//!     // e.g. "rendezvous timed out with 1 outstanding guard(s): worker-2 (forked at src/main.rs:16:26)"
//!     eprintln!("{err}");
//! }
//! # }
//! ```
//!
//! ## Async runtimes
//!
//! The crate does not depend on any async runtime. The async waits
//! ([`Rendezvous::rendezvous_async`], `.await` on a [`Rendezvous`], and
//! [`Rendezvous::rendezvous_or`]) only use the standard [`Waker`] machinery, so they run on any
//! executor. The one thing a *bounded* async wait needs from a runtime is a timer, and
//! [`Rendezvous::rendezvous_or`] takes it as a plain future. Its output is ignored, so any future
//! works as the "give up" signal, such as a second shutdown request.
//!
//! ### tokio
//!
//! ```rust
//! use std::time::Duration;
//! use rendezvous::Rendezvous;
//!
//! #[tokio::main]
//! async fn main() {
//!     let rendezvous = Rendezvous::new();
//!     let guard = rendezvous.fork_guard();
//!     tokio::spawn(async move { drop(guard) });
//!
//!     // Wait for at most 15 seconds.
//!     let budget = tokio::time::sleep(Duration::from_secs(15));
//!     assert!(rendezvous.rendezvous_or(budget).await.is_ok());
//!
//!     // Or until a deadline, e.g. one budget shared by several shutdown phases.
//!     let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
//!     let rendezvous = Rendezvous::new();
//!     assert!(rendezvous.rendezvous_or(tokio::time::sleep_until(deadline)).await.is_ok());
//! }
//! ```
//!
//! ### smol / async-io
//!
//! smol re-exports [`async_io::Timer`](https://docs.rs/async-io/latest/async_io/struct.Timer.html)
//! as `smol::Timer`.
//!
//! ```rust
//! use std::time::{Duration, Instant};
//! use rendezvous::Rendezvous;
//!
//! async_io::block_on(async {
//!     let rendezvous = Rendezvous::new();
//!     let guard = rendezvous.fork_guard();
//!     std::thread::spawn(move || drop(guard));
//!
//!     // Wait for at most 15 seconds.
//!     let budget = async_io::Timer::after(Duration::from_secs(15));
//!     assert!(rendezvous.rendezvous_or(budget).await.is_ok());
//!
//!     // Or until a deadline.
//!     let deadline = Instant::now() + Duration::from_secs(15);
//!     let rendezvous = Rendezvous::new();
//!     assert!(rendezvous.rendezvous_or(async_io::Timer::at(deadline)).await.is_ok());
//! });
//! ```
//!
//! ### Any executor
//!
//! [`futures-timer`](https://docs.rs/futures-timer) drives its `Delay` from its own thread, so it
//! works on every executor, including minimal ones like `futures::executor::block_on`:
//!
//! ```rust
//! use std::time::Duration;
//! use rendezvous::Rendezvous;
//!
//! # async_io::block_on(async {
//! let rendezvous = Rendezvous::new();
//! let guard = rendezvous.fork_named("slow");
//!
//! let budget = futures_timer::Delay::new(Duration::from_millis(10));
//! let err = rendezvous.rendezvous_or(budget).await.expect_err("the guard is still alive");
//! assert_eq!(err.outstanding_guards()[0].name(), Some("slow"));
//! drop(guard);
//! # });
//! ```
//!
//! The `graceful_shutdown` example in the repository shows the complete pattern on tokio: run it
//! with `cargo run --example graceful_shutdown`, add `-- --stuck` to see a timeout
//! name a hung worker, or `-- --fail-start` for the startup-failure path.
//!
//! ## Diagnosing stuck guards
//!
//! Every guard records the source location that forked it (via `#[track_caller]`), and guards
//! created with [`Rendezvous::fork_named`] or [`RendezvousGuard::fork_named`] carry a label that
//! their forks and clones inherit. When a bounded wait times out, the
//! [`RendezvousTimeoutError`] lists every guard that is still alive
//! ([`RendezvousTimeoutError::outstanding_guards`]), and its `Display` output names them, so a log
//! line points at the worker that failed to finish. [`Rendezvous::outstanding_guards`] returns the
//! same information at any time.
//!
//! ## Drop semantics
//!
//! A [`Rendezvous`] that is dropped without being waited on acts as an implicit rendezvous: the
//! destructor **blocks** until every outstanding guard is released. This is convenient in
//! synchronous code but a hazard in async code, where it parks an executor thread. To opt out:
//!
//! * use a bounded wait ([`Rendezvous::rendezvous_timeout`], [`Rendezvous::rendezvous_until`] or
//!   [`Rendezvous::rendezvous_or`]) — a rendezvous handed back on timeout is detached and drops
//!   without blocking;
//! * use [`Rendezvous::rendezvous_async`] (or `.await` the rendezvous directly) — the rendezvous
//!   is detached as soon as the future is created, so cancelling the future never blocks;
//! * call [`Rendezvous::detach`] to give up on the outstanding guards explicitly, e.g. on an
//!   early-return error path.
//!
//! ## Pitfalls
//!
//! * **Holding a guard in the waiting thread deadlocks.** `rendezvous()` waits for *every* guard,
//!   including one still alive in the caller's own scope. Move guards into the workers.
//! * **A worker that never ends keeps its guard forever.** A rendezvous only completes once workers
//!   actually exit; make sure each one observes a stop signal, and prefer a bounded wait so a stuck
//!   worker is reported instead of hanging the process.
//! * **Early returns between spawning workers and waiting.** A `?` after the workers are spawned
//!   drops the [`Rendezvous`] while they still run, which blocks (forever, if nobody tells them to
//!   stop). Signal the workers and wait with a timeout, or call [`Rendezvous::detach`], on those
//!   paths.
//! * **Forgetting a guard.** `std::mem::forget` on a guard leaks it; the rendezvous will never
//!   complete and a bounded wait will report it as outstanding.
//!
//! ## Crate Features
//!
//! * `log` - Emits diagnostics through the `log` crate.
//! * `tracing` - Emits diagnostics through the `tracing` crate (takes precedence over `log`).
//!
//! There are no runtime features; see [Async runtimes](#async-runtimes).

// only enables the `doc_cfg` feature when
// the `docsrs` configuration attribute is defined
#![cfg_attr(docsrs, feature(doc_cfg))]

use std::collections::BTreeMap;
use std::collections::btree_map::Entry;
use std::error::Error;
use std::fmt::{Debug, Display, Formatter};
use std::panic::Location;
use std::pin::Pin;
use std::task::{Context, Poll, Waker};

// Under `--cfg rendezvous_loom` the synchronization primitives are swapped for loom's instrumented versions
// so the model checker can explore every thread interleaving of the sync core.
#[cfg(rendezvous_loom)]
use loom::sync::{Arc, Condvar, Mutex, MutexGuard};
#[cfg(not(rendezvous_loom))]
use std::sync::{Arc, Condvar, Mutex, MutexGuard};

// The time types are only needed by the blocking timeout APIs, which are excluded from loom builds
// (loom does not model time).
#[cfg(not(rendezvous_loom))]
use std::time::{Duration, Instant};

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
    /// The registry of live guards.
    state: Mutex<State>,
    /// Notifies synchronous waiters when the last guard is released.
    cv: Condvar,
}

/// The registry of live guards. The owning [`Rendezvous`] does *not* register itself; the
/// rendezvous completes once `live` is empty.
struct State {
    /// The ID handed to the next forked guard or waiting future. IDs increase monotonically, so
    /// iterating `live` yields guards in fork order.
    next_id: u64,
    /// Every guard that has not been dropped yet, keyed by its ID.
    live: BTreeMap<u64, OutstandingGuard>,
    /// The wakers of pending async waits, keyed by the waiting future's ID. Drained and woken when
    /// the last guard is released.
    wakers: BTreeMap<u64, Waker>,
}

impl Shared {
    /// Locks the guard registry, recovering from poisoning. Every mutation of the registry is a
    /// single map insert or remove, so a panicking guard thread cannot leave it inconsistent, and
    /// recovering avoids permanently wedging a waiter.
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl State {
    /// Allocates a fresh ID.
    fn next_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    /// Registers a new guard and returns its ID.
    fn insert(
        &mut self,
        name: Option<std::sync::Arc<str>>,
        location: &'static Location<'static>,
    ) -> u64 {
        let id = self.next_id();
        self.live.insert(id, OutstandingGuard { name, location });
        id
    }

    /// Returns the live guards in fork order.
    fn snapshot(&self) -> Vec<OutstandingGuard> {
        self.live.values().cloned().collect()
    }
}

/// [`Rendezvous`] is a synchronization primitive that allows a thread to wait until a group of
/// worker threads have all reached a certain point in the code before proceeding.
///
/// See the [crate-level documentation](crate#drop-semantics) for what happens when it is dropped.
#[doc(alias = "WaitGroup")]
#[doc(alias = "latch")]
#[doc(alias = "barrier")]
pub struct Rendezvous {
    /// The shared guard registry and notification primitives.
    shared: Arc<Shared>,
    /// Set to `true` once the caller opted out of an implicit rendezvous (timeout, async wait or
    /// [`Rendezvous::detach`]). Tells [`Drop`] not to block waiting for outstanding guards.
    detached: bool,
}

/// A guard forked off a [`Rendezvous`] struct. While it is alive it keeps the owning
/// [`Rendezvous`] from completing; dropping it (or all clones of it) releases the rendezvous.
///
/// Each guard remembers where it was forked and, optionally, a name; see
/// [Diagnosing stuck guards](crate#diagnosing-stuck-guards).
#[must_use = "a guard that is dropped immediately does not hold the rendezvous; \
              bind it or move it into the worker"]
pub struct RendezvousGuard {
    /// The shared guard registry and notification primitives.
    shared: Arc<Shared>,
    /// This guard's key in the registry.
    id: u64,
}

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
                state: Mutex::new(State {
                    next_id: 0,
                    live: BTreeMap::new(),
                    wakers: BTreeMap::new(),
                }),
                cv: Condvar::new(),
            }),
            detached: false,
        }
    }

    /// Forks a guard off the [`Rendezvous`] channel.
    ///
    /// When all guards are dropped, [`Rendezvous::rendezvous`] will proceed; until then, that
    /// call blocks. The guard records the caller's source location for diagnostics.
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
    #[track_caller]
    pub fn fork_guard(&self) -> RendezvousGuard {
        emit!(trace, "Forking rendezvous guard");
        let id = self.shared.lock().insert(None, Location::caller());
        RendezvousGuard {
            shared: self.shared.clone(),
            id,
        }
    }

    /// Forks a named guard off the [`Rendezvous`] channel.
    ///
    /// Behaves like [`Rendezvous::fork_guard`], but the guard carries `name`, which its forks and
    /// clones inherit and which is reported in [`RendezvousTimeoutError`] if the guard is still
    /// outstanding when a bounded wait times out.
    ///
    /// ## Example
    ///
    /// ```
    /// use std::time::Duration;
    /// use rendezvous::Rendezvous;
    ///
    /// let rendezvous = Rendezvous::new();
    /// let guard = rendezvous.fork_named("event-pump");
    ///
    /// let err = rendezvous
    ///     .rendezvous_timeout(Duration::from_millis(10))
    ///     .expect_err("the guard is still alive");
    /// assert_eq!(err.outstanding_guards()[0].name(), Some("event-pump"));
    /// drop(guard);
    /// ```
    #[track_caller]
    pub fn fork_named(&self, name: impl Into<std::sync::Arc<str>>) -> RendezvousGuard {
        let name = name.into();
        emit!(trace, "Forking rendezvous guard {}", name);
        let id = self.shared.lock().insert(Some(name), Location::caller());
        RendezvousGuard {
            shared: self.shared.clone(),
            id,
        }
    }

    /// Forks a guard and ties it to `future`: the guard is released when the future completes or
    /// is dropped.
    ///
    /// Shorthand for `rendezvous.fork_guard().wrap(future)`; see [`RendezvousGuard::wrap`].
    ///
    /// ## Example
    ///
    /// ```
    /// use rendezvous::Rendezvous;
    ///
    /// # tokio_test::block_on(async {
    /// let rendezvous = Rendezvous::new();
    /// let task = rendezvous.wrap(async { 42 });
    /// assert_eq!(rendezvous.outstanding(), 1);
    /// assert_eq!(task.await, 42);
    /// assert_eq!(rendezvous.outstanding(), 0);
    /// # });
    /// ```
    #[track_caller]
    pub fn wrap<F: Future>(&self, future: F) -> GuardedFuture<F> {
        self.fork_guard().wrap(future)
    }

    /// Returns the number of guards that have not been released yet.
    ///
    /// The value is a snapshot; guards on other threads may be forked or dropped concurrently.
    #[must_use]
    pub fn outstanding(&self) -> usize {
        self.shared.lock().live.len()
    }

    /// Returns the guards that have not been released yet, in the order they were forked.
    ///
    /// The value is a snapshot; guards on other threads may be forked or dropped concurrently.
    ///
    /// ## Example
    ///
    /// ```
    /// use rendezvous::Rendezvous;
    ///
    /// let rendezvous = Rendezvous::new();
    /// let guard = rendezvous.fork_named("worker");
    /// let outstanding = rendezvous.outstanding_guards();
    /// assert_eq!(outstanding.len(), 1);
    /// assert_eq!(outstanding[0].name(), Some("worker"));
    /// drop(guard);
    /// ```
    #[must_use]
    pub fn outstanding_guards(&self) -> Vec<OutstandingGuard> {
        self.shared.lock().snapshot()
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
    /// Unlike a blocking wait, this does not occupy a thread: the returned [`RendezvousFuture`]
    /// registers the polling task's waker and is woken when the last guard is dropped. It works on
    /// any executor; no runtime feature is required.
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
    pub fn rendezvous_async(self) -> RendezvousFuture {
        RendezvousFuture::new(self)
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
    #[cfg(not(rendezvous_loom))]
    pub fn rendezvous_timeout(mut self, timeout: Duration) -> Result<(), RendezvousTimeoutError> {
        match self.wait_timeout(timeout) {
            Ok(()) => Ok(()),
            Err(outstanding) => {
                self.detached = true;
                Err(RendezvousTimeoutError::new(self, outstanding))
            }
        }
    }

    /// Executes the rendezvous process with a deadline.
    ///
    /// Behaves like [`Rendezvous::rendezvous_timeout`], but takes the point in time at which to
    /// give up. Handy when several shutdown phases share one overall budget. A deadline in the
    /// past still succeeds if no guards are outstanding.
    ///
    /// ## Example
    ///
    /// ```
    /// use std::time::{Duration, Instant};
    /// use rendezvous::Rendezvous;
    ///
    /// let deadline = Instant::now() + Duration::from_secs(1);
    ///
    /// let rendezvous = Rendezvous::new();
    /// let guard = rendezvous.fork_guard();
    /// std::thread::spawn(move || drop(guard));
    /// assert!(rendezvous.rendezvous_until(deadline).is_ok());
    /// ```
    #[cfg(not(rendezvous_loom))]
    pub fn rendezvous_until(self, deadline: Instant) -> Result<(), RendezvousTimeoutError> {
        self.rendezvous_timeout(deadline.saturating_duration_since(Instant::now()))
    }

    /// Asynchronously executes the rendezvous process, giving up when `timer` completes.
    ///
    /// This is the bounded async wait, and it works with any runtime: pass the runtime's sleep
    /// future, such as `tokio::time::sleep(duration)`, `async_io::Timer::after(duration)` (smol)
    /// or `futures_timer::Delay::new(duration)`, or any other future that should end the wait,
    /// e.g. a second shutdown signal. The timer's output is ignored. For a deadline, use the
    /// runtime's deadline timer (`tokio::time::sleep_until`, `async_io::Timer::at`). See
    /// [Async runtimes](crate#async-runtimes) for complete examples.
    ///
    /// Resolves like [`Rendezvous::rendezvous_timeout`]: `Ok(())` once every guard is released, or
    /// a [`RendezvousTimeoutError`] carrying the detached rendezvous and the outstanding guards if
    /// `timer` completes first. If both are ready at the same time, the rendezvous wins.
    ///
    /// Like [`Rendezvous::rendezvous_async`], the rendezvous is detached as soon as this method is
    /// called, so dropping the returned future cancels the wait without blocking.
    ///
    /// ## Example
    ///
    /// ```
    /// use std::time::Duration;
    /// use rendezvous::Rendezvous;
    ///
    /// # async_io::block_on(async {
    /// let rendezvous = Rendezvous::new();
    /// let guard = rendezvous.fork_named("slow");
    ///
    /// let err = rendezvous
    ///     .rendezvous_or(async_io::Timer::after(Duration::from_millis(10)))
    ///     .await
    ///     .expect_err("the guard is still alive");
    /// assert_eq!(err.outstanding_guards()[0].name(), Some("slow"));
    ///
    /// drop(guard);
    /// assert!(err.into_rendezvous().rendezvous_or(std::future::pending::<()>()).await.is_ok());
    /// # });
    /// ```
    pub fn rendezvous_or<T: Future>(self, timer: T) -> RendezvousOr<T> {
        RendezvousOr {
            wait: Some(RendezvousFuture::new(self)),
            timer,
        }
    }

    /// Blocks until no guards are outstanding.
    ///
    /// Uses a manual predicate loop (rather than `Condvar::wait_while`) because loom's `Condvar`
    /// only exposes `wait`; the loop re-checks the registry to absorb spurious wakeups.
    fn wait(&self) {
        let mut state = self.shared.lock();
        while !state.live.is_empty() {
            state = self
                .shared
                .cv
                .wait(state)
                .unwrap_or_else(|e| e.into_inner());
        }
    }

    /// Blocks until no guards are outstanding or the timeout elapses.
    ///
    /// Returns the outstanding guards on timeout.
    #[cfg(not(rendezvous_loom))]
    fn wait_timeout(&self, timeout: Duration) -> Result<(), Vec<OutstandingGuard>> {
        let state = self.shared.lock();
        let (state, _) = self
            .shared
            .cv
            .wait_timeout_while(state, timeout, |state| !state.live.is_empty())
            .unwrap_or_else(|e| e.into_inner());
        if state.live.is_empty() {
            Ok(())
        } else {
            Err(state.snapshot())
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
impl IntoFuture for Rendezvous {
    type Output = ();
    type IntoFuture = RendezvousFuture;

    fn into_future(self) -> Self::IntoFuture {
        self.rendezvous_async()
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
    #[track_caller]
    pub fn detached() -> Self {
        let rendezvous = Rendezvous::new();
        let guard = rendezvous.fork_guard();
        rendezvous.detach();
        guard
    }

    /// Forks a guard off the owning [`Rendezvous`] channel.
    ///
    /// When all guards are dropped, [`Rendezvous::rendezvous`] will proceed; until then, that
    /// call blocks. The fork inherits this guard's name, if any, and records the caller's source
    /// location.
    #[track_caller]
    pub fn fork(&self) -> RendezvousGuard {
        emit!(trace, "Forking nested rendezvous guard");
        let mut state = self.shared.lock();
        let name = state.live.get(&self.id).and_then(|g| g.name.clone());
        let id = state.insert(name, Location::caller());
        drop(state);
        RendezvousGuard {
            shared: self.shared.clone(),
            id,
        }
    }

    /// Forks a guard with a new name off the owning [`Rendezvous`] channel.
    ///
    /// Useful for handing a labelled guard to each of several workers that share a parent guard,
    /// e.g. `parent.fork_named(format!("job-worker-{i}"))`.
    #[track_caller]
    pub fn fork_named(&self, name: impl Into<std::sync::Arc<str>>) -> RendezvousGuard {
        let name = name.into();
        emit!(trace, "Forking nested rendezvous guard {}", name);
        let id = self.shared.lock().insert(Some(name), Location::caller());
        RendezvousGuard {
            shared: self.shared.clone(),
            id,
        }
    }

    /// Ties this guard to `future`: the guard is released when the future completes or is
    /// dropped (e.g. when its task is aborted).
    ///
    /// This replaces the pattern of passing a guard into a task and dropping it at the end. The
    /// returned [`GuardedFuture`] does not allocate, and is `Send`/`Unpin` whenever `future` is,
    /// so it can be handed to `tokio::spawn` directly.
    ///
    /// ## Example
    ///
    /// ```
    /// use rendezvous::Rendezvous;
    ///
    /// # tokio_test::block_on(async {
    /// let rendezvous = Rendezvous::new();
    /// let worker = rendezvous.fork_named("worker").wrap(async {
    ///     // ... work until asked to stop ...
    /// });
    /// worker.await;
    /// rendezvous.rendezvous(); // the guard is already released
    /// # });
    /// ```
    pub fn wrap<F: Future>(self, future: F) -> GuardedFuture<F> {
        GuardedFuture {
            future,
            guard: Some(self),
        }
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
    /// Equivalent to [`RendezvousGuard::fork`].
    #[track_caller]
    fn clone(&self) -> Self {
        self.fork()
    }
}

impl Debug for RendezvousGuard {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        let state = self.shared.lock();
        let info = state.live.get(&self.id);
        f.debug_struct("RendezvousGuard")
            .field("name", &info.and_then(|g| g.name()))
            .field("forked_at", &info.map(|g| AsDisplay(g.location)))
            .finish()
    }
}

impl Drop for RendezvousGuard {
    fn drop(&mut self) {
        let mut state = self.shared.lock();
        state.live.remove(&self.id);
        if state.live.is_empty() {
            // Removal and notification happen under the lock, so a blocking waiter cannot miss the
            // wakeup, and an async waiter either sees the empty registry when it polls or has its
            // waker in `wakers` by now.
            self.shared.cv.notify_all();
            let wakers = std::mem::take(&mut state.wakers);
            // Wake outside the lock: a waker may run arbitrary executor code.
            drop(state);
            for waker in wakers.into_values() {
                waker.wake();
            }
        }
    }
}

impl Drop for Rendezvous {
    fn drop(&mut self) {
        // The caller opted out of the implicit rendezvous; do not block.
        if self.detached {
            return;
        }

        let outstanding = self.outstanding_guards();
        if !outstanding.is_empty() {
            emit!(
                warn,
                "Rendezvous dropped with {} outstanding guard(s), blocking until they are released: {}",
                outstanding.len(),
                GuardList(&outstanding)
            );
        }

        // Acts as an implicit guard: block until all rendezvous points are reached.
        self.wait();
    }
}

pin_project_lite::pin_project! {
    /// A future that holds a [`RendezvousGuard`] until it completes or is dropped.
    ///
    /// Created by [`RendezvousGuard::wrap`] and [`Rendezvous::wrap`]. Polls the inner future in
    /// place, without boxing; the guard is released as soon as the inner future completes, even if
    /// the `GuardedFuture` itself is kept around afterwards.
    #[must_use = "futures do nothing unless polled"]
    pub struct GuardedFuture<F> {
        #[pin]
        future: F,
        guard: Option<RendezvousGuard>,
    }
}

impl<F: Future> Future for GuardedFuture<F> {
    type Output = F::Output;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<F::Output> {
        let this = self.project();
        let output = std::task::ready!(this.future.poll(cx));
        this.guard.take();
        Poll::Ready(output)
    }
}

impl<F> Debug for GuardedFuture<F> {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GuardedFuture")
            .field("guard", &self.guard)
            .finish_non_exhaustive()
    }
}

/// Future returned by [`Rendezvous::rendezvous_async`] and by `.await` on a [`Rendezvous`].
///
/// Completes once every guard has been released. It registers the polling task's
/// [`Waker`] with the rendezvous instead of relying on a runtime primitive, so it works on any
/// executor. The rendezvous it owns is detached: dropping the future unregisters the waker and
/// never blocks.
#[must_use = "futures do nothing unless polled"]
pub struct RendezvousFuture {
    rendezvous: Rendezvous,
    waiter: Waiter,
}

impl RendezvousFuture {
    fn new(mut rendezvous: Rendezvous) -> Self {
        // Detach eagerly (not on first poll) so that a future dropped before it is ever polled
        // does not fall back to the blocking destructor either.
        rendezvous.detached = true;
        let waiter = Waiter {
            shared: rendezvous.shared.clone(),
            id: None,
        };
        Self { rendezvous, waiter }
    }

    /// Ends the wait after a timer fired. The last guard may have been released since the last
    /// poll, in which case the rendezvous succeeded after all.
    fn finish_timed_out(self) -> Result<(), RendezvousTimeoutError> {
        let Self { rendezvous, waiter } = self;
        drop(waiter);
        let outstanding = rendezvous.shared.lock().snapshot();
        if outstanding.is_empty() {
            Ok(())
        } else {
            Err(RendezvousTimeoutError::new(rendezvous, outstanding))
        }
    }
}

impl Future for RendezvousFuture {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        if self.get_mut().waiter.poll_complete(cx.waker()) {
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    }
}

impl Debug for RendezvousFuture {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RendezvousFuture")
            .field("rendezvous", &self.rendezvous)
            .finish_non_exhaustive()
    }
}

/// A waiting future's registration in [`State::wakers`]. Unregisters on drop, so cancelled waits
/// do not accumulate stale wakers.
struct Waiter {
    shared: Arc<Shared>,
    /// The key in [`State::wakers`], allocated on the first pending poll.
    id: Option<u64>,
}

impl Waiter {
    /// Returns `true` once no guards are outstanding; otherwise (re-)registers `waker` and returns
    /// `false`.
    ///
    /// The check and the registration happen under the same lock that [`RendezvousGuard`]'s
    /// `Drop` takes to remove the last guard and drain the wakers, so a release cannot slip in
    /// between them and be missed.
    fn poll_complete(&mut self, waker: &Waker) -> bool {
        let mut state = self.shared.lock();
        if state.live.is_empty() {
            if let Some(id) = self.id.take() {
                state.wakers.remove(&id);
            }
            return true;
        }

        let id = match self.id {
            Some(id) => id,
            None => *self.id.insert(state.next_id()),
        };
        match state.wakers.entry(id) {
            Entry::Occupied(mut entry) => {
                if !entry.get().will_wake(waker) {
                    entry.insert(waker.clone());
                }
            }
            Entry::Vacant(entry) => {
                entry.insert(waker.clone());
            }
        }
        false
    }
}

impl Drop for Waiter {
    fn drop(&mut self) {
        if let Some(id) = self.id.take() {
            self.shared.lock().wakers.remove(&id);
        }
    }
}

pin_project_lite::pin_project! {
    /// Future returned by [`Rendezvous::rendezvous_or`].
    ///
    /// Resolves to `Ok(())` once every guard is released, or to a [`RendezvousTimeoutError`] when
    /// the timer completes first.
    #[must_use = "futures do nothing unless polled"]
    pub struct RendezvousOr<T> {
        wait: Option<RendezvousFuture>,
        #[pin]
        timer: T,
    }
}

impl<T: Future> Future for RendezvousOr<T> {
    type Output = Result<(), RendezvousTimeoutError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.project();
        let wait = this
            .wait
            .as_mut()
            .expect("`RendezvousOr` polled after completion");

        // The rendezvous is checked first, so a timer that fires at the same moment does not turn
        // a completed rendezvous into a timeout.
        if Pin::new(wait).poll(cx).is_ready() {
            *this.wait = None;
            return Poll::Ready(Ok(()));
        }
        if this.timer.poll(cx).is_pending() {
            return Poll::Pending;
        }

        let wait = this.wait.take().expect("checked above");
        Poll::Ready(wait.finish_timed_out())
    }
}

impl<T> Debug for RendezvousOr<T> {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RendezvousOr")
            .field("wait", &self.wait)
            .finish_non_exhaustive()
    }
}

/// A guard that was still alive when it was inspected, e.g. when a bounded wait timed out.
///
/// See [Diagnosing stuck guards](crate#diagnosing-stuck-guards).
#[derive(Clone)]
pub struct OutstandingGuard {
    name: Option<std::sync::Arc<str>>,
    location: &'static Location<'static>,
}

impl OutstandingGuard {
    /// The name given via `fork_named` (inherited by forks and clones), if any.
    #[must_use]
    pub fn name(&self) -> Option<&str> {
        self.name.as_deref()
    }

    /// The source location that forked or cloned the guard.
    #[must_use]
    pub fn location(&self) -> &'static Location<'static> {
        self.location
    }
}

impl Debug for OutstandingGuard {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OutstandingGuard")
            .field("name", &self.name())
            .field("forked_at", &AsDisplay(self.location))
            .finish()
    }
}

/// Formats as `name (forked at file:line:column)`, or `forked at file:line:column` if unnamed.
impl Display for OutstandingGuard {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match &self.name {
            Some(name) => write!(f, "{name} (forked at {})", self.location),
            None => write!(f, "forked at {}", self.location),
        }
    }
}

/// A rendezvous timed out while guards were still outstanding.
///
/// Carries the (detached) [`Rendezvous`] so the caller can retry, plus the guards that were still
/// alive at the time; dropping the error does not block. Its `Display` output names the
/// outstanding guards on a single line, ready for a log message.
pub struct RendezvousTimeoutError {
    rendezvous: Rendezvous,
    outstanding: Vec<OutstandingGuard>,
}

impl RendezvousTimeoutError {
    fn new(rendezvous: Rendezvous, outstanding: Vec<OutstandingGuard>) -> Self {
        let error = Self {
            rendezvous,
            outstanding,
        };
        emit!(debug, "{}", error);
        error
    }

    /// The number of guards that were still outstanding when the timeout occurred.
    #[must_use]
    pub fn outstanding(&self) -> usize {
        self.outstanding.len()
    }

    /// The guards that were still outstanding when the timeout occurred, in fork order.
    #[must_use]
    pub fn outstanding_guards(&self) -> &[OutstandingGuard] {
        &self.outstanding
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
            "rendezvous timed out with {} outstanding guard(s): {}",
            self.outstanding.len(),
            GuardList(&self.outstanding)
        )
    }
}

impl Error for RendezvousTimeoutError {}

/// Formats guards as a `; `-separated list.
struct GuardList<'a>(&'a [OutstandingGuard]);

impl Display for GuardList<'_> {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        for (i, guard) in self.0.iter().enumerate() {
            if i > 0 {
                f.write_str("; ")?;
            }
            Display::fmt(guard, f)?;
        }
        Ok(())
    }
}

/// Debug-formats a value through its `Display` impl, e.g. a [`Location`] as `file:line:column`.
struct AsDisplay<T>(T);

impl<T: Display> Debug for AsDisplay<T> {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        Display::fmt(&self.0, f)
    }
}

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

/// Runs the README examples as doctests.
#[cfg(doctest)]
#[doc = include_str!("../README.md")]
struct ReadmeDoctests;

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

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
            // Returns only once every guard has dropped (the registry is empty).
            rendezvous.rendezvous();
            for h in handles {
                h.join().unwrap();
            }
        }
    }

    /// Stresses concurrent registrations (`fork`) racing removals (`drop`) and the waiter.
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

    /// Exercises the cross-thread waker handoff under contention: guards are dropped from OS
    /// threads while an async task awaits the rendezvous. Runs on async-io's executor, not tokio's.
    #[test]
    fn stress_async_concurrent_drop() {
        async_io::block_on(async {
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
    fn guards_record_name_and_fork_site() {
        let rendezvous = Rendezvous::new();
        let (unnamed, unnamed_line) = (rendezvous.fork_guard(), line!());
        let (named, named_line) = (rendezvous.fork_named("pump"), line!());
        let (fork, fork_line) = (named.fork(), line!());
        let (clone, clone_line) = (named.clone(), line!());
        let (renamed, renamed_line) = (named.fork_named(String::from("pump-2")), line!());

        let guards = rendezvous.outstanding_guards();
        let summary: Vec<_> = guards
            .iter()
            .map(|g| (g.name(), g.location().file(), g.location().line()))
            .collect();
        assert_eq!(
            summary,
            [
                (None, file!(), unnamed_line),
                (Some("pump"), file!(), named_line),
                (Some("pump"), file!(), fork_line),
                (Some("pump"), file!(), clone_line),
                (Some("pump-2"), file!(), renamed_line),
            ]
        );

        drop((unnamed, named, fork, clone, renamed));
        assert!(rendezvous.outstanding_guards().is_empty());
        rendezvous.rendezvous();
    }

    #[test]
    fn timeout_error_names_outstanding_guards() {
        let rendezvous = Rendezvous::new();
        let released = rendezvous.fork_named("released");
        let (stuck, line) = (rendezvous.fork_named("stuck"), line!());
        let (anonymous, anonymous_line) = (rendezvous.fork_guard(), line!());
        drop(released);

        let err = rendezvous
            .rendezvous_timeout(Duration::from_millis(1))
            .expect_err("guards still held, should have timed out");
        let [first, second] = err.outstanding_guards() else {
            panic!("expected two outstanding guards: {err:?}");
        };
        assert_eq!(
            (first.location().file(), first.location().line()),
            (file!(), line)
        );
        assert_eq!(
            (second.location().file(), second.location().line()),
            (file!(), anonymous_line)
        );
        assert_eq!(
            err.to_string(),
            format!(
                "rendezvous timed out with 2 outstanding guard(s): \
                 stuck (forked at {}); forked at {}",
                first.location(),
                second.location()
            )
        );
        drop((stuck, anonymous));
    }

    #[test]
    fn debug_output() {
        let rendezvous = Rendezvous::new();
        let (guard, line) = (rendezvous.fork_named("w"), line!());
        assert_eq!(
            format!("{rendezvous:?}"),
            "Rendezvous { outstanding: 1, detached: false }"
        );
        let location = rendezvous.outstanding_guards()[0].location();
        assert_eq!(location.line(), line);
        let forked_at = location.to_string();
        assert_eq!(
            format!("{guard:?}"),
            format!("RendezvousGuard {{ name: Some(\"w\"), forked_at: Some({forked_at}) }}")
        );

        let err = rendezvous
            .rendezvous_timeout(Duration::from_millis(1))
            .expect_err("guard still held, should have timed out");
        assert_eq!(
            format!("{err:?}"),
            format!(
                "RendezvousTimeoutError {{ outstanding: \
                 [OutstandingGuard {{ name: Some(\"w\"), forked_at: {forked_at} }}], .. }}"
            )
        );
        drop(guard);
    }

    #[test]
    fn deadline_in_the_past_times_out_or_succeeds() {
        let past = Instant::now();

        let rendezvous = Rendezvous::new();
        assert!(Rendezvous::new().rendezvous_until(past).is_ok());

        let guard = rendezvous.fork_guard();
        let err = rendezvous
            .rendezvous_until(past)
            .expect_err("guard still held, should have timed out");
        assert_eq!(err.outstanding(), 1);
        drop(guard);
    }

    #[test]
    fn timeout_error_is_a_std_error() {
        fn assert_error<E: Error + Send + Sync + 'static>() {}
        assert_error::<RendezvousTimeoutError>();
    }

    #[test]
    fn public_types_are_thread_and_unwind_safe() {
        use std::panic::{RefUnwindSafe, UnwindSafe};

        fn assert_auto_traits<T: Send + Sync + UnwindSafe + RefUnwindSafe>() {}
        assert_auto_traits::<Rendezvous>();
        assert_auto_traits::<RendezvousGuard>();
        assert_auto_traits::<RendezvousTimeoutError>();
        assert_auto_traits::<OutstandingGuard>();
        assert_auto_traits::<RendezvousFuture>();
        assert_auto_traits::<RendezvousOr<std::future::Pending<()>>>();
    }

    #[test]
    fn wrapped_future_releases_guard_when_done_or_dropped() {
        fn assert_send<T: Send>(_: &T) {}

        let rendezvous = Rendezvous::new();
        let (done, line) = (rendezvous.wrap(async { 7 }), line!());
        let dropped = rendezvous.fork_named("dropped").wrap(async {});
        assert_send(&done);

        let guards = rendezvous.outstanding_guards();
        assert_eq!(guards[0].location().line(), line, "wrap records its caller");
        assert_eq!(guards[1].name(), Some("dropped"));

        assert_eq!(tokio_test::block_on(done), 7);
        assert_eq!(rendezvous.outstanding(), 1);
        drop(dropped);
        assert_eq!(rendezvous.outstanding(), 0);
        rendezvous.rendezvous();
    }

    /// The guard is released on completion, not only when the finished future is dropped.
    #[test]
    fn guarded_future_releases_guard_on_completion() {
        fn assert_unpin<T: Unpin>() {}
        assert_unpin::<GuardedFuture<std::future::Ready<()>>>();

        let rendezvous = Rendezvous::new();
        let mut future = std::pin::pin!(rendezvous.wrap(async { 1 }));
        let mut cx = Context::from_waker(std::task::Waker::noop());
        assert_eq!(future.as_mut().poll(&mut cx), Poll::Ready(1));
        assert_eq!(rendezvous.outstanding(), 0, "future is still alive here");
        rendezvous.rendezvous();
    }

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

    /// The bounded async wait with tokio's deadline timer, in both outcomes.
    #[test]
    fn rendezvous_or_with_tokio_deadline() {
        tokio_test::block_on(async {
            let rendezvous = Rendezvous::new();
            let guard = rendezvous.fork_named("late");
            let deadline = tokio::time::Instant::now() + Duration::from_millis(10);
            let err = rendezvous
                .rendezvous_or(tokio::time::sleep_until(deadline))
                .await
                .expect_err("guard still held, should have timed out");
            assert_eq!(err.outstanding_guards()[0].name(), Some("late"));

            drop(guard);
            let now = tokio::time::Instant::now();
            assert!(
                err.into_rendezvous()
                    .rendezvous_or(tokio::time::sleep_until(now))
                    .await
                    .is_ok()
            );
        });
    }

    /// Dropping an async wait (e.g. the losing branch of a `select!`) must not fall back to the
    /// blocking destructor, which would park the executor thread.
    #[test]
    fn dropping_async_wait_does_not_block() {
        let start = Instant::now();
        let mut guards = Vec::new();

        let rendezvous = Rendezvous::new();
        guards.push(rendezvous.fork_guard());
        drop(rendezvous.rendezvous_async());

        let rendezvous = Rendezvous::new();
        guards.push(rendezvous.fork_guard());
        drop(rendezvous.rendezvous_or(std::future::pending::<()>()));

        let rendezvous = Rendezvous::new();
        guards.push(rendezvous.fork_guard());
        drop(rendezvous.into_future());

        // A future that was polled (and registered its waker) must unregister on drop too.
        let rendezvous = Rendezvous::new();
        guards.push(rendezvous.fork_guard());
        let mut future = rendezvous.rendezvous_async();
        let mut cx = Context::from_waker(Waker::noop());
        assert!(Pin::new(&mut future).poll(&mut cx).is_pending());
        drop(future);
        assert!(guards.last().unwrap().shared.lock().wakers.is_empty());

        assert!(start.elapsed() < Duration::from_secs(1), "drop blocked");
        drop(guards);
    }

    /// Counts how often it is woken.
    struct CountingWaker(std::sync::atomic::AtomicUsize);

    impl std::task::Wake for CountingWaker {
        fn wake(self: std::sync::Arc<Self>) {
            self.wake_by_ref();
        }

        fn wake_by_ref(self: &std::sync::Arc<Self>) {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
    }

    impl CountingWaker {
        fn new() -> (std::sync::Arc<Self>, Waker) {
            let counter = std::sync::Arc::new(Self(std::sync::atomic::AtomicUsize::new(0)));
            (counter.clone(), Waker::from(counter))
        }

        fn count(&self) -> usize {
            self.0.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    /// Walks the waker bookkeeping by hand: one registration per future, replaced when the task
    /// is polled with a different waker, and woken exactly once, by the last guard release.
    #[test]
    fn async_wait_registers_and_wakes_once() {
        let rendezvous = Rendezvous::new();
        let first = rendezvous.fork_guard();
        let last = first.fork();
        let mut future = rendezvous.rendezvous_async();

        let (stale, stale_waker) = CountingWaker::new();
        let mut stale_cx = Context::from_waker(&stale_waker);
        assert!(Pin::new(&mut future).poll(&mut stale_cx).is_pending());

        // Re-polling with a different waker (the task moved) replaces the registration.
        let (counter, waker) = CountingWaker::new();
        let mut cx = Context::from_waker(&waker);
        assert!(Pin::new(&mut future).poll(&mut cx).is_pending());
        assert!(Pin::new(&mut future).poll(&mut cx).is_pending());
        let registered = last.shared.lock().wakers.len();
        assert_eq!(registered, 1, "re-polls must not add wakers");

        // Releasing one of two guards does not wake.
        drop(first);
        assert_eq!(counter.count(), 0);

        drop(last);
        assert_eq!(
            counter.count(),
            1,
            "the last release wakes the current waker"
        );
        assert_eq!(stale.count(), 0, "the replaced waker is not woken");
        assert_eq!(Pin::new(&mut future).poll(&mut cx), Poll::Ready(()));
    }

    #[test]
    fn rendezvous_or_times_out_when_the_timer_wins() {
        let rendezvous = Rendezvous::new();
        let guard = rendezvous.fork_named("held");

        let err = async_io::block_on(rendezvous.rendezvous_or(std::future::ready(())))
            .expect_err("guard still held, should have timed out");
        assert_eq!(err.outstanding_guards()[0].name(), Some("held"));

        // Retrying works, and a completed rendezvous wins over an already-ready timer.
        let rendezvous = err.into_rendezvous();
        drop(guard);
        assert!(async_io::block_on(rendezvous.rendezvous_or(std::future::ready(()))).is_ok());
    }

    /// The bounded async wait with a non-tokio runtime's timer, in both outcomes.
    #[test]
    fn rendezvous_or_with_async_io_timer() {
        async_io::block_on(async {
            let rendezvous = Rendezvous::new();
            let guard = rendezvous.fork_guard();
            let err = rendezvous
                .rendezvous_or(async_io::Timer::after(Duration::from_millis(10)))
                .await
                .expect_err("guard still held, should have timed out");

            let rendezvous = err.into_rendezvous();
            thread::spawn(move || {
                thread::sleep(Duration::from_millis(20));
                drop(guard);
            });
            assert!(
                rendezvous
                    .rendezvous_or(async_io::Timer::after(Duration::from_secs(5)))
                    .await
                    .is_ok()
            );
        });
    }

    /// Drops a rendezvous with one outstanding guard that a background thread releases shortly
    /// after, triggering the blocking-drop warning.
    #[cfg(any(feature = "log", feature = "tracing"))]
    fn drop_with_outstanding_guard() {
        let rendezvous = Rendezvous::new();
        let guard = rendezvous.fork_named("slow");
        let handle = thread::spawn(move || {
            thread::sleep(Duration::from_millis(20));
            drop(guard);
        });
        drop(rendezvous);
        handle.join().unwrap();
    }

    #[cfg(any(feature = "log", feature = "tracing"))]
    const DROP_WARNING: &str = "WARN Rendezvous dropped with 1 outstanding guard(s), \
                                blocking until they are released: slow (forked at ";

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

        let lines = CAPTURE.0.lock().unwrap();
        assert!(
            lines.iter().any(|line| line.starts_with(DROP_WARNING)),
            "{lines:?}"
        );
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

        let lines = lines.lock().unwrap();
        assert!(
            lines.iter().any(|line| line.starts_with(DROP_WARNING)),
            "{lines:?}"
        );
    }
}
