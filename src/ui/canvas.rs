// Owns the UI canvas: widgets laid out in design pixels, drawn as positioned
// GPUI elements (labels, CPU-composited images) and runs of GPU sprites
// (`gpu`), and pointer editing on it: picking, click versus drag, hover
// outlines, snapping. Pan, zoom and move/resize handles come from the
// kit's `design` surface. Layout input is the flat `Node` list built from
// the decoded widget tree; edits come back out as rectangle changes.
use super::UiEditor;
use gpui_editor_kit::{
    design::{Design, Guide, Handle},
    gpui::{prelude::*, *},
    theme,
};
use std::sync::Arc;

/// Screen pixels the pointer travels before a press becomes a drag: a
/// click with a shaky hand must not move the widget.
const DRAG_START: f32 = 4.;
/// Screen pixels within which a dragged edge snaps to a neighbour's.
const SNAP_REACH: f32 = 6.;

/// What a pointer move changed. Hover, a drag in progress and pan touch
/// only the canvas pane, and redrawing every pane for them cost ~40 ms a
/// move on the construction menu.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum Redraw {
    Nothing,
    Canvas,
    /// The selection or the status message changed too.
    All,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Container,
    Image,
    Label,
}

/// A nine-slice image: pieces left-to-right, top-to-bottom, and the border
/// widths in design pixels (left, top, right, bottom).
#[derive(Clone)]
pub struct Slices {
    pub pieces: Arc<[Arc<RenderImage>; 9]>,
    pub margins: [f32; 4],
}

/// A label's drop shadow: colour, alpha, and offset in design pixels.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Shadow {
    pub color: [u8; 3],
    pub alpha: f32,
    pub offset: [f32; 2],
}

/// One widget, positioned absolutely in design pixels.
#[derive(Clone)]
pub struct Node {
    /// Index into the widget list of the file it comes from.
    pub widget: usize,
    /// Comes from a container's template file; shown, not editable here.
    pub foreign: bool,
    /// 0 for a widget of its own; k for the k-th sample item its list
    /// stamped from the prototype, which shares `widget` and everything
    /// else (round 16: the third objective's Icon and the first's were one).
    pub item: u32,
    pub depth: usize,
    pub name: Arc<str>,
    pub kind: Kind,
    /// x, y, width, height in design pixels (absolute).
    pub rect: [f32; 4],
    pub image: Option<Arc<RenderImage>>,
    pub slices: Option<Slices>,
    /// Shared: a relayout hands most nodes over unchanged.
    pub text: Option<SharedString>,
    /// The game font's family, registered with GPUI by the canvas.
    pub font: Option<SharedString>,
    /// Font size in design pixels (TextStyle Size × scale).
    pub text_size: f32,
    /// Distance between a label's wrapped lines in design pixels.
    pub line_height: f32,
    /// Label AlignmentX (0 left, 1 centre, 2 right, 3 justify) and
    /// AlignmentY (0 top, 1 centre, 2 baseline, 3 bottom).
    pub align: [u32; 2],
    /// Label HorizontalOverflow: 2/3 clip, 4 ellipsis, others let the text
    /// run past the box.
    pub overflow: u32,
    pub color: Option<[u8; 3]>,
    /// The TextStyle's drop shadow, for labels.
    pub shadow: Option<Shadow>,
    pub opacity: f32,
    pub visible: bool,
    /// Only this region draws (design pixels): the box of the mask chain
    /// the widget draws through.
    pub clip: Option<[f32; 4]>,
    /// `image` is already composited for exactly `clip` (masks, UV
    /// transforms) and fills it.
    pub baked: bool,
    /// Placed by game code, not by its stored Position (radial menu items).
    pub runtime: bool,
    /// Drawn on the GPU instead of as `image` (see `ui::gpu`).
    pub sprite: Option<Arc<super::gpu::Sprite>>,
    /// The template instances it sits in, innermost first: (template guid,
    /// instance key of the container that expands it,
    /// `layout::Tree::instance_keys`). Empty for the open file's own
    /// widgets.
    pub owners: Arc<[(anno_formats::phx::Guid, u64)]>,
    /// Its index in the layout tree, and the tree index of the widget that
    /// provides the nearest masks of its `clip` (`usize::MAX`: none): a
    /// drag carries masks that move with the dragged widget and leaves the
    /// rest.
    pub tree: usize,
    pub clip_by: usize,
}

/// A finished move or resize, in the widget's own (absolute) rectangle.
pub struct Change {
    pub widget: usize,
    pub before: [f32; 4],
    pub after: [f32; 4],
}

/// A left press on the canvas that has not been released yet.
#[derive(Clone, Copy, Debug)]
pub struct Press {
    /// Window position of the press.
    pub start: Point<Pixels>,
    /// The widget a drag moves or resizes; None on empty page.
    pub node: Option<usize>,
    pub handle: Handle,
    /// Pressed inside the selection, which stays selected so it can be
    /// dragged; a click without a drag then picks inside it.
    pub inside: bool,
    /// Past [`DRAG_START`]: the design surface is editing.
    pub dragging: bool,
    /// The widget's parent stacks its children: the drag picks a new
    /// place in the stack instead of a Position.
    pub reorder: bool,
}

/// Where a widget dropped into its parent's stack goes: the parent widget,
/// the index among its other children, and the insertion line (design
/// pixels) to show.
struct Slot {
    parent: usize,
    index: usize,
    line: Guide,
}

/// The canvas's right-click menu: where it opens (pane pixels) and the
/// widgets under the pointer, topmost first.
#[derive(Clone, Debug)]
pub struct Menu {
    pub at: Point<Pixels>,
    pub stack: Vec<usize>,
}

pub struct Canvas {
    /// Page pan/zoom and move/resize drags, by node index. The page is the
    /// design surface, usually the screen the asset was authored for.
    pub view: Design<usize>,
    pub selected: Option<usize>,
    pub show_frames: bool,
    /// Show widgets stored transparent that animations fade in.
    pub design_view: bool,
    /// Outline widgets that do not draw (hidden, transparent, empty), so
    /// they can be found and picked.
    pub show_hidden: bool,
    pub press: Option<Press>,
    /// What a click at the pointer would select.
    pub hover: Option<usize>,
    /// Widgets outlined for something the inspector points at (what a Game
    /// data flag changes), without selecting them: the inspector stays on
    /// what pointed. A click on the canvas clears it.
    pub marked: Vec<usize>,
    /// Modifiers last seen with the pointer: Shift locks a move to one
    /// axis, Alt turns snapping off.
    pub modifiers: Modifiers,
    /// Where a right press began, to tell a click (menu) from a pan.
    pub right_press: Option<Point<Pixels>>,
    pub menu: Option<Menu>,
}

impl Default for Canvas {
    fn default() -> Self {
        Self {
            // The game-view chips sit over the canvas's top 40 px.
            view: {
                let mut view = Design::default();
                view.top_inset = 40.;
                // The key hints at the foot of the canvas.
                view.bottom_inset = 34.;
                view
            },
            selected: None,
            show_frames: true,
            // What the player sees; design view also shows faded-in groups.
            design_view: false,
            show_hidden: false,
            press: None,
            hover: None,
            marked: Vec::new(),
            modifiers: Modifiers::default(),
            right_press: None,
            menu: None,
        }
    }
}

fn contains([x, y, w, h]: [f32; 4], p: [f32; 2]) -> bool {
    p[0] >= x && p[1] >= y && p[0] < x + w && p[1] < y + h
}

fn overlaps(a: [f32; 4], b: [f32; 4]) -> bool {
    a[0] < b[0] + b[2] && b[0] < a[0] + a[2] && a[1] < b[1] + b[3] && b[1] < a[1] + a[3]
}

/// The canvas's paint order: GPUI elements and runs of sprites. Each run
/// is a render pass of its own between GPUI's (see `gpu::Batch`), so a
/// sprite joins the open run unless something GPUI drew since that run
/// began (a label) overlaps it; then it must draw after that, in a new run.
/// Splitting at every label made a few hundred passes a frame.
#[derive(Default)]
struct Runs {
    paint: Vec<Result<AnyElement, Vec<Arc<super::gpu::Sprite>>>>,
    open: Option<usize>,
    /// Design boxes of what GPUI draws after the open run.
    since: Vec<[f32; 4]>,
}

impl Runs {
    fn sprite(&mut self, sprite: Arc<super::gpu::Sprite>, covers: [f32; 4]) {
        let clear = !self.since.iter().any(|&q| overlaps(q, covers));
        if let Some(Err(run)) = self
            .open
            .filter(|_| clear)
            .and_then(|b| self.paint.get_mut(b))
        {
            run.push(sprite);
            return;
        }
        self.paint.push(Err(vec![sprite]));
        self.open = Some(self.paint.len() - 1);
        self.since.clear();
    }

