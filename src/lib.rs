//! Bolt's shell in Native Shell: the workspace strip, the terminals of each
//! workspace, and Bolt's palette (⌘;) over them, driving the command
//! registry and the hub. Settings are Bolt's `settings.json` schema, kept
//! in this app's own data directory and seeded from Bolt's on first run
//! (`settings.rs`).

mod changes;
mod collapse;
mod commands;
mod files;
mod git;
mod group_memory;
mod hub;
mod hints;
mod drag;
mod keybindings;
mod mcp;
mod notes;
mod panes;
mod ports;
mod pr_bar;
mod paths;
mod prs;
mod pty;
pub mod ptyd;
mod settings;
pub use settings::adopt_legacy_data_dir;
mod settings_tab;
mod sidebar;
mod state;
mod terminals;
mod theme;
mod viewer;
mod web;
mod workspace;

use nsh_base::{h_resizable, resizable_panel, theme};
use nsh_core::prelude::*;

use crate::{
    state::Shell,
    workspace::{neighbor_path, numbered_rows},
};

/// The app's UI set (`nsh.json` `"rust"`).
#[native_module("native/native")]
fn native(registry: &mut Registry) {
    registry.register::<App>();
}

/// The shell: the strip on the left, the palette right of it when open,
/// the current workspace's columns on the right, the PR bar above the
/// columns. The boundaries between them drag (Bolt's `useSplitPane`): the
/// strip keeps at least 100 points, the palette at least 240 (400 until
/// dragged), the terminals at least 300, and the widths they are left at
/// are saved on release.
#[component]
pub fn App() -> impl IntoElement {
    // Bolt's editor theme for NSH's own tokens, before anything reads them.
    match nsh_base::tokens_of(Some(&theme::BOLT_THEME.tokens_json())) {
        Ok(tokens) => set_tokens(tokens),
        Err(e) => eprintln!("rivet: the editor theme's tokens: {e}"),
    }
    let shell = state::provide();
    let hub = hub::provide(shell);
    notes::provide(shell);
    group_memory::provide(shell);
    mcp::start(shell);
    let hints = hints::provide();
    let pr = pr_bar::provide(shell);
    keys(shell, hub, hints, pr);
    let paths: Vec<String> = shell.settings.with_untracked(|s| {
        s.pinned.iter().chain(&s.tabs).map(|t| t.path.clone()).chain([s.directory.clone()]).collect()
    });
    for path in paths {
        shell.refresh_status(path);
    }
    let width = signal(saved_width(SIDEBAR_WIDTH_FILE, &[settings::data_dir(), settings::bolt_data_dir()]).unwrap_or(300.0));
    let palette_width = signal(saved_width(PALETTE_WIDTH_FILE, &[settings::data_dir()]).unwrap_or(400.0));
    view()
        .flex_1()
        .relative()
        .child(
            h_resizable()
                .flex_1()
                .bg(move || theme().sidebar)
                .text_color(move || theme().foreground)
                .handle_accessibility_label("Resize the sidebar or the palette")
                .child(
                    resizable_panel()
                        .size(move || Some(px(width.get())))
                        .min_size(px(100.0))
                        .fixed(true)
                        .visible(shell.sidebar_open)
                        .on_resize(move |w: &Points| {
                            width.set(w.0);
                            save_width(SIDEBAR_WIDTH_FILE, w.0, "sidebar");
                        })
                        .child(sidebar::sidebar()),
                )
                // The palette, right of the strip while open.
                .child(
                    resizable_panel()
                        .size(move || Some(px(palette_width.get())))
                        .min_size(px(240.0))
                        .fixed(true)
                        .visible(move || hub.palette.with(Option::is_some))
                        .on_resize(move |w: &Points| {
                            palette_width.set(w.0);
                            save_width(PALETTE_WIDTH_FILE, w.0, "palette");
                        })
                        .child(view().flex_1().relative().child(hub::palette::palette())),
                )
                .child(
                    resizable_panel()
                        .min_size(px(300.0))
                        .child(view().flex_1().flex_col().child(pr_bar::bar(pr)).child(terminals::terminals())),
                ),
        )
        // ⌘⏎'s labels, over everything.
        .child(hints::overlay(hints))
}

/// The strip's width (`{ width }`), as Bolt saves it.
const SIDEBAR_WIDTH_FILE: &str = "left-pane-width.json";
/// The palette's width, saved the same way.
const PALETTE_WIDTH_FILE: &str = "palette-width.json";

