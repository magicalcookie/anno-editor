// Owns placement editing: where a widget sits in its parent, the inspector's
// Layout block that edits it, and the canvas overlay that draws it. The
// game's rule, for a point p in the widget's own pixels:
//
//   parent pixels = anchor(AnchorPoint, parent Size) + Position
//                   + R(RotationAngle) · Scale ⊙ (p − PivotPoint)
//
// So Position is the arrow from the parent's anchor point to the widget's
// pivot, the pivot is a point in the widget's unscaled pixels, and scale and
// rotation happen about it. Anchor points exist only where the parent has a
// Size (screens: 1920×1080, containers: their template's ReferenceSize);
// in a plain group every anchor is the group's origin. The overlay draws the
// parent's Size box with its nine anchor points, the pivot, and the
// Position arrow between them.
use super::{
    UiEditor,
    scene::{Frame, apply, inverse},
};
use crate::editor::Editor;
use anno_formats::phx::{Edit, Value, layout::anchor_offset};
use anyhow::{Result, bail};
use gpui_editor_kit::{
    field::{EditEvent, Filter, TextEdit, text_field},
    gpui::{prelude::*, *},
    paint, theme,
    widgets::{Tooltip, button, info_row, section},
};

pub const ANCHOR_NAMES: [&str; 9] = [
    "top left",
    "top centre",
    "top right",
    "middle left",
    "centre",
    "middle right",
    "bottom left",
    "bottom centre",
    "bottom right",
];

/// Screen pixels within which a dragged pivot snaps to a preset.
const SNAP: f32 = 8.;

impl Frame {
    /// The pivot the game uses, in the widget's own pixels.
    pub fn effective_pivot(&self) -> [f32; 2] {
        match self.pivot_ratio {
            Some(r) => [
                self.local[0] + r[0] * self.local[2],
                self.local[1] + r[1] * self.local[3],
            ],
            None => [self.pivot[0], self.pivot[1]],
        }
    }

    pub fn pivot_world(&self) -> [f32; 2] {
        apply(self.transform, self.effective_pivot())
    }

    /// Anchor point `a` in world pixels; the parent's origin without a Size.
    pub fn anchor_world(&self, a: u32) -> [f32; 2] {
        apply(
            self.parent_transform,
            anchor_offset(a, self.anchor_size.unwrap_or_default()),
        )
    }

    /// Pivot presets: 3×3 points of the widget's own box.
    pub fn pivot_preset(&self, k: usize) -> [f32; 2] {
        [
            self.local[0] + (k % 3) as f32 * 0.5 * self.local[2],
            self.local[1] + (k / 3) as f32 * 0.5 * self.local[3],
        ]
    }
}

/// Sets AnchorPoint. With `keep`, Position changes by the opposite of the
/// anchor's move, so the widget stays where it is.
pub fn anchor_edits(f: &Frame, anchor: u32, keep: bool) -> Result<Vec<Edit>> {
    let Some((span, ty)) = f.anchor_span else {
        bail!("AnchorPoint is not stored for this widget");
    };
    let mut edits = vec![Edit::Value {
        span,
        value: Value::Enum { ty, value: anchor },
    }];
    if keep && let Some(size) = f.anchor_size {
        let (a, b) = (anchor_offset(f.anchor, size), anchor_offset(anchor, size));
        let Some(span) = f.position_span else {
            bail!("Position is not stored for this widget");
        };
        let p = f.position;
        edits.push(Edit::Value {
            span,
            value: Value::Vec3([p[0] + a[0] - b[0], p[1] + a[1] - b[1], p[2]]),
        });
    }
    Ok(edits)
}

/// Sets PivotPoint to `pivot` (own pixels). With `keep`, Position follows
/// the pivot's drawn move, `R · Scale ⊙ Δpivot`, so the widget stays put.
pub fn pivot_edits(f: &Frame, pivot: [f32; 2], keep: bool) -> Result<Vec<Edit>> {
    let Some(span) = f.pivot_span else {
        bail!("PivotPoint is not stored for this widget");
    };
    let mut edits = vec![Edit::Value {
        span,
        value: Value::Vec3([pivot[0], pivot[1], f.pivot[2]]),
    }];
    if keep {
        let Some(span) = f.position_span else {
            bail!("Position is not stored for this widget");
        };
        let d = [
            (pivot[0] - f.pivot[0]) * f.scale[0],
            (pivot[1] - f.pivot[1]) * f.scale[1],
        ];
        let (s, c) = f.rotation.to_radians().sin_cos();
        let p = f.position;
        edits.push(Edit::Value {
            span,
            value: Value::Vec3([p[0] + c * d[0] - s * d[1], p[1] + s * d[0] + c * d[1], p[2]]),
        });
    }
    Ok(edits)
}

/// One number of the Layout block.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Field {
    Position(usize),
    Pivot(usize),
    Scale(usize),
    /// Drawn size in the widget's own pixels × Scale; sets Scale.
    Size(usize),
    Rotation,
}

impl Field {
    /// The Layout block's fields in Tab order.
    const ORDER: [Field; 9] = [
        Field::Position(0),
        Field::Position(1),
        Field::Pivot(0),
        Field::Pivot(1),
        Field::Scale(0),
        Field::Scale(1),
        Field::Size(0),
        Field::Size(1),
        Field::Rotation,
    ];

    fn current(self, f: &Frame) -> f32 {
        match self {
            Field::Position(a) => f.position[a],
            Field::Pivot(a) => f.pivot[a],
            Field::Scale(a) => f.scale[a],
            Field::Size(a) => f.scale[a] * f.local[2 + a],
            Field::Rotation => f.rotation,
        }
    }