    fn element(&mut self, el: AnyElement, covers: [f32; 4]) {
        if self.open.is_some() {
            self.since.push(covers);
        }
        self.paint.push(Ok(el));
    }
}

/// The part of `a` inside `b`; empty (no overlaps) when they miss.
fn intersect(a: [f32; 4], b: [f32; 4]) -> [f32; 4] {
    let (x0, y0) = (a[0].max(b[0]), a[1].max(b[1]));
    let (x1, y1) = (
        (a[0] + a[2]).min(b[0] + b[2]),
        (a[1] + a[3]).min(b[1] + b[3]),
    );
    [x0, y0, (x1 - x0).max(0.), (y1 - y0).max(0.)]
}

impl UiEditor {
    /// The parent of every node (pre-order depths).
    pub(super) fn parent_of(&self, node: usize) -> Option<usize> {
        self.parents.get(node).copied().flatten()
    }

    /// The widget a drag on `node` moves: itself when it is this file's,
    /// else the container instance in this file that shows it. The root
    /// (the page) never moves.
    pub(super) fn movable(&self, node: usize) -> Option<usize> {
        let mut at = node;
        while self.nodes.get(at)?.foreign {
            at = self.parent_of(at)?;
        }
        (at > 0).then_some(at)
    }

    /// Widgets under design point `p`, topmost first: what draws there
    /// (`hits`), else the smallest box around it, so a frame's empty middle
    /// or a transparent backdrop still picks something. Show hidden adds
    /// the widgets that do not draw.
    fn under(&self, p: [f32; 2]) -> Vec<usize> {
        let mut out: Vec<usize> = self.hits(p).map(|(i, _)| i).collect();
        let [pw, ph] = self.canvas.view.size;
        let mut boxes: Vec<usize> = (1..self.nodes.len())
            .filter(|&i| {
                let n = &self.nodes[i];
                (self.canvas.show_hidden || (n.visible && n.opacity >= 0.01))
                    && contains(n.rect, p)
                    && n.clip.is_none_or(|c| contains(c, p))
                    // A page-sized backdrop is the screen, not a pick.
                    && n.rect[2] * n.rect[3] < 0.9 * pw * ph
                    && !out.contains(&i)
            })
            .collect();
        boxes.sort_by(|&a, &b| {
            let area = |i: usize| self.nodes[i].rect[2] * self.nodes[i].rect[3];
            area(a).total_cmp(&area(b)).then(b.cmp(&a))
        });
        out.extend(boxes);
        out
    }

    /// Every widget under `p` and its ancestors below the page, topmost
    /// and innermost first: the stack Alt-click cycles and the right-click
    /// menu lists.
    pub(super) fn stack_at(&self, p: [f32; 2]) -> Vec<usize> {
        let mut out = Vec::new();
        for n in self.under(p) {
            let mut at = Some(n).filter(|&n| n > 0);
            while let Some(e) = at {
                if !out.contains(&e) {
                    out.push(e);
                }
                at = self.parent_of(e).filter(|&p| p > 0);
            }
        }
        out
    }

    pub(super) fn design_point(&self, at: Point<Pixels>) -> [f32; 2] {
        self.canvas.view.to_design(at - self.canvas_bounds.origin)
    }

    /// What a plain click at `p` selects: inside the selection, the
    /// selection (it stays, to be dragged; the release picks); else the
    /// deepest widget drawn there, template widgets included.
    fn click_target(&self, p: [f32; 2]) -> (Option<usize>, bool) {
        if let Some(s) = self.canvas.selected.filter(|&s| s > 0)
            && self.nodes.get(s).is_some_and(|n| contains(n.rect, p))
        {
            return (Some(s), true);
        }
        (self.under(p).first().copied(), false)
    }

    /// A left press at `at` (window pixels): selects the deepest widget
    /// drawn there. Ctrl picks what a drag there moves instead (this file's
    /// widget or the container instance holding a template's); Alt cycles
    /// outward through everything stacked there. `handle` is a resize
    /// handle of the selection.
    pub(super) fn press(
        &mut self,
        at: Point<Pixels>,
        m: Modifiers,
        clicks: usize,
        handle: Option<(usize, Handle)>,
    ) {
        self.canvas.menu = None;
        self.canvas.marked.clear();
        self.take_keys(super::Keys::Canvas);
        self.cursor = at;
        self.canvas.modifiers = m;
        if let Some((node, handle)) = handle {
            self.canvas.press = Some(Press {
                start: at,
                node: Some(node),
                handle,
                inside: true,
                dragging: false,
                reorder: false,
            });
            return;
        }
        let p = self.design_point(at);
        // A double click on a template's widget edits that template here.
        if clicks >= 2
            && let Some(&deep) = self.under(p).first()
            && self.nodes[deep].foreign
            && let Some(&(g, key)) = self.nodes[deep].owners.first()
        {
            let widget = self.nodes[deep].widget;
            match self.enter_template(g, key) {
                Ok(m) => {
                    self.message = Some((m, false));
                    if self.nodes.get(deep).is_some_and(|n| n.widget == widget) {
                        self.select_widget(deep);
                    }
                }
                Err(e) => self.message = Some((format!("{e:#}"), true)),
            }
            return;
        }
        let (target, inside) = if m.control || m.platform {
            (self.under(p).first().and_then(|&n| self.movable(n)), false)
        } else if m.alt {
            let stack = self.stack_at(p);
            let next = match self
                .canvas
                .selected
                .and_then(|s| stack.iter().position(|&n| n == s))
            {
                Some(i) => stack.get(i + 1).or(stack.first()),
                None => stack.first(),
            };
            (next.copied(), false)
        } else {
            self.click_target(p)
        };
        match target {
            Some(n) if Some(n) != self.canvas.selected => self.select_widget(n),
            Some(_) => {}
            None => {
                self.canvas.selected = None;
                self.selected = None;
            }
        }
        self.canvas.press = Some(Press {
            start: at,
            node: target,
            handle: Handle::Move,
            inside,
            dragging: false,
            reorder: false,
        });
    }

    /// The pointer moved to `at`; `held` with the left button down.
    pub(super) fn pointer_move(&mut self, at: Point<Pixels>, m: Modifiers, held: bool) -> Redraw {
        self.cursor = at;
        let mods_changed = self.canvas.modifiers != m;
        self.canvas.modifiers = m;
        if let Some(press) = &mut self.canvas.press
            && held
        {
            if press.dragging {
                return Redraw::Canvas;
            }
            let d = at - press.start;
            if f32::from(d.x).hypot(f32::from(d.y)) < DRAG_START {
                return Redraw::Nothing;
            }
            let press = *press;
            // A template's widget moves with the container instance that
            // shows it, which this file places.
            let Some(node) = press.node.and_then(|n| self.movable(n)) else {
                if press.node.is_some() {
                    self.message = Some(("Nothing in this file places that widget".into(), true));
                }
                self.canvas.press = None;
                return if press.node.is_some() {
                    Redraw::All
                } else {
                    Redraw::Nothing
                };
            };
            let mut redraw = Redraw::Canvas;
            if Some(node) != press.node {
                self.select_widget(node);
                redraw = Redraw::All;
            }
            if let Some(why) = self.fixed_place(node, press.handle) {
                self.message = Some((why, true));
                self.canvas.press = None;
                return Redraw::All;
            }
            let reorder =
                press.handle == Handle::Move && self.frames[node].placed_by == Some("StackLayout");
            let rect = self.nodes[node].rect;
            self.canvas
                .view
                .begin_edit(node, press.handle, press.start, rect);
            if let Some(p) = &mut self.canvas.press {
                p.node = Some(node);
                p.dragging = true;
                p.reorder = reorder;
            }
            self.canvas.hover = None;
            return redraw;
        }
        if held || self.canvas.view.dragging() {
            return if mods_changed {
                Redraw::Canvas
            } else {
                Redraw::Nothing
            };
        }
        let hover = self.click_target(self.design_point(at)).0;
        let hover = hover.filter(|&h| Some(h) != self.canvas.selected);
        if hover != self.canvas.hover {
            self.canvas.hover = hover;
            return Redraw::Canvas;
        }
        Redraw::Nothing
    }

