# Phase-one SSH Connections Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Ship a Windows saved-host manager and multiple independent interactive SSH tabs using password or private-key authentication.

**Architecture:** `ssh` owns the remote connection, `hosts` owns persistence, and `terminal` owns emulation. A headless controller in `terminal_client` coordinates them; the GPUI workspace and `terminal_view` are replaceable consumers of the core.

**Tech Stack:** Rust 2024, Rust 1.100.0, Russh, Tokio, bundled SQLite via rusqlite, `alacritty_terminal`, GPUI 0.2.2, Windows x64 MSVC.

**Spec:** [Approved phase-one design](../specs/2026-11-18-ssh-connections-design.md).

## Global Constraints

- Follow `AGENTS.md`; centralize dependencies and lints in the workspace, use named library roots, and update `Cargo.lock` with dependency changes. No Zed application crates.
- Use embedded SSH and a remote PTY. No local `ssh` executable or local shell. Keep GPUI and SQLite out of the transport.
- Verify the server before loading private-key material or asking for secrets. Never persist or log secrets or transcripts. Unknown, refused, changed, stale, and cancelled trust decisions are distinct outcomes.
- Bound queues with backpressure, preserve terminal byte order, and coalesce redraws, not terminal data. Keep I/O off the GPUI foreground thread.
- Keep the existing Windows window and CI artifact. No redesign, extra platforms, installer, SSH config import, agent, jump hosts, SFTP, forwarding, sync, or scrollback search.

## Review Focus

1. Forgotten trust while a stale prompt is still visible: old approval cannot restore trust; no credential prompt appears for that attempt (Task 3).
2. Two tabs approve different keys for the same endpoint: only the first key wins; the other attempt fails before auth (Tasks 1 and 3).
3. DNS aliases, trailing dots, IPv6 spellings, and nondefault ports: trust is neither accidentally shared nor duplicated (Task 1).
4. A slow display or large paste: the session stays bounded, does not duplicate or drop accepted input/output, and can still close (Tasks 2 and 4).
5. Switching tabs while submitting a password: never route a response to the newly selected tab or to a replacement attempt (Tasks 3 and 5).

---

## File map

| Files | Responsibility |
| --- | --- |
| `Cargo.toml`, `Cargo.lock`, affected `crates/*/Cargo.toml` | Workspace-managed dependencies and crate edges, added with the task that needs them |
| `crates/ssh/src/ssh.rs`, `crates/ssh/src/target.rs`, `crates/ssh/src/client.rs` | Shared connection target, typed SSH interface, and Russh implementation |
| `crates/hosts/src/hosts.rs`, `crates/hosts/migrations/0001_init.sql` | Profiles and persistent host trust |
| `crates/ssh/tests/ssh_session.rs` | Isolated loopback server fixture and client integration tests |
| `crates/terminal/src/terminal.rs` | Headless emulator, snapshots, selection and input encoding |
| `crates/terminal_client/src/terminal_client.rs`, `crates/terminal_client/src/connections.rs` | GPUI-independent controller, runtime work, trust coordination, and tab lifecycle |
| `crates/terminal_client/src/main.rs`, `crates/terminal_client/src/workspace.rs`, `crates/terminal_client/src/profile_form.rs` | Windows startup, host manager, tab strip, and forms/prompt bindings |
| `crates/terminal_view/src/terminal_view.rs` | Focus, terminal rendering, selection, clipboard, and viewport measurement |
| `README.md` | Usage, changed-key recovery, and the Windows delivery checklist |

### Task 1: Validated endpoints, SQLite profiles, and trust records

**Files:** `crates/ssh/src/{ssh,target}.rs`, `crates/hosts/src/hosts.rs`, `crates/hosts/migrations/0001_init.sql`, and relevant manifests/lockfile. Put tests beside the code.

**Produces:**