    fn edits(self, f: &Frame, v: f32, keep: bool) -> Result<Vec<Edit>> {
        let vec3 = |span: Option<usize>, mut base: [f32; 3], a: usize, name: &str| {
            let Some(span) = span else {
                bail!("{name} is not stored for this widget");
            };
            base[a] = v;
            Ok(vec![Edit::Value {
                span,
                value: Value::Vec3(base),
            }])
        };
        match self {
            Field::Position(a) => vec3(f.position_span, f.position, a, "Position"),
            Field::Pivot(a) => {
                let mut p = [f.pivot[0], f.pivot[1]];
                p[a] = v;
                pivot_edits(f, p, keep)
            }
            Field::Scale(a) => vec3(f.scale_span, f.scale, a, "Scale"),
            Field::Size(a) => {
                let own = f.local[2 + a];
                if own <= 0. {
                    bail!("This widget has no size of its own; resize its children");
                }
                Field::Scale(a).edits(f, v / own, keep)
            }
            Field::Rotation => {
                let Some(span) = f.rotation_span else {
                    bail!("RotationAngle is not stored for this widget");
                };
                Ok(vec![Edit::Value {
                    span,
                    value: Value::F32(v),
                }])
            }
        }
    }
}

impl UiEditor {
    fn own_frame(&self) -> Option<(usize, Frame)> {
        let n = self.canvas.selected?;
        (n > 0 && !self.nodes.get(n)?.foreign).then(|| (n, self.frames[n]))
    }

    fn place(&mut self, edits: Result<Vec<Edit>>, what: &str) {
        let r = edits.and_then(|e| self.apply(&e));
        self.message = Some(match r {
            Ok(()) => (what.to_owned(), false),
            Err(e) => (format!("Not applied: {e:#}"), true),
        });
    }

    pub fn set_anchor(&mut self, anchor: u32) {
        let Some((_, f)) = self.own_frame() else {
            return;
        };
        let keep = self.keep_in_place;
        self.place(
            anchor_edits(&f, anchor, keep),
            &format!("Anchor: {} of the parent", ANCHOR_NAMES[anchor as usize]),
        );
    }

    pub fn set_pivot(&mut self, pivot: [f32; 2]) {
        let Some((_, f)) = self.own_frame() else {
            return;
        };
        let keep = self.keep_in_place;
        self.place(
            pivot_edits(&f, pivot, keep),
            &format!("Pivot: {:.1}, {:.1}", pivot[0], pivot[1]),
        );
    }

    /// Starts typing into Layout field `field` of the selected widget,
    /// committing whatever was being typed before.
    pub(super) fn start_layout_edit(&mut self, field: Field) {
        // Already typing it: a click there places the caret, nothing is
        // applied (round 9 applied the half-typed value).
        if self.layout_edit.as_ref().is_some_and(|e| e.field == field) {
            return;
        }
        self.flush_edits();
        let Some((n, f)) = self.own_frame() else {
            return;
        };
        let value = field.current(&f);
        let text = format!("{}", (value * 1000.).round() / 1000.);
        self.layout_edit = Some(TextEdit::new(field, text, Filter::Number));
        self.layout_target = Some(self.nodes[n].widget);
    }

    /// Commits the Layout number being typed to the widget it was typed
    /// for, wherever the selection is now; one the field refuses stays as
    /// typed.
    pub(super) fn commit_layout_edit(&mut self) -> Result<()> {
        let (Some(edit), Some(widget)) = (self.layout_edit.take(), self.layout_target.take())
        else {
            return Ok(());
        };
        let Some(n) = (1..self.nodes.len())
            .find(|&n| !self.nodes[n].foreign && self.nodes[n].widget == widget)
        else {
            return Ok(());
        };
        let f = self.frames[n];
        if edit
            .number()
            .is_some_and(|v| (v - edit.field.current(&f)).abs() < 1e-6)
        {
            return Ok(());
        }
        let keep = self.keep_in_place;
        let r = edit
            .number()
            .filter(|v| v.is_finite())
            .ok_or_else(|| anyhow::anyhow!("Enter a number"))
            .and_then(|v| edit.field.edits(&f, v, keep))
            .and_then(|e| self.apply(&e));
        match &r {
            Ok(()) => self.message = Some(("Edited".into(), false)),
            Err(_) => {
                self.layout_edit = Some(edit);
                self.layout_target = Some(widget);
            }
        }
        r
    }

    /// Typing into a Layout number; false when none is being edited. Enter
    /// commits, Escape cancels, Tab and Shift Tab commit and go to the
    /// next or previous field; Ctrl shortcuts pass through.
    pub fn layout_key(&mut self, event: &KeyDownEvent) -> bool {
        let Some(edit) = &mut self.layout_edit else {
            return false;
        };
        if event.keystroke.key == "tab" {
            let field = edit.field;
            let back = event.keystroke.modifiers.shift;
            // Refused, Tab stays in the field to fix.
            if let Err(e) = self.commit_layout_edit() {
                self.message = Some((format!("Not applied: {e:#}"), true));
                return true;
            }
            let i = Field::ORDER.iter().position(|&f| f == field).unwrap_or(0);
            let n = Field::ORDER.len();
            let next = Field::ORDER[if back { (i + n - 1) % n } else { (i + 1) % n }];
            self.start_layout_edit(next);
            return true;
        }
        match edit.key_down(event) {
            EditEvent::Commit => {
                if let Err(e) = self.commit_layout_edit() {
                    self.message = Some((format!("Not applied: {e:#}"), true));
                }
            }
            EditEvent::Cancel => {
                self.layout_edit = None;
                self.layout_target = None;
            }
            EditEvent::Ignored
                if event.keystroke.modifiers.control || event.keystroke.modifiers.platform =>
            {
                return false;
            }
            EditEvent::Changed | EditEvent::Ignored => {}
        }
        true
    }

