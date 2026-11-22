//! Bolt command scripts: the node types, the parser and the pure helpers
//! the runner leans on (`src/services/boltCommands/types.ts`).

use std::{cell::Cell, collections::{BTreeMap, HashMap}};

use nsh_core::time::unix_ms;
use serde_json::Value;

use crate::hub::BoltCommand;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InAppAction {
    CreateWorktrees,
    StartAgents,
    Terminal,
    Tab,
    Group,
    Palette,
}

/// What a `#fetch` answers as (`FetchType`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FetchType {
    String,
    StringList,
    Number,
    Boolean,
}

impl FetchType {
    pub fn name(self) -> &'static str {
        match self {
            FetchType::String => "string",
            FetchType::StringList => "string[]",
            FetchType::Number => "number",
            FetchType::Boolean => "boolean",
        }
    }

    fn from_name(name: &str) -> Option<FetchType> {
        match name.to_ascii_lowercase().as_str() {
            "string[]" => Some(FetchType::StringList),
            "string" => Some(FetchType::String),
            "number" => Some(FetchType::Number),
            "boolean" => Some(FetchType::Boolean),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FetchMode {
    Fork,
    Live,
    Fresh,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendMode {
    Terminal,
    Headless,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IfOp {
    Empty,
    NotEmpty,
    Equals,
}

#[derive(Debug, Clone, PartialEq)]
pub enum BoltNode {
    InApp { id: String, action: InAppAction, args: BTreeMap<String, String> },
    Fetch { id: String, target: String, prompt: String, as_: Option<FetchType>, into: Option<String>, mode: FetchMode },
    Send { id: String, target: String, text: String, mode: SendMode, into: Option<String> },
    Read { id: String, target: String, into: Option<String> },
    ForEach { id: String, over: String, item: String, body: Vec<BoltNode> },
    If { id: String, var: String, op: IfOp, value: String, then: Vec<BoltNode>, else_: Vec<BoltNode> },
    Approve { id: String, say: String },
    Alert { id: String, say: String },
    Set { id: String, into: String, value: String },
}

/// A script variable: a string, or a list (`string | string[]`).
#[derive(Debug, Clone, PartialEq)]
pub enum Var {
    Str(String),
    List(Vec<String>),
}

impl Var {
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Var::Str(s) => Some(s),
            Var::List(_) => None,
        }
    }

    /// JS's `String(v)`.
    pub fn text(&self) -> String {
        match self {
            Var::Str(s) => s.clone(),
            Var::List(l) => l.join(","),
        }
    }
}

pub type Vars = HashMap<String, Var>;

thread_local!(static SEQ: Cell<u64> = const { Cell::new(0) });

/// A new node or job id (`newId`).
pub fn new_id(prefix: &str) -> String {
    let n = SEQ.with(|s| {
        s.set(s.get() + 1);
        s.get()
    });
    format!("{prefix}{}{n}", base36(unix_ms() as u64))
}

fn base36(mut n: u64) -> String {
    if n == 0 {
        return "0".into();
    }
    let mut digits = Vec::new();
    while n > 0 {
        digits.push(std::char::from_digit((n % 36) as u32, 36).unwrap());
        n /= 36;
    }
    digits.iter().rev().collect()
}

/// `\w` of a JS regex.
fn is_word(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

/// `[\w.-]`.
fn is_key(c: char) -> bool {
    is_word(c) || c == '.' || c == '-'
}

/// Splices `{key}` of each var into `text`; a list joins with `, `, an
/// unknown key stays as written (`fill`).
pub fn fill(text: &str, vars: &Vars, escape: &dyn Fn(&str) -> String) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        let len = after.find(|c: char| !is_key(c)).unwrap_or(after.len());
        if len > 0 && after[len..].starts_with('}') {
            let key = &after[..len];
            match vars.get(key) {
                Some(Var::List(l)) => out.push_str(&escape(&l.join(", "))),
                Some(Var::Str(s)) => out.push_str(&escape(s)),
                None => out.push_str(&rest[open..open + len + 2]),
            }
            rest = &after[len + 1..];
        } else {
            out.push('{');
            rest = after;
        }
    }
    out.push_str(rest);
    out
}

pub fn plain(v: &str) -> String {
    v.to_owned()
}

/// `fill` with no escaping.
pub fn fill_plain(text: &str, vars: &Vars) -> String {
    fill(text, vars, &plain)
}

/// A value spliced into a palette line keeps its own quotes, so
/// `commit "{msg}"` stays one arg (`paletteSafe`).
pub fn palette_safe(v: &str) -> String {
    let mut out = String::with_capacity(v.len());
    for c in v.chars() {
        if matches!(c, '"' | '\'' | '\\') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

pub fn truthy(v: Option<&Var>) -> bool {
    match v {
        None => false,
        Some(Var::List(l)) => !l.is_empty(),
        Some(Var::Str(s)) => !s.is_empty(),
    }
}

/// A var naming a list is the list; anything else is split on commas or
/// lines, so `a, b` typed by hand and a #send's output both work
/// (`listOf`).
pub fn list_of(raw: &str, vars: &Vars) -> Vec<String> {
    let trimmed = raw.trim();
    let key = trimmed
        .strip_prefix('{')
        .and_then(|r| r.strip_suffix('}'))
        .filter(|k| !k.is_empty() && k.chars().all(is_key))
        .unwrap_or(trimmed);
    if let Some(Var::List(l)) = vars.get(key) {
        return l.clone();
    }
    fill_plain(raw, vars).split([',', '\n']).map(|s| s.trim().to_owned()).filter(|s| !s.is_empty()).collect()
}

pub fn answer_tail(key: &str, as_: FetchType) -> String {
    format!(
        "\n\nAnswer with one JSON object on the last line and nothing after it: {{\"{key}\": <{}>}}.{}",
        as_.name(),
        if as_ == FetchType::StringList { " The value is a JSON array of strings." } else { "" }
    )
}

/// JS's `String(v)` of a parsed JSON value.
fn js_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => "null".into(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => match n.as_f64() {
            Some(f) if f.fract() == 0.0 && f.abs() < 1e21 && !n.is_i64() && !n.is_u64() => format!("{}", f as i128),
            _ => n.to_string(),
        },
        Value::Array(a) => a.iter().map(|x| if x.is_null() { String::new() } else { js_string(x) }).collect::<Vec<_>>().join(","),
        Value::Object(_) => "[object Object]".into(),
    }
}

/// The last json object in the reply, not the first: an answer that
/// thinks out loud writes examples on the way (`parseAnswer`).
pub fn parse_answer(text: &str, key: &str, as_: FetchType) -> Option<Var> {
    let mut open = text.rfind('{');
    while let Some(start) = open {
        // A value can hold its own `}`, so the object may close at any later one.
        let mut close = text.rfind('}');
        while let Some(end) = close {
            if end <= start {
                break;
            }
            if let Ok(Value::Object(map)) = serde_json::from_str::<Value>(&text[start..=end]) {
                if let Some(v) = map.get(key) {
                    return Some(match as_ {
                        FetchType::StringList => Var::List(match v {
                            Value::Array(a) => a.iter().map(js_string).collect(),
                            other => vec![js_string(other)],
                        }),
                        FetchType::Boolean => {
                            Var::Str(if matches!(v, Value::Bool(true)) || v.as_str() == Some("true") { "true".into() } else { String::new() })
                        }
                        _ => Var::Str(match v {
                            Value::Array(a) => a.iter().map(js_string).collect::<Vec<_>>().join(", "),
                            other => js_string(other),
                        }),
                    });
                }
            }
            close = text[..end].rfind('}');
        }
        open = if start > 0 { text[..start].rfind('{') } else { None };
    }
    None
}

fn handle_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '/' || c == '-'
}

/// The `@name` spans of `text` (`/@([a-z0-9/-]+)/gi`): byte range of the
/// whole match and the name.
fn handle_spans(text: &str) -> Vec<(usize, usize, String)> {
    let mut out = Vec::new();
    let mut at = 0;
    while let Some(p) = text[at..].find('@') {
        let start = at + p;
        let name = &text[start + 1..];
        let len = name.find(|c: char| !handle_char(c)).unwrap_or(name.len());
        if len == 0 {
            at = start + 1;
            continue;
        }
        out.push((start, start + 1 + len, name[..len].to_owned()));
        at = start + 1 + len;
    }
    out
}

/// A `:keyword @a @b` match (`matchCommand`): the command and the handle
/// bound to each of its slots.
#[derive(Debug, Clone, PartialEq)]
pub struct CommandMatch {
    pub command: BoltCommand,
    pub bound: HashMap<String, String>,
    /// The slots no handle was bound to.
    pub missing: Vec<String>,
    /// What follows the keyword, minus its handles.
    pub input: String,
}

/// `do-thing @a @b` or `@a do-thing @b`: a leading handle is the first
/// target, so the compose box's prefilled @ still works.
pub fn match_command(text: &str, commands: &[BoltCommand], handles: &[String]) -> Option<CommandMatch> {
    let known = |h: &str| handles.iter().any(|k| k == h);
    let mut rest = text.trim();
    let mut lead: Vec<String> = Vec::new();
    if let Some(after) = rest.strip_prefix('@') {
        let len = after.find(|c: char| !handle_char(c)).unwrap_or(after.len());
        let tail = &after[len..];
        let spaces = tail.len() - tail.trim_start().len();
        if len > 0 && spaces > 0 {
            let h = after[..len].to_lowercase();
            if known(&h) {
                lead.push(h);
                rest = &tail[spaces..];
            }
        }
    }
    // `^([a-z0-9][\w-]*)(?=\s|$)`
    let word = {
        let mut chars = rest.char_indices();
        match chars.next() {
            Some((_, c)) if c.is_ascii_alphanumeric() => {
                let len = rest.find(|c: char| !(is_word(c) || c == '-')).unwrap_or(rest.len());
                let next = rest[len..].chars().next();
                next.is_none_or(char::is_whitespace).then(|| rest[..len].to_lowercase())
            }
            _ => None,
        }
    }?;
    let command = commands.iter().find(|c| c.keyword.to_lowercase() == word)?;
    rest = &rest[word.len()..];
    let spans = handle_spans(rest);
    let mut handles_in = lead;
    for (_, _, name) in &spans {
        let h = name.to_lowercase();
        if known(&h) {
            handles_in.push(h);
        }
    }
    let mut bound = HashMap::new();
    for (i, slot) in command.targets.iter().enumerate() {
        if let Some(h) = handles_in.get(i) {
            bound.insert(slot.clone(), h.clone());
        }
    }
    let missing = command.targets.iter().filter(|s| !bound.contains_key(*s)).cloned().collect();
    let mut input = String::new();
    let mut at = 0;
    for (start, end, _) in &spans {
        input.push_str(&rest[at..*start]);
        at = *end;
    }
    input.push_str(&rest[at..]);
    Some(CommandMatch { command: command.clone(), bound, missing, input: input.trim().to_owned() })
}

#[derive(Debug, Clone, PartialEq)]
pub struct ScriptError {
    pub line: usize,
    pub message: String,
}

struct Item {
    line: usize,
    indent: usize,
    word: String,
    rest: String,
}

/// Old git words, kept so saved scripts still parse; #run {t1} <command>
/// is the spelling now.
const GIT_WORDS: &[(&str, &str)] = &[("sync", "sync"), ("git-fetch", "fetch"), ("push", "push")];

/// One `#create` object (`CREATE_OBJECTS`).
pub struct CreateObject {
    pub name: &'static str,
    pub action: InAppAction,
    pub quoted: &'static str,
    pub bare: bool,
    pub target: bool,
    pub keys: &'static [&'static str],
    pub need: &'static [&'static str],
    pub hint: &'static str,
}

