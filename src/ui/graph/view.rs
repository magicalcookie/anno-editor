// The Bindings pane's graph on screen: its state (pan, zoom, drags, the
// remove menu, the colour picker), the graph shown and framed, its keys,
// and drawing nodes, pins and wires.
use super::layout::{FileKey, FitKey, Held, LINE_H, TITLE_H, ViewKey};
use super::wiring::{Drop, Source, UnbindMenu};
use super::{Graph, Kind, Node};
use crate::editor::Editor;
use crate::ui::bindings::{Target, plain};
use crate::ui::{Keys, UiEditor};
use anno_formats::phx::{DefaultValue, binding::Converter};
use anyhow::Result;
use gpui_editor_kit::{
    gpui::{prelude::*, *},
    paint, theme,
    widgets::Tooltip,
};
use std::collections::HashMap;

/// Screen pixels from a pin a dragged wire still lands on it.
const PIN_REACH: f32 = 14.;

/// Pane pixels a wire must be dragged before its release is a drop; less
/// is a click on the pin.
const WIRE_START: f32 = 4.;

/// A wire being dragged from an output pin.
#[derive(Clone, Debug)]
pub(super) struct Wiring {
    /// The place the pin stands for, and what it holds.
    pub(super) source: Source,
    pub(super) kind: Option<u32>,
    pub(super) from: [f32; 2],
    pub(super) at: [f32; 2],
    /// Where the press was (pane pixels): a release near it is a click.
    pub(super) start: [f32; 2],
    /// What a drop on each of `GraphView::drops` would do, worked out when
    /// the drag starts: the pins it can land on light up, the rest dim.
    pub(super) verdicts: Vec<Result<String, String>>,
}

/// A colour picker over the Bindings pane (graph or List): the colour
/// constant's span, where it opens (window pixels), the colour, whether it
/// changed anything yet (all its changes are one undo step), and the undo
/// steps it made (Esc takes them back; a save in between makes two).
#[derive(Clone, Debug)]
pub(in crate::ui) struct ColorPick {
    span: usize,
    at: Point<Pixels>,
    hsv: gpui_editor_kit::widgets::Hsv,
    changed: bool,
    steps: usize,
    /// What it edits ("then of if / else → Icon · ColorTint") and, when a
    /// pick cannot show on the canvas, why (`UiEditor::pick_context`).
    about: String,
    idle: Option<String>,
}

impl ColorPick {
    pub(super) fn new(span: usize, at: Point<Pixels>, rgb: [u8; 3]) -> Self {
        Self {
            span,
            at,
            hsv: gpui_editor_kit::widgets::Hsv::from_rgb(rgb),
            changed: false,
            steps: 0,
            about: String::new(),
            idle: None,
        }
    }
}

/// What a cached graph was built for: file, widget, What it feeds, open
/// pins, whether the file is edited here, layout generation.

type GraphKey = (FileKey, Option<usize>, bool, Vec<Target>, bool, u64);

/// A node being dragged by its title.
#[derive(Clone, Debug)]
pub(super) struct Moving {
    file: Option<anno_formats::phx::Guid>,
    key: String,
    /// Where the drag started, and where the node was then.
    start: Point<Pixels>,
    origin: [f32; 2],
}

/// Pan and zoom of the graph pane, what it was last fitted to, and a wire
/// being dragged.
#[derive(Clone, Debug)]
pub(in crate::ui) struct GraphView {
    /// The Bindings pane shows the graph rather than the list.
    pub on: bool,
    pub pan: [f32; 2],
    pub zoom: f32,
    pub bounds: Bounds<Pixels>,
    /// What the view was fitted to: file, widget (None: whole file), What
    /// it feeds, and the pane's size then (a maximized pane fits again).
    /// None fits anew.
    pub fitted: Option<FitKey>,
    /// Panned or zoomed by hand since the last fit: a new pane size keeps
    /// the view (see `frame`).
    pub(super) touched: bool,
    /// A pan in progress: where it started and the pan then.
    pub(super) drag: Option<(Point<Pixels>, [f32; 2])>,
    pub(super) wiring: Option<Wiring>,
    /// The node under the pointer: its wires light up, the rest dim.
    pub(super) hover: Option<usize>,
    /// Where the pointer last was over the graph (window pixels): Delete
    /// asks there about the node under it.
    pub(super) pointer: Point<Pixels>,
    /// Where nodes were dragged to, design units, by file and node key;
    /// the rest keep the layout's place. Tidy forgets a file's.
    pub moved: HashMap<(Option<anno_formats::phx::Guid>, String), [f32; 2]>,
    pub(super) moving: Option<Moving>,
    /// The last press moved a node: its release is not a click.
    pub(super) dragged: bool,
    /// The next fit shows the whole graph however small; a fit on its own
    /// (a new widget, a resized pane) stops where text stays readable.
    pub overview: bool,
    /// What a right-click would remove, asked before it does.
    pub(super) menu: Option<UnbindMenu>,
    /// A colour picker open on a pin's fixed value.
    pub(in crate::ui) pick: Option<ColorPick>,
    /// The last graph built, what it was built for (file, widget, What it
    /// feeds, open pins, layout generation), and where the layout put each
    /// node: its rects hold where they show (dragged, held), placed anew
    /// each render without copying the graph.
    pub(super) cache: Option<(GraphKey, Graph, Vec<[f32; 2]>)>,
    /// Where every node of a view was when an edit was made in it, by node
    /// key: the relayout after the edit keeps them there instead of
    /// reflowing (round 4: a rewired node jumped two columns). Per view,
    /// the last `HELD_VIEWS` of them (round 6: a look at What it feeds and
    /// back reflowed); new nodes take the layout's place; Tidy lets go of
    /// a file's.
    pub(super) held: Vec<(ViewKey, HashMap<String, Held>)>,
    /// The view last built and its nodes' keys (hashed): a node that
    /// appears in a rebuild of the same view is revealed, held place or
    /// not (round 13: one an undo removed came back, by a redo or the same
    /// bind again, where it had been and off the pane).
    pub(super) last_keys: Option<(ViewKey, std::collections::HashSet<u64>)>,
    /// The widget All in file last outlined: a new one is revealed.
    pub(super) outlined: Option<usize>,
    /// This widget also shows what its values reach.
    pub feeds: bool,
    /// Input pins a wire can land on, as last drawn (pane pixels).
    pub(super) drops: Vec<([f32; 2], Drop)>,
}

impl Default for GraphView {
    fn default() -> Self {
        Self {
            on: false,
            pan: [20., 20.],
            zoom: 1.,
            bounds: Bounds::default(),
            fitted: None,
            touched: false,
            drag: None,
            wiring: None,
            hover: None,
            pointer: Point::default(),
            moved: HashMap::new(),
            moving: None,
            dragged: false,
            overview: false,
            menu: None,
            pick: None,
            cache: None,
            held: Vec::new(),
            last_keys: None,
            outlined: None,
            feeds: false,
            drops: Vec::new(),
        }
    }
}

impl GraphView {
    /// The binding (and gate-pair partner) of the logic node under the
    /// pointer, in the graph as last drawn.
    fn hovered_logic(&self) -> Option<(usize, Option<usize>)> {
        let (_, g, _) = self.cache.as_ref()?;
        match g.nodes.get(self.hover?)?.kind {
            Kind::Logic { binding, also, .. } => Some((binding, also)),
            _ => None,
        }
    }

