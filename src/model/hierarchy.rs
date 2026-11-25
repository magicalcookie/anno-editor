// Owns the Model workspace's hierarchy pane: the cfg's lists and entries
// as a tree, a skinned part's joints under it with the entries that hang on
// them, its search, row menu and keys, and moving entries by dragging them
// along their list or onto a joint.

use super::{Field, LISTS, ModelEditor, playback::Source};
use crate::editor::Editor;
use anno_formats::xml::Id;
use gpui_editor_kit::{
    field::{EditEvent, Filter, TextEdit, clear_search, text_field},
    gpui::{prelude::*, *},
    icon::Icon,
    theme,
    tree::{self, Item},
    widgets::{self, DragLabel},
};
use std::rc::Rc;

/// A hierarchy row: a list of the MAIN config, one of its entries, a
/// part's skeleton, or one joint of it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Row {
    List(Id),
    Entry(Id),
    Joints(Id),
    Joint(Id, u16),
}

/// The hierarchy's own state beside the doc: its search, and the row whose
/// actions are open.
#[derive(Clone, Debug, Default)]
pub struct Pane {
    /// Narrows entries and joints to those whose name or file holds it,
    /// case-insensitive.
    pub filter: String,
    /// The row whose action menu is open, and where it opened (window px).
    menu: Option<(Row, Point<Pixels>)>,
}

/// A search stops adding rows after this many matches: one letter over a
/// city building's hundreds of props and a unit's two hundred joints.
const MATCH_LIMIT: usize = 400;

/// Row glyphs the kit lacks, on its 24-unit grid.
const PARTICLE: Icon = Icon(&[
    &[[12., 3.], [12., 9.]],
    &[[12., 15.], [12., 21.]],
    &[[3., 12.], [9., 12.]],
    &[[15., 12.], [21., 12.]],
    &[[6., 6.], [8., 8.]],
    &[[16., 16.], [18., 18.]],
    &[[18., 6.], [16., 8.]],
    &[[6., 18.], [8., 16.]],
]);
const LIGHT: Icon = Icon(&[
    &[
        [16., 12.],
        [14.8, 14.8],
        [12., 16.],
        [9.2, 14.8],
        [8., 12.],
        [9.2, 9.2],
        [12., 8.],
        [14.8, 9.2],
        [16., 12.],
    ],
    &[[12., 2.], [12., 5.]],
    &[[12., 19.], [12., 22.]],
    &[[2., 12.], [5., 12.]],
    &[[19., 12.], [22., 12.]],
    &[[5., 5.], [7., 7.]],
    &[[17., 17.], [19., 19.]],
    &[[19., 5.], [17., 7.]],
    &[[5., 19.], [7., 17.]],
]);
/// A film frame with a play mark.
const SEQUENCE: Icon = Icon(&[
    &[[3., 5.], [21., 5.], [21., 19.], [3., 19.], [3., 5.]],
    &[[10., 9.], [15., 12.], [10., 15.], [10., 9.]],
]);
/// A banner on its pole.
const CLOTH: Icon = Icon(&[
    &[[5., 3.], [5., 21.]],
    &[
        [5., 4.],
        [10., 6.],
        [15., 4.],
        [20., 6.],
        [20., 13.],
        [15., 11.],
        [10., 13.],
        [5., 11.],
    ],
]);
/// A bone: a shaft with split ends.
const JOINT: Icon = Icon(&[
    &[[7., 17.], [17., 7.]],
    &[[4., 16.], [7., 17.], [8., 20.]],
    &[[16., 4.], [17., 7.], [20., 8.]],
]);
/// A placed cfg: one model set down over another, unlike a mesh's cube.
const PROP: Icon = Icon(&[
    &[[9., 4.], [20., 4.], [20., 15.]],
    &[[4., 9.], [15., 9.], [15., 20.], [4., 20.], [4., 9.]],
]);

/// The glyph and the one word for what a list's entries are.
fn kind(list: &str) -> (Icon, &'static str) {
    match list {
        "Models" => (Icon::CUBE, "mesh"),
        "InstancingModels" => (Icon::CUBE, "stem"),
        "InstancingBBClusters" => (Icon::FOREST, "leaves"),
        "Files" => (PROP, "prop"),
        "Decals" | "TerrainDecals" => (Icon::IMAGE, "decal"),
        "Particles" => (PARTICLE, "particle"),
        "Effects" => (PARTICLE, "effect"),
        "Lights" => (LIGHT, "light"),
        "Sequences" => (SEQUENCE, "sequence"),
        "Clothes" => (CLOTH, "cloth"),
        _ => (Icon::FILE, "entry"),
    }
}

