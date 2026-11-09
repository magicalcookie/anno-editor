// Editor chrome: buttons, menus, panels, tooltips, modals and the status bar.
//
// Builders return plain GPUI elements without listeners. Attach behavior at
// the call site with `.on_click(cx.listener(...))`, so widgets stay usable
// from any view type.
use crate::{icon::Icon, theme};
use gpui::{prelude::*, *};

/// A text button. The label doubles as the element id, so keep labels unique
/// within one parent.
pub fn button(label: impl Into<SharedString>, selected: bool) -> Stateful<Div> {
    let label = label.into();
    div()
        .id(ElementId::Name(label.clone()))
        .flex()
        .items_center()
        .justify_center()
        .h(px(26.))
        .px_2()
        .rounded(px(2.))
        .cursor_pointer()
        .bg(if selected {
            theme::get().selected
        } else {
            theme::get().panel
        })
        .text_color(theme::get().text)
        // Hover must not wash out a selected button.
        .when(!selected, |d| d.hover(|s| s.bg(theme::get().hover)))
        .child(label)
}

/// How much an [`action`] button asks for attention.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tone {
    /// The one thing a view is for: Create, Install.
    Primary,
    /// Every other action.
    Secondary,
    /// Removes or overwrites something.
    Danger,
}

/// A command button: filled and bordered, so it reads as clickable next to
/// text. [`button`] stays for toggles, tabs and menu entries. The label is
/// the element id, as for `button`.
pub fn action(label: impl Into<SharedString>, tone: Tone) -> Stateful<Div> {
    let label = label.into();
    let t = theme::get();
    let (bg, border, text, hover) = match tone {
        Tone::Primary => (t.primary, t.primary_border, t.on_accent, t.primary_hover),
        Tone::Secondary => (t.secondary, t.border, t.text, t.hover),
        Tone::Danger => (t.secondary, t.danger_border, t.error, t.danger_hover),
    };
    div()
        .id(ElementId::Name(label.clone()))
        .flex()
        .flex_shrink_0()
        .items_center()
        .justify_center()
        .h(px(28.))
        .px_3()
        .rounded(px(3.))
        .border_1()
        .border_color(border)
        .bg(bg)
        .text_size(px(12.))
        .text_color(text)
        .cursor_pointer()
        .hover(move |s| s.bg(hover))
        .child(label)
}

/// An [`action`] that cannot run yet: same footprint, dimmed, no click,
/// and a tooltip that says what it is waiting for, so the toolbar does not
/// jump when the state changes.
pub fn action_off(label: impl Into<SharedString>, why: impl Into<SharedString>) -> Stateful<Div> {
    let label = label.into();
    div()
        .id(ElementId::Name(format!("off-{label}").into()))
        .flex()
        .flex_shrink_0()
        .items_center()
        .justify_center()
        .h(px(28.))
        .px_3()
        .rounded(px(3.))
        // A faint frame and disabled text: with the full border of a live
        // button, only the dimmed label set it apart and it still read as
        // clickable.
        .border_1()
        .border_color(theme::get().border.alpha(0.375))
        .text_size(px(12.))
        .text_color(theme::get().disabled)
        .child(label.clone())
        .tooltip(Tooltip::text(label, "", why.into()))
}

/// The primary [`action`] of a dialog that cannot run yet. `why` is the
/// tooltip; say it beside the button too, since a dimmed main button with
/// no visible reason reads as broken.
pub fn primary_off(label: impl Into<SharedString>, why: impl Into<SharedString>) -> Stateful<Div> {
    // Plain disabled: a faint tint of the primary read as "nearly enabled".
    action_off(label, why)
}

/// A flat [`button`] that cannot run yet; see [`action_off`].
pub fn button_off(label: impl Into<SharedString>, why: impl Into<SharedString>) -> Stateful<Div> {
    let label = label.into();
    div()
        .id(ElementId::Name(format!("off-{label}").into()))
        .flex()
        .items_center()
        .justify_center()
        .h(px(26.))
        .px_2()
        .rounded(px(2.))
        .bg(theme::get().panel)
        .text_color(theme::get().disabled)
        .child(label.clone())
        .tooltip(Tooltip::text(label, "", why.into()))
}

/// A document's state and the one action that moves it on, as one joined
/// control: `● Unsaved changes [Save]`, `● Game copy outdated [Update]`.
/// `dot` colours the state; `action` is left out when nothing is due.
/// "Saved ● game copy outdated": a settled lead before " · " stays muted
/// and the dot moves onto the tail, so a warning colour never marks the
/// healthy half of the sentence.
pub fn status_chip(dot: Rgba, text: impl Into<SharedString>, action: Option<Stateful<Div>>) -> Div {
    let text: SharedString = text.into();
    let (lead, state) = match text.split_once(" · ") {
        // The dot is the separator: "Saved · ●" read as two.
        Some((lead, state)) => (Some(lead.to_owned()), state.to_owned()),
        None => (None, text.to_string()),
    };
    div()
        .flex()
        .flex_shrink_0()
        .items_center()
        .gap_2()
        .h(px(24.))
        .pl_2()
        .pr(px(if action.is_some() { 1. } else { 8. }))
        .rounded(px(4.))
        .border_1()
        .border_color(theme::get().border)
        .bg(theme::get().input)
        .text_size(px(11.))
        .children(lead.map(|l| div().text_color(theme::get().muted).child(l)))
        .child(div().size(px(7.)).rounded_full().bg(dot))
        .child(div().text_color(theme::get().text).child(state))
        .children(action.map(|a| a.h(px(20.)).px_2().text_size(px(11.))))
}

/// A keyboard key, for shortcut hints: `Raise [1]`, `[Enter] Last selected`.
pub fn keycap(key: impl Into<SharedString>) -> Div {
    div()
        .flex_shrink_0()
        .px_1()
        .rounded(px(3.))
        .border_1()
        .border_color(theme::get().floating_border)
        .bg(theme::get().input)
        .text_size(px(10.))
        .font_weight(FontWeight::NORMAL)
        .text_color(theme::get().muted)
        .child(key.into())
}

/// A [`keycap`] for a filled (primary) button: an outline in the label's
/// colour, no fill of its own, so it reads as part of the button rather
/// than a second one set on it.
pub fn keycap_on_fill(key: impl Into<SharedString>) -> Div {
    div()
        .flex_shrink_0()
        .px_1()
        .rounded(px(3.))
        .border_1()
        .border_color(theme::get().on_accent.alpha(0.35))
        .text_size(px(10.))
        .font_weight(FontWeight::NORMAL)
        .text_color(theme::get().on_accent.alpha(0.85))
        .child(key.into())
}

/// A tool strip button: icon, its name under it. Names make a strip of
/// similar glyphs readable without hovering each. Attach `.on_click` and a
/// tooltip (shortcut, detail) at the call site.
pub fn tool_button(
    id: impl Into<ElementId>,
    icon: Icon,
    label: impl Into<SharedString>,
    selected: bool,
) -> Stateful<Div> {
    // The active tool fills its whole cell, caption included, so it reads
    // at a glance from across the viewport.
    div()
        .id(id)
        // At least the icon's width; longer captions widen the cell rather
        // than wrap.
        .min_w(px(48.))
        .px_1()
        .pb(px(2.))
        .flex()
        .flex_col()
        .items_center()
        .rounded(px(4.))
        .cursor_pointer()
        .when(selected, |d| d.bg(theme::get().selected))
        .when(!selected, |d| d.hover(|s| s.bg(theme::get().wash)))
        // The cell's fill marks the active tool, so the icon sits bare: a
        // button square inside it read as a second, nested highlight.
        // 28px, not icon_button's 32: the caption already makes the target
        // tall, and the panel covers the viewport.
        .child(
            div()
                .h(px(28.))
                .flex()
                .items_center()
                .justify_center()
                .child(icon.render(
                    20.,
                    if selected {
                        theme::get().text
                    } else {
                        theme::get().muted
                    },
                )),
        )
        .child(
            div()
                .text_size(px(11.))
                .text_center()
                .whitespace_nowrap()
                .when(selected, |d| d.font_weight(FontWeight::SEMIBOLD))
                .text_color(if selected {
                    theme::get().text
                } else {
                    theme::get().muted
                })
                .child(label.into()),
        )
}

/// A colour chip, `rgb` as 0xRRGGBB: a 28×18 tile with a light edge.
/// Not round-ended (that reads as a switch) nor square (a checkbox); put
/// the colour's name beside it.
pub fn swatch(rgb_color: u32) -> Div {
    div()
        .flex_shrink_0()
        .w(px(28.))
        .h(px(18.))
        .rounded(px(3.))
        .border_1()
        .border_color(theme::get().floating_border)
        .bg(rgb(rgb_color))
}

/// A colour by hue, saturation and value, each 0 to 1: what a picker drags.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Hsv {
    pub h: f32,
    pub s: f32,
    pub v: f32,
}

impl Hsv {
    pub fn from_rgb([r, g, b]: [u8; 3]) -> Hsv {
        let [r, g, b] = [r, g, b].map(|c| f32::from(c) / 255.);
        let max = r.max(g).max(b);
        let d = max - r.min(g).min(b);
        let h = if d == 0. {
            0.
        } else if max == r {
            ((g - b) / d).rem_euclid(6.) / 6.
        } else if max == g {
            ((b - r) / d + 2.) / 6.
        } else {
            ((r - g) / d + 4.) / 6.
        };
        let s = if max == 0. { 0. } else { d / max };
        Hsv { h, s, v: max }
    }

    pub fn rgb(self) -> [u8; 3] {
        let h = self.h.rem_euclid(1.) * 6.;
        let c = self.v * self.s;
        let x = c * (1. - (h % 2. - 1.).abs());
        let (r, g, b) = match h as u32 {
            0 => (c, x, 0.),
            1 => (x, c, 0.),
            2 => (0., c, x),
            3 => (0., x, c),
            4 => (x, 0., c),
            _ => (c, 0., x),
        };
        let m = self.v - c;
        [r, g, b].map(|t| ((t + m) * 255.).round().clamp(0., 255.) as u8)
    }

    /// As GPUI's `rgb` takes it: 0xRRGGBB.
    pub fn hex(self) -> u32 {
        let [r, g, b] = self.rgb();
        u32::from(r) << 16 | u32::from(g) << 8 | u32::from(b)
    }
}

/// What a colour picker's drag carries: the picker's id, and whether the
/// hue strip (not the square) is dragged.
#[derive(Clone, Debug, PartialEq)]
pub struct ColorDrag(pub SharedString, pub bool);

// The colour moves under the cursor; nothing follows it.
struct NoColorPreview;

impl Render for NoColorPreview {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
    }
}