    /// The left button came up at `at`. A drag becomes an edit; a click
    /// inside the selection selects the deepest widget drawn there.
    pub(super) fn release(&mut self, at: Point<Pixels>) {
        let Some(press) = self.canvas.press.take() else {
            return;
        };
        if press.dragging && press.reorder {
            self.canvas.view.cancel();
            if let Some(node) = press.node {
                let p = self.design_point(at);
                let r = self.drop_in_stack(node, p);
                self.message = Some(match r {
                    Ok(m) => (m, false),
                    Err(e) => (format!("Not moved: {e:#}"), true),
                });
            }
            return;
        }
        if press.dragging {
            let preview = self.drag_preview();
            self.canvas.view.cancel();
            if let Some((node, after, _)) = preview
                && after != self.nodes[node].rect
            {
                let change = Change {
                    widget: self.nodes[node].widget,
                    before: self.nodes[node].rect,
                    after,
                };
                self.canvas_change(change);
            }
            return;
        }
        if !press.inside || press.handle != Handle::Move {
            return;
        }
        let Some(selected) = press.node else {
            return;
        };
        let p = self.design_point(at);
        if let Some(&deep) = self.under(p).first()
            && deep != selected
        {
            self.select_widget(deep);
        }
    }

    /// Escape: drops a drag in progress or the menu, else selects the
    /// selection's parent, else clears the selection. False when there was
    /// nothing to do.
    pub(super) fn escape(&mut self) -> bool {
        if self.canvas.press.take().is_some_and(|p| p.dragging) {
            self.canvas.view.cancel();
            return true;
        }
        if self.canvas.menu.take().is_some() {
            return true;
        }
        if self.canvas.selected.is_none() {
            return self.step_out();
        }
        // Inside a template edited in place, the walk up stops at its top:
        // the selection clears, and the next Esc steps out.
        let parent = self
            .canvas
            .selected
            .and_then(|s| self.parent_of(s))
            .filter(|&p| p > 0)
            .filter(|&p| self.scoped(p));
        match parent {
            Some(p) => self.select_widget(p),
            None => {
                self.canvas.selected = None;
                self.selected = None;
            }
        }
        true
    }

    /// Leaves the template edited in place for the one around it, or for
    /// the screen. False when editing the screen already.
    pub(super) fn step_out(&mut self) -> bool {
        if self.scope.is_none() {
            return false;
        }
        // The enclosing instance: second in any own widget's chain.
        let outer = self
            .nodes
            .iter()
            .find(|n| !n.foreign)
            .and_then(|n| n.owners.get(1).copied());
        match outer {
            Some((g, key)) => {
                if let Err(e) = self.enter_template(g, key) {
                    self.message = Some((format!("{e:#}"), true));
                }
            }
            None => self.leave_template(),
        }
        true
    }

    /// The breadcrumb of the template edited in place: the screen, then
    /// each template instance from the outside in (file, instance key).
    pub(super) fn scope_chain(&self) -> Vec<(anno_formats::phx::Guid, u64)> {
        if self.scope.is_none() {
            return Vec::new();
        }
        let mut chain: Vec<_> = self
            .nodes
            .iter()
            .find(|n| !n.foreign)
            .map(|n| n.owners.to_vec())
            .unwrap_or_default();
        chain.reverse();
        chain
    }

    /// The rectangle the drag in progress shows, snapped to the parent,
    /// the siblings and the page unless Alt is held, on one axis with
    /// Shift, and the guides it snapped to.
    pub(super) fn drag_preview(&self) -> Option<(usize, [f32; 4], Vec<Guide>)> {
        let (node, handle, start) = self.canvas.view.editing()?;
        let mut rect = self.canvas.view.preview(node, self.cursor)?;
        if self.canvas.press.is_some_and(|p| p.reorder) {
            let p = self.design_point(self.cursor);
            let line = self.stack_slot(node, p).map(|s| s.line);
            return Some((node, rect, line.into_iter().collect()));
        }
        let m = self.canvas.modifiers;
        if m.shift && handle == Handle::Move {
            let (dx, dy) = (rect[0] - start[0], rect[1] - start[1]);
            if dx.abs() > dy.abs() {
                rect[1] = start[1];
            } else {
                rect[0] = start[0];
            }
        }
        if m.alt {
            return Some((node, rect, Vec::new()));
        }
        let parent = self.parent_of(node);
        let [pw, ph] = self.canvas.view.size;
        let view = self.visible_region();
        let mut targets = vec![[0., 0., pw, ph]];
        if let Some(p) = parent {
            targets.push(self.nodes[p].rect);
            // Siblings in view: hundreds of list items off screen only
            // cost time.
            targets.extend(
                (p + 1..self.nodes.len())
                    .take_while(|&i| self.nodes[i].depth > self.nodes[p].depth)
                    .filter(|&i| {
                        i != node
                            && self.parent_of(i) == Some(p)
                            && self.nodes[i].visible
                            && overlaps(self.nodes[i].rect, view)
                    })
                    .map(|i| self.nodes[i].rect),
            );
        }
        let reach = SNAP_REACH / self.canvas.view.zoom;
        let (mut rect, mut guides) = gpui_editor_kit::design::snap(rect, handle, &targets, reach);
        // Whole design pixels for what moved; a move keeps the size exact.
        if handle == Handle::Move {
            rect[0] = rect[0].round();
            rect[1] = rect[1].round();
        }
        if m.shift && handle == Handle::Move {
            // Snapping must not undo the axis lock.
            if (rect[0] - start[0]).abs() > (rect[1] - start[1]).abs() {
                rect[1] = start[1];
                guides.retain(|g| g.vertical);
            } else {
                rect[0] = start[0];
                guides.retain(|g| !g.vertical);
            }
        }
        Some((node, rect, guides))
    }

    /// Why `node` cannot be dragged with `handle`, if it cannot: template
    /// widgets, and Positions a binding or the parent's BackgroundLayout
    /// set (a stored one would not show).
    fn fixed_place(&self, node: usize, handle: Handle) -> Option<String> {
        let n = &self.nodes[node];
        let f = &self.frames[node];
        if n.foreign {
            return Some(
                "That widget comes from a container’s template file; open the template to edit it"
                    .into(),
            );
        }
        let resize = handle != Handle::Move;
        // Recording keys what a clip animates: its own tracks are "bound".
        let recording = self.player.recording;
        if !resize && f.bound.position.is_some() && !recording {
            return Some(format!(
                "{}’s Position comes from a data binding (see Bindings); a moved value would not show",
                n.name
            ));
        }
        if resize && (f.bound.size.is_some() || f.bound.scale.is_some()) && !recording {
            return Some(format!(
                "{}’s size comes from a data binding (see Bindings); a resized value would not show",
                n.name
            ));
        }
        if f.placed_by == Some("BackgroundLayout") {
            return Some(format!(
                "{} is the parent’s BackgroundLayout backdrop: it follows the other children",
                n.name
            ));
        }
        None
    }

    /// Where dropping `node` at design point `p` puts it in its parent's
    /// stack. The axis and the direction come from where the siblings are
    /// drawn (a StackLayout runs over its children last to first unless
    /// inverted).
    fn stack_slot(&self, node: usize, p: [f32; 2]) -> Option<Slot> {
        let parent = self.parent_of(node)?;
        if self.nodes[parent].foreign {
            return None;
        }
        let kids: Vec<usize> = (parent + 1..self.nodes.len())
            .take_while(|&i| self.nodes[i].depth > self.nodes[parent].depth)
            .filter(|&i| self.parent_of(i) == Some(parent))
            .collect();
        let centre = |i: usize, a: usize| self.nodes[i].rect[a] + self.nodes[i].rect[a + 2] / 2.;
        let spread = |a: usize| {
            let (lo, hi) = kids.iter().fold((f32::MAX, f32::MIN), |(lo, hi), &k| {
                (lo.min(centre(k, a)), hi.max(centre(k, a)))
            });
            hi - lo
        };
        let axis = usize::from(spread(1) > spread(0));
        let others: Vec<usize> = kids.iter().copied().filter(|&k| k != node).collect();
        let mut drawn: Vec<usize> = others
            .iter()
            .copied()
            .filter(|&k| self.nodes[k].visible && self.nodes[k].rect[2 + axis] > 0.)
            .collect();
        drawn.sort_by(|&a, &b| centre(a, axis).total_cmp(&centre(b, axis)));
        // Array order against drawn order: reversed when the first child
        // is drawn after the last.
        let reversed = drawn.len() >= 2 && {
            let pos = |k: usize| others.iter().position(|&o| o == k).unwrap_or(0);
            pos(drawn[0]) > pos(drawn[drawn.len() - 1])
        };
        let k = drawn.iter().filter(|&&d| centre(d, axis) < p[axis]).count();
        let pos = |d: usize| others.iter().position(|&o| o == d).unwrap_or(0);
        let index = match (drawn.get(k), reversed) {
            (Some(&next), false) => pos(next),
            (None, false) => drawn.last().map_or(others.len(), |&l| pos(l) + 1),
            (Some(&next), true) => pos(next) + 1,
            (None, true) => drawn.last().map_or(0, |&l| pos(l)),
        };
        // The line sits between the neighbours, across the stack.
        let r = self.nodes[parent].rect;
        let at = match (k.checked_sub(1).and_then(|i| drawn.get(i)), drawn.get(k)) {
            (Some(&a), Some(&b)) => {
                let ra = self.nodes[a].rect;
                (ra[axis] + ra[axis + 2] + self.nodes[b].rect[axis]) / 2.
            }
            (Some(&a), None) => self.nodes[a].rect[axis] + self.nodes[a].rect[axis + 2],
            (None, Some(&b)) => self.nodes[b].rect[axis],
            (None, None) => r[axis],
        };
        let o = 1 - axis;
        Some(Slot {
            parent: self.nodes[parent].widget,
            index,
            line: Guide {
                vertical: axis == 0,
                at,
                from: r[o],
                to: r[o] + r[o + 2],
            },
        })
    }

