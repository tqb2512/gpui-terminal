# Host Workspace Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Put saved hosts and live SSH sessions in a shared top tab strip, and show remote files beside the active terminal.

**Architecture:** Use `Workspace.active` as the selected session (`None` is Hosts) and keep the existing per-session `FilesView` entity. Lay out Files and Terminal as siblings so the existing terminal measurement still drives PTY resize.

**Tech Stack:** Rust 2024, GPUI 0.2.2, Windows x64.

**Spec:** `docs/superpowers/specs/2026-11-19-host-workspace-design.md`

## Global Constraints

- “Keep the Windows frame, terminal renderer, storage, SSH trust and authentication, and the SFTP transfer lifecycle.”
- “No new dependencies, connection features, file operations, or web surface.”

## Review Focus

- Switching to Hosts leaves session output and file saves running; returning to an SSH tab shows its own Files state.
- Closing a background tab or a tab on either end of the strip does not select the wrong host.
- At the 320 px minimum width, the open Files pane leaves usable terminal space and its own controls remain reachable.
- Long host labels, filenames, or paths do not force tabs or panes past the viewport.
- Keyboard focus remains visible and goes back to the terminal when Files closes; an SSH prompt remains on its own tab.

---

### Task 1: Hosts is the permanent first tab

**Files:** Modify and test `crates/terminal_client/src/workspace.rs`; update its constructor call in `crates/terminal_client/src/main.rs` so connection events can direct focus in the window.

**Interfaces:** `Workspace::select_tab(&mut self, id: TabId, cx: &mut Context<Self>)` retains its current session behavior. Add `Workspace::select_hosts(&mut self, cx: &mut Context<Self>)` and `Workspace::hosts_screen(&self, cx: &Context<Self>) -> AnyElement`. Add `neighbor_after_close(remaining: usize, closed_index: usize) -> Option<usize>`.

- [ ] **Write the failing test:** In `closing_active_tab_chooses_neighbor_or_hosts`, assert `neighbor_after_close(2, 0) == Some(0)`, `neighbor_after_close(2, 1) == Some(1)`, `neighbor_after_close(2, 2) == Some(1)`, and `neighbor_after_close(0, 0) == None`.
- [ ] **Run the red check:** `cargo test --locked -p terminal_client --bin terminal_client closing_active_tab_chooses_neighbor_or_hosts` on Windows; expect a missing helper. On Linux use a Windows-target `cargo check --tests` if a Windows test runner is unavailable.
- [ ] **Implement Hosts navigation:** Keep the first tab fixed and session tabs horizontally scrollable; cap long session names. Remove `hosts_visible` and its width-based sidebar behavior. `select_hosts` clears selection and transient forms, not the session list. When closing the selected tab, use `neighbor_after_close`; closing any other tab leaves selection alone. Prefer the credential field when explicitly focusing a tab waiting for credentials.
- [ ] **Implement the Hosts screen:** Replace `sidebar` with a full-width directory. Place Quick Connect and New host in its header, then search and scrollable rows with display name, endpoint, and Connect/Edit/Delete. Preserve filtering and order, existing editor and confirmation flows, and distinct empty/no-results copy. Use a labeled active style and keep the existing keyboard activation for controls.
- [ ] **Run the test again** and verify opening two profiles, returning to Hosts, closing a background tab, and switching back to a tab with a pending prompt in the Windows app.

### Task 2: Inset Files pane next to a live shell

**Files:** Modify and test `crates/terminal_client/src/workspace.rs`; modify `crates/terminal_client/src/files_view.rs`, `crates/terminal_view/src/terminal_view.rs`, and `README.md`. Clearing cached terminal geometry on tab switches avoids sending the previously shown tab's dimensions.

**Interfaces:** Keep `Workspace::show_files(&mut self, id: TabId, show: bool, window: &mut Window, cx: &mut Context<Self>)` and `FilesView::new` unchanged. Add `files_pane_width(window_width: gpui::Pixels) -> gpui::Pixels`.

- [ ] **Write the failing test:** In `files_pane_leaves_room_for_the_shell`, assert `files_pane_width(px(320.)) == px(144.)`, `files_pane_width(px(600.)) == px(270.)`, and `files_pane_width(px(800.)) == px(272.)`.
- [ ] **Run the red check:** `cargo test --locked -p terminal_client --bin terminal_client files_pane_leaves_room_for_the_shell` on Windows; expect a missing helper. On Linux use the Windows-target compile check.
- [ ] **Implement the session layout:** Always render the terminal for a session outside prompts/forms. Attach Files as a shrink-resistant left sibling only when that tab has it open, with `min(272 px, 45% of window width)`, padding, a border, and rounded corners. Use one **Files** / **Hide Files** toggle and retain existing reconnect and trust actions. When Files opens, give it focus; when it closes, return focus to the terminal. Preserve its entity when hidden or when navigating away.
- [ ] **Refine the Files view:** Show **Remote files** and the current path up top, distinguish file names from type/size, and preserve the existing controls, transfer messages, and edit recovery actions. Keep long values and excess rows from widening the pane; keep controls and feedback reachable in narrow windows.
- [ ] **Update `README.md`:** Document Hosts navigation and simultaneous Files and Terminal; add a smoke check for toggling Files and switching between hosts with different locations and visibility.
- [ ] **Verify:** Run `cargo fmt --all -- --check`, `cargo clippy --locked --workspace --all-targets -- -D warnings`, `cargo test --locked --workspace`, and `cargo build --locked -p terminal_client`. On Windows x64, check normal and minimum widths: open Files with long paths, resize with a running shell, switch tabs with different open folders, and use the main controls by keyboard. Report cross-compilation and native runtime checks separately.
