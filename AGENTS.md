# Rust coding guidelines

- Prioritize correctness and clarity. Use descriptive, unabbreviated names.
- Extend existing files unless there is a distinct component. Use `module.rs`,
  not `module/mod.rs`; name library roots with `[lib] path = "src/<crate>.rs"`.
- Inherit dependency versions and lints from the workspace. Commit `Cargo.lock`
  when dependencies change. Do not depend on Zed application crates.
- Comment on why a non-obvious decision was made, not on what the code does.
- In production code, propagate errors with `?` or handle them explicitly.
  Avoid `unwrap()`, `expect()`, panicking indexing, and silently discarding
  errors. Surface actionable failures to the user without exposing secrets.

# GPUI

- Name context parameters `cx`, put `window` before `cx`, and put callbacks after
  `cx`. Inside entity callbacks, use the inner context.
- Use `Render` for stateful views and `RenderOnce` for one-shot components.
  Keep render methods free of I/O. Schedule blocking work off the foreground
  thread, then update entities on it.
- Use typed actions for shortcuts, typed events for session changes, and
  `cx.notify()` after state changes that affect rendering.
- Hold GPUI task and subscription handles for their intended lifetimes. Use
  weak entity handles when work should not keep a closed tab alive. Do not
  update an entity while it is already being updated.
- In GPUI tests that drive the dispatcher, use GPUI executor timers rather
  than external timers.

# Terminal and SSH

- Keep the terminal view independent of SSH transport and storage. Keep GPUI
  and database dependencies out of the SSH transport. Use the remote PTY for
  SSH sessions; do not spawn a local shell for them.
- Verify server identity before sending credentials. Require explicit approval
  of unknown host keys, and block changed keys until a separate trust update.
  Trigger trust and authentication prompts from typed events, not terminal
  output. Do not persist or log passwords, private key material, or transcripts.
- Preserve byte order and bound buffers; coalesce redraws, never terminal bytes.

# Development checks

Run these on Windows x64 before submitting GUI changes:

```text
cargo fmt --all -- --check
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo test --locked --workspace
cargo build --locked -p terminal_client
```

Keep this file focused on non-obvious, recurring rules. See the
[project scope](terminal-client-project.md) for architecture and
[Zed’s `AGENTS.md`](https://github.com/zed-industries/zed/blob/main/AGENTS.md)
for upstream conventions.