/// The width saved in `file` (`{ width }`) in the first of `dirs` that has
/// one: the strip's in this app's, else the Electron Bolt's; the palette's
/// in this app's.
fn saved_width(file: &str, dirs: &[std::path::PathBuf]) -> Option<f32> {
    dirs.iter().find_map(|d| {
        let text = std::fs::read_to_string(d.join(file)).ok()?;
        serde_json::from_str::<serde_json::Value>(&text).ok()?.get("width")?.as_f64().map(|w| w as f32)
    })
}

fn save_width(file: &str, width: f32, what: &str) {
    let dir = settings::data_dir();
    let write = std::fs::create_dir_all(&dir)
        .and_then(|()| std::fs::write(dir.join(file), serde_json::json!({ "width": width.round() }).to_string()));
    if let Err(e) = write {
        eprintln!("rivet: could not save the {what} width: {e}");
    }
}

/// Bolt's keybindings (`keybindings.json` merged with the defaults,
/// [`keybindings::load`]) for the commands this shell has; the others
/// (the editor toggle, search, restart, …) bind nothing yet.
fn keys(shell: Shell, hub: hub::Hub, hints: hints::Hints, pr: pr_bar::PrBar) {
    // `Always`: a focused terminal or text input would otherwise keep ⌘⏎
    // and ⇧⌘X (its cut).
    let always = || KeyOptions { when: nsh_core::keymap::When::Always, ..Default::default() };
    for kb in keybindings::load() {
        let chord = keybindings::chord(&kb.key);
        match kb.command.as_str() {
            "bolt.palette" => on_key(&chord, move |_| hub.toggle_palette()),
            "hints.activate" => on_key_with(&chord, always(), move |_| hints.activate()),
            "sidebar.toggle" => on_key(&chord, move |_| shell.sidebar_open.update(|o| *o = !*o)),
            "pinned.toggle" => on_key(&chord, move |_| shell.cycle_collapse()),
            "pr.toggle" => on_key(&chord, move |_| pr.toggle()),
            "editor.openTerminal" => on_key(&chord, move |_| terminals::open_here(hub)),
            "editor.closeTab" => on_key(&chord, move |_| {
                if let Some(id) = shell.shown(&shell.directory()) {
                    shell.close_tab(id);
                }
            }),
            "editor.prevTab" => on_key(&chord, move |_| cycle_terminal(shell, -1)),
            "editor.nextTab" => on_key(&chord, move |_| cycle_terminal(shell, 1)),
            // The column actions, by key, until Escape.
            "panes.activate" => on_key(&chord, move |_| shell.panes_overlay.update(|o| *o = !*o)),
            // `request_close_tab` ignores a directory with no tab.
            "workspace.closeTab" => on_key_with(&chord, always(), move |_| sidebar::request_close_tab(hub, shell.directory())),
            // The code text of terminals, files and diffs, wherever the focus is.
            "view.zoomIn" => on_key_with(&chord, always(), move |_| shell.zoom_text(1.0)),
            "view.zoomOut" => on_key_with(&chord, always(), move |_| shell.zoom_text(-1.0)),
            // ⌘2–⌘9 number the rows on screen; index 0 (⌘1) is Bolt's
            // fleet view, not here.
            "workspace.switchTab" => {
                let Some(index) = kb.args.as_ref().and_then(|a| a["index"].as_u64()).filter(|i| *i > 0) else { continue };
                on_key(&chord, move |_| {
                    let (collapse, chipped) = (shell.collapse.get_untracked(), shell.chipped_paths());
                    let path = shell.settings.with_untracked(|s| {
                        numbered_rows(s, &collapse, |p| chipped.contains(p)).get(index as usize - 1).map(|t| t.path.clone())
                    });
                    if let Some(path) = path {
                        commands::submit(shell, format!("cd {path} --absolute"));
                    }
                })
            }
            _ => continue,
        };
    }
    // Not in Bolt's list: the strip's previous / next row.
    on_key("mod+alt+up", move |_| step_workspace(shell, -1));
    on_key("mod+alt+down", move |_| step_workspace(shell, 1));
}

fn step_workspace(shell: Shell, dir: isize) {
    if let Some(path) = shell.settings.with_untracked(|s| neighbor_path(s, dir)) {
        commands::submit(shell, format!("cd {path} --absolute"));
    }
}

/// The previous or next tab of the active column (`prevTabAtom`,
/// `nextTabAtom`), round from the ends.
fn cycle_terminal(shell: Shell, dir: isize) {
    let cwd = shell.directory();
    let next = shell.columns.with_untracked(|c| {
        let cols = c.get(&cwd)?;
        let g = cols.groups.iter().find(|g| g.id == cols.active_group)?;
        let n = g.tabs.len() as isize;
        (n > 0).then(|| g.tabs[(g.active as isize + dir).rem_euclid(n) as usize])
    });
    if let Some(id) = next {
        shell.show(id);
    }
}
