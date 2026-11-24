//! The terminals of the current workspace, in columns (Bolt's editor
//! groups): each column a tab bar over a framed box (`TermGraphSplit`) that
//! shows its active terminal, with the rivet notes strip under it. Columns
//! are dragged wider and narrower, and rearranged from the pane overlay
//! (⌘'). Only the shown terminals are mounted; every terminal keeps its
//! emulator in the shell state, so one shown again, or moved to another
//! column, comes back with its screen and scrollback. Web pages cannot be
//! kept that way: they live in a layer over the row (`web.rs`), laid over
//! the column that shows them.

use std::{
    cell::{Cell, RefCell},
    path::Path,
    rc::Rc,
};

use nsh_base::{
    Terminal, TerminalApi, TerminalColor, TerminalCursor, TerminalEmulator, TerminalEmulatorEvent,
    TerminalEmulatorFactory, TerminalEvent, TerminalLine, TerminalModes, TerminalSize, VtEmulator, h_resizable,
    resizable_panel, tab, tabs, terminal,
};
use nsh_core::prelude::*;

use crate::{
    changes,
    hints::{self, Opts},
    hub::{self, Hub, HubEntry, next_id},
    notes,
    panes::{Columns, ROWS},
    state::{self, Doc, Shell, Term, Web, shell},
    theme::{BOLT_THEME, Xterm},
    viewer,
    web::{self, Layer},
};

fn color(c: u32) -> Hsla {
    rgb(c).into()
}

/// The current workspace's columns, or none.
fn current(shell: Shell) -> Columns {
    let dir = shell.directory();
    shell.columns.with(|c| c.get(&dir).cloned().unwrap_or_default())
}

/// Bolt's active-column bar (`EditorPanel`'s `bar`): one blue bar in the gap
/// under the active column, centered and at most 250 wide, sliding between
/// columns, shown while the focus is in a column.
#[derive(Clone, Copy)]
struct ActiveBar {
    /// The columns' row, which the bar's offset is from.
    row: ElementRef,
    /// The active column's left edge in the row, and its width.
    at: Signal<Option<(f32, f32)>>,
}

thread_local! {
    /// Each mounted column's view, by a key of its own: when the row is made
    /// again, a column's old and new views may both be registered for a while.
    static COLUMN_VIEWS: RefCell<Vec<(u64, u64, ElementRef)>> = const { RefCell::new(Vec::new()) };
    static NEXT_VIEW: Cell<u64> = const { Cell::new(1) };
}

impl ActiveBar {
    /// Measures the active column against the row. A column's layout event
    /// fires only when its own frame changes, so every column's event
    /// re-measures the active one: a neighbour's resize moves it without
    /// resizing it.
    fn measure(self, shell: Shell) {
        let active = current(shell).active_group;
        let Some(col) = COLUMN_VIEWS.with(|c| c.borrow().iter().rev().find(|(_, g, _)| *g == active).map(|(.., r)| *r)) else {
            self.at.set(None);
            return;
        };
        spawn(async move {
            let (Some(c), Some(r)) = (col.measure().await, self.row.measure().await) else { return };
            let next = Some((c.page.x.0 - r.page.x.0, c.frame.size.width.0));
            if self.at.get_untracked() != next {
                self.at.set(next);
            }
        });
    }

    fn view(self) -> impl IntoElement {
        let width = move || self.at.get().map_or(0.0, |(_, w)| w.min(250.0));
        view()
            .absolute()
            .left_0()
            .bottom(px(4.0))
            .h(px(4.0))
            .rounded_full()
            // Fixed, as in Bolt: the theme's info color is yellow in some themes.
            .bg(color(0x3b82f6))
            .pointer_events(nsh_core::PointerEvents::None)
            .w(move || px(width()))
            .translate_x(move || {
                let (x, w) = self.at.get().unwrap_or_default();
                px(x + (w - width()) / 2.0)
            })
            .opacity(0.0)
            .group_focus_within_named("columns", |s| s.opacity(1.0))
            .transition_all()
            .duration(150.0)
            .ease_out()
    }
}