    /// Drops what holds binding or node indices of the last layout: the
    /// remove menu, a wire or node drag in progress.
    pub(in crate::ui) fn forget_indices(&mut self) {
        self.menu = None;
        self.hover = None;
        self.wiring = None;
        self.moving = None;
    }

    /// Whether the box between pane points `a` and `b` (a node, or a wire's
    /// ends: a curve stays between its ends' columns and rows) is on the
    /// pane, give or take a margin. Only those become elements: at the
    /// readable zoom a 381-binding template is mostly off-pane, and
    /// building its nodes was nearly all of a pan's redraw (4 ms, now
    /// 0.2 ms). Before the pane's first layout its size is not known, and
    /// everything shows.
    pub(super) fn shows(&self, [x0, y0]: [f32; 2], [x1, y1]: [f32; 2]) -> bool {
        const MARGIN: f32 = 40.;
        let (w, h) = (
            f32::from(self.bounds.size.width),
            f32::from(self.bounds.size.height),
        );
        w < 1.
            || (x0.max(x1) >= -MARGIN
                && x0.min(x1) <= w + MARGIN
                && y0.max(y1) >= -MARGIN
                && y0.min(y1) <= h + MARGIN)
    }

    /// The view moved by hand (wheel, pan, arrows, F). The colour picker
    /// and the remove menu sit where they opened, in pane pixels, so they
    /// close rather than float over another node (round 14: the picker
    /// stayed over a Multiply after a wheel zoom).
    pub(super) fn moved_by_hand(&mut self) {
        self.touched = true;
        self.pick = None;
        self.menu = None;
    }
}

impl UiEditor {
    /// The last click was in the binding graph: its keys go there first.
    pub(crate) fn graph_focused(&self) -> bool {
        self.keys == Keys::Graph
    }

    /// Ends a wire drag released at pane point `at`: a drop ends wired or
    /// says why not; a press and release in place is a click on the pin,
    /// not a drag, and says nothing (round 5: every click on an output row
    /// put a red "Not wired" in the status bar). None: no wire, or a click.
    pub(super) fn drop_wire(&mut self, at: [f32; 2]) -> Option<Result<String>> {
        let w = self.graph.wiring.take()?;
        if (at[0] - w.start[0]).hypot(at[1] - w.start[1]) < WIRE_START {
            return None;
        }
        let near = self
            .graph
            .drops
            .iter()
            .map(|(p, d)| ((p[0] - at[0]).hypot(p[1] - at[1]), *d))
            .filter(|(d, _)| *d <= PIN_REACH)
            .min_by(|a, b| a.0.total_cmp(&b.0));
        Some(match near {
            Some((_, Drop::No(why))) => Err(anyhow::anyhow!(why)),
            Some((_, drop)) => self.wire_up(w.source, w.kind, drop),
            None => Err(anyhow::anyhow!(
                "drop the wire on a pin at the left of a node"
            )),
        })
    }

    /// A colour picked in the open picker: into its constant, live; the
    /// picker's first change is an undo step, the rest join it.
    pub(super) fn pick_colour(&mut self, hsv: gpui_editor_kit::widgets::Hsv) {
        let Some(pick) = &mut self.graph.pick else {
            return;
        };
        pick.hsv = hsv;
        let step = !std::mem::replace(&mut pick.changed, true);
        let span = pick.span;
        let before = self.history.revision();
        if let Err(e) = self.set_colour(span, hsv.rgb(), step) {
            self.message = Some((format!("Not applied: {e:#}"), true));
        }
        if self.history.revision() != before
            && let Some(pick) = &mut self.graph.pick
        {
            pick.steps += 1;
        }
    }

    /// What the picker on fixed colour `span` edits, and when a pick there
    /// changes nothing the canvas shows, why: a then or else on the branch
    /// its if does not take now, a pass if whose condition is false (round
    /// 14: a pick on the untaken branch showed nowhere and nothing said so).
    pub(super) fn pick_context(&mut self, span: usize) -> (String, Option<String>) {
        let Some(ft) = self.file_table(self.subject()) else {
            return (String::new(), None);
        };
        let Some((&slot, _)) = ft.table.constants.iter().find(|&(_, &sp)| sp == span) else {
            return (String::new(), None);
        };
        let Some((i, k)) = ft
            .table
            .bindings
            .iter()
            .enumerate()
            .find_map(|(i, b)| Some((i, b.sources.iter().position(|&s| s == slot)?)))
        else {
            return (String::new(), None);
        };
        let b = &ft.table.bindings[i];
        let target = b
            .targets
            .first()
            .map(|&t| plain(ft.label(t)))
            .unwrap_or_default();
        // The condition's argument, and whether this argument is the one
        // passed on when it is true.
        let (cond, when) = match Converter::of(b.converter) {
            Some(Converter::IfTrueFalseAssign) => (2, k == 0),
            Some(Converter::IfTrueAssign) => (1, true),
            Some(Converter::IfFalseAssign) => (1, false),
            _ => (usize::MAX, true),
        };
        // In the graph's words: "then of if / else", "value of pass if".
        let role = match cond {
            usize::MAX => "fixed value".to_owned(),
            _ => {
                let (title, names) = ft.logic(i);
                format!("{} of {title}", names[k.min(cond - 1)])
            }
        };
        let about = format!("{role} → {target}");
        let condition = ft
            .trace(i)
            .and_then(|t| t.args.get(cond).cloned())
            .map(|v| matches!(v, DefaultValue::Bool(b) if b != 0));
        let idle = (k != cond && condition.is_some_and(|c| c != when)).then(|| {
            format!(
                "Not shown now: its condition is {}, so {target} takes the other value",
                !when
            )
        });
        (about, idle)
    }

    /// Esc in the picker: the colour goes back to what it was when the
    /// picker opened, as every other Esc here cancels (round 14: Esc kept
    /// the pick and only Ctrl Z, past any later edit, took it back). Its
    /// steps are undone, so a save in between leaves the file unsaved
    /// again, and Ctrl Y picks it once more. A click elsewhere keeps it.
    /// Whether a picker was open.
    pub(in crate::ui) fn cancel_pick(&mut self) -> bool {
        let Some(pick) = self.graph.pick.take() else {
            return false;
        };
        for _ in 0..pick.steps {
            if !self.undo(false) {
                break;
            }
        }
        if pick.steps > 0 {
            self.message = Some(("Colour put back".into(), false));
        }
        true
    }

