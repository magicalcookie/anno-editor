// Owns the widget tree pane and structural edits: the laid-out widgets as a
// collapsible tree (container template contents start collapsed), and add,
// duplicate, delete, reorder, reparent, copy and paste, each one undo step
// through the `phx` tree edits. Widgets from a container's template file
// show for context and are read-only here.
use super::{Row, UiEditor, canvas, states};
use crate::editor::Editor;
use anno_formats::phx::{self, Asset, Edit, ObjectId, Record, Value, Widget, WidgetCopy};
use anyhow::{Context as _, Result, bail};
use gpui_editor_kit::{
    field::{Filter, TextEdit, text_field},
    gpui::{prelude::*, *},
    theme,
    tree::{self, Tree},
    widgets::{self, Disclosure, DragLabel, Tooltip, checkbox},
};
use std::{collections::HashSet, rc::Rc};

// Widget classes the Add buttons create.
const CLASS_GROUP: u32 = phx::name_crc(b"GroupInstance");
const CLASS_IMAGE: u32 = phx::name_crc(b"ImageWidgetInstance");
const CLASS_LABEL: u32 = phx::name_crc(b"LabelWidgetInstance");
const PROP_NAME: u32 = phx::name_crc(b"PhoenixDebugName");
/// Templates scanned for a prototype before giving up; the first few
/// hundred always have plain images, labels and groups.
const PROTOTYPE_SCAN: usize = 2000;
/// `UiEditor::editing`'s field while the widget tree's search is typed.
pub(super) const TREE_FILTER: usize = usize::MAX - 2;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NewWidget {
    Image,
    Label,
    Group,
    Video,
}

impl NewWidget {
    /// Every kind, as the widget library lists them. The schema has a
    /// VectorShapeWidgetInstance too, but no shipped file uses one, so
    /// there is nothing to start one from.
    pub(super) const ALL: [Self; 4] = [Self::Image, Self::Label, Self::Group, Self::Video];

    fn class(self) -> u32 {
        match self {
            Self::Image => CLASS_IMAGE,
            Self::Label => CLASS_LABEL,
            Self::Group => CLASS_GROUP,
            Self::Video => phx::name_crc(b"VideoWidgetInstance"),
        }
    }

    fn base_name(self) -> &'static str {
        match self {
            Self::Image => "NewImage",
            Self::Label => "NewLabel",
            Self::Group => "NewGroup",
            Self::Video => "NewVideo",
        }
    }

    /// As the library names it, and what it is for.
    pub(super) fn label(self) -> (&'static str, &'static str) {
        match self {
            Self::Image => ("Image", "A picture or a 9-slice frame"),
            Self::Label => ("Label", "Text"),
            Self::Group => (
                "Group",
                "Holds widgets; moves, hides and lays them out together",
            ),
            Self::Video => ("Video", "A video clip"),
        }
    }
}

/// Where a new widget goes.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Place {
    /// Where Add puts one: into the selected group (or the root) as its
    /// last child, else right after the selected widget.
    Selection,
    /// Dropped on the canvas at a design point: into the innermost group of
    /// this file there (else the root), its pivot on the point.
    Canvas([f32; 2]),
    /// Dropped on a tree row: into it when it is a group, else before it.
    Tree(usize),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    /// Earlier among its siblings: drawn below them, first in stacks.
    Up,
    Down,
    /// Out of its group, right after it.
    Out,
    /// Into the group just before it, as its last child.
    In,
}

/// Drag payload: a canvas node being dragged in the tree.
#[derive(Clone, Copy)]
struct WidgetDrag(usize);

pub(super) fn object_id(asset: &Asset, object: usize) -> ObjectId {
    match &asset.spans()[object].record {
        Record::Object(h) => h.id,
        _ => ObjectId::None,
    }
}

pub(super) fn find_widget(asset: &Asset, widgets: &[Widget], id: ObjectId) -> Option<usize> {
    widgets
        .iter()
        .position(|w| object_id(asset, w.object) == id)
}

/// Copies onto a duplicated subtree (root `copy`, inserted next to root
/// `original`) the bindings that set the original's widgets: targets go to
/// the matching copy widgets, sources inside the subtree too (a widget
/// reading its own child), the rest stay shared. A binding that also sets
/// something outside, or a nested template inside, is left alone: it
/// cannot be split. Returns the asset and how many were copied and left.
fn carry_bindings(
    next: Asset,
    original: ObjectId,
    copy: ObjectId,
) -> Result<(Asset, usize, usize)> {
    use phx::binding::{BindingCopy, NewSlot, Slot};
    let widgets = next.widgets();
    let root = |id| find_widget(&next, &widgets, id).context("duplicated widget lost");
    // Preorder of each subtree: the copy has the original's shape.
    let preorder = |r: usize| {
        let mut out = vec![r];
        let mut i = 0;
        while i < out.len() {
            out.extend(widgets[out[i]].children.iter().copied());
            i += 1;
        }
        out
    };
    let (from, to) = (preorder(root(original)?), preorder(root(copy)?));
    anyhow::ensure!(
        from.len() == to.len(),
        "the copy's tree differs from the original's"
    );
    let map = |w: usize| from.iter().position(|&x| x == w).map(|i| to[i]);
    let table = next.bindings(&widgets);
    let inside = |s: usize| match &table.slots[s] {
        Slot::Widget {
            widget: Some(w), ..
        }
        | Slot::Nested { widget: w, .. } => map(*w).is_some(),
        _ => false,
    };
    let (mut copies, mut left) = (Vec::new(), 0);
    for (i, b) in table.bindings.iter().enumerate() {
        if !b.targets.iter().any(|&t| inside(t)) {
            continue;
        }
        let slots: Option<Vec<(usize, NewSlot)>> = b
            .sources
            .iter()
            .chain(&b.targets)
            .enumerate()
            .filter(|&(_, &s)| inside(s))
            .map(|(k, &s)| match &table.slots[s] {
                Slot::Widget {
                    widget: Some(w),
                    path,
                } => Some((
                    k,
                    NewSlot::Widget {
                        widget: map(*w)?,
                        path: path.clone(),
                    },
                )),
                // A nested template's widget: no way to name its copy yet.
                _ => None,
            })
            .collect();
        match slots {
            Some(slots) if b.targets.iter().all(|&t| inside(t)) => {
                copies.push(BindingCopy { binding: i, slots });
            }
            _ => left += 1,
        }
    }
    if copies.is_empty() {
        return Ok((next, 0, left));
    }
    let n = copies.len();
    Ok((next.copy_bindings(&widgets, &copies)?, n, left))
}

fn count_is_zero(asset: &Asset, w: &Widget, name: &str) -> bool {
    w.props
        .iter()
        .any(|p| p.name == Some(name) && matches!(asset.spans()[p.span].record, Record::Count(0)))
}

/// A widget without components, effects, property overrides or game
/// object: a neutral starting point for a new one.
pub(super) fn plain(asset: &Asset, w: &Widget) -> bool {
    ["Components", "Effects", "PropertyValues"]
        .iter()
        .all(|n| count_is_zero(asset, w, n))
        && w.props
            .iter()
            .any(|p| p.name == Some("GameObjectTemplate") && p.value == Value::Null)
}