pub fn terminals() -> impl IntoElement {
    let shell = shell();
    let hub = hub::hub();
    // The columns' ids, keyed: a column that comes or goes leaves the
    // others mounted, so their terminals keep their scroll. Widths reach the
    // panels' sizes without it, so a drag, S and Z leave them mounted too.
    let layout = computed(move || current(shell).groups.iter().map(|g| g.id).collect::<Vec<_>>());
    let row_width = signal(0.0_f32);
    let bar = ActiveBar { row: element_ref(), at: signal(None) };
    let layer = Layer::new(bar.row);
    // Another column taking the focus moves the bar without a layout.
    effect(move || bar.measure(shell));
    // Columns coming, going or reordering move the pages with them.
    effect(move || {
        current(shell);
        layer.measure();
    });
    // A workspace with no tabs keeps the palette open, to go somewhere or
    // start something: closing it opens it again.
    effect(move || {
        if layout.with(Vec::is_empty) && hub.palette.with(Option::is_none) {
            untracked(|| hub.toggle_palette());
        }
    });
    view()
        .flex_1()
        .bg(move || nsh_base::theme().sidebar)
        .group_named("columns")
        .element_ref(&bar.row)
        .on_layout(move |b: &Bounds<Points>| {
            row_width.set(b.size.width.0);
            bar.measure(shell);
            layer.measure();
        })
        .child(
            h_resizable()
                .flex_1()
                .handle_accessibility_label("Resize the column")
                .on_resize(move |sizes: &Vec<Points>| {
                    let widths: Vec<f32> = sizes.iter().map(|s| s.0).collect();
                    shell.set_column_widths(&widths);
                })
                // The group orders its panels by where they are laid out,
                // so one inserted between two lands between them.
                .child(each(move || layout.get(), |id| *id, move |id, _| {
                    let id = id.get_untracked();
                    let joins = move || {
                        let ids = layout.get();
                        let at = ids.iter().position(|g| *g == id).unwrap_or(0);
                        (at > 0, at + 1 < ids.len())
                    };
                    let width = computed(move || {
                        current(shell).groups.iter().find(|g| g.id == id).map_or(0.0, |g| g.width)
                    });
                    resizable_panel()
                        .min_size(px(160.0))
                        .size(move || {
                            let width = width.get();
                            (width > 0.0).then(|| px(width * row_width.get()))
                        })
                        .child(column(shell, hub, id, joins, bar, layer))
                })),
        )
        .child(dynamic(move || layout.with(Vec::is_empty).then(|| empty(shell))))
        .child(layer.view(shell))
        .child(bar.view())
        .child(panes_overlay(shell))
}

/// A workspace with no tabs: a card with its directory, large, on the
/// right, beside the palette that stays open meanwhile. It fills the space
/// a column's terminal box would, under a strip as tall as the tab row;
/// that strip and the margins move the window, as the tab rows they stand
/// in for do.
fn empty(shell: Shell) -> impl IntoElement {
    view()
        .absolute()
        .inset_0()
        .pt(px(36.0))
        .px_3()
        .pb_3()
        .window_drag(true)
        .child(
            view()
                .flex_1()
                .min_h(px(0.0))
                .window_drag(false)
                .gap_4()
                .p_6()
                .rounded(px(12.0))
                .overflow_hidden()
                .bg(color(BOLT_THEME.xterm.background))
                // The terminal box's frame.
                .border_1()
                .border_color(color(BOLT_THEME.vars.fg).opacity(0.09))
                .child(
                    text(move || crate::paths::tilde(&shell.directory()))
                        .number_of_lines(1)
                        .text_right()
                        .text_size(px(32.0))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(color(BOLT_THEME.vars.fg)),
                ),
        )
}

/// A column's tab: a terminal, a file, a web page, the Changes or the
/// Settings tab.
#[derive(Debug, Clone, PartialEq)]
enum TabItem {
    Term(Term),
    Doc(Doc),
    Web(Web),
    Changes(u64),
    Settings(u64),
}

impl TabItem {
    fn id(&self) -> u64 {
        match self {
            TabItem::Term(t) => t.id,
            TabItem::Doc(d) => d.id,
            TabItem::Web(w) => w.id,
            TabItem::Changes(id) | TabItem::Settings(id) => *id,
        }
    }

