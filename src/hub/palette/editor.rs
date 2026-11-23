//! The palette's input (`components/Editor/ScriptEditor.tsx`): a
//! multi-line field whose Return sends and Shift+Return breaks the line,
//! with a menu that opens upward at the caret for `/` commands, `:` Bolt
//! commands, `#` script words, `{vars}`, `$var:types` and `@` handles.

use std::rc::Rc;

use nsh_base::{input, theme};
use nsh_core::{keymap::When, prelude::*};

use super::help::HELP;
use crate::hub::{HubTarget, engine};

/// What `#` offers at a line's start (`NODE_WORDS`).
const NODE_WORDS: &[(&str, &str)] = &[
    ("ask", "[live] [{t1}] \"<question>\" [as $var[:type]] — no target asks a fresh claude, :type parses the answer"),
    ("send", "\"<text>\" to {t1} [as $var] — types it into the terminal, as saves the output"),
    ("read", "{t1} [as $var] — latest claude message or shell output"),
    ("create", "{t1} terminal | claude | worktree …, tab \"path\", group \"name\""),
    ("for-each", "{list} as $item — indent the body"),
    ("if", "{var} empty | not-empty | = value"),
    ("else", "under an #if, same indent"),
    ("alert", "\"<message>\" [wait] — desktop notification, wait holds for Continue"),
    ("set", "$var \"<value>\""),
    ("run", "[{t1}] <palette command> — e.g. sync, fetch, push; runs in t1, else the open workspace"),
];
const TYPES: &[&str] = &["string", "string[]", "number", "boolean"];

/// `#create`'s objects: whether one takes a target, and its words
/// (`CREATE_OBJECTS`' `target` and `keys`); the hints come from the engine.
const CREATE_OBJECTS: &[(&str, bool, &[&str])] = &[
    ("terminal", true, &[]),
    ("claude", true, &[]),
    ("worktree", true, &["from", "in"]),
    ("tab", false, &["in"]),
    ("group", false, &["with"]),
];

/// The input's pill and the posted bubble share these, so a line reads
/// the same before and after sending (`PREFIX_COLORS`).
pub fn prefix_color(c: char) -> Option<Rgba> {
    match c {
        '/' => Some(rgb(0x98c379)),
        '#' => Some(rgb(0xc678dd)),
        ':' => Some(rgb(0x56b6c2)),
        _ => None,
    }
}

/// One line of the menu: what it inserts, what it shows, and its hint.
#[derive(Debug, Clone, PartialEq)]
pub struct Item {
    pub keyword: String,
    pub hint: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Suggestion {
    pub insert: String,
    pub label: String,
    pub hint: String,
}

/// Where a suggestion replaces from (a byte of the text) and what it offers.
#[derive(Debug, Clone, PartialEq)]
pub struct Menu {
    pub from: usize,
    pub items: Vec<Suggestion>,
}

fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

fn all(s: &str, ok: impl Fn(char) -> bool) -> bool {
    s.chars().all(ok)
}

fn sug(insert: impl Into<String>, label: impl Into<String>, hint: impl Into<String>) -> Suggestion {
    Suggestion { insert: insert.into(), label: label.into(), hint: hint.into() }
}

/// The token at the start of `s` that `{var}` or `@handle` name, and what
/// follows its whitespace: `#run`'s optional target.
fn strip_target(s: &str) -> &str {
    let end = s.find(char::is_whitespace).unwrap_or(s.len());
    let tok = &s[..end];
    let braced = tok.len() > 2 && tok.starts_with('{') && tok.ends_with('}') && all(&tok[1..tok.len() - 1], |c| is_word(c) || c == '.' || c == '-');
    let handle = tok.len() > 1 && tok.starts_with('@') && all(&tok[1..], |c| is_word(c) && c != '_' || c == '/' || c == '-');
    if (braced || handle) && end < s.len() { s[end..].trim_start() } else { s }
}

/// The part of `line` after `#<word>` and the whitespace behind it.
fn after_word<'a>(line: &'a str, word: &str) -> Option<&'a str> {
    let rest = line.trim_start_matches([' ', '\t']).strip_prefix('#')?.strip_prefix(word)?;
    let trimmed = rest.trim_start();
    (trimmed.len() < rest.len()).then_some(trimmed)
}