/// One table for the parser and the editor's completion, so a word the
/// menu offers is a word the parser reads.
pub const CREATE_OBJECTS: &[CreateObject] = &[
    CreateObject { name: "terminal", action: InAppAction::Terminal, quoted: "command", bare: false, target: true, keys: &[], need: &[], hint: "{t1} terminal \"npm run dev\" [as $term]" },
    CreateObject { name: "claude", action: InAppAction::StartAgents, quoted: "prompt", bare: false, target: true, keys: &[], need: &[], hint: "{t1} claude \"prompt\" [as $term] — waits for claude" },
    CreateObject { name: "worktree", action: InAppAction::CreateWorktrees, quoted: "branches", bare: true, target: true, keys: &["from", "in"], need: &["branches"], hint: "{t1} worktree \"a, b\" from main in x [as $wts]" },
    CreateObject { name: "tab", action: InAppAction::Tab, quoted: "path", bare: true, target: false, keys: &["in"], need: &["path"], hint: "tab \"/path/to/dir\" in x [as $tab]" },
    CreateObject { name: "group", action: InAppAction::Group, quoted: "name", bare: true, target: false, keys: &["with"], need: &["name", "tabs"], hint: "group \"name\" with {wts} [as $g]" },
];

/// `from`, `in` and `with` and the arg each one sets (`CREATE_WORDS`).
pub const CREATE_WORDS: &[(&str, &str)] = &[("from", "parent"), ("in", "group"), ("with", "tabs")];