    /// Builds (once per layout and view) and places the graph the
    /// Bindings pane shows, into `graph.cache`: what feeds the selected
    /// widget, or the whole file. The file, the widget (None: all of it)
    /// and whether the file is edited here; None without a binding table.
    /// Render and the tests both go through here.
    pub(super) fn shown_graph(&mut self) -> Option<(FileKey, Option<usize>, bool)> {
        let node = self.subject();
        let file = self.node_file(node);
        let editable = self.is_edited_file(file.0) && self.editable();
        // Nothing selected: the whole file, not whichever widget stands in
        // for it.
        let widget = (!self.bindings_all && self.canvas.selected.is_some())
            .then(|| self.nodes.get(node).map(|n| n.widget))
            .flatten();
        // All in file keeps the graph it has while the selection changes
        // (a rebuild per click reflowed it); a widget's + pins are in its
        // own view, which holds the file's clips while it sends events.
        let open: Vec<Target> = match (editable, widget, self.nodes.get(node)) {
            (true, Some(_), Some(n)) if node > 0 => {
                Target::ALL.into_iter().filter(|t| t.fits(n.kind)).collect()
            }
            _ => Vec::new(),
        };
        // Built once per layout and view: hover, pan and drags only move
        // what is drawn.
        let built = (
            file,
            widget,
            self.graph.feeds,
            open.clone(),
            editable,
            self.layout_gen,
        );
        let mut fresh = false;
        let (built, mut graph, layout) = match self.graph.cache.take() {
            Some((k, g, l)) if k == built => (k, g, l),
            _ => {
                let ft = self.file_table(node)?;
                let g = Graph::build(&ft, widget, &open, editable, self.graph.feeds);
                let l = g.nodes.iter().map(|n| [n.rect[0], n.rect[1]]).collect();
                fresh = true;
                (built, g, l)
            }
        };
        let view = (file, widget, self.graph.feeds);
        self.graph.place(view, &mut graph, &layout);
        if fresh {
            let appeared = self.graph.appeared(view, &graph);
            if !appeared.is_empty() {
                self.graph.reveal_nodes(&graph, &appeared);
            }
        }
        // In All in file, the selected widget wears the outline it has in
        // This widget (round 12: nothing said where it was), and a new
        // selection is revealed, zoom kept: framing anew on every click
        // jumped the view from 0.6 to 1.6 (round 14).
        let mut subject = Vec::new();
        if widget.is_none() {
            let outlined = self
                .canvas
                .selected
                .and_then(|n| self.nodes.get(n))
                .map(|n| n.widget);
            for (i, n) in graph.nodes.iter_mut().enumerate() {
                if let Kind::Widget { widget: w, focus } = &mut n.kind {
                    *focus = Some(*w) == outlined;
                    if *focus {
                        subject.push(i);
                    }
                }
            }
            // A widget other bindings read is two nodes, and the two can be
            // a pane apart: the one its bindings set comes first. One
            // already on the pane (the node just clicked) keeps the view.
            if std::mem::replace(&mut self.graph.outlined, outlined) != outlined {
                let mut set_first = subject.clone();
                set_first.sort_by_key(|&i| graph.nodes[i].ins.is_empty());
                let rects: Vec<[f32; 4]> = set_first.iter().map(|&i| graph.nodes[i].rect).collect();
                if !rects.iter().any(|&r| self.graph.on_pane(r)) {
                    self.graph.reveal(&rects, 1);
                }
            }
        } else {
            self.graph.outlined = None;
        }
        // A new view (What it feeds adds nodes) frames what it shows, All
        // in file at the selected widget; a gesture in progress never has
        // the graph moved under it (round 6: the first fit landed mid-drag
        // and the wire missed its pin).
        let pane = self.graph.bounds.size;
        let busy =
            self.graph.wiring.is_some() || self.graph.moving.is_some() || self.graph.drag.is_some();
        if pane.width > px(20.) && !busy {
            let key = FitKey {
                file: file.0,
                widget,
                all: widget.is_none(),
                feeds: self.graph.feeds,
                size: [f32::from(pane.width) as i32, f32::from(pane.height) as i32],
            };
            // F and Fit (an overview) show everything.
            if self.graph.overview {
                subject.clear();
            }
            let size = [f32::from(pane.width), f32::from(pane.height)];
            self.graph.frame(key, || graph.opening(&subject, size));
        }
        self.graph.cache = Some((built, graph, layout));
        Some((file, widget, editable))
    }

    /// Keys while the graph has focus. Delete must not reach the canvas
    /// behind it (it would delete the selected widget); F and Home fit the
    /// graph; Esc gives the keys back. Of the chords, only the editor's own
    /// (`editor_shortcut`) go on: widget commands and Ctrl+Delete would act
    /// on the canvas or the tree behind.
    pub(crate) fn graph_key_down(&mut self, event: &KeyDownEvent) -> bool {
        let ui = self;
        let m = event.keystroke.modifiers;
        let key = event.keystroke.key.as_str();
        if crate::ui::editor_shortcut(&event.keystroke) {
            return false;
        }
        if m.control || m.platform || m.alt {
            return true;
        }
        // An open colour picker closes first, putting its colour back, over
        // the List as over the graph (the List's Esc handed the keys back
        // and left it open).
        if key == "escape" && ui.cancel_pick() {
            return true;
        }
        // The List shows no graph: its keys still stop here (nothing is
        // deleted), but they say where removal lives and leave the hidden
        // graph's view alone (round 10 answered with graph wording).
        if !ui.graph.on {
            match key {
                "space" => return false,
                "escape" => ui.hand_back = true,
                "delete" | "backspace" => {
                    ui.message = Some((
                        "To remove a binding, use its × in the inspector's Data section, or right-click it in Graph".into(),
                        false,
                    ));
                }
                _ => {}
            }
            return true;
        }
        const PAN: f32 = 60.;
        match key {
            // Play and pause work wherever the keys are.
            "space" => return false,
            "delete" | "backspace" if ui.graph.menu.is_some() => ui.graph.menu = None,
            // Over a logic node: the remove menu, as a right-click there;
            // only in the file being edited (a template's binding indices
            // are not this asset's).
            "delete" | "backspace"
                if ui.editable()
                    && ui.is_edited_file(ui.node_file(ui.subject()).0)
                    && let Some((binding, also)) = ui.graph.hovered_logic() =>
            {
                let at = ui.graph.pointer;
                ui.ask_unbind(at, [Some(binding), also].into_iter().flatten().collect());
            }
            "delete" | "backspace" => {
                ui.message = Some((
                    "In the graph, right-click a logic node or a wired pin to remove it".into(),
                    false,
                ));
            }
            "f" | "home" => {
                ui.graph.moved_by_hand();
                ui.graph.fitted = None;
                ui.graph.overview = true;
            }
            // A wire being dragged goes first; its release then says nothing.
            "escape" if ui.graph.wiring.is_some() => ui.graph.wiring = None,
            "escape" if ui.graph.menu.is_some() => ui.graph.menu = None,
            "escape" => ui.hand_back = true,
            // Arrows pan the graph: on the canvas they move the widget.
            "left" | "right" | "up" | "down" => {
                let (axis, by) = match key {
                    "left" => (0, PAN),
                    "right" => (0, -PAN),
                    "up" => (1, PAN),
                    _ => (1, -PAN),
                };
                ui.graph.pan[axis] += by;
                ui.graph.moved_by_hand();
            }
            // Every other plain key stops here too (F frames, nudges and
            // the like are the canvas's).
            _ => {}
        }
        true
    }

    /// Closes the colour picker and the remove menu: they belong to the
    /// pane as it showed when they opened (round 14: the picker outlived a
    /// switch to another workspace and back).
    pub(crate) fn close_popovers(&mut self) {
        self.graph.pick = None;
        self.graph.menu = None;
    }
}