fn glyph(m: &ModelEditor, row: Row) -> Icon {
    match row {
        Row::List(l) => kind(m.doc.name(l)).0,
        Row::Entry(e) => kind(m.list_of(e).unwrap_or("")).0,
        Row::Joints(_) => Icon::TREE,
        Row::Joint(..) => JOINT,
    }
}

/// What sits under a row. Entries that hang on a joint show under their
/// host part as well as in their list; a part with a skeleton has its
/// joints as a tree.
fn row_children(m: &ModelEditor, row: Row) -> Vec<Row> {
    let doc = &m.doc;
    let skeleton = |part: Id| {
        (m.model.parts.iter().find(|p| p.element == part))
            .and_then(|p| p.mesh.as_ref())
            .and_then(|r| r.skeleton.as_ref())
    };
    match row {
        Row::List(l) => (doc.children(l))
            .filter(|&c| doc.name(c) == "Config")
            .map(Row::Entry)
            .collect(),
        Row::Entry(e) => {
            let Some(k) = m.model.parts.iter().position(|p| p.element == e) else {
                return Vec::new();
            };
            let m2 = &m.model;
            let hung = (m2.parts.iter().map(|p| (p.element, p.rig)))
                .chain(m2.props.iter().map(|p| (p.element, p.rig)))
                .chain(m2.lights.iter().map(|p| (p.element, p.rig)))
                .chain(m2.particles.iter().map(|p| (p.element, p.rig)))
                .filter(|(c, r)| *c != e && r.link.is_some_and(|l| l.model as usize == k))
                .map(|(c, _)| Row::Entry(c));
            let mut rows: Vec<Row> = hung.collect();
            if skeleton(e).is_some_and(|s| !s.is_empty()) {
                rows.push(Row::Joints(e));
            }
            rows
        }
        Row::Joints(part) => skeleton(part).map_or_else(Vec::new, |s| {
            (0..s.len())
                .filter(|&j| s.parents[j] as usize >= s.len())
                .map(|j| Row::Joint(part, j as u16))
                .collect()
        }),
        Row::Joint(part, j) => skeleton(part).map_or_else(Vec::new, |s| {
            (0..s.len())
                .filter(|&c| s.parents[c] == j)
                .map(|c| Row::Joint(part, c as u16))
                .collect()
        }),
    }
}

/// A row's name and the muted note right of it: a list's count, where an
/// entry hangs, a sequence's length, else an entry's file or its kind.
pub(super) fn row_label(m: &ModelEditor, row: Row) -> (String, String) {
    let doc = &m.doc;
    match row {
        Row::List(l) => {
            let name = doc.name(l);
            let label = (LISTS.iter().find(|(n, _)| *n == name)).map_or(name, |(_, t)| t);
            let count = doc.children(l).filter(|&c| doc.name(c) == "Config").count();
            (label.to_owned(), count.to_string())
        }
        Row::Entry(e) => {
            let label = m.label(e);
            if let Some(j) = m.joint_of(e) {
                return (label, format!("on {j}"));
            }
            let list = m.list_of(e).unwrap_or("");
            if list == "Sequences" {
                let s = m.model.sequences.iter().find(|s| s.element == e);
                let detail = s.map_or_else(String::new, |s| {
                    let once = if s.looped { "" } else { " · once" };
                    format!("{:.2} s{once}", s.length())
                });
                return (label, detail);
            }
            // The file when the name is something else, so two meshes
            // named alike tell apart; else what the entry is.
            let file = (doc.child(e, "FileName"))
                .map(|f| doc.text(f))
                .and_then(|f| f.rsplit(['/', '\\']).next())
                .map(|f| f.split('.').next().unwrap_or(f))
                .filter(|f| !f.is_empty() && !label.starts_with(f));
            let detail = file.map_or_else(|| kind(list).1.to_owned(), str::to_owned);
            (label, detail)
        }
        Row::Joints(part) => {
            let n = (m.model.parts.iter().find(|p| p.element == part))
                .and_then(|p| p.mesh.as_ref()?.skeleton.as_ref())
                .map_or(0, |s| s.len());
            ("Joints".to_owned(), n.to_string())
        }
        Row::Joint(part, j) => {
            let name = (m.model.parts.iter().find(|p| p.element == part))
                .and_then(|p| p.mesh.as_ref()?.skeleton.as_ref())
                .map(|s| s.name(j as usize).to_owned())
                .unwrap_or_default();
            (name, String::new())
        }
    }
}