const CREATE_SYNTAX: &str = "expected #create {target} terminal | claude | worktree …, #create tab \"path\" or #create group \"name\" with …";

fn create_object(name: &str) -> Option<&'static CreateObject> {
    CREATE_OBJECTS.iter().find(|o| o.name == name)
}

fn create_word(word: &str) -> Option<&'static str> {
    CREATE_WORDS.iter().find(|(w, _)| *w == word).map(|(_, k)| *k)
}

fn syntax(word: &str) -> &'static str {
    match word {
        "ask" => "#ask [live] [{t1}] \"<question>\" [as $var[:type]]",
        "fetch" => "#fetch [live] {t1} \"<question>\" as $var[:type]",
        "prompt" => "#prompt \"<prompt>\" as $var[:type]",
        "send" => "#send [headless] \"<text>\" to {t1} [as $var]",
        "read" => "#read {t1} [as $var]",
        "for-each" => "#for-each {list} as $item",
        "if" => "#if {var} empty | not-empty | = <value>",
        "set" => "#set $var \"<value>\"",
        "alert" => "#alert \"<message>\" [wait]",
        _ => "",
    }
}

/// `from main in my cut`: each word's value runs until the next word.
fn tail(tokens: &[&str], keys: &[&str]) -> (Vec<String>, BTreeMap<String, String>) {
    let mut lead = Vec::new();
    let mut args: BTreeMap<String, String> = BTreeMap::new();
    let mut key = String::new();
    for t in tokens {
        if keys.contains(t) {
            key = create_word(t).unwrap_or_default().to_owned();
            args.insert(key.clone(), String::new());
        } else if !key.is_empty() {
            let v = args.entry(key.clone()).or_default();
            *v = format!("{v} {t}").trim().to_owned();
        } else {
            lead.push((*t).to_owned());
        }
    }
    (lead, args)
}

fn items(script: &str) -> Vec<Item> {
    let mut out: Vec<Item> = Vec::new();
    for (i, raw) in script.split('\n').enumerate() {
        let text = raw.replace('\t', "  ");
        if text.trim().is_empty() {
            continue;
        }
        let indent = text.chars().count() - text.trim_start().chars().count();
        let trimmed = text.trim();
        let head = trimmed.strip_prefix('#').and_then(|after| {
            let len = after.find(|c: char| !is_word_dash(c)).unwrap_or(after.len());
            (len > 0).then(|| (after[..len].to_owned(), after[len..].trim_start().to_owned()))
        });
        if let Some((word, rest)) = head {
            out.push(Item { line: i + 1, indent, word, rest });
        } else if let Some(last) = out.last_mut() {
            last.rest.push('\n');
            last.rest.push_str(trimmed);
        } else {
            out.push(Item { line: i + 1, indent, word: String::new(), rest: trimmed.to_owned() });
        }
    }
    out
}

/// `[\w-]+` word characters of a `#word`.
fn is_word_dash(c: char) -> bool {
    is_word(c) || c == '-'
}

/// The text from the first non-whitespace char on, if whitespace
/// came first (`\s+`).
fn after_space(s: &str) -> Option<&str> {
    let t = s.trim_start();
    (t.len() < s.len()).then_some(t)
}

/// `[\w.-]+` at the start of `s`: the name and what follows it.
fn name_of(s: &str) -> Option<(&str, &str)> {
    let len = s.find(|c: char| !is_key(c)).unwrap_or(s.len());
    (len > 0).then(|| (&s[..len], &s[len..]))
}

/// ` as $name[:type]` closing a line: `None` when `tail` is not one, else
/// the name and its type (`(?:\s+as\s+\$([\w.-]+)(?::(string[]|…))?)`).
fn as_clause(tail: &str, typed: bool) -> Option<(String, Option<FetchType>)> {
    let s = after_space(tail)?;
    let s = s.get(..2).filter(|h| h.eq_ignore_ascii_case("as")).map(|_| &s[2..])?;
    let s = after_space(s)?.strip_prefix('$')?;
    let (name, rest) = name_of(s)?;
    if rest.is_empty() {
        return Some((name.to_owned(), None));
    }
    if !typed {
        return None;
    }
    let ty = FetchType::from_name(rest.strip_prefix(':')?)?;
    Some((name.to_owned(), Some(ty)))
}

/// A `as` clause that may be absent: `Some(None)` for no tail at all.
fn opt_as_clause(tail: &str, typed: bool) -> Option<Option<(String, Option<FetchType>)>> {
    if tail.is_empty() { Some(None) } else { as_clause(tail, typed).map(Some) }
}