/// What the menu offers for the text before the caret (`suggestions`).
/// `files` are the workspace's, relative to it, for `/open`.
pub fn suggestions(
    value: &str,
    caret: usize,
    commands: &[Item],
    scripts: &[Item],
    targets: &[String],
    handles: &[HubTarget],
    files: &[String],
) -> Option<Menu> {
    let before = &value[..caret];
    let line_start = before.rfind('\n').map_or(0, |i| i + 1);
    let line = &before[line_start..];

    fn exact_first<'a>(mut items: Vec<&'a Item>, typed: &str) -> Vec<&'a Item> {
        items.sort_by_key(|c| std::cmp::Reverse(c.keyword == typed));
        items
    }

    if let Some(typed) = before.strip_prefix('/').filter(|t| all(t, |c| is_word(c) || c == '.' || c == '-')) {
        let matched = commands.iter().filter(|c| c.keyword.starts_with(typed)).collect();
        let items = exact_first(matched, typed)
            .into_iter()
            .map(|c| sug(format!("/{} ", c.keyword), format!("/{}", c.keyword), c.hint.clone()))
            .collect();
        return Some(Menu { from: 0, items });
    }
    // `/open <path>`: the workspace's files, fuzzily, the name with its
    // folder as the hint; a path with spaces goes in quotes.
    if let Some(query) = before.strip_prefix("/open ").map(str::trim_start).filter(|q| !q.contains(char::is_whitespace) && !q.starts_with('"')) {
        let items = crate::files::matches(query, files, 12)
            .into_iter()
            .map(|f| {
                let (dir, name) = f.rsplit_once('/').unwrap_or(("", f));
                let arg = if f.contains(' ') { format!("\"{f}\"") } else { f.to_owned() };
                sug(format!("/open {arg} "), name, dir)
            })
            .collect();
        return Some(Menu { from: 0, items });
    }
    if let Some(typed) = before.strip_prefix(':').filter(|t| all(t, |c| is_word(c) || c == '-')) {
        let matched = scripts.iter().filter(|c| c.keyword.starts_with(typed)).collect();
        let items = exact_first(matched, typed)
            .into_iter()
            .map(|c| sug(format!(":{} ", c.keyword), format!(":{}", c.keyword), c.hint.clone()))
            .collect();
        return Some(Menu { from: 0, items });
    }
    if let Some(typed) = line.trim_start_matches([' ', '\t']).strip_prefix('#').filter(|t| all(t, |c| is_word(c) || c == '-')) {
        let f = typed.to_lowercase();
        let items = NODE_WORDS
            .iter()
            .filter(|(w, _)| w.to_lowercase().starts_with(&f))
            .map(|(w, hint)| sug(format!("#{w} "), format!("#{w}"), *hint))
            .collect();
        return Some(Menu { from: caret - typed.len() - 1, items });
    }
    if let Some(rest) = after_word(line, "create") {
        let (first, typed) = match rest.find(char::is_whitespace) {
            None => (None, Some(rest)),
            Some(i) => (Some(&rest[..i]), Some(rest[i..].trim_start()).filter(|t| !t.contains(char::is_whitespace))),
        };
        if let Some(typed) = typed.filter(|t| all(t, is_word)) {
            if !first.is_some_and(|f| CREATE_OBJECTS.iter().any(|(o, ..)| *o == f)) {
                let hints = engine::create_words();
                let hint = |name: &str| hints.iter().find(|(w, _)| *w == name).map_or("", |(_, h)| *h).to_owned();
                let items = CREATE_OBJECTS
                    .iter()
                    .filter(|(o, target, _)| o.starts_with(typed) && (*target || first.is_none()))
                    .map(|(o, ..)| sug(format!("{o} "), *o, hint(o)))
                    .collect();
                return Some(Menu { from: caret - typed.len(), items });
            }
        }
    }
    if let Some(rest) = after_word(line, "run") {
        let rest = strip_target(rest);
        if all(rest, |c| is_word(c) || c == '-') {
            let items = HELP
                .iter()
                .filter(|(name, ..)| name.starts_with(rest))
                .map(|(name, description, _)| sug(format!("{name} "), *name, *description))
                .collect();
            return Some(Menu { from: caret - rest.len(), items });
        }
        let tokens: Vec<&str> = rest.split_whitespace().collect();
        if let Some(&flag) = tokens.last().filter(|t| t.starts_with('-') && all(&t[1..], |c| is_word(c) || c == '-')) {
            if tokens.len() >= 2 && line.ends_with(flag) && line[..line.len() - flag.len()].ends_with(char::is_whitespace) {
                if let Some((name, _, flags)) = HELP.iter().find(|(n, ..)| *n == tokens[0]) {
                    let used: Vec<&str> = line.split_whitespace().collect();
                    let items = flags
                        .iter()
                        .filter(|f| f.starts_with(flag) && !used.contains(f))
                        .map(|f| sug(format!("{f} "), *f, *name))
                        .collect();
                    return Some(Menu { from: caret - flag.len(), items });
                }
            }
        }
    }
    if let Some(create) = after_word(line, "create") {
        let key = {
            let at = line.trim_end_matches(|c: char| is_word(c) || c == '-').len();
            let prev_ok = line[..at].chars().next_back().is_none_or(char::is_whitespace);
            prev_ok.then(|| &line[at..])
        };
        let spec = CREATE_OBJECTS.iter().find(|(o, ..)| create.split_whitespace().any(|w| w == *o));
        if let (Some(key), Some((_, _, keys))) = (key, spec) {
            if line.matches('"').count() % 2 == 0 {
                let hints = engine::create_words();
                let used: Vec<&str> = line.split_whitespace().collect();
                let mut items: Vec<Suggestion> = keys
                    .iter()
                    .filter(|k| !used.contains(k))
                    .map(|k| sug(format!("{k} "), *k, hints.iter().find(|(w, _)| w == k).map_or("", |(_, h)| *h)))
                    .collect();
                let words: Vec<&str> = line.split_whitespace().collect();
                let saved = words.windows(2).any(|w| w[0] == "as" && w[1].starts_with('$'));
                if !saved {
                    items.push(sug("as $", "as $name", "save what it made"));
                }
                items.retain(|i| i.insert.starts_with(key));
                if !items.is_empty() {
                    return Some(Menu { from: caret - key.len(), items });
                }
            }
        }
    }
    if let Some(open) = before.rfind('{') {
        let typed = &before[open + 1..];
        if all(typed, |c| is_word(c) || c == '.' || c == '-') {
            let known = known_vars(value, targets);
            let items = known
                .iter()
                .filter(|v| v.starts_with(typed))
                .map(|v| {
                    let hint = if targets.contains(v) { "target" } else if v == "input" { "text after the targets" } else { "var" };
                    sug(format!("{{{v}}}"), format!("{{{v}}}"), hint)
                })
                .collect();
            return Some(Menu { from: caret - typed.len() - 1, items });
        }
    }
    if let Some(colon) = before.rfind(':') {
        let typed = &before[colon + 1..];
        let head = &before[..colon];
        let name_len = head.len() - head.trim_end_matches(|c: char| is_word(c) || c == '.' || c == '-').len();
        if all(typed, |c| is_word(c) || c == '[' || c == ']') && name_len > 0 && head[..head.len() - name_len].ends_with('$') {
            let items = TYPES.iter().filter(|t| t.starts_with(typed)).map(|t| sug(format!("{t} "), *t, "type")).collect();
            return Some(Menu { from: caret - typed.len(), items });
        }
    }
    let typed_len = before.len() - before.trim_end_matches(|c: char| c.is_ascii_alphanumeric() || c == '/' || c == '-').len();
    let head = &before[..before.len() - typed_len];
    if head.ends_with('@') && head[..head.len() - 1].chars().next_back().is_none_or(char::is_whitespace) {
        let typed = &before[before.len() - typed_len..];
        let lower = typed.to_lowercase();
        let mut items = Vec::new();
        if before == format!("@{typed}") && "bolt".starts_with(&lower) {
            items.push(sug("@bolt ", "@bolt", "ask claude to drive Bolt"));
        }
        items.extend(
            handles
                .iter()
                .filter(|h| h.handle.contains(&lower))
                .take(8)
                .map(|h| sug(format!("@{} ", h.handle), format!("@{}", h.handle), h.label.clone())),
        );
        return Some(Menu { from: caret - typed.len() - 1, items });
    }
    None
}

