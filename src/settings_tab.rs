//! The Settings tab (Bolt's `SettingsTab`, opened by `settings`), with the
//! two of Bolt's settings this shell uses: Open in New Column, and the
//! keybindings file.

use nsh_core::prelude::*;

use crate::{
    keybindings,
    state::Shell,
    theme::{BOLT_THEME, lighten},
};

fn color(c: u32) -> Hsla {
    rgb(c).into()
}

/// The tab's page: a heading per category, a row per setting.
pub fn settings_view(shell: Shell) -> AnyElement {
    let v = &BOLT_THEME.vars;
    let open_in_new_column = move || shell.opens_new_column_tracked();
    scroll()
        .flex_1()
        .bg(color(v.bg))
        .child(
            view()
                .p_6()
                .child(text("Settings").text_sm().font_weight(FontWeight::MEDIUM).mb_4().text_color(color(v.fg_muted)))
                .child(category("Editor"))
                .child(row(
                    "Open in New Column",
                    "Open new files, terminals and panels in a new column instead of the active one",
                    toggle(open_in_new_column, move || {
                        let on = !shell.opens_new_column();
                        shell.update_settings(|s| {
                            s.extra.insert("openInNewColumn".into(), on.into());
                        });
                    }),
                ))
                .child(view().h(px(24.0)))
                .child(category("Keybindings"))
                .child(row(
                    "Keybindings",
                    "Customize keyboard shortcuts (read at launch)",
                    button("Open keybindings.json", move || match keybindings::ensure_file() {
                        Ok(path) => shell.open_editable(shell.directory(), path.to_string_lossy().into_owned()),
                        Err(e) => eprintln!("rivet: could not write {}: {e}", keybindings::FILE),
                    }),
                )),
        )
        .into_element()
}

fn category(label: &'static str) -> impl IntoElement {
    text(label.to_uppercase()).text_xs().font_weight(FontWeight::MEDIUM).letter_spacing(px(0.6)).mb_2().text_color(color(BOLT_THEME.vars.fg_muted).opacity(0.7))
}

/// A setting: its label and description at the left, its control at the
/// right, within Bolt's `max-w-md`.
fn row(label: &'static str, description: &'static str, control: impl IntoElement) -> impl IntoElement {
    let v = &BOLT_THEME.vars;
    view()
        .flex_row()
        .items_center()
        .justify_between()
        .gap_4()
        .max_w(px(448.0))
        .py_2()
        .child(
            view()
                .flex_1()
                .min_w(px(0.0))
                .child(text(label).text_xs().font_weight(FontWeight::MEDIUM).text_color(color(v.fg)))
                .child(text(description).text_xs().mt(px(2.0)).text_color(color(v.fg_muted).opacity(0.7))),
        )
        .child(control)
}

/// Bolt's boolean setting: a 32×18 track, the accent when on, a 12pt
/// knob that slides.
fn toggle(on: impl Fn() -> bool + Clone + 'static, flip: impl Fn() + 'static) -> impl IntoElement {
    let v = &BOLT_THEME.vars;
    let (accent, off, border, fg) = (v.accent, lighten(v.bg, 0.08), v.border, v.fg);
    let knob = on.clone();
    view()
        .relative()
        .flex_shrink(0.0)
        .w(px(32.0))
        .h(px(18.0))
        .rounded(px(9.0))
        .border_1()
        .border_color(color(border))
        .bg(move || color(if on() { accent } else { off }))
        .on_press(flip)
        .child(view().absolute().top(px(2.0)).left(move || px(if knob() { 15.0 } else { 2.0 })).w(px(12.0)).h(px(12.0)).rounded_full().bg(color(fg)))
}

fn button(label: &'static str, on_press: impl Fn() + 'static) -> impl IntoElement {
    let v = &BOLT_THEME.vars;
    view()
        .flex_shrink(0.0)
        .px_3()
        .py_1()
        .rounded(px(4.0))
        .border_1()
        .border_color(color(v.border))
        .bg(color(v.bg_widget))
        .hover(|s| s.bg(color(v.bg_hover)))
        .on_press(on_press)
        .child(text(label).text_xs().text_color(color(v.fg)))
}