/// `{name}`, `@handle/path` or a bare word: a target and what follows.
fn target_of(s: &str, bare: bool) -> Option<(&str, &str)> {
    if let Some(inner) = s.strip_prefix('{') {
        let close = inner.find('}')?;
        let name = &inner[..close];
        return (!name.is_empty() && name.chars().all(is_key)).then(|| (&s[..close + 2], &inner[close + 1..]));
    }
    if let Some(after) = s.strip_prefix('@') {
        let len = after.find(|c: char| !(is_word(c) || c == '/' || c == '-')).unwrap_or(after.len());
        return (len > 0).then(|| (&s[..len + 1], &after[len..]));
    }
    if bare {
        let len = s.find(|c: char| !is_word_dash(c)).unwrap_or(s.len());
        return (len > 0).then(|| (&s[..len], &s[len..]));
    }
    None
}

/// `live\s+` (case-insensitive) at the start, and what follows it.
fn live_prefix(s: &str) -> Option<&str> {
    let head = s.get(..4)?;
    head.eq_ignore_ascii_case("live").then(|| after_space(&s[4..])).flatten()
}

struct Parser {
    all: Vec<Item>,
    errors: Vec<ScriptError>,
}

impl Parser {
    fn fail(&mut self, at: usize, message: impl Into<String>) {
        self.errors.push(ScriptError { line: self.all[at].line, message: message.into() });
    }

    fn block(&mut self, start: usize, indent: usize) -> (Vec<BoltNode>, usize) {
        let mut nodes = Vec::new();
        let mut i = start;
        while i < self.all.len() && self.all[i].indent >= indent {
            let at = i;
            let it_indent = self.all[at].indent;
            let mut j = i + 1;
            while j < self.all.len() && self.all[j].indent > it_indent {
                j += 1;
            }
            let has_kids = j > i + 1;
            let mut node = self.one(at, has_kids.then_some(i + 1));
            if has_kids && node.as_ref().is_some_and(|n| !matches!(n, BoltNode::ForEach { .. } | BoltNode::If { .. })) {
                self.fail(i + 1, "only #for-each and #if take indented lines");
            }
            i = j;
            if matches!(node, Some(BoltNode::If { .. })) && i < self.all.len() && self.all[i].word == "else" && self.all[i].indent == it_indent {
                let e = i;
                i += 1;
                while i < self.all.len() && self.all[i].indent > it_indent {
                    i += 1;
                }
                let body = if i > e + 1 { self.block(e + 1, self.all[e + 1].indent).0 } else { Vec::new() };
                if let Some(BoltNode::If { else_, .. }) = &mut node {
                    *else_ = body;
                }
            }
            if let Some(n) = node {
                nodes.push(n);
            }
        }
        (nodes, i)
    }

    fn kids(&mut self, first: Option<usize>) -> Vec<BoltNode> {
        match first {
            Some(i) => {
                let indent = self.all.get(i).map_or(0, |x| x.indent);
                self.block(i, indent).0
            }
            None => Vec::new(),
        }
    }

    fn one(&mut self, at: usize, kids: Option<usize>) -> Option<BoltNode> {
        let id = new_id("n");
        let word = self.all[at].word.clone();
        let rest = self.all[at].rest.trim().to_owned();
        let rest = rest.as_str();
        macro_rules! bad {
            () => {{
                self.fail(at, format!("expected {}", syntax(&word)));
                return None;
            }};
        }
        if word.is_empty() {
            self.fail(at, "a node starts with #, e.g. #fetch or #send");
            return None;
        }
        match word.as_str() {
            "ask" => {
                let Some((live, target, prompt, clause)) = parse_ask(rest) else { bad!() };
                if live && target.is_none() {
                    self.fail(at, "#ask live needs a {target}");
                    return None;
                }
                let (into, as_) = clause.map_or((None, None), |(n, t)| (Some(n), t));
                // An untyped ask of a target is a headless send, which fans out over a group.
                if let (Some(target), false, None) = (&target, live, as_) {
                    return Some(BoltNode::Send { id, text: prompt, target: target.clone(), mode: SendMode::Headless, into });
                }
                Some(BoltNode::Fetch {
                    id,
                    mode: if live { FetchMode::Live } else if target.is_some() { FetchMode::Fork } else { FetchMode::Fresh },
                    target: target.unwrap_or_default(),
                    prompt,
                    into,
                    as_,
                })
            }
            "fetch" => {
                let Some((live, target, prompt, (name, ty))) = parse_fetch(rest) else { bad!() };
                Some(BoltNode::Fetch { id, mode: if live { FetchMode::Live } else { FetchMode::Fork }, target, prompt, into: Some(name), as_: Some(ty.unwrap_or(FetchType::String)) })
            }
            "prompt" => {
                let Some((prompt, (name, ty))) = parse_quoted_as(rest) else { bad!() };
                Some(BoltNode::Fetch { id, mode: FetchMode::Fresh, target: String::new(), prompt, into: Some(name), as_: Some(ty.unwrap_or(FetchType::String)) })
            }
            "send" => {
                let Some((headless, text, target, into)) = parse_send(rest) else { bad!() };
                Some(BoltNode::Send { id, text, target, mode: if headless { SendMode::Headless } else { SendMode::Terminal }, into })
            }
            "read" => {
                let Some((target, tail)) = target_of(rest, true) else { bad!() };
                let Some(into) = opt_as_clause_exact(tail) else { bad!() };
                Some(BoltNode::Read { id, target: target.to_owned(), into })
            }
            "for-each" => {
                let Some((over, item)) = parse_for_each(rest) else { bad!() };
                let body = self.kids(kids);
                Some(BoltNode::ForEach { id, over, item, body })
            }
            "if" => {
                let Some((var, tail)) = rest.split_once(char::is_whitespace) else { bad!() };
                let tail = tail.trim_start();
                let (op, value) = if tail == "empty" {
                    (IfOp::Empty, String::new())
                } else if tail == "not-empty" {
                    (IfOp::NotEmpty, String::new())
                } else if let Some(v) = tail.strip_prefix('=') {
                    (IfOp::Equals, v.trim_start().to_owned())
                } else {
                    bad!()
                };
                let var = var.strip_prefix('{').unwrap_or(var);
                let var = var.strip_suffix('}').unwrap_or(var);
                let then = self.kids(kids);
                Some(BoltNode::If { id, var: var.to_owned(), op, value, then, else_: Vec::new() })
            }
            "approve" => Some(BoltNode::Approve { id, say: rest.to_owned() }),
            "alert" => {
                let q = rest.chars().next().filter(|c| *c == '"' || *c == '\'');
                let parsed = q.and_then(|q| {
                    let end = rest.rfind(q).filter(|e| *e > 0)?;
                    let tail = &rest[end + 1..];
                    let wait = if tail.is_empty() {
                        false
                    } else if after_space(tail) == Some("wait") {
                        true
                    } else {
                        return None;
                    };
                    Some((rest[1..end].to_owned(), wait))
                });
                match parsed {
                    Some((say, true)) => Some(BoltNode::Approve { id, say }),
                    Some((say, false)) => Some(BoltNode::Alert { id, say }),
                    None => bad!(),
                }
            }
            "set" => {
                let parsed = rest.strip_prefix('$').and_then(name_of).and_then(|(name, tail)| {
                    let q = after_space(tail)?;
                    (q.len() >= 2 && q.starts_with('"') && q.ends_with('"')).then(|| (name.to_owned(), q[1..q.len() - 1].to_owned()))
                });
                match parsed {
                    Some((into, value)) => Some(BoltNode::Set { id, into, value }),
                    None => bad!(),
                }
            }
            "run" => {
                if rest.is_empty() {
                    self.fail(at, "expected #run [{target}] <palette command>");
                    return None;
                }
                let with_target = target_of(rest, false).and_then(|(t, tail)| after_space(tail).map(|c| (t, c)));
                let mut args = BTreeMap::new();
                match with_target {
                    Some((target, command)) => {
                        args.insert("command".to_owned(), command.to_owned());
                        args.insert("target".to_owned(), target.to_owned());
                    }
                    None => {
                        args.insert("command".to_owned(), rest.to_owned());
                    }
                }
                Some(BoltNode::InApp { id, action: InAppAction::Palette, args })
            }
            "else" => {
                self.fail(at, "#else must follow an #if at the same indent");
                None
            }
            "create" => self.create(at, id, rest),
            other => {
                let Some((_, command)) = GIT_WORDS.iter().find(|(w, _)| *w == other) else {
                    self.fail(at, format!("unknown #{other}"));
                    return None;
                };
                let parts: Vec<&str> = rest.split_whitespace().collect();
                if parts.len() != 1 {
                    self.fail(at, format!("expected #{other} {{target}}"));
                    return None;
                }
                let args = BTreeMap::from([("command".to_owned(), (*command).to_owned()), ("target".to_owned(), parts[0].to_owned())]);
                Some(BoltNode::InApp { id, action: InAppAction::Palette, args })
            }
        }
    }

