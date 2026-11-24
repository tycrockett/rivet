//! Paths as the app shows them: the home directory as `~`.

use std::sync::LazyLock;

static HOME: LazyLock<String> =
    LazyLock::new(|| dirs::home_dir().map(|p| p.to_string_lossy().trim_end_matches('/').to_owned()).unwrap_or_default());

/// `text` with every path under the home directory starting `~`: a whole
/// path, or paths inside a message or a terminal title. The home counts only
/// where a path starts and ends with it, so `/Users/me2` stays.
pub fn tilde(text: &str) -> String {
    tilde_in(text, &HOME)
}

fn tilde_in(text: &str, home: &str) -> String {
    if home.is_empty() {
        return text.to_owned();
    }
    let part = |c: char| c.is_alphanumeric() || matches!(c, '/' | '.' | '-' | '_' | '~');
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find(home) {
        let (before, after) = (&rest[..at], &rest[at + home.len()..]);
        let starts = !before.chars().next_back().is_some_and(part);
        let ends = after.chars().next().is_none_or(|c| c == '/' || !part(c));
        out.push_str(before);
        out.push_str(if starts && ends { "~" } else { home });
        rest = after;
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::tilde_in;

    #[test]
    fn home_becomes_tilde_only_as_a_whole_path_prefix() {
        let h = "/Users/me";
        assert_eq!(tilde_in("/Users/me", h), "~");
        assert_eq!(tilde_in("/Users/me/.worktrees/a", h), "~/.worktrees/a");
        assert_eq!(tilde_in("Created worktree at /Users/me/w/b — ok", h), "Created worktree at ~/w/b — ok");
        assert_eq!(tilde_in("cd \"/Users/me/x\" then /Users/me", h), "cd \"~/x\" then ~");
        assert_eq!(tilde_in("/Users/me2/x", h), "/Users/me2/x");
        assert_eq!(tilde_in("/mnt/Users/me/x", h), "/mnt/Users/me/x");
        assert_eq!(tilde_in("/tmp/x", h), "/tmp/x");
    }
}