/// A saturation and value square over a hue strip. A press or a drag on
/// either picks: `on_pick` gets the colour, the hue kept while the square
/// moves and the square while the hue does. The host keeps `hsv` (a grey
/// has no hue of its own to come back to).
pub fn color_picker<V: 'static>(
    id: impl Into<SharedString>,
    hsv: Hsv,
    on_pick: impl Fn(&mut V, Hsv, &mut Window, &mut Context<V>) + 'static,
    cx: &mut Context<V>,
) -> Stateful<Div> {
    use std::{cell::Cell, rc::Rc};
    const W: f32 = 180.;
    let id: SharedString = id.into();
    // Where each part was laid out, for the pointer: the press event
    // carries no element bounds.
    let square: Rc<Cell<Option<Bounds<Pixels>>>> = Rc::default();
    let strip: Rc<Cell<Option<Bounds<Pixels>>>> = Rc::default();
    let pick = {
        let (square, strip) = (square.clone(), strip.clone());
        Rc::new(move |hue: bool, p: Point<Pixels>| -> Option<Hsv> {
            let b = if hue { strip.get() } else { square.get() }?;
            let across = |at: Pixels, from: Pixels, size: Pixels| {
                (f32::from(at - from) / f32::from(size).max(1.)).clamp(0., 1.)
            };
            let x = across(p.x, b.origin.x, b.size.width);
            let y = across(p.y, b.origin.y, b.size.height);
            Some(if hue {
                Hsv { h: x, ..hsv }
            } else {
                Hsv {
                    s: x,
                    v: 1. - y,
                    ..hsv
                }
            })
        })
    };
    let on_pick = Rc::new(on_pick);
    let press = |hue: bool| {
        let (pick, on_pick) = (pick.clone(), on_pick.clone());
        cx.listener(move |this: &mut V, e: &MouseDownEvent, window, cx| {
            cx.stop_propagation();
            if let Some(c) = pick(hue, e.position) {
                on_pick(this, c, window, cx);
            }
        })
    };
    let bounds_of = |cell: Rc<Cell<Option<Bounds<Pixels>>>>| {
        canvas(move |b, _, _| cell.set(Some(b)), |_, _, _, _| {})
            .absolute()
            .size_full()
    };
    let marker = || {
        div()
            .absolute()
            .size(px(10.))
            .ml(px(-5.))
            .mt(px(-5.))
            .rounded_full()
            .border_2()
            .border_color(white())
    };
    let hue = Hsv {
        s: 1.,
        v: 1.,
        ..hsv
    }
    .hex();
    let field = div()
        .id(ElementId::Name(format!("{id}-sv").into()))
        .relative()
        .w(px(W))
        .h(px(120.))
        .rounded(px(3.))
        .overflow_hidden()
        .cursor_crosshair()
        .bg(rgb(hue))
        .child(div().absolute().inset_0().bg(linear_gradient(
            90.,
            linear_color_stop(white(), 0.),
            linear_color_stop(white().opacity(0.), 1.),
        )))
        .child(div().absolute().inset_0().bg(linear_gradient(
            180.,
            linear_color_stop(black().opacity(0.), 0.),
            linear_color_stop(black(), 1.),
        )))
        .child(marker().left(relative(hsv.s)).top(relative(1. - hsv.v)))
        .child(bounds_of(square))
        .on_mouse_down(MouseButton::Left, press(false))
        .on_drag(ColorDrag(id.clone(), false), |_, _, _, cx| {
            cx.new(|_| NoColorPreview)
        });
    // Six hues, red back to red.
    let stops: Vec<u32> = (0..=6)
        .map(|i| {
            Hsv {
                h: i as f32 / 6.,
                s: 1.,
                v: 1.,
            }
            .hex()
        })
        .collect();
    let band = div()
        .id(ElementId::Name(format!("{id}-hue").into()))
        .relative()
        .w(px(W))
        .h(px(12.))
        .rounded(px(3.))
        .overflow_hidden()
        .cursor_ew_resize()
        .flex()
        .children(stops.windows(2).map(|w| {
            div().flex_1().h_full().bg(linear_gradient(
                90.,
                linear_color_stop(rgb(w[0]), 0.),
                linear_color_stop(rgb(w[1]), 1.),
            ))
        }))
        .child(
            div()
                .absolute()
                .top_0()
                .bottom_0()
                .left(relative(hsv.h))
                .ml(px(-1.5))
                .w(px(3.))
                .border_1()
                .border_color(black())
                .bg(white()),
        )
        .child(bounds_of(strip))
        .on_mouse_down(MouseButton::Left, press(true))
        .on_drag(ColorDrag(id.clone(), true), |_, _, _, cx| {
            cx.new(|_| NoColorPreview)
        });
    let drag_id = id.clone();
    div()
        .id(ElementId::Name(id))
        .flex()
        .flex_col()
        .gap(px(6.))
        .child(field)
        .child(band)
        .on_drag_move::<ColorDrag>(cx.listener(
            move |this: &mut V, e: &DragMoveEvent<ColorDrag>, window, cx| {
                let ColorDrag(of, hue) = e.drag(cx).clone();
                if of == drag_id
                    && let Some(c) = pick(hue, e.event.position)
                {
                    on_pick(this, c, window, cx);
                }
            },
        ))
}

/// An on/off switch floating over a canvas or viewport: dark and quiet
/// when off, filled with a lit edge and a ticked box when on, so its state
/// reads without hovering.
pub fn overlay_toggle(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    on: bool,
) -> Stateful<Div> {
    div()
        .id(id)
        .flex()
        .items_center()
        .gap(px(5.))
        .h(px(24.))
        .px_2()
        .rounded(px(4.))
        .border_1()
        .cursor_pointer()
        .text_size(px(11.))
        .border_color(if on {
            theme::get().accent
        } else {
            theme::get().overlay_edge
        })
        .bg(if on {
            theme::get().selected
        } else {
            theme::get().overlay
        })
        .text_color(if on {
            theme::get().text
        } else {
            theme::get().muted
        })
        .hover(|s| s.border_color(theme::get().floating_border))
        // A small check box, not a dot: a dot reads as a status light.
        .child(
            div()
                .size(px(12.))
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(2.))
                .border_1()
                .border_color(if on {
                    theme::get().accent
                } else {
                    theme::get().control_edge
                })
                .when(on, |d| d.bg(theme::get().accent))
                .text_size(px(9.))
                .text_color(theme::get().on_accent)
                .child(if on { "✓" } else { "" }),
        )
        .child(label.into())
}

/// Text over a viewport or canvas, on a dark pill so it reads on any
/// background: hints, coordinates.
pub fn overlay_pill(text: impl Into<SharedString>) -> Div {
    pill().child(text.into())
}

/// The dark pill both overlay pills share.
fn pill() -> Div {
    div()
        .px_2()
        .py(px(2.))
        .rounded(px(4.))
        .bg(theme::get().overlay)
        // A faint edge: over a dark canvas the fill alone did not show.
        .border_1()
        .border_color(theme::get().overlay_edge)
        .text_size(px(10.))
        .text_color(theme::get().section)
}

/// A muted "·" between the items of a line, a space's width each side, as
/// the `" · "` in plain text lines. Put it in a row without a gap: a dot
/// inside the text plus a row gap spaced it unevenly. At 6 px the facts
/// lines it served read in a wider rhythm than every other line.
pub fn middot() -> Div {
    div()
        .flex_shrink_0()
        .mx(px(3.))
        .text_color(theme::get().muted)
        .child("·")
}

/// A line of facts, `"a · b · c"`, with [`middot`] between the parts, so
/// a facts line under a path line spaces its dots the same way. Wraps only
/// between parts.
pub fn meta_line(text: &str) -> Div {
    let mut row = div().flex().flex_wrap().items_center();
    for (n, part) in text.split(" · ").enumerate() {
        if n > 0 {
            row = row.child(middot());
        }
        row = row.child(div().whitespace_nowrap().child(part.to_owned()));
    }
    row
}

/// An [`overlay_pill`] for a viewport's key hints, `"Drag: move · F: frame | Scroll: zoom"`.
/// Wraps only between the " · " separated items, so a narrow view never
/// splits "Scroll: zoom" across lines.
pub fn hint_pill(text: &str) -> Div {
    let mut row = div().flex().flex_wrap();
    // " | " starts a quieter group (the camera keys after a tool's own),
    // so a long hint reads as the task first, then the constants.
    let groups = text.split(" | ").filter(|g| !g.trim().is_empty());
    for (g, group) in groups.enumerate() {
        for (n, item) in group.split(" · ").filter(|s| !s.is_empty()).enumerate() {
            row = row.child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .mr(px(6.))
                    .whitespace_nowrap()
                    // Quieter than the tool's own keys, but readable: at 0.55
                    // the muted words fell to about 3:1 on the pill.
                    .when(g > 0, |d| d.opacity(0.75))
                    // A rule, not a wider space, where the camera keys
                    // start: the space read as a dropped dot.
                    .when(g > 0 && n == 0, |d| {
                        d.ml(px(2.)).child(
                            div()
                                .w(px(1.))
                                .h(px(10.))
                                .mr(px(2.))
                                .bg(theme::get().border),
                        )
                    })
                    .when(n > 0, |d| {
                        d.child(div().text_color(theme::get().muted).child("·"))
                    })
                    // The input reads first and the action after it, as in
                    // Blender's status bar; one grey sentence did not scan.
                    .child(match item.split_once(": ") {
                        Some((key, action)) => div()
                            .flex()
                            .gap(px(4.))
                            .child(
                                div()
                                    .text_color(theme::get().text)
                                    .font_weight(FontWeight::MEDIUM)
                                    .child(key.to_owned()),
                            )
                            .child(
                                div()
                                    .text_color(theme::get().muted)
                                    .child(action.to_owned()),
                            ),
                        None => div().child(item.to_owned()),
                    }),
            );
        }
    }
    // Not overlay_pill(""): its empty text took a line of its own.
    pill().child(row)
}

/// A small muted icon button for pane headers and list rows.
pub fn icon_action(id: impl Into<ElementId>, icon: Icon) -> Stateful<Div> {
    div()
        .id(id)
        .size(px(22.))
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(3.))
        .cursor_pointer()
        .hover(|s| s.bg(theme::get().hover))
        .child(
            div()
                .size(px(14.))
                .child(icon.render(14., theme::get().muted)),
        )
}

/// A row's show/hide eye for outliners. Put `.group("row")` on the row
/// and pass the same name: a shown row's eye is dimmed until hover, a
/// hidden row's stays lit, so the column is findable but quiet and hidden
/// rows stand out.
pub fn visibility_toggle(
    id: impl Into<ElementId>,
    shown: bool,
    group: &'static str,
) -> Stateful<Div> {
    div()
        .id(id)
        .flex_shrink_0()
        .size(px(22.))
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(3.))
        .cursor_pointer()
        .hover(|s| s.bg(theme::get().hover))
        // Faint at rest, full on the row's hover and when hidden. At 60% the
        // column was noise; hover-only (Figma) could not be found at all.
        .when(shown, |d| {
            d.opacity(0.35).group_hover(group, |s| s.opacity(1.))
        })
        .child(div().size(px(13.)).child(if shown {
            Icon::EYE.render(13., theme::get().muted)
        } else {
            // Solid and slashed sets it apart; amber would read as a warning.
            Icon::EYE_OFF.render(13., theme::get().text)
        }))
}

/// A square tool button for floating viewport toolbars.
pub fn icon_button(id: impl Into<ElementId>, icon: Icon, selected: bool) -> Stateful<Div> {
    div()
        .id(id)
        .size(px(32.))
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(5.))
        .cursor_pointer()
        .border_1()
        .when(selected, |d| {
            d.bg(theme::get().selected)
                .border_color(theme::get().accent.alpha(0.6))
        })
        .hover(|s| s.bg(theme::get().hover))
        .child(icon.render(
            21.,
            if selected {
                theme::get().text
            } else {
                theme::get().muted
            },
        ))
}

