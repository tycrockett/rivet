//! Bolt's editor theme (`themeStore`, `services/monaco`): the Monaco theme
//! the user picked in Bolt (`theme.json`), and what Bolt derives from it —
//! the terminal's xterm colors (`monacoToXtermTheme`) and the `--theme-*`
//! values the terminal frame and notes strip are drawn with
//! (`applyThemeCssVars`). The theme files are Bolt's own, compiled in.

use std::{collections::HashMap, sync::LazyLock};

use serde::Deserialize;

use crate::settings;

const THEMES: &[(&str, &str)] = &[
    ("dracula", include_str!("../themes/Dracula.json")),
    ("monokai", include_str!("../themes/Monokai.json")),
    ("night-owl", include_str!("../themes/Night Owl.json")),
    ("github-dark", include_str!("../themes/GitHub Dark.json")),
    ("solarized-dark", include_str!("../themes/Solarized-dark.json")),
    ("nord", include_str!("../themes/Nord.json")),
    ("oceanic-next", include_str!("../themes/Oceanic Next.json")),
    ("tomorrow-night", include_str!("../themes/Tomorrow-Night.json")),
    ("cobalt2", include_str!("../themes/Cobalt2.json")),
    ("monokai-bright", include_str!("../themes/Monokai Bright.json")),
    ("atom-one-dark", include_str!("../themes/Atom One Dark.json")),
];

/// `bolt-dark` (`monacoTheme.ts`), the default, which has no JSON file.
const BOLT_DARK: &str = r##"{
  "rules": [
    { "token": "comment", "foreground": "6b7280" },
    { "token": "keyword", "foreground": "67e8f9" },
    { "token": "string", "foreground": "86efac" },
    { "token": "number", "foreground": "fbbf24" },
    { "token": "type", "foreground": "a78bfa" }
  ],
  "colors": {
    "editor.background": "#0c0c0f",
    "editor.foreground": "#e4e4e7",
    "editorCursor.foreground": "#00ffff",
    "editor.selectionBackground": "#164e63",
    "editor.lineHighlightBackground": "#18181b",
    "editorWidget.background": "#18181b",
    "editorWidget.border": "#3f3f46",
    "input.background": "#27272a"
  }
}"##;

#[derive(Deserialize, Default)]
struct Rule {
    token: Option<String>,
    foreground: Option<String>,
}

#[derive(Deserialize, Default)]
struct MonacoTheme {
    #[serde(default)]
    colors: HashMap<String, String>,
    #[serde(default)]
    rules: Vec<Rule>,
}

/// An xterm theme (`ITheme`): the 16 ANSI colors in xterm's order (black,
/// red, green, yellow, blue, magenta, cyan, white, then the bright ones).
#[derive(Debug, Clone, PartialEq)]
pub struct Xterm {
    pub background: u32,
    pub foreground: u32,
    pub cursor: u32,
    pub selection: u32,
    pub ansi: [u32; 16],
}

/// The `--theme-*` values Bolt's chrome is drawn with.
#[derive(Debug, Clone, PartialEq)]
pub struct Vars {
    pub bg: u32,
    pub bg_surface: u32,
    pub bg_hover: u32,
    pub bg_widget: u32,
    pub border: u32,
    pub fg: u32,
    pub fg_muted: u32,
    pub fg_faint: u32,
    pub accent: u32,
    pub info: u32,
    pub success: u32,
    pub error: u32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct BoltTheme {
    pub xterm: Xterm,
    pub vars: Vars,
}

impl BoltTheme {
    /// The surface everything but the terminal box is drawn on: Bolt's
    /// terminal canvas (`--tx-canvas`, the background 12% toward black), so
    /// the box, in the editor background, stands out in the same tint.
    pub fn chrome(&self) -> u32 {
        mix(self.vars.bg, 0.88, 0x000000)
    }

