// Owns the widget library pane: the kinds of widget a screen can hold and
// the game's widget templates, searched by name. A double-click adds one
// where Add would; a drag drops it on the canvas (into the group there, its
// pivot where it lands) or on a widget tree row.
use super::GENERATED;
use super::files::{Index, spaced};
use super::hierarchy::{NewWidget, Place};
use crate::editor::{Editor, empty_pane};
use anno_formats::phx::Asset;
use anyhow::{Context as _, Result};
use gpui_editor_kit::{
    field::{EditEvent, Filter, TextEdit, text_field},
    gpui::{prelude::*, *},
    icon::Icon,
    theme,
    tree::{self, Tree},
    widgets::{Disclosure, DragLabel, Tooltip},
};
use std::sync::Arc;

/// What a library row adds.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Pick {
    Kind(NewWidget),
    /// A template, by its UI index entry.
    Template(usize),
}

/// Drag payload: a library row on its way to the canvas or the tree.
#[derive(Clone, Copy)]
pub struct LibraryDrag(pub Pick);

/// Template rows of one group past this many wait for a search: a list of
/// 2,000 names is a scroll nobody makes.
const SHOWN: usize = 400;

#[derive(Clone, Copy, PartialEq)]
enum Row {
    /// Into `Library::headings`.
    Heading(usize),
    Kind(NewWidget),
    Template(usize),
    More(usize),
}

/// A group's heading: its name, how many it holds, and what its widget
/// type is for (the type's own description).
#[derive(Clone)]
struct Heading {
    name: String,
    count: usize,
    tip: String,
    open: bool,
}

const OTHER: &str = "Other templates";

/// The pane's search, its rows, and the row picked last.
#[derive(Default)]
pub struct Library {
    query: String,
    editing: Option<TextEdit<()>>,
    rows: Arc<Vec<Row>>,
    headings: Arc<Vec<Heading>>,
    picked: Option<Pick>,
    /// Groups folded or unfolded against their default (Other templates,
    /// two thousand of them, starts folded; a search shows everything).
    flipped: std::collections::BTreeSet<String>,
    tree: Tree<usize>,
}

/// Widget types as a modder looks for them, in this order; the rest after
/// them by name.
const TYPE_ORDER: [(&str, &str); 13] = [
    ("Button", "Buttons"),
    ("Checkbox", "Checkboxes"),
    ("Slider", "Sliders"),
    ("Dropdown", "Dropdowns"),
    ("InputText", "Text inputs"),
    ("List", "Lists"),
    ("Grid", "Grids"),
    ("ScrollPanel", "Scroll panels"),
    ("ScrollBar", "Scroll bars"),
    ("SelectionWheel", "Selection wheels"),
    ("DynamicWidget", "Repeated items"),
    ("DynamicInteractiveWidget", "Repeated interactive items"),
    ("InteractiveWidget", "Focusable widgets"),
];

/// A template's group in the library: the kind of widget its root is, and
/// what that kind is for.
#[derive(Clone, Debug, PartialEq)]
pub struct Group {
    pub name: String,
    pub tip: String,
}