impl UiEditor {
    /// The own-file widget a canvas node shows, if it is not a template's.
    fn own_widget(&self, node: usize) -> Option<usize> {
        self.nodes
            .get(node)
            .filter(|n| !n.foreign)
            .map(|n| n.widget)
    }

    fn node_of(&self, widget: usize) -> Option<usize> {
        self.nodes
            .iter()
            .position(|n| !n.foreign && n.widget == widget)
    }

    /// Expands the widgets of this file, and the first two levels of
    /// container templates too, so a screen that is one container opens
    /// on what it shows rather than on a single closed row.
    pub(super) fn reset_tree(&mut self) {
        self.widget_tree = Tree::default();
        for (i, n) in self.nodes.iter().enumerate() {
            if self
                .nodes
                .get(i + 1)
                .is_some_and(|c| c.depth > n.depth && (!c.foreign || n.depth < 2))
            {
                self.widget_tree.expand(i);
            }
        }
    }

    /// Opens every ancestor of `node` and scrolls the tree to it.
    pub(super) fn reveal(&mut self, node: usize) {
        let mut at = node;
        while let Some(p) = (0..at)
            .rev()
            .find(|&p| self.nodes[p].depth < self.nodes[at].depth)
        {
            self.widget_tree.expand(p);
            at = p;
        }
        if let Some(row) = self
            .widget_rows()
            .iter()
            .position(|r| r.node == Row::Widget(node))
        {
            self.widget_tree.scroll_to(row);
        }
    }

    /// Widgets whose name holds the tree search (case-insensitive), in
    /// tree order; with the template edited in place, only its own.
    pub(super) fn tree_matches(&self) -> Vec<usize> {
        let needle = self.tree_filter.trim().to_lowercase();
        if needle.is_empty() {
            return Vec::new();
        }
        (1..self.nodes.len())
            .filter(|&i| self.scoped(i) && self.nodes[i].name.to_lowercase().contains(&needle))
            .collect()
    }

    /// Rows of the widget tree: canvas nodes under expanded parents. While
    /// searching, the matches and the groups they sit in, open.
    pub(super) fn widget_rows(&self) -> Vec<tree::Row<Row>> {
        let mut rows = Vec::new();
        let searching = !self.tree_filter.trim().is_empty();
        // Matches and their ancestors stay; the ancestors open.
        let n = if searching { self.nodes.len() } else { 0 };
        let (mut keep, mut open) = (super::Bits::new(n), super::Bits::new(n));
        if searching {
            for m in self.tree_matches() {
                keep.set(m, true);
                let mut at = self.parents[m];
                while let Some(i) = at {
                    open.set(i, true);
                    // Kept before: its ancestors are kept and open already.
                    if keep.get(i) {
                        break;
                    }
                    keep.set(i, true);
                    at = self.parents[i];
                }
            }
        }
        let mut closed_at: Option<usize> = None;
        // Editing a template in place: only its widgets, from its top.
        let scoped = !self.in_scope.is_empty();
        let base = if scoped {
            (0..self.nodes.len())
                .filter(|&i| self.in_scope.get(i))
                .map(|i| self.nodes[i].depth)
                .min()
                .unwrap_or(0)
        } else {
            0
        };
        for (i, n) in self.nodes.iter().enumerate() {
            if !self.scoped(i) {
                continue;
            }
            if searching {
                // A match's own children stay out: the search found it.
                if keep.get(i) {
                    rows.push(tree::Row {
                        node: Row::Widget(i),
                        depth: n.depth - base,
                        disclosure: if open.get(i) {
                            Disclosure::Expanded
                        } else {
                            Disclosure::Leaf
                        },
                    });
                }
                continue;
            }
            if let Some(d) = closed_at {
                if n.depth > d {
                    continue;
                }
                closed_at = None;
            }
            let parent = self.nodes.get(i + 1).is_some_and(|c| c.depth > n.depth);
            let disclosure = if !parent {
                Disclosure::Leaf
            } else if self.widget_tree.is_expanded(i) {
                Disclosure::Expanded
            } else {
                closed_at = Some(n.depth);
                Disclosure::Collapsed
            };
            rows.push(tree::Row {
                node: Row::Widget(i),
                depth: n.depth - base,
                disclosure,
            });
        }
        rows
    }

    /// Replaces the asset after a structural edit (one undo step) and
    /// selects the widget with id `select`.
    pub(super) fn restructure(&mut self, next: Asset, select: Option<ObjectId>) {
        self.editing = None;
        self.checkpoint();
        self.asset = next;
        self.images.forget(self.asset.guid());
        self.chosen_states.clear();
        self.canvas.selected = None;
        self.selected = None;
        self.collect_clips();
        self.relayout();
        self.reset_tree();
        let widgets = self.asset.widgets();
        if let Some(node) = select
            .and_then(|id| find_widget(&self.asset, &widgets, id))
            .and_then(|w| self.node_of(w))
        {
            self.select_widget(node);
            self.reveal(node);
        }
    }

    fn selected_own(&self) -> Result<usize> {
        let node = self.canvas.selected.context("Select a widget first")?;
        match self.own_widget(node) {
            Some(w) => Ok(w),
            None => {
                bail!("That widget lives in a container’s template; open the template to edit it")
            }
        }
    }

    /// Where new widgets go: into the selected group (or the root) as its
    /// last child, else right after the selected widget.
    fn insertion_point(&self, widgets: &[Widget]) -> (usize, usize) {
        let Some(w) = self.canvas.selected.and_then(|n| self.own_widget(n)) else {
            return (0, usize::MAX);
        };
        if self.asset.child_group(widgets, w).is_some() {
            return (w, usize::MAX);
        }
        let parent = widgets[w].parent.unwrap_or(0);
        let at = widgets[parent].children.iter().position(|&c| c == w);
        (parent, at.map_or(usize::MAX, |i| i + 1))
    }

    /// Inserts `copy` (fresh ids) and returns the asset and the new
    /// widget's id.
    fn insert_at(
        &self,
        parent: usize,
        index: usize,
        copy: &WidgetCopy,
    ) -> Result<(Asset, ObjectId)> {
        let widgets = self.asset.widgets();
        let parent_id = object_id(&self.asset, widgets[parent].object);
        let next = self
            .asset
            .insert_widget(&widgets, parent, index, copy, true)?;
        let nw = next.widgets();
        let p = find_widget(&next, &nw, parent_id).context("parent lost")?;
        let kids = &nw[p].children;
        let w = kids[index.min(kids.len() - 1)];
        let id = object_id(&next, nw[w].object);
        Ok((next, id))
    }

