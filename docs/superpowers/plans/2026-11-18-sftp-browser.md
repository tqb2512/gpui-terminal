# SFTP Browser Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Browse a remote host, upload and download files, and open an editable local copy whose saves go back to the original remote file.

**Architecture:** `ssh` provides an on-demand SFTP channel on the existing verified SSH connection, plus independent read and staged-write handles. The headless app core binds those handles to a connection attempt and owns local transfers, edit tracking, and their safety policies. A minimal GPUI view renders that state and calls a separate Windows dialog and launch adapter; either can be replaced without changing the core.

**Tech Stack:** Rust 2024 (toolchain 1.100.0), Tokio, Russh 0.65, `russh-sftp` 3.0.1, GPUI 0.2.2, Windows x64 MSVC. Use workspace versions of `tempfile`, `sha2` 0.11, and `windows` 0.61 where needed.

**Spec:** [Phase-one SFTP browser design](../specs/2026-11-18-sftp-browser-design.md).

## Global Constraints

- Follow `AGENTS.md`; centralize dependency versions and lints, update `Cargo.lock`, and do not depend on Zed application crates. Keep GPUI and SQLite out of `ssh`, and SFTP out of the terminal emulator.
- Verify and authenticate once through the existing SSH path; no local shell, external SFTP tool, extra credential prompts, or edit replay on reconnect.
- Keep I/O off the GPUI foreground thread. Keep one foreground Files operation per tab; the edit worker serializes its own writes and never queues unbounded saves.
- Limit operations to regular files and navigation to known directories. Ordinary transfers are create-only. Editable copies are limited to four active copies app-wide and 32 MiB per file.
- Local edit recovery lives under `%LOCALAPPDATA%\GPUI Terminal Client\edits`. Retain copies after launch and never log file bytes, credentials, or transcripts.
- Do not build remote-to-remote transfer, recursive operations, drag-and-drop, rename/delete, transfer queue, or resume.
- Complete Tasks 1–4 and their headless checks before touching the view. Task 5 uses existing controls and native dialogs; no custom components, layout system, styling pass, or new UX flows.

## Review Focus

1. An editor replaces a local file or saves again while upload is running: only complete versions publish and the latest save is detected (Task 4).
2. Another user changes the remote file without changing its size or timestamp: the edit pauses and neither version is discarded (Task 4).
3. A disconnect or reconnect overlaps publish: report unknown outcome as appropriate, preserve the local copy, and never apply the old edit to the new attempt (Tasks 2, 3, and 4).
4. A destination appears after a picker returns or during transfer: keep the existing file byte-for-byte and clean up the stage (Tasks 2 and 3).
5. A remote name resembles a Windows shortcut, alternate stream, or path traversal, or a watched local file becomes a junction/link: do not launch or read an unsafe path (Tasks 2 and 4).

## File Map

| Files | Responsibility |
| --- | --- |
| `Cargo.toml`, `Cargo.lock`, `crates/ssh/Cargo.toml`, `crates/terminal_client/Cargo.toml` | SFTP, hashing, Windows API bindings, and filesystem support |
| `crates/ssh/src/{ssh,client,sftp}.rs`, `crates/ssh/tests/ssh_session.rs` | Authenticated channel, remote paths, and SFTP operations; extend the existing loopback fixture |
| `crates/terminal_client/src/{terminal_client,connections,connections_test,file_transfer}.rs` | Attempt ownership and local transfer policy, testable without GPUI |
| `crates/terminal_client/src/remote_edit.rs` | Local copy, stable-save detection, conflict checks, and recovery state |
| `crates/terminal_client/src/{main,workspace,files_view,windows_open_with}.rs` | Minimal Windows Files view and separate native attachment/launch adapter |
| `README.md`, `terminal-client-project.md` | Usage, caveats, recovery, and delivered scope |

### Task 1: Open SFTP on the authenticated connection