/// Groups `index`'s templates by the kind of widget their root is, once
/// per index (a search then only filters). A root whose type chain
/// reaches a Phoenix widget type other than the generic InteractiveWidget
/// takes that type's group (Buttons, Grids, Scroll panels). The game
/// also builds types of its own straight on InteractiveWidget, with no
/// name (its sliders share one): such a type is named after the word its
/// templates share, Sliders (round 15: they all landed under Focusable
/// widgets and there was no Sliders group).
pub(super) fn group_templates(index: &Index) -> Vec<Option<Group>> {
    use anno_formats::phx::{Guid, widget_type_info};
    let by_file: std::collections::HashMap<&str, usize> = index
        .entries
        .iter()
        .enumerate()
        .map(|(i, e)| (e.file.as_str(), i))
        .collect();
    // The Phoenix type a widget type asset reaches (the bound stops a cycle).
    let phoenix = |start: &str| {
        let mut at = start;
        for _ in 0..8 {
            if let Some(t) = Guid::parse(at).and_then(widget_type_info) {
                return Some(t);
            }
            at = index
                .entries
                .get(*by_file.get(at)?)?
                .widget_type
                .as_deref()?;
        }
        None
    };
    // Templates by their root's immediate type, for the shared word.
    let mut members: std::collections::HashMap<&str, Vec<&str>> = Default::default();
    for e in &index.entries {
        if let (true, Some(t)) = (e.kind == "WI", e.widget_type.as_deref()) {
            members.entry(t).or_default().push(&e.name);
        }
    }
    let shared_word = |names: &[&str]| {
        let mut count: std::collections::HashMap<String, usize> = Default::default();
        for n in names {
            let mut seen = std::collections::HashSet::new();
            for w in words(n) {
                if w.len() >= 4 && seen.insert(w.clone()) {
                    *count.entry(w).or_default() += 1;
                }
            }
        }
        count
            .into_iter()
            .filter(|&(_, c)| c * 2 > names.len() && names.len() >= 2)
            .max_by_key(|(w, c)| (*c, w.len()))
            .map(|(w, _)| w)
    };
    index
        .entries
        .iter()
        .map(|e| {
            if e.kind != "WI" {
                return None;
            }
            let own = e.widget_type.as_deref()?;
            let t = phoenix(own);
            match t {
                Some(t) if t.name != "InteractiveWidget" => {
                    let name = TYPE_ORDER
                        .iter()
                        .find(|(n, _)| *n == t.name)
                        .map_or_else(|| spaced(&t.name), |(_, label)| (*label).to_owned());
                    Some(Group {
                        name,
                        tip: t.description,
                    })
                }
                _ => {
                    let word = shared_word(&members[own])?;
                    let mut name = word.clone();
                    name.push_str(if name.ends_with(['s', 'x']) {
                        "es"
                    } else {
                        "s"
                    });
                    Some(Group {
                        name,
                        tip: format!(
                            "A widget type of the game's own{}, named here after what its \
                             templates are called",
                            t.map(|t| format!(", built on {}", t.name))
                                .unwrap_or_default()
                        ),
                    })
                }
            }
        })
        .collect()
}

/// The words of a name, capitalised: `Skinned_Slider_DS` → Skinned,
/// Slider, Ds; `SliderH_Filled` → Slider, H, Filled.
fn words(name: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut word = String::new();
    let mut prev_lower = false;
    for c in name.chars() {
        if !c.is_ascii_alphanumeric() || (c.is_ascii_uppercase() && prev_lower) {
            if !word.is_empty() {
                out.push(std::mem::take(&mut word));
            }
        }
        if c.is_ascii_alphanumeric() {
            word.push(if word.is_empty() {
                c.to_ascii_uppercase()
            } else {
                c.to_ascii_lowercase()
            });
        }
        prev_lower = c.is_ascii_lowercase() || c.is_ascii_digit();
    }
    if !word.is_empty() {
        out.push(word);
    }
    out
}

impl Library {
    /// The rows for the query: the kinds whose name matches, then the
    /// templates a shipped screen shows (they can be copied from there),
    /// grouped by the widget type their root is (Buttons, Sliders, Lists…),
    /// then the rest, by name within each.
    pub(super) fn refresh(&mut self, index: Option<&Index>) {
        let words: Vec<String> = self
            .query
            .split_whitespace()
            .map(str::to_lowercase)
            .collect();
        let hit = |hay: &str| {
            let hay = hay.to_lowercase();
            words.iter().all(|w| hay.contains(w))
        };
        let mut rows = Vec::new();
        let mut headings = Vec::new();
        let searching = !words.is_empty();
        let flipped = &self.flipped;
        let mut group = |name: String, tip: String, members: Vec<Row>, rows: &mut Vec<Row>| {
            if members.is_empty() {
                return;
            }
            rows.push(Row::Heading(headings.len()));
            let open = searching || ((name != OTHER) != flipped.contains(&name));
            headings.push(Heading {
                name,
                count: members.len(),
                tip,
                open,
            });
            if !open {
                return;
            }
            let more = members.len().saturating_sub(SHOWN);
            rows.extend(members.into_iter().take(SHOWN));
            if more > 0 {
                rows.push(Row::More(more));
            }
        };
        let kinds: Vec<Row> = NewWidget::ALL
            .into_iter()
            .filter(|k| hit(k.label().0))
            .map(Row::Kind)
            .collect();
        group(
            "Widgets".into(),
            "The kinds of widget a screen holds, each a plain shipped one to start from".into(),
            kinds,
            &mut rows,
        );
        if let Some(index) = index {
            // Group name → (what it is for, templates); a search also
            // matches the group's name ("slider" finds every slider).
            let mut typed: std::collections::BTreeMap<String, (String, Vec<Row>)> =
                Default::default();
            let mut other = Vec::new();
            for (i, e) in index.entries.iter().enumerate() {
                if e.kind != "WI" || e.example.is_none() {
                    continue;
                }
                let group = index.groups.get(i).cloned().flatten();
                let group_name = group.as_ref().map(|g| g.name.as_str()).unwrap_or_default();
                if !hit(&format!("{} {} {group_name}", e.name, e.file)) {
                    continue;
                }
                match group {
                    Some(g) => typed
                        .entry(g.name)
                        .or_insert_with(|| (g.tip, Vec::new()))
                        .1
                        .push(Row::Template(i)),
                    None => other.push(Row::Template(i)),
                }
            }
            for (_, label) in TYPE_ORDER {
                if let Some((tip, members)) = typed.remove(label) {
                    group(label.into(), tip, members, &mut rows);
                }
            }
            for (name, (tip, members)) in typed {
                group(name, tip, members, &mut rows);
            }
            group(
                OTHER.into(),
                "Templates whose root has no widget type: panels, icons, layouts".into(),
                other,
                &mut rows,
            );
        }
        self.rows = Arc::new(rows);
        self.headings = Arc::new(headings);
    }