    pub fn delete_selected(&mut self) -> Result<String> {
        let w = self.selected_own()?;
        let widgets = self.asset.widgets();
        let name = widgets[w].name.clone().unwrap_or_default();
        // Select the next sibling, else the previous, else the parent.
        let parent = widgets[w].parent.unwrap_or(0);
        let kids = &widgets[parent].children;
        let at = kids.iter().position(|&c| c == w).unwrap_or(0);
        let next_sel = kids
            .get(at + 1)
            .or(at.checked_sub(1).and_then(|i| kids.get(i)))
            .map_or(parent, |&k| k);
        let next_id = object_id(&self.asset, widgets[next_sel].object);
        let removed = self.asset.remove_widget(&widgets, w)?;
        let dangling = removed.dangling;
        self.restructure(removed.asset, Some(next_id));
        Ok(if dangling > 0 {
            format!(
                "Deleted {name}; {dangling} data binding path(s) still name it or its children (Ctrl Z restores)"
            )
        } else {
            format!("Deleted {name}")
        })
    }

    pub fn copy_selected(&self) -> Result<WidgetCopy> {
        let w = self.selected_own()?;
        self.asset.widget_copy(&self.asset.widgets(), w)
    }

    /// Duplicates the selected widget next to itself, with the bindings
    /// that set it or its children, as one undo step. Round 4 showed a
    /// copy of the bound icon as a hidden 0×0 box: what draws it (its
    /// image, size, tint) lives in the file's binding table, not in the
    /// widget's bytes.
    pub fn duplicate_selected(&mut self) -> Result<String> {
        let w = self.selected_own()?;
        let widgets = self.asset.widgets();
        let copy = self.asset.widget_copy(&widgets, w)?;
        let original = object_id(&self.asset, widgets[w].object);
        let parent = widgets[w].parent.unwrap_or(0);
        let at = widgets[parent]
            .children
            .iter()
            .position(|&c| c == w)
            .map_or(usize::MAX, |i| i + 1);
        let (next, id) = self.insert_at(parent, at, &copy)?;
        let name = copy.name.unwrap_or_default();
        let (next, carried, left) = carry_bindings(next, original, id)?;
        self.restructure(next, Some(id));
        Ok(match (carried, left) {
            (0, 0) => format!("Duplicated {name}"),
            (n, 0) => format!(
                "Duplicated {name} with {}",
                crate::project::plural(n, "binding", "bindings")
            ),
            (n, l) => format!(
                "Duplicated {name} with {}; {} that also {} other widgets or nested templates stayed with the original",
                crate::project::plural(n, "binding", "bindings"),
                crate::project::plural(l, "binding", "bindings"),
                if l == 1 { "sets" } else { "set" }
            ),
        })
    }

    pub fn paste(&mut self, copy: &WidgetCopy) -> Result<String> {
        let widgets = self.asset.widgets();
        let (parent, at) = self.insertion_point(&widgets);
        let (next, id) = self.insert_at(parent, at, copy)?;
        self.restructure(next, Some(id));
        Ok(format!(
            "Pasted {}",
            copy.name.clone().unwrap_or_else(|| "widget".into())
        ))
    }

    pub fn step_selected(&mut self, step: Step) -> Result<String> {
        let w = self.selected_own()?;
        let widgets = self.asset.widgets();
        let parent = widgets[w].parent.context("the root does not move")?;
        let kids = &widgets[parent].children;
        let at = kids.iter().position(|&c| c == w).context("not a child")?;
        let (to, index) = match step {
            Step::Up if at > 0 => (parent, at - 1),
            Step::Down if at + 1 < kids.len() => (parent, at + 1),
            Step::Out if parent != 0 => {
                let grand = widgets[parent].parent.unwrap_or(0);
                let p = widgets[grand].children.iter().position(|&c| c == parent);
                (grand, p.map_or(usize::MAX, |i| i + 1))
            }
            Step::In => match at.checked_sub(1).map(|i| kids[i]) {
                Some(s) if self.asset.child_group(&widgets, s).is_some() => (s, usize::MAX),
                _ => bail!("Only a group right above it can take it in"),
            },
            _ => return Ok(String::new()),
        };
        let id = object_id(&self.asset, widgets[w].object);
        let next = self.asset.move_widget(&widgets, w, to, index)?;
        let next = self.keep_place(next, id, w, to)?;
        self.restructure(next, Some(id));
        Ok(format!(
            "Moved {}",
            widgets[w].name.clone().unwrap_or_default()
        ))
    }

    /// After widget `w` moved under widget `to` (both indices of the asset
    /// before the move, `id` its object id), its Position in `next` so it
    /// stays where it was drawn: its pivot's point on the screen, in the
    /// new parent's space, less the anchor there. A widget whose Position
    /// is not stored, or a parent without a frame, keeps what it had.
    fn keep_place(&self, next: Asset, id: ObjectId, w: usize, to: usize) -> Result<Asset> {
        let (Some(n), Some(p)) = (self.node_of(w), self.node_of(to)) else {
            return Ok(next);
        };
        let f = self.frames[n];
        let Some(inv) = super::scene::inverse(self.frames[p].transform) else {
            return Ok(next);
        };
        // The new parent's Size: the screen for the root; plain groups
        // have none, and every anchor is their origin.
        let size = (to == 0)
            .then_some(self.canvas.view.size)
            .unwrap_or_default();
        let local = super::scene::apply(inv, f.pivot_world());
        let anchor = anno_formats::phx::layout::anchor_offset(f.anchor, size);
        let pos = [local[0] - anchor[0], local[1] - anchor[1], f.position[2]];
        let ws = next.widgets();
        let Some(nw) = find_widget(&next, &ws, id) else {
            return Ok(next);
        };
        let Some(span) = ws[nw]
            .props
            .iter()
            .find(|p| p.name == Some("Position") && matches!(p.value, Value::Vec3(_)))
            .map(|p| p.span)
        else {
            return Ok(next);
        };
        Ok(next
            .apply(&Edit::Value {
                span,
                value: Value::Vec3(pos),
            })?
            .asset)
    }

    /// Drops tree node `dragged` on `target`: into it when it is a group,
    /// else right before it.
    fn drop_widget(&mut self, dragged: usize, target: usize) -> Result<String> {
        let (Some(w), Some(t)) = (self.own_widget(dragged), self.own_widget(target)) else {
            bail!("Only widgets of this file can be moved");
        };
        if w == t {
            return Ok(String::new());
        }
        let widgets = self.asset.widgets();
        let (to, index) = if self.asset.child_group(&widgets, t).is_some() {
            (t, usize::MAX)
        } else {
            let parent = widgets[t].parent.unwrap_or(0);
            let others: Vec<usize> = widgets[parent]
                .children
                .iter()
                .copied()
                .filter(|&c| c != w)
                .collect();
            (parent, others.iter().position(|&c| c == t).unwrap_or(0))
        };
        let id = object_id(&self.asset, widgets[w].object);
        let next = self.asset.move_widget(&widgets, w, to, index)?;
        let next = self.keep_place(next, id, w, to)?;
        self.restructure(next, Some(id));
        Ok(format!(
            "Moved {} into {}",
            widgets[w].name.clone().unwrap_or_default(),
            widgets[to]
                .name
                .clone()
                .unwrap_or_else(|| "the root".into())
        ))
    }

