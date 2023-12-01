# Easier Rendezvous Channels

In rust, [mpsc::channel](https://doc.rust-lang.org/std/sync/mpsc/fn.channel.html) can be used as a synchronization
primitive between threads by utilizing the fact that we can block on the receiver's `recv()` function until all senders
are dropped.

This crate aims at giving the concept an expressive name and at reducing some classes of race conditions, namely those
where the original sender was not dropped before the call to `recv()`.

This version of the crate only supports synchronous code due to the dropping semantics.

```shell
cargo add rendezvous
```

## Example usage

```rust
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;
use rendezvous::{Rendezvous, RendezvousGuard};

/// A slow worker function. Sleeps, then mutates a value.
fn slow_worker_fn(_guard: RendezvousGuard, mut value: Arc<Mutex<u32>>) {
    thread::sleep(Duration::from_millis(400));
    let mut value = value.lock().unwrap();
    *value = 42;
}

fn example() {
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
}
```