    fn create(&mut self, at: usize, id: String, rest: &str) -> Option<BoltNode> {
        // ` as $name` at the end, whose whitespace run starts the cut.
        let named = rest.rfind('$').and_then(|p| {
            let name = &rest[p + 1..];
            if name.is_empty() || !name.chars().all(is_key) {
                return None;
            }
            let before = rest[..p].trim_end();
            let before = before.strip_suffix("as")?;
            let body = before.trim_end();
            (body.len() < before.len()).then(|| (body.len(), name.to_owned()))
        });
        let body = named.as_ref().map_or(rest, |(cut, _)| &rest[..*cut]);
        let padded = format!(" {body}");
        // `\s"([^"]*)"\s*`: the first quoted value, with the space before it.
        let quote = padded.char_indices().find_map(|(i, c)| {
            if !c.is_whitespace() || !padded[i + c.len_utf8()..].starts_with('"') {
                return None;
            }
            let inner = &padded[i + c.len_utf8() + 1..];
            let close = inner.find('"')?;
            let mut end = i + c.len_utf8() + 1 + close + 1;
            end += padded[end..].len() - padded[end..].trim_start().len();
            Some((i, end, inner[..close].to_owned()))
        });
        let plain_text = match &quote {
            Some((start, end, _)) => format!("{} {}", &padded[..*start], &padded[*end..]).trim().to_owned(),
            None => body.to_owned(),
        };
        let mut words = plain_text.split_whitespace();
        let first = words.next().unwrap_or("").to_owned();
        let second = words.next().unwrap_or("").to_owned();
        let after: Vec<&str> = words.collect();
        let spec = create_object(&first).or_else(|| create_object(&second));
        let Some(spec) = spec.filter(|s| s.target || create_object(&first).is_some()) else {
            self.fail(at, CREATE_SYNTAX);
            return None;
        };
        let target = if spec.target { if create_object(&first).is_some() { second.clone() } else { first.clone() } } else { String::new() };
        if spec.target && (target.is_empty() || create_word(&target).is_some()) {
            self.fail(at, CREATE_SYNTAX);
            return None;
        }
        let tokens: Vec<&str> = if spec.target { after.clone() } else { std::iter::once(second.as_str()).chain(after.iter().copied()).collect() };
        let tokens: Vec<&str> = tokens.into_iter().filter(|t| !t.is_empty()).collect();
        let (lead, args) = tail(&tokens, spec.keys);
        if !lead.is_empty() && (quote.is_some() || !spec.bare) {
            let message = if spec.bare { format!("expected #create {}", spec.hint) } else { format!("quote the {}: #create {}", spec.quoted, spec.hint) };
            self.fail(at, message);
            return None;
        }
        let value = quote.as_ref().map(|q| q.2.clone()).or_else(|| lead.first().cloned());
        let mut out: BTreeMap<String, String> = BTreeMap::new();
        if !target.is_empty() {
            out.insert("target".into(), target);
        }
        if let Some(v) = value.filter(|v| !v.is_empty()) {
            out.insert(spec.quoted.into(), v);
        }
        out.extend(args.clone());
        if let Some((_, name)) = &named {
            out.insert("as".into(), name.clone());
        }
        let missing = spec.need.iter().copied().chain(args.keys().map(String::as_str)).any(|k| out.get(k).is_none_or(String::is_empty));
        if lead.len() > 1 || missing {
            self.fail(at, format!("expected #create {}", spec.hint));
            return None;
        }
        Some(BoltNode::InApp { id, action: spec.action, args: out })
    }
}