    /// A plain shipped widget of `class` to copy: from this file when it
    /// has one, else from the first template in the install that does.
    fn prototype(&mut self, class: u32) -> Result<WidgetCopy> {
        if let Some(c) = self.prototypes.get(&class) {
            return Ok(c.clone());
        }
        let pick = |a: &Asset| -> Option<WidgetCopy> {
            let ws = a.widgets();
            let w = (1..ws.len())
                .filter(|&i| ws[i].class == class && plain(a, &ws[i]))
                .min_by_key(|&i| ws[i].children.len())?;
            a.widget_copy(&ws, w).ok()
        };
        let copy = match pick(&self.asset) {
            Some(c) => c,
            None => self
                .install
                .list(super::GENERATED.trim_end_matches('/'))
                .iter()
                .take(PROTOTYPE_SCAN)
                .find_map(|p| {
                    let a = Asset::parse(self.install.read(p).ok()?).ok()?;
                    pick(&a)
                })
                .context("no shipped widget of that kind to start from")?,
        };
        self.prototypes.insert(class, copy.clone());
        Ok(copy)
    }

    pub fn add_widget(&mut self, kind: NewWidget) -> Result<String> {
        self.add_kind(kind, Place::Selection)
    }

    /// A new widget of `kind` at `place`: a plain shipped one (`prototype`).
    pub(super) fn add_kind(&mut self, kind: NewWidget, place: Place) -> Result<String> {
        let copy = self.prototype(kind.class())?;
        self.add_copy(&copy, kind.base_name(), Some(kind), place)
    }

    /// Inserts `copy` at `place` as one undo step, selected, named `base`
    /// (numbered when taken), and made to show: visible, opaque, untinted,
    /// unscaled, unturned (the shipped widget it comes from may be any of
    /// these for its own screen). A dropped one sits with its pivot where
    /// it was dropped; others at the parent's anchor.
    pub(super) fn add_copy(
        &mut self,
        copy: &WidgetCopy,
        base: &str,
        kind: Option<NewWidget>,
        place: Place,
    ) -> Result<String> {
        let widgets = self.asset.widgets();
        let (parent, at) = match place {
            Place::Selection => self.insertion_point(&widgets),
            Place::Canvas(p) => {
                let group = self
                    .stack_at(p)
                    .into_iter()
                    .filter_map(|n| self.own_widget(n))
                    .find(|&w| self.asset.child_group(&widgets, w).is_some());
                (group.unwrap_or(0), usize::MAX)
            }
            Place::Tree(node) => {
                let t = self.own_widget(node).context(
                    "That row lives in a container’s template; open the template to add to it",
                )?;
                if self.asset.child_group(&widgets, t).is_some() {
                    (t, usize::MAX)
                } else {
                    let parent = widgets[t].parent.unwrap_or(0);
                    let at = widgets[parent].children.iter().position(|&c| c == t);
                    (parent, at.unwrap_or(usize::MAX))
                }
            }
        };
        let (mut next, id) = self.insert_at(parent, at, copy)?;
        // A group prototype comes with children; a new group starts empty.
        while kind == Some(NewWidget::Group) {
            let ws = next.widgets();
            let w = find_widget(&next, &ws, id).context("new widget lost")?;
            let Some(&child) = ws[w].children.first() else {
                break;
            };
            next = next.remove_widget(&ws, child)?.asset;
        }
        let taken: HashSet<String> = widgets.iter().filter_map(|w| w.name.clone()).collect();
        let name = (1..)
            .map(|k| match k {
                1 => base.to_owned(),
                k => format!("{base}{k}"),
            })
            .find(|n| !taken.contains(n))
            .expect("a free name");
        let ws = next.widgets();
        let w = find_widget(&next, &ws, id).context("new widget lost")?;
        let mut edits = Vec::new();
        for p in &ws[w].props {
            match (p.crc, p.name, &p.value) {
                (PROP_NAME, ..) => edits.push(Edit::Text {
                    span: p.span,
                    text: name.clone(),
                }),
                (_, Some("Position"), Value::Vec3(_)) => edits.push(Edit::Value {
                    span: p.span,
                    value: Value::Vec3([0.; 3]),
                }),
                (_, Some("Text" | "DisplayedText"), Value::Str(_))
                    if kind == Some(NewWidget::Label) =>
                {
                    edits.push(Edit::Text {
                        span: p.span,
                        text: "Label".into(),
                    })
                }
                (_, Some("Visible"), Value::Bool(false)) => edits.push(Edit::Value {
                    span: p.span,
                    value: Value::Bool(true),
                }),
                (_, Some("IsHidden"), Value::Bool(true)) => edits.push(Edit::Value {
                    span: p.span,
                    value: Value::Bool(false),
                }),
                (_, Some("AlphaTint"), Value::F32(a)) if *a != 1. => edits.push(Edit::Value {
                    span: p.span,
                    value: Value::F32(1.),
                }),
                (_, Some("Scale"), Value::Vec3(_)) => edits.push(Edit::Value {
                    span: p.span,
                    value: Value::Vec3([1.; 3]),
                }),
                (_, Some("RotationAngle"), Value::F32(r)) if *r != 0. => edits.push(Edit::Value {
                    span: p.span,
                    value: Value::F32(0.),
                }),
                // Untinted: white, and no palette entry, which would win over
                // the white (round 15: a prototype's black palette tint stayed
                // on a new Image). The palette reference itself stays. Both
                // sit inside its ColorType.
                (_, Some("ColorTint"), _) => {
                    let spans = next.spans();
                    for i in p.span + 1..(p.span + 8).min(spans.len()) {
                        match (&spans[i].record, spans[i].field) {
                            (Record::Value(Value::U32(entry)), Some(f))
                                if f.object == p.span && *entry != 0 =>
                            {
                                edits.push(Edit::Value {
                                    span: i,
                                    value: Value::U32(0),
                                })
                            }
                            (Record::Color(c), _) if *c != [255; 3] => edits.push(Edit::Color {
                                span: i,
                                rgb: [255; 3],
                            }),
                            _ => {}
                        }
                    }
                }
                _ => {}
            }
        }
        for e in &edits {
            // Edits keep every span in place, so the spans stay valid.
            next = next.apply(e)?.asset;
        }
        self.restructure(next, Some(id));
        if let Place::Canvas(p) = place {
            self.put_pivot_at(p)?;
        }
        Ok(format!("Added {name}"))
    }

    /// Moves the selected widget, just added, so its pivot sits on design
    /// point `p`, inside the step that added it: Position in its parent's
    /// space moves by the pivot's offset to `p` there. One a stack places,
    /// or without a stored Position, stays where its parent puts it.
    fn put_pivot_at(&mut self, p: [f32; 2]) -> Result<()> {
        let Some(n) = self.canvas.selected else {
            return Ok(());
        };
        let f = self.frames[n];
        let (Some(span), None) = (f.position_span, f.placed_by) else {
            return Ok(());
        };
        let Some(inv) = super::scene::inverse(f.parent_transform) else {
            return Ok(());
        };
        let (to, from) = (
            super::scene::apply(inv, p),
            super::scene::apply(inv, f.pivot_world()),
        );
        let pos = [
            f.position[0] + to[0] - from[0],
            f.position[1] + to[1] - from[1],
            f.position[2],
        ];
        self.asset = self
            .asset
            .apply(&Edit::Value {
                span,
                value: Value::Vec3(pos),
            })?
            .asset;
        self.revision += 1;
        self.relayout();
        Ok(())
    }
}

