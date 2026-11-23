//! Bolt's web tab (`WebTab.tsx`, `webviewPreload.js`, the `<webview>` half
//! of `main.js`): a page in a column, under a toolbar with back, forward,
//! reload, the URL bar, the switch that lets the `browse` MCP tool drive
//! it, and Copy URL.
//!
//! A `WKWebView` loses its page when it unmounts, and a column mounts only
//! the tab it shows (and a workspace switch remounts the columns). So the
//! pages are not in the columns: every web tab's page, of every workspace,
//! lives in one [`Layer`] over the columns' row, for as long as the tab is
//! open. A column showing a web tab draws its toolbar over a placeholder
//! ([`tab_view`]); the layer lays the page over that placeholder, and parks
//! it out of sight, at its size, while no column shows it. Switching tabs,
//! moving the tab to another column and switching workspaces keep the
//! page, its history, its scroll and what was typed into it.
//!
//! The page runs [`PRELOAD`] before its own scripts: shift-click inspects
//! an element (`getInspectedElement`), the console is kept for `browse
//! console`, and links and `window.open` calls that would open a window go
//! to the system browser, as Bolt's `setWindowOpenHandler` sends them (a
//! web view without a window of its own opens nothing for them).
//!
//! [`evaluate`] does not use the web view ref's `evaluate`: nsh 0.6.1's
//! AppKit mounter never hands that command to the web view (it times out).
//! The expression is injected instead and its value comes back as a
//! message, as everything [`PRELOAD`] reports does.

use std::{
    cell::{Cell, RefCell},
    collections::{HashMap, VecDeque},
    time::Duration,
};

use nsh_base::input;
use nsh_core::{WebInput, WebLoad, prelude::*};
use serde_json::Value;

use crate::{
    hints::{self, Opts},
    hub::engine::spawn_root,
    state::{Shell, Web},
    theme::BOLT_THEME,
};

/// Console entries a tab keeps (Bolt's `pushLog`).
const CONSOLE_MAX: usize = 500;

/// Where a page no column shows is parked: left of the window.
const PARKED_X: f32 = -20_000.0;

/// What a page is laid out at before any column showed it.
const FIRST_SIZE: (f32, f32) = (1024.0, 768.0);

/// How long [`evaluate`] waits for the page.
const EVAL_TIMEOUT: Duration = Duration::from_secs(8);