    /// The pivot handle was released at `at` (window pixels).
    pub fn finish_pivot_drag(&mut self, at: Point<Pixels>) {
        self.pivot_drag = false;
        let Some((_, f)) = self.own_frame() else {
            return;
        };
        let local = self.pivot_target(&f, at);
        self.set_pivot(local);
    }

    /// Where a pivot dropped at `at` lands in the widget's own pixels,
    /// snapped to the nine presets near them.
    fn pivot_target(&self, f: &Frame, at: Point<Pixels>) -> [f32; 2] {
        let rel = at - self.canvas_bounds.origin;
        let world = self.canvas.view.to_design(rel);
        let zoom = self.canvas.view.zoom;
        for k in 0..9 {
            let p = f.pivot_preset(k);
            let w = apply(f.transform, p);
            if (w[0] - world[0]).hypot(w[1] - world[1]) * zoom <= SNAP {
                return p;
            }
        }
        inverse(f.transform).map_or([f.pivot[0], f.pivot[1]], |inv| {
            let p = apply(inv, world);
            [p[0].round(), p[1].round()]
        })
    }
}

/// What a Layout row's fields follow, from [`UiEditor::follows`]: the
/// properties named `names` or their axes (`Size.x`), merged ("follows
/// MOBO_02" once for both axes).
fn follows_for(follows: &[(String, String)], names: &[&str]) -> Option<String> {
    let mut texts: Vec<&str> = follows
        .iter()
        .filter(|(p, _)| {
            names.iter().any(|n| {
                p.strip_prefix(n)
                    .is_some_and(|rest| rest.is_empty() || rest.starts_with('.'))
            })
        })
        .map(|(_, t)| t.as_str())
        .collect();
    texts.dedup();
    (!texts.is_empty()).then(|| texts.join("; "))
}

/// A 3×3 grid of small squares, `active` filled; `on` gets the index.
fn grid(
    id: &'static str,
    active: Option<usize>,
    color: Rgba,
    tips: impl Fn(usize) -> String,
    on: impl Fn(&mut Editor, usize) + Clone + 'static,
    cx: &mut Context<Editor>,
) -> Div {
    let mut g = div()
        .flex()
        .flex_col()
        .gap(px(2.))
        .p(px(3.))
        .border_1()
        .border_color(theme::get().border)
        .rounded(px(2.));
    for row in 0..3 {
        let mut r = div().flex().gap(px(2.));
        for col in 0..3 {
            let k = row * 3 + col;
            let on = on.clone();
            let tip = tips(k);
            r = r.child(
                div()
                    .id((id, k))
                    .size(px(14.))
                    .rounded(px(1.))
                    .cursor_pointer()
                    .border_1()
                    .border_color(color)
                    .when(active == Some(k), |d| d.bg(color))
                    .hover(|d| d.bg(color.alpha(0.5)))
                    .tooltip(Tooltip::text(tip, "", ""))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.keys_to(super::Keys::Canvas);
                        on(this, k);
                        cx.notify();
                    })),
            );
        }
        g = g.child(r);
    }
    g
}

impl Editor {
    fn layout_number(&self, ui: &UiEditor, f: &Frame, field: Field, cx: &mut Context<Self>) -> Div {
        let id = format!("layout-{field:?}");
        let value = field.current(f);
        let text = format!("{}", (value * 1000.).round() / 1000.);
        let editing = ui
            .layout_edit
            .as_ref()
            .filter(|e| e.field == field)
            .map(|e| e.display());
        div().flex_1().min_w(px(40.)).child(
            text_field(SharedString::from(id), &text, editing, "").on_click(cx.listener(
                move |this, _, window, cx| {
                    this.focus.focus(window, cx);
                    if let Some(ui) = &mut this.ui {
                        ui.start_layout_edit(field);
                    }
                    cx.notify();
                },
            )),
        )
    }

    /// A Layout row a binding or clip overrides: the value drawn, what it
    /// follows (a click shows that in the graph), and the stored fields
    /// under it, which only show once nothing overrides them. Round 3 showed
    /// the stored 0, 0 alone while the canvas drew 24×24.
    fn bound_row(
        &self,
        label: &str,
        drawn: String,
        from: Option<String>,
        fields: Vec<Div>,
        cx: &mut Context<Self>,
    ) -> Div {
        let from = from.unwrap_or_else(|| "set by the playing clip".to_owned());
        let link = div()
            .id(SharedString::from(format!("layout-follows-{label}")))
            .flex_1()
            .min_w_0()
            .truncate()
            .text_size(px(10.))
            .text_color(crate::colors::get().layout.bound)
            .cursor_pointer()
            .hover(|d| d.underline())
            .tooltip(Tooltip::text(
                from.clone(),
                "",
                "Show what feeds it in the Bindings graph",
            ))
            .child(from)
            .on_click(cx.listener(|this, _, _, cx| {
                if let Some(ui) = &mut this.ui {
                    ui.graph.on = true;
                }
                this.show_pane(crate::editor::Pane::UiBindings);
                cx.notify();
            }));
        div()
            .flex()
            .flex_col()
            .gap(px(2.))
            .child(self.layout_row(
                label,
                vec![
                        div()
                            .text_size(px(11.))
                            .text_color(crate::colors::get().layout.bound)
                            .child(drawn),
                        div().flex_1().min_w_0().child(link),
                    ],
            ))
            .child(self.layout_row("  stored", fields))
    }