impl Editor {
    pub(super) fn ui_tree_result(&mut self, r: Result<String>) {
        match r {
            Ok(m) if m.is_empty() => {}
            Ok(m) => self.message(m, false),
            Err(e) => self.message(format!("{e:#}"), true),
        }
    }

    /// Delete, Ctrl D/C/V and Alt arrows on the selected widget.
    pub fn ui_tree_key(&mut self, event: &KeyDownEvent) -> bool {
        let Some(ui) = &mut self.ui else {
            return false;
        };
        let m = event.keystroke.modifiers;
        let ctrl = m.control || m.platform;
        let r = match event.keystroke.key.as_str() {
            // Plain Delete only: a chord (Ctrl+Delete deletes a word in a
            // field) never deletes a widget.
            "delete" if !ctrl && !m.alt && !m.shift => ui.delete_selected(),
            "d" if ctrl => ui.duplicate_selected(),
            "c" if ctrl => match ui.copy_selected() {
                Ok(c) => {
                    let name = c.name.clone().unwrap_or_default();
                    self.ui_clipboard = Some(c);
                    Ok(format!(
                        "Copied {name}; Ctrl V pastes it here or in another file"
                    ))
                }
                Err(e) => Err(e),
            },
            "v" if ctrl => match self.ui_clipboard.clone() {
                Some(c) => ui.paste(&c),
                None => Ok(String::new()),
            },
            "up" if m.alt => ui.step_selected(Step::Up),
            "down" if m.alt => ui.step_selected(Step::Down),
            "left" if m.alt => ui.step_selected(Step::Out),
            "right" if m.alt => ui.step_selected(Step::In),
            _ => return false,
        };
        self.ui_tree_result(r);
        true
    }

