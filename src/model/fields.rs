// Owns the inspector's field grid: an entry's fields grouped by the config
// they sit in (a material's under its name, folding), each edited by what
// it holds: a flag as a check box, a number with steps, `.x/.y/.z` siblings
// as one vector row, `.r/.g/.b` as a colour with its swatch, a file path
// with the asset picker and drops from the Asset browser, the rest as text.
// A filter narrows the grid by name.

use super::{Field, ModelEditor, picker::Kind};
use crate::browser::AssetDrag;
use crate::editor::Editor;
use anno_formats::xml::Id;
use gpui_editor_kit::{
    field::{Filter, TextEdit, text_field},
    gpui::{prelude::*, *},
    theme,
    widgets::{self, Tooltip},
};

/// One row of the grid: a leaf, or sibling leaves that edit as one.
enum Cell {
    Flag(Id),
    Number(Id),
    /// `Name.x`, `Name.y`, … in order.
    Vector(String, Vec<Id>),
    /// `Name.r`, `.g`, `.b`.
    Colour(String, [Id; 3]),
    Path(Id, Kind),
    Text(Id),
}

impl Cell {
    fn name<'a>(&'a self, m: &'a ModelEditor) -> &'a str {
        match self {
            Cell::Vector(n, _) | Cell::Colour(n, _) => n,
            Cell::Flag(id) | Cell::Number(id) | Cell::Path(id, _) | Cell::Text(id) => {
                m.doc.name(*id)
            }
        }
    }
}

/// The leaves of one config: an entry's own, or a nested material's.
struct Group {
    /// Unique among the entry's groups: folds by it.
    key: String,
    heading: String,
    cells: Vec<Cell>,
}

/// Names that read as switches when their value is 0 or 1: the shaders'
/// `SOMETHING_ENABLED` and the cfgs' `cUseX`, `IsX`, `Looped`, …
fn flaggy(name: &str) -> bool {
    let caps = name
        .chars()
        .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_');
    let words = [
        "Enable", "Use", "Is", "Has", "Allow", "Disable", "Looped", "Visible", "Double", "Adapt",
        "Deform", "Flip", "Rotate", "Bound", "Receive",
    ];
    caps || words.iter().any(|w| name.contains(w))
}

/// What a path field names, by its value's extension and its name.
fn path_kind(m: &ModelEditor, leaf: Id, name: &str, text: &str) -> Option<Kind> {
    let lower = text.to_ascii_lowercase();
    let ext = lower.rsplit_once('.').map(|(_, e)| e);
    let texture_name = ["Tex", "Texture", "Map", "Mask", "RampPath", "TexPath"]
        .iter()
        .any(|s| name.ends_with(s));
    match ext {
        Some("psd" | "png" | "tga" | "dds" | "jpg") => Some(Kind::Texture),
        Some("cfg" | "prp") => Some(Kind::Prop),
        Some("rdp") => Some(Kind::Effect),
        Some("fbx" | "rdm") => {
            let parent = m.doc.parent(leaf)?;
            let animation =
                (m.doc.child(parent, "ConfigType")).is_some_and(|t| m.doc.text(t) == "ANIMATION");
            Some(if animation {
                Kind::Animation
            } else {
                Kind::Mesh
            })
        }
        None if texture_name && text.is_empty() => Some(Kind::Texture),
        _ => None,
    }
}

