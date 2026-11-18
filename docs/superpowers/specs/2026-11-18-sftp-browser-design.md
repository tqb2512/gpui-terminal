# Phase-one SFTP browser design

## What ships

Windows x64 users can open **Files** in an authenticated SSH tab, start in the remote home directory, navigate folders, refresh, and upload or download a single regular file. **Open with…** downloads an editable local copy and lets the user choose a Windows app. Saving that copy uploads the edit to its original remote file automatically while the original connection remains active. Moving between Files and Terminal or changing tabs does not stop save-back.

```text
 Terminal   [Files]                 build.example
 [Up]  /home/ops/config            [Refresh] [Upload…]
 Name                         Type        Size
 ▸ archive                    Folder         —
   service.toml               File       3.2 KB
 [Download…] [Open with…]
 service.toml · Saved to remote       [Show local copy] [Finish editing]
```

The Files view is a minimal host for the core workflows and will be replaced later. Reuse existing controls, keyboard focus patterns, and native dialogs. Show the current path, actionable errors, and save-back status; defer custom layout, styling, and UX polish. Do not add a second workspace. No remote-to-remote transfer, recursive operations, drag-and-drop, rename/delete, transfer queue, or resume in this phase. Full transfer progress and per-transfer cancellation remain follow-ups.

## Approaches

- **Recommended: one SFTP channel on the tab's verified SSH connection.** Open it on first use, independently of the PTY. Reuses authentication without retaining credentials. A subsystem denial affects Files, not Terminal.
- A separate SSH connection would isolate transport and allow standalone browsing, but could require another authentication prompt. Defer it until a workflow needs it. Shell commands or an external SFTP executable would bypass the existing transport boundaries and are out of scope.

## Boundaries for future features

`ssh` owns SFTP, validated slash-separated remote paths, streaming reads, and staged remote writes; it does not know about local paths, GPUI, or SQLite. The headless `terminal_client` layer ties a remote handle to an exact `AttemptId`, handles local destinations, and manages editable copies, including their limits, state, and cancellation rules. GPUI owns selection, dialog wiring, focus, and rendering the core's status. Keep Windows attachment handling and app launching in a separate platform adapter, not in the view. The remote read and staged write APIs remain usable separately: a later transfer coordinator can pipe a reader from one authenticated handle into a staged writer on another without touching the browser or inventing a unified filesystem or job queue now.

Keep I/O off the GPUI foreground thread. Keep one foreground Files operation per tab; the edit worker serializes its own writes and never queues unbounded saves. Limit uploads and downloads to regular files; show symbolic links and special entries but do not follow them for operations in this phase. Do not use a server-provided name as a local path.

## Transfer and save-back rules

- Stream into an exclusive temporary sibling, then publish only after a successful close. Ordinary transfers are **create-only**; never truncate an existing local or remote file. Remove uncommitted staging files when possible; disclose cleanup failures, and treat a lost acknowledgment during publish as an unknown outcome rather than replaying.
- For **Open with…**, use a private, randomly named directory under `%LOCALAPPDATA%\GPUI Terminal Client\edits`. Use a safe generated basename with the expected extension, mark the copy as originating remotely using Windows attachment handling, and only launch it when explicitly requested. Reject executable, script, shortcut, and unknown extensions via a supported-type allowlist. The app does not open anything automatically. Support at most four active copies app-wide, 32 MiB per editable file; ordinary streamed transfers do not use that size limit.
- Poll the editable **path**, not an open handle or editor process, to catch editors that replace the file on save. Require two consecutive matching content samples one second apart. Ignore unchanged bytes, tolerate a temporarily missing or locked file, and stage an immutable snapshot. Recheck the local path after each upload so a save during an upload is not lost. Do not follow a reparse point or read a non-regular local file.
- Capture a baseline from the downloaded bytes and remote attributes. Before committing any update, recheck the original path and content, including same-size changes with the same modification timestamp. On missing, changed, or unverifiable originals, pause save-back and preserve the local copy. No **Overwrite anyway** action in this phase. Publish by atomic replacement only when the server advertises `posix-rename@openssh.com` version 1. If unsupported, disable **Open with…** with a reason; **Download…** stays available. Preserve basic file permissions, owner, and group or refuse the edit. This is not for files that need ACLs, xattrs, or hard-link identity preserved. SFTP has no atomic compare-and-swap: another writer can still win the race between final verification and rename; document that limit.
- **Finish editing** stops watching and keeps the local copy. Tab close, disconnect, app exit, or reconnect stop old work and never replay edits. Retain the local copy after the native app successfully opens it; show its location while active or paused and disclose the recovery directory on tab close and in the README. Abandoned pre-launch copies are cleaned up. Saved edits can be recovered manually from that folder; the app does not persist an edit registry or resume save-back after restart. Never log file bytes, credentials, or transcripts.

## Acceptance

Prove browse, upload, download, save-back, conflicts, and cancellation in headless tests before wiring the Windows view. Two SSH tabs can browse different hosts while shell output continues. Upload and download round-trip a file without overwriting pre-existing destinations. Opening a supported file in a Windows editor and saving twice, including an atomic-replace save, updates only its originating remote file. A concurrent remote edit, unsupported server, or disconnect leaves the user with a clear status and a usable local copy where one was opened. The Windows gate checks native dialog wiring, basic keyboard use, and the existing Windows x64 check suite; it does not require a polished design.