**Files:** `crates/ssh/src/{ssh,client,sftp}.rs`, `crates/ssh/tests/ssh_session.rs`, workspace and SSH manifests, `Cargo.lock`.

**Produces:** `Session::sftp_opener(&self) -> SftpOpener`; cloneable `SftpOpener::open(&self) -> Result<Arc<SftpClient>, SftpError>` (async); `SftpClient::supports_atomic_replace(&self) -> bool`. Expose typed errors for `NotConnected`, `SubsystemUnavailable`, `Disconnected`, `Timeout`, and protocol failure; do not expose unfiltered server messages.

- [ ] **RED:** Extend `ssh_session.rs` with `sftp_opens_once_without_reauthentication`: open SFTP twice after `Connected`, assert `Arc::ptr_eq`, a single auth and subsystem request, and live PTY echo. In `sftp_refusal_preserves_shell`, reject the subsystem and assert typed failure plus working shell. In `sftp_opener_expires_with_session`, cancel the SSH session while SFTP startup is stalled and assert the request ends and no held opener can open a channel afterward. Use bounded waits.
- [ ] **Run RED:** `cargo test -p ssh --test ssh_session sftp_`; expect missing opener or failing assertions.
- [ ] **Implement:** After the existing shell is accepted, serve lazy open requests from a bounded mailbox in a separate future within the connection's cancellation scope. Request `sftp` with a reply on a distinct channel and initialize `RawSftpSession` over `Channel::into_stream`. Retain the successful SFTP handle for that connection; do not cache failed opens or let them end the shell. Use a close path for denied, stalled, or disconnected subsystem channels. Read `posix-rename@openssh.com` version `1` from server negotiation; no shell commands.
- [ ] **Verify and commit:** `cargo test --locked -p ssh --test ssh_session`; expect new and existing SSH tests to pass. Commit as `Open SFTP on authenticated SSH tabs`.

### Task 2: Provide composable remote file operations

**Files:** `crates/ssh/src/{ssh,sftp}.rs`, `crates/ssh/tests/ssh_session.rs`; add focused path tests beside `sftp.rs`.

**Consumes:** `SftpClient` from Task 1.

**Produces:** `RemotePath::absolute(path: &str) -> Result<RemotePath, SftpError>`, `child(&self, name: &str) -> Result<RemotePath, SftpError>`, `parent(&self) -> Option<RemotePath>`, `as_str(&self) -> &str`; `RemoteEntry { name: String, kind: RemoteKind, size: Option<u64> }`, with `RemoteKind::{Directory, File, Symlink, Other}`. Async `SftpClient::{home(&self) -> Result<RemotePath, SftpError>, list(&self, path: &RemotePath) -> Result<Vec<RemoteEntry>, SftpError>, metadata(&self, path: &RemotePath) -> Result<RemoteMetadata, SftpError>, open_read(&self, path: &RemotePath) -> Result<RemoteReader, SftpError>, stage_write(&self, path: &RemotePath) -> Result<StagedRemoteWrite, SftpError>}`. `RemoteMetadata` carries kind, size, mtime, mode, uid, and gid when supplied. The reader exposes async `read(&mut self, buffer: &mut [u8]) -> Result<usize, SftpError>` and `close(self) -> Result<(), SftpError>`; the stage exposes async `write_all(&mut self, bytes: &[u8])`, `seal(&mut self, preserve: Option<&RemoteMetadata>)`, `publish(&mut self, mode: PublishMode)`, and `abort(self)`, each returning `Result<(), SftpError>`. `PublishMode::{CreateOnly, AtomicReplace}`; `SftpError` distinguishes `AlreadyExists`, `NotFound`, `InvalidPath`, unsupported ops, and `OutcomeUnknown` during publish.