    fn layout_row(&self, label: &str, fields: Vec<Div>) -> Div {
        div()
            .flex()
            .items_center()
            .gap_1()
            .child(
                div()
                    .w(px(70.))
                    .text_size(px(11.))
                    .text_color(theme::get().muted)
                    .child(label.to_owned()),
            )
            .children(fields)
    }

    /// The inspector's Layout block for the selected widget of this file.
    pub(super) fn layout_block(
        &self,
        follows: &[(String, String)],
        cx: &mut Context<Self>,
    ) -> Option<Div> {
        let ui = self.ui.as_ref()?;
        let (node, f) = ui.own_frame()?;
        let parent = (0..node)
            .rev()
            .find(|&p| ui.nodes[p].depth < ui.nodes[node].depth)
            .map(|p| ui.nodes[p].name.clone())
            .unwrap_or_default();
        let note = |t: String| {
            div()
                .text_size(px(10.))
                .text_color(theme::get().muted)
                .child(t)
        };
        let warn = |t: String| {
            div()
                .text_size(px(11.))
                .text_color(crate::colors::get().layout.bound)
                .child(t)
        };
        let mut b = div()
            .flex()
            .flex_col()
            .gap_1()
            .child(section("Layout"))
            .child(info_row(
                "Parent",
                match f.anchor_size {
                    Some([w, h]) => format!("{parent} · Size {w:.0}×{h:.0}"),
                    None => format!("{parent} · no Size"),
                },
            ));
        let bound: Vec<&str> = [
            ("Position", f.bound.position.is_some()),
            ("Pivot", f.bound.pivot.is_some()),
            ("Scale", f.bound.scale.is_some()),
            ("Size", f.bound.size.is_some()),
            ("Rotation", f.bound.rotation.is_some()),
            ("AlphaTint", f.bound.alpha.is_some()),
            ("Visible", f.bound.visible.is_some()),
        ]
        .into_iter()
        .filter_map(|(n, on)| on.then_some(n))
        .collect();
        if !bound.is_empty() {
            b = b.child(warn(format!(
                "Bound: {}. A bound field shows the value drawn and what it follows; edits change the stored value under it.",
                bound.join(", ")
            )));
        }
        if let Some(by) = f.placed_by {
            b = b.child(warn(format!(
                "The parent’s {by} places this widget; Position and anchor are overridden."
            )));
        }
        if f.anchor_size.is_none() {
            b = b.child(note(
                "Groups have no Size, so every anchor is the group’s origin. Anchors work inside screens and containers."
                    .into(),
            ));
        }
        let anchor_grid = grid(
            "anchor-cell",
            (f.anchor < 9).then_some(f.anchor as usize),
            crate::colors::get().layout.anchor,
            |k| format!("Anchor to the parent’s {}", ANCHOR_NAMES[k]),
            |this, k| {
                if let Some(ui) = &mut this.ui {
                    ui.set_anchor(k as u32);
                }
            },
            cx,
        );
        let pivot_now = f.effective_pivot();
        let pivot_cell = (0..9).find(|&k| {
            let p = f.pivot_preset(k);
            (p[0] - pivot_now[0]).abs() < 0.5 && (p[1] - pivot_now[1]).abs() < 0.5
        });
        let presets: Vec<[f32; 2]> = (0..9).map(|k| f.pivot_preset(k)).collect();
        let pivot_grid = grid(
            "pivot-cell",
            pivot_cell,
            crate::colors::get().layout.pivot,
            |k| format!("Pivot at the widget’s {}", ANCHOR_NAMES[k]),
            move |this, k| {
                if let Some(ui) = &mut this.ui {
                    ui.set_pivot(presets[k]);
                }
            },
            cx,
        );
        let caption = |title: &str, body: String| {
            div()
                .flex()
                .flex_col()
                .gap(px(2.))
                .child(div().text_size(px(11.)).child(title.to_owned()))
                .child(note(body))
        };
        b = b.child(
            div()
                .flex()
                .gap_3()
                .py_1()
                .child(div().flex().gap_2().child(anchor_grid).child(caption(
                    "Anchor",
                    if f.anchor < 9 {
                        format!("{} of the parent", ANCHOR_NAMES[f.anchor as usize])
                    } else {
                        format!("value {} (adds nothing)", f.anchor)
                    },
                )))
                .child(div().flex().gap_2().child(pivot_grid).child(caption(
                    "Pivot",
                    format!("{:.0}, {:.0}", pivot_now[0], pivot_now[1]),
                ))),
        );
        if let Some(r) = f.pivot_ratio {
            b = b.child(warn(format!(
                "ProportionalPivotPoint puts the pivot at {:.2}, {:.2} of the box; PivotPoint is overridden.",
                r[0], r[1]
            )));
        }
        let xy = |v: [f32; 3]| format!("{}, {}", v[0], v[1]);
        let pair = |a: Field, b: Field, cx: &mut Context<Self>| {
            vec![
                self.layout_number(ui, &f, a, cx),
                self.layout_number(ui, &f, b, cx),
            ]
        };
        let rows = [
            (
                "Position",
                &["Position"][..],
                f.bound.position.map(xy),
                pair(Field::Position(0), Field::Position(1), cx),
            ),
            (
                "Pivot",
                &["PivotPoint"][..],
                f.bound.pivot.map(xy),
                pair(Field::Pivot(0), Field::Pivot(1), cx),
            ),
            (
                "Scale",
                &["Scale"][..],
                f.bound.scale.map(xy),
                pair(Field::Scale(0), Field::Scale(1), cx),
            ),
            (
                "Size",
                &["Size"][..],
                f.bound.size.map(|s| format!("{} × {}", s[0], s[1])),
                pair(Field::Size(0), Field::Size(1), cx),
            ),
            (
                "Rotation",
                &["RotationAngle"][..],
                f.bound.rotation.map(|r| format!("{r}°")),
                vec![self.layout_number(ui, &f, Field::Rotation, cx)],
            ),
        ];
        for (label, names, drawn, fields) in rows {
            b = b.child(match drawn {
                Some(drawn) => {
                    self.bound_row(label, drawn, follows_for(follows, names), fields, cx)
                }
                None => self.layout_row(label, fields),
            });
        }
        b = b
            .child(
                div().flex().items_center().gap_2().child(
                    button("Keep in place", ui.keep_in_place)
                        .tooltip(Tooltip::text(
                            "Keep in place",
                            "",
                            "Changing the anchor or pivot adjusts Position so the widget does not move",
                        ))
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.keys_to(super::Keys::Canvas);
                            if let Some(ui) = &mut this.ui {
                                ui.keep_in_place = !ui.keep_in_place;
                            }
                            cx.notify();
                        })),
                ),
            )
            .child(note(
                "Position runs from the parent’s anchor point (orange) to the pivot (blue). Scale and rotation turn about the pivot. On the canvas: click an anchor dot, drag the pivot."
                    .into(),
            ));
        if f.metrics {
            b = b.child(note(
                "Metrics on this widget cancel ancestor scale; Position is divided by it when a parent scales."
                    .into(),
            ));
        }
        Some(b)
    }
}