    fn label(&self) -> String {
        match self {
            TabItem::Term(t) if t.title.trim().is_empty() => format!("shell {}", t.id),
            TabItem::Term(t) => crate::paths::tilde(&t.title),
            // Bolt's diff tab label (`Diff: <file name>`).
            TabItem::Doc(d) => {
                let name = Path::new(&d.path).file_name().map_or_else(|| d.path.clone(), |n| n.to_string_lossy().into_owned());
                if d.proposal.is_some() { format!("Diff: {name}") } else { name }
            }
            TabItem::Web(w) => web::label(&w.url),
            TabItem::Changes(_) => "Changes".into(),
            TabItem::Settings(_) => "Settings".into(),
        }
    }
}

/// One column: its tabs, its shown terminal or file, the notes strip under
/// a terminal. `joins` (left, right): that edge borders another column,
/// whose padding is the other half of the gap (Bolt's `joinsLeft` /
/// `joinsRight`); it changes as columns come and go beside it.
fn column(
    shell: Shell,
    hub: Hub,
    group: u64,
    joins: impl Fn() -> (bool, bool) + Copy + 'static,
    bar: ActiveBar,
    layer: Layer,
) -> impl IntoElement {
    let left = move || if joins().0 { 6.0 } else { 12.0 };
    let right = move || if joins().1 { 6.0 } else { 12.0 };
    let v = &BOLT_THEME.vars;
    let (fg, fg_muted, accent) = (v.fg, v.fg_muted, v.accent);
    let items = move || {
        let cols = current(shell);
        let ids = cols.groups.iter().find(|g| g.id == group).map(|g| g.tabs.clone()).unwrap_or_default();
        let (terms, docs, webs) = (shell.terms.get(), shell.docs.get(), shell.webs.get());
        let changes = shell.changes.with(|c| c.iter().map(|c| c.id).collect::<Vec<_>>());
        let settings = shell.settings_tab.with(|s| s.as_ref().map(|(id, _)| *id));
        ids.iter()
            .filter_map(|id| {
                terms
                    .iter()
                    .find(|t| t.id == *id)
                    .map(|t| TabItem::Term(t.clone()))
                    .or_else(|| docs.iter().find(|d| d.id == *id).map(|d| TabItem::Doc(d.clone())))
                    .or_else(|| webs.iter().find(|w| w.id == *id).map(|w| TabItem::Web(w.clone())))
                    .or_else(|| changes.contains(id).then_some(TabItem::Changes(*id)))
                    .or_else(|| (settings == Some(*id)).then_some(TabItem::Settings(*id)))
            })
            .collect::<Vec<TabItem>>()
    };
    let shown = computed(move || current(shell).shown_in(group));
    let active = computed(move || current(shell).active_group == group);
    let view_ref = element_ref();
    let key = NEXT_VIEW.with(|n| n.replace(n.get() + 1));
    COLUMN_VIEWS.with(|c| c.borrow_mut().push((key, group, view_ref)));
    cleanup(move || COLUMN_VIEWS.with(|c| c.borrow_mut().retain(|(k, ..)| *k != key)));
    let column = view()
        .flex_1()
        .on_press_down(move |_: &PressEvent| shell.focus_column(group))
        // A terminal takes the press itself (to focus its input), so the
        // press never reaches the column; a mouse release over anything
        // inside it does.
        .on_mouse_up(move |_: &MouseEvent| shell.focus_column(group));
    hints::hint(column, "editor-column", Opts { center: true, ..Opts::default() }, None, move || shell.focus_column(group))
        .child(
            view()
                // Measured for the active-column bar: as wide as the column,
                // and not the column itself, whose ref the hint takes.
                .element_ref(&view_ref)
                .on_layout(move |_: &Bounds<Points>| {
                    bar.measure(shell);
                    layer.measure();
                })
                .flex_row()
                .items_center()
                .h(px(36.0))
                // The first column's row clears the traffic lights when the
                // strip that holds them is hidden.
                .pl(move || {
                    let lights = if !joins().0 && !shell.sidebar_open.get() { nsh_core::element::desktop::title_bar().left.0 } else { 0.0 };
                    px(left() - 4.0 + lights)
                })
                .pr(move || px(right() - 4.0))
                .gap_1()
                // The tab rows are the window's top edge: they move it.
                .window_drag(true)
                .child(
                    tabs()
                        .flex_row()
                        .gap_1()
                        .flex_1()
                        .indicator_style(move |s| {
                            s.bottom_0().h(px(2.0)).bg(move || color(if active.get() { accent } else { fg_muted }))
                        })
                        .child(each(items, TabItem::id, move |item, _| item_tab(shell, item, shown))),
                )
                .child(hints::hint(
                    text("+")
                        .px_2()
                        .text_color(color(fg_muted))
                        .hover(move |s| s.text_color(color(fg)))
                        .on_press(move || {
                            shell.focus_column(group);
                            open_here(hub);
                        }),
                    "editor-tabs",
                    Opts::default(),
                    None,
                    move || {
                        shell.focus_column(group);
                        open_here(hub);
                    },
                )),
        )
        .child(
            view()
                .flex_1()
                .pl(move || px(left()))
                .pr(move || px(right()))
                .pb_3()
                .gap_2()
                .child(hints::hint(
                    view()
                        .flex_1()
                        .rounded(px(12.0))
                        .overflow_hidden()
                        .bg(color(BOLT_THEME.xterm.background))
                        // Bolt's frame: `1px solid color-mix(in srgb,
                        // var(--theme-fg) 9%, transparent)`.
                        .border_1()
                        .border_color(color(fg).opacity(0.09))
                        .child(each(move || shown.get().into_iter().collect::<Vec<_>>(), |id| *id, move |id, _| {
                            let id = id.get_untracked();
                            if shell.terms.with_untracked(|t| t.iter().any(|t| t.id == id)) {
                                term_view().term(id as u32).group(group as u32).flex_1().into_element()
                            } else if shell.changes.with_untracked(|c| c.iter().any(|c| c.id == id)) {
                                changes::changes_view().tab(id as u32).flex_1().into_element()
                            } else if shell.webs.with_untracked(|w| w.iter().any(|w| w.id == id)) {
                                web::tab_view(shell, layer, id, active.get_untracked()).into_element()
                            } else if shell.settings_tab.with_untracked(|s| s.as_ref().is_some_and(|(s, _)| *s == id)) {
                                crate::settings_tab::settings_view(shell)
                            } else {
                                viewer::doc_view().doc(id as u32).flex_1().into_element()
                            }
                        })),
                    "editor-view",
                    Opts::default(),
                    None,
                    move || shell.focus_column(group),
                ))
                .child(dynamic(move || {
                    let id = shown.get()?;
                    let t = shell.terms.with_untracked(|ts| ts.iter().find(|t| t.id == id).cloned())?;
                    Some(notes::strip(shell, id, t.session_id, t.cwd))
                })),
        )
}