/// What the editor's window tests (editor/tests.rs) read of the graph.
#[cfg(test)]
impl UiEditor {
    /// Opens the colour picker on constant `span`, as a swatch press does.
    pub(crate) fn open_pick_for_test(&mut self, span: usize, rgb: [u8; 3]) {
        self.graph.pick = Some(ColorPick::new(span, Point::default(), rgb));
    }

    pub(crate) fn picking(&self) -> bool {
        self.graph.pick.is_some()
    }

    pub(crate) fn show_all_in_file(&mut self) {
        self.graph.on = true;
        self.bindings_all = true;
    }

    pub(crate) fn graph_zoom(&self) -> f32 {
        self.graph.zoom
    }

    /// Where input `pin` of the first node titled `title` (whose subtitle
    /// holds `sub`) was last drawn: a point on its row (window pixels).
    pub(crate) fn pin_row_at(&self, title: &str, sub: &str, pin: &str) -> Option<Point<Pixels>> {
        let g = &self.graph.cache.as_ref()?.1;
        let (n, k) = g.nodes.iter().enumerate().find_map(|(n, node)| {
            (node.title == title && node.sub.contains(sub))
                .then(|| node.ins.iter().position(|p| p.name == pin).map(|k| (n, k)))
                .flatten()
        })?;
        let [x, y] = self.graph.to_screen(g.in_at(n, k));
        Some(self.graph.bounds.origin + Point::new(px(x + 20.), px(y)))
    }

    /// The bindings the open remove menu would remove.
    pub(crate) fn unbind_menu(&self) -> Option<Vec<usize>> {
        self.graph.menu.as_ref().map(|m| m.bindings.clone())
    }

    /// The remove menu's Remove, as its button does.
    pub(crate) fn remove_from_menu(&mut self) -> anyhow::Result<String> {
        let menu = self.graph.menu.take().expect("a remove menu");
        self.unbind(&menu.bindings)
    }

    /// Titles of the graph's logic nodes as last built.
    pub(crate) fn logic_titles(&self) -> Vec<String> {
        let Some((_, g, _)) = &self.graph.cache else {
            return Vec::new();
        };
        g.nodes
            .iter()
            .filter(|n| matches!(n.kind, Kind::Logic { .. }))
            .map(|n| n.title.clone())
            .collect()
    }

    pub(crate) fn binding_count(&self) -> usize {
        self.asset.bindings(&self.asset.widgets()).bindings.len()
    }

    /// The middle of the graph pane as last drawn (window pixels).
    pub(crate) fn graph_middle(&self) -> Point<Pixels> {
        self.graph.bounds.center()
    }

    pub(crate) fn selected_name(&self) -> Option<String> {
        Some(self.nodes.get(self.canvas.selected?)?.name.to_string())
    }

    /// Where the title of a widget node `title` was last drawn (window
    /// pixels), the first of its copies that shows.
    pub(crate) fn node_title_at(&self, title: &str) -> Option<Point<Pixels>> {
        let g = &self.graph.cache.as_ref()?.1;
        let b = self.graph.bounds;
        g.nodes
            .iter()
            .filter(|n| n.title == title && matches!(n.kind, Kind::Widget { .. }))
            .find_map(|n| {
                let [x, y] = self
                    .graph
                    .to_screen([n.rect[0] + 10., n.rect[1] + TITLE_H / 2.]);
                (x > 0. && y > 0. && x < f32::from(b.size.width) && y < f32::from(b.size.height))
                    .then(|| b.origin + Point::new(px(x), px(y)))
            })
    }

    /// The span of the fixed value on pin `pin` of the first graph node
    /// whose title holds `title`, as the Bindings graph shows it now.
    pub(crate) fn pin_constant(&mut self, title: &str, pin: &str) -> Option<usize> {
        self.shown_graph()?;
        let g = &self.graph.cache.as_ref()?.1;
        g.nodes
            .iter()
            .filter(|n| n.title.contains(title))
            .flat_map(|n| &n.ins)
            .find(|p| p.name == pin)
            .and_then(|p| p.constant)
    }
}