    /// Folds or unfolds group `name`. Folding while searching would hide
    /// hits the search asked for, so it waits for the search to clear.
    fn fold(&mut self, name: &str, index: Option<&Index>) {
        if !self.flipped.remove(name) {
            self.flipped.insert(name.to_owned());
        }
        self.refresh(index);
    }

    pub(crate) fn typing(&mut self) -> Option<&mut dyn gpui_editor_kit::field::InputBuffer> {
        self.editing
            .as_mut()
            .map(|e| e as &mut dyn gpui_editor_kit::field::InputBuffer)
    }

    pub(super) fn stop_typing(&mut self) {
        self.editing = None;
    }

    /// Typing into the search field; false when it is not focused.
    pub(super) fn key_down(&mut self, event: &KeyDownEvent, index: Option<&Index>) -> bool {
        let Some(edit) = &mut self.editing else {
            return false;
        };
        match edit.key_down(event) {
            EditEvent::Commit | EditEvent::Cancel => self.editing = None,
            EditEvent::Changed => {
                self.query = edit.text.clone();
                self.refresh(index);
            }
            EditEvent::Ignored if event.keystroke.modifiers.control => return false,
            EditEvent::Ignored => {}
        }
        true
    }

    #[cfg(test)]
    pub(crate) fn search(&mut self, query: &str, index: &Index) -> Vec<String> {
        self.query = query.into();
        self.refresh(Some(index));
        self.rows
            .iter()
            .filter_map(|r| match *r {
                Row::Kind(k) => Some(k.label().0.to_owned()),
                Row::Template(e) => Some(index.entries[e].name.clone()),
                Row::Heading(h) => Some(format!("# {}", self.headings[h].name)),
                _ => None,
            })
            .collect()
    }
}

/// A widget name from a template's: letters, digits and underscores.
fn widget_name(template: &str) -> String {
    let name: String = template
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '_')
        .collect();
    if name.is_empty() {
        "NewTemplate".into()
    } else {
        name
    }
}

impl super::UiEditor {
    /// Adds what library row `pick` stands for at `place`, one undo step: a
    /// kind as Add makes it, a template as a copy of the container a
    /// shipped screen shows it with (the plainest one, `Entry::example`).
    pub(crate) fn add_from_library(
        &mut self,
        pick: Pick,
        index: &Index,
        place: Place,
    ) -> Result<String> {
        match pick {
            Pick::Kind(kind) => self.add_kind(kind, place),
            Pick::Template(e) => {
                let entry = index.entries.get(e).context("no such template")?;
                let (file, w) = entry
                    .example
                    .as_ref()
                    .context("No shipped screen shows this template to copy")?;
                let asset = Asset::parse(self.install.read(&format!("{GENERATED}{file}"))?)?;
                let copy = asset.widget_copy(&asset.widgets(), *w as usize)?;
                self.add_copy(&copy, &widget_name(&entry.name), None, place)
            }
        }
    }
}