/// The pane overlay (`PaneOverlay`, ⌘'): the column actions and their
/// keys; a key runs its action and the panel stays up so actions chain;
/// Escape closes it. Actions that cannot run are dimmed.
fn panes_overlay(shell: Shell) -> impl IntoElement {
    let v = &BOLT_THEME.vars;
    let (widget, fg, faint, lighter) = (v.bg_widget, v.fg, v.fg_muted, crate::theme::lighten(v.bg, 0.04));
    when(shell.panes_overlay, move || {
        on_key_with("escape", KeyOptions { when: nsh_core::keymap::When::Always, ..Default::default() }, move |_| {
            shell.panes_overlay.set(false)
        });
        for row in ROWS {
            for (key, _, action) in row.iter() {
                let action = *action;
                on_key_with(&key.to_lowercase(), KeyOptions { when: nsh_core::keymap::When::Always, ..Default::default() }, move |_| {
                    shell.pane_action(action);
                });
            }
        }
        view().absolute().inset_0().items_center().justify_center().child(
            view()
                .min_w(px(380.0))
                .p(px(16.0))
                .rounded(px(10.0))
                .bg(color(widget))
                .gap(px(6.0))
                .child(text("PANES").text_size(px(10.0)).text_color(color(faint)).mb(px(6.0)))
                .child(
                    ROWS.iter()
                        .map(|row| {
                            view().flex_row().gap_2().child(
                                row.iter()
                                    .map(|(key, label, action)| {
                                        let action = *action;
                                        let enabled = move || current(shell).is_available(action);
                                        view()
                                            .flex_1()
                                            .flex_row()
                                            .items_center()
                                            .gap_2()
                                            .px(px(6.0))
                                            .py_1()
                                            .rounded(px(4.0))
                                            .bg(color(lighter))
                                            .opacity(move || if enabled() { 1.0_f32 } else { 0.25 })
                                            .child(
                                                text(*key)
                                                    .w(px(18.0))
                                                    .text_center()
                                                    .rounded(px(3.0))
                                                    .bg(color(crate::theme::lighten(lighter, 0.08)))
                                                    .font_family("Menlo")
                                                    .text_size(px(11.0))
                                                    .text_color(color(fg)),
                                            )
                                            .child(text(*label).font_family("Menlo").text_size(px(12.0)).text_color(color(fg)))
                                    })
                                    .collect::<Vec<_>>(),
                            )
                        })
                        .collect::<Vec<_>>(),
                ),
        )
    })
}

