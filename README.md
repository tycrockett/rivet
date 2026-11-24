# Rivet

A pure-Rust rebuild of Bolt's shell on [Native Shell](https://nsh.dev) (nsh 0.6.1): the workspace tab strip, the
terminals of each workspace, and Bolt's palette (⌘;) with its hub thread, Bolt commands and scripts, driving Bolt's
command registry. No JavaScript runs; on macOS the views are AppKit. Formerly `native/` in
[git-ui](https://github.com/tycrockett/git-ui) as "Bolt Native": the `.ts`/`.tsx`/`.js` paths below are Bolt's files
in that repo, the code each part ports; `themes/` is a copy of its `src/services/monaco/themes/`.

```sh
cargo run                       # this machine's desktop backend
nsh run                         # the same, through the nsh CLI
cargo test --lib                # parser, settings and workspace-rule tests
RIVET_DATA_DIR=/tmp/rv cargo run   # throwaway settings
```

## What it reads and writes

Bolt's `settings.json` schema (`src/types/app.ts`): `directory`, `tabs`, `pinned`, `groups`, `bookmarks`, `commands`;
unknown fields are kept. The file lives in this app's own data directory (`~/Library/Application
Support/Rivet/settings.json`, or `$RIVET_DATA_DIR`; an old `Bolt Native/` is moved there on first launch), seeded from Bolt's (`Bolt/settings.json`, or
`$BOLT_USER_DATA_DIR`) the first time, so the two apps never overwrite each other. `bolt-commands.json` is seeded the
same way; the hub thread is saved as `hub-thread.json` (last 300 entries) beside them, and the strip's dragged width as
`left-pane-width.json` (`{ width }`, read from Bolt's until this app saves its own).

## Layout

| File | Ports |
|---|---|
| `src/settings.rs` | `directoryStore` load/save, `tabNameFor`, legacy string tabs, pinned/tabs de-dupe |
| `src/workspace.rs` | `services/workspaces/workspaceActions.ts`: cursor, groups, regroup, ⌘N numbering, move / pin / hide / group |
| `src/commands/` | `commandParser.ts`, `commandRegistry.ts` (same order), `commandExecutor.ts` (git → `/custom` → registry → `command not found`), handlers |
| `src/state.rs` | the signals every view reads, each workspace's columns, and the PTY table with each terminal's shared emulator; output that clears the scrollback (omp redrawing its history after a resize) is held until it pauses and shown as one write, so the view keeps its place at the end |
| `src/panes.rs` | `editorStore`'s groups and `paneActions.ts`: split, move, focus, reorder, rotate, equalize, the maximize cycle |
| `src/pty.rs` | `ptyDaemonClient.js`: connects to the daemon, starting it when absent as `rvt-ptyd --ptyd` (a hard link to the app in the data directory, named without `rivet` so `pkill rivet` misses it) in a session of its own with launchd as its parent, so stopping the app by name, group or process tree leaves the terminals running; `--ptyd` is taken among the executable's initializers (`ptyd::ENTRY`), before any `main`, so the executables nsh generates (`nsh run`, `nsh test`, the dev host) start the daemon too rather than a windowless copy of the app; replaces a daemon of another protocol version; spawn, attach with replay, write, resize, kill. Output and exits come as events: a channel that closes is a lost connection, never an exit, so it ends nothing; closing a tab ends its shell and the program in front of it (hangup, terminate, kill) |
| `src/ptyd.rs` | `src/ptyd/`: the daemon that owns the shells (`$SHELL` else `/bin/zsh`, `TERM=xterm-256color`), a 5 MB replay ring each, length-prefixed JSON over `ptyd.sock` in the data directory, a log of spawns, attaches, kills and exits in `ptyd.log`; exits after 5 min with no shell and no client. Unlike Bolt's it is not stopped at launch: terminals outlive the app, and `terminals.json` (`state.rs`) puts them back in their workspaces and columns on the next launch; a live shell nothing saved refers to comes back in a column of its own |
| `src/sidebar.rs` | `WorkspacesPanel`: rows, git dot, ⌘N (× over it while hovered or active, closing through the palette's choice), port pill (click to kill), terminal chips with their status square (`TerminalChip`, `termBg`: agent, working, idle, cyan while waiting for you), the PR chip, right-click menu; a click on Pinned's or a group's heading collapses it, a right click locks it against ⌘0; a collapsed heading shows the status of the terminals it hides (`StatusBadge`); rows and group headings drag up and down. `SidebarFooter`: the current directory's `branch · repo` and its lines changed (`useGitStatus`' `locStats`: `+added / -deleted` since the branch's base with the uncommitted in parentheses, else the uncommitted; a spinner while a directory's first count runs), counted again on a directory or branch change and when the working tree or HEAD changes (looked at every 2 s, where Bolt watches the directory) |
| `src/drag.rs` | the strip's drag (`targetBefore`, the drop on a heading): a row reorders in its section, joins the group it is dropped among or on, leaves its group when dropped below everything; a group heading reorders the groups |
| `src/ports.rs` | `usePorts`: this user's TCP listeners (omp's left out) by their process's directory, via `lsof`, every 30 s |
| `src/prs.rs` | `useTabPrs` + `getPrForBranch` + `prStatus.ts`: `gh pr view` for branches with an upstream, again when the branch moves, every 15 s while checks run, every 5 min otherwise; for the PR bar also review threads (GraphQL) and the branch's last push (GitHub's repository activity), and `gh pr merge` |
| `src/pr_bar.rs` | `PrStatusBox` as a header bar above the terminals: a refresh button (`↻`, a spinner while asking), the current workspace's PR title and number, checks (a spinner while any run), review (`Approved - <who>` once approved), comments (new from others since the last push, unresolved threads, total) and a two-press squash merge (admin when branch protection blocks it), then `clean up` (`delete <head branch>`); a press on the bar or ⇧⌘P opens a dialog with the title, link, reviewers, each check and each comment. Asked again on each git state read, every 15 s while checks run, every minute otherwise, and on the refresh button |
| `src/collapse.rs` | `searchStore`'s collapse atoms and the ⌘0 cycle (`togglePinnedCollapsedAtom`), with `pinnedCycle.check.ts` as tests; saved as `workspace-collapse.json`, seeded from the Electron Bolt's |
| `src/hints.rs` | `HintOverlay` + `hintLabels` (⌘⏎): labels for what views register as clickable (rows, chips, headings, column tabs, +, ×, columns, views, the Changes list), the same prefixes and ordinals, typed to click; `hintLabels.check.ts` as tests |
| `src/terminals.rs` | `EditorPanel` + `EditorGroup` + `PaneOverlay`: one resizable column per editor group, each with its tabs (terminals, files and web pages), the shown one and a terminal's notes strip, Bolt's 12px gap between frames; the active-column bar (blue, at most 250 wide, centered under the active column, sliding between columns, shown while the focus is in a column); xterm's look (theme colors, 12px JetBrains Mono else Menlo, block cursor, 8px inset). Only the shown tab is mounted; one emulator per terminal outlives its views, so screen and scrollback survive switching and moving columns. A workspace with no tabs shows its directory instead, 32px in the top left (96px in), which drags the window |
| `src/web.rs` | `WebTab` + `webviewPreload.js` + the `<webview>` half of `main.js`: a web tab (`browser [url]`, `open`) with back, forward, reload / stop, the URL bar (`toUrl`: a bare host over https, `localhost` over http, else a Google search), the AI switch that lets `browse` drive it, and Copy URL. Its page (a `WKWebView`) lives in a layer over the columns' row for as long as the tab is open, laid over the column showing it and parked offscreen otherwise, so switching tabs, columns and workspaces never reloads it. Each page runs a script first: shift-click inspects an element, the console is kept (500 entries, cleared on each load), and `target=_blank` links, ⌘-clicks and `window.open` go to the system browser |
| `src/viewer.rs` | Bolt's file and diff tabs, for notes, `keybindings.json` and files agents open (`openFile`, with its selection): nsh-base's `Editor` or its `DiffView` against `HEAD` (split or unified), switched in the tab's header; saved 400 ms after typing stops and when the tab closes. `openDiff`'s tabs show a file against proposed text and write nothing |
| `src/settings_tab.rs` | `SettingsTab` (`settings`): Open in New Column (new terminals, files, Changes and Settings tabs go in a column of their own); Agent command (`agentCommand`, what `startAgents` and the hub type to start an agent, `c` while empty); and Keybindings, which opens `keybindings.json` for editing |
| `src/keybindings.rs` | `keybindings.ts`: `keybindings.json` (this app's, else the Electron Bolt's) merged with the defaults, read at launch; `when` clauses are ignored, and only commands this shell has bind |
| `src/changes.rs` | `ChangesTab` + `changesStore`: `diff`'s tab, the changed files (staged and unstaged in the working tree, or a commit's, or a range's) beside the selected file's diff; the header picks a commit |
| `src/theme.rs` | `monacoToXtermTheme` and `applyThemeCssVars` over Bolt's Monaco themes (compiled in from `src/services/monaco/themes`), the one `theme.json` names |
| `src/paths.rs` | `tilde`: every path the app shows (the empty workspace, terminal titles and chips, the strip's footer, the palette's header and plan line, result cards, posts, choices, the finder and bookmarks) starts `~` under the home directory; MCP replies keep absolute paths |
| `src/notes.rs` | `notesStore`, `useNotesPoll`, `NotesStrip`: the rivet notes strip under omp terminals, read from the Electron Bolt's `bolt-notes` folder (kept by that name so both apps share the notes), with group memories |
| `src/hub/mod.rs` | `fleetStore`'s thread, handles (`hubTargetsAtom`, `sessionTag`), `parsePost`, choices, finder matching |
| `src/hub/engine/` | `boltCommands/types.ts`, `boltCommandStore`, `fleetStore`'s hops and posts, `terminalSendStore`, `pty:status` |
| `src/hub/palette/` | `BoltPalette`, `HubCards`, `ScriptEditor`, and `SwitchResults`' cards: git steps with their icon and detail, `status` (lines changed, ahead/behind, unpushed, each kind of change with its files to open), branches (default, worktree, last commit, remote; click to switch), conflicts with their files, bookmarks, diffs in color |
| `src/commands/rich.rs` | what a result card draws beyond its text (Bolt's result `type`s), which commands set beside the text they print |
| `src/mcp/` | `mcpServer.js`'s streamable HTTP transport and `useMcpBridge.ts`'s tools: the `bolt` MCP server at `http://127.0.0.1:41777/mcp` (the URL omp's and claude's configs name), retried every 5 s while another app holds the port; a request whose `Origin` is not this machine is refused. Bolt's schemas (`tools.json`), except that `startAgents` names no agent (it types `agentCommand`) and `saveNote` saves rivet notes; workspace tabs, `runCommand`, `createWorktrees`, `setTabGroup`, `closeTabs`, `sendToTerminals`, `startAgents`, file and diff tabs, and `saveNote` work; `browse` reads, navigates, clicks, types (trusted input, `webview.input` in `nsh.json`) and lists the console of the web tab showing, but has no `screenshot`, `draw`, `clear`, `network` or `devtools`, and `getInspectedElement` no screenshot; selection, diagnostics, the model canvas, graphs, boards, `readTerminals` and `startGroupSession` (and so `groupAgentCommand`) answer that Rivet does not support them |
| `src/group_memory.rs` | `groupSessionStore`'s prompts: each group home's and member tab's `.omp/APPEND_SYSTEM.md`, kept current as groups and membership change, so omp sessions keep saving the group's memories in Bolt's `group-sessions` folder. A group needs no home: its folder is `group-<id>`, from `groupIds` in settings, which a rename keeps; on launch, memories in Bolt's per-home folders move into it (`notes::migrate_home_memories`) |

## Keys

Bolt's defaults, rebindable in `keybindings.json` (`settings` → Keybindings; read at launch):

| Key | Does |
|---|---|
| ⌘; | Bolt's palette: `/command`, `@handle message`, `#script`, `:keyword`, `@bolt …`, or a shell line for a new terminal |
| ⌘2–⌘9 | The Nth row on screen (⌘1 is Bolt's fleet view, not here) |
| ⌥⌘↑ / ⌥⌘↓ | Previous / next workspace |
| ⌘\` | New terminal here |
| ⌘W | Close the shown tab |
| ⇧⌘[ / ⇧⌘] | Previous / next terminal in the active column |
| ⌘' | Pane overlay, until Escape: E/R split left/right, D/F move tab left/right, C/V focus column, Q/W reorder tab, A rotate, S equalize, Z cycle width (75%, 30%, back) |
| ⇧⌘X | Close the current tab: the palette asks Close all / Close tab / Cancel |
| ⌘0 | Bolt's collapse cycle over Pinned and the groups: collapsed (rows with terminals stay), folded, back; locked sections (right-click a heading) keep theirs |
| ⌘B | Toggle the strip |
| ⇧⌘P | The PR dialog, while the bar shows a PR |
| ⌘⏎ | Hint overlay: type a label to click it, Backspace to undo a letter, Escape to leave |

`notes` opens this directory's notes for editing (`notes --list` lists them all, click to open, × to delete; `notes
clear` deletes them), in the Electron Bolt's `notes` folder, so both apps share them. `settings` opens the Settings tab.

Files other than notes and `keybindings.json` open in Helix (`hx`, which must be on the shell's `PATH`) in a new
terminal: `/open`, the files a palette card lists, and the Changes tab's file rows.

## Not here yet

Every git verb is ported (`src/commands/git_verbs.rs`). Commands that open Electron-only views (`settings set` /
`settings list`, `repo`, `ports`, `todo`, `edit`, `snippets`, `bolt`, `board`, `bm` / `bm go`, `tab session`)
are listed in the palette, greyed out, and say so when run, as do `commit --ai` / `amend --ai` (Bolt's AI client).
`status --show` and `show` print their diffs as results instead of opening a diff tab; `open` shows the PR or compare
page in a web tab, `open --browser` and `browser <url> -o` hand it to the system browser (Bolt asks which browser
window); web tabs, like file tabs, are not kept across launches;
`open <path>[:line[:column]]` opens the file in Helix at that place; in the palette, `/open ` completes the
workspace's files fuzzily (`git ls-files`, so
ignored files stay out; `src/files.rs`); `diff` (`--commit <hash>`, `--since <hash>`) opens the Changes tab
(`src/changes.rs`) read-only: no stage / unstage / discard / remove-from-branch, no branch notes panel, no ↑/↓ in the
list, and the working tree is polled every 2 s rather than watched;
`delete` on a worktree branch closes its tab without Bolt's confirmation modal.

The palette's result cards are drawn with characters for Bolt's Phosphor icons and without its flash and shake
animations; the todo, notes, settings and orphan-terminal views are not here. `@bolt` and headless hops run your
`claude` with no `--mcp-config`, so they reach this app's server only where your own claude config names
`http://127.0.0.1:41777/mcp`. A terminal's output is read from its screen (no `pty:tail`), and its status comes from
the process table and its last output time.

Terminals do not reflow: NSH's emulator cuts lines when a column narrows (xterm.js re-wraps them), so text cut by a
narrow column stays cut after it widens again.

The PTY daemon is unix-only, keeps no 64-session cap and captures no zsh `lastCommand` label. The IDE environment
(`CLAUDE_CODE_SSE_PORT`, the `BROWSER` shim), Bolt's WebSocket IDE server and its lock file are not set up, so the
Claude CLI's IDE integration and omp's selection extension see no IDE.