/// Runs in every page before its own scripts (`webviewPreload.js`, plus
/// what Electron's `console-message` and `setWindowOpenHandler` gave Bolt
/// for free). It reports through the web view's script message handler as
/// JSON (the page may replace `window.nsh`, not WebKit's handler): `t` is
/// `inspect`, `console` or `open`.
const PRELOAD: &str = r#"(() => {
  if (window.__rivetPreload) return;
  window.__rivetPreload = true;
  const handler = window.webkit.messageHandlers.nsh;
  const post = (m) => {
    try { handler.postMessage(JSON.stringify(m)); } catch (_) {}
  };

  let shift = false;
  let hovered = null;
  let prevOutline = "";
  const paint = (el) => {
    if (hovered === el) return;
    if (hovered) hovered.style.outline = prevOutline;
    hovered = el;
    if (!el) return;
    prevOutline = el.style.outline;
    el.style.outline = "2px solid #4f9cf9";
  };
  const clear = () => { shift = false; paint(null); };
  // nth-of-type at every level: unique without depending on ids or classes
  // the page may reuse.
  const selectorFor = (el) => {
    const parts = [];
    for (let n = el; n && n.nodeType === 1 && n !== document.body; n = n.parentElement) {
      const tag = n.tagName.toLowerCase();
      const siblings = n.parentElement ? [...n.parentElement.children].filter((c) => c.tagName === n.tagName) : [];
      parts.unshift(siblings.length > 1 ? `${tag}:nth-of-type(${siblings.indexOf(n) + 1})` : tag);
    }
    return parts.join(" > ");
  };
  const describe = (el) => {
    const r = el.getBoundingClientRect();
    const attrs = {};
    for (const a of el.attributes) if (a.name.startsWith("data-")) attrs[a.name] = a.value;
    return {
      selector: selectorFor(el),
      tag: el.tagName.toLowerCase(),
      id: el.id || null,
      classes: el.className && typeof el.className === "string" ? el.className : null,
      text: (el.innerText || el.textContent || "").trim().slice(0, 300),
      attrs,
      rect: { x: r.x, y: r.y, width: r.width, height: r.height },
      url: location.href,
      outerHTML: el.outerHTML.slice(0, 2000),
    };
  };
  addEventListener("keydown", (e) => { if (e.key === "Shift") shift = true; });
  addEventListener("keyup", (e) => { if (e.key === "Shift") clear(); });
  addEventListener("blur", clear);
  addEventListener("mousemove", (e) => {
    if (!shift) return paint(null);
    if (e.target && e.target.nodeType === 1) paint(e.target);
  });
  // Capture phase on both: preventDefault alone leaves the page's own
  // handlers running, and shift-drag selection anchors on mousedown.
  const swallow = (e) => {
    if (!shift) return;
    e.preventDefault();
    e.stopPropagation();
    if (e.type === "click" && e.target && e.target.nodeType === 1) {
      post({ t: "inspect", el: describe(e.target) });
      clear();
    }
  };
  addEventListener("mousedown", swallow, true);
  addEventListener("click", swallow, true);

  const str = (v) => {
    if (typeof v === "string") return v;
    if (v instanceof Error) return v.stack || String(v);
    try { return JSON.stringify(v); } catch (_) { return String(v); }
  };
  for (const level of ["log", "info", "warn", "error", "debug"]) {
    const orig = console[level];
    console[level] = function (...args) {
      post({ t: "console", level, message: args.map(str).join(" ") });
      return orig.apply(this, args);
    };
  }
  addEventListener("error", (e) => {
    post({ t: "console", level: "error", message: e.message, source: e.filename ? `${e.filename}:${e.lineno}` : "" });
  });
  addEventListener("unhandledrejection", (e) => {
    post({ t: "console", level: "error", message: "Unhandled rejection: " + str(e.reason) });
  });

  window.open = function (url) {
    if (url) post({ t: "open", url: new URL(String(url), location.href).href });
    return null;
  };
  // Bubbling, so a page that handles the click itself (a router) keeps it.
  addEventListener("click", (e) => {
    if (e.defaultPrevented || !e.target || !e.target.closest) return;
    const a = e.target.closest(e.metaKey ? "a[href]" : "a[target]");
    if (!a || !a.href) return;
    const target = (a.getAttribute("target") || "").toLowerCase();
    if (!e.metaKey && ["", "_self", "_parent", "_top"].includes(target)) return;
    e.preventDefault();
    post({ t: "open", url: a.href });
  });
})();"#;

/// A shown page's place in the columns' row.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Slot {
    x: f32,
    y: f32,
    w: f32,
    h: f32,
}

/// A page's live parts, by web tab id.
struct Live {
    el: ElementRef,
    console: VecDeque<String>,
}

thread_local! {
    static LIVE: RefCell<HashMap<u64, Live>> = RefCell::new(HashMap::new());
    /// The mounted placeholders: (key, web tab, view). When a column is made
    /// again, its old and new placeholders may both be registered for a
    /// while; the newest wins.
    static PLACEHOLDERS: RefCell<Vec<(u64, u64, ElementRef)>> = const { RefCell::new(Vec::new()) };
    static NEXT_KEY: Cell<u64> = const { Cell::new(1) };
    /// The element last shift-clicked in a web tab (`webviewInspect.ts`):
    /// one slot, as only the last click matters.
    static INSPECTED: RefCell<Option<Value>> = const { RefCell::new(None) };
    /// [`evaluate`]s waiting for the page's answer, by request.
    static EVALS: RefCell<HashMap<u64, tokio::sync::oneshot::Sender<Result<Value, String>>>> = RefCell::new(HashMap::new());
    static NEXT_EVAL: Cell<u64> = const { Cell::new(1) };
}