/// The selected widget's placement on the canvas: the parent's Size box and
/// nine anchor points (click to anchor), the Position arrow, and the pivot
/// (drag to move it). `entity` routes the clicks.
pub fn overlay(ui: &UiEditor, entity: Entity<Editor>) -> Vec<AnyElement> {
    let mut out = Vec::new();
    let view = &ui.canvas.view;
    let screen_rect = |[x, y, w, h]: [f32; 4]| {
        [[x, y], [x + w, y], [x + w, y + h], [x, y + h], [x, y]].map(|c| view.to_screen(c[0], c[1]))
    };
    if ui.canvas.show_hidden {
        let hidden: Vec<[Point<Pixels>; 5]> = ui
            .nodes
            .iter()
            .skip(1)
            .filter(|n| !n.visible && n.rect[2] > 0. && n.rect[3] > 0.)
            .map(|n| screen_rect(n.rect))
            .collect();
        out.push(
            canvas(
                |_, _, _| {},
                move |bounds, _, window, _| {
                    for r in &hidden {
                        paint::line(
                            &r.map(|p| bounds.origin + p),
                            1.,
                            rgba(crate::colors::CANVAS_HIDDEN),
                            window,
                        );
                    }
                },
            )
            .absolute()
            .size_full()
            .into_any_element(),
        );
    }
    let Some(n) = ui.canvas.selected.filter(|&n| n > 0) else {
        return out;
    };
    let Some(f) = ui.frames.get(n).copied() else {
        return out;
    };
    // The region the selection's mask chain lets through.
    let mask = ui.nodes[n].clip.map(screen_rect);
    let own = !ui.nodes[n].foreign;
    let screen = |p: [f32; 2]| view.to_screen(p[0], p[1]);
    let size = f.anchor_size.unwrap_or_default();
    let corners = [[0., 0.], [size[0], 0.], size, [0., size[1]], [0., 0.]]
        .map(|c| screen(apply(f.parent_transform, c)));
    let anchors: [Point<Pixels>; 9] = std::array::from_fn(|k| screen(f.anchor_world(k as u32)));
    let anchor = screen(f.anchor_world(f.anchor.min(9)));
    let pivot = if ui.pivot_drag {
        let p = ui.pivot_target(&f, ui.cursor);
        screen(apply(f.transform, p))
    } else {
        screen(f.pivot_world())
    };
    let has_size = f.anchor_size.is_some();
    let active = f.anchor;
    // The widget's own box as drawn, rotation included; the canvas
    // selection frame is only its bounding box.
    let [lx, ly, lw, lh] = f.local;
    let outline = [
        [lx, ly],
        [lx + lw, ly],
        [lx + lw, ly + lh],
        [lx, ly + lh],
        [lx, ly],
    ]
    .map(|c| screen(apply(f.transform, c)));
    let rotated = f.transform[1].abs() > 1e-4 || f.transform[2].abs() > 1e-4;
    out.push(
        canvas(
            |_, _, _| {},
            move |bounds, _, window, _| {
                let o = bounds.origin;
                let at = |p: Point<Pixels>| o + p;
                if let Some(m) = mask {
                    paint::line(&m.map(at), 1.5, crate::colors::get().layout.mask, window);
                }
                if rotated {
                    paint::line(&outline.map(at), 1.5, theme::get().highlight, window);
                }
                if has_size {
                    paint::line(
                        &corners.map(at),
                        1.,
                        crate::colors::get().layout.anchor.alpha(0.56),
                        window,
                    );
                    for (k, &p) in anchors.iter().enumerate() {
                        if k as u32 == active {
                            paint::filled_circle(
                                at(p),
                                5.,
                                crate::colors::get().layout.anchor,
                                window,
                            );
                        } else {
                            paint::circle(
                                at(p),
                                3.5,
                                1.,
                                crate::colors::get().layout.anchor.alpha(0.69),
                                window,
                            );
                        }
                    }
                } else {
                    paint::filled_circle(
                        at(anchor),
                        4.,
                        crate::colors::get().layout.anchor,
                        window,
                    );
                }
                paint::line(
                    &[at(anchor), at(pivot)],
                    1.5,
                    crate::colors::get().layout.link,
                    window,
                );
                paint::circle(at(pivot), 6., 2., crate::colors::get().layout.pivot, window);
                paint::line(
                    &[
                        at(pivot) - point(px(9.), px(0.)),
                        at(pivot) + point(px(9.), px(0.)),
                    ],
                    1.,
                    crate::colors::get().layout.pivot,
                    window,
                );
                paint::line(
                    &[
                        at(pivot) - point(px(0.), px(9.)),
                        at(pivot) + point(px(0.), px(9.)),
                    ],
                    1.,
                    crate::colors::get().layout.pivot,
                    window,
                );
            },
        )
        .absolute()
        .size_full()
        .into_any_element(),
    );
    // The arrow's label sits at its middle.
    let mid = point(
        (anchor.x + pivot.x) * 0.5 + px(6.),
        (anchor.y + pivot.y) * 0.5 - px(16.),
    );
    out.push(
        div()
            .absolute()
            .left(mid.x)
            .top(mid.y)
            .px_1()
            .rounded(px(2.))
            .bg(theme::get().overlay)
            .text_size(px(10.))
            .text_color(crate::colors::get().layout.link)
            .child(format!(
                "Position {:.0}, {:.0}",
                f.position[0], f.position[1]
            ))
            .into_any_element(),
    );
    if !own {
        return out;
    }
    if has_size {
        for (k, p) in anchors.iter().enumerate() {
            let entity = entity.clone();
            out.push(
                div()
                    .id(("anchor-dot", k))
                    .absolute()
                    .left(p.x - px(7.))
                    .top(p.y - px(7.))
                    .size(px(14.))
                    .rounded(px(7.))
                    .cursor_pointer()
                    .hover(|d| d.bg(crate::colors::get().layout.anchor.alpha(0.38)))
                    .tooltip(Tooltip::text(
                        format!("Anchor: {}", ANCHOR_NAMES[k]),
                        "",
                        "Click to anchor the widget here",
                    ))
                    .on_mouse_down(MouseButton::Left, move |_, _, cx| {
                        cx.stop_propagation();
                        entity.update(cx, |this, cx| {
                            if let Some(ui) = &mut this.ui {
                                ui.set_anchor(k as u32);
                            }
                            cx.notify();
                        });
                    })
                    .into_any_element(),
            );
        }
    }
    out.push(
        div()
            .id("pivot-handle")
            .absolute()
            .left(pivot.x - px(8.))
            .top(pivot.y - px(8.))
            .size(px(16.))
            .rounded(px(8.))
            .cursor(CursorStyle::Crosshair)
            .hover(|d| d.bg(crate::colors::get().layout.pivot.alpha(0.31)))
            .on_mouse_down(MouseButton::Left, move |e, _, cx| {
                cx.stop_propagation();
                entity.update(cx, |this, cx| {
                    if let Some(ui) = &mut this.ui {
                        ui.pivot_drag = true;
                        ui.cursor = e.position;
                    }
                    cx.notify();
                });
            })
            .into_any_element(),
    );
    out
}