    /// NSH's design tokens (shadcn's `cssVars`) in this theme's colors, so
    /// the strip, the tab bar and the palette's NSH parts follow the editor
    /// theme as Bolt's `--theme-*` chrome does.
    pub fn tokens_json(&self) -> String {
        let v = &self.vars;
        let chrome = self.chrome();
        let pairs: [(&str, u32); 22] = [
            ("background", chrome),
            ("foreground", v.fg),
            ("card", v.bg_widget),
            ("card-foreground", v.fg),
            ("popover", v.bg_widget),
            ("popover-foreground", v.fg),
            ("primary", v.accent),
            ("primary-foreground", v.bg),
            ("secondary", v.bg_surface),
            ("secondary-foreground", v.fg),
            ("muted", v.bg_surface),
            ("muted-foreground", v.fg_muted),
            ("accent", v.bg_hover),
            ("accent-foreground", v.fg),
            ("destructive", v.error),
            ("border", v.border),
            ("input", v.border),
            ("ring", v.accent),
            ("sidebar", chrome),
            ("sidebar-foreground", v.fg),
            ("sidebar-accent", v.bg_surface),
            ("sidebar-accent-foreground", v.fg),
        ];
        let mut vars: serde_json::Map<String, serde_json::Value> =
            pairs.iter().map(|(k, c)| ((*k).to_owned(), format!("#{c:06x}").into())).collect();
        vars.insert("sidebar-border".into(), format!("#{:06x}", v.border).into());
        vars.insert("sidebar-primary".into(), format!("#{:06x}", v.accent).into());
        vars.insert("sidebar-ring".into(), format!("#{:06x}", v.accent).into());
        serde_json::json!({ "cssVars": { "theme": vars } }).to_string()
    }
}

fn hex(c: &str) -> Option<u32> {
    let h = c.trim_start_matches('#');
    u32::from_str_radix(h.get(..6)?, 16).ok()
}

fn channels(c: u32) -> [u32; 3] {
    [(c >> 16) & 0xff, (c >> 8) & 0xff, c & 0xff]
}

fn join([r, g, b]: [u32; 3]) -> u32 {
    (r << 16) | (g << 8) | b
}

/// Toward white by `amount` (`lighten`).
pub fn lighten(c: u32, amount: f32) -> u32 {
    join(channels(c).map(|v| (v as f32 + (255.0 - v as f32) * amount).round().min(255.0) as u32))
}

/// `color-mix(in srgb, a p, b)`.
pub fn mix(a: u32, p: f32, b: u32) -> u32 {
    let (a, b) = (channels(a), channels(b));
    join([0, 1, 2].map(|i| (a[i] as f32 * p + b[i] as f32 * (1.0 - p)).round() as u32))
}

fn build(theme: &MonacoTheme) -> BoltTheme {
    let color = |key: &str| theme.colors.get(key).and_then(|c| hex(c));
    // The last rule naming a token wins, as Bolt's map is built.
    let mut tokens: HashMap<&str, u32> = HashMap::new();
    for r in &theme.rules {
        if let (Some(t), Some(f)) = (r.token.as_deref(), r.foreground.as_deref().and_then(hex)) {
            tokens.insert(t, f);
        }
    }
    let token = |names: &[&str], fallback: u32| names.iter().find_map(|n| tokens.get(n).copied()).unwrap_or(fallback);

    // monacoToXtermTheme
    let bg = color("editor.background").unwrap_or(0x1a1b2e);
    let fg = color("editor.foreground").unwrap_or(0xa9b1d6);
    let red = token(&["invalid", "invalid.illegal"], 0xf7768e);
    let green = token(&["string", "string.quoted"], 0x9ece6a);
    let yellow = token(&["constant.numeric", "number"], 0xe0af68);
    let blue = token(&["entity.name.function", "support.function", "keyword"], 0x7aa2f7);
    let magenta = token(&["constant.language", "type", "storage.type"], 0xbb9af7);
    let cyan = token(&["entity.name.tag", "keyword.control", "keyword"], 0x7dcfff);
    let bright_black = token(&["comment"], 0x6b7280);
    let xterm = Xterm {
        background: bg,
        foreground: fg,
        cursor: color("editorCursor.foreground").unwrap_or(fg),
        selection: color("editor.selectionBackground").unwrap_or(0x33467c),
        ansi: [bg, red, green, yellow, blue, magenta, cyan, fg, bright_black, red, green, yellow, blue, magenta, cyan, fg],
    };

    // applyThemeCssVars
    let bg = color("editor.background").unwrap_or(0x0c0c0f);
    let accent = color("editorCursor.foreground").unwrap_or(0x00ffff);
    let added = color("editorGutter.addedBackground");
    let deleted = color("editorGutter.deletedBackground").unwrap_or(0xef4444);
    let modified = color("editorGutter.modifiedBackground").unwrap_or(0x3b82f6);
    let vars = Vars {
        bg,
        bg_surface: lighten(bg, 0.06),
        bg_hover: lighten(bg, 0.1),
        bg_widget: color("editorWidget.background").unwrap_or_else(|| lighten(bg, 0.04)),
        border: color("editorWidget.border").unwrap_or_else(|| lighten(bg, 0.15)),
        fg: color("editor.foreground").unwrap_or(0xe4e4e7),
        fg_muted: lighten(bg, 0.45),
        fg_faint: lighten(bg, 0.3),
        accent,
        info: color("editorInfo.foreground").unwrap_or(modified),
        success: added.unwrap_or_else(|| token(&["string"], 0x16a34a)),
        error: color("editorError.foreground").unwrap_or(deleted),
    };
    BoltTheme { xterm, vars }
}

#[derive(Deserialize)]
struct Choice {
    theme: Option<String>,
}

/// The key Bolt's `theme.json` names: this app's own, else Bolt's, else
/// `bolt-dark`.
fn chosen() -> String {
    [settings::data_dir(), settings::bolt_data_dir()]
        .iter()
        .find_map(|d| {
            let text = std::fs::read_to_string(d.join("theme.json")).ok()?;
            serde_json::from_str::<Choice>(&text).ok()?.theme
        })
        .unwrap_or_else(|| "bolt-dark".into())
}

/// The theme, read once.
pub static BOLT_THEME: LazyLock<BoltTheme> = LazyLock::new(|| {
    let key = chosen();
    let json = THEMES.iter().find(|(k, _)| *k == key).map_or(BOLT_DARK, |(_, j)| j);
    build(&serde_json::from_str(json).unwrap_or_default())
});

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn atom_one_dark_maps_as_bolts_xterm_theme_does() {
        let (_, json) = THEMES.iter().find(|(k, _)| *k == "atom-one-dark").unwrap();
        let t = build(&serde_json::from_str(json).unwrap());
        assert_eq!(t.xterm.background, 0x1e2127);
        assert_eq!(t.xterm.foreground, 0xabb2bf);
        assert_eq!(t.xterm.cursor, 0x528bff);
        // red has no `invalid` rule: xterm's fallback; blue and cyan fall to `keyword`.
        assert_eq!(t.xterm.ansi[1], 0xf7768e);
        assert_eq!(t.xterm.ansi[2], 0x98c379);
        assert_eq!(t.xterm.ansi[4], 0xc678dd);
        assert_eq!(t.xterm.ansi[8], 0x5c6370);
        assert_eq!(t.vars.bg_surface, lighten(0x1e2127, 0.06));
    }
}
