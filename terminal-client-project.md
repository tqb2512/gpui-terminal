# Terminal Client: Project Scope and Architecture

## Goal

Build a local-first, Termius-style desktop SSH client in Rust and GPUI. The first release should let a user save a host, verify its identity, connect in one click, and work in multiple terminal tabs. The project follows Zed’s Rust and GPUI conventions without depending on Zed’s application crates.

## Release scope

| Stage | In scope |
| --- | --- |
| **MVP** | Windows; quick connect with `user@host`; saved profiles, search, and recent hosts; embedded SSH; key and password authentication; host-key verification; multiple tabs; resize, Unicode, color, scrollback search, and copy/paste. Installable without an account or a system `ssh` executable. |
| **SFTP phase one** | Browse remote folders, upload/download individual files, and open local copies with automatic save-back. Minimal Windows Files view. |
| **After MVP** | SSH agent support, import of common `~/.ssh/config` fields, jump hosts, remote-to-remote transfers, full transfer progress and cancellation, saved local port forwarding, and snippets that paste for review before execution. |
| **Later** | More desktop OSes, split panes, workspace restore, interactive local shells, and optional encrypted sync. Team vaults and mobile clients are separate product decisions. |

The MVP is **Windows-first and SSH-only**. Start with the recommended Windows x64 target, `x86_64-pc-windows-msvc`; decide the minimum supported Windows version before release. Use GPUI’s Windows backend (Win32 windowing and DirectWrite text).[2] Russh requests a PTY on the remote server, so the app does not need to launch a local ConPTY session or add `portable-pty`.[7]

## Architecture

```text
GPUI workspace → TerminalView → Entity<Terminal> ↔ session worker
                                                        ↔ Russh ↔ remote PTY and shell
Host profiles and approved public host keys ↔ SQLite
```

- **GPUI foreground:** Render tabs, the host list, and terminal snapshots; handle focus and input; present native connection prompts. A `TerminalView` observes its `Entity<Terminal>`, following Zed’s model/view split.[13]
- **Session worker:** Own terminal parsing state for the session, using `alacritty_terminal` behind an adapter.[4] Process ordered bytes, encode keystrokes using the current terminal mode, and publish render updates. Bound queues and scrollback; coalesce redraws, not terminal bytes.
- **SSH transport:** Run Russh on a shared Tokio runtime. Start with one SSH connection per tab. After verification and authentication, open a channel, request a remote PTY and shell, then send remote window-size updates when the viewport changes.[7] Closing a tab cancels pending prompts and stops the session. Reconnect creates a fresh connection without replaying commands.
- **Persistence:** Keep profiles, preferences, and approved public host keys in SQLite. Use an existing private key by reference or prompt for a password when needed. Never persist private keys, passwords, or transcripts in the MVP. Run database I/O off the UI thread.

### Host verification

The application decides which host keys to trust through Russh’s `check_server_key` hook.[6] Verify the server **before authentication**. For a new endpoint, show the host, port, and fingerprint and ask the user to verify it before approving. Store the approved public key against the endpoint. Reject a changed key; updating trust is a separate, deliberate action. Send trust and credential prompts as typed application events, never by interpreting remote terminal output.

## Zed-style workspace

```text
terminal-client/
├── AGENTS.md
├── Cargo.toml
├── Cargo.lock
├── rust-toolchain.toml
├── rustfmt.toml
└── crates/
    ├── terminal_client/
    │   ├── Cargo.toml
    │   └── src/{main.rs, workspace.rs}
    ├── terminal_view/
    │   ├── Cargo.toml
    │   └── src/terminal_view.rs
    ├── terminal/
    │   ├── Cargo.toml
    │   └── src/{terminal.rs, session.rs}
    ├── ssh/
    │   ├── Cargo.toml
    │   ├── src/{ssh.rs, auth.rs, trust.rs}
    │   └── tests/ssh_session.rs
    └── hosts/
        ├── Cargo.toml
        ├── migrations/0001_init.sql
        └── src/hosts.rs
```

`terminal_client` wires the workspace and host repository together. `terminal_view` depends on `terminal`, not on SQLite or Russh. `terminal` coordinates the emulator and session lifecycle; `ssh` implements transport without importing GPUI or SQLite. The app wires the SSH trust decisions to the host repository. SFTP uses `russh-sftp` on a separate channel of the same authenticated SSH connection; streaming, staged remote writes, and local edit tracking do not depend on the Files view.[9]

### Coding standards

- Use named library roots such as `[lib] path = "src/terminal_view.rs"`. Prefer `foo.rs` and `foo/` over `foo/mod.rs`. Extend an existing file unless a new logical component warrants a split.[10]
- Centralize dependency versions and lints in the workspace manifest. Use Rust 2024, pin a tested toolchain and compatible GPUI revision, and use a matching formatter edition.[11][15]
- Follow GPUI conventions: `window` before `cx`, `actions!` for application shortcuts, `cx.notify()` for visible changes, and typed events for lifecycle changes. Hold subscriptions and task handles for their intended lifetimes; use weak entity handles when a callback should not keep a closed tab alive.[10][13]
- Use descriptive names. Comment on non-obvious decisions rather than restating code. Prefer `?` or explicit error handling over production panics or silently discarded failures. Show actionable connection errors in the UI.[10]

## Implementation and release checks

1. **Prove a single tab:** Quick Connect → fingerprint approval → key authentication → interactive shell. Check resize in `vim` or `tmux` and responsiveness under sustained output.
2. **Add everyday workflows:** Save and search hosts, open multiple tabs, support password authentication, and implement cancellation, disconnect, and reconnect.
3. **Test and package on Windows:** Build and test on a Windows CI runner. Use unit tests for trust decisions and terminal behavior, GPUI tests for view and action behavior, and integration tests against an isolated SSH server for authentication, changed-key rejection, resize, and cleanup. Run `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets`, and `cargo test --workspace`. Check mixed-DPI monitor moves, IME input, Windows clipboard shortcuts, and keyboard focus switching on Windows.

**Release gate:** On a clean Windows machine without OpenSSH Client installed, a user can install the app, save a host, connect and switch tabs, close the app and retain the profile, and reconnect without retyping host details. A changed host key blocks the connection before any credentials are sent, and closing a tab releases its session resources.

## Sources

[2] https://github.com/zed-industries/zed/tree/main/crates/gpui
[4] https://docs.rs/alacritty_terminal
[6] https://docs.rs/russh/latest/russh/client/trait.Handler.html
[7] https://docs.rs/russh/latest/russh/struct.Channel.html
[9] https://docs.rs/russh-sftp/latest/russh_sftp
[10] https://github.com/zed-industries/zed/blob/main/AGENTS.md
[11] https://github.com/zed-industries/zed/blob/main/Cargo.toml
[13] https://github.com/zed-industries/zed/blob/main/crates/terminal_view/src/terminal_view.rs
[15] https://github.com/zed-industries/zed/blob/main/rustfmt.toml