/// Tool tips with a title, a shortcut and one line of detail.
/// Use as `.tooltip(Tooltip::text("Move", "W", "Drag handles to move"))`.
pub struct Tooltip {
    pub label: SharedString,
    pub shortcut: SharedString,
    pub detail: SharedString,
}

impl Tooltip {
    pub fn text(
        label: impl Into<SharedString>,
        shortcut: impl Into<SharedString>,
        detail: impl Into<SharedString>,
    ) -> impl Fn(&mut Window, &mut App) -> AnyView + 'static {
        let (label, shortcut, detail) = (label.into(), shortcut.into(), detail.into());
        move |_, cx| {
            cx.new(|_| Tooltip {
                label: label.clone(),
                shortcut: shortcut.clone(),
                detail: detail.clone(),
            })
            .into()
        }
    }
}

impl Render for Tooltip {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .max_w(px(290.))
            .p_3()
            .rounded(px(7.))
            .bg(theme::get().tooltip)
            .border_1()
            .border_color(theme::get().floating_border)
            .shadow_lg()
            .text_color(theme::get().text)
            .text_size(px(11.))
            // No heading line when there is no label or key: an empty one
            // left a blank band over the detail.
            .when(!self.label.is_empty() || !self.shortcut.is_empty(), |d| {
                d.child(
                    div()
                        .flex()
                        .justify_between()
                        .gap_4()
                        .mb_1()
                        .child(
                            div()
                                .font_weight(FontWeight::SEMIBOLD)
                                .child(self.label.clone()),
                        )
                        .child(
                            div()
                                .text_color(theme::get().accent)
                                .child(self.shortcut.clone()),
                        ),
                )
            })
            .when(!self.detail.is_empty(), |d| {
                d.child(
                    div()
                        .text_color(theme::get().muted)
                        .child(self.detail.clone()),
                )
            })
    }
}

/// The floating preview while dragging a tab or divider.
pub struct DragLabel(pub SharedString);

impl Render for DragLabel {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .px_3()
            .py_2()
            .bg(theme::get().panel)
            .border_1()
            .border_color(theme::get().accent)
            .text_color(theme::get().text)
            .child(self.0.clone())
    }
}

/// The root of an editor window: full size, base font and colors.
pub fn window_root(id: impl Into<ElementId>) -> Stateful<Div> {
    div()
        .id(id)
        .relative()
        .size_full()
        .flex()
        .flex_col()
        .bg(theme::get().bg)
        .text_color(theme::get().text)
        .font_family("Noto Sans")
        .text_size(px(12.))
}

pub fn panel_header(label: impl AsRef<str>) -> Div {
    div()
        .h(px(27.))
        .flex_shrink_0()
        .flex()
        .items_center()
        .px_2()
        .bg(theme::get().header)
        .border_b_1()
        .border_color(theme::get().border)
        .text_size(px(11.))
        .font_weight(FontWeight::SEMIBOLD)
        .child(label.as_ref().to_owned())
}

/// A fixed-width side panel. `left` puts the border on the right edge.
pub fn side_panel(width: f32, left: bool) -> Div {
    div()
        .w(px(width))
        .flex_shrink_0()
        .flex()
        .flex_col()
        .min_h_0()
        .bg(theme::get().panel)
        .border_color(theme::get().border)
        .when(left, |d| d.border_r_1())
        .when(!left, |d| d.border_l_1())
}

/// A static group heading. For one that folds, use [`collapsible`].
pub fn section(label: &str) -> Div {
    // The same ruled 28px row as `collapsible`, so a panel mixing fixed and
    // folding groups reads as one stack of sections.
    div()
        .mt_1()
        .h(px(28.))
        .flex()
        .items_center()
        .border_t_1()
        .border_color(theme::get().border)
        .text_size(px(11.))
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(theme::get().section)
        .child(label.to_owned())
}

/// A group heading that folds its group: a click on it toggles. Returns the
/// heading and whether the group is open; the host shows the group's rows
/// only when open. The state lives in GPUI keyed element state under `id`,
/// starting at `open`.
pub fn collapsible(
    id: impl Into<SharedString>,
    label: impl Into<SharedString>,
    open: bool,
    window: &mut Window,
    cx: &mut App,
) -> (Stateful<Div>, bool) {
    let id: SharedString = id.into();
    let state = window.use_keyed_state(ElementId::Name(id.clone()), cx, |_, _| Flag(open));
    let is_open = state.read(cx).0;
    let heading = div()
        .id(ElementId::Name(format!("{id}-heading").into()))
        .flex()
        .items_center()
        .gap_1()
        // A 28px row under a rule, as property panels stack sections: with
        // a 12px gap above each, collapsed headings read loose and
        // unfinished.
        .mt_1()
        .h(px(28.))
        .border_t_1()
        .border_color(theme::get().border)
        .cursor_pointer()
        .text_size(px(11.))
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(theme::get().section)
        .hover(|s| s.text_color(theme::get().text))
        .child(
            // Fixed: a long heading row shrank it, and that heading's text
            // then started left of the ones around it.
            div()
                .w(px(10.))
                .flex_none()
                .text_color(theme::get().muted)
                .child(if is_open { "▾" } else { "▸" }),
        )
        .child(label.into())
        .on_click(move |_, _, cx| {
            state.update(cx, |f, cx| {
                f.0 = !f.0;
                cx.notify();
            })
        });
    (heading, is_open)
}

/// A [`collapsible`] for inside a line of text: "Show files ▸" / "Hide
/// files ▾" in the link colour, no rule or heading weight. As a section
/// heading in a sentence it drew a rule over itself and sat below the
/// text's baseline. Same keyed state as `collapsible`.
pub fn disclosure_link(
    id: impl Into<SharedString>,
    closed: impl Into<SharedString>,
    opened: impl Into<SharedString>,
    window: &mut Window,
    cx: &mut App,
) -> (Stateful<Div>, bool) {
    let id: SharedString = id.into();
    let state = window.use_keyed_state(ElementId::Name(id.clone()), cx, |_, _| Flag(false));
    let is_open = state.read(cx).0;
    let link = div()
        .id(ElementId::Name(format!("{id}-link").into()))
        .flex_shrink_0()
        .cursor_pointer()
        .text_color(theme::get().accent)
        .hover(|s| s.underline())
        .child(if is_open {
            format!("{} ▾", opened.into())
        } else {
            format!("{} ▸", closed.into())
        })
        .on_click(move |_, _, cx| {
            state.update(cx, |f, cx| {
                f.0 = !f.0;
                cx.notify();
            })
        });
    (link, is_open)
}

/// A read-only label and value. The label column is 96 px and
/// the value starts right after it, so values line up under each other
/// instead of drifting to the far edge; long values wrap.
/// [`info_row`] with an element as the value: a swatch, a badge, a link.
pub fn info_row_with(label: impl AsRef<str>, value: impl IntoElement) -> Div {
    div()
        .flex()
        .items_center()
        .gap_2()
        .min_h(px(24.))
        .text_size(px(11.))
        .child(
            div()
                .w(px(96.))
                .flex_shrink_0()
                .text_color(theme::get().muted)
                .child(label.as_ref().to_owned()),
        )
        .child(div().flex_1().min_w_0().child(value))
}

pub fn info_row(label: impl AsRef<str>, value: impl AsRef<str>) -> Div {
    div()
        .flex()
        .items_center()
        .gap_2()
        .min_h(px(24.))
        .text_size(px(11.))
        .child(
            div()
                .w(px(96.))
                .flex_shrink_0()
                .text_color(theme::get().muted)
                .child(label.as_ref().to_owned()),
        )
        // Sentences wrap; a single token (a file name) ends in an ellipsis
        // rather than breaking mid-name.
        .child({
            let value = value.as_ref().to_owned();
            let token = !value.contains(' ');
            div()
                .flex_1()
                .min_w_0()
                .when(token, |d| {
                    d.overflow_hidden().whitespace_nowrap().text_ellipsis()
                })
                .child(value)
        })
}

/// Byte ranges of `text` that a search `query` matched: each
/// whitespace-separated word, ASCII case-insensitive, merged where they
/// touch. The same rule the editor's search filters use, so what a row was
/// kept for is what lights up.
pub fn match_ranges(text: &str, query: &str) -> Vec<std::ops::Range<usize>> {
    // ASCII folding keeps byte offsets intact; full Unicode lowercasing can
    // change a character's length and shift every range after it.
    let hay = text.to_ascii_lowercase();
    let mut found: Vec<std::ops::Range<usize>> = Vec::new();
    for word in query.split_whitespace() {
        let word = word.to_ascii_lowercase();
        let mut from = 0;
        while let Some(at) = hay[from..].find(&word) {
            let start = from + at;
            found.push(start..start + word.len());
            from = start + word.len();
        }
    }
    found.sort_by_key(|r| r.start);
    let mut merged: Vec<std::ops::Range<usize>> = Vec::with_capacity(found.len());
    for r in found {
        match merged.last_mut() {
            Some(last) if r.start <= last.end => last.end = last.end.max(r.end),
            _ => merged.push(r),
        }
    }
    merged
}

/// `text` with what `query` matched marked: amber text over an amber
/// underline, white on a `selected` row so the selection blue stays clean.
/// A wash behind the match (Unreal's style) read as a text selection, mixed
/// with the selection blue into a grey block, and broke wrapped tile labels
/// into patches. Colour and line only, no weight change, so a marked name
/// keeps the width and cut of an unmarked one.
pub fn matched_text(text: impl Into<SharedString>, query: &str, selected: bool) -> AnyElement {
    let text = text.into();
    let ranges = match_ranges(&text, query);
    if ranges.is_empty() {
        return text.into_any_element();
    }
    let style = HighlightStyle {
        color: Some(
            if selected {
                theme::get().text
            } else {
                theme::get().highlight
            }
            .into(),
        ),
        underline: Some(UnderlineStyle {
            thickness: px(1.),
            color: Some(theme::get().highlight.into()),
            wavy: false,
        }),
        ..Default::default()
    };
    StyledText::new(text)
        .with_highlights(ranges.into_iter().map(|r| (r, style)))
        .into_any_element()
}

/// Expansion state of a tree row's disclosure arrow.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Disclosure {
    Leaf,
    Collapsed,
    Expanded,
}

/// One fixed-height row of a flattened tree, for `uniform_list`. Hosts keep
/// the tree flattened to visible rows; rows stay 22 px so the list can
/// virtualize hundreds of thousands of entries. `detail` is right-aligned
/// and muted (sizes, counts, origin).
pub fn tree_row(
    id: impl Into<ElementId>,
    depth: usize,
    disclosure: Disclosure,
    label: impl Into<SharedString>,
    detail: impl Into<SharedString>,
    selected: bool,
) -> Stateful<Div> {
    tree_row_with(
        id,
        depth,
        disclosure_arrow(disclosure),
        None,
        None,
        label,
        detail,
        selected,
        TREE_ROW,
        false,
        None,
        None,
        None,
        None,
        "",
    )
}

