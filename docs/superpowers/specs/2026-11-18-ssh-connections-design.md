# Phase one: SSH connections and saved hosts

## Goal

On Windows, a user can save and manage SSH hosts, connect with a private key or a password, and work in multiple simultaneous terminal tabs. Prioritize the connection, terminal, and storage code. The initial GPUI controls must be usable, but replacing them must not require rewriting any of that core.

This phase does not target visual polish, new desktop platforms, or an installer. It does not add an SSH agent, SSH config import, jump hosts, SFTP, forwarding, sync, local shells, or scrollback search. The existing Windows CI build remains the way to get an executable.

## Approaches

- **Selected: a standalone Rust core behind a thin GPUI workspace.** Keep the existing crate boundaries and give each tab its own session. This adds explicit boundaries for prompts and session events, but lets the core run under headless tests and keeps a future UI replaceable.
- **Build the flow into the GUI first.** Keeping connection and storage state in GPUI would simplify the first demo, but would tie trust and session handling to the UI the user expects to replace.

Use the project’s existing architecture choices: embedded Russh on a shared Tokio runtime, SQLite for persistence, and `alacritty_terminal` for terminal emulation. No local `ssh` executable or local shell.

## Connection manager

- Quick Connect accepts `user@host` and an optional port; use port 22 by default and support bracketed IPv6 for explicit ports. Select password or key-file login before connecting. If a key path is needed, ask for it after host verification. Quick Connect does not create a saved profile.
- A saved profile has a label, host, port, username, and authentication method. For key login it may remember a private-key **path**, never the contents. Users can add, edit, search, delete, and connect to saved hosts. Show recently used saved profiles based on their last successful connection.
- Profiles and approved public keys persist in the user's local app-data directory. SQLite operations run off the GUI thread. Report storage failures instead of silently replacing or bypassing the database.
- Editing or deleting a profile does not affect a live tab or remove server trust. A tab reconnects using its original connection details and may reuse its most recently selected key path in memory. It asks for secrets again if needed; choosing a path in a tab does not update a profile.

## SSH trust and authentication

- Identify trust by normalized host and port, independently of username and saved profile. Lowercase ASCII DNS names, retain any final dot (it can change resolver behavior), and canonicalize literal IPv4 and IPv6 addresses. Accept internationalized hostnames in ASCII Punycode form. Never merge aliases or merge a DNS name with the IP it resolves to. Before loading a key or requesting a password or passphrase, verify the presented public host key. For a new endpoint, display the endpoint and SHA-256 fingerprint and ask the user to verify it independently before approving. Persist the approved key before authentication; concurrent approvals must not overwrite a different key.
- A changed key fails closed without asking for credentials. To re-establish trust, the user separately forgets the old key for that endpoint, then starts a fresh connection and verifies the new fingerprint. Never combine forgetting and approving in a single prompt. Forgetting invalidates all in-progress attempts and prompts for that endpoint; an older prompt cannot restore the key or advance authentication. Leave established sessions open. Coordinate invalidation and persisted trust updates so a delayed response cannot undo the forget operation.
- Support password authentication and key-file authentication, including a passphrase prompt for supported encrypted keys. Passwords, passphrases, private key material, and transcripts never go into SQLite or logs. Use typed, session-scoped prompts, no fallback from one authentication method to another, and no prompts triggered by terminal output. A cancelled or stale prompt cannot approve or authenticate a later session.

## Sessions and workspace

- `ssh` owns the connection and remote PTY channel, exposing typed events and accepting input/resize/shutdown commands. It depends on neither GPUI nor SQLite. `hosts` owns profiles and trusted public keys. `terminal` turns ordered SSH bytes into renderable terminal state, encodes terminal input, and reports session lifecycle independently of GPUI. The app wires these together; `terminal_view` reads terminal state and forwards input without knowing about SSH or storage.
- Open a new tab for each connection. Tabs connect independently, including to the same saved profile; prompts and failures identify their tab. Switching tabs does not suspend them. Closing a tab cancels pending work and terminates its connection without affecting other tabs. Reconnect starts a new connection without replaying previous input.
- Request a remote PTY and shell after authentication. Forward viewport sizes, keyboard input, and paste to the active tab. Render Unicode, color, and cursor state; retain bounded scrollback and provide selection and copy. Bound queues with backpressure, preserve terminal byte order, and coalesce redraw notifications rather than terminal bytes.
- Keep the existing window. Add a host list with Quick Connect and basic profile editing, a tab strip above the terminal, and a prompt/status area scoped to the selected tab. Preserve keyboard access, mask credential fields, and make connecting, errors, and disconnects visible. Visual refinements are not a phase-one gate.

```text
Saved hosts [Search]     [web-01 ×] [staging ×]
[New host] [Quick Connect] ───────────────────
  web-01                   Active terminal
  staging                  or connection prompt
```

## Delivery checks

- Test profile CRUD, search, reopening the database, trust persistence, changed-key rejection, conflicting approvals, and forgetting during a pending attempt without a GUI. Test ordered terminal output and bounded scrollback.
- Run an isolated loopback SSH server in integration tests. Cover password and private-key login, trust before authentication, rejection of a changed key, remote PTY resize, cancellation and cleanup, and two simultaneous sessions with independent I/O.
- Run the repository’s Windows x64 format, lint, test, and build checks. Smoke-test Quick Connect, profile persistence, fingerprint approval, both auth methods, tab switching under output, and closing one tab without dropping another. Record any Windows checks that cannot be run; passing headless tests alone does not verify the GUI.

The exact internal Rust types, migrations, widget implementation, and SSH test fixture are implementation decisions. Wire compatibility stays within the supported defaults of the chosen SSH library; do not weaken defaults to accommodate legacy servers.