- `ssh::Endpoint::new(host: &str, port: u16) -> Result<Endpoint, TargetError>`, `host(&self) -> &str`, and `port(&self) -> u16`. `ssh::Target::new(endpoint: Endpoint, username: &str) -> Result<Target, TargetError>`, `Target::parse(input: &str) -> Result<Target, TargetError>`, `endpoint(&self) -> &Endpoint`, and `username(&self) -> &str`. Keep fields private so callers can't bypass normalization. `ssh::AuthMethod::{Password, PrivateKey { path: Option<PathBuf> }}` holds configuration, not credentials.
- `hosts::Repository::open(path: &Path) -> Result<Repository, StoreError>`; `create(&mut self, profile: &ProfileDraft) -> Result<ProfileId, StoreError>`, `update(&mut self, id: ProfileId, profile: &ProfileDraft) -> Result<(), StoreError>`, `delete(&mut self, id: ProfileId) -> Result<(), StoreError>`, `list(&self, search: &str) -> Result<Vec<Profile>, StoreError>`, and `record_connected(&mut self, id: ProfileId) -> Result<(), StoreError>`. `ProfileId` is a typed `i64`; `ProfileDraft` has `label: String`, `target: Target`, and `auth: AuthMethod`; `Profile` adds `id` and optional last-connected time. Allow a late `record_connected` for a deleted profile to be a no-op.
- `Repository::check_key(&self, endpoint: &Endpoint, key: &[u8]) -> Result<KeyCheck, StoreError>`, with `KeyCheck::{Unknown { revision: u64 }, Match, Changed}`. `Repository::approve_key(&mut self, endpoint: &Endpoint, key: &[u8], revision: u64) -> Result<Approval, StoreError>`, with `Approval::{Approved, Changed, Stale}`; `Repository::forget_key(&mut self, endpoint: &Endpoint) -> Result<(), StoreError>` increments the revision in the same transaction as key removal.

- [ ] **RED:** Write `quick_connect_normalizes_without_merging_dns_identities`: `"ops@BUILD.Example"` resolves to `("ops", "build.example", 22)`; `"ops@build.example."` is distinct; `"ops@[2001:0DB8:0::1]:2222"` normalizes to `("ops", "2001:db8::1", 2222)`. Reject unknown format, missing username, empty host, or port zero; assert distinct ports and DNS aliases stay distinct.
- [ ] **RED:** Write `profiles_survive_database_reopen` (round-trip create/edit, literal and case-insensitive search on label/host/username, recent order, delete), `approval_never_overwrites_a_different_key`, and `forget_rejects_old_revision`, using two SQLite connections to a temporary file. Test cross-profile trust for the same endpoint and verify a literal `%` in search is not a wildcard.
- [ ] **Run RED:** `cargo test -p ssh -p hosts`. Expect missing target and repository interfaces.
- [ ] **Implement:** Route typed endpoints to the repository through an `ssh` dependency; keep SQLite out of `ssh`. Use bundled SQLite, numbered initialization, bound SQL parameters, one atomic writer transaction to compare revision and approve, and a short busy timeout. Persist only profile fields, last-success time, public-key bytes and trust revision. Keep a revision row even when there is no trusted key. Sort saved profiles deterministically with most recent successful connections first.
- [ ] **Verify and commit:** `cargo test --locked -p ssh -p hosts`; expect both suites to pass. Commit the task files and lockfile as `Add SSH endpoints and saved host storage`.

### Task 2: Embedded SSH with trust-gated auth and a remote PTY

**Files:** `crates/ssh/src/{ssh,client}.rs`, `crates/ssh/tests/ssh_session.rs`, and relevant manifests/lockfile.

**Consumes:** `Target`, `Endpoint`, and `AuthMethod` from Task 1.

**Produces:** `ssh::connect(runtime: &tokio::runtime::Handle, target: Target, auth: AuthMethod, size: PtySize) -> Session`; `PtySize { columns: u16, rows: u16, pixel_width: u16, pixel_height: u16 }`; `Session::recv(&mut self) -> Option<Event>` (async), `Session::try_send(&self, input: Input) -> Result<(), SendInputError>` (a full queue returns the unsent input), and `Session::cancel(&self)`. Dropping `Session` cancels it. `Event::{VerifyHost { key, fingerprint, reply }, Password { reply }, KeyPath { reply }, Passphrase { reply }, Connected, Output(Vec<u8>), Ended(Result<(), SessionError>)}` carries single-use replies: trust `oneshot::Sender<bool>`, passwords and passphrases `oneshot::Sender<Zeroizing<String>>`, and key paths `oneshot::Sender<PathBuf>`. `Input::{Bytes(Vec<u8>), Resize(PtySize)}`. The event sender never logs values, and terminal output cannot produce prompt events.

