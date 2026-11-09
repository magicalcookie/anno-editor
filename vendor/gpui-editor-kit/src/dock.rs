// Docked panes: a persistent split/tab layout tree and its GPUI rendering.
//
// Panes are identified by a small `Copy` id the host defines, usually an
// enum. Each id appears at most once, so moving a tab never duplicates a
// view, and emptied tab groups collapse their split.
use crate::{theme, widgets::DragLabel};
use anyhow::{Context as _, Result, ensure};
use gpui::{prelude::*, *};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::{
    collections::{BTreeMap, BTreeSet},
    fmt::Debug,
    path::PathBuf,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Axis {
    /// Side by side.
    Horizontal,
    /// Stacked.
    Vertical,
}

/// Where a dragged tab lands relative to the target pane.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Edge {
    Center,
    Left,
    Right,
    Top,
    Bottom,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Branch {
    First,
    Second,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Layout<P> {
    Tabs {
        #[serde(alias = "views")]
        panes: Vec<P>,
        active: P,
    },
    Split {
        axis: Axis,
        ratio: f32,
        first: Box<Layout<P>>,
        second: Box<Layout<P>>,
    },
}

// Saved layouts are user files. Bound what we parse and how deep we recurse.
const MAX_FILE: u64 = 32 * 1024;
const MAX_DEPTH: usize = 8;

impl<P: Copy + Ord> Layout<P> {
    pub fn tabs(panes: Vec<P>, active: P) -> Self {
        assert!(!panes.is_empty() && panes.contains(&active));
        Self::Tabs { panes, active }
    }

    pub fn split(axis: Axis, ratio: f32, first: Self, second: Self) -> Self {
        Self::Split {
            axis,
            ratio: ratio.clamp(0.1, 0.9),
            first: Box::new(first),
            second: Box::new(second),
        }
    }

    pub fn validate(&self) -> Result<()> {
        fn visit<P: Copy + Ord>(
            node: &Layout<P>,
            seen: &mut BTreeSet<P>,
            depth: usize,
        ) -> Result<()> {
            ensure!(depth < MAX_DEPTH, "View layout nesting is too deep");
            match node {
                Layout::Tabs { panes, active } => {
                    ensure!(
                        !panes.is_empty() && panes.contains(active),
                        "View layout has an empty or invalid tab group"
                    );
                    for pane in panes {
                        ensure!(
                            seen.insert(*pane),
                            "A view occurs more than once in the layout"
                        );
                    }
                }
                Layout::Split {
                    ratio,
                    first,
                    second,
                    ..
                } => {
                    ensure!(
                        ratio.is_finite() && (0.1..=0.9).contains(ratio),
                        "Invalid split ratio"
                    );
                    visit(first, seen, depth + 1)?;
                    visit(second, seen, depth + 1)?;
                }
            }
            Ok(())
        }
        visit(self, &mut BTreeSet::new(), 0)
    }

    /// The active tab of the first group, which takes focus after a close.
    pub fn primary(&self) -> P {
        match self {
            Self::Tabs { active, .. } => *active,
            Self::Split { first, .. } => first.primary(),
        }
    }

    /// Every pane in reading order: a split's first side before its second,
    /// tabs left to right. For menus that list panes as they sit on screen.
    pub fn panes(&self) -> Vec<P>
    where
        P: Copy,
    {
        match self {
            Layout::Tabs { panes, .. } => panes.clone(),
            Layout::Split { first, second, .. } => {
                let mut out = first.panes();
                out.extend(second.panes());
                out
            }
        }
    }

    pub fn contains(&self, id: P) -> bool {
        match self {
            Self::Tabs { panes, .. } => panes.contains(&id),
            Self::Split { first, second, .. } => first.contains(id) || second.contains(id),
        }
    }

    pub fn activate(&mut self, id: P) -> bool {
        match self {
            Self::Tabs { panes, active } if panes.contains(&id) => {
                *active = id;
                true
            }
            Self::Tabs { .. } => false,
            Self::Split { first, second, .. } => first.activate(id) || second.activate(id),
        }
    }

    /// Activates `id`, adding it to the first tab group if it was closed.
    pub fn show(&mut self, id: P) {
        if self.activate(id) {
            return;
        }
        match self {
            Self::Tabs { panes, active } => {
                panes.push(id);
                *active = id;
            }
            Self::Split { first, .. } => first.show(id),
        }
    }

    /// Removes `id`. The last pane cannot be closed.
    pub fn close(&mut self, id: P) -> bool {
        let Some(next) = self.clone().remove(id) else {
            return false;
        };
        let changed = *self != next;
        *self = next;
        changed
    }

    pub fn move_pane(&mut self, id: P, target: P, edge: Edge) {
        if id == target || !self.contains(id) || !self.contains(target) {
            return;
        }
        let Some(mut next) = self.clone().remove(id) else {
            return;
        };
        next.attach(id, target, edge);
        *self = next;
    }

    pub fn resize(&mut self, path: &[Branch], ratio: f32) {
        if let Some((branch, path)) = path.split_first() {
            if let Self::Split { first, second, .. } = self {
                match branch {
                    Branch::First => first,
                    Branch::Second => second,
                }
                .resize(path, ratio);
            }
        } else if let Self::Split { ratio: current, .. } = self {
            *current = ratio.clamp(0.1, 0.9);
        }
    }

    fn remove(self, id: P) -> Option<Self> {
        match self {
            Self::Tabs { mut panes, active } => {
                panes.retain(|v| *v != id);
                let first = *panes.first()?;
                Some(Self::tabs(panes, if active == id { first } else { active }))
            }
            Self::Split {
                axis,
                ratio,
                first,
                second,
            } => match (first.remove(id), second.remove(id)) {
                (Some(first), Some(second)) => Some(Self::Split {
                    axis,
                    ratio,
                    first: Box::new(first),
                    second: Box::new(second),
                }),
                (Some(node), None) | (None, Some(node)) => Some(node),
                (None, None) => None,
            },
        }
    }

    fn attach(&mut self, id: P, target: P, edge: Edge) {
        match self {
            Self::Tabs { panes, active } if panes.contains(&target) => {
                if edge == Edge::Center {
                    panes.push(id);
                    *active = id;
                    return;
                }
                let prior = std::mem::replace(self, Self::tabs(vec![id], id));
                let new = Self::tabs(vec![id], id);
                let (first, second) = if matches!(edge, Edge::Left | Edge::Top) {
                    (new, prior)
                } else {
                    (prior, new)
                };
                *self = Self::split(
                    if matches!(edge, Edge::Left | Edge::Right) {
                        Axis::Horizontal
                    } else {
                        Axis::Vertical
                    },
                    0.5,
                    first,
                    second,
                );
            }
            Self::Tabs { .. } => {}
            Self::Split { first, second, .. } => {
                if first.contains(target) {
                    first.attach(id, target, edge);
                } else {
                    second.attach(id, target, edge);
                }
            }
        }
    }
}

/// Height of a pane's tab bar, and all a folded pane keeps.
const TAB_BAR: f32 = 30.;

/// The dock state one window owns: layout, focus, and where it persists.
pub struct Dock<P> {
    pub layout: Layout<P>,
    /// The pane that receives keyboard and tool input.
    pub focused: P,
    pub maximized: Option<P>,
    default: Layout<P>,
    path: Option<PathBuf>,
    dirty: bool,
    tab_dragging: bool,
    bounds: BTreeMap<Vec<Branch>, Bounds<Pixels>>,
    /// Panes folded to their tab bar: the split gives their room to the
    /// neighbour until a tab is clicked. View state, not saved.
    collapsed: BTreeSet<P>,
    /// Panes a host folds and unfolds; they get a fold button when open.
    foldable: BTreeSet<P>,
}

impl<P: Copy + Ord + Serialize + DeserializeOwned> Dock<P> {
    /// An unsaved dock.
    pub fn new(default: Layout<P>) -> Self {
        Self {
            focused: default.primary(),
            layout: default.clone(),
            maximized: None,
            default,
            path: None,
            dirty: false,
            tab_dragging: false,
            bounds: BTreeMap::new(),
            collapsed: BTreeSet::new(),
            foldable: BTreeSet::new(),
        }
    }

    /// Folds `pane`'s tab group to its tab bar (or unfolds it). True when
    /// that changed. Hosts fold what the current task does not use, such
    /// as a model library while sculpting.
    pub fn set_collapsed(&mut self, pane: P, collapsed: bool) -> bool {
        self.foldable.insert(pane);
        if collapsed {
            self.collapsed.insert(pane)
        } else {
            self.collapsed.remove(&pane)
        }
    }

    /// Folds `pane`'s tab group, as its ▾ does. Focus on it moves to the
    /// first pane still showing, as a close moves it; true when it moved.
    pub fn fold(&mut self, pane: P) -> bool {
        // A maximized pane folded is restored first: folded over the whole
        // window it left it blank, with the keys on it (round 13).
        if self.maximized == Some(pane) {
            self.maximized = None;
        }
        self.set_collapsed(pane, true);
        if self.focused != pane {
            return false;
        }
        self.focused = self
            .visible_panes()
            .first()
            .copied()
            .unwrap_or_else(|| self.layout.primary());
        self.focused != pane
    }

    pub fn is_collapsed(&self, pane: P) -> bool {
        self.collapsed.contains(&pane)
    }

    /// Active tabs currently on screen, excluding hidden tabs and panes covered
    /// by maximization. Hosts use this to prioritize rendering or streaming work.
    pub fn visible_panes(&self) -> Vec<P> {
        fn collect<P: Copy>(layout: &Layout<P>, panes: &mut Vec<P>) {
            match layout {
                Layout::Tabs { active, .. } => panes.push(*active),
                Layout::Split { first, second, .. } => {
                    collect(first, panes);
                    collect(second, panes);
                }
            }
        }
        // A folded pane shows nothing, maximized or not.
        if let Some(pane) = self.maximized.filter(|p| !self.collapsed.contains(p)) {
            return vec![pane];
        }
        let mut panes = Vec::new();
        collect(&self.layout, &mut panes);
        panes.retain(|p| !self.collapsed.contains(p));
        panes
    }

    /// Loads the layout saved at `path`, falling back to `default`. A missing
    /// file is normal; a malformed one is returned so the host can report it.
    pub fn persistent(path: PathBuf, default: Layout<P>) -> (Self, Option<anyhow::Error>) {
        let mut dock = Self::new(default);
        let error = match Self::load(&path) {
            Ok(Some(layout)) => {
                dock.focused = layout.primary();
                dock.layout = layout;
                None
            }
            Ok(None) => None,
            Err(error) => Some(error.context(format!("Load view layout {}", path.display()))),
        };
        dock.path = Some(path);
        (dock, error)
    }

    fn load(path: &std::path::Path) -> Result<Option<Layout<P>>> {
        if !path.exists() {
            return Ok(None);
        }
        ensure!(
            std::fs::metadata(path)?.len() <= MAX_FILE,
            "Saved layout exceeds 32 KiB"
        );
        let layout: Layout<P> = serde_json::from_slice(&std::fs::read(path)?)?;
        layout.validate()?;
        Ok(Some(layout))
    }

    /// Writes the layout if it changed. The write is atomic, so a crash never
    /// leaves a half-written layout that would fail to load.
    pub fn save(&mut self) -> Result<()> {
        let Some(path) = self.path.as_ref().filter(|_| self.dirty) else {
            return Ok(());
        };
        let parent = path.parent().context("Layout has no parent directory")?;
        std::fs::create_dir_all(parent)?;
        let mut file = tempfile::NamedTempFile::new_in(parent)?;
        serde_json::to_writer_pretty(file.as_file_mut(), &self.layout)?;
        file.persist(path)?;
        self.dirty = false;
        Ok(())
    }

    pub fn reset(&mut self) {
        self.layout = self.default.clone();
        self.maximized = None;
        self.focused = self.layout.primary();
        self.dirty = true;
    }

    /// Shows and focuses `pane`. Returns whether focus moved.
    pub fn focus(&mut self, pane: P) -> bool {
        // Choosing a folded pane's tab unfolds it.
        self.collapsed.remove(&pane);
        if self.maximized.is_some() {
            self.maximized = Some(pane);
        }
        let prior = self.layout.clone();
        self.layout.show(pane);
        self.dirty |= prior != self.layout;
        std::mem::replace(&mut self.focused, pane) != pane
    }

    pub fn close(&mut self, pane: P) -> bool {
        if !self.layout.close(pane) {
            return false;
        }
        self.maximized = None;
        self.dirty = true;
        if self.focused == pane {
            self.focused = self.layout.primary();
        }
        true
    }

    pub fn toggle_maximized(&mut self, pane: P) {
        self.maximized = (self.maximized != Some(pane)).then_some(pane);
        self.focus(pane);
    }

    pub fn move_pane(&mut self, pane: P, target: P, edge: Edge) {
        self.layout.move_pane(pane, target, edge);
        self.dirty = true;
        self.focus(pane);
    }
}

/// Implemented by the view that owns a [`Dock`], usually the editor window.
pub trait DockHost: Sized + 'static {
    type Pane: Copy + Ord + Debug + Serialize + DeserializeOwned + 'static;

    fn dock(&mut self) -> &mut Dock<Self::Pane>;

    fn pane_label(&self, pane: Self::Pane) -> SharedString;

    /// The content below the tab strip. It fills the remaining pane area.
    fn render_pane(
        &mut self,
        pane: Self::Pane,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement;

    /// Whether the tab strip of the group showing `pane` offers maximize
    /// and close. Central views (a viewport, a page) answer no: closing one
    /// by accident leaves a hole.
    fn pane_chrome(&self, _pane: Self::Pane) -> bool {
        true
    }

    /// Whether `pane` has nothing to show yet (a library with no document
    /// open). A tab group whose panes are all idle is left out of its
    /// split until one is not, so empty chrome does not sit under an
    /// empty state.
    fn pane_idle(&self, _pane: Self::Pane) -> bool {
        false
    }

    /// Called after a click, tab drop or close moved focus to `pane`. The
    /// dock has already updated `focused`. Cancel per-pane gestures here.
    fn pane_focused(&mut self, _pane: Self::Pane, _cx: &mut Context<Self>) {}

    /// Called whenever the user picks `pane` through the dock: a click on
    /// one of its tabs (also the one already focused), maximize or restore,
    /// unfolding it, a close that leaves it focused, a tab dropped there. A
    /// host that routes keys by pane hands them over here. A press in the
    /// pane's content is the content's to handle.
    fn pane_chosen(&mut self, _pane: Self::Pane, _cx: &mut Context<Self>) {}

    /// Called after a user change worth persisting. The default saves and
    /// logs failures; override to show them in the status bar instead.
    fn layout_changed(&mut self, _cx: &mut Context<Self>) {
        if let Err(error) = self.dock().save() {
            eprintln!("Save view layout: {error:#}");
        }
    }
}

#[derive(Clone)]
struct PaneDrag<P>(P);

#[derive(Clone)]
struct ResizeDrag {
    path: Vec<Branch>,
    axis: Axis,
}

fn focus_pane<H: DockHost>(host: &mut H, pane: H::Pane, cx: &mut Context<H>) {
    if host.dock().focus(pane) {
        host.pane_focused(pane, cx);
    }
    cx.notify();
}

/// Folds `pane` as its ▾ does. The pane with the keys takes them again, or
/// the one the focus moved to when it was the folded one: a field typing
/// anywhere ends, as with any press on the dock (round 12: a folded
/// Timeline kept the keys and Delete removed a key out of sight; a value
/// being typed in the inspector went on taking digits).
pub fn fold_pane<H: DockHost>(host: &mut H, pane: H::Pane, cx: &mut Context<H>) {
    if host.dock().fold(pane) {
        let next = host.dock().focused;
        host.pane_focused(next, cx);
    }
    let focused = host.dock().focused;
    host.pane_chosen(focused, cx);
    cx.notify();
}

/// Closes `pane` as its × does. The pane with the keys takes them again,
/// or the one the focus moved to when it was the closed one: a field typing
/// anywhere ends, as with any press on the dock (round 13: closing another
/// pane left a value typing, and 77 became 774).
pub fn close_pane<H: DockHost>(host: &mut H, pane: H::Pane, cx: &mut Context<H>) {
    let was_focused = host.dock().focused == pane;
    if !host.dock().close(pane) {
        return;
    }
    let focused = host.dock().focused;
    if was_focused {
        host.pane_focused(focused, cx);
    }
    host.pane_chosen(focused, cx);
    host.layout_changed(cx);
    cx.notify();
}

/// Renders the host's dock. Call from `Render::render` and place the result
/// in a flex container that gives it the remaining window area.
pub fn render<H: DockHost>(host: &mut H, window: &mut Window, cx: &mut Context<H>) -> AnyElement {
    if !cx.has_active_drag() {
        host.dock().tab_dragging = false;
    }
    let dock = host.dock();
    let layout = match dock.maximized {
        Some(id) => Layout::tabs(vec![id], id),
        None => dock.layout.clone(),
    };
    div()
        .size_full()
        .flex()
        .min_w_0()
        .min_h_0()
        // A divider drag ends wherever the button is released; persist then
        // rather than on every move.
        .on_mouse_up(
            MouseButton::Left,
            cx.listener(|this: &mut H, _, _, cx| {
                if this.dock().dirty {
                    this.layout_changed(cx);
                }
            }),
        )
        .child(node(host, &layout, vec![], window, cx))
        .into_any_element()
}

fn node<H: DockHost>(
    host: &mut H,
    node: &Layout<H::Pane>,
    path: Vec<Branch>,
    window: &mut Window,
    cx: &mut Context<H>,
) -> AnyElement {
    match node {
        Layout::Split {
            axis,
            ratio,
            first,
            second,
        } => {
            let branch = |b| {
                let mut p = path.clone();
                p.push(b);
                p
            };
            // A folded tab group keeps only its tab bar; the other side of
            // the split takes the rest.
            let folded = |n: &Layout<H::Pane>, dock: &Dock<H::Pane>| matches!(n, Layout::Tabs { active, .. } if dock.collapsed.contains(active));
            let idle = |n: &Layout<H::Pane>, host: &H| matches!(n, Layout::Tabs { panes, .. } if panes.iter().all(|p| host.pane_idle(*p)));
            // An idle side gives the split to the other one whole.
            if idle(first, host) && !idle(second, host) {
                return self::node(host, second, branch(Branch::Second), window, cx);
            }
            if idle(second, host) && !idle(first, host) {
                return self::node(host, first, branch(Branch::First), window, cx);
            }
            let (first_folded, second_folded) =
                (folded(first, host.dock()), folded(second, host.dock()));
            let first = self::node(host, first, branch(Branch::First), window, cx);
            let second = self::node(host, second, branch(Branch::Second), window, cx);
            let (axis, ratio) = (*axis, *ratio);
            if first_folded || second_folded {
                let bar = |d: Div| match axis {
                    Axis::Vertical => d.h(px(TAB_BAR)).w_full().flex_shrink_0(),
                    Axis::Horizontal => d.w(px(TAB_BAR)).h_full().flex_shrink_0(),
                };
                let rest = |d: Div| d.flex_1().min_w_0().min_h_0();
                return div()
                    .flex()
                    .size_full()
                    .min_w_0()
                    .min_h_0()
                    .when(axis == Axis::Vertical, |d| d.flex_col())
                    .child(
                        if first_folded {
                            bar(div())
                        } else {
                            rest(div())
                        }
                        .child(first),
                    )
                    .child(
                        div()
                            .flex_shrink_0()
                            .bg(theme::get().border)
                            .when(axis == Axis::Vertical, |d| d.h(px(1.)).w_full())
                            .when(axis == Axis::Horizontal, |d| d.w(px(1.)).h_full()),
                    )
                    .child(
                        if second_folded {
                            bar(div())
                        } else {
                            rest(div())
                        }
                        .child(second),
                    )
                    .into_any_element();
            }
            let entity = cx.entity();
            let location = path.clone();
            // A hairline in a 5 px grab area: a solid 5 px bar read as a
            // second scrollbar beside the panes' own. Accent on hover.
            let group = SharedString::from(format!("dock-divider-{path:?}"));
            let divider = div()
                .id(group.clone())
                .group(group.clone())
                .flex_shrink_0()
                .flex()
                .justify_center()
                .items_center()
                .when(axis == Axis::Horizontal, |d| {
                    d.w(px(5.)).h_full().cursor_col_resize()
                })
                .when(axis == Axis::Vertical, |d| {
                    d.h(px(5.)).w_full().flex_col().cursor_row_resize()
                })
                .child(
                    div()
                        .bg(theme::get().border)
                        .group_hover(group, |d| d.bg(theme::get().accent))
                        .when(axis == Axis::Horizontal, |d| d.w(px(1.)).h_full())
                        .when(axis == Axis::Vertical, |d| d.h(px(1.)).w_full()),
                )
                .on_drag(ResizeDrag { path, axis }, |_, _, _, cx| {
                    cx.new(|_| DragLabel("Resize views".into()))
                })
                .on_drag_move(cx.listener(
                    |this: &mut H, event: &DragMoveEvent<ResizeDrag>, _, cx| {
                        let drag = event.drag(cx).clone();
                        let dock = this.dock();
                        if let Some(bounds) = dock.bounds.get(&drag.path) {
                            let ratio = match drag.axis {
                                Axis::Horizontal => {
                                    f32::from(event.event.position.x - bounds.left())
                                        / f32::from(bounds.size.width)
                                }
                                Axis::Vertical => {
                                    f32::from(event.event.position.y - bounds.top())
                                        / f32::from(bounds.size.height)
                                }
                            };
                            dock.layout.resize(&drag.path, ratio);
                            dock.dirty = true;
                            cx.notify();
                        }
                    },
                ));
            div()
                .relative()
                .flex()
                .size_full()
                .min_w_0()
                .min_h_0()
                .when(axis == Axis::Vertical, |d| d.flex_col())
                .child(
                    // Records the split's bounds so divider drags can convert
                    // cursor positions into ratios.
                    canvas(
                        move |bounds, _, cx| {
                            entity.update(cx, |this, _| {
                                this.dock().bounds.insert(location.clone(), bounds);
                            });
                        },
                        |_, _, _, _| {},
                    )
                    .absolute()
                    .size_full(),
                )
                .child(
                    div()
                        .min_w_0()
                        .min_h_0()
                        .flex_shrink_0()
                        .when(axis == Axis::Horizontal, |d| d.w(relative(ratio)).h_full())
                        .when(axis == Axis::Vertical, |d| d.h(relative(ratio)).w_full())
                        .child(first),
                )
                .child(divider)
                .child(div().flex_1().min_w_0().min_h_0().child(second))
                .into_any_element()
        }
        Layout::Tabs { panes, active } => {
            let id = *active;
            let dock = host.dock();
            let focused = dock.focused;
            let maximized = dock.maximized == Some(id);
            // A folded group shows no tab as open: muted tabs, no underline,
            // and a mark that a click unfolds it.
            let folded = dock.collapsed.contains(&id);
            let dragging = cx.has_active_drag() && dock.tab_dragging;
            let header = SharedString::from(format!("dock-header-{id:?}"));
            let mut tabs = div()
                .group(header.clone())
                .flex()
                .h(px(TAB_BAR))
                .flex_shrink_0()
                .bg(theme::get().panel)
                .border_b_1()
                .border_color(theme::get().border);
            for &pane in panes {
                let label = host.pane_label(pane);
                let entity = cx.entity();
                tabs = tabs.child(
                    div()
                        .id(SharedString::from(format!("dock-tab-{pane:?}")))
                        .flex()
                        .items_center()
                        .px_3()
                        .h_full()
                        // The shown tab is raised and underlined, brighter in
                        // the focused pane; the rest are muted text.
                        .border_b_2()
                        .border_color(if folded {
                            theme::get().panel
                        } else if focused == pane {
                            theme::get().accent
                        } else if id == pane {
                            theme::get().muted
                        } else {
                            theme::get().panel
                        })
                        .bg(if id == pane && !folded {
                            theme::get().header
                        } else {
                            theme::get().panel
                        })
                        .text_size(px(11.))
                        .text_color(if id == pane && !folded {
                            theme::get().text
                        } else {
                            theme::get().muted
                        })
                        .cursor_pointer()
                        .hover(|s| s.text_color(theme::get().text))
                        .child(label.clone())
                        .on_click(cx.listener(move |this, _, _, cx| {
                            focus_pane(this, pane, cx);
                            this.pane_chosen(pane, cx);
                        }))
                        .on_drag(PaneDrag(pane), move |_, _, _, cx| {
                            entity.update(cx, |this, _| this.dock().tab_dragging = true);
                            cx.new(|_| DragLabel(label.clone()))
                        }),
                );
            }
            let chrome = host.pane_chrome(id);
            tabs = tabs.child(div().flex_1());

            if chrome {
                // Folded to its tab bar, a group's way to grow is its fold
                // mark; maximize beside it was a third control on an idle
                // strip.
                tabs = tabs
                    .when(!folded, |t| {
                        t.child(
                            crate::widgets::icon_action(
                                SharedString::from(format!("dock-max-{id:?}")),
                                if maximized {
                                    crate::icon::Icon::RESTORE
                                } else {
                                    crate::icon::Icon::MAXIMIZE
                                },
                            )
                            .my_auto()
                            // Pane controls rest at 35%, as the Hierarchy's eyes
                            // do, full on hover of the header: hidden at rest,
                            // close was found by accident; full, a window of
                            // panes was a wall of icons.
                            .opacity(if maximized { 1. } else { 0.35 })
                            .group_hover(header.clone(), |s| s.opacity(1.))
                            .tooltip(crate::widgets::Tooltip::text(
                                if maximized { "Restore" } else { "Maximize" },
                                "",
                                "Gives this pane the whole window, or puts it back",
                            ))
                            .on_click(cx.listener(
                                move |this, _, _, cx| {
                                    this.dock().toggle_maximized(id);
                                    this.pane_focused(id, cx);
                                    this.pane_chosen(id, cx);
                                    cx.notify();
                                },
                            )),
                        )
                    })
                    // Not on a group that folds (a bottom strip): its fold
                    // mark already hides it, and close beside it removed the
                    // tab bar too, found again only under View → Panes.
                    .when(!folded && !host.dock().foldable.contains(&id), |t| {
                        t.child(
                            crate::widgets::icon_action(
                                SharedString::from(format!("dock-close-{id:?}")),
                                crate::icon::Icon::CLOSE,
                            )
                            .my_auto()
                            .mr_1()
                            .opacity(0.35)
                            .group_hover(header.clone(), |s| s.opacity(1.))
                            .tooltip(crate::widgets::Tooltip::text(
                                "Close pane",
                                "",
                                "The View menu brings it back",
                            ))
                            .on_click(cx.listener(
                                move |this, _, _, cx| {
                                    close_pane(this, id, cx);
                                },
                            )),
                        )
                    });
            }
            // At the bar's right end, after the pane icons. Folded panes offer to open; open ones that a host may fold
            // offer to fold, so the state can be changed both ways.
            if folded || host.dock().foldable.contains(&id) {
                tabs = tabs.child(
                    div()
                        .id(SharedString::from(format!("dock-fold-{id:?}")))
                        .mx_1()
                        .my_auto()
                        .size(px(22.))
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded(px(3.))
                        .cursor_pointer()
                        .text_size(px(11.))
                        .text_color(theme::get().muted)
                        .hover(|s| s.bg(theme::get().hover).text_color(theme::get().text))
                        .child(if folded { "▴" } else { "▾" })
                        .tooltip(crate::widgets::Tooltip::text(
                            if folded { "Open" } else { "Fold" },
                            "",
                            if folded {
                                "Unfold this pane; its tabs open it too"
                            } else {
                                "Fold this pane to its tabs"
                            },
                        ))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if this.dock().is_collapsed(id) {
                                focus_pane(this, id, cx);
                                this.pane_chosen(id, cx);
                            } else {
                                fold_pane(this, id, cx);
                            }
                        })),
                );
            }
            // Folded: the tab bar alone, nothing rendered under it.
            if host.dock().collapsed.contains(&id) {
                return div()
                    .id(SharedString::from(format!("dock-pane-{id:?}")))
                    .size_full()
                    .child(tabs)
                    .into_any_element();
            }
            let content = host.render_pane(id, window, cx);
            let mut pane = div()
                .id(SharedString::from(format!("dock-pane-{id:?}")))
                .relative()
                .size_full()
                .min_w_0()
                .min_h_0()
                .flex()
                .flex_col()
                .child(tabs)
                .child(
                    div()
                        .id(SharedString::from(format!("dock-content-{id:?}")))
                        .relative()
                        .flex_1()
                        .min_h_0()
                        .min_w_0()
                        .overflow_hidden()
                        // Capture runs before the content's own handlers, so a
                        // viewport sees itself focused on the same click.
                        .capture_any_mouse_down(
                            cx.listener(move |this, _, _, cx| focus_pane(this, id, cx)),
                        )
                        .child(content),
                );
            if dragging {
                let zone = |edge, label| drop_zone::<H>(id, edge, label, cx);
                pane = pane.child(
                    div()
                        .absolute()
                        .inset_0()
                        .flex()
                        .flex_col()
                        .p_3()
                        .gap_2()
                        .bg(theme::get().scrim.alpha(0.53))
                        .child(zone(Edge::Top, "Dock above").h(px(45.)))
                        .child(
                            div()
                                .flex()
                                .flex_1()
                                .gap_2()
                                .child(zone(Edge::Left, "Dock left").flex_1())
                                .child(zone(Edge::Center, "Group tabs").flex_1())
                                .child(zone(Edge::Right, "Dock right").flex_1()),
                        )
                        .child(zone(Edge::Bottom, "Dock below").h(px(45.))),
                );
            }
            pane.into_any_element()
        }
    }
}