impl Editor {
    /// The Bindings pane as a graph: what feeds the selected widget, or the
    /// whole file. Drag empty space to pan, wheel to zoom; drag from an
    /// output pin onto an input pin to wire them; right-click an input pin
    /// fed by a copy, or a logic node, to remove that binding.
    pub(in crate::ui) fn render_binding_graph(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let gc = &crate::colors::get().graph;
        let Some(ui) = &mut self.ui else {
            return div().into_any_element();
        };
        let Some((file, _, editable)) = ui.shown_graph() else {
            return div().into_any_element();
        };
        // Out of the view while the view is copied: a copy of a
        // 381-binding graph per hover was most of a redraw.
        let (built, graph, layout) = ui.graph.cache.take().expect("just shown");
        // Where wires can land, for the drop at the end of a drag.
        ui.graph.drops = graph
            .drop_targets()
            .into_iter()
            .map(|(n, k, drop)| (ui.graph.to_screen(graph.in_at(n, k)), drop))
            .collect();
        let view = ui.graph.clone();
        let editing = ui.editing.as_ref().map(|e| (e.field, e.display()));
        ui.graph.cache = Some((built, graph, layout));
        let graph = &ui.graph.cache.as_ref().expect("just put back").1;
        // Only what the pane shows becomes elements (see `shows`).
        let shows = |a: [f32; 2], b: [f32; 2]| view.shows(a, b);
        let mut curves: Vec<([[f32; 2]; 4], Rgba)> = graph
            .wires
            .iter()
            .filter_map(|w| {
                let a = view.to_screen(graph.out_at(w.from.0, w.from.1));
                let b = view.to_screen(graph.in_at(w.to.0, w.to.1));
                // A curve stays between its ends' columns and rows.
                if !shows(a, b) {
                    return None;
                }
                let color = match (w.event, w.live) {
                    (true, _) => gc.event,
                    (false, true) => gc.wire,
                    (false, false) => gc.wire_idle,
                };
                let color = match view.hover {
                    Some(h) if w.from.0 != h && w.to.0 != h => gc.wire_faded,
                    _ => color,
                };
                Some((paint::link(a, b), color))
            })
            .collect();
        if let Some(w) = &view.wiring {
            // White: a wire not made yet, apart from the blue ones that are.
            curves.push((paint::link(w.from, w.at), gc.wire_new));
        }
        let entity = cx.entity();
        // The view-model lanes, behind the wires and nodes: game data apart
        // from the widgets, a strip each with its heading (user, round 17).
        let lanes = graph.lanes().into_iter().map(|([l, r], label)| {
            const PAD: f32 = 10.;
            const HEAD: f32 = 26.;
            let at = view.to_screen([l - PAD, -HEAD]);
            let z = view.zoom;
            div()
                .absolute()
                .left(px(at[0]))
                .top(px(at[1]))
                .w(px((r - l + 2. * PAD) * z))
                .h(px((graph.size[1] + HEAD + PAD) * z))
                .rounded(px(4. * z))
                .bg(crate::colors::get().graph.data[0].alpha(0.25))
                .border_1()
                .border_color(crate::colors::get().graph.data[0].alpha(0.63))
                .child(
                    div()
                        .px(px(6. * z))
                        .pt(px(4. * z))
                        .text_size(px(11. * z))
                        .text_color(crate::colors::get().graph.drop_ok)
                        .whitespace_nowrap()
                        .child(label),
                )
        });
        let mut el = div()
            .id("ui-binding-graph")
            .relative()
            .size_full()
            .overflow_hidden()
            .bg(gc.canvas)
            .children(lanes)
            .child(
                canvas(
                    move |bounds, window, cx| {
                        let resized = entity.update(cx, |this, _| {
                            this.ui.as_mut().is_some_and(|ui| {
                                let resized = ui.graph.bounds.size != bounds.size;
                                ui.graph.bounds = bounds;
                                resized
                            })
                        });
                        // This frame was fitted to the old size; a notify
                        // from inside the draw is dropped, so redraw the pane
                        // next frame. Round 7: the docked graph kept a fit
                        // for a stale size until the first hover redrew it,
                        // and the zoom jumped under the pointer.
                        if resized {
                            let entity = entity.clone();
                            window.on_next_frame(move |_, cx| {
                                entity.update(cx, |this, cx| {
                                    this.notify_pane(crate::editor::Pane::UiBindings, cx)
                                })
                            });
                        }
                    },
                    move |bounds, _, window, _| {
                        let o = [f32::from(bounds.origin.x), f32::from(bounds.origin.y)];
                        for (c, color) in &curves {
                            let c = c.map(|p| [p[0] + o[0], p[1] + o[1]]);
                            paint::curve(c, 1.5, *color, window);
                        }
                    },
                )
                .absolute()
                .size_full(),
            );
        // During a drag, which input pins take the wire: `drops` lists them
        // node by node, as the verdicts do.
        let lit: Vec<Option<bool>> = match &view.wiring {
            Some(w) => w.verdicts.iter().map(|v| Some(v.is_ok())).collect(),
            None => Vec::new(),
        };
        let mut first = 0;
        for (i, n) in graph.nodes.iter().enumerate() {
            let pins = lit.get(first..first + n.ins.len()).unwrap_or_default();
            first += n.ins.len();
            let at = view.to_screen([n.rect[0], n.rect[1]]);
            let end = [at[0] + n.rect[2] * view.zoom, at[1] + n.rect[3] * view.zoom];
            if shows(at, end) {
                el = el.child(node_element(
                    i,
                    n,
                    graph,
                    &view,
                    pins,
                    file,
                    editable,
                    editing.as_ref(),
                    cx,
                ));
            }
        }
        // The pin in reach says what a drop there does, by the cursor: the
        // status bar was a screen away (round 4: `if` taken for `then`).
        if let Some(w) = &view.wiring
            && let Some((j, _)) = view
                .drops
                .iter()
                .enumerate()
                .map(|(j, (p, _))| (j, (p[0] - w.at[0]).hypot(p[1] - w.at[1])))
                .filter(|&(_, d)| d <= PIN_REACH)
                .min_by(|a, b| a.1.total_cmp(&b.1))
            && let Some(v) = w.verdicts.get(j)
        {
            let (text, color) = match v {
                Ok(t) => (t.clone(), gc.drop_ok),
                Err(t) => (format!("Not here: {t}"), gc.drop_no),
            };
            el = el.child(
                div()
                    .absolute()
                    .left(px(w.at[0] + 14.))
                    .top(px(w.at[1] - 26.))
                    .max_w(px(360.))
                    .px_2()
                    .py_1()
                    .rounded(px(3.))
                    .bg(gc.canvas.alpha(0.94))
                    .border_1()
                    .border_color(color)
                    .text_size(px(11.))
                    .text_color(color)
                    .child(text),
            );
        }
        if let Some(m) = &view.menu {
            el = el.child(unbind_menu(m, &view, cx));
        }

        let note = |text: String| {
            div()
                .absolute()
                .px_2()
                .py_1()
                .rounded(px(3.))
                .bg(crate::colors::get().graph.canvas.alpha(0.88))
                .text_size(px(10.))
                .text_color(theme::get().muted)
                .child(text)
        };
        if graph.nodes.is_empty() {
            el = el.child(
                note("No bindings read or write this widget.".into())
                    .top_2()
                    .left_2(),
            );
        }
        if graph.dropped > 0 {
            el = el.child(
                note(format!(
                    "{} more bindings not drawn; pick a widget to see what feeds it",
                    graph.dropped
                ))
                .bottom_2()
                .left_2(),
            );
        }
        // How to edit lives in the Graph button's tooltip: a note here sat
        // over nodes. Read only is worth saying where it shows.
        if !editable {
            el = el.child(
                note(
                    "Read only: these bindings belong to a template. Edit the template here to change them."
                        .into(),
                )
                .top_2()
                .right_2(),
            );
        }
        el.on_mouse_down(
            MouseButton::Left,
            cx.listener(|this, e: &MouseDownEvent, _, _| {
                this.press_keys_to(e.position, Keys::Graph);
                if let Some(ui) = &mut this.ui {
                    ui.graph.drag = Some((e.position, ui.graph.pan));
                }
            }),
        )
        // Pans, node and wire drags, zoom and hover change only this pane:
        // the rest of the window stays as drawn (a pan of the 381-binding
        // graph redrew the inspector at 4.6 ms a frame, round 6).
        .on_mouse_move(cx.listener(|this, e: &MouseMoveEvent, _, cx| {
            let Some(ui) = &mut this.ui else { return };
            ui.graph.pointer = e.position;
            if e.pressed_button != Some(MouseButton::Left) {
                ui.graph.drag = None;
                ui.graph.wiring = None;
                ui.graph.moving = None;
                return;
            }
            let at = e.position - ui.graph.bounds.origin;
            if let Some(Moving {
                file,
                key,
                start,
                origin,
            }) = ui.graph.moving.clone()
            {
                let d = e.position - start;
                let (dx, dy) = (f32::from(d.x), f32::from(d.y));
                if dx.hypot(dy) > 3. {
                    ui.graph.dragged = true;
                }
                let z = ui.graph.zoom;
                ui.graph
                    .moved
                    .insert((file, key), [origin[0] + dx / z, origin[1] + dy / z]);
                // An arrangement made by hand is framed by hand too: a resize
                // keeps it (round 13).
                ui.graph.touched = true;
            } else if let Some(w) = &mut ui.graph.wiring {
                w.at = [f32::from(at.x), f32::from(at.y)];
            } else if let Some((start, pan)) = ui.graph.drag {
                let d = e.position - start;
                ui.graph.pan = [pan[0] + f32::from(d.x), pan[1] + f32::from(d.y)];
                ui.graph.moved_by_hand();
            } else {
                return;
            }
            this.notify_pane(crate::editor::Pane::UiBindings, cx);
        }))
        .on_mouse_up(
            MouseButton::Left,
            cx.listener(|this, e: &MouseUpEvent, _, cx| {
                let Some(ui) = &mut this.ui else { return };
                ui.graph.drag = None;
                ui.graph.moving = None;
                let at = e.position - ui.graph.bounds.origin;
                if let Some(r) = ui.drop_wire([f32::from(at.x), f32::from(at.y)]) {
                    ui.message = Some(match r {
                        Ok(m) => (m, false),
                        Err(e) => (format!("Not wired: {e:#}"), true),
                    });
                }
                cx.notify();
            }),
        )
        .on_scroll(cx.listener(|this, e: &ScrollEvent, _, cx| {
            let Some(ui) = &mut this.ui else { return };
            let dy = match e.delta {
                ScrollDelta::Pixels(p) => f32::from(p.y),
                ScrollDelta::Lines(p) => p.y * 32.,
            };
            let g = &mut ui.graph;
            let at = e.position - g.bounds.origin;
            g.zoom_by(dy, [f32::from(at.x), f32::from(at.y)]);
            this.notify_pane(crate::editor::Pane::UiBindings, cx);
        }))
        .into_any_element()
    }
}

