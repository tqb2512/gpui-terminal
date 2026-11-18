# GPUI Terminal Client

A Windows-first desktop SSH client in Rust, built with Zed’s GPUI. Save hosts and
open independent SSH terminal tabs with passwords or private-key files. It uses
an embedded SSH client and a remote shell; no Windows OpenSSH installation is
required.

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

## Connect and manage hosts

- **Quick Connect:** Enter `user@host` or `user@host:2222` and select **Password**
  or **Private key**. For IPv6 with a port, use `user@[2001:db8::1]:2222`.
  Quick Connect does not save a profile.
- **Saved hosts:** Choose **New host** to save a display name, address, and
  authentication method. Use the search field or choose **Connect**, **Edit**,
  or **Delete** beside a host. The most recently connected hosts appear first.
  Editing or deleting a host leaves its open tabs and trusted key alone.
- **Verify the host first:** Compare the app’s SHA-256 fingerprint with a trusted
  source, such as your server administrator, before approving it. The app then
  asks for a password, key path, or key passphrase as needed.
- **If a host key changes:** The connection stops before authentication. Don't
  approve a replacement without independently verifying the change. Once
  verified, choose **Forget host key…** on the failed tab, confirm, and then
  **Reconnect** and verify the new fingerprint. Forgetting a key does not close
  established sessions.
- **Terminal tabs:** Each connection opens a new tab. Use **Close** to end just
  that tab. **Reconnect** opens a new remote shell and does not replay input.
  Drag to select, use **Ctrl+Shift+C** and **Ctrl+Shift+V** for copy and paste,
  and use **Ctrl+Tab** to move focus out of the terminal.

Profiles and approved host keys are stored in
`%LOCALAPPDATA%\GPUI Terminal Client\connections.sqlite3`. A profile may remember
a key file path, but the app does not save private keys, passwords, passphrases,
or terminal transcripts.

## Development checks

```powershell
cargo fmt --all -- --check
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo test --locked --workspace
cargo build --locked -p terminal_client
```

### Windows smoke test

On a Windows x64 desktop, use an SSH server you control:

1. Quick Connect, reject the first fingerprint, then reconnect and verify it
   against the server. Connect once with a password and once with a private
   key, including a passphrase-protected key.
2. Add and edit a saved host, restart the application, find the host by search,
   and reconnect. Delete the profile and check an open tab stays connected.
3. Keep two tabs open while both print output. Switch between them, resize a
   full-screen terminal program, use Unicode input and copy/paste, then close
   one tab and check that the other stays usable.
4. Rotate the test server's key. Verify no credential prompt appears until you
   separately forget the old key and accept the independently verified new
   fingerprint.
5. Check keyboard focus and small/high-DPI window layouts, then close the last
   window and verify the app exits. Repeat with the system SSH client absent.

See the [project scope and architecture](terminal-client-project.md),
[architecture diagram](gpui-terminal-architecture.html), and [coding guidelines](AGENTS.md).