/// Line height over font size: xterm's 14 over 12, Monaco's 18 over 12;
/// both keep their ratio as ⌘+ / ⌘- change the size.
const TERMINAL_LINE: f32 = 14.0 / 12.0;
pub(crate) const CODE_LINE: f32 = 18.0 / 12.0;

/// xterm's font: JetBrains Mono when installed, else what Chromium's
/// `monospace` falls back to on macOS, Menlo.
pub(crate) fn font_family() -> &'static str {
    let installed = [dirs::font_dir(), Some("/Library/Fonts".into())].into_iter().flatten().any(|d| {
        std::fs::read_dir(Path::new(&d))
            .map(|r| r.filter_map(Result::ok).any(|e| e.file_name().to_string_lossy().starts_with("JetBrainsMono")))
            .unwrap_or(false)
    });
    if installed { "JetBrains Mono" } else { "Menlo" }
}

/// The terminal's emulator, shared with the shell state, with Bolt's xterm
/// theme: the 16 ANSI colors and the default foreground from the editor
/// theme, as `monacoToXtermTheme` gives xterm; and mode 2004 reported to
/// the shell state, for pastes.
struct Themed {
    id: u64,
    inner: Rc<RefCell<VtEmulator>>,
    xterm: &'static Xterm,
}

fn map_color(xterm: &Xterm, c: TerminalColor) -> TerminalColor {
    match c {
        TerminalColor::Indexed(n) if n < 16 => {
            let v = xterm.ansi[n as usize];
            TerminalColor::Rgb((v >> 16) as u8, (v >> 8) as u8, v as u8)
        }
        other => other,
    }
}

impl TerminalEmulator for Themed {
    fn feed(&mut self, bytes: &[u8]) {
        let mut inner = self.inner.borrow_mut();
        inner.feed(bytes);
        state::set_bracketed(self.id, inner.modes().bracketed_paste);
    }
    fn resize(&mut self, size: TerminalSize) {
        self.inner.borrow_mut().resize(size);
    }
    fn line_count(&self) -> usize {
        self.inner.borrow().line_count()
    }
    fn line(&self, index: usize) -> TerminalLine {
        let fg = self.xterm.foreground;
        let mut line = self.inner.borrow().line(index);
        for run in &mut line.runs {
            let style = &mut run.style;
            style.fg = Some(map_color(
                self.xterm,
                style.fg.unwrap_or(TerminalColor::Rgb((fg >> 16) as u8, (fg >> 8) as u8, fg as u8)),
            ));
            style.bg = style.bg.map(|c| map_color(self.xterm, c));
        }
        line
    }
    fn cursor(&self) -> Option<TerminalCursor> {
        self.inner.borrow().cursor()
    }
    fn cursor_position(&self) -> TerminalCursor {
        self.inner.borrow().cursor_position()
    }
    fn take_events(&mut self) -> Vec<TerminalEmulatorEvent> {
        self.inner.borrow_mut().take_events()
    }
    fn modes(&self) -> TerminalModes {
        self.inner.borrow().modes()
    }
}

/// A new terminal in the current workspace's active column; a shell that
/// cannot start says why in the palette.
pub fn open_here(hub: Hub) {
    if let Err(e) = hub.shell.open_terminal(hub.shell.directory()) {
        hub.post(HubEntry::Say { id: next_id(), from: "bolt".into(), text: e, error: true, to: None });
        if hub.palette.with_untracked(Option::is_none) {
            hub.toggle_palette();
        }
    }
}

