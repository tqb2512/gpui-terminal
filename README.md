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
If Windows reports that `VCRUNTIME140.dll` is missing, install Microsoft's
[x64 Visual C++ Redistributable](https://aka.ms/vc14/vc_redist.x64.exe).

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
- **Saved hosts:** The app starts on the **Hosts** tab. Choose **New host** to
  save a display name, address, and authentication method. Use the search field
  or choose **Connect**, **Edit**, or **Delete** beside a host. The most recently
  connected hosts appear first.
  Editing or deleting a host leaves its open tabs and trusted key alone.
- **Verify the host first:** Compare the app’s SHA-256 fingerprint with a trusted
  source, such as your server administrator, before approving it. The app then
  asks for a password, key path, or key passphrase as needed.
- **If a host key changes:** The connection stops before authentication. Don't
  approve a replacement without independently verifying the change. Once
  verified, choose **Forget host key…** on the failed tab, confirm, and then
  **Reconnect** and verify the new fingerprint. Forgetting a key does not close
  established sessions.
- **Terminal tabs:** Each connection opens a tab next to **Hosts** at the top.
  Return to **Hosts** without disconnecting. Use **Close** to end just
  that tab. **Reconnect** opens a new remote shell and does not replay input.
  Drag to select, use **Ctrl+Shift+C** and **Ctrl+Shift+V** for copy and paste,
  and use **Ctrl+Tab** to move focus out of the terminal.

Profiles and approved host keys are stored in
`%LOCALAPPDATA%\GPUI Terminal Client\connections.sqlite3`. A profile may remember
a key file path, but the app does not save private keys, passwords, passphrases,
or terminal transcripts. Only one app instance can use this store at a time so
forgetting a key revokes all pending attempts that use it.

## Browse and edit remote files

Choose **Files** in a connected SSH tab to open the browser on the left. The
terminal narrows and stays usable beside it. Browse from the remote home
directory and use **Upload…** or select a regular file and choose **Download…**.
Transfers do not overwrite existing destinations. **Hide Files** gives the shell
the full width again without interrupting transfers or save-back.
Upload requires `hardlink@openssh.com` and hard links on the destination
filesystem so the app can publish without replacing an existing file. Browsing
and downloads do not require that extension.

**Open with…** copies a supported file (up to 32 MiB) to Windows and prompts for
a local app. Save normally in that app. The Files view reports when the save
has been uploaded to the original remote file. **Finish editing** stops
automatic uploads. Closing the tab or losing the connection also stops them;
reconnect does not replay changes. The local copy remains under
`%LOCALAPPDATA%\GPUI Terminal Client\edits` and can be recovered or removed
manually. Remove copies when you no longer want them stored on this PC.

Open with supports common text, config, data, image and document types and
requires the server's `posix-rename@openssh.com` extension. Download remains
available on servers without it. If the original has changed, automatic
save-back pauses rather than overwriting it. SFTP cannot make the conflict
check and rename a single operation, so avoid automatic save-back on files
with concurrent writers or files whose ACLs, extended attributes, or hard-link
identity must be preserved.

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
6. Open Files on two hosts, go to a different folder in each, and hide one
   browser. Switch through **Hosts** and back to each session and check that
   its folder and visibility are preserved. Toggle Files in a narrow window
   and verify the shell resizes and gets keyboard focus when Files closes.
   Upload and download a regular file, try an existing destination, and cancel
   each native picker. Select a Windows shortcut in the upload picker and verify that
   neither it nor its target is sent. Open a supported file once in Notepad and
   once in an editor that saves by replacing the file. Save twice, then test a
   remote conflict and disconnect during an edit; verify that the local copy
   survives and reconnect does not send it automatically.

See the [project scope and architecture](terminal-client-project.md),
[architecture diagram](gpui-terminal-architecture.html), and [coding guidelines](AGENTS.md).
