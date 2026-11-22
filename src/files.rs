//! A workspace's files for the palette's `/open` completion: listed once
//! per palette opening, matched fuzzily on every keystroke.

use std::path::Path;

/// More files than this are cut off: matching runs on each keystroke.
const MAX_FILES: usize = 50_000;

/// Folders a walk outside git skips: dependencies, build output, VCS data.
const SKIPPED: &[&str] = &["node_modules", "target", "dist", "build", "out", ".git"];

/// The files under `dir`, relative to it: git's tracked and untracked
/// ones minus the ignored (`git ls-files`), or, outside a repository, a
/// walk that skips hidden and generated folders.
pub fn list(dir: &str) -> Vec<String> {
    let listed = crate::git::git(&["ls-files", "--cached", "--others", "--exclude-standard"], dir);
    let mut files: Vec<String> = match listed {
        Ok(out) => out.lines().filter(|l| !l.is_empty()).take(MAX_FILES).map(str::to_owned).collect(),
        Err(_) => {
            let mut files = Vec::new();
            walk(Path::new(dir), Path::new(dir), &mut files);
            files
        }
    };
    // Deleted but still tracked files are listed by git; there is nothing
    // to open.
    files.retain(|f| Path::new(dir).join(f).is_file());
    files
}

fn walk(root: &Path, dir: &Path, out: &mut Vec<String>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        if out.len() >= MAX_FILES {
            return;
        }
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with('.') || SKIPPED.contains(&name.as_ref()) {
            continue;
        }
        let path = entry.path();
        match entry.file_type() {
            Ok(t) if t.is_dir() => walk(root, &path, out),
            Ok(t) if t.is_file() => {
                if let Ok(rel) = path.strip_prefix(root) {
                    out.push(rel.to_string_lossy().into_owned());
                }
            }
            _ => {}
        }
    }
}

/// The `limit` best of `files` for `query`, best first. A file matches
/// when the query's characters appear in its path in order, ignoring case;
/// matches in the file name, at the start of a name or word, and in a run
/// rank higher, then shorter paths. An empty query gives the shallowest
/// files.
pub fn matches<'a>(query: &str, files: &'a [String], limit: usize) -> Vec<&'a str> {
    let query: Vec<u8> = query.bytes().map(|b| b.to_ascii_lowercase()).collect();
    let mut scored: Vec<(i32, &str)> = files.iter().filter_map(|f| score(&query, f).map(|s| (s, f.as_str()))).collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.len().cmp(&b.1.len())).then(a.1.cmp(b.1)));
    scored.into_iter().take(limit).map(|(_, f)| f).collect()
}

/// How well `query` (lowercase) matches `path`, or `None` when it does
/// not: the best placement of its characters, each scoring for being in
/// the file name, at a word's start (after `/ _ - .` or a lower-to-upper
/// case change) and right after the previous one.
fn score(query: &[u8], path: &str) -> Option<i32> {
    let bytes = path.as_bytes();
    if !is_subsequence(query, bytes) {
        return None;
    }
    let name_start = path.rfind('/').map_or(0, |i| i + 1);
    let bonus = |i: usize| {
        let mut b = 1;
        if i >= name_start {
            b += 4;
        }
        let starts = i == 0
            || matches!(bytes[i - 1], b'/' | b'_' | b'-' | b'.' | b' ')
            || (bytes[i - 1].is_ascii_lowercase() && bytes[i].is_ascii_uppercase());
        if starts {
            b += 6;
        }
        b
    };
    // `row[i]`: the best score with the query so far ending on byte `i`.
    let mut row: Vec<Option<i32>> =
        (0..bytes.len()).map(|i| query.first().filter(|q| bytes[i].to_ascii_lowercase() == **q).map(|_| bonus(i))).collect();
    for q in query.iter().skip(1) {
        let mut next = vec![None; bytes.len()];
        // The best of the row up to two bytes back: a gap before byte `i`.
        let mut best_before: Option<i32> = None;
        for i in 1..bytes.len() {
            if i >= 2 {
                best_before = best_before.max(row[i - 2]);
            }
            if bytes[i].to_ascii_lowercase() != *q {
                continue;
            }
            let run = row[i - 1].map(|s| s + 5);
            next[i] = run.max(best_before).map(|s| s + bonus(i));
        }
        row = next;
    }
    let best = if query.is_empty() { 0 } else { row.into_iter().flatten().max()? };
    Some(best * 4 - bytes.len() as i32 / 8)
}

fn is_subsequence(query: &[u8], bytes: &[u8]) -> bool {
    let mut q = query.iter().peekable();
    for b in bytes {
        if q.peek().is_some_and(|c| **c == b.to_ascii_lowercase()) {
            q.next();
        }
    }
    q.peek().is_none()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn files(paths: &[&str]) -> Vec<String> {
        paths.iter().map(|p| (*p).to_owned()).collect()
    }

    #[test]
    fn the_file_name_beats_a_folder_that_spells_the_query() {
        let all = files(&["src/main/other.rs", "src/main.rs", "docs/maintenance.md"]);
        assert_eq!(matches("main", &all, 3)[0], "src/main.rs");
    }

    #[test]
    fn characters_in_order_match_across_the_path_ignoring_case() {
        let all = files(&["native/src/viewer.rs", "native/src/state.rs", "README.md"]);
        assert_eq!(matches("NSV", &all, 5), vec!["native/src/viewer.rs"]);
        assert!(matches("xyz", &all, 5).is_empty());
    }

    #[test]
    fn word_starts_beat_letters_inside_words() {
        let all = files(&["src/stores/editorStore.ts", "src/settings_restore.ts"]);
        assert_eq!(matches("es", &all, 2)[0], "src/stores/editorStore.ts");
    }

    #[test]
    fn an_empty_query_lists_the_shortest_paths_first() {
        let all = files(&["a/b/c.rs", "z.rs", "a/b.rs"]);
        assert_eq!(matches("", &all, 2), vec!["z.rs", "a/b.rs"]);
    }
}