    /// Drops `node` into its parent's stack at design point `p`.
    fn drop_in_stack(&mut self, node: usize, p: [f32; 2]) -> anyhow::Result<String> {
        let slot = self
            .stack_slot(node, p)
            .ok_or_else(|| anyhow::anyhow!("its parent is not this file’s"))?;
        let widgets = self.asset.widgets();
        let w = self.nodes[node].widget;
        let now = widgets[slot.parent].children.iter().position(|&c| c == w);
        if now == Some(slot.index) {
            return Ok(String::new());
        }
        let id = super::hierarchy::object_id(&self.asset, widgets[w].object);
        let next = self
            .asset
            .move_widget(&widgets, w, slot.parent, slot.index)?;
        self.restructure(next, Some(id));
        Ok(format!(
            "Moved {} in its stack",
            widgets[w].name.clone().unwrap_or_default()
        ))
    }

    /// The part of the page the canvas pane shows, in design pixels.
    pub(super) fn visible_region(&self) -> [f32; 4] {
        let s = self.canvas_bounds.size;
        let a = self.canvas.view.to_design(point(px(0.), px(0.)));
        let b = self.canvas.view.to_design(point(s.width, s.height));
        [a[0], a[1], b[0] - a[0], b[1] - a[1]]
    }
}

/// Selection frame, hover outline, snap guides and the drag readout: one
/// painted layer over the canvas.
pub fn chrome(ui: &UiEditor, drag: Option<&(usize, [f32; 4], Vec<Guide>)>) -> Vec<AnyElement> {
    let view = &ui.canvas.view;
    let screen = |[x, y, w, h]: [f32; 4]| {
        let p = view.to_screen(x, y);
        Bounds::new(p, size(px(w * view.zoom), px(h * view.zoom)))
    };
    let selected = ui
        .canvas
        .selected
        .filter(|&s| s > 0)
        .and_then(|s| Some((s, ui.nodes.get(s)?)))
        .map(|(s, n)| match drag {
            Some((d, r, _)) if *d == s => (*r, n.foreign),
            _ => (n.rect, n.foreign),
        });
    // Named, with how a drag treats it when that is not a plain move.
    let hover = ui
        .canvas
        .hover
        .and_then(|h| Some((ui.nodes.get(h)?, ui.frames.get(h)?)))
        .map(|(n, f)| {
            let note = match f.placed_by {
                _ if n.foreign => " · template, double-click to edit it here",
                Some("StackLayout") => " · drag to reorder",
                Some(_) => " · placed by its parent",
                None if f.bound.position.is_some() => " · position bound",
                None => "",
            };
            (n.rect, format!("{}{note}", n.name))
        });
    let frames: Vec<[f32; 4]> = if ui.canvas.show_frames {
        let view_rect = ui.visible_region();
        ui.nodes
            .iter()
            .skip(1)
            .filter(|n| n.visible && !n.foreign && overlaps(n.rect, view_rect))
            .map(|n| n.rect)
            .collect()
    } else {
        Vec::new()
    };
    let guides: Vec<Guide> = drag.map(|d| d.2.clone()).unwrap_or_default();
    let selected_rect = selected.map(|s| screen(s.0));
    let foreign = selected.is_some_and(|s| s.1);
    let hover_rect = hover.as_ref().map(|h| screen(h.0));
    let marked: Vec<Bounds<Pixels>> = ui
        .canvas
        .marked
        .iter()
        .filter_map(|&m| ui.nodes.get(m))
        .map(|n| screen(n.rect))
        .collect();
    let guide_lines: Vec<[Point<Pixels>; 2]> = guides
        .iter()
        .map(|g| {
            if g.vertical {
                [view.to_screen(g.at, g.from), view.to_screen(g.at, g.to)]
            } else {
                [view.to_screen(g.from, g.at), view.to_screen(g.to, g.at)]
            }
        })
        .collect();
    let frames: Vec<Bounds<Pixels>> = frames.into_iter().map(screen).collect();
    let mut out = vec![
        gpui_editor_kit::gpui::canvas(
            |_, _, _| {},
            move |bounds, _, window, _| {
                let o = bounds.origin;
                let shift = |b: Bounds<Pixels>| Bounds::new(o + b.origin, b.size);
                for f in &frames {
                    window.paint_quad(outline(
                        shift(*f),
                        rgba(crate::colors::CANVAS_FRAME),
                        BorderStyle::Solid,
                    ));
                }
                for m in &marked {
                    window.paint_quad(outline(
                        shift(*m),
                        rgb(crate::colors::CANVAS_MARK),
                        BorderStyle::Dashed,
                    ));
                }
                if let Some(h) = hover_rect {
                    window.paint_quad(outline(
                        shift(h),
                        rgb(crate::colors::CANVAS_HOVER),
                        BorderStyle::Solid,
                    ));
                }
                if let Some(s) = selected_rect {
                    let color = if foreign {
                        rgb(crate::colors::CANVAS_FOREIGN)
                    } else {
                        theme::get().highlight
                    };
                    let style = if foreign {
                        BorderStyle::Dashed
                    } else {
                        BorderStyle::Solid
                    };
                    window.paint_quad(quad(
                        shift(s),
                        px(0.),
                        transparent_black(),
                        px(1.5),
                        color,
                        style,
                    ));
                }
                for [a, b] in &guide_lines {
                    gpui_editor_kit::paint::line(
                        &[o + *a, o + *b],
                        1.,
                        rgb(crate::colors::CANVAS_GUIDE),
                        window,
                    );
                }
            },
        )
        .absolute()
        .size_full()
        .into_any_element(),
    ];
    // Tags stay inside the pane: a widget at its edge (the construction
    // bar at the page foot) still names itself in full.
    let pane = ui.canvas_bounds.size;
    let inside = |p: Point<Pixels>, text: &str| {
        // 10 px text: about 6 px a character, plus the tag's padding.
        let w = text.chars().count() as f32 * 6. + 10.;
        let x = f32::from(p.x).min(f32::from(pane.width) - w).max(2.);
        let y = f32::from(p.y).min(f32::from(pane.height) - 18.).max(2.);
        point(px(x), px(y))
    };
    // The hovered widget's name, at its top-left corner.
    if let (Some((r, name)), None) = (&hover, drag) {
        let p = view.to_screen(r[0], r[1]);
        out.push(tag(
            inside(p - point(px(0.), px(17.)), name),
            name.to_string(),
            rgb(crate::colors::CANVAS_HOVER),
        ));
    }
    // Where a drag puts the widget, by the pointer.
    if let Some((_, r, _)) = drag {
        let text = format!("{:.0}, {:.0} · {:.0} × {:.0}", r[0], r[1], r[2], r[3]);
        let p = view.to_screen(r[0], r[1] + r[3]);
        out.push(tag(
            inside(p + point(px(0.), px(4.)), &text),
            text,
            rgb(crate::colors::PICK),
        ));
    }
    out
}

