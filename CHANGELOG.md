# Changelog

All notable changes to this project will be documented in this file.
This project uses [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.5.0] - Unreleased

[0.5.0]: https://github.com/sunsided/rendezvous-rs/releases/tag/v0.5.0

### Changed

- **Breaking:** `RendezvousTimeoutError` is now a struct that carries the detached `Rendezvous`
  (`into_rendezvous()`) and the number of guards still outstanding (`outstanding()`).
  `rendezvous_timeout` and `rendezvous_timeout_async` return `Result<(), RendezvousTimeoutError>`
  instead of `Result<(), (Self, RendezvousTimeoutError)>`, so the error works with `?`.
- **Breaking:** `rendezvous_async` and `rendezvous_timeout_async` are now plain functions returning
  `impl Future + Send`. They detach the rendezvous at call time, so dropping the future (e.g. a
  losing `select!` branch) no longer falls back to the blocking destructor.
- Dropping a non-detached `Rendezvous` with outstanding guards now logs a warning in all builds
  (previously a debug-only `error!`).
- Moved to edition 2024; the minimum supported Rust version is now 1.85.
- The `tokio` dependency no longer enables the `rt` feature.

### Added

- `Rendezvous::detach` to give up on outstanding guards without blocking.
- `Rendezvous::outstanding` to read the current guard count.
- `RendezvousGuard::detached` for a guard that no rendezvous waits on (replaces
  `Option<RendezvousGuard>` plumbing in tests).
- Guard diagnostics: every guard records the source location that forked it (`#[track_caller]`),
  and `fork_named` attaches a label that forks and clones inherit. `RendezvousTimeoutError`
  lists the outstanding guards (`outstanding_guards()`) and names them in its `Display` output;
  `Rendezvous::outstanding_guards` returns the same at any time. The blocking-drop warning names
  them too.
- `RendezvousGuard::wrap` / `Rendezvous::wrap` tie a guard to a future via the allocation-free
  `GuardedFuture`, which releases the guard when the future completes or is dropped.
- `rendezvous_until` and `rendezvous_until_async` (`tokio` feature) wait until a deadline
  (`Instant`) instead of for a duration.
- `IntoFuture` for `Rendezvous` (`tokio` feature): `rendezvous.await`.
- `Debug` for `Rendezvous`, `RendezvousGuard` and `RendezvousTimeoutError`.
- `tracing` feature to emit diagnostics through `tracing` instead of `log`.
- `#[must_use]` on `RendezvousGuard` and the constructors/getters.

### Fixed

- Documentation: the README no longer describes the old `mpsc`-based design, and the crate-level
  example now actually runs as a doctest. `cargo doc` without the `tokio` feature no longer has
  broken links.

### Documentation

- Crate docs gained a comparison with `std::sync::Barrier`, `crossbeam`'s `WaitGroup` and
  `tokio-util`'s `TaskTracker`, plus sections on async graceful shutdown, diagnosing stuck guards
  and common pitfalls. `Rendezvous` is searchable as `WaitGroup`, `latch` and `barrier`.

## [0.4.0] - 2026-06-17

[0.4.0]: https://github.com/sunsided/rendezvous-rs/releases/tag/v0.4.0

### Changed

- Reimplemented the core on an explicit guard counter guarded by a `Mutex`/`Condvar` (plus
  `tokio::sync::Notify` under the `tokio` feature) instead of `mpsc::channel` drop detection. The
  waiter no longer relinquishes its own handle, removing the finicky drop-before-`recv` ordering.
- **Breaking:** `rendezvous_timeout` now consumes `self` and returns
  `Result<(), (Self, RendezvousTimeoutError)>`, handing the `Rendezvous` back on timeout so retrying
  or forking more guards is type-enforced rather than convention.
- **Breaking:** `rendezvous_async` is now truly asynchronous (it parks on a `Notify` rather than
  `spawn_blocking`) and returns `()` instead of `Result<(), JoinError>`.

### Added

- `rendezvous_timeout_async` (tokio feature): a bounded, cancellable async wait.

### Fixed

- `fork_guard` no longer panics when called after a `rendezvous_timeout` timeout.
- Dropping a `Rendezvous` after a timeout no longer blocks indefinitely; the caller already opted
  into bounded-wait semantics, so the destructor returns immediately.
- The async wait no longer occupies a blocking-pool thread and is cancellable.

### Internal

- Added [loom](https://docs.rs/loom) model-checking of the synchronous core (`tests/loom.rs`, run
  with `RUSTFLAGS="--cfg loom" cargo test --release --test loom`) plus high-concurrency stress
  tests for the sync and async paths.

## [0.3.0] - 2025-01-25

[0.3.0]: https://github.com/sunsided/rendezvous-rs/releases/tag/v0.3.0

### Updated

- Removed unused `tokio` features when enabled.

### Internal

- Removed `Cargo.lock` from repository.

## [0.2.3] - 2024-03-05

[0.2.3]: https://github.com/sunsided/rendezvous-rs/releases/tag/0.2.3

### Fixed

- [#2](https://github.com/sunsided/rendezvous-rs/pull/2):
  Fixed a security issue in the `mio` dependency.

## [0.2.2] - 2023-12-02

### Fixed

- Fixed build on docs.rs.

## [0.2.1] - 2023-12-02

### Added

- Added `Clone` for `RendezvousGuard`.

## [0.2.0] - 2023-12-01

### Added

- Added support for the `log` crate.
- `Rendezvous` now implements `Default`.

## [0.1.1] - 2023-12-01

### Fixed

- Fixed a doctest issue preventing `cargo test --docs` from succeeding.

## [0.1.0] - 2023-12-01

### Added

- Added `Rendezvous` type.

[0.2.2]: https://github.com/sunsided/rendezvous-rs/releases/tag/0.2.2

[0.2.1]: https://github.com/sunsided/rendezvous-rs/releases/tag/0.2.1

[0.2.0]: https://github.com/sunsided/rendezvous-rs/releases/tag/0.2.0

[0.1.1]: https://github.com/sunsided/rendezvous-rs/releases/tag/0.1.1

[0.1.0]: https://github.com/sunsided/rendezvous-rs/releases/tag/0.1.0