/// The cells of `id`'s own leaves, siblings folded into vector and colour
/// rows.
fn cells(m: &ModelEditor, id: Id) -> Vec<Cell> {
    let doc = &m.doc;
    let leaves: Vec<Id> = (doc.children(id))
        .filter(|&c| doc.is_leaf(c) && doc.name(c) != "ConfigType")
        .collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < leaves.len() {
        let name = doc.name(leaves[i]);
        if let Some((stem, axis)) = name.rsplit_once('.') {
            let run: Vec<Id> = leaves[i..]
                .iter()
                .copied()
                .take_while(|&l| doc.name(l).rsplit_once('.').is_some_and(|(s, _)| s == stem))
                .collect();
            let axes: Vec<&str> = run
                .iter()
                .map(|&l| doc.name(l).rsplit('.').next().unwrap_or(""))
                .collect();
            if axis == "r" && axes == ["r", "g", "b"] {
                out.push(Cell::Colour(stem.to_owned(), [run[0], run[1], run[2]]));
                i += 3;
                continue;
            }
            if run.len() >= 2 && axes.iter().all(|a| ["x", "y", "z", "w"].contains(a)) {
                i += run.len();
                out.push(Cell::Vector(stem.to_owned(), run));
                continue;
            }
        }
        let leaf = leaves[i];
        let text = doc.text(leaf);
        out.push(if let Some(kind) = path_kind(m, leaf, name, text) {
            Cell::Path(leaf, kind)
        } else if (text == "0" || text == "1") && flaggy(name) {
            Cell::Flag(leaf)
        } else if text.parse::<f32>().is_ok() {
            Cell::Number(leaf)
        } else {
            Cell::Text(leaf)
        });
        i += 1;
    }
    out
}

/// `id`'s groups, depth first: its own leaves, then each nested config's
/// under a heading (its `Name`, else the list it is in). The Transformer
/// edits as the placement above.
fn groups(m: &ModelEditor, id: Id, heading: &str, key: &str, out: &mut Vec<Group>) {
    let doc = &m.doc;
    let own = cells(m, id);
    if !own.is_empty() {
        out.push(Group {
            key: key.to_owned(),
            heading: heading.to_owned(),
            cells: own,
        });
    }
    for (k, c) in doc.children(id).filter(|&c| !doc.is_leaf(c)).enumerate() {
        let name = doc.name(c);
        if name == "Transformer" {
            continue;
        }
        let named = doc
            .child(c, "Name")
            .map(|n| doc.text(n))
            .filter(|n| !n.is_empty());
        let kind = doc
            .child(c, "ConfigType")
            .map(|t| doc.text(t).to_lowercase());
        let inner = match (name, named, kind) {
            ("Config", Some(n), _) => n.to_owned(),
            ("Config", None, Some(kind)) => format!("{kind} {}", k + 1),
            (list, ..) => list.to_owned(),
        };
        let inner = match heading.is_empty() {
            true => inner,
            false => format!("{heading} › {inner}"),
        };
        groups(m, c, &inner, &format!("{key}/{k}"), out);
    }
}

/// The value a typed number steps by: whole for whole numbers, else a
/// tenth of its size's order, at least a hundredth.
fn step_of(text: &str) -> f32 {
    let v: f32 = text.parse().unwrap_or(0.);
    if !text.contains('.') && !text.contains('e') {
        return 1.;
    }
    let order = 10f32.powf(v.abs().max(0.01).log10().floor());
    (order / 10.).max(0.01)
}

/// Text without float noise: `1.2000000476837158` as `1.2`.
fn number_text(v: f32) -> String {
    let rounded = (v * 1e5).round() / 1e5;
    format!("{rounded}")
}

/// A click that starts typing into `leaf`.
fn edit(
    leaf: Id,
    filter: Filter,
    cx: &mut Context<Editor>,
) -> impl Fn(&ClickEvent, &mut Window, &mut App) + 'static {
    cx.listener(move |this: &mut Editor, _: &ClickEvent, _, cx| {
        if let Some(m) = &mut this.model {
            let text = m.doc.text(leaf).to_owned();
            m.typing = Some(TextEdit::new(Field::Value(leaf), text, filter));
        }
        cx.notify();
    })
}

/// A click that sets `leaf` to `value`.
fn set(
    leaf: Id,
    value: String,
    cx: &mut Context<Editor>,
) -> impl Fn(&ClickEvent, &mut Window, &mut App) + 'static {
    cx.listener(move |this: &mut Editor, _: &ClickEvent, _, cx| {
        if let Some(m) = &mut this.model {
            m.set_value(leaf, &value);
        }
        this.model_edited(cx);
    })
}

