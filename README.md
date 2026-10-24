# Easier Rendezvous Channels

[![crates.io](https://img.shields.io/crates/v/rendezvous.svg)](https://crates.io/crates/rendezvous)
[![docs.rs](https://docs.rs/rendezvous/badge.svg)](https://docs.rs/rendezvous)
[![CI](https://github.com/sunsided/rendezvous-rs/actions/workflows/rust.yml/badge.svg)](https://github.com/sunsided/rendezvous-rs/actions/workflows/rust.yml)
[![MSRV](https://img.shields.io/badge/MSRV-1.85-blue.svg)](https://blog.rust-lang.org/2025/02/20/Rust-1.85.0/)
[![License: EUPL-1.2](https://img.shields.io/badge/license-EUPL--1.2-blue.svg)](LICENSE.md)

A `Rendezvous` lets one thread or task wait until a group of workers have all reached a
synchronization point. Each worker holds a `RendezvousGuard`; once every guard is dropped, the
waiting `rendezvous()` call proceeds. Typical use: a graceful-shutdown barrier that waits, within a
time budget, for background workers to drain — and tells you which ones did not.

```shell
cargo add rendezvous
```

## Highlights

* **Sync and async in one type, on any runtime.** Block a thread with `rendezvous()`, or
  `.await` it on tokio, smol or any other executor. The crate depends on no async runtime, and
  workers can be OS threads, async tasks, or both.
* **Bounded waits that can be retried.** On timeout you get the `Rendezvous` back, detached so
  dropping it never blocks.
* **Reports stuck workers.** Guards record where they were forked (and an optional name), so a
  timeout reads like
  `rendezvous timed out with 1 outstanding guard(s): stuck-worker (forked at src/main.rs:39:33)`.
* **Guards tied to futures.** `tokio::spawn(guard.wrap(work))` releases the guard when the task
  ends, without boxing.

See the [crate documentation](https://docs.rs/rendezvous) for how it compares to
`std::sync::Barrier`, `crossbeam`'s `WaitGroup` and `tokio-util`'s `TaskTracker`, plus drop
semantics and common pitfalls.

## Crate features

* `tracing` / `log` - diagnostics through the respective crate (`tracing` wins if both are on).

There are no runtime features: async support is always available and works on any executor.

## Example usage

```rust
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;
use rendezvous::{Rendezvous, RendezvousGuard};

/// A slow worker function. Sleeps, then mutates a value.
fn slow_worker_fn(_guard: RendezvousGuard, value: Arc<Mutex<u32>>) {
    thread::sleep(Duration::from_millis(400));
    let mut value = value.lock().unwrap();
    *value = 42;
}

// The guard that ensures synchronization across threads.
// Rendezvous itself acts as a guard: If not explicitly dropped, it will block the current
// scope until all rendezvous points are reached.
let rendezvous = Rendezvous::new();

// A value to mutate in a different thread.
let value = Arc::new(Mutex::new(0u32));

// Run the worker in a thread.
thread::spawn({
    let guard = rendezvous.fork_guard();
    let value = value.clone();
    move || slow_worker_fn(guard, value)
});

// Block until the thread has finished its work.
rendezvous.rendezvous();

// The thread finished in time.
assert_eq!(*(value.lock().unwrap()), 42);
```

## Async runtimes

Async waiting (`rendezvous.rendezvous_async().await`, or just `rendezvous.await`) only uses the
standard `Waker` machinery. A *bounded* async wait needs a timer, which `rendezvous_or` takes as a
plain future from whatever runtime you use; the timer's output is ignored.

### tokio

```rust
use std::time::Duration;
use rendezvous::Rendezvous;

#[tokio::main]
async fn main() {
    let rendezvous = Rendezvous::new();
    let guard = rendezvous.fork_guard();
    tokio::spawn(async move { drop(guard) });

    // Wait for at most 15 seconds.
    let budget = tokio::time::sleep(Duration::from_secs(15));
    assert!(rendezvous.rendezvous_or(budget).await.is_ok());

    // Or until a deadline, e.g. one budget shared by several shutdown phases.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    let rendezvous = Rendezvous::new();
    assert!(rendezvous.rendezvous_or(tokio::time::sleep_until(deadline)).await.is_ok());
}
```

### smol / async-io

smol re-exports `async_io::Timer` as `smol::Timer`.

```rust
use std::time::{Duration, Instant};
use rendezvous::Rendezvous;

async_io::block_on(async {
    let rendezvous = Rendezvous::new();
    let guard = rendezvous.fork_guard();
    std::thread::spawn(move || drop(guard));

    // Wait for at most 15 seconds.
    let budget = async_io::Timer::after(Duration::from_secs(15));
    assert!(rendezvous.rendezvous_or(budget).await.is_ok());

    // Or until a deadline.
    let deadline = Instant::now() + Duration::from_secs(15);
    let rendezvous = Rendezvous::new();
    assert!(rendezvous.rendezvous_or(async_io::Timer::at(deadline)).await.is_ok());
});
```

### Any executor

[`futures-timer`](https://docs.rs/futures-timer) drives its `Delay` from its own thread, so it
works on every executor, including minimal ones like `futures::executor::block_on`:

```rust
use std::time::Duration;
use rendezvous::Rendezvous;

async fn shutdown(rendezvous: Rendezvous) {
    let budget = futures_timer::Delay::new(Duration::from_secs(15));
    if let Err(err) = rendezvous.rendezvous_or(budget).await {
        eprintln!("{err}");
    }
}
```

## Graceful shutdown (tokio)

```rust
use std::time::Duration;
use rendezvous::Rendezvous;

async fn run(stop: tokio::sync::watch::Receiver<bool>) {
    let rendezvous = Rendezvous::new();

    for id in 0..4 {
        let mut stop = stop.clone();
        let worker = async move {
            // ... work until asked to stop, then drain ...
            let _ = stop.wait_for(|stop| *stop).await;
        };
        tokio::spawn(rendezvous.fork_named(format!("worker-{id}")).wrap(worker));
    }

    // ... once the stop signal was sent:
    let budget = tokio::time::sleep(Duration::from_secs(15));
    if let Err(err) = rendezvous.rendezvous_or(budget).await {
        // The rendezvous inside `err` is detached; dropping it does not block.
        eprintln!("{err}");
    }
}
```

The [`graceful_shutdown`](examples/graceful_shutdown.rs) example shows the full pattern with a
`CancellationToken`, a stuck worker and a startup failure:

```shell
cargo run --example graceful_shutdown -- --stuck
```

## Drop semantics

Dropping a `Rendezvous` that was never waited on **blocks** until all guards are released. In
async code, or on early-return error paths where workers will never be told to stop, opt out:

* a timed-out bounded wait (`rendezvous_timeout`, `rendezvous_until`, `rendezvous_or`) hands back a
  detached rendezvous;
* `rendezvous_async` (and `.await`) detach immediately, so a cancelled wait never blocks;
* `Rendezvous::detach()` gives up explicitly without waiting.

Where an API wants a guard but nothing needs to wait for it (e.g. tests), use
`RendezvousGuard::detached()` instead of `Option<RendezvousGuard>`.

## Minimum supported Rust version

Rust 1.85 (edition 2024). Raising it is treated as a minor-version change.