- [ ] **RED:** Write loopback tests against an ephemeral localhost listener with generated keys: `password_requires_approved_host_before_auth`, `key_login_opens_remote_pty`, `encrypted_key_requests_passphrase_after_verification`, `rejected_host_never_authenticates`, `rotated_host_key_never_authenticates`, `closing_pending_session_releases_socket`, and `two_sessions_forward_independent_io_and_resize`. For rotation, reconnect to the same endpoint with a different host key and an existing pin, then assert no credential prompt and no server-side auth call. Assert shell/resize requests reach the intended channel, and the second session still echoes after the first closes.
- [ ] **RED:** Write `backpressure_keeps_byte_order_and_allows_cancel`: hold the consumer, force a full queue, recover the returned unsent input in order, drain output, then independently assert cancellation while output remains stalled. Use bounded waits so a regression cannot hang CI.
- [ ] **Run RED:** `cargo test -p ssh --test ssh_session`; expect missing transport API.
- [ ] **Implement:** Use Russh defaults for algorithms and `xterm-256color` for the remote PTY. Emit the SHA-256 fingerprint and public key from `check_server_key`, await the decision, pin the accepted key for the connection, and fail closed on rekey mismatch. Only after trust succeeds, ask for the configured authentication method's inputs and load any key off the async executor; request a passphrase only when the key is encrypted. Open the session channel, await PTY and shell acceptance, handle bytes/extended data/EOF and resize, and report disconnect/auth/transport errors without secrets. Bound queue item sizes as well as counts; cancellation must not wait for an event slot or a prompt response.
- [ ] **Verify and commit:** `cargo test --locked -p ssh --test ssh_session`; expect loopback tests to pass without an external daemon. Commit as `Implement trust-gated embedded SSH sessions`.

### Task 3: Headless connection manager and tab lifecycle

**Files:** `crates/terminal_client/src/{terminal_client,connections}.rs`, `crates/terminal_client/Cargo.toml`; add tests in the controller module using controlled session events and a real temporary database.

**Consumes:** Tasks 1 and 2. The binary and Windows GPUI dependency stay target-gated; this named library builds and tests on Linux.

**Produces:** `Connections::open(runtime: &tokio::runtime::Handle, directory: &Path) -> Result<(Connections, tokio::sync::mpsc::Receiver<ConnectionEvent>), ManagerError>`; `start(&self, spec: ConnectionSpec) -> Result<TabId, ManagerError>`, `reconnect(&self, tab: TabId) -> Result<(), ManagerError>`, `close(&self, tab: TabId) -> Result<(), ManagerError>`, `forget_host(&self, endpoint: Endpoint) -> Result<(), ManagerError>`, `respond(&self, prompt: PromptId, response: PromptResponse) -> Result<(), ManagerError>`, `list_profiles(&self, search: &str) -> Result<Vec<Profile>, ManagerError>`, `save_profile(&self, id: Option<ProfileId>, draft: ProfileDraft) -> Result<ProfileId, ManagerError>`, and `delete_profile(&self, id: ProfileId) -> Result<(), ManagerError>`. These calls are async. `ConnectionSpec { label: String, target: Target, auth: AuthMethod, profile_id: Option<ProfileId> }`. The manager uses the supplied runtime for workers and its bounded event stream carries tab and attempt identifiers. `PromptId` uniquely identifies a tab, attempt, and prompt. `PromptResponse::{Trust(bool), Secret(Zeroizing<String>), KeyPath(PathBuf)}` only matches a corresponding typed prompt; closure or a stale/mismatched ID produces an error without consuming another prompt.