#[cfg(test)]
mod tests {
    use super::{Field, anchor_edits, apply, follows_for, inverse, pivot_edits};
    use crate::ui::scene::Frame;
    use crate::ui::test_support::{
        ICON_ROW, TECH_TREE, key, movable, node, number_input, open, select,
    };
    use crate::ui::{Keys, UiEditor};
    use anno_formats::phx::{Edit, Value};
    use gpui_editor_kit::field::{Filter, TextEdit};

    fn frame() -> Frame {
        Frame {
            position: [10., 20., 0.],
            pivot: [0., 0., 0.],
            scale: [2., 2., 1.],
            local: [0., 0., 50., 30.],
            anchor: 0,
            anchor_size: Some([400., 200.]),
            position_span: Some(1),
            pivot_span: Some(2),
            scale_span: Some(3),
            anchor_span: Some((4, 0xabcd)),
            ..Frame::default()
        }
    }

    fn vec3(e: &Edit) -> [f32; 3] {
        match e {
            Edit::Value {
                value: Value::Vec3(v),
                ..
            } => *v,
            other => panic!("{other:?}"),
        }
    }

    // Moving the anchor from top-left to the centre of a 400×200 parent
    // keeps the widget in place: Position loses the anchor's 200, 100.
    #[test]
    fn anchor_change_keeps_the_widget_in_place() {
        let e = anchor_edits(&frame(), 4, true).unwrap();
        assert!(matches!(
            e[0],
            Edit::Value {
                span: 4,
                value: Value::Enum {
                    ty: 0xabcd,
                    value: 4
                }
            }
        ));
        assert_eq!(vec3(&e[1]), [-190., -80., 0.]);
        assert_eq!(anchor_edits(&frame(), 4, false).unwrap().len(), 1);
        // Without a parent Size the anchor moves nothing.
        let group = Frame {
            anchor_size: None,
            ..frame()
        };
        assert_eq!(anchor_edits(&group, 8, true).unwrap().len(), 1);
    }

