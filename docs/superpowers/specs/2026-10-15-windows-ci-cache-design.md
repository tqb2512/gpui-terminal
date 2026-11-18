# Windows CI dependency cache

Speed up repeat Windows CI runs by reusing Cargo downloads and compiled GPUI dependencies. Keep the `windows-2025` runner, the pinned Rust toolchain, all four Windows checks, and the Ubuntu job unchanged. Do not change workflow triggers or permissions.

## Approaches

Use a Windows-only Rust cache. It can reuse compiled dependencies across runs without spreading Cargo work across multiple runners. Splitting the checks could reduce wall time, but would duplicate setup and potentially compilation.

Restore the cache after formatting so formatting failures still stop early and Rustup has selected the repository toolchain. Use a commit-pinned `Swatinem/rust-cache` action with its default dependency caching. A cold run should still build normally; compare its Windows job duration with a subsequent warm run to measure the benefit.
