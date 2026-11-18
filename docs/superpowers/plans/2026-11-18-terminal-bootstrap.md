# Windows terminal bootstrap implementation plan

> **For agentic workers:** Use `executing-plans` or `subagent-driven-development` to implement this plan task by task.

**Goal:** Make a fresh Windows checkout run a GPUI window showing “No active session”.

**Architecture:** `terminal_client` owns the window; `terminal_view` renders the empty state. Reserve `terminal`, `ssh`, and `hosts` as documented library crates until their first features land.

**Tech stack:** Rust 2024, Cargo, GPUI, Windows x64 MSVC, GitHub Actions.

**Spec:** [Approved bootstrap design](../specs/2026-11-18-terminal-bootstrap-design.md).

## Global constraints

Centralize versions and lints; pin Rust and GPUI, commit `Cargo.lock`, and use named library roots. Preserve the existing architecture documents. Add no Zed application crates, credentials, network sessions, or mock connection controls.

## Review focus

Check clean-checkout dependency resolution, Windows SDK prerequisites, visible startup errors, readable text after resizing, and process exit on last-window close. CI covers the build; the README supplies the interactive Windows smoke check.

### Task 1: Runnable workspace

- [ ] Create `Cargo.toml`, `rust-toolchain.toml`, `rustfmt.toml`, `.gitignore`, and `crates/{terminal_client,terminal_view,terminal,ssh,hosts}/Cargo.toml`. Use a named `.rs` library root for each library. Generate `Cargo.lock` and confirm `cargo metadata --locked --no-deps` lists all five members.
- [ ] In `crates/terminal_client/src/main.rs` open the workspace window, handle startup errors, and quit when the last window closes. Implement the workspace root in `crates/terminal_client/src/workspace.rs` using the empty state from `crates/terminal_view/src/terminal_view.rs`. Leave the three backend library roots free of speculative APIs.
- [ ] Build `cargo build --locked -p terminal_client` on Windows. Launch, resize, and close the window on a Windows desktop; verify the message remains visible and the process exits. Record which checks were possible.

### Task 2: Maintainer instructions and CI

- [ ] Create `AGENTS.md` with applicable Zed Rust and GPUI conventions, crate boundaries, error handling, and SSH trust requirements. Link the upstream guidance and existing scope rather than copying Zed-specific scripts.
- [ ] Create `README.md` with Windows toolchain prerequisites, launch command, current scope, and smoke check. Add `.github/workflows/ci.yml` on a Windows runner for formatting, locked Clippy, workspace tests, and a linked application build.
- [ ] Check all documented commands against the workflow and manifests. Report local results separately from checks awaiting a Windows runner.