    // Moving the pivot to the box centre (25, 15) under Scale 2 moves the
    // drawn pivot by 50, 30; Position follows so nothing moves.
    #[test]
    fn pivot_change_keeps_the_widget_in_place() {
        let e = pivot_edits(&frame(), [25., 15.], true).unwrap();
        assert_eq!(vec3(&e[0]), [25., 15., 0.]);
        assert_eq!(vec3(&e[1]), [60., 50., 0.]);
        // Rotated 90°: the shift turns with it.
        let turned = Frame {
            rotation: 90.,
            ..frame()
        };
        let p = vec3(&pivot_edits(&turned, [25., 15.], true).unwrap()[1]);
        assert!(
            (p[0] - -20.).abs() < 1e-3 && (p[1] - 70.).abs() < 1e-3,
            "{p:?}"
        );
    }

    #[test]
    fn size_sets_scale_and_inverse_undoes_transforms() {
        let e = Field::Size(0).edits(&frame(), 200., false).unwrap();
        assert_eq!(vec3(&e[0]), [4., 2., 1.]);
        let t = [2., 0.5, -1., 3., 10., 20.];
        let inv = inverse(t).unwrap();
        let p = apply(t, [3., 4.]);
        let back = apply(inv, p);
        assert!((back[0] - 3.).abs() < 1e-4 && (back[1] - 4.).abs() < 1e-4);
    }

    // Needs the game: a bound Layout field says what it follows. The icon
    // row's Icon takes both Size axes from one template property; a copy
    // reads "follows", a converter "computed from".
    #[test]
    fn bound_fields_name_what_they_follow() {
        let Some(mut ui) = open(ICON_ROW) else {
            return;
        };
        let icon = node(&ui, "Icon");
        let follows = ui.follows(icon);
        assert_eq!(
            follows_for(&follows, &["Size"]).as_deref(),
            Some("follows MOBO_02")
        );
        assert!(follows_for(&follows, &["Position"]).is_none());
        assert!(
            follows_for(&follows, &["ColorTint"]).is_some_and(|t| t.starts_with("computed from"))
        );
        assert!(
            ui.frames[icon].bound.size.is_some(),
            "the layout draws the bound size"
        );
        let follows = ui.follows(node(&ui, "BgIconText"));
        assert_eq!(
            follows_for(&follows, &["AlphaTint"]).as_deref(),
            Some("follows Bg_Alpha")
        );
        // A prefix that is not an axis does not match: Scale is not ScaleX.
        let fake = [("ScaleFactor".to_owned(), "follows X".to_owned())];
        assert!(follows_for(&fake, &["Scale"]).is_none());
    }