/// The pages over the columns' row, and where the columns want them.
#[derive(Clone, Copy)]
pub struct Layer {
    /// The columns' row, which the places are measured from.
    row: ElementRef,
    /// Each shown web tab's place, by its id.
    slots: Signal<HashMap<u64, Slot>>,
}

impl Layer {
    pub fn new(row: ElementRef) -> Self {
        Self { row, slots: signal(HashMap::new()) }
    }

    /// Measures every placeholder against the row, once the frame being
    /// laid out is: a layout event fires only for the view whose frame
    /// changed, so anything that may move a placeholder (a column coming,
    /// going, resized or reordered, the row resized) measures them all.
    pub fn measure(self) {
        spawn_root(async move {
            next_frame().await;
            let shown: Vec<(u64, ElementRef)> = PLACEHOLDERS.with(|p| {
                let mut latest: Vec<(u64, ElementRef)> = Vec::new();
                for (_, tab, el) in p.borrow().iter() {
                    match latest.iter_mut().find(|(t, _)| t == tab) {
                        Some(entry) => entry.1 = *el,
                        None => latest.push((*tab, *el)),
                    }
                }
                latest
            });
            let Some(row) = self.row.measure().await else { return };
            let mut next = HashMap::new();
            for (tab, el) in shown {
                let Some(m) = el.measure().await else { continue };
                let (x, y) = (m.page.x.0 - row.page.x.0, m.page.y.0 - row.page.y.0);
                next.insert(tab, Slot { x, y, w: m.frame.size.width.0, h: m.frame.size.height.0 });
            }
            if self.slots.get_untracked() != next {
                self.slots.set(next);
            }
        });
    }

    /// The pages, every open web tab's, kept mounted while their tabs are
    /// open. The row's children under it take the presses it does not.
    pub fn view(self, shell: Shell) -> impl IntoElement {
        let ids = computed(move || shell.webs.with(|w| w.iter().map(|w| w.id).collect::<Vec<_>>()));
        view()
            .absolute()
            .inset_0()
            .pointer_events(nsh_core::PointerEvents::BoxNone)
            .child(each(move || ids.get(), |id| *id, move |id, _| {
                let id = id.get_untracked();
                match shell.web(id) {
                    Some(web) => page(shell, self, web).into_element(),
                    None => view().into_element(),
                }
            }))
    }
}

/// One tab's page, over its placeholder or parked.
fn page(shell: Shell, layer: Layer, web: Web) -> impl IntoElement {
    let id = web.id;
    let el = element_ref();
    LIVE.with(|l| l.borrow_mut().insert(id, Live { el, console: VecDeque::new() }));
    cleanup(move || {
        LIVE.with(|l| l.borrow_mut().remove(&id));
    });
    let at = computed(move || layer.slots.with(|s| s.get(&id).copied()));
    // Parked at the size it last showed at, so the page keeps its layout
    // (and its scroll) rather than reflowing to nothing.
    let size = signal(FIRST_SIZE);
    effect(move || {
        if let Some(s) = at.get() {
            size.set((s.w, s.h));
        }
    });
    let page = if web.opened.is_empty() { web_view_html(String::new(), None) } else { web_view(web.opened.clone()) };
    view()
        .absolute()
        .left(move || px(at.get().map_or(PARKED_X, |s| s.x)))
        .top(move || px(at.get().map_or(0.0, |s| s.y)))
        .w(move || px(at.get().map_or_else(|| size.get().0, |s| s.w)))
        .h(move || px(at.get().map_or_else(|| size.get().1, |s| s.h)))
        // The column's frame is rounded; the page is its bottom part.
        .rounded_b(px(11.0))
        .overflow_hidden()
        .child(
            page.element_ref(&el)
                .flex_1()
                .injected_java_script_before_content(PRELOAD.to_owned())
                .on_load_start(move |n: &WebNavigation| {
                    LIVE.with(|l| {
                        if let Some(live) = l.borrow_mut().get_mut(&id) {
                            live.console.clear();
                        }
                    });
                    navigated(shell, id, n);
                })
                .on_navigation_state_change(move |n: &WebNavigation| navigated(shell, id, n))
                .on_load(move |n: &WebNavigation| navigated(shell, id, n))
                .on_error(move |message: &str| {
                    log(id, format!("[error] {message}"));
                    shell.update_web(id, |w| w.loading = false);
                })
                .on_message(move |data: &str| message(shell, id, data)),
        )
}

