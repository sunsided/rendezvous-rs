# Changelog

All notable changes to this project will be documented in this file.
This project uses [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

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
