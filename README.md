# Easier Rendezvous Channels

A `Rendezvous` lets one thread (or task) wait until a group of workers have all reached a
synchronization point. Each worker holds a `RendezvousGuard`; once every guard is dropped, the
waiting `rendezvous()` call proceeds. Typical use: a graceful-shutdown barrier that waits for
background workers to drain.

Internally this is a guard counter behind a `Mutex`/`Condvar` (plus a `tokio::sync::Notify` with
the `tokio` feature). The waiter never gives up its own handle, so forking a guard always works,
even after a timed-out wait.

```shell
cargo add rendezvous
```

## Crate features

* `tokio` - asynchronous waits: `rendezvous_async`, `rendezvous_timeout_async`, and `.await` on a
  `Rendezvous`. They park on a `Notify` instead of blocking a thread.
* `tracing` / `log` - diagnostics through the respective crate (`tracing` wins if both are on).

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

## Bounded shutdown (async)

```rust
use std::time::Duration;
use rendezvous::Rendezvous;

async fn shutdown(rendezvous: Rendezvous) {
    if let Err(err) = rendezvous.rendezvous_timeout_async(Duration::from_secs(15)).await {
        // The rendezvous inside `err` is detached; dropping it does not block.
        eprintln!("giving up: {err}");
    }
}
```

## Drop semantics

Dropping a `Rendezvous` that was never waited on **blocks** until all guards are released. In
async code, or on early-return error paths where workers will never be told to stop, opt out:

* a timed-out `rendezvous_timeout`/`rendezvous_timeout_async` hands back a detached rendezvous;
* `rendezvous_async` (and `.await`) detach immediately, so a cancelled wait never blocks;
* `Rendezvous::detach()` gives up explicitly without waiting.

Where an API wants a guard but nothing needs to wait for it (e.g. tests), use
`RendezvousGuard::detached()` instead of `Option<RendezvousGuard>`.