impl ModelEditor {
    /// The joint `entry` hangs on, by name.
    fn joint_of(&self, entry: Id) -> Option<String> {
        let l = self.rig(entry)?.link?;
        let host = self.model.parts.get(l.model as usize)?;
        let s = host.mesh.as_ref()?.skeleton.as_ref()?;
        Some(s.name(s.find_hash(l.bone)?).to_owned())
    }

    /// Types into `entry`'s `Name` leaf, for F2 and the row menu's Rename;
    /// Enter commits it as one edit. Sequences have none: their id names
    /// them.
    pub fn start_rename(&mut self, entry: Id) -> bool {
        let Some(leaf) = self.doc.child(entry, "Name") else {
            self.message = Some(("This entry has no Name to rename".into(), false));
            return false;
        };
        let text = self.doc.text(leaf).to_owned();
        self.typing = Some(TextEdit::new(Field::Value(leaf), text, Filter::Text));
        true
    }

    /// The cfg a `Files` entry places.
    pub fn prop_cfg(&self, entry: Id) -> Option<String> {
        (self.model.props.iter())
            .find(|p| p.element == entry)
            .map(|p| p.cfg.clone())
    }

    /// Whether `part` hangs on `entry`, directly or through other parts:
    /// hanging `entry` on one of `part`'s joints would close a loop.
    fn hangs_on(&self, part: Id, entry: Id) -> bool {
        let mut at = part;
        // A chain longer than the cfg has parts is a loop already.
        for _ in 0..=self.model.parts.len() {
            if at == entry {
                return true;
            }
            let link = self.rig(at).and_then(|r| r.link);
            let Some(host) = link.and_then(|l| self.model.parts.get(l.model as usize)) else {
                return false;
            };
            at = host.element;
        }
        true
    }

    /// `entry` dropped on `row` in the hierarchy: onto an entry of its own
    /// list it takes that entry's place, onto its list's header it goes
    /// last, onto a joint it hangs there. Anything else is not a drop.
    pub fn drop_entry(&mut self, entry: Id, row: Row) {
        let list = self.doc.parent(entry);
        match row {
            Row::Entry(target) if target != entry && self.doc.parent(target) == list => {
                // Dragged down, it lands after the row; up, before it: the
                // row it is dropped on is where it ends up either way.
                let siblings = self.entries(self.list_of(entry).unwrap_or(""));
                let from = siblings.iter().position(|&e| e == entry);
                let to = siblings.iter().position(|&e| e == target);
                let before = match (from, to) {
                    (Some(f), Some(t)) if f < t => siblings.get(t + 1).copied(),
                    _ => Some(target),
                };
                self.reorder(entry, before);
            }
            Row::List(l) if Some(l) == list => self.reorder(entry, None),
            Row::Joint(part, j) if self.placement(entry).is_some() => {
                if self.hangs_on(part, entry) {
                    self.message = Some((
                        "Not hung: that part hangs on this entry already".into(),
                        true,
                    ));
                    return;
                }
                let link = self.link_to(part, j);
                self.attach(entry, link);
            }
            _ => {}
        }
    }

    /// After entries of `list` moved, came or went: references by place
    /// (meshes', particles') follow each entry of `old`, the list as it was,
    /// to where it is now. A reference to an entry gone stays as it was.
    /// Duplicating or removing a mesh shifts the ones after it alike.
    pub(super) fn follow_places(&mut self, list: &str, old: &[Id]) {
        if !matches!(list, "Models" | "Particles") {
            return;
        }
        let new = self.entries(list);
        let to: Vec<usize> = (old.iter().enumerate())
            .map(|(k, e)| new.iter().position(|n| n == e).unwrap_or(k))
            .collect();
        self.renumber(list, &to);
    }

