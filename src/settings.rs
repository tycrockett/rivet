//! Bolt's `settings.json`: the workspace tabs, the current directory,
//! bookmarks and `/custom` commands, in Bolt's own schema (`src/types/app.ts`)
//! so the file stays readable by both apps. Fields this app does not know
//! are kept as they were.

use std::{
    collections::BTreeMap,
    fs, io,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// A workspace tab (Bolt's `Tab`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Tab {
    pub name: String,
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub hidden: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub home: bool,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

fn is_false(b: &bool) -> bool {
    !*b
}

impl Tab {
    pub fn new(name: impl Into<String>, path: impl Into<String>) -> Self {
        Tab {
            name: name.into(),
            path: path.into(),
            color: None,
            hidden: false,
            group: None,
            home: false,
            extra: Map::new(),
        }
    }
}

/// Legacy settings store tabs as bare path strings (`normalizeTab`).
#[derive(Deserialize)]
#[serde(untagged)]
enum TabOrPath {
    Tab(Tab),
    Path(String),
}

fn tabs<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Vec<Tab>, D::Error> {
    let raw: Option<Vec<TabOrPath>> = Option::deserialize(d)?;
    Ok(raw
        .unwrap_or_default()
        .into_iter()
        .map(|t| match t {
            TabOrPath::Tab(tab) => tab,
            TabOrPath::Path(path) => {
                Tab::new(path.rsplit('/').next().unwrap_or(&path).to_owned(), path)
            }
        })
        .collect())
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Settings {
    #[serde(default)]
    pub directory: String,
    #[serde(default)]
    pub home: String,
    #[serde(default, deserialize_with = "tabs")]
    pub tabs: Vec<Tab>,
    #[serde(default, deserialize_with = "tabs")]
    pub pinned: Vec<Tab>,
    #[serde(default)]
    pub groups: Vec<String>,
    /// Each group's id, by its name: what names the group's memory folder,
    /// so a rename keeps it (`assign_group_ids`).
    #[serde(default, rename = "groupIds", skip_serializing_if = "BTreeMap::is_empty")]
    pub group_ids: BTreeMap<String, String>,
    #[serde(default)]
    pub bookmarks: BTreeMap<String, String>,
    #[serde(default)]
    pub commands: BTreeMap<String, String>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl Settings {
    /// Gives every group an id and drops the ids of groups that are gone,
    /// returning whether any changed. Runs on load and on every settings
    /// write, so a group made anywhere has one; a rename carries its id
    /// over itself.
    pub fn assign_group_ids(&mut self) -> bool {
        let before = self.group_ids.len();
        self.group_ids.retain(|name, _| self.groups.contains(name));
        let mut changed = self.group_ids.len() != before;
        for g in &self.groups {
            if !self.group_ids.contains_key(g) {
                self.group_ids.insert(g.clone(), new_group_id());
                changed = true;
            }
        }
        changed
    }

    /// Renames group `old` to `new`, its tabs and its id with it, so its
    /// memory folder stays its own.
    pub fn rename_group(&mut self, old: &str, new: &str) {
        for g in &mut self.groups {
            if g == old {
                new.clone_into(g);
            }
        }
        for t in &mut self.tabs {
            if t.group.as_deref() == Some(old) {
                t.group = Some(new.to_owned());
            }
        }
        if let Some(id) = self.group_ids.remove(old) {
            self.group_ids.insert(new.to_owned(), id);
        }
    }
}

/// Unique on this machine: the time in nanoseconds, then a counter for ids
/// made in the same nanosecond.
fn new_group_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNT: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos());
    format!("{nanos:x}-{:x}", COUNT.fetch_add(1, Ordering::Relaxed))
}

/// The tab name Bolt gives a directory: its leaf, without a
/// `<project>--` prefix (`tabNameFor`).
pub fn tab_name_for(path: &str) -> String {
    let leaf = path.rsplit('/').next().filter(|s| !s.is_empty()).unwrap_or(path);
    leaf.rsplit("--").next().unwrap_or(leaf).to_owned()
}

/// Where this app keeps its settings: `$RIVET_DATA_DIR`, else the
/// platform's data directory under `Rivet`. Its own file, so it never
/// races the Electron app writing `Bolt/settings.json`.
pub fn data_dir() -> PathBuf {
    std::env::var_os("RIVET_DATA_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| dirs::data_dir().unwrap_or_else(std::env::temp_dir).join("Rivet"))
}

/// Moves the data directory of this app's old name (`Bolt Native`) to
/// `Rivet` the first time Rivet runs without `$RIVET_DATA_DIR`, so its
/// settings, hub thread and saved terminals carry over. The old daemon's
/// link (`bolt-ptyd`) is dropped; a live daemon keeps its open file.
pub fn adopt_legacy_data_dir() {
    if std::env::var_os("RIVET_DATA_DIR").is_some() {
        return;
    }
    let Some(base) = dirs::data_dir() else { return };
    let (old, new) = (base.join("Bolt Native"), base.join("Rivet"));
    if new.exists() || !old.is_dir() {
        return;
    }
    match std::fs::rename(&old, &new) {
        Ok(()) => {
            let _ = std::fs::remove_file(new.join("bolt-ptyd"));
        }
        Err(e) => eprintln!("rivet: could not move {} to {}: {e}", old.display(), new.display()),
    }
}

/// The Electron Bolt's data directory (`app.getPath("userData")`, or
/// `BOLT_USER_DATA_DIR` as Bolt's `main.js` honours it): what seeds ours,
/// and where the folders shared with its agents live (`bolt-notes`,
/// `group-sessions`).
pub fn bolt_data_dir() -> PathBuf {
    std::env::var_os("BOLT_USER_DATA_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| dirs::data_dir().unwrap_or_else(std::env::temp_dir).join("Bolt"))
}

fn bolt_settings_path() -> PathBuf {
    bolt_data_dir().join("settings.json")
}

fn settings_path() -> PathBuf {
    data_dir().join("settings.json")
}

fn home() -> String {
    dirs::home_dir().map(|p| p.to_string_lossy().into_owned()).unwrap_or_else(|| "/".into())
}

fn read(path: &Path) -> io::Result<Settings> {
    let text = fs::read_to_string(path)?;
    serde_json::from_str(&text).map_err(io::Error::other)
}

/// Loads our settings, seeding them from Bolt's the first time. A path in
/// both `pinned` and `tabs` stays pinned, as Bolt's loader keeps it.
pub fn load() -> Settings {
    let mut settings = read(&settings_path())
        .or_else(|_| read(&bolt_settings_path()))
        .unwrap_or_else(|_| Settings {
            directory: String::new(),
            home: String::new(),
            tabs: Vec::new(),
            pinned: Vec::new(),
            groups: Vec::new(),
            group_ids: BTreeMap::new(),
            bookmarks: BTreeMap::new(),
            commands: BTreeMap::new(),
            extra: Map::new(),
        });
    if settings.home.is_empty() {
        settings.home = home();
    }
    if settings.directory.is_empty() || !Path::new(&settings.directory).is_dir() {
        settings.directory = settings.home.clone();
    }
    let pinned: Vec<String> = settings.pinned.iter().map(|t| t.path.clone()).collect();
    settings.tabs.retain(|t| !pinned.contains(&t.path));
    // An id made here but never written would be a new one next launch,
    // and the group's memories would stay behind in the old folder.
    if settings.assign_group_ids()
        && let Err(e) = save(&settings)
    {
        eprintln!("rivet: could not save settings: {e}");
    }
    settings
}

/// Writes `settings` atomically: a temporary file renamed over the old one.
pub fn save(settings: &Settings) -> io::Result<()> {
    let path = settings_path();
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension(format!("json.{}.tmp", std::process::id()));
    fs::write(&tmp, serde_json::to_vec_pretty(settings).map_err(io::Error::other)?)?;
    fs::rename(tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_unknown_fields_and_reads_legacy_string_tabs() {
        let json = r##"{"directory":"/a","home":"/h","tabs":["/x/y",{"name":"n","path":"/p","color":"#fff","lane":1}],"minimap":true}"##;
        let s: Settings = serde_json::from_str(json).unwrap();
        assert_eq!(s.tabs[0].name, "y");
        assert_eq!(s.tabs[0].path, "/x/y");
        let out = serde_json::to_value(&s).unwrap();
        assert_eq!(out["minimap"], Value::Bool(true));
        assert_eq!(out["tabs"][1]["lane"], 1);
        assert!(out["tabs"][0].get("hidden").is_none());
    }

    #[test]
    fn tab_names_drop_the_project_prefix() {
        assert_eq!(tab_name_for("/w/backend/mem-1389--08-metabase-view"), "08-metabase-view");
        assert_eq!(tab_name_for("/w/builder"), "builder");
    }
}