- [ ] **RED:** `remote_paths_do_not_reinterpret_invalid_children` rejects `""`, `.`, `..`, `a/b`, `a\b`, and control characters as a child; `/` has no parent and `/home/ops` joins `résumé.txt` without case folding. Add `sftp_remote_listing_is_bounded` (10,001 entries rejects, 10,000 succeeds), `sftp_create_only_keeps_late_destination` (inject `old` after staging `new`; verify destination is `old` and no stage remains after abort), and `sftp_publish_timeout_is_unknown` (server commits but withholds its response; assert no second publish). Test that links and unknown types are listed but cannot be read as regular files.
- [ ] **Run RED:** `cargo test -p ssh`; expect the path or file APIs to be missing or assertions to fail.
- [ ] **Implement paths and listing:** Resolve `home` with `REALPATH "."`; model absolute `/` paths independent of local `Path`. Reject invalid path components and skip unsupported listing names without interpreting them. Use `LSTAT`/`FSTAT` and type checks; validate returned names and ignore `.`/`..`. Bound listing at 10,000 entries using incremental `READDIR`, bound protocol packets and each read/write buffer, and close remote handles on success and error.
- [ ] **Implement staged I/O:** Read/write in chunks of at most 16 KiB. Create a random exclusive sibling with restrictive initial permissions. `seal` closes the file after writes, verifies promised mode/owner/group if applicable, and must succeed before `publish`. Use SFTP v3 no-replace `RENAME` for create-only; never implement it by deleting an existing destination or using POSIX rename. Use the negotiated OpenSSH extension for `AtomicReplace`, never a delete-then-rename fallback. Mark ambiguous publish failures separately, and abort uncommitted stages explicitly while reporting cleanup failure. Stage and reader depend only on remote handle and path, not local disk or UI.
- [ ] **Verify and commit:** `cargo test --locked -p ssh`; expect path, file, transport and failure-injection tests to pass. Commit as `Add bounded remote paths and staged SFTP writes`.

### Task 3: Bind the browser and ordinary transfers to an attempt

**Files:** `crates/terminal_client/src/{terminal_client,connections,connections_test,file_transfer}.rs`, app and workspace manifests, `Cargo.lock`.

**Consumes:** `SftpOpener`, `SftpClient`, `RemotePath`, and the reader/stage APIs.

**Produces:** Async `Connections::files(&self, attempt: AttemptId) -> Result<AttemptFiles, FileError>`; `AttemptFiles` holds the remote handle, attempt ID and attempt-scoped cancellation token. Its async operations are `home(&self) -> Result<RemotePath, FileError>`, `list(&self, path: &RemotePath) -> Result<Vec<RemoteEntry>, FileError>`, `download(&self, remote: &RemotePath, destination: &Path) -> Result<(), FileError>`, and `upload(&self, source: &Path, remote: &RemotePath) -> Result<(), FileError>`. Expose `supports_atomic_replace(&self) -> bool`; make the existing handle, cancellation token, and path available inside the headless crate for Task 4.

- [ ] **RED:** `files_require_a_current_connected_attempt` rejects starting/ended attempts and an `AttemptId` superseded by reconnect. `download_keeps_existing_destination` writes staged `new` beside a late `old` destination, asserts `old` stays and the temporary sibling disappears. `transfer_cancellation_keeps_original_and_ends_promptly` interrupts a copy after a partial chunk and asserts no partial destination; test with a staged local file and the loopback server for the remote counterpart.
- [ ] **Run RED:** `cargo test -p terminal_client --lib`; expect missing controller API or failing tests.
- [ ] **Implement attempt binding:** Attach the opener when the existing `Connected` path applies to the current attempt. Clone it and the attempt token under the controller lock and release the lock before open or copy. Cancel in-flight work on close, terminal end, or reconnect. Do not use the selected tab to identify any operation. Ensure no stage can begin publish after cancellation; report unknown outcome if a publish request was already sent.
- [ ] **Implement local policy:** Download to a same-directory `tempfile::NamedTempFile` and use `persist_noclobber` after streaming and flushing. Upload from an explicitly selected, regular local file into `stage_write`, then use `CreateOnly`; validate the remote basename from the picker as one component. Refuse links or non-regular files and return operation-specific, actionable errors without showing private contents. I/O stays on Tokio or the blocking pool, not on the GPUI thread.
- [ ] **Verify and commit:** `cargo test --locked -p terminal_client --lib`; expect lifecycle and transfer tests to pass. Commit as `Bind local file transfers to SSH attempts`.