    /// Rewrites every reference to an entry of `list` ("Models" or
    /// "Particles") from its old place `k` to `to[k]`: track elements that
    /// play a mesh's animation or run a particle, and bone links. A sound's
    /// `ModelID` is not a place in the list and stays.
    fn renumber(&mut self, list: &str, to: &[usize]) {
        let Some(main) = self.main() else { return };
        let doc = &self.doc;
        let text = |c: Id, name: &str| doc.child(c, name).map(|t| doc.text(t));
        let mut refs = Vec::new();
        let mut stack = vec![main];
        while let Some(at) = stack.pop() {
            let field = match (doc.name(at), list) {
                ("TrackElement", "Models") if text(at, "Type") == Some("0") => "ModelID",
                ("TrackElement", "Particles") if text(at, "Type") == Some("1") => "ParticleID",
                ("Config", "Models") if text(at, "ConfigType") == Some("BONELINK_TRANSFORM") => {
                    "ModelID"
                }
                _ => {
                    stack.extend(doc.children(at));
                    continue;
                }
            };
            refs.extend(doc.child(at, field));
        }
        for leaf in refs {
            let old = self.doc.text(leaf).trim().parse::<usize>().ok();
            if let Some(&new) = old.and_then(|k| to.get(k))
                && Some(new) != old
            {
                self.doc.set_text(leaf, &new.to_string());
            }
        }
    }
}

/// Drag payload: an entry dragged in the hierarchy, with its list, its
/// place in it, and whether it has a placement to hang on a joint.
#[derive(Clone, Copy, Debug)]
pub struct EntryDrag {
    entry: Id,
    list: Id,
    place: usize,
    placed: bool,
}

impl EntryDrag {
    /// Whether dropping it on `row` does something; see
    /// [`ModelEditor::drop_entry`].
    fn fits(&self, row: Row, row_list: Option<Id>) -> bool {
        match row {
            Row::Entry(e) => e != self.entry && row_list == Some(self.list),
            Row::List(l) => l == self.list,
            Row::Joint(part, _) => self.placed && part != self.entry,
            Row::Joints(_) => false,
        }
    }
}

/// What the row menu does.
#[derive(Clone, Copy, Debug)]
enum Act {
    Play(Source),
    Rename(Id),
    Duplicate(Id),
    Detach(Id),
    Open(Id),
    Delete(Id),
    /// Hang the selected entry on this joint.
    Hang(Id, u16),
}

/// A row's menu: label, its key, what it does; disabled items carry why.
fn actions(m: &ModelEditor, row: Row) -> Vec<(String, &'static str, Result<Act, &'static str>)> {
    let mut out = Vec::new();
    match row {
        Row::Entry(e) => {
            if let Some(k) = m.model.sequences.iter().position(|s| s.element == e) {
                out.push(("Play".into(), "", Ok(Act::Play(Source::Sequence(k)))));
            }
            if let Some(part) = m.model.parts.iter().position(|p| p.element == e) {
                // A dozen at most: a unit part carries forty, and the
                // timeline's source list has them all.
                for (animation, path) in m.model.parts[part].animations.iter().enumerate().take(12)
                {
                    let file = path.rsplit('/').next().unwrap_or(path);
                    let play = Act::Play(Source::Animation { part, animation });
                    out.push((
                        format!("Play {}", file.trim_end_matches(".rdm")),
                        "",
                        Ok(play),
                    ));
                }
            }
            let rename = match m.doc.child(e, "Name") {
                Some(_) => Ok(Act::Rename(e)),
                None => Err("It has no Name: a sequence is named by its id"),
            };
            out.push(("Rename".into(), "F2", rename));
            out.push(("Duplicate".into(), "Ctrl D", Ok(Act::Duplicate(e))));
            if let Some(j) = m.joint_of(e) {
                out.push((format!("Detach from {j}"), "", Ok(Act::Detach(e))));
            }
            if m.prop_cfg(e).is_some() {
                out.push(("Open prop cfg".into(), "Enter", Ok(Act::Open(e))));
            }
            out.push(("Delete".into(), "Delete", Ok(Act::Delete(e))));
        }
        Row::Joint(part, j) => {
            let hang = match m.selected {
                None => Err("Select an entry to hang here first"),
                Some(s) if m.placement(s).is_none() => Err("A sequence has no place to hang"),
                Some(s) if m.hangs_on(part, s) => Err("That part hangs on the selection"),
                Some(_) => Ok(Act::Hang(part, j)),
            };
            let label = match m.selected {
                Some(s) => format!("Hang {} here", m.label(s)),
                None => "Hang the selection here".into(),
            };
            out.push((label, "", hang));
        }
        Row::List(_) | Row::Joints(_) => {}
    }
    out
}

/// The lists of the MAIN config the cfg has, in the hierarchy's order.
fn lists(m: &ModelEditor) -> Vec<Row> {
    let Some(main) = m.main() else {
        return Vec::new();
    };
    (LISTS.iter())
        .filter_map(|(l, _)| m.doc.child(main, l).map(Row::List))
        .collect()
}