pub(crate) fn disclosure_arrow(disclosure: Disclosure) -> Div {
    div()
        .w(px(10.))
        .flex_shrink_0()
        .text_color(theme::get().muted)
        .child(match disclosure {
            Disclosure::Leaf => "",
            Disclosure::Collapsed => "▸",
            Disclosure::Expanded => "▾",
        })
}

/// `tree_row` with a caller-built arrow, so the tree list can give it its own
/// click target.
/// Height of a [`tree_row`]; [`crate::tree::List::row_height`] raises it
/// for lists whose pictures need room.
pub const TREE_ROW: f32 = 22.;

pub(crate) fn tree_row_with(
    id: impl Into<ElementId>,
    depth: usize,
    arrow: impl IntoElement,
    icon: Option<std::sync::Arc<RenderImage>>,
    glyph: Option<Icon>,
    label: impl Into<SharedString>,
    detail: impl Into<SharedString>,
    selected: bool,
    height: f32,
    heading: bool,
    // Muted text after the label, and its share of the row's width.
    note: Option<(SharedString, f32)>,
    // Clicks on a tab-separated detail's columns, by column index.
    cell_click: Option<std::rc::Rc<dyn Fn(usize, &mut App)>>,
    // Widths of a tab-separated detail's columns, else DETAIL_COLUMN each.
    columns: Option<std::rc::Rc<[f32]>>,
    // The picture's tile colour, else the neutral grey.
    icon_bg: Option<Rgba>,
    // The list's search, marked in the label.
    query: &str,
) -> Stateful<Div> {
    // In a list with pictures, glyph rows take the picture's slot too, so
    // names start at one x whichever a row has.
    let slot = if height > TREE_ROW { height - 4. } else { 14. };
    let has_note = note.is_some();
    div()
        .id(id)
        .flex()
        .items_center()
        .h(px(height))
        .w_full()
        .pl(px(6. + depth as f32 * 14.))
        // Clear of the overlay scrollbar, so right-aligned details stay
        // readable on a scrolled list.
        .pr(px(16.))
        .gap_1()
        .text_size(px(11.))
        .cursor_pointer()
        // Hover must not hide which row is selected. An accent edge on
        // the left as well: in a long table of near-identical rows the
        // fill alone was missed.
        .relative()
        .when(selected, |d| {
            d.bg(theme::get().selected).child(
                div()
                    .absolute()
                    .left_0()
                    .top_0()
                    .bottom_0()
                    .w(px(2.))
                    .bg(theme::get().accent),
            )
        })
        .when(!selected, |d| d.hover(|s| s.bg(theme::get().hover)))
        .child(arrow)
        .when_some(icon, |d, icon| {
            // The picture fills the row less two pixels above and below, on
            // a lighter, framed tile: edge to edge, a column of textures ran
            // into one strip.
            d.child(
                div()
                    .size(px(height - 4.))
                    .flex_shrink_0()
                    .mr_1()
                    .rounded(px(3.))
                    .border_1()
                    .border_color(theme::get().border)
                    .overflow_hidden()
                    // The thumbnail cards' grey: dark stumps and cliffs
                    // lost their outline on the old, darker one.
                    .bg(icon_bg.unwrap_or(theme::get().picture_bg))
                    .child(img(icon).size_full()),
            )
        })
        .when_some(glyph, |d, glyph| {
            d.child(
                div()
                    .w(px(slot))
                    .h(px(14.))
                    .flex()
                    .justify_center()
                    .flex_shrink_0()
                    .mr_1()
                    .child(glyph.render(14., theme::get().muted)),
            )
        })
        .child(
            div()
                .flex_1()
                .min_w_0()
                .overflow_hidden()
                .whitespace_nowrap()
                // A gutter before a note, so a long name is cut rather than
                // running up to the folder and reading as one string.
                .when(has_note, |d| d.pr_3())
                // Cut in the middle: siblings ("Mountain slot 1", "… slot 2",
                // "Dropzone_PinMaterial_Remove") differ at the end.
                .text_ellipsis_middle()
                // A heading row (a size group) reads as a caption, not as
                // one more entry.
                .when(heading, |d| {
                    d.text_size(px(10.))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(theme::get().muted)
                })
                .when(!heading, |d| d.text_color(theme::get().text))
                .child(matched_text(label, query, selected)),
        )
        // Where the entry lives (a file's folder), muted after its name and
        // cut from the start: the nearest folder is the telling part. A fixed
        // share of the row, so every row of one folder cuts it alike and the
        // notes form a column.
        .when_some(note, |d, (note, share)| {
            d.child(
                // A gutter before the detail column, so a long note does not
                // run into "704×512": the cut text sits in an inner box, as
                // text clipped at the outer one ran over its own padding.
                div().flex_none().w(relative(share)).pr_4().min_w_0().child(
                    div()
                        .w_full()
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .text_ellipsis_start()
                        .text_color(theme::get().muted)
                        .child(note),
                ),
            )
        })
        // The detail gives way before the name: at most 60% of the row
        // (an aligned x/y/z position needs that beside a short "#518"),
        // cut with an ellipsis. 8px clear of a cut name, and a 28px
        // right-aligned floor so short counts line up as a column.
        .child({
            let detail: SharedString = detail.into();
            // Beside a note the detail is a fixed column ("◆ 704×512 · BC7"),
            // so a wider one does not push that row's note out of line.
            let cell = div()
                // A caption row's columns in its label's weight and size;
                // a plain count on a heading row stays quiet, or a size
                // group's count outshone its region's.
                .when(heading && detail.contains('\t'), |d| {
                    d.text_size(px(10.)).font_weight(FontWeight::SEMIBOLD)
                })
                .ml_1()
                .when(has_note, |d| {
                    // Set columns and their gutters, else one 84 px cell.
                    let w = columns
                        .as_deref()
                        .filter(|_| detail.contains('\t'))
                        .map_or(84., |c| c.iter().sum::<f32>() + 8. * (c.len() as f32 - 1.));
                    d.w(px(w)).flex_none()
                })
                .min_w(px(28.))
                .max_w(relative(0.6))
                .text_right()
                .truncate()
                .text_color(theme::get().muted);
            // Tabs split the detail into fixed right-aligned cells, so
            // values line up under a caption row's letters: padding with
            // figure spaces drifted, a "." being narrower than a digit.
            if detail.contains('\t') {
                // A gutter between columns: right-aligned neighbours such as
                // "102.3" and "0.1" read as one number without it.
                cell.flex()
                    .flex_none()
                    .gap(px(8.))
                    .children(detail.split('\t').enumerate().map(|(k, v)| {
                        let cell = div()
                            .w(px(columns
                                .as_deref()
                                .and_then(|c| c.get(k).copied())
                                .unwrap_or(DETAIL_COLUMN)))
                            .flex_none()
                            .text_right()
                            .child(SharedString::from(v.to_owned()));
                        match &cell_click {
                            Some(click) => {
                                let click = click.clone();
                                cell.id(("detail-cell", k))
                                    .cursor_pointer()
                                    .hover(|s| s.text_color(theme::get().text))
                                    .on_click(move |_, _, cx| {
                                        cx.stop_propagation();
                                        click(k, cx);
                                    })
                                    .into_any_element()
                            }
                            None => cell.into_any_element(),
                        }
                    }))
            } else {
                cell.child(detail)
            }
        })
}

/// Width of one tab-separated detail column in a tree row: "-105.4" at
/// 11px; the 8 px gutter between columns is separate, and three of them
/// must fit the 60% a detail may take beside a short "#518".
pub const DETAIL_COLUMN: f32 = 38.;

/// A labeled checkbox row. `detail` is right-aligned and muted (counts).
/// The label doubles as the element id.
pub fn checkbox(
    label: impl Into<SharedString>,
    checked: bool,
    detail: impl Into<SharedString>,
) -> Stateful<Div> {
    let label = label.into();
    div()
        .id(ElementId::Name(format!("check-{label}").into()))
        .flex()
        .items_center()
        .gap_2()
        .h(px(24.))
        .text_size(px(11.))
        .cursor_pointer()
        .hover(|s| s.bg(theme::get().hover))
        .child(
            div()
                .size(px(14.))
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(2.))
                .border_1()
                // Unchecked boxes need a lighter edge than panel borders,
                // or they vanish on the panel.
                .border_color(if checked {
                    theme::get().accent
                } else {
                    theme::get().control_edge
                })
                .bg(if checked {
                    theme::get().selected
                } else {
                    theme::get().input
                })
                .text_size(px(10.))
                .text_color(theme::get().text)
                .child(if checked { "✓" } else { "" }),
        )
        .child(div().flex_1().child(label))
        .child(
            div()
                .text_size(px(11.))
                .text_color(theme::get().muted)
                .child(detail.into()),
        )
}

/// A raised group on a page (settings, a list): rounded, bordered, padded.
/// Start it with [`card_title`].
pub fn card() -> Div {
    div()
        .w_full()
        .flex()
        .flex_col()
        .gap_2()
        .p_4()
        .rounded(px(6.))
        .border_1()
        .border_color(theme::get().border)
        .bg(theme::get().card)
}

/// A card's heading, with `trailing` controls (buttons, a count) at the right.
pub fn card_title(title: impl Into<SharedString>, trailing: impl IntoElement) -> Div {
    div()
        .flex()
        .items_center()
        // As tall as an action button, so titles line up across cards
        // whether or not they carry one.
        .min_h(px(28.))
        .gap_2()
        .child(
            div()
                .flex_1()
                .text_size(px(13.))
                .font_weight(FontWeight::SEMIBOLD)
                .child(title.into()),
        )
        .child(trailing)
}

/// Muted help or detail text under a control. Wraps.
pub fn hint(text: impl Into<SharedString>) -> Div {
    div()
        .text_size(px(11.))
        .text_color(theme::get().muted)
        .child(text.into())
}

/// A small status pill over a card or after a name: "installed", "missing",
/// "✓ set". `color` is its fill; the text is dark on it.
pub fn badge(text: impl Into<SharedString>, color: Rgba) -> Div {
    div()
        .flex_shrink_0()
        .px_1()
        .rounded(px(3.))
        .bg(color)
        .text_size(px(10.))
        .text_color(theme::get().bg)
        .child(text.into())
}

/// A quiet chip over a thumbnail ("4 variants"): dark and translucent, so
/// the picture stays the loudest thing on the card.
pub fn overlay_badge(text: impl Into<SharedString>) -> Div {
    div()
        .flex_shrink_0()
        .px_1()
        .rounded(px(3.))
        .bg(theme::get().overlay)
        .border_1()
        .border_color(theme::get().overlay_edge)
        .text_size(px(10.))
        .text_color(theme::get().text)
        .child(text.into())
}

/// A centered message for a pane with nothing to show yet: `text` is the
/// heading, hints follow as children, then a row of buttons (with `.pt_2()`)
/// to offer the way out ("Open a project").
pub fn empty_state(text: impl Into<SharedString>) -> Div {
    empty(None, text.into())
}

/// [`empty_state`] with a large muted `icon` above the heading, for a whole
/// workspace pane: text alone in a big empty pane read as unfinished.
pub fn empty_state_icon(icon: Icon, text: impl Into<SharedString>) -> Div {
    empty(Some(icon), text.into())
}