### Task 4: Track editable copies and save back safely

**Files:** `crates/terminal_client/src/{terminal_client,connections,file_transfer,remote_edit}.rs`, workspace and app manifests, `Cargo.lock`.

**Consumes:** `AttemptFiles`, metadata/read/stage APIs, and the app-local data directory.

**Produces:** Async `AttemptFiles::prepare_edit(&self, remote: &RemotePath, edits_root: &Path) -> Result<PreparedEdit, FileError>`; `PreparedEdit::local_path(&self) -> &Path`, `PreparedEdit::start(self) -> ActiveEdit`, and async `ActiveEdit::run(self, runtime: &tokio::runtime::Handle, stop: CancellationToken, status: tokio::sync::watch::Sender<EditStatus>)`. Use `EditStatus::{Watching, Saving, Saved, Paused(EditPause), Finished}`; `EditPause` distinguishes conflict, disconnect, unverifiable/unsupported change, I/O failure, and unknown publish outcome. `Connections` shares the four app-wide edit slots with its `AttemptFiles` handles; a prepared or active edit holds a slot until it ends or is dropped.

- [ ] **RED:** `edit_tracks_replace_and_save_during_upload` starts with `a`, samples stable `b`, saves `c` during publish, and asserts complete `b` then `c` are uploaded; unchanged `c` produces no third upload. `edit_refuses_remote_same_size_same_mtime_change` changes remote `a` to `z` with equal length/mtime; assert no publish and local `b` survives. `edit_limits_duplicate_and_parallel_copies` verifies that the same attempt/path cannot open twice, a fifth app-wide reservation starts no download, and dropping a prepared edit frees its slot. Add `edit_discards_unlaunched_copy_but_keeps_launched_copy`, `edit_rejects_unsafe_local_extension`, and `edit_stops_on_disconnect_without_replaying`; pass a controllable tick/loopback response for timing rather than sleeping. Add Windows test `edit_rejects_local_reparse_point` using a reparse fixture or an equivalent handle-attribute test plus an on-machine smoke case.
- [ ] **Run RED:** `cargo test -p terminal_client --lib remote_edit`; expect missing edit workflow or failing cases.
- [ ] **Implement preparation:** Reserve a core edit slot by `(AttemptId, RemotePath)` before I/O, without holding the slot registry lock during I/O. Check atomic replace support and that the server reports verifiable mode, owner, group and regular-file type. Limit streamed bytes to 32 MiB even if listed size is absent or wrong. Compare remote attributes before/after initial download and store a SHA-256 digest of the downloaded bytes. Use a randomly created directory under the per-user edits root; do not reuse directory names. Generate `edit.<extension>` for `txt`, `log`, `md`, `toml`, `ini`, `cfg`, `conf`, `json`, `yaml`, `yml`, `xml`, `csv`, `tsv`, `pdf`, `docx`, `xlsx`, `png`, or `jpg` (case-insensitive); use `edit.txt` for safe extensionless names, and refuse ambiguous names including `:`, trailing dot/space, or other Windows-invalid characters. Prepared copies clean up on drop; `start` transfers them to recovery retention.
- [ ] **Implement save detection:** Poll the path every second using no-follow file opens (on Windows, open the reparse point itself, inspect handle attributes, and reject reparse points). Matching consecutive content digests separated by a tick qualify an immutable local snapshot; transient missing/locked states reset the candidate. Never upload a changing or oversized copy. Stay single-flight and resample after publish. Use bounded streaming; do not allocate a whole file or enqueue every save.
- [ ] **Implement publication and recovery:** Stage the snapshot remotely; immediately before publish, check the original's type and basic attributes, stream its digest with the edit size bound, and recheck attributes afterward. Pause if anything is changed or unverifiable; never overwrite on conflict. After an acknowledged publish advance the baseline from the staged digest and fresh remote attributes, or pause if inaccessible. Stop on an explicit stop or attempt cancellation; retain every launched copy and clean up only staging files. No restart or reconnect recovery worker.
- [ ] **Verify and commit:** `cargo test --locked -p terminal_client --lib`; expect edit and transfer tests to pass. Commit as `Save local edits back to their verified remote file`.