- [ ] **RED:** Write `forget_cancels_pending_without_touching_established`, `simultaneous_different_key_approval_rejects_loser_before_auth`, `closing_a_tab_discards_its_prompt`, and `stale_reply_cannot_answer_reconnect`. In the forget test, forget while trust, password or passphrase is pending; assert the previous prompt ID cannot be accepted, the trust row cannot be restored by an old response, and the established tab and unrelated pending attempts are untouched.
- [ ] **RED:** Write `reconnect_preserves_target_and_remembers_only_tab_key_path`: edit/delete the source profile, reconnect with a fresh attempt ID, assert target and selected path are reused, and assert no secret or old input is replayed. Exercise a missing/deleted profile on late success.
- [ ] **Run RED:** `cargo test -p terminal_client --lib`; expect missing controller or failing lifecycle assertions.
- [ ] **Implement:** Open `%LOCALAPPDATA%\GPUI Terminal Client` from the app (Task 5) and `connections.sqlite3` from `Connections::open`. Serialize endpoint trust decisions, cancel/forget and setup transitions within one controller so no stale response can advance an attempt; apply the SQLite revision guard even if another process wrote to the database. Fail closed on trust I/O failures. Keep per-attempt cancellation independent of UI and buffering. On `Connected`, mark the saved profile recent without treating a reporting failure as an authentication failure. UI connection-management calls and event delivery must not block on database access, socket I/O, or rendering.
- [ ] **Verify and commit:** `cargo test --locked -p terminal_client --lib`; expect lifecycle and storage tests to pass. Commit as `Coordinate trusted connections and independent tabs`.

### Task 4: Terminal state and bounded I/O

**Files:** `crates/terminal/src/terminal.rs`, `crates/terminal_client/src/connections.rs`, and relevant manifests/lockfile. Put tests beside the emulator and the controller worker.

**Consumes:** Task 2 `Input` and `PtySize`, and Task 3 tab/attempt lifecycle.

**Produces:** `terminal::Terminal::new(columns: u16, rows: u16) -> Terminal`, `advance(&mut self, bytes: &[u8])`, `resize(&mut self, columns: u16, rows: u16)`, `snapshot(&self) -> Snapshot`, `encode_key(&self, key: KeyStroke) -> Option<Vec<u8>>`, `encode_paste(&self, text: &str) -> Vec<u8>`, `scroll(&mut self, lines: i32)`, and `select(&mut self, selection: Option<Selection>)`; `selected_text(&self) -> Option<String>`. `Snapshot` contains visible grid cells with graphemes and style, selection, cursor, and grid geometry. `terminal::TerminalInput::{Key(KeyStroke), Paste(String), Resize { columns: u16, rows: u16, pixel_width: u16, pixel_height: u16 }, Scroll(i32), Select(Option<Selection>)}` keeps SSH-specific types out of the view. `Connections::input(&self, tab: TabId, input: TerminalInput) -> Result<(), InputError>` is nonblocking. Controller events carry a tab/attempt-tagged redraw notification; `Connections::snapshot(&self, tab: TabId) -> Option<Arc<Snapshot>>` exposes the latest completed frame without touching the emulator, and `selected_text(&self, tab: TabId) -> Option<String>` exposes the latest selected text.

- [ ] **RED:** Write `split_utf8_and_escape_sequences_render_in_order` (split a wide Unicode scalar and SGR across reads; assert expected cells, color, and cursor), `history_is_bounded_and_copy_excludes_cell_padding`, and `key_and_paste_follow_terminal_modes` (assert application-cursor arrows and bracketed paste escapes only in those modes).
- [ ] **RED:** Write `stalled_view_coalesces_redraws_but_not_bytes` (feed multiple chunks to two sessions and compare final snapshots); `oversized_paste_is_rejected_without_partial_input` (assert a visible retryable size error, zero partial bytes sent); and `close_resolves_even_when_input_or_output_is_full`.
- [ ] **Run RED:** `cargo test -p terminal -p terminal_client --lib`; expect missing terminal API or failing tests.
- [ ] **Implement:** Use one alacritty parser/emulator per tab, with 10,000 lines of history and nonzero dimensions. Encode accepted input in worker order against the current modes; cap paste at 1 MiB and chunk it into ordered writes of at most 16 KiB without interleaving later input. Never silently drop accepted bytes; a queue-full error leaves ownership with the caller or displays a retry path for an input not accepted. Publish a latest frame and at most one outstanding redraw notification per tab; the slowest consumer must not block unrelated tabs or shutdown.
- [ ] **Verify and commit:** `cargo test --locked -p terminal -p terminal_client --lib`; expect emulator and controller suites to pass. Commit as `Render and drive bounded per-tab terminal state`.