fn label(name: &str) -> Stateful<Div> {
    div()
        .id(SharedString::from(format!("field-label-{name}")))
        .w(px(150.))
        .flex_shrink_0()
        .truncate()
        .text_color(theme::get().muted)
        .child(name.to_owned())
        .tooltip(Tooltip::text(name.to_owned(), "", ""))
}

fn row() -> Div {
    div().flex().items_center().gap_1().min_h(px(26.))
}

impl Editor {
    /// The field grid of `entry`, filtered by the inspector's filter.
    pub(super) fn render_fields(
        &mut self,
        entry: Id,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Div {
        let Some(m) = &self.model else {
            return div();
        };
        let filter = m.field_filter.to_ascii_lowercase();
        let mut all = Vec::new();
        groups(m, entry, "", "fields", &mut all);
        let editing_filter = (m.typing.as_ref())
            .filter(|e| e.field == Field::InspectorFilter)
            .map(|e| e.display());
        let mut out = div().flex().flex_col().gap_1().child(
            text_field(
                "model-field-filter",
                &m.field_filter,
                editing_filter,
                "Filter fields",
            )
            .on_click(cx.listener(|this, _, _, cx| {
                if let Some(m) = &mut this.model {
                    let text = m.field_filter.clone();
                    m.typing = Some(TextEdit::new(Field::InspectorFilter, text, Filter::Text));
                }
                cx.notify();
            })),
        );
        for g in all {
            let shown: Vec<&Cell> = (g.cells.iter())
                .filter(|c| filter.is_empty() || c.name(m).to_ascii_lowercase().contains(&filter))
                .collect();
            if shown.is_empty() {
                continue;
            }
            // An entry's own fields open; nested configs (materials) folded,
            // a building's twenty would bury the rest. A filter opens all.
            let mut rows = div().flex().flex_col().gap_1();
            for cell in shown {
                rows = rows.child(self.render_cell(cell, cx));
            }
            if g.heading.is_empty() {
                out = out.child(rows);
                continue;
            }
            let id = format!("model-{}", g.key);
            let (heading, open) =
                widgets::collapsible(id, g.heading.clone(), !filter.is_empty(), window, cx);
            out = out.child(heading);
            if open || !filter.is_empty() {
                out = out.child(rows.pl_2());
            }
        }
        out
    }

    fn render_cell(&self, cell: &Cell, cx: &mut Context<Self>) -> Div {
        let m = self.model.as_ref().expect("the grid draws with a model");
        let typing = |leaf: Id| {
            (m.typing.as_ref())
                .filter(|e| e.field == Field::Value(leaf))
                .map(|e| e.display())
        };
        // A click starts typing into a leaf.
        let small = |leaf: Id, cx: &mut Context<Self>| {
            let text = number_text(m.doc.text(leaf).parse().unwrap_or(0.));
            text_field(
                ("model-field", leaf.raw() as usize),
                &text,
                typing(leaf),
                "",
            )
            .flex_1()
            .min_w(px(40.))
            .on_click(edit(leaf, Filter::Number, cx))
        };
        match *cell {
            Cell::Flag(leaf) => {
                let on = m.doc.text(leaf) == "1";
                let name = m.doc.name(leaf).to_owned();
                row().child(label(&name)).child(
                    div()
                        .id(("model-flag", leaf.raw() as usize))
                        .size(px(16.))
                        .rounded(px(2.))
                        .border_1()
                        .border_color(if on {
                            theme::get().accent
                        } else {
                            theme::get().control_edge
                        })
                        .when(on, |d| d.bg(theme::get().accent))
                        .flex()
                        .items_center()
                        .justify_center()
                        .text_size(px(11.))
                        .text_color(theme::get().on_accent)
                        .cursor_pointer()
                        .when(on, |d| d.child("✓"))
                        .on_click(set(leaf, if on { "0" } else { "1" }.to_owned(), cx)),
                )
            }
            Cell::Number(leaf) => {
                let name = m.doc.name(leaf).to_owned();
                let text = m.doc.text(leaf).to_owned();
                let step = step_of(&text);
                let v: f32 = text.parse().unwrap_or(0.);
                row()
                    .child(label(&name))
                    .child(small(leaf, cx))
                    .child(
                        widgets::button("−", false)
                            .id(("model-less", leaf.raw() as usize))
                            .w(px(22.))
                            .on_click(set(leaf, number_text(v - step), cx)),
                    )
                    .child(
                        widgets::button("+", false)
                            .id(("model-more", leaf.raw() as usize))
                            .w(px(22.))
                            .on_click(set(leaf, number_text(v + step), cx)),
                    )
            }
            Cell::Vector(ref name, ref leaves) => {
                let mut r = row().child(label(name));
                for &leaf in leaves {
                    r = r.child(small(leaf, cx));
                }
                r
            }
            Cell::Colour(ref name, leaves) => {
                let [r, g, b] = leaves.map(|l| {
                    let v: f32 = m.doc.text(l).parse().unwrap_or(0.);
                    (v.clamp(0., 1.) * 255.) as u32
                });
                let mut out = row()
                    .child(label(name))
                    .child(widgets::swatch(r << 16 | g << 8 | b));
                for leaf in leaves {
                    out = out.child(small(leaf, cx));
                }
                out
            }
            Cell::Path(leaf, kind) => {
                let name = m.doc.name(leaf).to_owned();
                let text = m.doc.text(leaf).to_owned();
                row()
                    .child(label(&name))
                    .child(
                        div()
                            .id(("model-path", leaf.raw() as usize))
                            .flex_1()
                            .min_w_0()
                            .child(
                                text_field(
                                    ("model-field", leaf.raw() as usize),
                                    &text,
                                    typing(leaf),
                                    "",
                                )
                                .on_click(edit(
                                    leaf,
                                    Filter::Text,
                                    cx,
                                )),
                            )
                            // A file from the Asset browser lands here.
                            .on_drop(cx.listener(move |this, drag: &AssetDrag, _, cx| {
                                if let Some(m) = &mut this.model {
                                    m.set_value(leaf, &kind.cfg_path(&drag.0));
                                }
                                this.model_edited(cx);
                            })),
                    )
                    .child(
                        widgets::button("…", false)
                            .id(("model-pick", leaf.raw() as usize))
                            .w(px(24.))
                            .tooltip(Tooltip::text(
                                format!("Pick {}", kind.noun()),
                                "",
                                "Or drop a file from the Asset browser on the field",
                            ))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.open_model_picker(super::picker::Want::Field(leaf, kind), cx);
                            })),
                    )
            }
            Cell::Text(leaf) => {
                let name = m.doc.name(leaf).to_owned();
                let text = m.doc.text(leaf).to_owned();
                row().child(label(&name)).child(
                    text_field(
                        ("model-field", leaf.raw() as usize),
                        &text,
                        typing(leaf),
                        "",
                    )
                    .flex_1()
                    .on_click(edit(leaf, Filter::Text, cx)),
                )
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{flaggy, number_text, step_of};

    #[test]
    fn fields_read_as_what_they_hold() {
        assert!(flaggy("DIFFUSE_ENABLED") && flaggy("cUseTerrainTinting") && flaggy("Looped"));
        assert!(!flaggy("ShaderID") && !flaggy("cOpacity"));
        assert_eq!(
            (step_of("3"), step_of("0.5"), step_of("12.5")),
            (1., 0.01, 1.)
        );
        // 1.2 as an f32 is 1.20000005 widened; it reads back as typed.
        assert_eq!(number_text(1.2), "1.2");
    }
}