/// The colour picker over a fixed colour, under the swatch that opened it,
/// kept on the window: each pick shows on the canvas at once; Esc or a
/// click elsewhere closes it.
pub(in crate::ui) fn color_popover(
    p: &ColorPick,
    canvas_hidden: bool,
    cx: &mut Context<Editor>,
) -> AnyElement {
    let [r, g, b] = p.hsv.rgb();
    // Why a pick may show nowhere, in amber: the branch not taken, or the
    // canvas hidden behind this maximized pane.
    let idle = p.idle.clone().or_else(|| {
        canvas_hidden.then(|| "The canvas is hidden while this pane is maximized".to_owned())
    });
    let body = div()
        .id("ui-graph-pick")
        .w(px(196.))
        .bg(theme::get().panel)
        .rounded(px(6.))
        .border_1()
        .border_color(theme::get().floating_border)
        .shadow_lg()
        .p_2()
        .flex()
        .flex_col()
        .gap_2()
        .occlude()
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
        .on_mouse_down_out(cx.listener(|this, _, _, cx| {
            if let Some(ui) = &mut this.ui {
                ui.graph.pick = None;
            }
            cx.notify();
        }))
        .when(!p.about.is_empty(), |d| {
            d.child(
                div()
                    .text_size(px(11.))
                    .text_color(theme::get().muted)
                    .child(p.about.clone()),
            )
        })
        .children(idle.map(|why| {
            div()
                .text_size(px(11.))
                .text_color(theme::get().highlight)
                .child(why)
        }))
        .child(
            div()
                .flex()
                .items_center()
                .gap_2()
                .text_size(px(11.))
                .child(gpui_editor_kit::widgets::swatch(p.hsv.hex()))
                .child(format!("[{r}, {g}, {b}]"))
                .child(
                    div()
                        .text_color(theme::get().muted)
                        .child(format!("{r:02x}{g:02x}{b:02x}")),
                ),
        )
        .child(gpui_editor_kit::widgets::color_picker(
            "ui-graph-picker",
            p.hsv,
            |this: &mut Editor, hsv, _, cx| {
                if let Some(ui) = &mut this.ui {
                    ui.pick_colour(hsv);
                }
                cx.notify();
            },
            cx,
        ));
    deferred(
        anchored()
            .position(p.at)
            .snap_to_window_with_margin(px(8.))
            .child(body),
    )
    .with_priority(1)
    .into_any_element()
}

/// A colour swatch for fixed colour `rgb`, `size` pixels; with a span, a
/// press opens the picker on it.
pub(in crate::ui) fn color_swatch(
    id: SharedString,
    rgb3: [u8; 3],
    size: f32,
    span: Option<usize>,
    cx: &mut Context<Editor>,
) -> AnyElement {
    let [r, g, b] = rgb3;
    let swatch = div()
        .id(id)
        .flex_none()
        .size(px(size))
        .rounded(px(size / 5.))
        .border_1()
        .border_color(theme::get().floating_border)
        .bg(rgb(u32::from(r) << 16 | u32::from(g) << 8 | u32::from(b)));
    match span {
        Some(span) => swatch
            .cursor_pointer()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, e: &MouseDownEvent, _, cx| {
                    cx.stop_propagation();
                    let keys = this.ui.as_ref().map_or(Keys::Canvas, |u| u.keys);
                    this.press_keys_to(e.position, keys);
                    if let Some(ui) = &mut this.ui {
                        let mut pick =
                            ColorPick::new(span, e.position + point(px(-8.), px(10.)), rgb3);
                        (pick.about, pick.idle) = ui.pick_context(span);
                        ui.graph.pick = Some(pick);
                    }
                    cx.notify();
                }),
            )
            .into_any_element(),
        None => swatch.into_any_element(),
    }
}

/// The right-click menu: what removing the binding would change, then
/// Remove or Cancel. Kept inside the pane.
fn unbind_menu(m: &UnbindMenu, view: &GraphView, cx: &mut Context<Editor>) -> AnyElement {
    use gpui_editor_kit::widgets::{menu_header, menu_item};
    let (w, h) = (
        300.,
        70. + 18. * (m.sets.len().min(9) + m.reach.len().min(7)) as f32,
    );
    let pane = view.bounds.size;
    let x = m.at[0].min(f32::from(pane.width) - w - 4.).max(4.);
    let y = m.at[1].min(f32::from(pane.height) - h - 4.).max(4.);
    let bindings = m.bindings.clone();
    let title = match bindings.len() {
        1 => "Remove this binding?".to_owned(),
        n => format!("Remove these {n} bindings?"),
    };
    let mut list = div().flex().flex_col().px_2().pb_1();
    for set in m.sets.iter().take(8) {
        list = list.child(
            div()
                .text_size(px(10.5))
                .text_color(theme::get().muted)
                .truncate()
                .child(format!("stops setting {set}")),
        );
    }
    if m.sets.len() > 8 {
        list = list.child(
            div()
                .text_size(px(10.5))
                .text_color(theme::get().muted)
                .child(format!("and {} more", m.sets.len() - 8)),
        );
    }
    for p in m.reach.iter().take(6) {
        list = list.child(
            div()
                .text_size(px(10.5))
                .text_color(crate::colors::get().graph.reach)
                .truncate()
                .child(format!("and through it {p}")),
        );
    }
    if m.reach.len() > 6 {
        list = list.child(
            div()
                .text_size(px(10.5))
                .text_color(crate::colors::get().graph.reach)
                .child(format!("and {} more widget properties", m.reach.len() - 6)),
        );
    }
    div()
        .id("ui-graph-unbind")
        .absolute()
        .left(px(x))
        .top(px(y))
        .w(px(w))
        .bg(theme::get().popup)
        .rounded(px(6.))
        .border_1()
        .border_color(theme::get().floating_border)
        .shadow_lg()
        .p_1()
        .flex()
        .flex_col()
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
        .on_mouse_down_out(cx.listener(|this, _, _, cx| {
            if let Some(ui) = &mut this.ui {
                ui.graph.menu = None;
            }
            cx.notify();
        }))
        .child(menu_header(title))
        .child(list)
        .child(
            menu_item("Remove", "").on_click(cx.listener(move |this, _, _, cx| {
                if let Some(ui) = &mut this.ui {
                    ui.graph.menu = None;
                    let r = ui.unbind(&bindings);
                    this.ui_tree_result(r);
                }
                cx.notify();
            })),
        )
        .child(
            menu_item("Cancel", "Esc").on_click(cx.listener(|this, _, _, cx| {
                if let Some(ui) = &mut this.ui {
                    ui.graph.menu = None;
                }
                cx.notify();
            })),
        )
        .into_any_element()
}