fn navigated(shell: Shell, id: u64, n: &WebNavigation) {
    // A blank tab's empty page.
    let url = if n.url == "about:blank" { String::new() } else { n.url.clone() };
    let same = shell.web(id).is_none_or(|w| {
        w.url == url && w.loading == n.loading && w.back == n.can_go_back && w.forward == n.can_go_forward
    });
    if !same {
        shell.update_web(id, |w| {
            w.url = url;
            w.loading = n.loading;
            w.back = n.can_go_back;
            w.forward = n.can_go_forward;
        });
    }
}

fn log(id: u64, line: String) {
    LIVE.with(|l| {
        if let Some(live) = l.borrow_mut().get_mut(&id) {
            if live.console.len() == CONSOLE_MAX {
                live.console.pop_front();
            }
            live.console.push_back(line);
        }
    });
}

/// What [`PRELOAD`] reported.
fn message(shell: Shell, id: u64, data: &str) {
    let Ok(m) = serde_json::from_str::<Value>(data) else { return };
    match m["t"].as_str() {
        Some("console") => {
            let (level, text) = (m["level"].as_str().unwrap_or("log"), m["message"].as_str().unwrap_or_default());
            let line = match m["source"].as_str().filter(|s| !s.is_empty()) {
                Some(source) => format!("[{level}] {text} ({source})"),
                None => format!("[{level}] {text}"),
            };
            log(id, line);
        }
        Some("eval") => {
            let Some(sender) = m["n"].as_u64().and_then(|n| EVALS.with(|e| e.borrow_mut().remove(&n))) else { return };
            let result = match m["ok"].as_bool() {
                Some(true) => Ok(m["value"].clone()),
                _ => Err(m["error"].as_str().unwrap_or("the script failed").to_owned()),
            };
            let _ = sender.send(result);
        }
        Some("inspect") => {
            INSPECTED.with(|i| *i.borrow_mut() = Some(m["el"].clone()));
            shell.update_web(id, |w| w.inspected += 1);
        }
        Some("open") => {
            let url = m["url"].as_str().unwrap_or_default();
            let lower = url.to_ascii_lowercase();
            if lower.starts_with("http://") || lower.starts_with("https://") || lower.starts_with("mailto:") {
                let _ = open_url(url);
            }
        }
        _ => {}
    }
}

/// The web tab the `browse` MCP tool acts on (`getActiveWebview`): the one
/// the current workspace's active column shows, else the first column's
/// that shows one.
pub fn shown(shell: Shell) -> Option<u64> {
    let cwd = shell.directory();
    let cols = shell.columns.with_untracked(|c| c.get(&cwd).cloned())?;
    let is_web = |id: &u64| shell.webs.with_untracked(|w| w.iter().any(|w| w.id == *id));
    cols.shown_in(cols.active_group)
        .filter(is_web)
        .or_else(|| cols.groups.iter().filter_map(|g| cols.shown_in(g.id)).find(is_web))
}

fn el_of(id: u64) -> Option<ElementRef> {
    LIVE.with(|l| l.borrow().get(&id).map(|l| l.el))
}

/// Loads `url` in web tab `id` (the URL bar, `browse navigate`).
pub fn navigate(shell: Shell, id: u64, url: &str) {
    let (Some(el), Some(load)) = (el_of(id), WebLoad::new(url, Vec::new())) else { return };
    el.load(load);
    shell.update_web(id, |w| {
        w.url = url.to_owned();
        w.loading = true;
    });
}

