# Host workspace redesign

## Intent

Make saved hosts the starting point. Keep open sessions easy to find, and let users browse remote files and use the shell at the same time.

## Approaches

- **Chosen: permanent Hosts tab and an inset Files pane.** Hosts opens a full saved-host screen without disconnecting any session. Files takes real layout space so it never obscures terminal text.
- A separate Hosts button and a session-only strip would keep destinations in different controls. A permanent saved-host sidebar would use terminal space even when no one was managing hosts.
- Overlaying Files would preserve terminal width but cover output. Replacing the terminal with Files, as today, prevents using both together.

## Screens and behavior

```text
[ Hosts ] [ build-server ] [ staging ]                 ← same strip on every screen
  Saved hosts                    [ Quick Connect ] [ New host ]
  [ Search saved hosts                                      ]
  Build server     ops@build.example        [ Connect ] [ Edit ] [ Delete ]
  Staging          ops@staging.example      [ Connect ] [ Edit ] [ Delete ]
```

```text
[ Hosts ] [ build-server ] [ staging ]
  Build server · Connected                                   [ Hide Files ]
  ┌─ Remote files ─────────┐  ┌─ Terminal ────────────────────────────────┐
  │ /home/ops              │  │ ops@build:~$                              │
  │ archive/               │  │                                           │
  │ service.toml           │  │                                           │
  └────────────────────────┘  └───────────────────────────────────────────┘
```

- **Hosts:** Start here. Keep search, existing recent-first ordering, direct Connect, and profile management. Give empty and no-match results distinct messages. New, edit, delete confirmation, and Quick Connect stay in context.
- **Session tabs:** Opening a connection selects its tab. Returning to Hosts does not close sessions. Each session has its own close control and can indicate when it needs a response. Closing the active tab selects a neighboring session or returns to Hosts if none remain. Keep tabs navigable when the strip overflows.
- **Session:** Keep connection state and existing actions in a compact header. Trust and credential prompts remain scoped to the session. Files is closed initially, then opens in an inset left pane beside the terminal. Toggling it reflows the terminal via the existing viewport resize path. Its open state, location, and selection stay with the session. Hiding Files or switching tabs does not stop active save-back.
- **Small windows:** Keep Hosts visible and allow host row controls to wrap. Limit Files to a fraction of the window and keep its content scrollable rather than overlap the terminal. Maintain visible keyboard focus and give focus back to the shell when Files closes.

## Visual direction

- **One workspace, two views.** Use charcoal chrome, a darker terminal surface, and a slightly raised Files pane with a thin border. Reserve the existing cool blue for the active tab, primary actions, and focus. Use Windows UI type for controls and monospace for terminal text, remote paths, and fingerprints.
- **Hosts is a directory, not a dashboard.** Align the heading, search, and saved-host rows to one content grid. Lead each row with its display name and show the address immediately below it. Keep Connect in the most prominent position; Edit and Delete are quieter but visible. Keep Quick Connect and New host together in the header, and wrap the row actions cleanly at narrow widths.
- **The terminal stays the main work area.** Use a compact tab strip and connection header. Make the selected tab distinct by shape and fill, not color alone. The inset Files pane puts the current path and folder controls above a scrollable list, separates file names from type and size, and keeps transfer and save-back feedback near the file work. Its edges align with the terminal's work area.
- **Controls respond without moving the layout.** Use consistent hover, press, and keyboard focus states for tabs, rows, and buttons. No decorative motion or placeholder content; the actual host inventory, shell, and remote files are the interface.

## Boundaries and checks

Rework the GPUI workspace and Files presentation in the existing native app. Keep the Windows frame, terminal renderer, storage, SSH trust and authentication, and the SFTP transfer lifecycle. No new dependencies, connection features, file operations, or web surface.

Verify the saved-host flow, navigation with multiple live tabs, per-tab Files state, terminal resize with Files open and closed, and keyboard focus at normal and narrow window sizes on Windows x64. Visually check long host names and paths, empty lists, prompts, and file errors for clipping and legibility. Update the README to match.