    // Needs the game: a Layout value typed on a movable widget of the tech
    // tree, and every way typing ends or the keys move on.
    #[test]
    fn typed_layout_values_apply_once_typing_ends() {
        let Some(mut ui) = open(TECH_TREE) else {
            return;
        };
        let n = (1..ui.nodes.len())
            .find(|&i| movable(&ui, i))
            .expect("a movable widget");
        ui.select_widget(n);

        // Leaving (quit, open, switch game) asks about unsaved edits only
        // after ending typing, so a half-typed value has to count as one
        // (round 12: Ctrl Q dropped a typed X on a clean file).
        assert!(!ui.dirty());
        let x = ui.position_x();
        ui.type_position_x(&format!("{}", x + 3.));
        assert!(!ui.dirty(), "typing alone edits nothing");
        // What `Editor::end_typing` does.
        let keys = ui.keys;
        ui.take_keys(keys);
        assert!(ui.typing().is_none());
        assert!(ui.dirty());
        assert_eq!(ui.keys, keys, "the keys stay where they are");

        // A click on the Layout field being typed leaves the edit as it is,
        // half-typed text and all; nothing is applied (round 9: it applied
        // "12" as an undo step and refused a lone "-").
        let bytes = ui.asset.encode();
        ui.message = None;
        for half in ["-", "12"] {
            ui.type_position_x(half);
            ui.start_layout_edit(Field::Position(0));
            assert_eq!(ui.typed().as_deref(), Some(half), "still typing");
            assert_eq!(ui.asset.encode(), bytes, "nothing applied");
            assert!(ui.message.is_none(), "{:?}", ui.message);
            ui.layout_edit = None;
        }
        // Another field still applies the one before.
        ui.type_position_x("12");
        ui.start_layout_edit(Field::Position(1));
        assert_eq!(ui.position_x(), 12.);
        ui.layout_edit = None;

        // While a pane the UI editor does not own (the asset browser or
        // preview) has the keys, the canvas takes none: an arrow moves no
        // widget (round 10: Delete in the browser's search deleted the
        // selected container). With the canvas's keys it moves it.
        let at = |ui: &UiEditor| ui.frames[ui.canvas.selected.unwrap()].position;
        let before = at(&ui);
        ui.take_keys(Keys::Other);
        assert!(!ui.key_down(&key("right")), "not the canvas's");
        assert_eq!(at(&ui), before);
        ui.take_keys(Keys::Canvas);
        assert!(ui.key_down(&key("right")));
        assert_ne!(at(&ui), before, "the canvas's again");

        // A click that takes the keys leaves no field typing. The binder's
        // search stops taking keys and keeps its text (round 4: F after a
        // graph click landed in it); a half-typed layout value is applied,
        // as a canvas click always did; graph and timeline never both hold
        // the keys (round 4: the graph kept a timeline click's keys).
        ui.open_binder();
        let b = ui.binder.as_mut().unwrap();
        b.search = "Bg".into();
        b.edit = Some(TextEdit::new(0, "Bg", Filter::Text));
        assert!(ui.typing().is_some());
        ui.take_keys(Keys::Graph);
        assert!(ui.typing().is_none(), "the binder's search stopped");
        let search = &ui.binder.as_ref().unwrap().search;
        assert_eq!(search, "Bg", "and kept its text");
        assert!(ui.graph_focused() && !ui.timeline_focused());
        ui.type_position_x("321");
        ui.take_keys(Keys::Timeline);
        assert!(ui.typing().is_none());
        assert!(ui.timeline_focused() && !ui.graph_focused());
        assert_eq!(ui.position_x(), 321., "the typed value applied");

        // A Layout number typed for one widget lands on that widget when
        // the selection moves to another before Enter, never on the new
        // one; Tab commits and moves to the next field of the same widget;
        // undo drops what is half typed.
        let b = (n + 1..ui.nodes.len())
            .find(|&i| movable(&ui, i))
            .expect("two movable widgets");
        let (wa, wb) = (ui.nodes[n].widget, ui.nodes[b].widget);
        let b_before = ui.frames[b].position;
        ui.type_position_x("1234");
        ui.select_widget(b);
        let at = |ui: &UiEditor, w: usize| {
            let n = (1..ui.nodes.len())
                .find(|&n| !ui.nodes[n].foreign && ui.nodes[n].widget == w)
                .unwrap();
            ui.frames[n].position
        };
        assert_eq!(at(&ui, wa)[0], 1234.);
        assert_eq!(at(&ui, wb), b_before, "the other widget is untouched");
        ui.type_position_x("77");
        assert!(ui.layout_key(&key("tab")));
        assert_eq!(at(&ui, wb)[0], 77.);
        let field = ui.layout_edit.as_ref().map(|e| e.field);
        assert_eq!(field, Some(Field::Position(1)));
        assert_eq!(ui.layout_target, Some(wb));
        // Undo drops the half-typed Y and takes back the X.
        ui.layout_edit.as_mut().unwrap().text = "5".into();
        assert!(ui.undo(false));
        assert!(ui.layout_edit.is_none());
        assert_eq!(at(&ui, wb), b_before);

        // An on/off property clicked while Position X is typed: the press
        // in the inspector applies X first, then the switch flips, and no
        // later key reaches X (round 8: 123 then 4 became 1234).
        ui.select_widget(n);
        let w = ui.nodes[n].widget;
        let flag = ui.asset.widgets()[w]
            .props
            .iter()
            .find(|p| matches!(p.value, Value::Bool(_)))
            .map(|p| (p.span, p.value.clone()))
            .expect("an on/off property");
        ui.type_position_x("123");
        ui.take_keys(Keys::Canvas);
        let on = flag.1 == Value::Bool(true);
        ui.commit(flag.0, if on { "false" } else { "true" })
            .unwrap();
        assert!(ui.typing().is_none(), "X no longer takes keys");
        assert_eq!(ui.position_x(), 123.);
        let now = ui.asset.widgets()[w]
            .props
            .iter()
            .find(|p| p.span == flag.0)
            .map(|p| p.value.clone());
        assert_eq!(now, Some(Value::Bool(!on)));
    }

    // Needs the game: Enter (or Tab) on a value the field refuses keeps the
    // text to fix, with why; a click elsewhere then drops it (round 13: the
    // text was gone at once). Layout and Preview fields alike.
    #[test]
    fn a_refused_value_stays_to_fix() {
        let Some(mut ui) = open(ICON_ROW) else {
            return;
        };
        select(&mut ui, "Icon");
        let x = ui.position_x();
        ui.type_position_x("-");
        for k in ["enter", "tab"] {
            assert!(ui.layout_key(&key(k)));
            assert_eq!(ui.typed().as_deref(), Some("-"), "{k} keeps it");
            let said = ui.message.take().map(|m| m.0).unwrap_or_default();
            assert!(said.contains("Enter a number"), "{said}");
        }
        ui.take_keys(ui.keys);
        assert_eq!(ui.typed(), None, "a click elsewhere drops it");
        assert_eq!(ui.position_x(), x);
        // A Preview number.
        let input = number_input(&ui);
        let text = |ui: &UiEditor| ui.preview.edit.as_ref().map(|e| e.text.clone());
        ui.preview.edit = Some(TextEdit::new(input, "abc", Filter::Text));
        // Tab first, as Enter (round 14: Tab did nothing, said nothing).
        ui.message = None;
        assert!(ui.preview_key(&key("tab")));
        assert!(ui.message.take().is_some_and(|m| m.1), "Tab says why");
        assert_eq!(text(&ui).as_deref(), Some("abc"));
        assert!(ui.preview_key(&key("enter")));
        assert_eq!(text(&ui).as_deref(), Some("abc"));
        assert!(ui.message.as_ref().is_some_and(|m| m.1));
        // In the inspector's words, and never a number the layout cannot
        // place (round 14: "inf" went through and blanked the canvas).
        for (typed, says) in [("abc", "Enter a number"), ("inf", "Enter a finite number")] {
            ui.preview.edit = Some(TextEdit::new(input, typed, Filter::Text));
            ui.message = None;
            assert!(ui.preview_key(&key("enter")));
            assert_eq!(text(&ui).as_deref(), Some(typed));
            let said = ui.message.take().map(|m| m.0).unwrap_or_default();
            assert!(said.contains(says), "{typed}: {said}");
        }
    }
}