/// The rows the pane shows: the open part of the tree, or while searching,
/// every entry and joint that matches under its list and part, whatever is
/// open.
pub(super) fn visible_rows(m: &ModelEditor) -> Vec<tree::Row<Row>> {
    let needle = m.hierarchy.filter.trim().to_lowercase();
    if needle.is_empty() {
        return m.tree.flatten(
            lists(m),
            |row| (!row_children(m, *row).is_empty()).then_some(*row),
            |row| row_children(m, *row),
        );
    }
    // Hung entries also sit in their own list; found there once is enough.
    let children = |row: &Row| {
        let mut rows = row_children(m, *row);
        if matches!(row, Row::Entry(_)) {
            rows.retain(|r| !matches!(r, Row::Entry(_)));
        }
        rows
    };
    let matches = |row: &Row| found(m, *row, &needle);
    tree::Tree::<Row>::flatten_matching(lists(m), children, matches, MATCH_LIMIT)
}

/// Whether the search `needle` (lowercase) finds `row`: an entry or joint
/// whose name or note holds it.
fn found(m: &ModelEditor, row: Row, needle: &str) -> bool {
    matches!(row, Row::Entry(_) | Row::Joint(..)) && {
        let (label, detail) = row_label(m, row);
        label.to_lowercase().contains(needle) || detail.to_lowercase().contains(needle)
    }
}

/// The row the arrow keys move from: the selected entry, else the picked
/// joint.
fn current(m: &ModelEditor, rows: &[tree::Row<Row>]) -> Option<usize> {
    let entry = m.selected.map(Row::Entry);
    let joint = m.joint.map(|(p, j)| Row::Joint(p, j));
    (entry.and_then(|e| rows.iter().position(|r| r.node == e)))
        .or_else(|| joint.and_then(|j| rows.iter().position(|r| r.node == j)))
}

/// One shown row, worked out once per render.
struct Shown {
    label: SharedString,
    detail: SharedString,
    selected: bool,
    glyph: Icon,
    /// The list an entry sits in and its place there, for drops.
    list: Option<Id>,
    place: usize,
    drag: Option<EntryDrag>,
    /// The rename field's text with its caret, while this row is renamed.
    rename: Option<String>,
}

impl Editor {
    /// Keys for the hierarchy. While its search is typed: typing filters,
    /// Enter keeps the search and selects the first match, Escape clears
    /// it. While nothing is typed: Escape closes the row menu, arrows move
    /// through the rows (an entry selects, a joint picks), Enter opens the
    /// selected prop's cfg, F2 renames the selected entry. Runs before the
    /// typed field and the viewport; passes on every key while another
    /// field is typed.
    pub(crate) fn model_hierarchy_key(
        &mut self,
        event: &KeyDownEvent,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(m) = &mut self.model else {
            return false;
        };
        if let Some(t) = &mut m.typing {
            if t.field != Field::HierarchyFilter {
                return false;
            }
            match t.key_down_in(event, cx) {
                EditEvent::Changed => m.hierarchy.filter = t.text.clone(),
                EditEvent::Commit => {
                    m.typing = None;
                    let rows = visible_rows(m);
                    let first = rows.iter().find_map(|r| match r.node {
                        Row::Entry(e) => Some(e),
                        _ => None,
                    });
                    if first.is_some() {
                        m.select(first);
                    }
                }
                EditEvent::Cancel => {
                    m.typing = None;
                    m.hierarchy.filter.clear();
                }
                EditEvent::Ignored => return false,
            }
            self.model_edited(cx);
            return true;
        }
        let k = &event.keystroke;
        let plain = !(k.modifiers.control || k.modifiers.platform || k.modifiers.alt);
        match k.key.as_str() {
            "escape" if m.hierarchy.menu.is_some() => m.hierarchy.menu = None,
            "f2" if plain => {
                let Some(entry) = m.selected else {
                    return false;
                };
                m.start_rename(entry);
            }
            "enter" if plain => {
                let Some(cfg) = m.selected.and_then(|e| m.prop_cfg(e)) else {
                    return false;
                };
                self.open_model(&cfg, cx);
            }
            _ => {
                let rows = visible_rows(m);
                let at = current(m, &rows);
                // Asked only of rows that open, which key themselves.
                let nav = m.tree.navigate(event, &rows, at, |row| Some(*row));
                match nav {
                    Some(tree::Nav::Select(i)) => match rows[i].node {
                        Row::Entry(e) => m.select(Some(e)),
                        Row::Joint(p, j) => {
                            m.joint = Some((p, j));
                            m.pose();
                        }
                        // Lists and the Joints heading hold no selection.
                        _ => return self.model_hierarchy_skip(event, i, cx),
                    },
                    Some(tree::Nav::Relayout) => {}
                    None => return false,
                }
            }
        }
        self.model_edited(cx);
        true
    }