/// The value of the JavaScript expression `expression` in web tab `id`'s
/// page (a Promise's once it settles), as JSON. Injected as source, not
/// `eval`ed, so a page whose CSP forbids `eval` (GitHub's) still runs it.
pub async fn evaluate(id: u64, expression: String) -> Result<Value, String> {
    let el = el_of(id).ok_or("The web tab is gone")?;
    let n = NEXT_EVAL.with(|c| c.replace(c.get() + 1));
    let (tx, rx) = tokio::sync::oneshot::channel();
    EVALS.with(|e| e.borrow_mut().insert(n, tx));
    el.inject_java_script(format!(
        r#"(() => {{
  const reply = (m) => window.webkit.messageHandlers.nsh.postMessage(JSON.stringify(Object.assign({{ t: "eval", n: {n} }}, m)));
  Promise.resolve().then(() => (
{expression}
  )).then(
    (value) => {{ try {{ reply({{ ok: true, value: value === undefined ? null : value }}); }} catch (e) {{ reply({{ ok: false, error: "the result is not JSON: " + e }}); }} }},
    (e) => reply({{ ok: false, error: String(e && e.message !== undefined ? e.message : e) }})
  );
}})();"#
    ));
    // A page that navigates away, or never answers, is given up on.
    set_timeout(EVAL_TIMEOUT, move || {
        if let Some(sender) = EVALS.with(|e| e.borrow_mut().remove(&n)) {
            let _ = sender.send(Err("The page did not answer in time".into()));
        }
    });
    rx.await.unwrap_or_else(|_| Err("The web tab is gone".into()))
}

/// Gives web tab `id`'s page `input` as the user's own.
pub fn send_input(id: u64, input: WebInput) {
    if let Some(el) = el_of(id) {
        el.send_input(input);
    }
}

/// Web tab `id`'s console since its page loaded, oldest first.
pub fn console(id: u64) -> Vec<String> {
    LIVE.with(|l| l.borrow().get(&id).map(|l| l.console.iter().cloned().collect()).unwrap_or_default())
}

/// The element last shift-clicked in a web tab.
pub fn inspected() -> Option<Value> {
    INSPECTED.with(|i| i.borrow().clone())
}

fn color(c: u32) -> Hsla {
    rgb(c).into()
}