fn empty(icon: Option<Icon>, text: SharedString) -> Div {
    div()
        .size_full()
        .flex()
        .flex_col()
        // Lines close, as one paragraph; give an actions row `.pt_2()`.
        .gap_1()
        .items_center()
        .justify_center()
        .bg(theme::get().panel)
        .text_color(theme::get().muted)
        .text_size(px(12.))
        .when_some(icon, |d, icon| {
            d.child(div().pb_2().child(icon.render(40., theme::get().disabled)))
        })
        // The first line is the heading: what is missing, in the body's
        // colour so it does not read as a stray status line. Children
        // (hints, then actions) follow muted.
        .child(
            div()
                .max_w(px(520.))
                .px_4()
                .pb_1()
                .text_center()
                .text_size(px(15.))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(theme::get().text)
                .child(text),
        )
}

/// Width of [`field_row`]'s label column and the gap after it, so text can
/// line up with the controls ([`field_note`]).
const FIELD_LABEL: f32 = 110.;

/// Help under a [`field_row`]'s control, lined up with it rather than with
/// the labels.
pub fn field_note(text: impl Into<SharedString>) -> Div {
    div().pl(px(FIELD_LABEL + 8.)).child(hint(text))
}

/// A form row: a fixed-width muted label, then the control filling the rest.
pub fn field_row(label: impl Into<SharedString>, control: impl IntoElement) -> Div {
    // The label lines up with the control's first line, so a control with
    // a note or a second row under it keeps its label beside the control,
    // not centred on the stack.
    div()
        .flex()
        .items_start()
        .gap_2()
        .min_h(px(28.))
        .child(
            div()
                .w(px(FIELD_LABEL))
                .h(px(28.))
                .flex()
                .items_center()
                .flex_shrink_0()
                .text_size(px(11.))
                .text_color(theme::get().muted)
                .child(label.into()),
        )
        .child(div().flex_1().min_w_0().child(control))
}

/// One choice out of a few, as joined buttons: region, size class, list tab.
/// For long or open-ended lists use [`dropdown`]. `id` scopes the buttons,
/// which take their label as id.
pub fn segmented<V: 'static, T: Clone + PartialEq + 'static>(
    id: impl Into<ElementId>,
    options: impl IntoIterator<Item = (T, SharedString)>,
    selected: &T,
    cx: &mut Context<V>,
    on: impl Fn(&mut V, T, &mut Window, &mut Context<V>) + 'static,
) -> Stateful<Div> {
    segmented_where(id, options, selected, |_| None, cx, on)
}

/// A [`segmented`] whose options can be off for now: `off` gives the
/// reason, shown as the dimmed option's tooltip. Kept in place rather
/// than hidden, so a toolbar holds its shape as choices come and go.
pub fn segmented_where<V: 'static, T: Clone + PartialEq + 'static>(
    id: impl Into<ElementId>,
    options: impl IntoIterator<Item = (T, SharedString)>,
    selected: &T,
    off: impl Fn(&T) -> Option<SharedString>,
    cx: &mut Context<V>,
    on: impl Fn(&mut V, T, &mut Window, &mut Context<V>) + 'static,
) -> Stateful<Div> {
    let on = std::rc::Rc::new(on);
    // One line always: a wrapped choice reads as two controls.
    let mut row = div()
        .flex()
        .flex_shrink_0()
        .gap_px()
        .p_px()
        .rounded(px(3.))
        .bg(theme::get().border);
    for (value, label) in options {
        let on = on.clone();
        let current = value == *selected;
        let why = off(&value);
        let b = button(label.clone(), current)
            .h(px(24.))
            .text_size(px(11.))
            .when(!current, |b| b.bg(theme::get().input));
        row = row.child(match why {
            // Faded as a whole, not only its text: with its frame and fill
            // at full strength an off choice read as one more live one.
            Some(why) => b
                .text_color(theme::get().disabled)
                .opacity(0.55)
                .cursor_default()
                .tooltip(Tooltip::text(label, "", why)),
            None => b.on_click(cx.listener(move |this, _, window, cx| {
                on(this, value.clone(), window, cx);
                cx.notify();
            })),
        });
    }
    // In a row of its own, so it is as wide as its buttons, not its slot.
    div().id(id).flex().child(row)
}