    pub fn render_ui_tree(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let Some(ui) = &self.ui else {
            // A note, as the Inspector beside it; a heading read as an error.
            return crate::editor::pane_note("Open a screen to see its widget tree.");
        };
        // Custom properties are listed in the inspector (Template
        // properties) and, with the records, here.
        let records = ui.show_records || ui.nodes.is_empty();
        let rows = if records { ui.rows() } else { ui.widget_rows() };
        let searching = !records && !ui.tree_filter.trim().is_empty();
        let hits = if searching {
            ui.tree_matches().len()
        } else {
            0
        };
        let labels: Rc<Vec<(String, String, bool)>> = Rc::new(
            rows.iter()
                .map(|row| match row.node {
                    Row::Widget(n) => {
                        let node = &ui.nodes[n];
                        let kind = match node.kind {
                            canvas::Kind::Image => "image",
                            canvas::Kind::Label => "label",
                            canvas::Kind::Container => "group",
                        };
                        let mut detail =
                            match states::state_of(&ui.state_groups, &ui.chosen_states, n) {
                                Some((_, true)) => format!("state ● {kind}"),
                                Some((_, false)) => format!("state ○ {kind}"),
                                None if states::is_helper(node) => format!("runtime {kind}"),
                                None => kind.to_owned(),
                            };
                        // A short mark for "from a template", not a word that
                        // pushes the kind into an ellipsis on every row.
                        if node.foreign {
                            detail = format!("↳ {detail}");
                        }
                        (node.name.to_string(), detail, ui.canvas.selected == Some(n))
                    }
                    Row::Node(i) => {
                        let n = &ui.asset.nodes()[i];
                        let id = match n.header.id {
                            ObjectId::Root(_) => "root".to_owned(),
                            ObjectId::Id(g) => g.to_string()[..8].to_owned(),
                            ObjectId::None => String::new(),
                        };
                        (
                            ui.name(n.header.class),
                            format!("{id} · {}", n.records.len()),
                            ui.selected == Some(n.span),
                        )
                    }
                    Row::Record(s) => (
                        ui.describe(s),
                        format!("@{:x}", ui.asset.spans()[s].range.start),
                        ui.selected == Some(s),
                    ),
                    Row::Property(p) => {
                        let prop = &ui.asset.properties()[p];
                        (
                            format!("◇ {}", ui.asset.property_name(prop)),
                            format!(
                                "{} = {}",
                                phx::property_type_name(prop.kind).unwrap_or("?"),
                                super::property_default_text(&ui.asset, prop)
                            ),
                            ui.selected == Some(prop.default),
                        )
                    }
                })
                .collect(),
        );
        let rows = Rc::new(rows);
        let item = {
            let (rows, labels) = (rows.clone(), labels.clone());
            move |i: usize| {
                let (label, detail, selected) = labels[i].clone();
                tree::Item {
                    depth: rows[i].depth,
                    disclosure: rows[i].disclosure,
                    label: label.into(),
                    detail: detail.into(),
                    selected,
                }
            }
        };
        let click = {
            let rows = rows.clone();
            move |this: &mut Self, i: usize, _: &ClickEvent, _: &mut Context<Self>| {
                this.keys_to(super::Keys::Canvas);
                let Some(ui) = &mut this.ui else { return };
                match rows[i].node {
                    Row::Widget(n) => ui.select_widget(n),
                    Row::Node(n) => {
                        ui.tree.toggle(n);
                        ui.selected = Some(ui.asset.nodes()[n].span);
                    }
                    Row::Record(s) => ui.selected = Some(s),
                    Row::Property(p) => ui.selected = Some(ui.asset.properties()[p].default),
                }
            }
        };
        let toggle = {
            let rows = rows.clone();
            move |this: &mut Self, i: usize, _: &mut Context<Self>| {
                let Some(ui) = &mut this.ui else { return };
                match rows[i].node {
                    Row::Widget(n) => ui.widget_tree.toggle(n),
                    Row::Node(n) => ui.tree.toggle(n),
                    _ => {}
                }
            }
        };
        let decorate = {
            let (rows, labels) = (rows.clone(), labels.clone());
            let entity = cx.entity().downgrade();
            let own: Rc<super::Bits> = Rc::new(
                rows.iter()
                    .map(|r| matches!(r.node, Row::Widget(n) if !ui.nodes[n].foreign))
                    .collect(),
            );
            move |i: usize, row: Stateful<Div>| {
                let Row::Widget(node) = rows[i].node else {
                    return row;
                };
                if !own.get(i) {
                    return row;
                }
                let label: SharedString = labels[i].0.clone().into();
                let (entity, library) = (entity.clone(), entity.clone());
                row.on_drag(WidgetDrag(node), move |_, _, _, cx| {
                    cx.new(|_| DragLabel(label.clone()))
                })
                .drag_over::<WidgetDrag>(|d, _, _, _| d.bg(theme::get().selected))
                .on_drop(move |drag: &WidgetDrag, _, cx| {
                    let _ = entity.update(cx, |this, cx| {
                        if let Some(ui) = &mut this.ui {
                            let r = ui.drop_widget(drag.0, node);
                            this.ui_tree_result(r);
                        }
                        cx.notify();
                    });
                })
                // A widget library row: into this group, or before it.
                .drag_over::<super::library::LibraryDrag>(|d, _, _, _| d.bg(theme::get().selected))
                .on_drop(move |drag: &super::library::LibraryDrag, _, cx| {
                    let _ = library.update(cx, |this, cx| {
                        this.add_library_pick(drag.0, Place::Tree(node));
                        cx.notify();
                    });
                })
            }
        };
        let list = tree::List::new("ui-rows", rows.len(), item, click)
            .on_toggle(toggle)
            .decorate(decorate)
            .render(
                if ui.show_records || ui.nodes.is_empty() {
                    &ui.tree
                } else {
                    &ui.widget_tree
                },
                window,
                cx,
            )
            .flex_1()
            .min_h_0();

        let a = &ui.asset;
        let info = div().px_2().py_1().text_size(px(11.)).child(
            // What the file is and how many widgets it holds, counted as
            // the file list counts them (the root is the screen itself).
            div().w_full().text_color(theme::get().muted).child(format!(
                "{} · {}{}",
                {
                    let mut kind = super::files::kind_label(&a.header().kind_str()).to_owned();
                    if let Some(first) = kind.get_mut(..1) {
                        first.make_ascii_uppercase();
                    }
                    kind
                },
                // Its own widgets, and how many more its templates bring
                // (the ↳ rows), so the count matches the tree.
                {
                    let own = ui
                        .nodes
                        .iter()
                        .filter(|n| !n.foreign)
                        .count()
                        .saturating_sub(1);
                    let borrowed = ui.nodes.iter().filter(|n| n.foreign).count();
                    let own = crate::project::plural(own, "own widget", "own widgets");
                    if borrowed > 0 {
                        format!(
                            "{own} · ↳ {} from templates",
                            crate::project::count(borrowed)
                        )
                    } else {
                        own
                    }
                },
                if ui.editable() {
                    ""
                } else {
                    " · Read-only: unknown schema"
                }
            )),
        );
        // What the canvas previews is secondary to the tree: folded until
        // asked for.
        let (options_heading, options_open) =
            widgets::collapsible("ui-tree-options", "Preview options", false, window, cx);
        let toggles = div()
            .flex()
            .flex_wrap()
            .items_center()
            .gap_1()
            .px_1()
            .pb_1()
            .text_size(px(11.))
            .child(
                checkbox("Design view", ui.canvas.design_view, "").w(relative(0.5))
                    .tooltip(Tooltip::text(
                        "Design view",
                        "",
                        "Show widgets that start transparent and fade in at runtime",
                    ))
                    .on_click(cx.listener(|this, _, _, cx| {
                        if let Some(ui) = &mut this.ui {
                            ui.canvas.design_view = !ui.canvas.design_view;
                            ui.relayout();
                            ui.relayout_screens();
                        }
                        cx.notify();
                    })),
            )
            .child(
                checkbox("Runtime helpers", ui.show_helpers, "").w(relative(0.5))
                    .tooltip(Tooltip::text(
                        "Runtime helpers",
                        "",
                        "Show blockers, scroll bars, focus frames and items the game spawns into lists",
                    ))
                    .on_click(cx.listener(|this, _, _, cx| {
                        if let Some(ui) = &mut this.ui {
                            ui.show_helpers = !ui.show_helpers;
                            ui.relayout();
                            ui.relayout_screens();
                        }
                        cx.notify();
                    })),
            )
            .child(
                checkbox("Data bindings", ui.bindings, "").w(relative(0.5))
                    .tooltip(Tooltip::text(
                        "Data bindings",
                        "",
                        "Show what the template’s bindings put over the stored values (copies only; converters are not run)",
                    ))
                    .on_click(cx.listener(|this, _, _, cx| {
                        if let Some(ui) = &mut this.ui {
                            ui.bindings = !ui.bindings;
                            ui.relayout();
                            ui.relayout_screens();
                        }
                        cx.notify();
                    })),
            )
            .child(
                checkbox("Opening pose", ui.shown, "").w(relative(0.5))
                    .tooltip(Tooltip::text(
                        "Opening pose",
                        "",
                        "Apply each file’s opening clip (Anim_In, FadeIn, Show…) at its end, as screens look once opened",
                    ))
                    .on_click(cx.listener(|this, _, _, cx| {
                        if let Some(ui) = &mut this.ui {
                            ui.shown = !ui.shown;
                            ui.relayout();
                            ui.relayout_screens();
                        }
                        cx.notify();
                    })),
            )
            .child(
                checkbox("Sample data", ui.sample, "").w(relative(0.5))
                    .tooltip(Tooltip::text(
                        "Sample data",
                        "",
                        "Give template properties nothing sets a plausible game value: state flags such as IsLocked off, texts filled",
                    ))
                    .on_click(cx.listener(|this, _, _, cx| {
                        if let Some(ui) = &mut this.ui {
                            ui.sample = !ui.sample;
                            ui.relayout();
                            ui.relayout_screens();
                        }
                        cx.notify();
                    })),
            )
            .child(
                checkbox("Raw records", ui.show_records, "").w(relative(0.5)).on_click(cx.listener(|this, _, _, cx| {
                    if let Some(ui) = &mut this.ui {
                        ui.show_records = !ui.show_records;
                    }
                    cx.notify();
                })),
            )
            .when(ui.show_records, |d| {
                d.child(
                    checkbox("Opaque records", ui.show_opaque, "").w(relative(0.5)).on_click(cx.listener(|this, _, _, cx| {
                        if let Some(ui) = &mut this.ui {
                            ui.show_opaque = !ui.show_opaque;
                        }
                        cx.notify();
                    })),
                )
            });
        // Commands on the selected widget, in words; the arrows keep their
        // glyphs with the move spelled out in the tooltip.
        // Commands that act on the selection stay in place but dim without
        // one, so they do not look ready.
        let why_off = match ui.selected_own() {
            Ok(_) => None,
            Err(e) => Some(e.to_string()),
        };
        let action = |label: &'static str,
                      tip: &'static str,
                      keys: &'static str,
                      f: fn(&mut UiEditor) -> Result<String>| {
            widgets::action(
                label,
                if label == "Delete" {
                    widgets::Tone::Danger
                } else {
                    widgets::Tone::Secondary
                },
            )
            .h(px(24.))
            .px_2()
            .text_size(px(11.))
            .tooltip(Tooltip::text(label, keys, tip))
            .on_click(cx.listener(move |this, _, _, cx| {
                if let Some(ui) = &mut this.ui {
                    let r = f(ui);
                    this.ui_tree_result(r);
                }
                cx.notify();
            }))
        };
        let on_selection = |label: &'static str,
                            tip: &'static str,
                            keys: &'static str,
                            f: fn(&mut UiEditor) -> Result<String>| {
            match &why_off {
                Some(why) => widgets::action_off(label, why.clone())
                    .h(px(24.))
                    .px_2()
                    .text_size(px(11.)),
                None => action(label, tip, keys, f),
            }
        };
        // Two labelled rows: what can be added, and what acts on the
        // selection; a wrap that strands one button reads as a glitch.
        let caption = |text: &'static str| {
            div()
                .w(px(56.))
                .flex_shrink_0()
                .text_size(px(10.))
                .text_color(theme::get().muted)
                .child(text)
        };
        let actions = div()
            .flex()
            .flex_col()
            .gap_1()
            .px_2()
            .pb_1()
            .text_size(px(11.))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_1()
                    .child(caption("Add"))
                    .child(action(
                        "Image",
                        "Add an image to the selected group, or after the selected widget",
                        "",
                        |ui| ui.add_widget(NewWidget::Image),
                    ))
                    .child(action(
                        "Label",
                        "Add a text label to the selected group, or after the selected widget",
                        "",
                        |ui| ui.add_widget(NewWidget::Label),
                    ))
                    .child(action(
                        "Group",
                        "Add an empty group to hold other widgets",
                        "",
                        |ui| ui.add_widget(NewWidget::Group),
                    )),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_1()
                    .child(caption("Selection"))
                    .child(on_selection(
                        "Duplicate",
                        "Duplicate the selected widget with everything in it",
                        "Ctrl D",
                        UiEditor::duplicate_selected,
                    ))
                    .child(on_selection(
                        "Delete",
                        "Delete the selected widget with everything in it",
                        "Delete",
                        UiEditor::delete_selected,
                    )),
            )
            // Its own captioned row, on the same column as Add and
            // Selection, in words where arrows were cryptic.
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_1()
                    .child(caption("Order"))
                    .child(on_selection(
                        "Up",
                        "Move up: earlier among its siblings (drawn below them)",
                        "Alt ↑",
                        |ui| ui.step_selected(Step::Up),
                    ))
                    .child(on_selection(
                        "Down",
                        "Move down: later among its siblings (drawn above them)",
                        "Alt ↓",
                        |ui| ui.step_selected(Step::Down),
                    ))
                    .child(on_selection(
                        "Outdent",
                        "Out of group: after the group it is in",
                        "Alt ←",
                        |ui| ui.step_selected(Step::Out),
                    ))
                    .child(on_selection(
                        "Indent",
                        "Into group: the group just above it",
                        "Alt →",
                        |ui| ui.step_selected(Step::In),
                    )),
            );
        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(theme::get().panel)
            // A plain title, not the header strip, whose fill read as a
            // selected row above the tree.
            .child(
                div()
                    .px_2()
                    .pt_2()
                    .text_size(px(13.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(self.ui_find.display_name(&ui.path).unwrap_or_else(|| {
                        ui.path.rsplit('/').next().unwrap_or_default().to_owned()
                    })),
            )
            .child(info)
            .child(div().px_2().child(options_heading.mt_0()))
            .when(options_open, |d| d.child(toggles))
            .child(actions)
            .when(!records, |d| {
                let filter = ui.tree_filter.clone();
                let typing = ui
                    .editing
                    .as_ref()
                    .filter(|e| e.field == TREE_FILTER)
                    .map(|e| e.display());
                d.child(
                    div()
                        .px_2()
                        .pb_1()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(
                            text_field("ui-tree-filter", &filter, typing, "Find widgets by name")
                                .flex_1()
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    // Already typing here: the press places the caret.
                                    let typing = |ui: &super::UiEditor| {
                                        ui.editing.as_ref().is_some_and(|e| e.field == TREE_FILTER)
                                    };
                                    if this.ui.as_ref().is_some_and(typing) {
                                        return;
                                    }
                                    this.focus.focus(window, cx);
                                    // One field types at a time.
                                    this.keys_to(super::Keys::Canvas);
                                    if let Some(ui) = &mut this.ui {
                                        ui.editing = Some(TextEdit::new(
                                            TREE_FILTER,
                                            filter.clone(),
                                            Filter::Text,
                                        ));
                                    }
                                    cx.notify();
                                })),
                        )
                        .when(searching, |d| {
                            d.child(
                                div()
                                    .text_size(px(10.))
                                    .text_color(theme::get().muted)
                                    .child(match hits {
                                        0 => "none".to_owned(),
                                        n => crate::project::plural(n, "match", "matches"),
                                    }),
                            )
                        }),
                )
            })
            .child(list)
            .child(
                div()
                    .px_2()
                    .py_1()
                    .text_size(px(10.))
                    .text_color(theme::get().muted)
                    .child("Drag onto a group to nest a widget, onto a widget to reorder."),
            )
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::{NewWidget, Row, Step, TREE_FILTER};
    use crate::ui::UiEditor;
    use crate::ui::test_support::{
        ICON_ROW, TECH_TREE, bindings, key, movable, node, open, reopen, select,
    };
    use gpui_editor_kit::field::{Filter, TextEdit};

    fn own_names(ui: &UiEditor) -> Vec<String> {
        ui.asset
            .widgets()
            .iter()
            .skip(1)
            .map(|w| w.name.clone().unwrap_or_default())
            .collect()
    }

    // Needs the game: a duplicate of the icon row's Icon draws like the
    // original (24×24, shown, its image), because the four bindings that
    // set it come along onto the copy; the file reads back with them; one
    // undo takes the copy and its bindings out. Round 4 got a hidden 0×0.
    // Undoing a delete selects the widget it brings back, redoing it the
    // neighbour the delete moved to; the canvas outline, the inspector and
    // the widget an edit goes to stay one widget (round 12: the undo
    // selected the neighbour).
    #[test]
    fn duplicating_and_deleting_the_icon_undo_whole() {
        let Some(mut ui) = open(ICON_ROW) else {
            return;
        };
        let count = |ui: &UiEditor| bindings(&ui.asset).bindings.len();
        let before = count(&ui);
        let icon = node(&ui, "Icon");
        let original = ui.nodes[icon].clone();
        let mut follows = ui.follows(icon);
        ui.select_widget(icon);
        let said = ui.duplicate_selected().unwrap();
        assert_eq!(said, "Duplicated Icon with 4 bindings");
        assert_eq!(count(&ui), before + 4);
        let copy = ui.canvas.selected.expect("the copy is selected");
        assert_ne!(copy, icon);
        let n = &ui.nodes[copy];
        assert!(n.visible, "the copy shows");
        assert_eq!(n.rect[2..], original.rect[2..], "and is as big");
        assert_eq!(n.text, original.text, "with the same image");
        let mut copied = ui.follows(copy);
        follows.sort();
        copied.sort();
        assert_eq!(copied, follows, "fed as the original is");
        // What Save writes parses back with the copies in it, each the
        // original but for where it writes: same mode, converter, rules.
        let table = bindings(&reopen(&ui.asset));
        assert_eq!(table.bindings.len(), before + 4);
        let settings = |b: &anno_formats::phx::binding::Binding| {
            (b.mode, b.converter, b.rules.clone(), b.entry, b.enum_rule)
        };
        let (old, new) = table.bindings.split_at(before);
        for b in new {
            let twin = old
                .iter()
                .find(|o| settings(o) == settings(b) && o.sources.len() == b.sources.len())
                .expect("a copy has an original");
            assert_ne!(twin.targets, b.targets, "the copy writes elsewhere");
        }
        assert!(ui.undo(false));
        assert_eq!(count(&ui), before);

        let agree = |ui: &UiEditor| {
            let n = ui.canvas.selected.expect("a selection");
            let node = &ui.nodes[n];
            let widgets = ui.asset.widgets();
            let w = widgets
                .iter()
                .position(|w| Some(w.object) == ui.selected)
                .expect("the inspector's widget");
            assert_eq!(
                node.widget, w,
                "outline {} vs inspector {:?}",
                node.name, widgets[w].name
            );
            assert_eq!(Some(&*node.name), widgets[w].name.as_deref());
            node.name.to_string()
        };
        select(&mut ui, "Icon");
        ui.delete_selected().unwrap();
        let next = agree(&ui);
        assert_ne!(next, "Icon");
        assert!(ui.undo(false));
        assert_eq!(agree(&ui), "Icon", "what the undo brought back");
        assert!(ui.undo(true));
        assert_eq!(agree(&ui), next, "where the delete went");
    }

    // Needs the game: add, duplicate, move and delete on a real template,
    // each one undo step.
    #[test]
    fn tree_edits_round_trip_through_undo() {
        let Some(mut ui) = open("a4f82d26-29a8-41b5-a58d-216ce59f5e88") else {
            return;
        };
        let original = ui.asset.encode();
        let before = own_names(&ui);

        // Nothing selected: a new image goes at the end of the root.
        ui.canvas.selected = None;
        ui.add_widget(NewWidget::Image).unwrap();
        let names = own_names(&ui);
        assert_eq!(names.len(), before.len() + 1);
        let sel = ui.canvas.selected.expect("the new widget is selected");
        assert_eq!(&*ui.nodes[sel].name, "NewImage");

        // Duplicate it, then move the copy up, then delete it.
        ui.duplicate_selected().unwrap();
        assert_eq!(own_names(&ui).len(), before.len() + 2);
        ui.step_selected(Step::Up).unwrap();
        ui.delete_selected().unwrap();
        assert_eq!(own_names(&ui).len(), before.len() + 1);

        // A new group starts empty and can take the image.
        ui.canvas.selected = None;
        ui.add_widget(NewWidget::Group).unwrap();
        let group = ui.canvas.selected.unwrap();
        assert_eq!(&*ui.nodes[group].name, "NewGroup");
        assert!(
            ui.nodes
                .get(group + 1)
                .is_none_or(|n| n.depth <= ui.nodes[group].depth),
            "no children"
        );

        for _ in 0..5 {
            assert!(ui.undo(false));
        }
        assert_eq!(ui.asset.encode(), original);
    }

    // Needs the game: a widget added to the minimap shows, fully opaque,
    // whatever the shipped widget it is copied from looked like.
    #[test]
    fn added_widgets_are_visible() {
        let Some(mut ui) = open("minimapscene") else {
            return;
        };
        for kind in [NewWidget::Image, NewWidget::Label] {
            let message = ui.add_widget(kind).unwrap();
            let name = message.trim_start_matches("Added ").to_owned();
            let n = node(&ui, &name);
            assert!(ui.nodes[n].visible, "{name} shows");
            assert!((ui.nodes[n].opacity - 1.).abs() < 1e-3, "{name} is opaque");
        }
    }

    // Needs the game: dropping a widget onto another group in the tree
    // keeps it where it is drawn (its Position is worked out in the new
    // parent's space); undo puts both back.
    #[test]
    fn reparenting_keeps_the_widget_in_place() {
        let Some(mut ui) = open(TECH_TREE) else {
            return;
        };
        let widgets = ui.asset.widgets();
        let image = (1..ui.nodes.len())
            .find(|&n| movable(&ui, n) && ui.nodes[n].depth == 1 && ui.nodes[n].image.is_some())
            .expect("an image at the top level");
        // A group that does not stack its children, and is moved or scaled
        // (else keeping the place would be trivial).
        let group = (1..ui.nodes.len())
            .find(|&n| {
                let w = ui.nodes[n].widget;
                !ui.nodes[n].foreign
                    && n != image
                    && ui.asset.child_group(&widgets, w).is_some()
                    && ui.frames[n].transform != [1., 0., 0., 1., 0., 0.]
                    && (n + 1..ui.nodes.len())
                        .take_while(|&c| ui.nodes[c].depth > ui.nodes[n].depth)
                        .all(|c| ui.frames[c].placed_by.is_none())
            })
            .expect("a moved group");
        let (name, before) = (ui.nodes[image].name.clone(), ui.nodes[image].rect);
        ui.drop_widget(image, group).unwrap();
        let after = ui.nodes[node(&ui, &name)].rect;
        for k in 0..4 {
            assert!((after[k] - before[k]).abs() < 1., "{before:?} -> {after:?}");
        }
        assert!(ui.undo(false));
    }

    // Needs the game: searching the construction menu's 1,100 template
    // widgets lists the matches with the groups they sit in, Enter selects
    // the first and Esc brings the whole tree back.
    #[test]
    fn search_narrows_the_tree_to_matches() {
        let Some(mut ui) = open("constructionmenuscene") else {
            return;
        };
        let all = ui.widget_rows().len();
        ui.editing = Some(TextEdit::new(TREE_FILTER, String::new(), Filter::Text));
        for c in ["s", "h", "a", "d", "o", "w"] {
            assert!(ui.key_down(&key(c)));
        }
        let matches = ui.tree_matches();
        assert!(!matches.is_empty());
        assert!(
            matches
                .iter()
                .all(|&m| ui.nodes[m].name.to_lowercase().contains("shadow"))
        );
        let rows: Vec<usize> = ui
            .widget_rows()
            .iter()
            .map(|r| match r.node {
                Row::Widget(n) => n,
                _ => unreachable!(),
            })
            .collect();
        // Every match shows, and every other row holds one.
        assert!(matches.iter().all(|m| rows.contains(m)));
        for &r in &rows {
            let holds = matches.iter().any(|&m| {
                let mut at = Some(m);
                while let Some(i) = at {
                    if i == r {
                        return true;
                    }
                    at = ui.parents[i];
                }
                false
            });
            assert!(holds, "{} holds no match", ui.nodes[r].name);
        }
        assert!(rows.len() < all || all < matches.len());
        assert!(ui.key_down(&key("enter")));
        assert_eq!(ui.canvas.selected, Some(matches[0]));
        // Esc after Enter: the whole tree again, the found widget kept.
        assert!(ui.key_down(&key("escape")));
        assert!(ui.tree_filter.is_empty());
        assert_eq!(ui.canvas.selected, Some(matches[0]));
        ui.editing = Some(TextEdit::new(
            TREE_FILTER,
            "shadow".to_owned(),
            Filter::Text,
        ));
        assert!(ui.key_down(&key("escape")));
        // The whole tree again, opened down to the widget found.
        assert!(ui.tree_filter.is_empty());
        let rows = ui.widget_rows();
        assert!(rows.len() >= all);
        assert!(rows.iter().any(|r| r.node == Row::Widget(matches[0])));
    }
}