/// A web tab as its column shows it: the toolbar over the place its page
/// is laid over.
pub fn tab_view(shell: Shell, layer: Layer, id: u64, column_active: bool) -> impl IntoElement {
    let web = computed(move || shell.webs.with(|w| w.iter().find(|w| w.id == id).cloned()));
    let url = computed(move || web.with(|w| w.as_ref().map(|w| w.url.clone()).unwrap_or_default()));
    let draft = signal(url.get_untracked());
    // The bar follows the page.
    effect(move || draft.set(url.get()));
    let flag = move |f: fn(&Web) -> bool| move || web.with(|w| w.as_ref().is_some_and(f));
    let (back, forward, loading, armed) = (flag(|w| w.back), flag(|w| w.forward), flag(|w| w.loading), flag(|w| w.armed));
    let copied = signal(false);
    let sent = signal(false);
    let inspected = computed(move || web.with(|w| w.as_ref().map_or(0, |w| w.inspected)));
    effect(move || {
        if inspected.get() == 0 {
            return;
        }
        sent.set(true);
        set_timeout(Duration::from_millis(1500), move || sent.set(false));
    });

    let v = &BOLT_THEME.vars;
    let (bg, fg, muted, border, hover) = (v.bg, v.fg, v.fg_muted, v.border, v.bg_hover);
    let button = move |label: &'static str, on: fn(Shell, u64), enabled: Box<dyn Fn() -> bool>| {
        let enabled: std::rc::Rc<dyn Fn() -> bool> = enabled.into();
        let (e1, e2, e3) = (enabled.clone(), enabled.clone(), enabled);
        hints::hint(
            text(label)
                .text_sm()
                .px(px(6.0))
                .py(px(2.0))
                .rounded(px(4.0))
                .text_color(move || color(if e1() { fg } else { muted }).opacity(if e1() { 1.0 } else { 0.35 }))
                .hover(move |s| s.bg(color(hover)))
                .on_press(move || {
                    if e2() {
                        on(shell, id);
                    }
                }),
            "web",
            Opts::default(),
            None,
            move || {
                if e3() {
                    on(shell, id);
                }
            },
        )
    };

    let slot = element_ref();
    let key = NEXT_KEY.with(|n| n.replace(n.get() + 1));
    PLACEHOLDERS.with(|p| p.borrow_mut().push((key, id, slot)));
    cleanup(move || {
        PLACEHOLDERS.with(|p| p.borrow_mut().retain(|(k, ..)| *k != key));
        layer.measure();
    });
    let blank = url.get_untracked().is_empty();

    view()
        .flex_1()
        .bg(color(bg))
        .child(
            view()
                .flex_row()
                .items_center()
                .gap_1()
                .h(px(32.0))
                .px_2()
                .border_b_1()
                .border_color(color(border))
                .child(button("←", |s, id| go(s, id, Go::Back), Box::new(back)))
                .child(button("→", |s, id| go(s, id, Go::Forward), Box::new(forward)))
                .child(dynamic(move || {
                    // Reload, or Stop while a page loads.
                    if loading() {
                        button("✕", |s, id| go(s, id, Go::Stop), Box::new(|| true)).into_element()
                    } else {
                        button("↻", |s, id| go(s, id, Go::Reload), Box::new(|| true)).into_element()
                    }
                }))
                .child(
                    input()
                        .value(move || draft.get())
                        .on_change(move |t: &str| draft.set(t.to_owned()))
                        .submit_behavior(SubmitBehavior::Submit)
                        .on_submit(move |t: &str| {
                            if !t.trim().is_empty() {
                                navigate(shell, id, &to_url(t));
                            }
                        })
                        .on_key_press(move |key: &str| {
                            if key == "Escape" {
                                draft.set(url.get_untracked());
                            }
                        })
                        .auto_focus(blank && column_active)
                        .placeholder("Search or enter address")
                        .placeholder_color(color(muted))
                        .text_color(color(muted))
                        .text_size(px(12.0))
                        .flex_1()
                        .min_w(px(0.0))
                        .ml_1(),
                )
                .child(when(move || sent.get(), move || text("⌖").text_sm().text_color(color(fg))))
                .child(hints::hint(
                    text("AI")
                        .text_xs()
                        .px(px(6.0))
                        .py(px(2.0))
                        .rounded(px(4.0))
                        .text_color(move || color(if armed() { fg } else { muted }))
                        .bg(move || if armed() { color(hover) } else { color(bg).opacity(0.0) })
                        .hover(move |s| s.bg(color(hover)))
                        .on_press(move || toggle_armed(shell, id)),
                    "web",
                    Opts::default(),
                    None,
                    move || toggle_armed(shell, id),
                ))
                .child(hints::hint(
                    text(move || if copied.get() { "✓" } else { "⧉" })
                        .text_sm()
                        .px(px(6.0))
                        .py(px(2.0))
                        .rounded(px(4.0))
                        .text_color(color(muted))
                        .hover(move |s| s.bg(color(hover)))
                        .on_press(move || copy_url(url.get_untracked(), copied)),
                    "web",
                    Opts::default(),
                    None,
                    move || copy_url(url.get_untracked(), copied),
                )),
        )
        .child(
            view()
                .flex_1()
                .element_ref(&slot)
                .on_layout(move |_: &Bounds<Points>| layer.measure()),
        )
}

#[derive(Clone, Copy)]
enum Go {
    Back,
    Forward,
    Reload,
    Stop,
}