    /// An arrow landed on row `i`, a heading (a list, a part's Joints): up
    /// and down keep going to the next entry or joint, right opens it and
    /// steps in, left stays where it was.
    fn model_hierarchy_skip(
        &mut self,
        event: &KeyDownEvent,
        i: usize,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(m) = &mut self.model else {
            return false;
        };
        let key = event.keystroke.key.as_str();
        if key == "left" {
            return true;
        }
        if key == "right" {
            let heading = visible_rows(m)[i].node;
            m.tree.expand(heading);
        }
        let rows = visible_rows(m);
        let pick = |r: &tree::Row<Row>| matches!(r.node, Row::Entry(_) | Row::Joint(..));
        let found = match key {
            "down" | "home" => rows[i..].iter().position(pick).map(|k| i + k),
            "right" => (rows.get(i + 1))
                .filter(|r| r.depth > rows[i].depth && pick(r))
                .map(|_| i + 1),
            _ => rows[..=i].iter().rposition(pick),
        };
        match found.map(|k| rows[k].node) {
            Some(Row::Entry(e)) => m.select(Some(e)),
            Some(Row::Joint(p, j)) => {
                m.joint = Some((p, j));
                m.pose();
            }
            _ => return true,
        }
        if let Some(k) = found {
            m.tree.scroll_to(k);
        }
        self.model_edited(cx);
        true
    }

    /// Runs a row menu item.
    fn model_hierarchy_act(&mut self, act: Act, cx: &mut Context<Self>) {
        let Some(m) = &mut self.model else { return };
        m.hierarchy.menu = None;
        match act {
            Act::Play(source) => {
                m.play(source);
                m.playback.playing = true;
            }
            Act::Rename(e) => {
                m.select(Some(e));
                m.start_rename(e);
            }
            Act::Duplicate(e) => m.duplicate(e),
            Act::Detach(e) => m.attach(e, None),
            Act::Open(e) => {
                if let Some(cfg) = m.prop_cfg(e) {
                    return self.open_model(&cfg, cx);
                }
            }
            Act::Delete(e) => m.remove(e),
            Act::Hang(part, j) => {
                if let Some(s) = m.selected {
                    m.drop_entry(s, Row::Joint(part, j));
                }
            }
        }
        let playing = m.playback.playing;
        self.model_edited(cx);
        if playing {
            self.model_tick(cx);
        }
    }

