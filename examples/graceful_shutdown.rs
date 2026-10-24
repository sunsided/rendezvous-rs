//! A tokio service that drains its background workers on shutdown, within a time budget.
//!
//! ```sh
//! cargo run --example graceful_shutdown --features tokio                  # clean shutdown
//! cargo run --example graceful_shutdown --features tokio -- --stuck       # a worker hangs
//! cargo run --example graceful_shutdown --features tokio -- --fail-start  # startup error
//! ```
//!
//! The pattern: a `CancellationToken` *tells* workers to stop, the `Rendezvous` *waits* for them,
//! and every worker's guard is tied to its task with `wrap`, so it is released when the task ends.

use std::process::ExitCode;
use std::time::Duration;

use rendezvous::Rendezvous;
use tokio_util::sync::CancellationToken;

/// How long shutdown may take before we give up and report the stragglers. In a real service,
/// size this against the orchestrator's grace period (e.g. Kubernetes'
/// `terminationGracePeriodSeconds`).
const SHUTDOWN_BUDGET: Duration = Duration::from_secs(2);

#[tokio::main]
async fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    let simulate_stuck = args.iter().any(|arg| arg == "--stuck");
    let simulate_startup_failure = args.iter().any(|arg| arg == "--fail-start");

    let rendezvous = Rendezvous::new();
    let shutdown = CancellationToken::new();

    for id in 0..3 {
        // The named guard is released when the worker's future completes (or its task is aborted).
        let guard = rendezvous.fork_named(format!("worker-{id}"));
        tokio::spawn(guard.wrap(worker(id, shutdown.child_token())));
    }

    if simulate_stuck {
        tokio::spawn(rendezvous.fork_named("stuck-worker").wrap(stuck_worker()));
    }

    if simulate_startup_failure {
        eprintln!("startup failed; stopping the workers that already started");
        shutdown.cancel();
        // Returning here would drop the `Rendezvous` while workers still hold guards, and that
        // drop blocks the executor thread until they finish. Either wait with a budget (as below)
        // or, when there is nothing worth waiting for, detach explicitly:
        rendezvous.detach();
        return ExitCode::FAILURE;
    }

    // Stand-in for serving traffic until SIGTERM / Ctrl+C (`tokio::signal::ctrl_c()`).
    println!("serving; shutting down in one second");
    tokio::time::sleep(Duration::from_secs(1)).await;

    println!("shutting down");
    shutdown.cancel();

    match rendezvous.rendezvous_timeout_async(SHUTDOWN_BUDGET).await {
        Ok(()) => {
            println!("all workers drained, goodbye");
            ExitCode::SUCCESS
        }
        Err(err) => {
            // e.g. "rendezvous timed out with 1 outstanding guard(s):
            //       stuck-worker (forked at examples/graceful_shutdown.rs:39:33)"
            eprintln!("{err}");
            ExitCode::FAILURE
        }
    }
}

/// Works until cancelled, then drains.
async fn worker(id: usize, shutdown: CancellationToken) {
    let mut ticks = tokio::time::interval(Duration::from_millis(250));
    loop {
        tokio::select! {
            () = shutdown.cancelled() => break,
            _ = ticks.tick() => { /* process a unit of work */ }
        }
    }

    // Flush buffers, finish in-flight jobs, ...
    tokio::time::sleep(Duration::from_millis(100)).await;
    println!("worker-{id} drained");
}

/// A worker with a bug: it never observes the shutdown signal.
async fn stuck_worker() {
    std::future::pending::<()>().await;
}