impl crate::editor::Editor {
    /// Over the canvas while a template is edited in place: where it sits
    /// (the screen, then each template from the outside in; click one to
    /// edit it instead) and Done.
    pub(super) fn scope_bar(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let ui = self.ui.as_ref()?;
        let chain = ui.scope_chain();
        if chain.is_empty() {
            return None;
        }
        let name_of = |path: &str| {
            self.ui_find
                .display_name(path)
                .unwrap_or_else(|| path.rsplit('/').next().unwrap_or(path).to_owned())
        };
        let root_path = ui
            .root
            .as_ref()
            .map_or_else(|| ui.path.clone(), |r| r.path.clone());
        let chip = |id: SharedString, label: String, current: bool| {
            div()
                .id(id)
                .px_2()
                .h(px(22.))
                .flex()
                .items_center()
                .rounded(px(3.))
                .text_size(px(11.))
                .when(current, |d| {
                    d.bg(theme::get().selected).text_color(theme::get().text)
                })
                .when(!current, |d| {
                    d.cursor_pointer()
                        .text_color(theme::get().muted)
                        .hover(|s| s.bg(theme::get().hover))
                })
                .child(label)
        };
        let mut bar = div()
            .id("ui-scope-bar")
            .absolute()
            .top(px(34.))
            .left_2()
            .max_w(relative(0.96))
            .flex()
            .flex_wrap()
            .items_center()
            .gap_1()
            .p(px(2.))
            .rounded(px(4.))
            .bg(theme::get().overlay)
            .border_1()
            .border_color(theme::get().accent)
            .tooltip(gpui_editor_kit::widgets::Tooltip::text(
                "Editing a template in place",
                "Esc",
                "Edits go into the template and change every place that uses it. Click a level to edit that one instead.",
            ))
            // It occludes the pane's press capture: it hands the keys to the
            // canvas itself, as the game view bar does (round 12: a Preview
            // value went on taking digits after a click on a level).
            .occlude()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    this.keys_to(super::Keys::Canvas);
                    cx.stop_propagation();
                }),
            )
            .child(chip("ui-scope-root".into(), name_of(&root_path), false).on_click(
                cx.listener(|this, _, _, cx| {
                    if let Some(ui) = &mut this.ui {
                        ui.leave_template();
                    }
                    cx.notify();
                }),
            ));
        let last = chain.len() - 1;
        for (k, (g, key)) in chain.into_iter().enumerate() {
            let label = name_of(&format!("{}{g}", super::GENERATED));
            bar = bar
                .child(div().text_color(theme::get().muted).child("›"))
                .child(
                    chip(
                        SharedString::from(format!("ui-scope-{k}")),
                        label,
                        k == last,
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if let Some(ui) = &mut this.ui {
                            match ui.enter_template(g, key) {
                                Ok(m) if !m.is_empty() => ui.message = Some((m, false)),
                                Ok(_) => {}
                                Err(e) => ui.message = Some((format!("{e:#}"), true)),
                            }
                        }
                        cx.notify();
                    })),
                );
        }
        bar = bar.child(
            gpui_editor_kit::widgets::button("Done", false)
                .tooltip(gpui_editor_kit::widgets::Tooltip::text(
                    "Back to the screen",
                    "Esc",
                    "",
                ))
                .on_click(cx.listener(|this, _, _, cx| {
                    if let Some(ui) = &mut this.ui {
                        ui.leave_template();
                    }
                    cx.notify();
                })),
        );
        Some(bar.into_any_element())
    }
}

/// A small dark caption at pane position `p`./// A small dark caption at pane position `p`.
fn tag(p: Point<Pixels>, text: String, color: Rgba) -> AnyElement {
    div()
        .absolute()
        .left(p.x)
        .top(p.y)
        .px_1()
        .rounded(px(2.))
        // Canvas marks, not chrome: dark in every theme like the marks.
        .bg(rgba(crate::colors::LABEL_BG))
        .text_size(px(10.))
        .text_color(color)
        .whitespace_nowrap()
        .child(text)
        .into_any_element()
}

