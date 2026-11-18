# Windows terminal client: first checkout

The product direction is in [the project scope](../../../terminal-client-project.md). This milestone establishes a Windows x64 Rust workspace for that SSH client. Running `cargo run -p terminal_client` on Windows opens a GPUI window displaying **No active session**. Closing the last window exits the application.

## Approaches

The selected approach is a runnable window and the five crate boundaries in the project scope. An empty Cargo workspace would not exercise GPUI’s Windows backend. Building a live SSH tab now would also require host verification, credential handling, and terminal emulation; those belong together in the next milestone.

## Repository conventions

Use Rust 2024, a pinned toolchain and GPUI dependency, a committed lockfile, workspace lints, and named library roots. Keep GPUI out of transport and storage. Add a project-specific `AGENTS.md`, Windows build instructions, and Windows CI. Preserve both existing architecture documents. No credentials, connections, or local shells are needed for the initial window.
