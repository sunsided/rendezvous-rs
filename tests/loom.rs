//! Loom model-checking of the synchronous rendezvous core.
//!
//! These tests exhaustively explore the thread interleavings of the guard counter, `Condvar`
//! wakeups and `Arc` drops, which is a stronger guarantee than the probabilistic stress tests in
//! the library crate. The async wait is model-checked too, since its waker registry shares the
//! same lock. Loom does not model time, so the timeout paths are covered only by the stress tests.
//!
//! Run with:
//!
//! ```sh
//! RUSTFLAGS="--cfg rendezvous_loom" cargo test --release --test loom
//! ```
#![cfg(rendezvous_loom)]

use rendezvous::Rendezvous;

/// A single guard dropped on a background thread must always release a waiting `rendezvous()`,
/// regardless of whether the drop happens before, during or after the wait parks.
#[test]
fn single_guard_releases_waiter() {
    loom::model(|| {
        let rendezvous = Rendezvous::new();
        let guard = rendezvous.fork_guard();

        let handle = loom::thread::spawn(move || drop(guard));

        rendezvous.rendezvous();
        handle.join().unwrap();
    });
}

/// Two guards dropped concurrently from separate threads: the rendezvous must complete exactly
/// once both reach zero, with no lost wakeup on any interleaving.
#[test]
fn two_guards_release_waiter() {
    loom::model(|| {
        let rendezvous = Rendezvous::new();
        let g1 = rendezvous.fork_guard();
        let g2 = g1.fork();

        let h1 = loom::thread::spawn(move || drop(g1));
        let h2 = loom::thread::spawn(move || drop(g2));

        rendezvous.rendezvous();
        h1.join().unwrap();
        h2.join().unwrap();
    });
}

/// Dropping the `Rendezvous` itself acts as an implicit rendezvous; it must block until the
/// outstanding guard is gone on every schedule.
#[test]
fn drop_acts_as_implicit_rendezvous() {
    loom::model(|| {
        let rendezvous = Rendezvous::new();
        let guard = rendezvous.fork_guard();

        let handle = loom::thread::spawn(move || drop(guard));

        drop(rendezvous);
        handle.join().unwrap();
    });
}

/// An async waiter must be woken by a guard dropped on another thread, whether the drop happens
/// before the first poll, between the registry check and the waker registration, or after.
#[test]
fn async_waiter_is_woken() {
    loom::model(|| {
        let rendezvous = Rendezvous::new();
        let guard = rendezvous.fork_guard();

        let handle = loom::thread::spawn(move || drop(guard));

        loom::future::block_on(rendezvous.rendezvous_async());
        handle.join().unwrap();
    });
}

/// Two guards released concurrently while an async waiter is parked: exactly the last release
/// must wake it, with no lost wakeup on any interleaving.
#[test]
fn async_waiter_two_guards() {
    loom::model(|| {
        let rendezvous = Rendezvous::new();
        let g1 = rendezvous.fork_guard();
        let g2 = g1.fork();

        let h1 = loom::thread::spawn(move || drop(g1));
        let h2 = loom::thread::spawn(move || drop(g2));

        loom::future::block_on(rendezvous.rendezvous_async());
        h1.join().unwrap();
        h2.join().unwrap();
    });
}