### Task 5: Wire a minimal Files view and Windows Open With

**Files:** `crates/terminal_client/src/{main,workspace,files_view,windows_open_with}.rs`, app and workspace manifests, `Cargo.lock`, `README.md`, `terminal-client-project.md`.

**Consumes:** Tasks 1–4.

**Produces:** `FilesView::new(attempt: AttemptId, connections: Connections, cx: &mut Context<Self>) -> Self`; per-tab Files state with `AttemptId`, remote directory and selection, one foreground operation slot, and edit status subscribed for the active lifetime. `windows_open_with::mark_remote_attachment(path: &Path) -> Result<(), OpenWithError>` and `windows_open_with::choose_application(path: &Path, parent: HWND) -> Result<OpenWithOutcome, OpenWithError>`, where `OpenWithOutcome::{Opened, Cancelled}`. Keep filesystem work outside rendering.

- [ ] **RED:** Add Windows test `files_discards_stale_results`: a picker or listing issued on attempt A completes after reconnect to B; B's panel and files are unchanged and no copy starts against B. `open_with_cancel_does_not_watch_or_keep_unlaunched_copy`: inject a cancelled launcher result and assert zero edit workers and cleanup. Add native picker/manual cases to the existing Windows smoke checklist.
- [ ] **Run RED:** On Windows x64 run `cargo test -p terminal_client --bin terminal_client`; expect missing Files/launch state or test failure.
- [ ] **Implement the view:** Add **Terminal / Files** to live SSH tabs. Reuse current controls and focus helpers: a path label, Up/Refresh, a scrollable folder/file list, upload/download/open buttons, and a status line with Show local copy and Finish editing. Use the existing GPUI path prompts for upload/download; suggest `download` if the remote name is not a valid local filename. Capture attempt and path before opening a dialog. Use request IDs and weak entity handles so stale results can't overwrite newer navigation or launch after tab close. Keep edit workers alive across tab/view switches, not across attempt changes. Display the recovery directory when closing a tab with retained edits and surface the core's paused/error states without recreating its decisions.
- [ ] **Implement native launch:** Mark the prepared file using `IAttachmentExecute` with a remote origin that contains no credentials; fail closed if marking fails. Launch only through Windows `SHOpenWithDialog` with `OAIF_EXEC` and a parent window, in an STA; no command construction and no automatic default-app fallback. Distinguish user cancellation from shell failure. Start watching only after `Opened`; a cancelled picker drops the prepared edit and releases its slot. If the attempt ends while the native picker is active and the app opens the file anyway, retain the copy but do not start save-back.
- [ ] **Document and verify:** Describe activation, save-back, recovery, supported server/file types, ACL/xattr limitations, and the SFTP compare-and-swap race. Update project scope to reflect basic SFTP now, with broader transfers later. On Windows x64 run the four commands in `AGENTS.md` and smoke-test two hosts, native picker cancel, Notepad and an atomic-replace editor, existing destinations, conflict with a remote writer, disconnect/reconnect, keyboard focus, and narrow/mixed-DPI layouts. Expect all commands to succeed and no writes to the wrong file or tab.
- [ ] **Commit** as `Add Windows Files browser and Open With`.