/// Draws `nodes` as absolutely positioned elements of the canvas pane.
/// `drag` is the rectangle a move or resize in progress shows for its node.
/// `region` (design pixels) is what the pane shows; nodes outside it are
/// skipped. `on_handle(node, handle, position)` starts a resize of the
/// selection; without it the nodes are context only (other screens of the
/// game view) and get no handles. Nodes with a sprite draw through `gpu`,
/// in runs between GPUI's own elements. Returns the drawn widgets and the
/// handles, which go over the page's clip. Picking is not done per
/// element: the pane's own pointer handlers ask [`UiEditor::press`].
pub fn render(
    canvas: &Canvas,
    nodes: &[Node],
    in_scope: &super::Bits,
    drag: Option<(usize, [f32; 4])>,
    region: [f32; 4],
    gpu: &super::gpu::Renderer,
    on_handle: Option<
        impl Fn(usize, Handle, Point<Pixels>, &mut Window, &mut App) + Clone + 'static,
    >,
) -> (Vec<AnyElement>, Vec<AnyElement>) {
    let view = &canvas.view;
    let mut out = Vec::new();
    let mut overlay = Vec::new();
    // The game antialiases text in greyscale; GPUI's subpixel default
    // drew labels heavier, with colour fringes. The mode is read as glyphs
    // paint, so it is switched for the canvas's own elements only and
    // put back before the frames and the rest of the editor.
    let before = std::rc::Rc::new(std::cell::Cell::new(TextRenderingMode::PlatformDefault));
    let set = before.clone();
    out.push(
        gpui_editor_kit::gpui::canvas(
            |_, _, _| {},
            move |_, _, _, cx| {
                set.set(cx.text_rendering_mode());
                cx.set_text_rendering_mode(TextRenderingMode::Grayscale);
            },
        )
        .absolute()
        .into_any_element(),
    );
    let mut runs = Runs::default();
    // A widget being moved takes everything under it along; masks stay
    // where they are until the relayout.
    let carried = drag.and_then(|(d, r)| {
        let n = nodes.get(d)?;
        let end = (d + 1..nodes.len())
            .find(|&k| nodes[k].depth <= n.depth)
            .unwrap_or(nodes.len());
        (r[2] == n.rect[2] && r[3] == n.rect[3])
            .then_some((d + 1..end, [r[0] - n.rect[0], r[1] - n.rect[1]]))
    });
    // Masks the moving widgets provide move with them; the rest stay.
    let mut moving: Vec<usize> = match (drag, &carried) {
        (Some((d, _)), Some((range, _))) => std::iter::once(d)
            .chain(range.clone())
            .map(|k| nodes[k].tree)
            .collect(),
        _ => Vec::new(),
    };
    moving.sort_unstable();
    let moves = |by: usize| moving.binary_search(&by).is_ok();
    for (i, node) in nodes.iter().enumerate() {
        if !node.visible {
            continue;
        }
        let rect = match (drag, &carried) {
            (Some((d, r)), _) if d == i => r,
            (_, Some((range, [dx, dy]))) if range.contains(&i) => [
                node.rect[0] + dx,
                node.rect[1] + dy,
                node.rect[2],
                node.rect[3],
            ],
            _ => node.rect,
        };
        let [x, y, w, h] = rect;
        let shift = [rect[0] - node.rect[0], rect[1] - node.rect[1]];
        let clip = node
            .clip
            .map(|c| match shift != [0.; 2] && moves(node.clip_by) {
                true => [c[0] + shift[0], c[1] + shift[1], c[2], c[3]],
                false => c,
            });
        let p = view.to_screen(x, y);
        let (sw, sh) = (w * view.zoom, h * view.zoom);
        // Off the pane, or masked to a region off it. Text running out of
        // its box (HorizontalOverflow visible) gets a box's width of slack.
        let slack = if node.text.is_some() { w.max(h) } else { 0. };
        let reach = [x - slack, y - slack, w + 2. * slack, h + 2. * slack];
        if sw < 0.5
            || sh < 0.5
            || !overlaps(reach, region)
            || clip.is_some_and(|c| !overlaps(c, region))
        {
            continue;
        }
        // A move or resize in progress draws the sprite where it is dragged
        // to, mapped onto the new box.
        let moved = [rect[0] - node.rect[0], rect[1] - node.rect[1]];
        let resized = rect[2] != node.rect[2] || rect[3] != node.rect[3];
        let drawn = match &node.sprite {
            Some(sprite) if !in_scope.is_empty() && !in_scope.get(i) => {
                Some(Arc::new(sprite.faded(0.3)))
            }
            Some(sprite) if !resized && moved == [0.; 2] => Some(sprite.clone()),
            Some(sprite) if !resized => Some(Arc::new(sprite.dragged(moved, moves))),
            Some(sprite) => Some(Arc::new(sprite.resized(node.rect, rect))),
            None => None,
        };
        if let Some(sprite) = drawn {
            // A pixel of slack for filtering at the edges.
            let r = [x - 1., y - 1., w + 2., h + 2.];
            runs.sprite(sprite, clip.map_or(r, |c| intersect(r, c)));
            continue;
        }
        // Groups draw nothing of their own: no element (picking is the
        // pane's, not per element).
        if !(node.image.is_some()
            || node.slices.is_some()
            || node.text.is_some()
            || node.kind == Kind::Image)
        {
            continue;
        }
        let covers = clip.map_or(reach, |c| intersect(reach, c));
        // Content and the frame/hit box are separate: a clip cuts the
        // content, never the selection outline.
        let clip = clip.map(|[cx, cy, cw, ch]| {
            let q = view.to_screen(cx, cy);
            (q, cw * view.zoom, ch * view.zoom)
        });
        let (origin, cw, ch) = match clip {
            Some((q, ..)) if !node.baked => (p - q, sw, sh),
            Some((_, cw, ch)) => (point(px(0.), px(0.)), cw, ch),
            None => (point(px(0.), px(0.)), sw, sh),
        };
        // Outside a template edited in place, the screen is context only.
        let dim = if !in_scope.is_empty() && !in_scope.get(i) {
            0.3
        } else {
            1.
        };
        let mut el = div()
            .absolute()
            .left(origin.x)
            .top(origin.y)
            .w(px(cw))
            .h(px(ch))
            .opacity((node.opacity * dim).clamp(0., 1.))
            // Text with HorizontalOverflow Visible (and wrapped lines past
            // a short box) runs out of its box, as in game.
            .when(node.text.is_none() || matches!(node.overflow, 2..=4), |d| {
                d.overflow_hidden()
            });
        if node.baked {
            if let Some(image) = &node.image {
                el = el.child(img(image.clone()).size_full().object_fit(ObjectFit::Fill));
            }
        } else if let Some(slices) = &node.slices {
            // Fixed borders, stretched edges and centre; borders shrink
            // together when the box is smaller than both of them.
            let [l, t, r, b] = slices.margins.map(|m| m * view.zoom);
            let kx = (sw / (l + r).max(0.01)).min(1.);
            let ky = (sh / (t + b).max(0.01)).min(1.);
            let xs = [0., l * kx, sw - r * kx, sw];
            let ys = [0., t * ky, sh - b * ky, sh];
            for (k, piece) in slices.pieces.iter().enumerate() {
                let (cx, cy) = (k % 3, k / 3);
                let (w, h) = (xs[cx + 1] - xs[cx], ys[cy + 1] - ys[cy]);
                if w > 0.1 && h > 0.1 {
                    el = el.child(
                        img(piece.clone())
                            .absolute()
                            .left(px(xs[cx]))
                            .top(px(ys[cy]))
                            .w(px(w))
                            .h(px(h))
                            .object_fit(ObjectFit::Fill),
                    );
                }
            }
        } else if let Some(image) = &node.image {
            el = el.child(img(image.clone()).size_full().object_fit(ObjectFit::Fill));
        } else if node.kind == Kind::Image {
            let [r, g, b] = node.color.unwrap_or([120, 130, 150]);
            el = el.bg(rgba(
                u32::from(r) << 24 | u32::from(g) << 16 | u32::from(b) << 8 | 0x60,
            ));
        }
        if let Some(text) = &node.text {
            // The label box is an estimate; align inside it the way the
            // game aligns against its anchor, and let text overflow.
            let layer = |[r, g, b]: [u8; 3], alpha: f32| {
                div()
                    .absolute()
                    .size_full()
                    .flex()
                    .whitespace_nowrap()
                    .map(|d| match node.overflow {
                        2 | 3 => d.overflow_x_hidden(),
                        4 => d.overflow_x_hidden().text_ellipsis(),
                        _ => d,
                    })
                    .text_size(px((node.text_size * view.zoom).max(3.)))
                    .when(node.line_height > 0., |d| {
                        d.line_height(px((node.line_height * view.zoom).max(3.)))
                    })
                    .when_some(node.font.clone(), |d, f| d.font_family(f))
                    .text_color(rgba(
                        u32::from(r) << 24
                            | u32::from(g) << 16
                            | u32::from(b) << 8
                            | (alpha.clamp(0., 1.) * 255.) as u32,
                    ))
                    .map(|d| match node.align[0] {
                        1 => d.justify_center(),
                        2 => d.justify_end(),
                        _ => d.justify_start(),
                    })
                    .map(|d| match node.align[1] {
                        1 => d.items_center(),
                        2 | 3 => d.items_end(),
                        _ => d.items_start(),
                    })
                    .map(|d| {
                        if !text.contains('\n') {
                            return d.child(text.clone());
                        }
                        // Wrapped lines, aligned per line like the game does.
                        d.child(
                            div()
                                .flex()
                                .flex_col()
                                .map(|c| match node.align[0] {
                                    1 => c.items_center(),
                                    2 => c.items_end(),
                                    _ => c.items_start(),
                                })
                                .children(text.lines().map(|l| div().child(l.to_owned()))),
                        )
                    })
            };
            // The TextStyle's drop shadow: the text again, offset, under it.
            if let Some(sh) = node.shadow {
                el = el.child(
                    layer(sh.color, sh.alpha)
                        .left(px(sh.offset[0] * view.zoom))
                        .top(px(sh.offset[1] * view.zoom)),
                );
            }
            el = el.child(layer(node.color.unwrap_or([235, 235, 235]), 1.));
        }
        let content = match clip {
            Some((q, cw, ch)) => Some(
                div()
                    .absolute()
                    .left(q.x)
                    .top(q.y)
                    .w(px(cw))
                    .h(px(ch))
                    .overflow_hidden()
                    .child(el),
            ),
            None => Some(el.left(p.x).top(p.y)),
        };
        if let Some(content) = content {
            runs.element(content.into_any_element(), covers);
        }
    }
    // Resize handles on the selection, drawn or not (a hidden widget picked
    // in the tree still resizes).
    if let Some(on) = on_handle
        && let Some(i) = canvas.selected.filter(|&i| i > 0)
        && let Some(node) = nodes.get(i).filter(|n| !n.foreign)
    {
        let rect = match drag {
            Some((d, r)) if d == i => r,
            _ => node.rect,
        };
        overlay.extend(view.handles(rect, move |handle, at, window, cx| {
            on(i, handle, at, window, cx)
        }));
    }
    let o = view.to_screen(0., 0.);
    let (origin, zoom) = ([f32::from(o.x), f32::from(o.y)], view.zoom);
    out.extend(runs.paint.into_iter().map(|p| match p {
        Ok(el) => el,
        Err(sprites) => {
            let gpu = gpu.clone();
            gpui_editor_kit::gpui::canvas(
                |_, _, _| {},
                move |bounds, _, window, _| {
                    super::gpu::Batch::paint(&gpu, sprites, origin, zoom, bounds, window)
                },
            )
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .into_any_element()
        }
    }));
    out.push(
        gpui_editor_kit::gpui::canvas(
            |_, _, _| {},
            move |_, _, _, cx| cx.set_text_rendering_mode(before.get()),
        )
        .absolute()
        .into_any_element(),
    );
    (out, overlay)
}

