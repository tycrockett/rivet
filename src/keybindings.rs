//! Bolt's keybindings (`services/keybindings/keybindings.ts`):
//! `keybindings.json`'s `{ key, command, args }` list, merged with the
//! defaults whose keys it does not bind, read once at launch as Bolt does.
//! `when` clauses are not read: every binding here applies everywhere a
//! default one would.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::settings;

pub const FILE: &str = "keybindings.json";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct KeyBinding {
    pub key: String,
    pub command: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub when: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub args: Option<Value>,
}

fn kb(key: &str, command: &str) -> KeyBinding {
    KeyBinding { key: key.into(), command: command.into(), when: None, args: None }
}

/// `DEFAULT_KEYBINDINGS`, in Bolt's order.
pub fn defaults() -> Vec<KeyBinding> {
    let mut out: Vec<KeyBinding> = [
        ("e", "editor.toggle"),
        ("w", "editor.closeTab"),
    ]
    .iter()
    .flat_map(|(k, c)| [kb(&format!("cmd+{k}"), c), kb(&format!("ctrl+{k}"), c)])
    .collect();
    out.extend([kb("cmd+shift+[", "editor.prevTab"), kb("cmd+shift+]", "editor.nextTab")]);
    for (k, c) in [
        ("b", "sidebar.toggle"),
        ("shift+p", "pr.toggle"),
        ("shift+f", "search.toggle"),
        ("shift+o", "branch.openRemote"),
        ("'", "panes.activate"),
        (";", "bolt.palette"),
        ("t", "terminal.toggleGraph"),
        ("`", "editor.openTerminal"),
        ("shift+y", "sidebar.nudgeWidth"),
        ("shift+r", "app.restart"),
        ("enter", "hints.activate"),
        ("0", "pinned.toggle"),
        ("shift+x", "workspace.closeTab"),
    ] {
        out.extend([kb(&format!("cmd+{k}"), c), kb(&format!("ctrl+{k}"), c)]);
    }
    out.push(kb("escape", "input.clear"));
    for i in 0..9 {
        for m in ["cmd", "ctrl"] {
            out.push(KeyBinding { args: Some(serde_json::json!({ "index": i })), ..kb(&format!("{m}+{}", i + 1), "workspace.switchTab") });
        }
    }
    out
}

/// The file Rivet reads and the Settings tab opens: this app's.
pub fn path() -> PathBuf {
    settings::data_dir().join(FILE)
}

/// `loadKeybindingsAtom`: this app's file, else the Electron Bolt's, else
/// the defaults; a file's list gets the defaults whose keys it lacks.
pub fn load() -> Vec<KeyBinding> {
    let stored = [path(), settings::bolt_data_dir().join(FILE)].iter().find_map(|p| {
        let list: Vec<KeyBinding> = serde_json::from_str(&std::fs::read_to_string(p).ok()?).ok()?;
        (!list.is_empty()).then_some(list)
    });
    match stored {
        Some(list) => merge(list),
        None => defaults(),
    }
}

fn merge(mut list: Vec<KeyBinding>) -> Vec<KeyBinding> {
    let missing: Vec<KeyBinding> = defaults().into_iter().filter(|d| !list.iter().any(|k| k.key == d.key)).collect();
    list.extend(missing);
    list
}

/// Writes the defaults to this app's file unless it already lists
/// bindings (the Settings tab's "Open keybindings.json"), seeded from the
/// Electron Bolt's when that has some.
pub fn ensure_file() -> std::io::Result<PathBuf> {
    let p = path();
    let has = std::fs::read_to_string(&p).ok().and_then(|t| serde_json::from_str::<Vec<KeyBinding>>(&t).ok()).is_some_and(|l| !l.is_empty());
    if !has {
        std::fs::create_dir_all(settings::data_dir())?;
        std::fs::write(&p, serde_json::to_string_pretty(&load()).unwrap_or_default())?;
    }
    Ok(p)
}

/// Bolt's key string as NSH's chord: `cmd` is ⌘ on macOS and Control
/// elsewhere (`parseKeyCombo`), which is NSH's `mod`.
pub fn chord(key: &str) -> String {
    key.to_lowercase()
        .split('+')
        .map(|p| match p {
            "cmd" | "meta" => "mod",
            "control" => "ctrl",
            "opt" | "option" => "alt",
            "backquote" => "`",
            "backslash" => "\\",
            other => other,
        })
        .collect::<Vec<_>>()
        .join("+")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_file_keeps_its_bindings_and_gains_the_defaults_it_lacks() {
        let merged = merge(vec![kb("cmd+b", "pinned.toggle")]);
        assert_eq!(merged[0], kb("cmd+b", "pinned.toggle"));
        assert_eq!(merged.iter().filter(|k| k.key == "cmd+b").count(), 1);
        assert!(merged.iter().any(|k| k.key == "cmd+;" && k.command == "bolt.palette"));
    }

    #[test]
    fn bolt_keys_become_nsh_chords() {
        assert_eq!(chord("cmd+shift+["), "mod+shift+[");
        assert_eq!(chord("Ctrl+Backquote"), "ctrl+`");
    }
}