/// [`segmented_where`] drawn as icons: for a view switch that sits beside
/// a search field, where words would take the width the field needs. Each
/// icon names itself on hover; a dimmed one gives `off`'s reason instead.
pub fn segmented_icons<V: 'static, T: Clone + PartialEq + 'static>(
    id: impl Into<ElementId>,
    options: impl IntoIterator<Item = (T, Icon, &'static str)>,
    selected: &T,
    off: impl Fn(&T) -> Option<SharedString>,
    cx: &mut Context<V>,
    on: impl Fn(&mut V, T, &mut Window, &mut Context<V>) + 'static,
) -> Stateful<Div> {
    let on = std::rc::Rc::new(on);
    let mut row = div()
        .flex()
        .flex_shrink_0()
        .gap_px()
        .p_px()
        .rounded(px(3.))
        .bg(theme::get().border);
    for (value, icon, label) in options {
        let on = on.clone();
        let current = value == *selected;
        let why = off(&value);
        let color = match (&why, current) {
            (Some(_), _) => theme::get().disabled,
            (None, true) => theme::get().text,
            (None, false) => theme::get().muted,
        };
        let b = div()
            .id(label)
            .flex()
            .items_center()
            .justify_center()
            .w(px(28.))
            .h(px(24.))
            .rounded(px(2.))
            .bg(if current {
                theme::get().selected
            } else {
                theme::get().input
            })
            .child(icon.render(15., color));
        row = row.child(match why {
            Some(why) => b.tooltip(Tooltip::text(label, "", why)),
            None => b
                .cursor_pointer()
                .when(!current, |b| b.hover(|s| s.bg(theme::get().hover)))
                .tooltip(Tooltip::text(label, "", ""))
                .on_click(cx.listener(move |this, _, window, cx| {
                    on(this, value.clone(), window, cx);
                    cx.notify();
                })),
        });
    }
    div().id(id).flex().child(row)
}

/// [`segmented`] whose choices carry a shortcut, drawn as a dim [`keycap`]
/// after the label (`Move [W]`), as tool buttons and the inspector show
/// keys, rather than a second word.
pub fn segmented_keys<V: 'static, T: Clone + PartialEq + 'static>(
    id: impl Into<ElementId>,
    options: impl IntoIterator<Item = (T, SharedString, &'static str)>,
    selected: &T,
    cx: &mut Context<V>,
    on: impl Fn(&mut V, T, &mut Window, &mut Context<V>) + 'static,
) -> Stateful<Div> {
    let on = std::rc::Rc::new(on);
    let mut row = div()
        .flex()
        .flex_shrink_0()
        .gap_px()
        .p_px()
        .rounded(px(3.))
        .bg(theme::get().border);
    for (value, label, key) in options {
        let on = on.clone();
        let current = value == *selected;
        row = row.child(
            button(label, current)
                .h(px(24.))
                .gap(px(6.))
                .text_size(px(11.))
                .when(!current, |b| b.bg(theme::get().input))
                .when(!key.is_empty(), |b| b.child(keycap(key).opacity(0.75)))
                .on_click(cx.listener(move |this, _, window, cx| {
                    on(this, value.clone(), window, cx);
                    cx.notify();
                })),
        );
    }
    div().id(id).flex().child(row)
}

/// [`segmented`] whose choices explain themselves on hover: for choices
/// named by a game's own terms (Props, Decorations, Objects).
pub fn segmented_tips<V: 'static, T: Clone + PartialEq + 'static>(
    id: impl Into<ElementId>,
    options: impl IntoIterator<Item = (T, SharedString, &'static str)>,
    selected: &T,
    cx: &mut Context<V>,
    on: impl Fn(&mut V, T, &mut Window, &mut Context<V>) + 'static,
) -> Stateful<Div> {
    let on = std::rc::Rc::new(on);
    let mut row = div()
        .flex()
        .flex_shrink_0()
        .gap_px()
        .p_px()
        .rounded(px(3.))
        .bg(theme::get().border);
    for (value, label, tip) in options {
        let on = on.clone();
        let current = value == *selected;
        row = row.child(
            button(label.clone(), current)
                .h(px(24.))
                .text_size(px(11.))
                .when(!current, |b| b.bg(theme::get().input))
                // Not on the chosen one: it explains the others, and over a
                // just-clicked tab it covered the pane under it.
                .when(!tip.is_empty() && !current, |b| {
                    b.tooltip(Tooltip::text(label, "", tip))
                })
                .on_click(cx.listener(move |this, _, window, cx| {
                    on(this, value.clone(), window, cx);
                    cx.notify();
                })),
        );
    }
    div().id(id).flex().child(row)
}

/// Open state of a [`dropdown`] or arming of a [`confirm_button`]; kept by
/// GPUI per element id, so hosts carry no field for it.
struct Flag(bool);

struct Placed(Option<Bounds<Pixels>>);

/// A button showing the current choice; a click opens the list below it,
/// a pick calls `on` and closes it, a click elsewhere closes it. For lists
/// too long for [`segmented`]: presets, sets, categories.
pub fn dropdown<V: 'static, T: Clone + PartialEq + 'static>(
    id: impl Into<SharedString>,
    options: Vec<(T, SharedString)>,
    selected: &T,
    window: &mut Window,
    cx: &mut Context<V>,
    on: impl Fn(&mut V, T, &mut Window, &mut Context<V>) + 'static,
) -> Stateful<Div> {
    let options = options.into_iter().map(|(v, l)| (v, l, None)).collect();
    dropdown_detailed(id, options, None, selected, window, cx, on)
}

/// A [`dropdown`] whose options may carry a muted second line (what a
/// preset contains, so similar names can be told apart before picking),
/// and whose open list may start with a note (a side effect of picking).
pub fn dropdown_detailed<V: 'static, T: Clone + PartialEq + 'static>(
    id: impl Into<SharedString>,
    options: Vec<(T, SharedString, Option<SharedString>)>,
    note: Option<SharedString>,
    selected: &T,
    window: &mut Window,
    cx: &mut Context<V>,
    on: impl Fn(&mut V, T, &mut Window, &mut Context<V>) + 'static,
) -> Stateful<Div> {
    let options = options
        .into_iter()
        .map(|(v, l, d)| (v, l, d, Vec::new()))
        .collect();
    dropdown_impl(id, None, options, note, selected, window, cx, on)
}

/// A [`dropdown_detailed`] whose options carry small pictures before their
/// label (up to three, 28 px): for choices made by how they look, such as a
/// ground cover preset by its plants, where names and counts alone needed
/// each one tried.
pub fn dropdown_pictured<V: 'static, T: Clone + PartialEq + 'static>(
    id: impl Into<SharedString>,
    options: Vec<(
        T,
        SharedString,
        Option<SharedString>,
        Vec<std::sync::Arc<RenderImage>>,
    )>,
    note: Option<SharedString>,
    selected: &T,
    window: &mut Window,
    cx: &mut Context<V>,
    on: impl Fn(&mut V, T, &mut Window, &mut Context<V>) + 'static,
) -> Stateful<Div> {
    dropdown_impl(id, None, options, note, selected, window, cx, on)
}

/// A [`dropdown`] shaped as a tool bar's compact field: 28 px tall with
/// its `label` inside, as `field::slider_field` is, so a row mixing
/// sliders and a choice reads as one strip.
pub fn field_dropdown<V: 'static, T: Clone + PartialEq + 'static>(
    id: impl Into<SharedString>,
    label: &'static str,
    options: Vec<(T, SharedString)>,
    selected: &T,
    window: &mut Window,
    cx: &mut Context<V>,
    on: impl Fn(&mut V, T, &mut Window, &mut Context<V>) + 'static,
) -> Stateful<Div> {
    let options = options
        .into_iter()
        .map(|(v, l)| (v, l, None, Vec::new()))
        .collect();
    dropdown_impl(id, Some(label), options, None, selected, window, cx, on)
}

#[allow(clippy::too_many_arguments)]
fn dropdown_impl<V: 'static, T: Clone + PartialEq + 'static>(
    id: impl Into<SharedString>,
    inline_label: Option<&'static str>,
    options: Vec<(
        T,
        SharedString,
        Option<SharedString>,
        Vec<std::sync::Arc<RenderImage>>,
    )>,
    note: Option<SharedString>,
    selected: &T,
    window: &mut Window,
    cx: &mut Context<V>,
    on: impl Fn(&mut V, T, &mut Window, &mut Context<V>) + 'static,
) -> Stateful<Div> {
    let id: SharedString = id.into();
    let open = window.use_keyed_state(ElementId::Name(id.clone()), cx, |_, _| Flag(false));
    let is_open = open.read(cx).0;
    // Where the trigger was last frame: the list takes its width and opens
    // from whichever edge keeps it beside the trigger.
    let placed = window.use_keyed_state(
        ElementId::Name(format!("{id}-placed").into()),
        cx,
        |_, _| Placed(None),
    );
    let last = placed.read(cx).0;
    let current = options
        .iter()
        .find(|(v, ..)| v == selected)
        .map_or_else(|| SharedString::from("—"), |(_, l, ..)| l.clone());
    let toggle = open.clone();
    let trigger = div()
        .id(ElementId::Name(format!("{id}-trigger").into()))
        .flex()
        .items_center()
        .justify_between()
        .gap_2()
        .h(px(if inline_label.is_some() { 28. } else { 26. }))
        .px_2()
        .min_w(px(120.))
        .rounded(px(if inline_label.is_some() { 4. } else { 2. }))
        .border_1()
        .border_color(if is_open {
            theme::get().accent
        } else {
            theme::get().border
        })
        .bg(if inline_label.is_some() {
            theme::get().well
        } else {
            theme::get().input
        })
        .cursor_pointer()
        .hover(|s| s.bg(theme::get().hover))
        .children(inline_label.map(|l| {
            div()
                .flex_shrink_0()
                .text_size(px(10.))
                .text_color(theme::get().muted)
                .child(l)
        }))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .overflow_hidden()
                .whitespace_nowrap()
                .text_ellipsis()
                .child(current),
        )
        .child(div().text_color(theme::get().muted).child("▾"))
        .on_click(move |_, _, cx| {
            toggle.update(cx, |f, cx| {
                f.0 = !f.0;
                cx.notify();
            })
        });
    let mut root = div()
        .id(ElementId::Name(id.clone()))
        .relative()
        .flex()
        .flex_col()
        .child(trigger)
        .child(
            canvas(
                move |bounds, _, cx| placed.update(cx, |p, _| p.0 = Some(bounds)),
                |_, _, _, _| {},
            )
            .absolute()
            .inset_0(),
        );
    if is_open {
        let on = std::rc::Rc::new(on);
        let close = open.clone();
        // Options with pictures get a list wide enough for name and detail
        // on one line each (see max_w).
        let pictured = options.iter().any(|o| !o.3.is_empty());
        let mut list = div()
            .id(ElementId::Name(format!("{id}-list").into()))
            .occlude()
            // At least the trigger's width (160 px before it is measured).
            .min_w(last.map_or(px(160.), |b| b.size.width))
            // Wide enough for its options up to 360 px, so similar names
            // stay apart; longer ones end in an ellipsis.
            // Pictured lists at 340 px, so a name and its detail fit a line
            // each (held to a 280 px trigger they wrapped to one-word
            // orphans); they open away from the window edge like the rest.
            .max_w(last.map_or(px(360.), |b| {
                if pictured {
                    b.size.width.max(px(340.))
                } else {
                    b.size.width.max(px(360.))
                }
            }))
            .when(pictured, |d| d.min_w(px(340.)))
            // As tall as the room under the trigger allows: at a fixed
            // 320 px a list near the window's foot scrolled with no sign of
            // it, and its last option looked cut in half.
            .max_h(last.map_or(px(320.), |b| {
                (window.viewport_size().height - b.bottom() - px(16.)).max(px(320.))
            }))
            .overflow_y_scroll()
            .p_1()
            .flex()
            .flex_col()
            // The menus' surface: on the panel's own colour the open list
            // read as inline content, not an overlay.
            .bg(theme::get().popup)
            .border_1()
            .border_color(theme::get().floating_border)
            .rounded(px(3.))
            .shadow_lg()
            .on_mouse_down_out(move |_, _, cx| {
                close.update(cx, |f, cx| {
                    f.0 = false;
                    cx.notify();
                })
            });
        // The note wraps to the options' width: as one line it widened
        // the list past its trigger, across the edge of the pane it sits
        // in. No width of its own (w 0) so it never sizes the list; at
        // least the list's (min_w_full) so it fills it.
        if let Some(note) = note {
            list = list.child(
                div()
                    .w_0()
                    .min_w_full()
                    .whitespace_normal()
                    .px_1()
                    .pt_1()
                    .pb(px(6.))
                    .mb_1()
                    .border_b_1()
                    .border_color(theme::get().border)
                    .text_size(px(10.5))
                    .text_color(theme::get().muted)
                    .child(note),
            );
        }
        for (n, (value, label, detail, pictures)) in options.into_iter().enumerate() {
            let (on, open) = (on.clone(), open.clone());
            let current = value == *selected;
            // The current value is marked with a check, so it does not
            // look like the hovered row.
            list = list.child(
                div()
                    .id(("dropdown-option", n))
                    .flex()
                    // With a second line, the check sits on the title's line,
                    // not centred between it and its caption.
                    .map(|d| {
                        if detail.is_some() {
                            d.items_start().py_1()
                        } else {
                            d.items_center().h(px(24.))
                        }
                    })
                    .px_1()
                    .rounded(px(3.))
                    .cursor_pointer()
                    .text_size(px(11.))
                    .text_color(theme::get().text)
                    .hover(|s| s.bg(theme::get().hover))
                    .child(
                        div()
                            .w(px(16.))
                            .flex_shrink_0()
                            .text_color(theme::get().accent)
                            .child(if current { "✓" } else { "" }),
                    )
                    // Long options may be cut to the list's width; the
                    // whole name is on hover.
                    .when(label.len() > 20, |d| {
                        d.tooltip(Tooltip::text(label.clone(), "", ""))
                    })
                    // On the card grey the Library uses, so pale plants
                    // keep their shape against the dark list.
                    // One slot width in every row of a pictured list, so
                    // names start in one column whether a row has three
                    // pictures, one or none.
                    .when(pictured, |d| {
                        d.child(
                            div()
                                .flex()
                                .flex_shrink_0()
                                .w(px(3. * 28. + 2. * 2.))
                                .gap(px(2.))
                                .mr_2()
                                .children(pictures.into_iter().take(3).map(|p| {
                                    div()
                                        .size(px(28.))
                                        .rounded(px(3.))
                                        .bg(theme::get().picture_bg)
                                        .overflow_hidden()
                                        .child(img(p).size_full())
                                })),
                        )
                    })
                    .child(
                        div()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .child(div().truncate().child(label))
                            // Wrapped too with pictures: the detail is what
                            // tells look-alike presets apart ("4% flowers"),
                            // and cut it read "4% fl…".
                            .children(detail.map(|d| {
                                div()
                                    .truncate()
                                    .text_size(px(10.))
                                    .text_color(theme::get().muted)
                                    .child(d)
                            })),
                    )
                    .on_click(cx.listener(move |this, _, window, cx| {
                        open.update(cx, |f, _| f.0 = false);
                        on(this, value.clone(), window, cx);
                        cx.notify();
                    })),
            );
        }
        // Drawn after everything else, so panes below cannot cover it.
        // `anchored` takes the origin of its parent, so it sits in a box
        // pinned under the trigger.
        // Under the trigger, 3 px below: left edges aligned, or right edges
        // when a list that wide would run off the window, so it grows away
        // from the edge instead of being pushed over the labels beside it.
        let room = last.map_or(px(f32::MAX), |b| window.viewport_size().width - b.origin.x);
        let right = room < px(360. + 8.);
        let pin = div().absolute().top_full();
        root = root.child(if right {
            pin.right_0().child(
                deferred(
                    anchored()
                        .anchor(Anchor::TopRight)
                        .offset(point(px(0.), px(3.)))
                        .snap_to_window_with_margin(px(8.))
                        .child(list),
                )
                .with_priority(1),
            )
        } else {
            pin.left_0().child(
                deferred(
                    anchored()
                        .offset(point(px(0.), px(3.)))
                        .snap_to_window_with_margin(px(8.))
                        .child(list),
                )
                .with_priority(1),
            )
        });
    }
    root
}

/// A destructive button that asks twice: the first click turns it into
/// `confirm` (in the error color), the second calls `on`. Moving the
/// pointer off it disarms it. `id` must be unique among its siblings.
pub fn confirm_button<V: 'static>(
    id: impl Into<SharedString>,
    label: impl Into<SharedString>,
    confirm: impl Into<SharedString>,
    window: &mut Window,
    cx: &mut Context<V>,
    on: impl Fn(&mut V, &mut Window, &mut Context<V>) + 'static,
) -> Stateful<Div> {
    let id: SharedString = id.into();
    let armed = window.use_keyed_state(ElementId::Name(id.clone()), cx, |_, _| Flag(false));
    let is_armed = armed.read(cx).0;
    let (click, leave) = (armed.clone(), armed);
    // Not `button`: its id is its label, which changes when armed.
    div()
        .id(ElementId::Name(id))
        .flex()
        .items_center()
        .justify_center()
        .flex_shrink_0()
        .h(px(28.))
        .px_3()
        .rounded(px(3.))
        .border_1()
        // A neutral frame at rest, red only once armed: red-framed it was
        // the loudest thing on a page, and a second style beside the
        // neutral Delete buttons elsewhere.
        .border_color(if is_armed {
            theme::get().danger_border
        } else {
            theme::get().border
        })
        .cursor_pointer()
        .bg(if is_armed {
            theme::get().danger
        } else {
            theme::get().secondary
        })
        .hover(|s| s.bg(theme::get().danger_hover))
        .text_size(px(12.))
        .text_color(if is_armed {
            theme::get().on_accent
        } else {
            theme::get().error
        })
        .child(if is_armed {
            confirm.into()
        } else {
            label.into()
        })
        .on_hover(move |hovered, _, cx| {
            if !hovered {
                leave.update(cx, |f, cx| {
                    if std::mem::replace(&mut f.0, false) {
                        cx.notify();
                    }
                });
            }
        })
        .on_click(cx.listener(move |this, _, window, cx| {
            let fire = click.update(cx, |f, cx| {
                f.0 = !f.0;
                cx.notify();
                !f.0
            });
            if fire {
                on(this, window, cx);
                cx.notify();
            }
        }))
}

/// A thumbnail card: picture (or `placeholder` while there is none), then
/// a one-line caption. Add badges with `.child(badge(..).absolute()...)`;
/// the picture box is `relative`. `size` is the picture's side in px.
pub fn thumbnail_card(
    id: impl Into<ElementId>,
    image: Option<std::sync::Arc<RenderImage>>,
    size: f32,
    label: impl Into<SharedString>,
    // The grid's search, marked in the caption; "" for none.
    query: &str,
    placeholder: &'static str,
    selected: bool,
) -> Stateful<Div> {
    let picture = div()
        .size(px(size))
        .flex()
        .items_center()
        .justify_center()
        .rounded_sm()
        .bg(theme::get().picture_bg)
        .child(match image {
            Some(image) => img(image).size(px(size)).into_any_element(),
            None => div()
                .text_size(px(10.))
                .text_color(theme::get().muted)
                .child(placeholder)
                .into_any_element(),
        });
    div()
        .id(id)
        .relative()
        .w(px(size + CARD_PAD))
        .p_1()
        .flex()
        .flex_col()
        .items_center()
        .gap_1()
        .rounded_sm()
        .cursor_pointer()
        .bg(if selected {
            theme::get().selected
        } else {
            theme::get().panel
        })
        .hover(|s| s.bg(theme::get().hover))
        .child(picture)
        // Two lines, so names that differ only at the end (`…_02_a`,
        // `…_02_b`) stay apart. 11 px: at 10 the distinctions between
        // look-alike models were the hardest text on screen to read.
        .child(
            div()
                .w_full()
                .h(px(28.))
                .text_size(px(11.))
                .line_height(px(14.))
                .overflow_hidden()
                .child(matched_text(keep_tail(&label.into()), query, selected)),
        )
}

/// `label` with its short trailing words (`01`, `b`) bound to the word
/// before by non-breaking spaces, so a wrap never strands a number on its
/// own line.
fn keep_tail(label: &str) -> SharedString {
    let words: Vec<&str> = label.split(' ').collect();
    let mut out = String::with_capacity(label.len());
    for (i, w) in words.iter().enumerate() {
        if i > 0 {
            out.push(if w.len() <= 2 { '\u{a0}' } else { ' ' });
        }
        out.push_str(w);
    }
    out.into()
}

/// A card's width beyond its picture: `p_1` on both sides.
const CARD_PAD: f32 = 8.;

/// A virtualized grid of `count` cards `width` px wide (a
/// [`thumbnail_card`] of picture size `s` is `s + 8`). Only rows on screen
/// are built, so libraries of thousands stay cheap; a flex-wrap grid of 300
/// cards cost ~6 ms of layout and paint a frame. Columns follow the grid's
/// width from the previous frame. The grid fills its parent and shows a
/// [`scrollbar`].
pub fn card_grid(
    id: impl Into<SharedString>,
    count: usize,
    width: f32,
    window: &mut Window,
    cx: &mut App,
    card: impl Fn(usize, &mut Window, &mut App) -> AnyElement + 'static,
) -> AnyElement {
    let id: SharedString = id.into();
    let scroll = window
        .use_keyed_state(
            ElementId::Name(format!("{id}-scroll").into()),
            cx,
            |_, _| UniformListScrollHandle::new(),
        )
        .read(cx)
        .clone();
    let base = scroll.0.borrow().base_handle.clone();
    // gap_1 between cards, px_1 around the row, and the scrollbar's strip
    // kept clear on the right: the last column touched the bar while the
    // first sat 4 px in.
    let inner = f32::from(base.bounds().size.width) - 8. - SCROLLBAR_WIDTH;
    let columns = ((inner + 4.) / (width + 4.)).floor().max(1.) as usize;
    // The slack is shared out between the columns, so the grid fills the
    // row instead of leaving a ragged band on the right; the last row keeps
    // the same columns.
    let gap = if columns > 1 {
        ((inner - columns as f32 * width) / (columns - 1) as f32).clamp(4., 4. + width / 2.)
    } else {
        4.
    };
    let list = uniform_list(
        id.clone(),
        count.div_ceil(columns),
        move |range, window, cx| {
            range
                .map(|r| {
                    div()
                        .flex()
                        .gap(px(gap))
                        .pl_1()
                        .pr(px(4. + SCROLLBAR_WIDTH))
                        .pb_1()
                        .children(
                            (r * columns..((r + 1) * columns).min(count))
                                .map(|n| card(n, window, cx)),
                        )
                        .into_any_element()
                })
                .collect()
        },
    )
    .track_scroll(&scroll)
    .absolute()
    .inset_0();
    div()
        .relative()
        .size_full()
        .overflow_hidden()
        .child(list)
        .child(scrollbar(id, &base))
        .into_any_element()
}

pub fn separator() -> Div {
    div().h(px(1.)).my_1().mx_1().bg(theme::get().border)
}

/// A thin vertical divider between toolbar groups.
pub fn toolbar_separator() -> Div {
    div().h(px(20.)).w(px(1.)).mx_1().bg(theme::get().border)
}

/// The menu strip at the top of the window. Add `button(...).h(px(24.)).px_2()` per menu.
pub fn menu_bar() -> Div {
    div()
        .h(px(27.))
        .flex_shrink_0()
        .flex()
        .items_center()
        .px_1()
        .bg(theme::get().menu_bar)
}

/// A dropdown below the menu bar at `left` px. Close it from
/// `.on_mouse_down_out(...)` on the returned element.
pub fn menu_popup(left: f32) -> Stateful<Div> {
    div()
        .id("menu-popup")
        .occlude()
        .absolute()
        .top(px(28.))
        .left(px(left))
        // Fixed, so a menu keeps its width as items change; 285 holds the
        // longest second lines in one row (265 left a lone "m" on its own).
        .w(px(285.))
        // A shade above the panes, with a light edge, so the menu does
        // not melt into what is under it.
        .bg(theme::get().popup)
        .rounded(px(6.))
        .border_1()
        .border_color(theme::get().floating_border)
        .shadow_lg()
        .p_1()
        .flex()
        .flex_col()
}

/// A group caption inside a menu.
pub fn menu_header(label: impl Into<SharedString>) -> Div {
    div()
        .px_2()
        .pt_2()
        .pb_1()
        .text_size(px(10.))
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(theme::get().muted)
        .child(label.into())
}

/// A later group in a menu: a divider, then its caption. The first group
/// uses a bare [`menu_header`].
pub fn menu_section(label: impl Into<SharedString>) -> Div {
    div()
        .flex()
        .flex_col()
        .child(separator())
        .child(menu_header(label))
}

/// A menu entry that toggles something on or off: a box in the check
/// column, hollow when off, so an off toggle never reads as a plain action.
pub fn menu_item_checked(
    label: impl Into<SharedString>,
    checked: bool,
    shortcut: impl Into<SharedString>,
) -> Stateful<Div> {
    let mark = div()
        .size(px(11.))
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(2.))
        .border_1()
        .text_size(px(9.))
        .map(|d| {
            // An accent edge and tick on no fill: solid squares on every
            // ticked pane made the View menu the loudest in the app.
            if checked {
                d.border_color(theme::get().accent)
                    .text_color(theme::get().accent)
                    .child("✓")
            } else {
                d.border_color(theme::get().muted).opacity(0.6)
            }
        });
    menu_row(label, mark, shortcut)
}