impl Editor {
    /// Adds library row `pick` at `place` and says what happened.
    pub(crate) fn add_library_pick(&mut self, pick: Pick, place: Place) {
        let (Some(ui), Some(index)) = (&mut self.ui, self.ui_find.index.clone()) else {
            return;
        };
        match ui.add_from_library(pick, &index, place) {
            Ok(m) => self.message(m, false),
            Err(e) => self.message(format!("{e:#}"), true),
        }
    }

    pub fn render_ui_library(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        self.ensure_ui_index(cx);
        let Some(index) = self.ui_find.index.clone() else {
            return empty_pane("Indexing UI files…");
        };
        if self.ui.is_none() {
            return crate::editor::pane_note("Open a screen to add widgets to it.");
        }
        let lib = &self.ui_find.library;
        let search = text_field(
            "ui-library-search",
            &lib.query,
            lib.editing.as_ref().map(|e| e.display()),
            "Search widgets and templates…",
        )
        .flex_1()
        .on_click(cx.listener(|this, _, window, cx| {
            this.focus.focus(window, cx);
            // One field types at a time; the pane keys go to stays.
            this.ui_find.stop_typing();
            if let Some(ui) = &mut this.ui {
                let keys = ui.keys;
                ui.take_keys(keys);
            }
            let lib = &mut this.ui_find.library;
            lib.editing = Some(TextEdit::new((), lib.query.clone(), Filter::Text));
            cx.notify();
        }));
        let rows = lib.rows.clone();
        let headings = lib.headings.clone();
        let picked = lib.picked;
        let (item_rows, item_index) = (rows.clone(), index.clone());
        let (click_rows, glyph_rows, drag_rows, drag_index) =
            (rows.clone(), rows.clone(), rows.clone(), index.clone());
        let (tip_headings, fold_headings) = (headings.clone(), headings.clone());
        let pick_of = |row: Row| match row {
            Row::Kind(k) => Some(Pick::Kind(k)),
            Row::Template(e) => Some(Pick::Template(e)),
            _ => None,
        };
        let list = tree::List::new(
            "ui-library-rows",
            rows.len(),
            move |i| {
                let (label, detail, depth) = match item_rows[i] {
                    Row::Heading(h) => (
                        headings[h].name.clone(),
                        crate::project::count(headings[h].count),
                        0,
                    ),
                    Row::Kind(k) => (k.label().0.to_owned(), String::new(), 1),
                    Row::Template(e) => (spaced(&item_index.entries[e].name), String::new(), 1),
                    Row::More(n) => (format!("{n} more · search to narrow"), String::new(), 1),
                };
                tree::Item {
                    depth,
                    disclosure: match item_rows[i] {
                        Row::Heading(h) if headings[h].open => Disclosure::Expanded,
                        Row::Heading(_) => Disclosure::Collapsed,
                        _ => Disclosure::Leaf,
                    },
                    label: label.into(),
                    detail: detail.into(),
                    selected: pick_of(item_rows[i]).is_some_and(|p| Some(p) == picked),
                }
            },
            move |this: &mut Editor, i, event, _| {
                if let Row::Heading(h) = click_rows[i] {
                    let index = this.ui_find.index.clone();
                    this.ui_find
                        .library
                        .fold(&fold_headings[h].name, index.as_deref());
                    return;
                }
                let Some(pick) = pick_of(click_rows[i]) else {
                    return;
                };
                this.ui_find.library.picked = Some(pick);
                // A double-click adds it where Add would.
                if event.click_count() >= 2 {
                    this.add_library_pick(pick, Place::Selection);
                }
            },
        )
        .glyph(move |i| match glyph_rows[i] {
            Row::Kind(NewWidget::Image) => Some(Icon::IMAGE),
            Row::Kind(NewWidget::Label) => Some(Icon::TEXT),
            Row::Kind(NewWidget::Group) => Some(Icon::FOLDER),
            Row::Kind(_) => Some(Icon::LAYOUT),
            Row::Template(_) => Some(Icon::CUBE),
            _ => None,
        })
        // Rows drag onto the canvas or a widget tree row; each says what
        // it is on hover.
        .decorate(move |i, row| {
            // A group says what its widget type is for.
            if let Row::Heading(h) = drag_rows[i] {
                let h = &tip_headings[h];
                return row.tooltip(Tooltip::text(h.name.clone(), "", h.tip.clone()));
            }
            let Some(pick) = pick_of(drag_rows[i]) else {
                return row;
            };
            let (label, tip): (SharedString, String) = match pick {
                Pick::Kind(k) => (k.label().0.into(), k.label().1.into()),
                Pick::Template(e) => {
                    let entry = &drag_index.entries[e];
                    (
                        spaced(&entry.name).into(),
                        format!(
                            "A template: {} widgets, shown by {} files",
                            entry.widgets, entry.used_by
                        ),
                    )
                }
            };
            let drag = label.clone();
            row.on_drag(LibraryDrag(pick), move |_, _, _, cx| {
                cx.new(|_| DragLabel(drag.clone()))
            })
            .tooltip(Tooltip::text(
                label,
                "",
                format!("{tip}. Drag onto the canvas or the widget tree; double-click adds it"),
            ))
        })
        .render(&lib.tree, window, cx)
        .flex_1()
        .min_h_0();
        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(theme::get().panel)
            .child(div().flex().p_1().child(search))
            .child(list)
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::{Library, Pick};
    use crate::ui::files::{Entry, Index};
    use crate::ui::hierarchy::{NewWidget, Place};
    use crate::ui::test_support::{ICON_ROW, node, one_template, open, reopen, select};

    // Needs the game: the library finds kinds and templates by any word of
    // their name, and lists only templates a shipped screen shows.
    // Templates group by the widget type their root is, through the game's
    // own types built on a Phoenix one (a game button type, then Button),
    // and a search for the type finds them all.
    #[test]
    fn the_library_finds_and_groups_templates_by_name() {
        let Some(ui) = open(ICON_ROW) else {
            return;
        };
        let mut index = one_template(&ui, "Text");
        index.entries.push(Entry {
            example: None,
            name: "UnshownWT".into(),
            ..index.entries[0].clone()
        });
        let mut lib = Library::default();
        assert_eq!(
            lib.search("", &index),
            [
                "# Widgets",
                "Image",
                "Label",
                "Group",
                "Video",
                "# Other templates"
            ],
            "the two thousand untyped templates start folded"
        );
        assert_eq!(
            lib.search("button", &index),
            ["# Other templates", "AnnoButtonWT"]
        );
        assert_eq!(lib.search("lab", &index), ["# Widgets", "Label"]);
        assert!(lib.search("unshown", &index).is_empty());

        let mut index = one_template(&ui, "Text");
        const BUTTON: &str = "4fdbe42a-f548-4eb8-818e-fcbafc9ad83e";
        const GAME_BUTTON: &str = "00000000-0000-0000-0000-00000000b077";
        index.entries[0].widget_type = Some(GAME_BUTTON.into());
        index.entries.push(Entry {
            file: GAME_BUTTON.into(),
            kind: "BE".into(),
            name: "GameButtonType".into(),
            widgets: 0,
            used_by: 1,
            example: None,
            widget_type: Some(BUTTON.into()),
        });
        index.entries.push(Entry {
            name: "PlainPanel".into(),
            widget_type: None,
            ..index.entries[0].clone()
        });
        // Grouped once it holds them all, as a loaded index is.
        let index = Index::of(index.entries);
        let mut lib = Library::default();
        let all = lib.search("", &index);
        assert_eq!(
            &all[5..],
            ["# Buttons", "AnnoButtonWT", "# Other templates"]
        );
        assert_eq!(lib.search("button", &index), ["# Buttons", "AnnoButtonWT"]);
        assert_eq!(
            lib.search("panel", &index),
            ["# Other templates", "PlainPanel"]
        );
        // A heading folds its group, and unfolds it again; a search shows
        // what it finds either way.
        lib.fold("Buttons", Some(&index));
        assert_eq!(
            &lib.search("", &index)[5..],
            ["# Buttons", "# Other templates"]
        );
        assert_eq!(lib.search("button", &index), ["# Buttons", "AnnoButtonWT"]);
        lib.fold("Other templates", Some(&index));
        assert!(lib.search("", &index).contains(&"PlainPanel".to_owned()));
        // The game's sliders share an unnamed type built straight on
        // InteractiveWidget: named after what they are called (round 15:
        // no Sliders group).
        const INTERACTIVE: &str = "198b2ec0-769e-4052-93ca-5408467849a9";
        const GAME_SLIDER: &str = "00000000-0000-0000-0000-0000000051d0";
        let template = index.entries[0].clone();
        let mut entries = index.entries.clone();
        entries.push(Entry {
            file: GAME_SLIDER.into(),
            kind: "BE".into(),
            name: GAME_SLIDER.into(),
            widgets: 0,
            used_by: 3,
            example: None,
            widget_type: Some(INTERACTIVE.into()),
        });
        for name in ["Slider_DS", "SliderH_Filled", "Skinned_Slider_DS"] {
            entries.push(Entry {
                name: name.into(),
                widget_type: Some(GAME_SLIDER.into()),
                ..template.clone()
            });
        }
        let index = Index::of(entries);
        assert_eq!(
            lib.search("slider", &index),
            [
                "# Sliders",
                "Slider_DS",
                "SliderH_Filled",
                "Skinned_Slider_DS"
            ]
        );
    }

    // Needs the game: every kind the library lists can be added, as one
    // undo step, from a plain shipped widget of its class. A new Image
    // starts untinted: white, whatever palette entry the shipped widget it
    // is copied from had (round 15: a black palette tint stayed on it,
    // saved and reopened).
    #[test]
    fn every_kind_in_the_library_adds() {
        let Some(mut ui) = open(ICON_ROW) else {
            return;
        };
        let index = Index::of(Vec::new());
        for kind in NewWidget::ALL {
            let before = ui.asset.widgets().len();
            let said = ui
                .add_from_library(Pick::Kind(kind), &index, Place::Selection)
                .unwrap_or_else(|e| panic!("{kind:?}: {e:#}"));
            assert!(said.starts_with("Added "), "{said}");
            assert_eq!(ui.asset.widgets().len(), before + 1, "{kind:?}");
            assert!(ui.undo(false));
            assert_eq!(ui.asset.widgets().len(), before, "{kind:?} undone");
        }

        select(&mut ui, "Icon");
        let image = Pick::Kind(NewWidget::Image);
        ui.add_from_library(image, &index, Place::Selection)
            .unwrap();
        let w = ui.nodes[ui.canvas.selected.unwrap()].widget;
        let tint = |a: &anno_formats::phx::Asset| {
            a.widgets()[w]
                .props
                .iter()
                .find(|p| p.name == Some("ColorTint"))
                .map(|p| p.value.clone())
        };
        let white = Some(anno_formats::phx::Value::Color([255; 3]));
        assert_eq!(tint(&ui.asset), white);
        assert_eq!(tint(&reopen(&ui.asset)), white);
    }

    // Needs the game: a template dropped on the canvas goes into the group
    // drawn there, as a copy of the container a shipped screen shows it
    // with, its pivot where it was dropped; one undo takes it out. Dropped
    // on a tree row of a group, it goes into that group.
    #[test]
    fn a_dropped_template_lands_where_it_is_dropped() {
        let Some(mut ui) = open(ICON_ROW) else {
            return;
        };
        let index = one_template(&ui, "Text");
        let group = node(&ui, "Icon_Group");
        let r = ui.nodes[group].rect;
        let at = [r[0] + r[2] / 2. + 1., r[1] + r[3] / 2. + 1.];
        let before = ui.asset.widgets().len();
        ui.add_from_library(Pick::Template(0), &index, Place::Canvas(at))
            .unwrap();
        let added = ui.canvas.selected.expect("the new widget is selected");
        let widgets = ui.asset.widgets();
        let w = ui.nodes[added].widget;
        assert_eq!(
            ui.asset
                .container_template(&widgets[w])
                .map(|r| r.id.to_string()),
            Some(index.entries[0].file.clone())
        );
        assert_eq!(&*ui.nodes[added].name, "AnnoButtonWT");
        let parent = widgets[w].parent.unwrap();
        assert_eq!(widgets[parent].name.as_deref(), Some("Icon_Group"));
        let pivot = ui.frames[added].pivot_world();
        assert!(
            (pivot[0] - at[0]).abs() < 0.5 && (pivot[1] - at[1]).abs() < 0.5,
            "{pivot:?} vs {at:?}"
        );
        assert!(ui.undo(false));
        assert_eq!(ui.asset.widgets().len(), before, "one step");
        // Onto the group's tree row.
        ui.add_from_library(Pick::Template(0), &index, Place::Tree(group))
            .unwrap();
        let added = ui.canvas.selected.unwrap();
        let widgets = ui.asset.widgets();
        let parent = widgets[ui.nodes[added].widget].parent.unwrap();
        assert_eq!(widgets[parent].name.as_deref(), Some("Icon_Group"));
    }
}