fn drop_zone<H: DockHost>(
    target: H::Pane,
    edge: Edge,
    label: &'static str,
    cx: &Context<H>,
) -> Stateful<Div> {
    div()
        .id(SharedString::from(format!("dock-drop-{target:?}-{edge:?}")))
        .occlude()
        .flex()
        .items_center()
        .justify_center()
        .bg(theme::get().selected.alpha(0.67))
        .border_1()
        .border_color(theme::get().accent)
        .text_size(px(11.))
        .child(label)
        .drag_over::<PaneDrag<H::Pane>>(|d, _, _, _| d.bg(theme::get().selected))
        .on_drop(cx.listener(move |this, drag: &PaneDrag<H::Pane>, _, cx| {
            this.dock().move_pane(drag.0, target, edge);
            this.pane_focused(drag.0, cx);
            this.pane_chosen(drag.0, cx);
            this.layout_changed(cx);
            cx.notify();
        }))
}

#[cfg(test)]
mod tests {
    #[::core::prelude::v1::test]
    fn existing_view_layouts_keep_their_tabs_and_focus() {
        let layout: Layout<u8> =
            serde_json::from_str(r#"{"Tabs":{"views":[1,2],"active":2}}"#).unwrap();
        layout.validate().unwrap();
        assert_eq!(layout.primary(), 2);
        assert!(layout.contains(1));
        let reloaded: Layout<u8> =
            serde_json::from_slice(&serde_json::to_vec(&layout).unwrap()).unwrap();
        assert_eq!(layout, reloaded);
    }

    use super::{Axis, Dock, Edge, Layout, MAX_DEPTH};
    use serde::{Deserialize, Serialize};

    #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
    enum Pane {
        Scene,
        Map,
        Graph,
    }

    fn layout() -> Layout<Pane> {
        Layout::split(
            Axis::Vertical,
            0.72,
            Layout::tabs(vec![Pane::Scene], Pane::Scene),
            Layout::tabs(vec![Pane::Map, Pane::Graph], Pane::Graph),
        )
    }

    #[test]
    fn a_folded_pane_is_not_visible_until_its_tab_is_chosen() {
        let mut dock = Dock::new(layout());
        assert!(dock.set_collapsed(Pane::Graph, true));
        assert!(!dock.set_collapsed(Pane::Graph, true));
        assert_eq!(dock.visible_panes(), vec![Pane::Scene]);
        dock.focus(Pane::Graph);
        assert!(!dock.is_collapsed(Pane::Graph));
        assert_eq!(dock.visible_panes(), vec![Pane::Scene, Pane::Graph]);
    }

    #[test]
    fn visible_panes_excludes_hidden_tabs_and_respects_maximization() {
        let mut dock = Dock::new(layout());
        dock.focus(Pane::Map);
        assert_eq!(dock.visible_panes(), [Pane::Scene, Pane::Map]);
        dock.focus(Pane::Graph);
        assert_eq!(dock.visible_panes(), [Pane::Scene, Pane::Graph]);
        dock.maximized = Some(Pane::Graph);
        assert_eq!(dock.visible_panes(), [Pane::Graph]);
        dock.maximized = None;
        dock.close(Pane::Graph);
        assert_eq!(dock.visible_panes(), [Pane::Scene, Pane::Map]);
    }

    #[test]
    fn panes_move_between_tabs_and_splits_without_duplicates() {
        let mut layout = layout();
        layout.move_pane(Pane::Graph, Pane::Scene, Edge::Right);
        layout.validate().unwrap();
        assert!(
            [Pane::Scene, Pane::Map, Pane::Graph]
                .iter()
                .all(|p| layout.contains(*p))
        );
        layout.move_pane(Pane::Map, Pane::Graph, Edge::Center);
        layout.validate().unwrap();
        assert!(layout.activate(Pane::Map));
        let restored: Layout<Pane> =
            serde_json::from_slice(&serde_json::to_vec(&layout).unwrap()).unwrap();
        assert_eq!(layout, restored);
    }

    #[test]
    fn closing_collapses_empty_groups_and_keeps_the_last_pane() {
        let mut layout = layout();
        assert!(layout.close(Pane::Map));
        assert!(layout.close(Pane::Graph));
        assert_eq!(layout, Layout::tabs(vec![Pane::Scene], Pane::Scene));
        assert!(!layout.close(Pane::Scene));
        layout.show(Pane::Map);
        layout.validate().unwrap();
    }

    #[test]
    fn malformed_saved_layouts_are_rejected() {
        let duplicate = Layout::Tabs {
            panes: vec![Pane::Map, Pane::Map],
            active: Pane::Map,
        };
        assert!(duplicate.validate().is_err());
        let empty = Layout::<Pane>::Tabs {
            panes: vec![],
            active: Pane::Scene,
        };
        assert!(empty.validate().is_err());
        let mut deep = Layout::tabs(vec![Pane::Scene], Pane::Scene);
        for _ in 0..MAX_DEPTH {
            deep = Layout::Split {
                axis: Axis::Vertical,
                ratio: 0.5,
                first: Box::new(deep),
                second: Box::new(Layout::tabs(vec![Pane::Map], Pane::Map)),
            };
        }
        assert!(deep.validate().is_err());
    }

    #[test]
    fn dock_persists_atomically_and_reports_bad_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested/layout.json");
        let (mut dock, error) = Dock::persistent(path.clone(), layout());
        assert!(error.is_none());
        dock.close(Pane::Map);
        assert!(dock.focus(Pane::Graph));
        dock.save().unwrap();
        let (loaded, error) = Dock::<Pane>::persistent(path.clone(), layout());
        assert!(error.is_none());
        assert_eq!(loaded.layout, dock.layout);
        std::fs::write(&path, b"{\"Tabs\":{\"panes\":[],\"active\":\"Map\"}}").unwrap();
        let (fallback, error) = Dock::<Pane>::persistent(path, layout());
        assert!(error.is_some());
        assert_eq!(fallback.layout, layout());
    }

    #[test]
    fn focusing_a_closed_pane_reopens_it_and_close_moves_focus() {
        let mut dock = Dock::new(layout());
        assert_eq!(dock.focused, Pane::Scene);
        dock.close(Pane::Graph);
        assert!(dock.focus(Pane::Graph));
        assert!(dock.layout.contains(Pane::Graph));
        assert!(dock.close(Pane::Graph));
        assert_eq!(dock.focused, Pane::Scene);
    }
}
