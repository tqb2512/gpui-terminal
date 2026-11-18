# GPUI Terminal Client

A Windows-first desktop SSH client in Rust, built with Zed’s GPUI. This initial
checkout opens a window with **No active session**. It does not connect to SSH
hosts or start local shells yet.

## Run on Windows

To try a CI build, open a successful `push` run for `main` under **Actions**
and select **Download Windows build (ZIP)** in the summary. Sign in to GitHub
with repository access, then extract and run `terminal_client.exe`. Avoid
running executables from pull requests you don't trust. The download expires
according to the repository's artifact retention policy.

To build from source:

Use an x64 Windows desktop with [Rustup](https://rustup.rs/) and Visual Studio
Build Tools with the **Desktop development with C++** workload and a Windows
SDK. Use an x64 Native Tools command prompt so Cargo can find the MSVC and SDK
tools. Rustup reads the toolchain version from `rust-toolchain.toml`.

```powershell
cargo run --locked -p terminal_client
```

Check that a **GPUI Terminal Client** window opens, **No active session** stays
visible while resizing, and closing the window returns control to the prompt.
On a small, high-DPI display, check that the title bar is visible at launch.

## Development checks

```powershell
cargo fmt --all -- --check
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo test --locked --workspace
cargo build --locked -p terminal_client
```

See the [project scope and architecture](terminal-client-project.md),
[architecture diagram](gpui-terminal-architecture.html), and [coding guidelines](AGENTS.md).