impl crate::editor::Editor {
    /// The right-click menu: every widget under the pointer (click one to
    /// select it), then actions on the selection.
    pub(super) fn canvas_menu(&self, menu: Menu, cx: &mut Context<Self>) -> AnyElement {
        use gpui_editor_kit::widgets::{menu_header, menu_item};
        let Some(ui) = &self.ui else {
            return div().into_any_element();
        };
        // Inside the canvas: flipped above or left of the pointer when it
        // would run past the bottom or right edge, and never taller than
        // the pane.
        let pane = ui.canvas_bounds.size;
        let (w, rows) = (260., menu.stack.len() as f32);
        let want = 34.
            + rows * 24.
            + if ui.canvas.selected.is_some_and(|s| s > 0) {
                96.
            } else {
                0.
            };
        let h = want.min(f32::from(pane.height) - 16.).max(80.);
        let x = if f32::from(menu.at.x) + w > f32::from(pane.width) - 8. {
            (f32::from(menu.at.x) - w).max(8.)
        } else {
            f32::from(menu.at.x)
        };
        let y = (f32::from(menu.at.y))
            .min(f32::from(pane.height) - h - 8.)
            .max(8.);
        let mut popup = div()
            .id("ui-canvas-menu")
            .occlude()
            .absolute()
            .left(px(x))
            .top(px(y))
            .max_h(px(h))
            .overflow_y_scroll()
            .w(px(w))
            .bg(theme::get().popup)
            .rounded(px(6.))
            .border_1()
            .border_color(theme::get().floating_border)
            .shadow_lg()
            .p_1()
            .flex()
            .flex_col()
            .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                if let Some(ui) = &mut this.ui {
                    ui.canvas.menu = None;
                }
                cx.notify();
            }))
            .child(menu_header("Select"));
        for (k, &n) in menu.stack.iter().enumerate() {
            let node = &ui.nodes[n];
            let kind = match node.kind {
                Kind::Image => "image",
                Kind::Label => "label",
                Kind::Container if ui.nodes.get(n + 1).is_some_and(|c| c.foreign) => "instance",
                Kind::Container => "group",
            };
            let kind = if node.foreign {
                format!("{kind} · template")
            } else {
                kind.to_owned()
            };
            popup = popup.child(
                div()
                    .id(("ui-menu-pick", k))
                    .flex()
                    .items_center()
                    .gap_2()
                    .h(px(24.))
                    .pl(px(8.))
                    .pr_2()
                    .rounded(px(4.))
                    .cursor_pointer()
                    .hover(|s| s.bg(theme::get().hover))
                    .when(ui.canvas.selected == Some(n), |d| {
                        d.bg(theme::get().selected)
                    })
                    .text_size(px(11.))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .child(node.name.to_string()),
                    )
                    .child(
                        div()
                            .text_size(px(10.))
                            .text_color(theme::get().muted)
                            .child(kind),
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if let Some(ui) = &mut this.ui {
                            ui.canvas.menu = None;
                            ui.select_widget(n);
                        }
                        cx.notify();
                    })),
            );
        }
        let act = |label: &'static str, key: &'static str, f: fn(&mut Self)| {
            menu_item(label, key).on_click(cx.listener(move |this, _, _, cx| {
                if let Some(ui) = &mut this.ui {
                    ui.canvas.menu = None;
                }
                f(this);
                cx.notify();
            }))
        };
        if ui.canvas.selected.is_some_and(|s| s > 0) {
            popup = popup
                .child(div().h(px(1.)).my_1().bg(theme::get().border))
                .child(act("Select parent", "Esc", |this| {
                    if let Some(ui) = &mut this.ui {
                        ui.escape();
                    }
                }))
                .child(act("Duplicate", "Ctrl D", |this| {
                    if let Some(ui) = &mut this.ui {
                        let r = ui.duplicate_selected();
                        this.ui_tree_result(r);
                    }
                }))
                .child(act("Delete", "Del", |this| {
                    if let Some(ui) = &mut this.ui {
                        let r = ui.delete_selected();
                        this.ui_tree_result(r);
                    }
                }));
        }
        popup.into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::{Change, Node};
    use crate::ui::UiEditor;
    use crate::ui::test_support::{TECH_TREE, movable, node, number_input, open};
    use gpui_editor_kit::gpui::{Modifiers, Pixels, Point, point, px};

    /// The first node `pick` takes that is topmost at its centre, and that
    /// centre on screen: where a click lands on it.
    fn on_top(ui: &UiEditor, pick: impl Fn(&Node) -> bool) -> Option<(usize, Point<Pixels>)> {
        (1..ui.nodes.len())
            .filter(|&i| pick(&ui.nodes[i]))
            .find_map(|i| {
                let r = ui.nodes[i].rect;
                let c = [r[0] + r[2] / 2., r[1] + r[3] / 2.];
                (ui.under(c).first() == Some(&i)).then(|| (i, ui.canvas.view.to_screen(c[0], c[1])))
            })
    }

    /// Where the file's own widget `widget` is drawn.
    fn own_rect(ui: &UiEditor, widget: usize) -> [f32; 4] {
        let n = ui.nodes.iter().find(|n| n.widget == widget && !n.foreign);
        n.unwrap().rect
    }

    // Needs the game: every widget of the tech tree that its own Position
    // places moves by what a canvas drag asks, nothing else under it.
    // Dragging a stacked item past its last sibling makes it the last one
    // drawn, and undo puts it back.
    #[test]
    fn drags_move_widgets_and_reorder_stacks() {
        let Some(mut ui) = open(TECH_TREE) else {
            return;
        };
        let widgets: Vec<usize> = (1..ui.nodes.len())
            .filter(|&i| movable(&ui, i) && ui.nodes[i].visible && ui.nodes[i].rect[2] > 0.)
            .collect();
        assert!(widgets.len() > 10);
        let mut stuck = Vec::new();
        for n in widgets {
            let widget = ui.nodes[n].widget;
            let before = ui.nodes[n].rect;
            let after = [before[0] + 25., before[1] - 10., before[2], before[3]];
            ui.canvas_change(Change {
                widget,
                before,
                after,
            });
            let now = own_rect(&ui, widget);
            if (now[0] - after[0]).abs() > 0.6 || (now[1] - after[1]).abs() > 0.6 {
                stuck.push(format!("{} {before:?} -> {now:?}", ui.nodes[n].name));
            }
            ui.undo(false);
        }
        assert!(stuck.is_empty(), "{stuck:#?}");

        // A stack of three visible items of this file.
        let parent = (1..ui.nodes.len())
            .filter(|&i| {
                !ui.nodes[i].foreign
                    && ui.frames[i].placed_by == Some("StackLayout")
                    && ui.nodes[i].visible
            })
            .find_map(|i| {
                let p = ui.parent_of(i)?;
                let n = (1..ui.nodes.len())
                    .filter(|&k| ui.parent_of(k) == Some(p) && ui.nodes[k].visible)
                    .count();
                (n >= 3).then_some(p)
            })
            .expect("a stack of three");
        let kids = |ui: &UiEditor| -> Vec<usize> {
            (1..ui.nodes.len())
                .filter(|&k| ui.parent_of(k) == Some(parent) && ui.nodes[k].visible)
                .collect()
        };
        let rects: Vec<[f32; 4]> = kids(&ui).iter().map(|&k| ui.nodes[k].rect).collect();
        let (lo, hi) = rects
            .iter()
            .fold(([f32::MAX; 2], [f32::MIN; 2]), |(lo, hi), r| {
                (
                    [lo[0].min(r[0]), lo[1].min(r[1])],
                    [hi[0].max(r[0] + r[2]), hi[1].max(r[1] + r[3])],
                )
            });
        let axis = usize::from(hi[1] - lo[1] > hi[0] - lo[0]);
        let along = |ui: &UiEditor, a: &usize, b: &usize| {
            ui.nodes[*a].rect[axis].total_cmp(&ui.nodes[*b].rect[axis])
        };
        // The first one drawn, dropped past the far end.
        let node = *kids(&ui).iter().min_by(|a, b| along(&ui, a, b)).unwrap();
        let name = ui.nodes[node].name.clone();
        let mut p = [lo[0] + 1., lo[1] + 1.];
        p[axis] = hi[axis] + 5.;
        let message = ui.drop_in_stack(node, p).unwrap();
        assert!(message.contains("stack"), "{message}");
        let last = *kids(&ui).iter().max_by(|a, b| along(&ui, a, b)).unwrap();
        assert_eq!(ui.nodes[last].name, name);
        assert!(ui.undo(false));
        let back: Vec<[f32; 4]> = kids(&ui).iter().map(|&k| ui.nodes[k].rect).collect();
        assert_eq!(back, rects);
    }

    // Needs the game: a click selects the deepest widget drawn there, a
    // template's included; dragging it moves the container instance that
    // shows it, and the drag's size is left alone.
    #[test]
    fn clicks_pick_the_deepest_widget_and_drags_move_its_instance() {
        let Some(mut ui) = open("questtrackerscene") else {
            return;
        };
        let (node, screen) = on_top(&ui, |n| n.foreign && n.text.is_some() && n.visible)
            .expect("a template label on top at its centre");
        ui.press(screen, Modifiers::default(), 1, None);
        assert_eq!(ui.canvas.selected, Some(node));
        let owner = ui.movable(node).unwrap();
        let (widget, before) = (ui.nodes[owner].widget, ui.nodes[owner].rect);
        let to = screen + point(px(30.), px(0.));
        ui.pointer_move(screen + point(px(5.), px(0.)), Modifiers::default(), true);
        ui.pointer_move(to, Modifiers::default(), true);
        assert_eq!(ui.canvas.selected, Some(owner));
        ui.release(to);
        let now = own_rect(&ui, widget);
        let dx = 30. / ui.canvas.view.zoom;
        assert!(
            (now[0] - before[0] - dx).abs() <= 1.,
            "{before:?} -> {now:?}"
        );
        assert_eq!([now[2], now[3]], [before[2], before[3]]);
    }

    // Needs the game: in the construction menu, whose only own widget is
    // one container, a double click on a nested template's widget edits
    // that template in place: the widget moves with a drag and the screen
    // around it shows the move, leaving keeps the edit, and saving writes
    // the template into the mod.
    #[test]
    fn nested_templates_edit_in_place() {
        let Some(mut ui) = open("constructionmenuscene") else {
            return;
        };
        // A drawn widget two templates deep, topmost at its centre.
        let (node, screen) = on_top(&ui, |n| {
            n.foreign && n.visible && n.owners.len() >= 2 && n.image.is_some() && n.rect[2] > 4.
        })
        .expect("a nested widget");
        let (g, _) = ui.nodes[node].owners[0];
        let widget = ui.nodes[node].widget;
        ui.press(screen, Modifiers::default(), 2, None);
        assert_eq!(ui.scope.map(|s| s.0), Some(g));
        assert!(!ui.nodes[node].foreign && ui.nodes[node].widget == widget);
        assert_eq!(ui.canvas.selected, Some(node));
        assert_eq!(ui.scope_chain().len(), ui.nodes[node].owners.len());
        // Outside the instance is context, inside is editable.
        assert!(ui.in_scope.get(node) && !ui.in_scope.get(0));
        let before = ui.nodes[node].rect;
        if movable(&ui, node) {
            let mut after = before;
            after[0] += 20.;
            let pos_before = ui.frames[node].position;
            ui.canvas_change(Change {
                widget,
                before,
                after,
            });
            // The template's stored Position moved by the drag; what draws
            // follows the template's own layout (a centred group recentres
            // around its moved child).
            assert!((ui.frames[node].position[0] - pos_before[0] - 20.).abs() < 0.01);
            let moved = ui.nodes[node].rect;
            assert_ne!(moved, before);
            assert!(ui.dirty());
            // Back on the screen the edit stays and still shows.
            ui.leave_template();
            assert!(ui.scope.is_none() && ui.dirty());
            assert_eq!(ui.nodes[node].rect, moved);
            let config = tempfile::tempdir().unwrap();
            let mut project =
                crate::project::Project::load(config.path(), anno_formats::game::Game::Anno117);
            assert_eq!(ui.stage(&mut project).unwrap(), 1);
            assert!(!ui.dirty());
            let folder = project.write().unwrap();
            // The screen itself was not edited: no copy of it.
            let file = |f: &str| folder.join(format!("data/ui/studio/generated/{f}"));
            assert!(!file("constructionmenuscene").exists());
            assert!(file(&g.to_string()).exists());
        }
    }

    // Needs the game: on the quest tracker (a screen laid out before, so
    // its nodes are kept between layouts), a double click on an objective's
    // text edits its template: the template's widgets become editable with
    // edit frames. Esc walks up inside the template only, clears the
    // selection at its top, then steps out.
    #[test]
    fn in_place_edits_stay_in_their_template() {
        let Some(mut ui) = open("questtrackerscene") else {
            return;
        };
        // Laid out twice: the second layout keeps the first one's nodes.
        ui.relayout();
        let (node, screen) = on_top(&ui, |n| {
            n.foreign && n.text.is_some() && n.visible && n.owners.len() >= 2
        })
        .expect("a nested template label");
        ui.press(screen, Modifiers::default(), 2, None);
        assert!(ui.scope.is_some());
        assert!(
            !ui.nodes[node].foreign,
            "the template's widgets are editable"
        );
        assert!(ui.frames[node].position_span.is_some());
        assert!(!ui.scope_chain().is_empty());
        let scope = ui.scope;
        let (g, key) = scope.unwrap();
        for _ in 0..12 {
            if ui.canvas.selected.is_none() {
                break;
            }
            assert!(ui.escape());
            if let Some(s) = ui.canvas.selected {
                assert!(ui.in_scope.get(s), "Esc left the template");
            }
            assert_eq!(ui.scope, scope);
        }
        assert!(ui.canvas.selected.is_none());
        // Nothing selected in the template: the Bindings pane is about it,
        // not the screen around it.
        assert_eq!(ui.node_file(ui.subject()).0, Some(g));
        assert!(ui.escape());
        assert_ne!(ui.scope, scope, "with nothing selected, Esc steps out");

        // A Preview value being typed when the scope changes is applied to
        // the input it was typed into, and typing ends (round 12: it went
        // on typing across the change, its indices naming another file's
        // inputs).
        ui.leave_template();
        let (fi, pi) = number_input(&ui);
        let (guid, uid) = (
            ui.preview.files[fi].guid,
            ui.preview.files[fi].props[pi].uid,
        );
        let count = |ui: &UiEditor| ui.preview.values.values().map(|m| m.len()).sum::<usize>();
        let before = count(&ui);
        ui.preview.edit = Some(gpui_editor_kit::field::TextEdit::new(
            (fi, pi),
            "17.5",
            gpui_editor_kit::field::Filter::Text,
        ));
        // Into it again from the scope bar: no press reaches the canvas.
        ui.enter_template(g, key).unwrap();
        assert!(ui.preview.edit.is_none(), "typing ended");
        assert_eq!(
            ui.preview.values.get(&guid).and_then(|m| m.get(&uid)),
            Some(&anno_formats::phx::DefaultValue::F32(17.5f32.to_bits())),
            "applied where it was typed"
        );
        assert_eq!(count(&ui), before + 1, "and nowhere else");

        // An edit in the template, then Done: undo goes back into the
        // template and takes the edit back; redo puts it back.
        let n = (1..ui.nodes.len())
            .find(|&i| {
                let f = &ui.frames[i];
                !ui.nodes[i].foreign && f.position_span.is_some() && f.bound.position.is_none()
            })
            .expect("a widget of the template with a stored Position");
        let (w, before) = (ui.nodes[n].widget, ui.frames[n].position);
        ui.select_widget(n);
        ui.type_position_x(&format!("{}", before[0] + 7.));
        ui.commit_layout_edit().unwrap();
        ui.leave_template();
        assert!(ui.scope.is_none() && ui.dirty());
        // Unsaved lists the template, with a copy to edit it at, and
        // not the untouched screen.
        let unsaved = ui.unsaved();
        assert_eq!(unsaved.len(), 1);
        assert_eq!(unsaved[0].0, Some(g));
        assert!(unsaved[0].2.is_some());
        let pos = |ui: &UiEditor| {
            let n = (1..ui.nodes.len())
                .find(|&i| !ui.nodes[i].foreign && ui.nodes[i].widget == w)
                .unwrap();
            ui.frames[n].position
        };
        // Named as the UI files list names it, not by its guid.
        ui.names = std::sync::Arc::new([(g, "Body Atom".to_owned())].into_iter().collect());
        assert!(ui.undo(false));
        assert_eq!(ui.scope, scope, "undo went back into the template");
        let said = ui.message.clone().unwrap().0;
        assert_eq!(said, "Undid a change to the Body Atom template");
        assert_eq!(pos(&ui), before);
        assert!(ui.undo(true));
        assert_eq!(pos(&ui)[0], before[0] + 7.);
        // Saved: nothing left to list.
        let config = tempfile::tempdir().unwrap();
        let mut project =
            crate::project::Project::load(config.path(), anno_formats::game::Game::Anno117);
        assert_eq!(ui.stage(&mut project).unwrap(), 1);
        assert!(ui.unsaved().is_empty());
    }

    // Needs the game: dragging the minimap container carries the masks its
    // own widgets provide (round 5 left them behind and cut the ring into
    // wedges until release), and only those: a mask from outside the
    // dragged subtree stays where it is.
    #[test]
    fn a_dragged_container_carries_its_own_masks() {
        let Some(ui) = open("minimapscene") else {
            return;
        };
        let nodes = &ui.nodes;
        let c = node(&ui, "MinimapSceneContainer");
        let end = (c + 1..nodes.len())
            .find(|&k| nodes[k].depth <= nodes[c].depth)
            .unwrap_or(nodes.len());
        let mut moving: Vec<usize> = nodes[c..end].iter().map(|n| n.tree).collect();
        moving.sort_unstable();
        let carried = |by: usize| moving.binary_search(&by).is_ok();
        // Clips: the nearest masks of what draws in it are its own.
        let clipped: Vec<_> = nodes[c..end]
            .iter()
            .filter(|n| n.clip.is_some() && n.clip_by != usize::MAX)
            .collect();
        assert!(!clipped.is_empty(), "the minimap clips its content");
        assert!(clipped.iter().any(|n| carried(n.clip_by)));
        // Sprites (the GPU path; tests draw on the CPU unless ANNO_UI_GPU=1).
        if !crate::ui::gpu::enabled() {
            return;
        }
        let d = [10., 0.];
        let mut own = 0;
        for n in &nodes[c..end] {
            let Some(sprite) = &n.sprite else { continue };
            let dragged = sprite.dragged(d, carried);
            for (before, after) in sprite.masks.iter().zip(&dragged.masks) {
                if carried(before.by) {
                    own += 1;
                    assert_eq!(
                        after.quad,
                        before.quad.moved(d),
                        "{} carries its mask",
                        n.name
                    );
                } else {
                    assert_eq!(after.quad, before.quad, "{} leaves an outside mask", n.name);
                }
            }
        }
        assert!(own > 0, "the minimap draws through masks of its own");
    }
}