/// One of a set where exactly one is current (workspaces, views): a dot
/// marks the current one, unlike a toggle's box.
pub fn menu_item_radio(
    label: impl Into<SharedString>,
    current: bool,
    shortcut: impl Into<SharedString>,
) -> Stateful<Div> {
    let mark = div()
        .size(px(6.))
        .ml(px(2.))
        .rounded_full()
        .when(current, |d| d.bg(theme::get().accent));
    menu_row(label, mark, shortcut)
}

fn menu_row(
    label: impl Into<SharedString>,
    mark: Div,
    shortcut: impl Into<SharedString>,
) -> Stateful<Div> {
    let label = label.into();
    div()
        .id(ElementId::Name(format!("check-{label}").into()))
        .flex()
        .items_center()
        .h(px(28.))
        .px_2()
        .rounded(px(4.))
        .cursor_pointer()
        .hover(|s| s.bg(theme::get().hover))
        .text_size(px(11.))
        .text_color(theme::get().text)
        .child(
            div()
                .w(px(16.))
                .flex_shrink_0()
                .flex()
                .items_center()
                .child(mark),
        )
        .child(div().flex_1().child(label))
        .child(
            div()
                .text_color(theme::get().muted)
                .text_size(px(10.))
                .child(shortcut.into()),
        )
}

/// A menu entry that cannot run now; `why` is its tooltip.
pub fn menu_item_off(
    label: impl Into<SharedString>,
    why: impl Into<SharedString>,
) -> Stateful<Div> {
    let label = label.into();
    let why = why.into();
    div()
        .id(ElementId::Name(format!("off-{label}").into()))
        .flex()
        .items_center()
        .h(px(28.))
        .px_2()
        .pl(px(24.))
        .overflow_hidden()
        .text_size(px(11.))
        .text_color(theme::get().disabled)
        .child(label.clone())
        .tooltip(Tooltip::text(label, "", why))
}

/// A [`menu_item_off`] whose reason shows on a second line, for a command
/// whose prerequisite is the next thing to do, not a detail to hover for.
/// Its shortcut stays, dimmed, so the key is learned before it works.
pub fn menu_item_off_why(
    label: impl Into<SharedString>,
    why: impl Into<SharedString>,
    shortcut: impl Into<SharedString>,
) -> Stateful<Div> {
    let label = label.into();
    div()
        .id(ElementId::Name(format!("off-{label}").into()))
        .flex()
        .flex_col()
        .py_1()
        .px_2()
        .pl(px(24.))
        .text_size(px(11.))
        .text_color(theme::get().disabled)
        .child(
            div()
                .flex()
                .justify_between()
                .child(label)
                .child(div().text_size(px(10.)).child(shortcut.into())),
        )
        .child(
            div()
                // Flush with its label, as other menus' second lines; the
                // smaller size keeps it from reading as an item of its own.
                .text_size(px(10.))
                // The disabled colour, as the label above it: brighter, the reason
                // read as the item itself.
                .text_color(theme::get().disabled)
                .child(why.into()),
        )
}

/// [`menu_popup`] anchored `right` px from the window's right edge.
pub fn menu_popup_right(right: f32) -> Stateful<Div> {
    div()
        .id("menu-popup")
        .occlude()
        .absolute()
        .top(px(28.))
        .right(px(right))
        .w(px(300.))
        // A shade above the panes, with a light edge, so the menu does
        // not melt into what is under it.
        .bg(theme::get().popup)
        .rounded(px(6.))
        .border_1()
        .border_color(theme::get().floating_border)
        .shadow_lg()
        .p_1()
        .flex()
        .flex_col()
}