/// `[live ][target ]"prompt"[ as $name[:type]]` (`#ask`).
#[allow(clippy::type_complexity)]
fn parse_ask(rest: &str) -> Option<(bool, Option<String>, String, Option<(String, Option<FetchType>)>)> {
    let (live, s) = match live_prefix(rest) {
        Some(s) => (true, s),
        None => (false, rest),
    };
    let (target, s) = if s.starts_with('"') {
        (None, s)
    } else {
        let len = s.find(|c: char| c.is_whitespace() || c == '"')?;
        if len == 0 {
            return None;
        }
        let q = after_space(&s[len..])?;
        (Some(s[..len].to_owned()), q)
    };
    let q = s.strip_prefix('"')?;
    let end = q.rfind('"')?;
    let clause = opt_as_clause(&q[end + 1..], true)?;
    Some((live, target, q[..end].to_owned(), clause))
}

/// `[live ]target "prompt" as $name[:type]` (`#fetch`).
#[allow(clippy::type_complexity)]
fn parse_fetch(rest: &str) -> Option<(bool, String, String, (String, Option<FetchType>))> {
    let attempt = |s: &str| -> Option<(String, String, (String, Option<FetchType>))> {
        let len = s.find(char::is_whitespace)?;
        let q = after_space(&s[len..])?.strip_prefix('"')?;
        let end = q.rfind('"')?;
        let clause = as_clause(&q[end + 1..], true)?;
        Some((s[..len].to_owned(), q[..end].to_owned(), clause))
    };
    if let Some(s) = live_prefix(rest) {
        if let Some((t, p, c)) = attempt(s) {
            return Some((true, t, p, c));
        }
    }
    attempt(rest).map(|(t, p, c)| (false, t, p, c))
}

/// `"prompt" as $name[:type]` (`#prompt`).
fn parse_quoted_as(rest: &str) -> Option<(String, (String, Option<FetchType>))> {
    let q = rest.strip_prefix('"')?;
    let end = q.rfind('"')?;
    let clause = as_clause(&q[end + 1..], true)?;
    Some((q[..end].to_owned(), clause))
}

/// `[headless ]"text" to target[ as $var]` (`#send`).
fn parse_send(rest: &str) -> Option<(bool, String, String, Option<String>)> {
    let (headless, s) = match rest.strip_prefix("headless").and_then(after_space) {
        Some(s) => (true, s),
        None => (false, rest),
    };
    let q = s.strip_prefix('"')?;
    let end = q.rfind('"')?;
    let tail = after_space(&q[end + 1..])?;
    let tail = tail.strip_prefix("to").and_then(after_space)?;
    let (target, after) = target_of(tail, true)?;
    let into = match opt_as_clause_exact(after)? {
        Some(name) => Some(name),
        None => None,
    };
    Some((headless, q[..end].to_owned(), target.to_owned(), into))
}

/// `(\S+)\s+as\s+\$([\w.-]+)$`, case-sensitive.
fn parse_for_each(rest: &str) -> Option<(String, String)> {
    let (over, tail) = rest.split_once(char::is_whitespace)?;
    let s = tail.trim_start().strip_prefix("as").and_then(after_space)?.strip_prefix('$')?;
    let (name, left) = name_of(s)?;
    (!over.is_empty() && left.is_empty()).then(|| (over.to_owned(), name.to_owned()))
}

/// `\s+as\s+\$name` or nothing, case-sensitive, untyped.
fn opt_as_clause_exact(tail: &str) -> Option<Option<String>> {
    if tail.is_empty() {
        return Some(None);
    }
    let s = after_space(tail)?.strip_prefix("as").and_then(after_space)?.strip_prefix('$')?;
    let (name, rest) = name_of(s)?;
    rest.is_empty().then(|| Some(name.to_owned()))
}