### Task 5: Minimal Windows connection manager and tab view

**Files:** `crates/terminal_client/src/{main,workspace,profile_form}.rs`, `crates/terminal_view/src/terminal_view.rs`, and relevant manifests/lockfile.

**Consumes:** Tasks 1–4. Test workspace bindings without hitting live endpoints where possible; use a GPUI timer for dispatcher tests.

**Produces:** `Workspace::new(cx: &mut Context<Self>) -> Workspace`; use existing startup to build it and show initial storage status. `TerminalView` becomes a focusable `Render` entity receiving a snapshot and emitting typed `TerminalInput`; it depends only on `terminal`, not the controller, SSH, or SQLite. Convert pixel sizes to nonzero viewport dimensions before forwarding.

- [ ] **RED:** Write a Windows workspace test `prompt_submit_uses_its_own_id_after_tab_switch`: render/bind a prompt for tab A, switch to B or reconnect A, submit the old binding, and assert B receives nothing. Check `profile_editor_validates_and_clears_secrets_on_cancel` with invalid host/port and a masked password prompt; assert no save/connect request on invalid fields, and cancel removes the prompt buffer.
- [ ] **Run RED:** `cargo test --locked -p terminal_client --bin terminal_client` on Windows; expect missing bindings or failing assertions. On Linux, compile the Windows test targets with `cargo xwin check --target x86_64-pc-windows-msvc -p terminal_client --all-targets` once dependencies are in the lock; record native test execution as pending.
- [ ] **Implement:** Keep the incumbent window and styling. Own the Tokio runtime at application scope; surface startup errors and drop the runtime after GPUI exits. Provide label/endpoint/auth profile fields, search/recent ordering, Quick Connect, separately confirmed delete and forget actions, a scrollable tab strip, reconnect/close actions, and a tab-local status and prompt area. Use GPUI input handling with focus, masked secret fields, and normal editing; clear secret buffers when submitted, closed, or invalidated. Bind each prompt's exact `PromptId` in the submission handler. Draw terminal cells with monospace metrics, Unicode width, color, cursor and selection; forward keyboard input, clipboard actions, and measured resize to the current tab. Keep GPUI entity callbacks on the foreground; hold asynchronous task lifetimes in the workspace and update it through weak handles.
- [ ] **Verify and commit:** Run the Windows test from the RED step, or cross-check and explicitly record the runtime gap. Run `cargo xwin build --locked --target x86_64-pc-windows-msvc -p terminal_client` on Linux or `cargo build --locked -p terminal_client` on Windows. Commit as `Wire Windows host manager and SSH tabs`.

### Task 6: Usage, Windows verification, and final review

**Files:** `README.md`; code only if a check exposes a defect. Keep the existing workflow and Windows downloadable artifact.

- [ ] **Document:** Replace the bootstrap copy with instructions for Quick Connect, saving/editing/deleting/searching hosts, key and password login, and separate host-key recovery. Explain independent fingerprint verification and that only key paths are saved. Add the smoke checklist below.
- [ ] **Run automated release checks:** On Windows x64 run `cargo fmt --all -- --check`, `cargo clippy --locked --workspace --all-targets -- -D warnings`, `cargo test --locked --workspace`, and `cargo build --locked -p terminal_client`. On Linux run headless format/lint/tests and the locked `cargo xwin build` from Task 5; do not report those as native runtime checks.
- [ ] **Smoke-test on Windows:** Quick Connect to a controlled server, deny and then verify a new host fingerprint, use both auth methods (including an encrypted key), save a profile and restart, switch between two active tabs under output, resize a full-screen program, copy/paste, close one tab and keep the other usable, and verify the changed-key block and separate forget flow. Check keyboard focus and small/high-DPI layouts. Confirm no local SSH executable is required.
- [ ] **Review and commit:** Review the branch against the spec, focusing on trust-before-auth and race tests; capture GUI evidence if a Windows desktop is available. Record any unchecked Windows items explicitly. Commit README and any reviewed verification fixes with narrowly scoped messages.