/// One node: a coloured header, its input pins, then its output pins,
/// each with its last value.
#[allow(clippy::too_many_arguments)]
fn node_element(
    i: usize,
    n: &Node,
    graph: &Graph,
    view: &GraphView,
    // During a wire drag: whether each input pin takes it.
    lit: &[Option<bool>],
    file: (Option<anno_formats::phx::Guid>, u64),
    editable: bool,
    // The span being typed into, and what it shows (caret included).
    editing: Option<&(usize, String)>,
    cx: &mut Context<Editor>,
) -> AnyElement {
    let z = view.zoom;
    let [x, y] = view.to_screen([n.rect[0], n.rect[1]]);
    let (w, h) = (n.rect[2] * z, n.rect[3] * z);
    // Colour by kind: where values come from, what they set, and the logic.
    let gc = &crate::colors::get().graph;
    let (head, border) = match n.kind {
        Kind::Widget { focus: true, .. } => (gc.widget[0], theme::get().accent),
        Kind::Widget { .. } => (gc.widget[1], gc.widget[2]),
        Kind::Inputs | Kind::Value => (gc.inputs[0], gc.inputs[1]),
        Kind::Data => (gc.data[0], gc.data[1]),
        Kind::Clip => (gc.clip[0], gc.clip[1]),
        Kind::Logic { ran: true, .. } => (gc.logic[0], gc.logic[1]),
        Kind::Logic { ran: false, .. } => (gc.logic_idle[0], gc.logic_idle[1]),
    };
    // Each pin's full name and value on hover: values cut at the node's
    // width (an image path) read in full there. Not while a menu, a wire or
    // a node is being handled (round 4: "a = 24" sat over "Remove this
    // binding?"). Ids follow the node's key, not its index, so a graph
    // rebuilt by an edit does not inherit another pin's tooltip.
    let tips = view.menu.is_none()
        && view.pick.is_none()
        && view.wiring.is_none()
        && view.moving.is_none();
    // `tipped`: no tooltip over a pin's own field being typed in.
    let row = |id: String,
               name: String,
               value: Option<String>,
               about: Option<String>,
               out: bool,
               dim: bool,
               tipped: bool| {
        let tip = match &value {
            Some(v) => format!("{name} = {v}"),
            None => name.clone(),
        };
        div()
            .id(SharedString::from(id))
            .when(tips && tipped, |d| {
                d.tooltip(Tooltip::text(tip, "", about.unwrap_or_default()))
            })
            .relative()
            .h(px(LINE_H * z))
            .px(px(8. * z))
            .flex()
            .items_center()
            .gap(px(4. * z))
            .when(out, |d| d.flex_row_reverse())
            .text_size(px(10.5 * z))
            .child(
                div()
                    .flex_none()
                    .text_color(if dim {
                        theme::get().muted
                    } else {
                        theme::get().text
                    })
                    .child(name),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .when(!out, |d| d.text_right())
                    .text_color(gc.value)
                    .child(value.unwrap_or_default()),
            )
    };
    // A value's pin is round; an event's is a square in the event wires'
    // colour, so the two read apart before any wire is drawn.
    let dot = |filled: bool, event: bool| {
        let c = if event { gc.event } else { gc.value };
        div()
            .absolute()
            .top(px((LINE_H / 2. - 3.5) * z))
            .size(px(7. * z))
            .rounded(px(if event { 1. } else { 4. } * z))
            .border_1()
            .border_color(c)
            .when(filled, |d| d.bg(c))
    };
    let key = n.key.clone();
    let origin = [n.rect[0], n.rect[1]];
    let widget = n.kind.widget();
    let pressed = n.key.clone();
    let mut body = div().flex().flex_col().child(
        div()
            .id(("ui-graph-head", i))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, e: &MouseDownEvent, _, cx| {
                    cx.stop_propagation();
                    this.press_keys_to(e.position, Keys::Graph);
                    if let Some(ui) = &mut this.ui {
                        ui.graph.drag = None;
                        ui.graph.dragged = false;
                        ui.graph.moving = Some(Moving {
                            file: file.0,
                            key: key.clone(),
                            start: e.position,
                            origin,
                        });
                    }
                }),
            )
            // A click on a widget's title selects it, as one on its body
            // does (the press starts a move, so the body's click never
            // came); a drag only moves the node.
            .when_some(widget, |d, widget| {
                d.on_mouse_up(
                    MouseButton::Left,
                    cx.listener(move |this, _: &MouseUpEvent, _, cx| {
                        let Some(ui) = &mut this.ui else { return };
                        let here = ui.graph.moving.as_ref().is_some_and(|m| m.key == pressed);
                        if here && !ui.graph.dragged {
                            ui.select_in_file(file, widget);
                            cx.notify();
                        }
                    }),
                )
            })
            .cursor_move()
            .h(px(TITLE_H * z))
            .px(px(8. * z))
            .flex()
            .items_center()
            .gap(px(6. * z))
            .bg(head)
            .child(
                div()
                    .flex_none()
                    .text_size(px(11.5 * z))
                    .text_color(theme::get().text)
                    .child(n.title.clone()),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_size(px(9.5 * z))
                    .text_color(gc.sub)
                    .child(n.sub.clone()),
            ),
    );
    for (k, p) in n.ins.iter().enumerate() {
        // A fixed value of the file being edited is its own field.
        let span = p.constant.filter(|_| editable);
        // A colour shows as one, read only too.
        let read_only_colour = p.rgb.filter(|_| span.is_none());
        // An event holds no value to show, whatever the last run left.
        let value = p
            .value
            .as_ref()
            .filter(|_| span.is_none() && read_only_colour.is_none() && !p.event)
            .map(|v| {
                if p.literal {
                    format!("= {v}")
                } else {
                    v.clone()
                }
            });
        let takes = lit.get(k).copied().flatten();
        let typed = span.is_some() && editing.is_some_and(|e| Some(e.0) == span);
        let mut r = row(
            format!("ui-graph-in {} {k}", n.key),
            p.name.clone(),
            value,
            p.about.clone(),
            false,
            p.open.is_some() || p.make.is_some(),
            !typed,
        )
        .when(takes == Some(false), |r| r.opacity(0.35))
        .child(match takes {
            Some(true) => dot(true, p.event)
                .size(px(9. * z))
                .border_color(gc.drop_ok)
                .bg(gc.drop_ok)
                .left(px(-5. * z)),
            _ => dot(!p.literal && p.open.is_none() && p.make.is_none(), p.event).left(px(-4. * z)),
        });
        if let (Some(c), Some(v)) = (read_only_colour, &p.value) {
            r = r.child(
                div()
                    .flex_none()
                    .flex()
                    .items_center()
                    .gap(px(4. * z))
                    .text_color(gc.value)
                    .child(color_swatch(
                        format!("ui-graph-swatch {} {k}", n.key).into(),
                        c,
                        10. * z,
                        None,
                        cx,
                    ))
                    .child(format!("= {v}")),
            );
        }
        if let (Some(span), Some(v)) = (span, &p.value) {
            let id = SharedString::from(format!("ui-graph-const {} {k}", n.key));
            let cell = div()
                .flex_none()
                // Room for a colour's swatch beside "= [255, 255, 255]".
                .max_w(px(n.rect[2] * z * 0.72))
                .h(px((LINE_H - 3.) * z))
                .px(px(3. * z))
                .rounded(px(2. * z))
                .flex()
                .items_center()
                .overflow_hidden()
                .cursor_text();
            r = r.child(match editing.filter(|e| e.0 == span) {
                // Room for the caret in an empty or short value, and the
                // text fills the box: a press anywhere in it places the
                // caret (`press_keys_to` lets it be).
                Some((_, shown)) => cell
                    .min_w(px(40. * z))
                    .bg(theme::get().input)
                    .border_1()
                    .border_color(theme::get().accent)
                    .text_color(theme::get().text)
                    .child(gpui_editor_kit::field::edit_text(id, shown.clone(), true).w_full())
                    .into_any_element(),
                // A click types into it, as the List's fields do; Enter
                // applies (one undo step), Esc keeps what was there.
                None => cell
                    .id(id)
                    .gap(px(4. * z))
                    .text_color(gc.value)
                    .hover(|d| d.bg(theme::get().hover).text_color(theme::get().text))
                    // A colour shows as one; its swatch opens a picker, the
                    // text still types.
                    .when_some(p.rgb, |d, c| {
                        d.child(color_swatch(
                            format!("ui-graph-swatch {} {k}", n.key).into(),
                            c,
                            10. * z,
                            Some(span),
                            cx,
                        ))
                    })
                    .child(format!("= {v}"))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _, window, cx| {
                            cx.stop_propagation();
                            this.start_span_edit(
                                span,
                                Keys::Graph,
                                gpui_editor_kit::field::Filter::Text,
                                window,
                                cx,
                            );
                        }),
                    )
                    .into_any_element(),
            });
        }
        // Right-click unbinds the copy that fills it; on a gate pair's then
        // or else, the one binding of the two that row is (round 14: only
        // both went, and keeping one meant rebuilding it in the binder).
        let half = match n.kind {
            Kind::Logic { also: Some(_), .. } => p.arg.map(|(b, _)| b),
            _ => None,
        };
        if let (true, Some(b)) = (editable, p.fed_by.or(half)) {
            r = r.on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, e: &MouseDownEvent, _, cx| {
                    cx.stop_propagation();
                    this.press_keys_to(e.position, Keys::Graph);
                    if let Some(ui) = &mut this.ui {
                        ui.ask_unbind(e.position, vec![b]);
                    }
                    cx.notify();
                }),
            );
        }
        body = body.child(r);
    }
    for (k, p) in n.outs.iter().enumerate() {
        let pin = view.to_screen(graph.out_at(i, k));
        let kind = p.kind;
        let r = row(
            format!("ui-graph-out {} {k}", n.key),
            p.name.clone(),
            p.value.clone().filter(|_| !p.event),
            p.about.clone(),
            true,
            false,
            true,
        );
        let dot = dot(true, p.event).right(px(-4. * z));
        // The whole row starts a wire (the dot alone was a 7 px target), and
        // so does the dot, half of which sits outside the row: a press there
        // missed the row and panned the graph (round 9).
        // A place nothing reads yet starts a wire too (a new button's
        // OnPress): the drop makes it.
        let source = p
            .slot
            .map(Source::Slot)
            .or(p.make.map(Source::New))
            .filter(|_| editable);
        let r = match source {
            Some(source) => {
                let start =
                    move |this: &mut Editor, e: &MouseDownEvent, cx: &mut Context<Editor>| {
                        cx.stop_propagation();
                        this.press_keys_to(e.position, Keys::Graph);
                        if let Some(ui) = &mut this.ui {
                            let at = e.position - ui.graph.bounds.origin;
                            ui.graph.drag = None;
                            let drops: Vec<Drop> = ui.graph.drops.iter().map(|d| d.1).collect();
                            let verdicts = ui.wire_verdicts(source, kind, &drops);
                            ui.graph.wiring = Some(Wiring {
                                source,
                                kind,
                                from: pin,
                                at: [f32::from(at.x), f32::from(at.y)],
                                start: [f32::from(at.x), f32::from(at.y)],
                                verdicts,
                            });
                        }
                    };
                r.cursor_pointer()
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, e: &MouseDownEvent, _, cx| start(this, e, cx)),
                    )
                    .child(
                        dot.id(SharedString::from(format!(
                            "ui-graph-out-dot {} {k}",
                            n.key
                        )))
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this, e: &MouseDownEvent, _, cx| start(this, e, cx)),
                        ),
                    )
            }
            None => r.child(dot),
        };
        body = body.child(r);
    }
    if let Some(why) = &n.note {
        body = body.child(
            div()
                .h(px(2. * LINE_H * z))
                .px(px(8. * z))
                .overflow_hidden()
                .text_size(px(9.5 * z))
                .line_height(px(LINE_H * z))
                .text_color(gc.why)
                .child(why.clone()),
        );
    }
    // Only what the node does not already show: why it did not run.
    let tip = n.note.as_ref().map(|why| format!("{} · {why}", n.title));
    let mut boxed = div()
        .id(("ui-graph-node", i))
        .on_hover(cx.listener(move |this, hovered: &bool, _, cx| {
            if let Some(ui) = &mut this.ui {
                let g = &mut ui.graph;
                if *hovered {
                    g.hover = Some(i);
                } else if g.hover == Some(i) {
                    g.hover = None;
                }
            }
            this.notify_pane(crate::editor::Pane::UiBindings, cx);
        }))
        .absolute()
        .left(px(x))
        .top(px(y))
        .w(px(w))
        .h(px(h))
        .rounded(px(4. * z))
        .border_1()
        .border_color(border)
        .bg(gc.node)
        .when_some(tip.filter(|_| tips), |d, tip| {
            d.tooltip(Tooltip::text(tip, "", ""))
        })
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(|this, e: &MouseDownEvent, _, cx| {
                cx.stop_propagation();
                this.press_keys_to(e.position, Keys::Graph);
            }),
        )
        .child(body);
    match n.kind {
        Kind::Widget { widget, .. } => {
            boxed = boxed
                .cursor_pointer()
                .on_click(cx.listener(move |this, _, _, cx| {
                    if let Some(ui) = &mut this.ui {
                        // The end of a drag is not a click.
                        if std::mem::take(&mut ui.graph.dragged) {
                            return;
                        }
                        ui.select_in_file(file, widget);
                    }
                    cx.notify();
                }));
        }
        // Right-click asks to remove the binding the node is (both of a
        // gate pair).
        Kind::Logic { binding, also, .. } if editable => {
            boxed = boxed.on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, e: &MouseDownEvent, _, cx| {
                    this.press_keys_to(e.position, Keys::Graph);
                    if let Some(ui) = &mut this.ui {
                        ui.ask_unbind(
                            e.position,
                            [Some(binding), also].into_iter().flatten().collect(),
                        );
                    }
                    cx.notify();
                }),
            );
        }
        _ => {}
    }
    boxed.into_any_element()
}