/// The `{vars}` a line may use: the targets, `input` and each `$var` it
/// declares (`known`).
pub fn known_vars(value: &str, targets: &[String]) -> Vec<String> {
    let mut known: Vec<String> = targets.to_vec();
    known.push("input".into());
    let mut rest = value;
    while let Some(i) = rest.find('$') {
        rest = &rest[i + 1..];
        let name: String = rest.chars().take_while(|c| is_word(*c) || *c == '.' || *c == '-').collect();
        if !name.is_empty() && !known.contains(&name) {
            known.push(name);
        }
    }
    known
}

/// A UTF-16 offset (what the input reports) as a byte of `s`.
fn byte_of(s: &str, utf16: usize) -> usize {
    let mut units = 0;
    for (i, c) in s.char_indices() {
        if units >= utf16 {
            return i;
        }
        units += c.len_utf16();
    }
    s.len()
}

fn utf16_of(s: &str, byte: usize) -> u32 {
    s[..byte].encode_utf16().count() as u32
}

/// The input and its menu. `on_enter` sends; `on_tab` lets the palette use
/// Tab (the finder's folder completion) and answers whether it did.
pub struct Editor {
    pub value: Signal<String>,
    pub commands: Vec<Item>,
    pub scripts: Memo<Vec<Item>>,
    pub targets: Memo<Vec<HubTarget>>,
    /// The workspace's files, for `/open` (empty until listed).
    pub files: Signal<Rc<Vec<String>>>,
    pub on_enter: Rc<dyn Fn()>,
    pub on_tab: Rc<dyn Fn() -> bool>,
    pub input_ref: ElementRef,
    /// The keyword the plan found, as bytes at the line's start.
    pub lead: Memo<usize>,
    pub close: Rc<dyn Fn()>,
    /// What the empty input says: the finder's lines while it is open.
    pub placeholder: Memo<&'static str>,
}