fn go(shell: Shell, id: u64, to: Go) {
    let Some(el) = el_of(id) else { return };
    match to {
        Go::Back => el.go_back(),
        Go::Forward => el.go_forward(),
        Go::Reload => {
            el.reload();
            shell.update_web(id, |w| w.loading = true);
        }
        Go::Stop => el.stop_loading(),
    }
}

fn toggle_armed(shell: Shell, id: u64) {
    shell.update_web(id, |w| w.armed = !w.armed);
}

fn copy_url(url: String, copied: Signal<bool>) {
    set_clipboard(url);
    copied.set(true);
    set_timeout(Duration::from_millis(1500), move || copied.set(false));
}

/// What the URL bar's text loads (`toUrl`): a URL as it is, a bare host
/// over https (`localhost` over http), anything else a search.
pub fn to_url(input: &str) -> String {
    let s = input.trim();
    let scheme_len = s.find("://").filter(|&at| {
        let scheme = &s[..at];
        scheme.chars().next().is_some_and(|c| c.is_ascii_alphabetic())
            && scheme.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '+' | '.' | '-'))
    });
    if scheme_len.is_some() {
        return s.to_owned();
    }
    // `^localhost(:\d+)?([/?#]|$)`
    if let Some(rest) = s.strip_prefix("localhost") {
        let after_port = match rest.strip_prefix(':') {
            Some(r) => Some(r.trim_start_matches(|c: char| c.is_ascii_digit())).filter(|t| t.len() < r.len()),
            None => Some(rest),
        };
        if after_port.is_some_and(|r| r.is_empty() || r.starts_with(['/', '?', '#'])) {
            return format!("http://{s}");
        }
    }
    if !s.chars().any(char::is_whitespace) && s.contains('.') {
        return format!("https://{s}");
    }
    let query: String = s
        .bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'!' | b'~' | b'*' | b'\'' | b'(' | b')' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect();
    format!("https://www.google.com/search?q={query}")
}

/// A web tab's label (`webTabLabel`): `New Tab` while blank, `PR #n` on a
/// pull request, `Compare` on a compare page, else the host without `www.`.
pub fn label(url: &str) -> String {
    if url.is_empty() {
        return "New Tab".into();
    }
    let Some((_, rest)) = url.split_once("://") else {
        return url.to_owned();
    };
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let (authority, path) = (&rest[..end], &rest[end..]);
    let path = &path[..path.find(['?', '#']).unwrap_or(path.len())];
    let host = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    let host = match host.rsplit_once(':') {
        Some((h, port)) if !h.is_empty() && port.chars().all(|c| c.is_ascii_digit()) && !host.ends_with(']') => h,
        _ => host,
    };
    if let Some(at) = path.find("/pull/") {
        let digits: String = path[at + 6..].chars().take_while(char::is_ascii_digit).collect();
        if !digits.is_empty() {
            return format!("PR #{digits}");
        }
    }
    if path.contains("/compare/") {
        return "Compare".into();
    }
    let host = host.to_ascii_lowercase();
    host.strip_prefix("www.").unwrap_or(&host).to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn to_url_matches_bolt() {
        assert_eq!(to_url("https://example.com/a"), "https://example.com/a");
        assert_eq!(to_url("localhost:3000/x"), "http://localhost:3000/x");
        assert_eq!(to_url("localhost"), "http://localhost");
        assert_eq!(to_url("localhostfoo"), "https://www.google.com/search?q=localhostfoo");
        assert_eq!(to_url("github.com/org/repo"), "https://github.com/org/repo");
        assert_eq!(to_url("rust borrow checker"), "https://www.google.com/search?q=rust%20borrow%20checker");
    }

    #[test]
    fn label_matches_bolt() {
        assert_eq!(label(""), "New Tab");
        assert_eq!(label("https://github.com/o/r/pull/42/files"), "PR #42");
        assert_eq!(label("https://github.com/o/r/compare/main...b?expand=1"), "Compare");
        assert_eq!(label("https://www.Example.com:8080/x"), "example.com");
    }
}