    pub fn render_model_hierarchy(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(m) = &self.model else {
            return crate::editor::pane_note(
                "The open model's meshes, props, effects, lights and sequences list here.",
            );
        };
        if m.main().is_none() {
            return crate::editor::empty_pane("This file has no MAIN config");
        }
        let rows = Rc::new(visible_rows(m));
        let query = m.hierarchy.filter.trim().to_owned();
        let searching = !query.is_empty();
        let renaming = match m.typing.as_ref().map(|t| t.field) {
            Some(Field::Value(leaf)) => m.doc.parent(leaf).filter(|&e| m.list_of(e).is_some()),
            _ => None,
        };
        let place = |e: Id| {
            let list = m.doc.parent(e);
            (list.into_iter().flat_map(|l| m.doc.children(l)))
                .filter(|&c| m.doc.name(c) == "Config")
                .position(|c| c == e)
                .unwrap_or(0)
        };
        let shown: Rc<Vec<Shown>> = Rc::new(
            (rows.iter())
                .map(|r| {
                    let (label, detail) = row_label(m, r.node);
                    let (selected, list, drag) = match r.node {
                        Row::Entry(e) => {
                            let list = m.doc.parent(e);
                            let drag = list.map(|list| EntryDrag {
                                entry: e,
                                list,
                                place: place(e),
                                placed: m.placement(e).is_some(),
                            });
                            (m.selected == Some(e), list, drag)
                        }
                        Row::Joint(p, j) => (m.joint == Some((p, j)), None, None),
                        Row::List(l) => (false, Some(l), None),
                        Row::Joints(_) => (false, None, None),
                    };
                    let rename = match r.node {
                        Row::Entry(e) if renaming == Some(e) => {
                            m.typing.as_ref().map(|t| t.display())
                        }
                        _ => None,
                    };
                    Shown {
                        label: label.into(),
                        detail: detail.into(),
                        selected,
                        glyph: glyph(m, r.node),
                        list,
                        place: match r.node {
                            Row::Entry(e) => place(e),
                            _ => 0,
                        },
                        drag,
                        rename,
                    }
                })
                .collect(),
        );
        let item = {
            let (rows, shown) = (rows.clone(), shown.clone());
            move |i: usize| Item {
                depth: rows[i].depth,
                disclosure: rows[i].disclosure,
                label: shown[i].label.clone(),
                detail: shown[i].detail.clone(),
                selected: shown[i].selected,
            }
        };
        let click = {
            let rows = rows.clone();
            move |this: &mut Self, i: usize, e: &ClickEvent, cx: &mut Context<Self>| {
                let Some(m) = &mut this.model else { return };
                m.hierarchy.menu = None;
                match rows[i].node {
                    Row::List(_) | Row::Joints(_) => m.tree.toggle(rows[i].node),
                    Row::Joint(part, j) => {
                        m.joint = (m.joint != Some((part, j))).then_some((part, j));
                        m.pose();
                    }
                    Row::Entry(id) if e.click_count() == 2 && m.prop_cfg(id).is_some() => {
                        // A prop's own cfg opens in its place.
                        if let Some(cfg) = m.prop_cfg(id) {
                            return this.open_model(&cfg, cx);
                        }
                    }
                    Row::Entry(id) => m.select(Some(id)),
                }
                this.model_edited(cx);
            }
        };
        let toggle = {
            let rows = rows.clone();
            move |this: &mut Self, i: usize, cx: &mut Context<Self>| {
                if let Some(m) = &mut this.model {
                    m.tree.toggle(rows[i].node);
                }
                cx.notify();
            }
        };
        let decorate = {
            let (rows, shown) = (rows.clone(), shown.clone());
            let entity = cx.entity().downgrade();
            move |i: usize, mut el: Stateful<Div>| {
                let (row, s) = (rows[i].node, &shown[i]);
                let opened = entity.clone();
                el = el.on_mouse_down(MouseButton::Right, move |e, _, cx| {
                    let _ = opened.update(cx, |this, cx| {
                        let Some(m) = &mut this.model else { return };
                        if let Row::Entry(id) = row {
                            m.select(Some(id));
                        }
                        m.hierarchy.menu = Some((row, e.position));
                        this.model_edited(cx);
                    });
                });
                if let Some(drag) = s.drag {
                    let label = s.label.clone();
                    el = el.on_drag(drag, move |_, _, _, cx| {
                        cx.new(|_| DragLabel(label.clone()))
                    });
                }
                if matches!(row, Row::Entry(_) | Row::List(_) | Row::Joint(..)) {
                    let (list, place) = (s.list, s.place);
                    let dropped = entity.clone();
                    // Onto an entry it takes that place: an accent rule on
                    // the side it lands, under the row dragged down, over
                    // it dragged up. Onto a joint or a header it goes in.
                    el = el
                        .drag_over::<EntryDrag>(move |style, drag, _, _| {
                            if !drag.fits(row, list) {
                                return style;
                            }
                            match row {
                                Row::Entry(_) if place > drag.place => {
                                    style.border_b_2().border_color(theme::get().accent)
                                }
                                Row::Entry(_) => {
                                    style.border_t_2().border_color(theme::get().accent)
                                }
                                _ => style.bg(theme::get().selected),
                            }
                        })
                        .on_drop(move |drag: &EntryDrag, _, cx| {
                            if !drag.fits(row, list) {
                                return;
                            }
                            let entry = drag.entry;
                            let _ = dropped.update(cx, |this, cx| {
                                if let Some(m) = &mut this.model {
                                    m.drop_entry(entry, row);
                                }
                                this.model_edited(cx);
                            });
                        });
                }
                if let Some(text) = &s.rename {
                    // Over the label: arrow, glyph and gaps take 32 px
                    // after the row's indent.
                    let left = 6. + rows[i].depth as f32 * 14. + 30.;
                    el = el.child(
                        div()
                            .id("model-rename")
                            .absolute()
                            .top(px(1.))
                            .bottom(px(1.))
                            .left(px(left))
                            .right(px(4.))
                            // A press here places the caret; the row under
                            // it must not take it as a click that selects
                            // and ends the rename.
                            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                            .child(
                                text_field("model-rename-field", "", Some(text.clone()), "Name")
                                    .h(px(20.)),
                            ),
                    );
                }
                el
            }
        };
        let glyphs = {
            let shown = shown.clone();
            move |i: usize| Some(shown[i].glyph)
        };
        let headings = {
            let rows = rows.clone();
            move |i: usize| matches!(rows[i].node, Row::List(_))
        };
        let list = tree::List::new("model-hierarchy", rows.len(), item, click)
            .on_toggle(toggle)
            .decorate(decorate)
            .glyph(glyphs)
            .heading(headings)
            .guides()
            .highlight(query.clone())
            .render(&m.tree, window, cx)
            .flex_1()
            .min_h_0();

        let editing = (m.typing.as_ref())
            .filter(|t| t.field == Field::HierarchyFilter)
            .map(|t| t.display());
        let filled = !m.hierarchy.filter.is_empty();
        let filter = m.hierarchy.filter.clone();
        // The rows found, not the parts and joints shown above them.
        let hits = searching.then(|| {
            let needle = query.to_lowercase();
            rows.iter().filter(|r| found(m, r.node, &needle)).count()
        });
        let search = div()
            .px_2()
            .pt_2()
            .pb_1()
            .flex()
            .items_center()
            .gap_2()
            .child(
                div()
                    .relative()
                    .flex_1()
                    .min_w_0()
                    .child(
                        text_field(
                            "model-hierarchy-filter",
                            &filter,
                            editing,
                            "Find entries and joints",
                        )
                        .when(filled, |d| d.pr(px(22.)))
                        .on_click(cx.listener(
                            move |this, _, window, cx| {
                                let Some(m) = &mut this.model else { return };
                                if m.typing
                                    .as_ref()
                                    .is_some_and(|t| t.field == Field::HierarchyFilter)
                                {
                                    return;
                                }
                                this.focus.focus(window, cx);
                                let m = this.model.as_mut().expect("checked");
                                let text = m.hierarchy.filter.clone();
                                m.typing =
                                    Some(TextEdit::new(Field::HierarchyFilter, text, Filter::Text));
                                cx.notify();
                            },
                        )),
                    )
                    .when(filled, |d| {
                        d.child(clear_search("model-hierarchy-clear").on_click(cx.listener(
                            |this, _, _, cx| {
                                cx.stop_propagation();
                                if let Some(m) = &mut this.model {
                                    m.hierarchy.filter.clear();
                                    if m.typing
                                        .as_ref()
                                        .is_some_and(|t| t.field == Field::HierarchyFilter)
                                    {
                                        m.typing = None;
                                    }
                                }
                                cx.notify();
                            },
                        )))
                    }),
            )
            .when_some(hits, |d, n| {
                d.child(
                    div()
                        .flex_shrink_0()
                        .text_size(px(10.))
                        .text_color(theme::get().muted)
                        .child(match n {
                            0 => "none".to_owned(),
                            n => crate::project::plural(n, "match", "matches"),
                        }),
                )
            });
        let menu = m.hierarchy.menu.and_then(|(row, at)| {
            let items = actions(m, row);
            if items.is_empty() {
                return None;
            }
            let mut body = div()
                .id("model-hierarchy-menu")
                .occlude()
                .w(px(230.))
                .bg(theme::get().popup)
                .rounded(px(6.))
                .border_1()
                .border_color(theme::get().floating_border)
                .shadow_lg()
                .p_1()
                .flex()
                .flex_col()
                .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                    if let Some(m) = &mut this.model {
                        m.hierarchy.menu = None;
                    }
                    cx.notify();
                }));
            for (label, keys, act) in items {
                if label == "Delete" {
                    body = body.child(widgets::separator());
                }
                body = body.child(match act {
                    Ok(act) => widgets::menu_item(label, keys).on_click(
                        cx.listener(move |this, _, _, cx| this.model_hierarchy_act(act, cx)),
                    ),
                    Err(why) => widgets::menu_item_off(label, why),
                });
            }
            Some(
                deferred(
                    anchored()
                        .position(at)
                        .snap_to_window_with_margin(px(8.))
                        .child(body),
                )
                .with_priority(1),
            )
        });
        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(theme::get().panel)
            .child(search)
            .child(list)
            .child(
                div()
                    .px_2()
                    .py_1()
                    .text_size(px(10.))
                    .text_color(theme::get().muted)
                    .child("Drag to reorder or onto a joint to hang there. Right-click for more."),
            )
            .children(menu)
            .into_any_element()
    }
}