pub fn menu_item(
    label: impl Into<SharedString>,
    shortcut: impl Into<SharedString>,
) -> Stateful<Div> {
    // Rows are the menu's own colour; only the hovered one lights up. The
    // left padding leaves the check column every menu shares, so items
    // line up under their headers whether or not they can be checked.
    button(label, false)
        .justify_between()
        .pl(px(24.))
        .h(px(28.))
        .rounded(px(4.))
        .bg(transparent_black())
        .text_size(px(11.))
        .child(
            div()
                .text_color(theme::get().muted)
                .text_size(px(10.))
                .child(shortcut.into()),
        )
}

/// A two-line menu row: the label with a muted detail under it (a
/// project's folder, a file's path) and a short note on the right. For
/// lists where names alone collide.
pub fn menu_item_detail(
    id: impl Into<SharedString>,
    label: impl Into<SharedString>,
    detail: impl Into<SharedString>,
    note: impl Into<SharedString>,
    checked: bool,
) -> Stateful<Div> {
    div()
        .id(ElementId::Name(id.into()))
        .flex()
        .items_center()
        .gap_2()
        .px_2()
        .py_1()
        .rounded(px(4.))
        .cursor_pointer()
        .hover(|s| s.bg(theme::get().hover))
        .child(
            // 8 px + the gap: labels start where `menu_item`'s do.
            div()
                .w(px(8.))
                .flex_shrink_0()
                .text_size(px(11.))
                .text_color(theme::get().accent)
                .child(if checked { "✓" } else { "" }),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .child(
                    div()
                        .text_size(px(11.))
                        .text_color(theme::get().text)
                        .child(label.into()),
                )
                .child(
                    // Wraps rather than cuts: the detail is often the
                    // consequence of the command, worth reading whole.
                    div()
                        .text_size(px(10.))
                        .text_color(theme::get().muted)
                        .child(detail.into()),
                ),
        )
        .child(
            div()
                .flex_shrink_0()
                .text_size(px(10.))
                .text_color(theme::get().muted)
                .child(note.into()),
        )
}

pub fn toolbar() -> Div {
    div()
        .h(px(40.))
        .flex_shrink_0()
        .flex()
        .items_center()
        .px_1()
        .gap_1()
        .bg(theme::get().panel)
        .border_b_1()
        .border_color(theme::get().border)
}

/// A rounded surface floating over a viewport. It swallows pointer input so
/// clicking a tool never becomes a stroke or pick in the viewport below.
pub fn floating_surface(id: impl Into<ElementId>) -> Stateful<Div> {
    div()
        .id(id)
        .occlude()
        .flex()
        .items_center()
        .rounded(px(8.))
        .bg(theme::get().floating)
        .border_1()
        .border_color(theme::get().floating_border)
        .shadow_lg()
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
        .on_mouse_down(MouseButton::Right, |_, _, cx| cx.stop_propagation())
        .on_mouse_down(MouseButton::Middle, |_, _, cx| cx.stop_propagation())
        .on_mouse_move(|_, _, cx| cx.stop_propagation())
        .on_scroll(|_, _, cx| cx.stop_propagation())
}

/// A dialog card. Put it in [`modal`].
pub fn dialog(title: impl Into<SharedString>) -> Div {
    div()
        .w(px(540.))
        .bg(theme::get().panel)
        .border_1()
        .border_color(theme::get().border)
        .rounded(px(4.))
        .shadow_lg()
        .p_5()
        .flex()
        .flex_col()
        .gap_3()
        .child(
            div()
                .text_size(px(16.))
                .font_weight(FontWeight::SEMIBOLD)
                .child(title.into()),
        )
}

/// Dims the window and centers `content` above everything else. `id` scopes
/// the dialog's element ids: buttons take their label as id, and a dialog's
/// "Save" must not share click state with a "Save" in the toolbar beneath.
pub fn modal(id: impl Into<ElementId>, content: impl IntoElement) -> Stateful<Div> {
    div()
        .id(id)
        .occlude()
        .absolute()
        .inset_0()
        .bg(theme::get().scrim)
        .flex()
        .items_center()
        .justify_center()
        .child(content)
}

/// The bottom strip: a status message on the left, cursor info on the right.
pub fn status_bar(
    message: impl Into<SharedString>,
    error: bool,
    // Text, or an element with fixed slots (a live readout) before it.
    right: impl IntoElement,
) -> Div {
    div()
        .h(px(25.))
        .flex_shrink_0()
        .border_t_1()
        .border_color(theme::get().border)
        .flex()
        .items_center()
        .justify_between()
        .px_2()
        .text_size(px(10.))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .overflow_hidden()
                .text_color(if error {
                    theme::get().error
                } else {
                    theme::get().muted
                })
                .child(message.into()),
        )
        .child(
            div()
                .flex()
                .items_center()
                .text_color(theme::get().muted)
                .child(right),
        )
}

/// What a scrollbar thumb drag carries: which scrollbar.
#[derive(Clone, Debug)]
pub struct ScrollThumbDrag(SharedString);

struct NoThumbPreview;

impl Render for NoThumbPreview {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
    }
}

// Track width, and the smallest thumb, so long lists stay grabbable.
const SCROLLBAR_WIDTH: f32 = 8.;
const MIN_THUMB: f32 = 24.;

/// A vertical scrollbar over the right edge of the area `handle` tracks;
/// put it last in the same `relative()` parent, which the area must fill.
/// The thumb's length is the visible share of the content; dragging it or
/// pressing the track scrolls so the thumb's middle follows the cursor.
/// Draws nothing until the area overflows (or before its first layout,
/// when the handle has no bounds yet).
pub fn scrollbar(id: impl Into<SharedString>, handle: &ScrollHandle) -> AnyElement {
    let id: SharedString = id.into();
    let area = handle.bounds();
    let view = f32::from(area.size.height);
    let max = f32::from(handle.max_offset().y);
    if view <= 1. || max <= 0.5 {
        return div().into_any_element();
    }
    let thumb = (view * view / (view + max)).clamp(MIN_THUMB.min(view), view);
    let scrolled = (-f32::from(handle.offset().y)).clamp(0., max);
    let top = scrolled / max * (view - thumb);
    // Scrolls so the thumb's middle sits at window y.
    let follow = {
        let handle = handle.clone();
        move |y: Pixels, window: &mut Window| {
            let top = f32::from(y - handle.bounds().origin.y) - thumb / 2.;
            let f = (top / (view - thumb).max(1.)).clamp(0., 1.);
            handle.set_offset(point(handle.offset().x, px(-f * max)));
            window.refresh();
        }
    };
    let press = follow.clone();
    let me = id.clone();
    div()
        .id(ElementId::Name(format!("{id}-scrollbar").into()))
        .absolute()
        .top_0()
        .right_0()
        .bottom_0()
        .w(px(SCROLLBAR_WIDTH))
        .on_mouse_down(MouseButton::Left, move |e, window, cx| {
            cx.stop_propagation();
            press(e.position.y, window);
        })
        .on_drag_move(move |e: &DragMoveEvent<ScrollThumbDrag>, window, cx| {
            if e.drag(cx).0 == me {
                follow(e.event.position.y, window);
            }
        })
        .child(
            div()
                .id(ElementId::Name(format!("{id}-thumb").into()))
                .absolute()
                .top(px(top))
                .right(px(1.))
                .w(px(SCROLLBAR_WIDTH - 2.))
                .h(px(thumb))
                .rounded_full()
                .bg(theme::get().scrollbar)
                .hover(|s| s.bg(theme::get().scrollbar_hover))
                .on_drag(ScrollThumbDrag(id), |_, _, _, cx| {
                    cx.new(|_| NoThumbPreview)
                }),
        )
        .into_any_element()
}

/// Scroll handles by name, created on first use, for hosts that render
/// scroll areas from `&self` without a window at hand. Clones share the
/// handles.
#[derive(Clone, Default)]
pub struct ScrollHandles(
    std::rc::Rc<std::cell::RefCell<std::collections::HashMap<SharedString, ScrollHandle>>>,
);

impl ScrollHandles {
    pub fn get(&self, id: impl Into<SharedString>) -> ScrollHandle {
        self.0.borrow_mut().entry(id.into()).or_default().clone()
    }
}

/// A vertical scroll area that shows a [`scrollbar`]: `content` scrolls
/// inside a box that fills the returned element, so style that one
/// (`flex_1`, `min_h_0`, `size_full`). Content must not be `size_full`
/// itself, or it never overflows; `w_full` is right.
pub fn vscroll(
    id: impl Into<SharedString>,
    handle: &ScrollHandle,
    content: impl IntoElement,
) -> Stateful<Div> {
    let id: SharedString = id.into();
    div()
        .id(ElementId::Name(format!("{id}-area").into()))
        .relative()
        .overflow_hidden()
        .child(
            div()
                .id(ElementId::Name(id.clone()))
                .absolute()
                .inset_0()
                .overflow_y_scroll()
                .track_scroll(handle)
                .child(content),
        )
        .child(scrollbar(id, handle))
}

#[cfg(test)]
mod tests {
    use super::match_ranges;

    #[test]
    fn match_ranges_marks_every_word_case_insensitive() {
        assert_eq!(
            match_ranges("Tree dead jungle 02", "DEAD 02"),
            [5..9, 17..19]
        );
        // Repeats are all marked; overlapping and touching words merge.
        assert_eq!(match_ranges("palm_palm", "palm"), [0..4, 5..9]);
        assert_eq!(match_ranges("rockface", "rock kface"), [0..8]);
        assert!(match_ranges("Bush", "tree").is_empty());
        assert!(match_ranges("Bush", "  ").is_empty());
    }

    #[test]
    fn match_ranges_keep_byte_offsets_past_non_ascii() {
        // "Größe" has two-byte letters before the hit; the range must
        // still land on "stein" in the original string.
        let text = "Größe stein";
        let r = match_ranges(text, "STEIN");
        assert_eq!(&text[r[0].clone()], "stein");
    }
}

#[cfg(test)]
mod color_tests {
    use super::Hsv;

    // RGB → HSV → RGB comes back as it went in, greys and primaries too;
    // the hue strip runs red, green, blue, red.
    #[test]
    fn hsv_round_trips_rgb() {
        for c in [
            [0, 0, 0],
            [255, 255, 255],
            [128, 128, 128],
            [255, 0, 0],
            [0, 255, 0],
            [0, 0, 255],
            [57, 10, 28],
            [255, 136, 0],
        ] {
            assert_eq!(Hsv::from_rgb(c).rgb(), c, "{c:?}");
        }
        let at = |h: f32| Hsv { h, s: 1., v: 1. }.rgb();
        assert_eq!(at(0.), [255, 0, 0]);
        assert_eq!(at(1. / 3.), [0, 255, 0]);
        assert_eq!(at(2. / 3.), [0, 0, 255]);
        assert_eq!(at(1.), [255, 0, 0]);
        assert_eq!(
            Hsv {
                h: 0.,
                s: 1.,
                v: 1.
            }
            .hex(),
            0xff0000
        );
    }
}