fn item_tab(shell: Shell, item: ReadSignal<TabItem>, shown: Memo<Option<u64>>) -> impl IntoElement {
    let id = item.get_untracked().id();
    let selected = move || shown.get() == Some(id);
    let v = &BOLT_THEME.vars;
    let (fg, fg_muted) = (v.fg, v.fg_muted);
    // The cap is the tab's, the label shrinking inside it: a cap on the
    // label alone clips its text, but the tab is still laid out as wide as
    // the whole label, and the underline spans that empty width.
    let tab = tab()
        .selected(selected)
        .on_press(move |_: &()| shell.show(id))
        .flex_row()
        .items_center()
        .gap_2()
        .px_3()
        .py_1()
        .max_w(px(260.0))
        .child(
            text(move || item.get().label())
                .text_sm()
                .number_of_lines(1)
                .flex_shrink(1.0)
                .min_w(px(0.0))
                .text_color(move || color(if selected() { fg } else { fg_muted })),
        )
        .child(hints::hint(
            text("×").text_sm().text_color(color(fg_muted)).on_press(move || shell.close_tab(id)),
            "row-controls",
            Opts::default(),
            None,
            move || shell.close_tab(id),
        ));
    hints::hint(tab, "editor-tabs", Opts { primary: true, ..Opts::default() }, None, move || shell.show(id))
}

#[derive(Clone, Copy, PartialEq, Props)]
pub struct TermViewProps {
    /// The terminal ([`Term::id`]).
    pub term: Prop<u32>,
    /// The column showing it ([`crate::panes::Group::id`]).
    pub group: Prop<u32>,
}

/// One shown terminal, wired to its PTY: the program's output reaches it
/// through the ref (the shell state writes to whichever view is mounted),
/// keys go out through `onData`, the cell size through `onResize`. A
/// component: a terminal's view is made under the component that places
/// it, so the emulator factory has to be provided by one.
#[component]
pub fn TermView(props: TermViewProps) -> impl IntoElement {
    let (shell, hub) = (shell(), hub::hub());
    let id = u64::from(props.term.get_untracked());
    let group = u64::from(props.group.get_untracked());
    let column_active = computed(move || current(shell).active_group == group);
    // Bolt's xterm colors over the terminal's shared emulator.
    let xterm = &BOLT_THEME.xterm;
    provide_context(TerminalEmulatorFactory::new(move |size, scrollback| {
        Box::new(Themed { id, inner: state::emulator(id, size, scrollback), xterm })
    }));
    let tref = component_ref::<Terminal>();
    let mounted = signal(false);
    // Not a signal: cleanup runs once the owner's signals are gone.
    let token = Rc::new(Cell::new(None::<u64>));
    let set = token.clone();
    spawn(async move {
        while tref.api::<TerminalApi>().is_none() {
            next_frame().await;
        }
        set.set(Some(state::set_view(id, tref)));
        mounted.set(true);
    });
    cleanup(move || {
        if let Some(t) = token.get() {
            state::clear_view(id, t);
        }
    });
    // The active column's terminal takes the keys when it shows, when its
    // column becomes active, and when the palette or the pane overlay
    // closes over it.
    effect(move || {
        if mounted.get() && column_active.get() && hub.palette.with(Option::is_none) && !shell.panes_overlay.get() {
            if let Some(api) = tref.api::<TerminalApi>() {
                api.focus();
            }
        }
    });
    terminal()
        .component_ref(&tref)
        .accessibility_label("Shell")
        .font_family(font_family())
        .font_size(move || Points(shell.font_size_tracked()))
        .line_height(move || Points((shell.font_size_tracked() * TERMINAL_LINE).round()))
        .cursor_style(|s| s.bg(color(BOLT_THEME.xterm.cursor)))
        .scrollback(state::SCROLLBACK as u32)
        .on_data(move |data: &SharedString| state::pty_write(id, data))
        .on_resize(move |e: &TerminalEvent| {
            if let &TerminalEvent::Resize(cols, rows) = e {
                state::pty_resize(id, cols, rows);
            }
        })
        .on_title_change(move |title: &SharedString| shell.set_title(id, title.to_string()))
        .flex_1()
        .p_2()
}