pub fn editor(e: Editor) -> impl IntoElement {
    let Editor { value, commands, scripts, targets, files, on_enter, on_tab, input_ref, lead, close, placeholder } = e;
    let commands = Rc::new(commands);
    let caret = signal(0_usize);
    let sel = signal(0_usize);
    let dismissed = signal(None::<usize>);
    let focused = signal(false);
    let caret_x = signal(0.0_f32);
    let height = signal(44.0_f32);
    let force = signal_with((0_u32, 0_u32), SignalEquals::Never);

    let menu = computed({
        let commands = commands.clone();
        move || {
            let v = value.get();
            let at = byte_of(&v, caret.get()).min(v.len());
            let handles = targets.get();
            let this = vec!["this".to_owned()];
            files.with(|f| suggestions(&v, at, &commands, &scripts.get(), &this, &handles, f))
        }
    });
    let open = computed(move || {
        let m = menu.get()?;
        (focused.get() && !m.items.is_empty() && dismissed.get() != Some(m.from)).then_some(m)
    });

    let accept = {
        move |s: &Suggestion| {
            let Some(m) = open.get_untracked() else {
                return;
            };
            let v = value.get_untracked();
            let at = byte_of(&v, caret.get_untracked()).min(v.len());
            let next = format!("{}{}{}", &v[..m.from], s.insert, &v[at..]);
            let to = m.from + s.insert.len();
            let units = utf16_of(&next, to);
            value.set(next);
            caret.set(units as usize);
            force.set((units, units));
            input_ref.focus();
        }
    };

    on_key_with("tab", KeyOptions { when: When::Always, ..Default::default() }, {
        let on_tab = on_tab.clone();
        move |_| {
            if let Some(m) = open.get_untracked() {
                let s = m.items[sel.get_untracked().min(m.items.len() - 1)].clone();
                accept(&s);
            } else if !on_tab() {
                let v = value.get_untracked();
                let at = byte_of(&v, caret.get_untracked()).min(v.len());
                let units = utf16_of(&v, at) + 2;
                value.set(format!("{}  {}", &v[..at], &v[at..]));
                caret.set(units as usize);
                force.set((units, units));
            }
        }
    });
    on_key_with("escape", KeyOptions { when: When::Always, ..Default::default() }, move |_| {
        if let Some(m) = open.get_untracked() {
            dismissed.set(Some(m.from));
        } else {
            close();
        }
    });

    let submit = {
        let on_enter = on_enter.clone();
        move |_: &str| {
            if let Some(m) = open.get_untracked() {
                let s = m.items[sel.get_untracked().min(m.items.len() - 1)].clone();
                let v = value.get_untracked();
                let at = byte_of(&v, caret.get_untracked()).min(v.len());
                if s.label == v[..at] {
                    on_enter();
                } else {
                    accept(&s);
                }
                return;
            }
            on_enter();
        }
    };

    // The line painted under the input, whose own text is clear: the
    // caret and the selection are the input's, the glyphs are painted here.
    let backdrop = dynamic(move || {
        let v = value.get();
        let kw = lead.get();
        untracked(|| {
            let known = known_vars(&v, &["this".to_owned()]);
            let auto = {
                let t = v.trim_start_matches(char::is_whitespace);
                let skip = v.len() - t.len();
                match t.chars().next() {
                    Some('/' | ':') => {
                        skip + 1 + t[1..].chars().take_while(|c| is_word(*c) || *c == '-').map(char::len_utf8).sum::<usize>()
                    }
                    _ => 0,
                }
            };
            let color = v.trim_start().chars().next().and_then(prefix_color).unwrap_or_else(|| rgb(0x38bdf8));
            let shown = super::cards::paint(&v, Some(&known), rgb(0xc678dd), if kw > 0 { kw } else { auto }, color, false)
                .font_family("ui-monospace");
            view().child(shown)
        })
    });
    // The field's width: the menu is kept inside it, however narrow the
    // strip holding the palette is.
    let field = signal(0.0_f32);
    view()
        .relative()
        .on_layout(move |b: &Bounds<Points>| field.set(b.size.width.0))
        .child(view().absolute().top_0().left_0().right_0().px(px(10.0)).py(px(8.0)).child(backdrop))
        .child(
            input()
                .element_ref(&input_ref)
                .multiline(true)
                .submit_behavior(SubmitBehavior::Submit)
                .value(move || value.get())
                .on_change(move |t: &str| value.set(t.to_owned()))
                .on_submit(submit)
                .on_focus(move || focused.set(true))
                .on_blur(move || focused.set(false))
                .on_selection_change(move |s: &Selection| {
                    caret.set(s.start as usize);
                    sel.set(0);
                })
                .on_caret_move(move |c: &CaretEvent| caret_x.set(c.bounds.origin.x.0))
                .on_content_size_change(move |s: &Size<Points>| height.set((s.height.0 + 16.0).clamp(44.0, 160.0)))
                .selected_range(move || force.get())
                .auto_focus(true)
                .auto_correct(false)
                .auto_capitalize(AutoCapitalize::None)
                .placeholder(move || placeholder.get().to_owned())
                .placeholder_color(rgba(0xffffff80))
                .caret_color(white())
                .selection_color(rgba(0xffffff40))
                .font_family("ui-monospace")
                .text_size(px(12.0))
                .line_height(px(20.0))
                .px(px(10.0))
                .py(px(8.0))
                .w_full()
                .h(move || px(height.get()))
                .text_color(rgba(0xffffff00)),
        )
        .child(when(move || open.get().is_some(), move || {
            let items = move || open.get().map(|m| m.items).unwrap_or_default();
            // At the caret, moved left when the field leaves no room for
            // its narrowest; never wider than what is right of it.
            let left = computed(move || caret_x.get().clamp(0.0, (field.get() - 220.0).max(0.0)));
            view()
                .absolute()
                .bottom(px(height.get_untracked() + 4.0))
                .left(move || px(left.get()))
                .min_w(move || px(field.get().min(220.0)))
                .max_w(move || px((field.get() - left.get()).clamp(0.0, 420.0)))
                .rounded_md()
                .overflow_hidden()
                .bg(move || theme().popover)
                .border_1()
                .border_color(move || theme().border)
                .child(each(items, |s| s.label.clone(), move |s, i| {
                    let pick = accept;
                    view()
                        .flex_row()
                        .items_center()
                        .gap_2()
                        .px_2()
                        .py_1()
                        .bg(move || if sel.get() == i.get() { theme().accent } else { theme().popover.opacity(0.0) })
                        .on_mouse_enter(move |_: &_| sel.set(i.get_untracked()))
                        .on_press_down(move || pick(&s.get_untracked()))
                        .child(text(move || s.get().label).text_xs().number_of_lines(1).flex_shrink(0.0).text_color(move || theme().primary))
                        .child(
                            text(move || s.get().hint)
                                .flex_1()
                                .min_w(px(0.0))
                                .text_xs()
                                .number_of_lines(1)
                                .text_color(move || theme().muted_foreground),
                        )
                }))
        }))
        .child(when(move || open.get().is_some(), move || {
            on_key_with("down", KeyOptions { when: When::Always, repeat: true, ..Default::default() }, move |_| {
                let n = open.get_untracked().map_or(1, |m| m.items.len()).max(1);
                sel.update(|s| *s = (*s + 1) % n);
            });
            on_key_with("up", KeyOptions { when: When::Always, repeat: true, ..Default::default() }, move |_| {
                let n = open.get_untracked().map_or(1, |m| m.items.len()).max(1);
                sel.update(|s| *s = (*s + n - 1) % n);
            });
            view()
        }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn commands() -> Vec<Item> {
        ["status", "stash", "go"].iter().map(|k| Item { keyword: (*k).into(), hint: String::new() }).collect()
    }

    fn labels(m: Option<Menu>) -> Vec<String> {
        m.map(|m| m.items.into_iter().map(|s| s.label).collect()).unwrap_or_default()
    }

    fn at(value: &str) -> Option<Menu> {
        let files = ["src/main.rs".to_owned(), "my notes.md".to_owned()];
        suggestions(value, value.len(), &commands(), &[], &["this".into()], &[], &files)
    }

    #[test]
    fn open_offers_files_and_inserts_the_command_line() {
        let menu = at("/open mai").expect("a menu");
        assert_eq!(menu.items[0].label, "main.rs");
        assert_eq!(menu.items[0].hint, "src");
        assert_eq!(menu.items[0].insert, "/open src/main.rs ");
        assert_eq!(at("/open notes").expect("a menu").items[0].insert, "/open \"my notes.md\" ");
        // Once a path is in, nothing more is offered.
        assert!(labels(at("/open src/main.rs ")).is_empty());
    }

    #[test]
    fn slash_lists_commands_with_the_exact_one_first() {
        assert_eq!(labels(at("/st")), ["/status", "/stash"]);
        assert_eq!(labels(at("/stash")), ["/stash"]);
        assert_eq!(at("/st").map(|m| m.from), Some(0));
        assert!(at("status").is_none());
    }

    #[test]
    fn a_line_start_hash_offers_the_script_words() {
        assert_eq!(labels(at("#r")), ["#read", "#run"]);
        assert_eq!(labels(at("#ask {t1}\n  #se")), ["#send", "#set"]);
    }

    #[test]
    fn braces_offer_the_targets_input_and_declared_vars() {
        assert_eq!(labels(at("#send {")), ["{this}", "{input}"]);
        assert_eq!(labels(at("#ask \"x\" as $note\n#send {n")), ["{note}"]);
    }

    #[test]
    fn a_declaration_offers_types() {
        assert_eq!(labels(at("#ask \"x\" as $note:s")), ["string", "string[]"]);
    }

    #[test]
    fn run_offers_verbs_then_flags() {
        assert_eq!(labels(at("#run {t1} fe")), ["fetch"]);
        assert_eq!(labels(at("#run {t1} commit -")), ["--ai", "-p", "-f"]);
        assert_eq!(labels(at("#run {t1} commit --ai -")), ["-p", "-f"]);
    }

    #[test]
    fn utf16_offsets_map_to_bytes() {
        assert_eq!(byte_of("a😀b", 1), 1);
        assert_eq!(byte_of("a😀b", 3), 5);
        assert_eq!(utf16_of("a😀b", 5), 3);
    }
}