/// The nodes of `script` and the lines that did not parse (`parseScript`).
pub fn parse_script(script: &str) -> (Vec<BoltNode>, Vec<ScriptError>) {
    let mut p = Parser { all: items(script), errors: Vec::new() };
    let (nodes, _) = p.block(0, 0);
    (nodes, p.errors)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vars(pairs: &[(&str, Var)]) -> Vars {
        pairs.iter().map(|(k, v)| ((*k).to_owned(), v.clone())).collect()
    }

    fn s(v: &str) -> Var {
        Var::Str(v.into())
    }

    fn cmd(keyword: &str, targets: &[&str]) -> BoltCommand {
        BoltCommand { id: "c".into(), name: keyword.into(), keyword: keyword.into(), targets: targets.iter().map(|t| (*t).to_owned()).collect(), script: String::new() }
    }

    #[test]
    fn fill_splices_known_keys_and_joins_lists() {
        let v = vars(&[("a", s("x")), ("l", Var::List(vec!["p".into(), "q".into()]))]);
        assert_eq!(fill_plain("{a}-{l}-{nope}-{ }-{", &v), "x-p, q-{nope}-{ }-{");
        assert_eq!(fill("{a}", &v, &palette_safe), "x");
        assert_eq!(fill("{m}", &vars(&[("m", s("a \"b\" 'c' \\"))]), &palette_safe), "a \\\"b\\\" \\'c\\' \\\\");
        assert_eq!(fill_plain("{{a}}", &v), "{x}");
    }

    #[test]
    fn list_of_takes_a_list_var_or_splits_text() {
        let v = vars(&[("l", Var::List(vec!["a".into(), "b".into()])), ("t", s("x, y\nz"))]);
        assert_eq!(list_of("{l}", &v), ["a", "b"]);
        assert_eq!(list_of("l", &v), ["a", "b"]);
        assert_eq!(list_of("{t}", &v), ["x", "y", "z"]);
        assert_eq!(list_of(" one, two ,, ", &v), ["one", "two"]);
    }

    #[test]
    fn parse_answer_reads_the_last_object_with_the_key() {
        let text = "Think {\"a\": 1}.\nExample {\"x\": 2}\nDone: {\"a\": \"v}\"}";
        assert_eq!(parse_answer(text, "a", FetchType::String), Some(s("v}")));
        assert_eq!(parse_answer("{\"a\": [1, \"b\"]}", "a", FetchType::StringList), Some(Var::List(vec!["1".into(), "b".into()])));
        assert_eq!(parse_answer("{\"a\": \"one\"}", "a", FetchType::StringList), Some(Var::List(vec!["one".into()])));
        assert_eq!(parse_answer("{\"a\": [1, 2]}", "a", FetchType::String), Some(s("1, 2")));
        assert_eq!(parse_answer("{\"a\": true}", "a", FetchType::Boolean), Some(s("true")));
        assert_eq!(parse_answer("{\"a\": \"false\"}", "a", FetchType::Boolean), Some(s("")));
        assert_eq!(parse_answer("{\"a\": 3.0}", "a", FetchType::Number), Some(s("3")));
        assert_eq!(parse_answer("{\"b\": 1}", "a", FetchType::String), None);
        assert_eq!(parse_answer("no json", "a", FetchType::String), None);
        assert_eq!(parse_answer("{\"a\":1} {", "a", FetchType::Number), Some(s("1")));
    }

    #[test]
    fn answer_tail_names_the_key_and_type() {
        assert_eq!(
            answer_tail("k", FetchType::StringList),
            "\n\nAnswer with one JSON object on the last line and nothing after it: {\"k\": <string[]>}. The value is a JSON array of strings."
        );
        assert!(answer_tail("k", FetchType::Number).ends_with("{\"k\": <number>}."));
    }

    #[test]
    fn match_command_binds_handles_in_order() {
        let cmds = vec![cmd("ship", &["a", "b"]), cmd("solo", &["t1"])];
        let handles: Vec<String> = ["web", "api", "web/k3f9"].iter().map(|h| (*h).to_owned()).collect();
        let m = match_command("ship @web and @API now @ghost", &cmds, &handles).unwrap();
        assert_eq!(m.command.keyword, "ship");
        assert_eq!(m.bound["a"], "web");
        assert_eq!(m.bound["b"], "api");
        assert!(m.missing.is_empty());
        assert_eq!(m.input, "and  now");

        // A leading handle is the first target.
        let m = match_command("@web ship @api", &cmds, &handles).unwrap();
        assert_eq!((m.bound["a"].as_str(), m.bound["b"].as_str()), ("web", "api"));

        let m = match_command("SOLO", &cmds, &handles).unwrap();
        assert_eq!(m.missing, ["t1"]);

        // An unknown leading handle is no lead and no keyword.
        assert!(match_command("@ghost ship @web", &cmds, &handles).is_none());
        assert!(match_command("shipping @web", &cmds, &handles).is_none());
        assert!(match_command("", &cmds, &handles).is_none());
    }

    fn one(script: &str) -> BoltNode {
        let (nodes, errors) = parse_script(script);
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(nodes.len(), 1);
        nodes.into_iter().next().unwrap()
    }

    fn err(script: &str) -> String {
        let (_, errors) = parse_script(script);
        errors.first().map(|e| format!("{}:{}", e.line, e.message)).unwrap_or_default()
    }

    #[test]
    fn parses_ask_fetch_and_prompt() {
        match one("#ask \"what now\" as $x:string[]") {
            BoltNode::Fetch { mode: FetchMode::Fresh, target, prompt, into, as_, .. } => {
                assert_eq!((target.as_str(), prompt.as_str()), ("", "what now"));
                assert_eq!(into.as_deref(), Some("x"));
                assert_eq!(as_, Some(FetchType::StringList));
            }
            n => panic!("{n:?}"),
        }
        // An untyped ask of a target is a headless send.
        assert!(matches!(&one("#ask {t1} \"q\""), BoltNode::Send { mode: SendMode::Headless, target, .. } if target == "{t1}"));
        assert!(matches!(&one("#ask @a \"q\" as $r"), BoltNode::Send { mode: SendMode::Headless, into: Some(i), .. } if i == "r"));
        assert!(matches!(one("#ask {t1} \"q\" as $r:number"), BoltNode::Fetch { mode: FetchMode::Fork, as_: Some(FetchType::Number), .. }));
        assert!(matches!(one("#ask live {t1} \"q\""), BoltNode::Fetch { mode: FetchMode::Live, .. }));
        assert_eq!(err("#ask live \"q\""), "1:#ask live needs a {target}");
        assert!(err("#ask nonsense").starts_with("1:expected #ask"));
        match one("#fetch live {t1} \"say \"hi\" ok\" as $v") {
            BoltNode::Fetch { mode: FetchMode::Live, prompt, as_: Some(FetchType::String), .. } => assert_eq!(prompt, "say \"hi\" ok"),
            n => panic!("{n:?}"),
        }
        // `live` as a target name still works.
        assert!(matches!(&one("#fetch live \"q\" as $v"), BoltNode::Fetch { mode: FetchMode::Fork, target, .. } if target == "live"));
        assert!(err("#fetch {t1} \"q\"").starts_with("1:expected #fetch"));
        assert!(matches!(one("#prompt \"p\" as $v:boolean"), BoltNode::Fetch { mode: FetchMode::Fresh, as_: Some(FetchType::Boolean), .. }));
    }

    #[test]
    fn parses_send_read_set_alert_run() {
        match one("#send headless \"do it\" to @web/k3f9 as $out") {
            BoltNode::Send { mode: SendMode::Headless, text, target, into, .. } => {
                assert_eq!((text.as_str(), target.as_str(), into.as_deref()), ("do it", "@web/k3f9", Some("out")));
            }
            n => panic!("{n:?}"),
        }
        assert!(matches!(one("#send \"x\" to {t1}"), BoltNode::Send { mode: SendMode::Terminal, into: None, .. }));
        assert!(matches!(&one("#send \"x\" to web"), BoltNode::Send { target, .. } if target == "web"));
        assert!(err("#send x to y").starts_with("1:expected #send"));
        assert!(matches!(&one("#read {t1} as $r"), BoltNode::Read { into: Some(i), .. } if i == "r"));
        assert!(matches!(one("#read @a"), BoltNode::Read { into: None, .. }));
        assert!(matches!(&one("#set $v \"a b\""), BoltNode::Set { into, value, .. } if into == "v" && value == "a b"));
        assert!(err("#set v \"x\"").starts_with("1:expected #set"));
        assert!(matches!(&one("#alert \"hi\""), BoltNode::Alert { say, .. } if say == "hi"));
        assert!(matches!(&one("#alert 'hi' wait"), BoltNode::Approve { say, .. } if say == "hi"));
        assert!(err("#alert hi").starts_with("1:expected #alert"));
        assert!(matches!(&one("#approve go on?"), BoltNode::Approve { say, .. } if say == "go on?"));
        match one("#run {t1} commit -m \"x\"") {
            BoltNode::InApp { action: InAppAction::Palette, args, .. } => {
                assert_eq!(args["command"], "commit -m \"x\"");
                assert_eq!(args["target"], "{t1}");
            }
            n => panic!("{n:?}"),
        }
        match one("#run status") {
            BoltNode::InApp { args, .. } => assert!(!args.contains_key("target") && args["command"] == "status"),
            n => panic!("{n:?}"),
        }
        assert_eq!(err("#run"), "1:expected #run [{target}] <palette command>");
        match one("#sync {t1}") {
            BoltNode::InApp { args, .. } => assert_eq!((args["command"].as_str(), args["target"].as_str()), ("sync", "{t1}")),
            n => panic!("{n:?}"),
        }
        match one("#git-fetch {t1}") {
            BoltNode::InApp { args, .. } => assert_eq!(args["command"], "fetch"),
            n => panic!("{n:?}"),
        }
        assert_eq!(err("#sync"), "1:expected #sync {target}");
        assert_eq!(err("#bogus x"), "1:unknown #bogus");
        assert_eq!(err("loose text"), "1:a node starts with #, e.g. #fetch or #send");
        assert_eq!(err("#else"), "1:#else must follow an #if at the same indent");
    }

    #[test]
    fn parses_blocks_and_else() {
        let script = "#for-each {wts} as $w\n  #send \"x\" to {w}\n  #set $a \"b\"\n#if {a} = b c\n  #alert \"yes\"\n#else\n  #alert \"no\"\n  #alert \"no2\"\n#alert \"end\"";
        let (nodes, errors) = parse_script(script);
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(nodes.len(), 3);
        match &nodes[0] {
            BoltNode::ForEach { over, item, body, .. } => {
                assert_eq!((over.as_str(), item.as_str(), body.len()), ("{wts}", "w", 2));
            }
            n => panic!("{n:?}"),
        }
        match &nodes[1] {
            BoltNode::If { var, op: IfOp::Equals, value, then, else_, .. } => {
                assert_eq!((var.as_str(), value.as_str(), then.len(), else_.len()), ("a", "b c", 1, 2));
            }
            n => panic!("{n:?}"),
        }
        assert!(matches!(nodes[2], BoltNode::Alert { .. }));
        assert!(matches!(one("#if {a} not-empty"), BoltNode::If { op: IfOp::NotEmpty, .. }));
        assert!(matches!(&one("#if a empty"), BoltNode::If { op: IfOp::Empty, var, .. } if var == "a"));
        assert!(err("#if a maybe").starts_with("1:expected #if"));
        assert_eq!(err("#set $a \"b\"\n  #alert \"x\""), "2:only #for-each and #if take indented lines");
        assert!(err("#for-each x").starts_with("1:expected #for-each"));
    }

    #[test]
    fn continuation_lines_extend_the_node() {
        match one("#send \"line one\nline two\" to {t1}") {
            BoltNode::Send { text, .. } => assert_eq!(text, "line one\nline two"),
            n => panic!("{n:?}"),
        }
    }

    #[test]
    fn parses_create() {
        match one("#create {t1} terminal \"npm run dev\" as $term") {
            BoltNode::InApp { action: InAppAction::Terminal, args, .. } => {
                assert_eq!(args["target"], "{t1}");
                assert_eq!(args["command"], "npm run dev");
                assert_eq!(args["as"], "term");
            }
            n => panic!("{n:?}"),
        }
        match one("#create claude {t1} \"fix it\"") {
            BoltNode::InApp { action: InAppAction::StartAgents, args, .. } => assert_eq!((args["target"].as_str(), args["prompt"].as_str()), ("{t1}", "fix it")),
            n => panic!("{n:?}"),
        }
        match one("#create {t1} worktree \"a, b\" from main in my cut as $wts") {
            BoltNode::InApp { action: InAppAction::CreateWorktrees, args, .. } => {
                assert_eq!(args["branches"], "a, b");
                assert_eq!(args["parent"], "main");
                assert_eq!(args["group"], "my cut");
                assert_eq!(args["as"], "wts");
            }
            n => panic!("{n:?}"),
        }
        match one("#create {t1} worktree feat") {
            BoltNode::InApp { args, .. } => assert_eq!(args["branches"], "feat"),
            n => panic!("{n:?}"),
        }
        match one("#create tab \"/tmp/x\" in grp") {
            BoltNode::InApp { action: InAppAction::Tab, args, .. } => assert_eq!((args["path"].as_str(), args["group"].as_str()), ("/tmp/x", "grp")),
            n => panic!("{n:?}"),
        }
        match one("#create group \"g\" with {wts}") {
            BoltNode::InApp { action: InAppAction::Group, args, .. } => assert_eq!((args["name"].as_str(), args["tabs"].as_str()), ("g", "{wts}")),
            n => panic!("{n:?}"),
        }
        assert!(err("#create").contains("expected #create {target} terminal"));
        assert!(err("#create terminal").contains("expected #create {target} terminal"));
        assert!(err("#create {t1} terminal npm").starts_with("1:quote the command: #create {t1} terminal"));
        assert!(err("#create {t1} worktree").starts_with("1:expected #create {t1} worktree"));
        assert!(err("#create group \"g\"").starts_with("1:expected #create group"));
        assert!(err("#create {t1} worktree \"a\" from").starts_with("1:expected #create {t1} worktree"));
    }
}
