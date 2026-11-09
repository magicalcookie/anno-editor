// Tree views over host-owned data: which nodes are open, flattening to the
// visible rows a `uniform_list` draws, revealing a node, arrow-key movement,
// and the virtualized list itself.
//
// The host keeps its own tree and its own row type. Rebuild the rows after
// anything that changes them (data, filter, expansion); flattening only walks
// open nodes, so it stays cheap for large trees with few open branches.
use crate::widgets::{Disclosure, disclosure_arrow, tree_row_with};
use gpui::{prelude::*, *};
use std::cell::Cell;
use std::collections::BTreeSet;
use std::rc::Rc;
use std::sync::Arc;

/// Expansion state of one tree view. `K` identifies an expandable node.
///
/// It is `Send`, so documents built on a worker can own one. The list's
/// scroll handle is GPUI element state (see [`list`]); scroll requests wait
/// here until the next render applies them.
#[derive(Clone)]
pub struct Tree<K> {
    expanded: BTreeSet<K>,
    scroll: Cell<Option<usize>>,
}

impl<K> Default for Tree<K> {
    fn default() -> Self {
        Self {
            expanded: BTreeSet::new(),
            scroll: Cell::new(None),
        }
    }
}

/// One visible row after flattening.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Row<N> {
    pub node: N,
    pub depth: usize,
    pub disclosure: Disclosure,
}

/// What an arrow key did, so the host knows what to refresh.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Nav {
    /// The selection moved to this row; it is scrolled into view.
    Select(usize),
    /// A node opened or closed; rebuild the rows.
    Relayout,
}

impl<K: Copy + Ord> Tree<K> {
    pub fn is_expanded(&self, key: K) -> bool {
        self.expanded.contains(&key)
    }

    pub fn expand(&mut self, key: K) -> bool {
        self.expanded.insert(key)
    }

    pub fn collapse(&mut self, key: K) -> bool {
        self.expanded.remove(&key)
    }

    pub fn toggle(&mut self, key: K) {
        if !self.expanded.remove(&key) {
            self.expanded.insert(key);
        }
    }

    pub fn collapse_all(&mut self) {
        self.expanded.clear();
    }

    pub fn any_expanded(&self) -> bool {
        !self.expanded.is_empty()
    }

    /// Opens every ancestor of a node, root first or last, so it becomes visible.
    pub fn reveal(&mut self, ancestors: impl IntoIterator<Item = K>) {
        self.expanded.extend(ancestors);
    }

    /// Centers `row` on the next render.
    pub fn scroll_to(&self, row: usize) {
        self.scroll.set(Some(row));
    }

    /// Appends `roots` and, under each open node, its children, depth first.
    /// `key` returns the expansion key of nodes that can open; others are
    /// leaves. `children` is only called for open nodes.
    pub fn flatten_into<N, I>(
        &self,
        out: &mut Vec<Row<N>>,
        depth: usize,
        roots: impl IntoIterator<Item = N>,
        key: &impl Fn(&N) -> Option<K>,
        children: &impl Fn(&N) -> I,
    ) where
        I: IntoIterator<Item = N>,
    {
        for node in roots {
            let open = key(&node).map(|k| self.is_expanded(k));
            let disclosure = match open {
                None => Disclosure::Leaf,
                Some(false) => Disclosure::Collapsed,
                Some(true) => Disclosure::Expanded,
            };
            let below = (open == Some(true)).then(|| children(&node));
            out.push(Row {
                node,
                depth,
                disclosure,
            });
            if let Some(below) = below {
                self.flatten_into(out, depth + 1, below, key, children);
            }
        }
    }

    pub fn flatten<N, I>(
        &self,
        roots: impl IntoIterator<Item = N>,
        key: impl Fn(&N) -> Option<K>,
        children: impl Fn(&N) -> I,
    ) -> Vec<Row<N>>
    where
        I: IntoIterator<Item = N>,
    {
        let mut rows = Vec::new();
        self.flatten_into(&mut rows, 0, roots, &key, &children);
        rows
    }

    /// Rows for a search: every node that `matches`, under its ancestors,
    /// whatever is expanded. Walks the whole tree, so hosts call it when the
    /// query changes, not every frame. Stops adding matches after `limit`.
    /// Ancestors show as expanded; matches with no matching descendants show
    /// as leaves.
    pub fn flatten_matching<N, I>(
        roots: impl IntoIterator<Item = N>,
        children: impl Fn(&N) -> I,
        matches: impl Fn(&N) -> bool,
        limit: usize,
    ) -> Vec<Row<N>>
    where
        I: IntoIterator<Item = N>,
    {
        fn walk<N, I>(
            out: &mut Vec<Row<N>>,
            found: &mut usize,
            depth: usize,
            node: N,
            children: &impl Fn(&N) -> I,
            matches: &impl Fn(&N) -> bool,
            limit: usize,
        ) -> bool
        where
            I: IntoIterator<Item = N>,
        {
            let hit = *found < limit && matches(&node);
            if hit {
                *found += 1;
            }
            let at = out.len();
            let below = children(&node);
            out.push(Row {
                node,
                depth,
                disclosure: Disclosure::Leaf,
            });
            let mut kept = false;
            for child in below {
                if *found >= limit && !kept && !hit {
                    break;
                }
                kept |= walk(out, found, depth + 1, child, children, matches, limit);
            }
            if kept {
                out[at].disclosure = Disclosure::Expanded;
            } else if !hit {
                out.truncate(at);
            }
            hit || kept
        }
        let mut out = Vec::new();
        let mut found = 0;
        for root in roots {
            if found >= limit {
                break;
            }
            walk(&mut out, &mut found, 0, root, &children, &matches, limit);
        }
        out
    }

    /// Arrow keys over the flattened `rows` with `selected` the current row:
    /// up and down move, home and end jump, right opens a closed node or
    /// steps into an open one, left closes an open node or steps to its
    /// parent. `None` when the key is not a tree key or nothing changes.
    pub fn navigate<N>(
        &mut self,
        event: &KeyDownEvent,
        rows: &[Row<N>],
        selected: Option<usize>,
        key: impl Fn(&N) -> Option<K>,
    ) -> Option<Nav> {
        let m = event.keystroke.modifiers;
        if m.control || m.alt || m.platform || rows.is_empty() {
            return None;
        }
        let last = rows.len() - 1;
        let target = match (event.keystroke.key.as_str(), selected) {
            ("down", None) | ("home", _) => 0,
            ("up", None) | ("end", _) => last,
            ("down", Some(i)) => (i + 1).min(last),
            ("up", Some(i)) => i.saturating_sub(1),
            ("right", Some(i)) => {
                let row = rows.get(i)?;
                match row.disclosure {
                    Disclosure::Collapsed => {
                        self.expand(key(&row.node)?);
                        return Some(Nav::Relayout);
                    }
                    Disclosure::Expanded
                        if rows.get(i + 1).is_some_and(|r| r.depth > row.depth) =>
                    {
                        i + 1
                    }
                    _ => return None,
                }
            }
            ("left", Some(i)) => {
                let row = rows.get(i)?;
                if row.disclosure == Disclosure::Expanded {
                    self.collapse(key(&row.node)?);
                    return Some(Nav::Relayout);
                }
                rows[..i].iter().rposition(|r| r.depth < row.depth)?
            }
            _ => return None,
        };
        if selected == Some(target) {
            return None;
        }
        self.scroll_to(target);
        Some(Nav::Select(target))
    }
}

/// What a row shows. Built lazily per visible row, so labels for hundreds of
/// thousands of rows are never formatted at once.
pub struct Item {
    pub depth: usize,
    pub disclosure: Disclosure,
    pub label: SharedString,
    /// Right-aligned and muted; tabs split it into aligned columns.
    pub detail: SharedString,
    pub selected: bool,
}

/// A virtualized list of `tree_row`s. `item` describes row `i`; `on_click`
/// gets the row index and the click (check `click_count()` for double-clicks).
/// `id` must be unique in the window: it also keys the scroll position.
/// Clicking a disclosure arrow also goes to `on_click`; use
/// [`list_with_toggle`] when rows are selectable and expandable at once, or
/// [`List`] for more.
pub fn list<V: 'static, K>(
    id: &'static str,
    count: usize,
    tree: &Tree<K>,
    window: &mut Window,
    cx: &mut Context<V>,
    item: impl Fn(usize) -> Item + 'static,
    on_click: impl Fn(&mut V, usize, &ClickEvent, &mut Context<V>) + 'static,
) -> Stateful<Div> {
    List::new(id, count, item, on_click).render(tree, window, cx)
}

/// [`list`] where clicking a row's disclosure arrow calls `on_toggle` instead
/// of `on_click`, for trees whose expandable nodes are also selectable (a
/// scene object with child objects).
#[allow(clippy::too_many_arguments)]
pub fn list_with_toggle<V: 'static, K>(
    id: &'static str,
    count: usize,
    tree: &Tree<K>,
    window: &mut Window,
    cx: &mut Context<V>,
    item: impl Fn(usize) -> Item + 'static,
    on_click: impl Fn(&mut V, usize, &ClickEvent, &mut Context<V>) + 'static,
    on_toggle: impl Fn(&mut V, usize, &mut Context<V>) + 'static,
) -> Stateful<Div> {
    List::new(id, count, item, on_click)
        .on_toggle(on_toggle)
        .render(tree, window, cx)
}

type Toggle<V> = dyn Fn(&mut V, usize, &mut Context<V>);
type Click<V> = dyn Fn(&mut V, usize, &ClickEvent, &mut Context<V>);
type Decorate = dyn Fn(usize, Stateful<Div>) -> Stateful<Div>;
type RowIcon = dyn Fn(usize) -> Option<Arc<RenderImage>>;
type RowGlyph = dyn Fn(usize) -> Option<crate::icon::Icon>;
type CellClick<V> = dyn Fn(&mut V, usize, usize, &mut Context<V>);

/// The tree list with every option: a separate disclosure toggle and a hook
/// that decorates each row element (drag sources, tooltips, context menus).
pub struct List<V> {
    id: &'static str,
    count: usize,
    item: Rc<dyn Fn(usize) -> Item>,
    on_click: Rc<Click<V>>,
    on_toggle: Option<Rc<Toggle<V>>>,
    decorate: Option<Rc<Decorate>>,
    icon: Option<Rc<RowIcon>>,
    glyph: Option<Rc<RowGlyph>>,
    heading: Option<Rc<dyn Fn(usize) -> bool>>,
    sticky: Option<Rc<dyn Fn(usize) -> Vec<usize>>>,
    note: Option<Rc<dyn Fn(usize) -> Option<SharedString>>>,
    cell_click: Option<Rc<CellClick<V>>>,
    columns: Option<Rc<[f32]>>,
    crumb: Option<Rc<dyn Fn(usize) -> Option<SharedString>>>,
    icon_bg: Option<Rgba>,
    /// Per row, overriding `icon_bg`: a picture's own backdrop.
    icon_bg_for: Option<Rc<dyn Fn(usize) -> Option<Rgba>>>,
    banded: bool,
    guides: bool,
    note_share: f32,
    row: f32,
    query: SharedString,
}

impl<V: 'static> List<V> {
    pub fn new(
        id: &'static str,
        count: usize,
        item: impl Fn(usize) -> Item + 'static,
        on_click: impl Fn(&mut V, usize, &ClickEvent, &mut Context<V>) + 'static,
    ) -> Self {
        Self {
            id,
            count,
            item: Rc::new(item),
            on_click: Rc::new(on_click),
            on_toggle: None,
            decorate: None,
            icon: None,
            glyph: None,
            heading: None,
            sticky: None,
            note: None,
            cell_click: None,
            note_share: 0.34,
            columns: None,
            crumb: None,
            icon_bg: None,
            icon_bg_for: None,
            banded: false,
            guides: false,
            row: crate::widgets::TREE_ROW,
            query: SharedString::default(),
        }
    }

    /// The search the rows were filtered by, marked in each label.
    pub fn highlight(mut self, query: impl Into<SharedString>) -> Self {
        self.query = query.into();
        self
    }

    /// Taller rows, for pictures that must be told apart (island
    /// outlines); every row takes this height, headers included.
    pub fn row_height(mut self, height: f32) -> Self {
        self.row = height;
        self
    }

    /// Rows drawn as muted captions inside the list (size groups under a
    /// region), not as entries.
    pub fn heading(mut self, f: impl Fn(usize) -> bool + 'static) -> Self {
        self.heading = Some(Rc::new(f));
        self
    }

    /// Rows held at the top while the rows under them scroll past, as an
    /// outliner keeps a group's name over its instances. Called with the
    /// first visible row; returns the rows to pin (its group, a caption),
    /// in order. Rows already in view at the top are not pinned twice.
    pub fn sticky(mut self, f: impl Fn(usize) -> Vec<usize> + 'static) -> Self {
        self.sticky = Some(Rc::new(f));
        self
    }

    /// Muted text after a row's label, cut from the start when short of
    /// room: a flat file list's folders, after the names that tell rows
    /// apart.
    pub fn note(mut self, f: impl Fn(usize) -> Option<SharedString> + 'static) -> Self {
        self.note = Some(Rc::new(f));
        self
    }

    /// The notes' share of the row width (0.34 by default), for a host that
    /// lets the column be dragged wider.
    pub fn note_share(mut self, share: f32) -> Self {
        self.note_share = share.clamp(0.1, 0.8);
        self
    }

    /// A muted line over the pinned rows, given the first pinned row: the
    /// ancestors that did not fit as rows ("data › ui ›"), so the band
    /// does not start mid-indent with nothing above it.
    pub fn sticky_crumb(mut self, f: impl Fn(usize) -> Option<SharedString> + 'static) -> Self {
        self.crumb = Some(Rc::new(f));
        self
    }

    /// Every other row a shade lighter, for a wide list whose details sit
    /// far right of short names: the band carries the eye across.
    pub fn banded(mut self) -> Self {
        self.banded = true;
        self
    }

    /// A faint vertical line under each ancestor's arrow, for deep trees:
    /// at 14 px a step, a row one level up after a long deep block read as
    /// one of its children.
    pub fn guides(mut self) -> Self {
        self.guides = true;
        self
    }

    /// The colour behind row pictures, for pictures made to sit on one
    /// (island outlines on sea blue); else a neutral grey.
    pub fn icon_backdrop(mut self, color: Rgba) -> Self {
        self.icon_bg = Some(color);
        self
    }

    /// A row's own picture backdrop where `f` gives one (dark artwork on
    /// transparency wants a light one), else `icon_backdrop`'s.
    pub fn icon_backdrop_for(mut self, f: impl Fn(usize) -> Option<Rgba> + 'static) -> Self {
        self.icon_bg_for = Some(Rc::new(f));
        self
    }

    /// Widths of the tab-separated detail columns, for columns wider than
    /// a coordinate ("512² · BC7" beside a file size).
    pub fn detail_columns(mut self, widths: &[f32]) -> Self {
        self.columns = Some(widths.into());
        self
    }

    /// Clicks on the tab-separated columns of heading rows, with the row and
    /// column index: a caption row's columns as sorts. Other rows' columns
    /// stay plain text.
    pub fn on_cell_click(
        mut self,
        f: impl Fn(&mut V, usize, usize, &mut Context<V>) + 'static,
    ) -> Self {
        self.cell_click = Some(Rc::new(f));
        self
    }

    /// A drawn icon before each row's label: a file type, an object kind.
    pub fn glyph(mut self, f: impl Fn(usize) -> Option<crate::icon::Icon> + 'static) -> Self {
        self.glyph = Some(Rc::new(f));
        self
    }

    /// A small picture before each row's label (an island's outline, a
    /// file type); rows where it returns `None` keep the label in place.
    pub fn icon(mut self, f: impl Fn(usize) -> Option<Arc<RenderImage>> + 'static) -> Self {
        self.icon = Some(Rc::new(f));
        self
    }

    /// Disclosure arrows call this instead of `on_click`.
    pub fn on_toggle(mut self, f: impl Fn(&mut V, usize, &mut Context<V>) + 'static) -> Self {
        self.on_toggle = Some(Rc::new(f));
        self
    }

    /// Runs on each visible row element, after the list's own click handler,
    /// for example `|i, row| row.on_drag(payload(i), preview)`.
    pub fn decorate(mut self, f: impl Fn(usize, Stateful<Div>) -> Stateful<Div> + 'static) -> Self {
        self.decorate = Some(Rc::new(f));
        self
    }

    pub fn render<K>(
        self,
        tree: &Tree<K>,
        window: &mut Window,
        cx: &mut Context<V>,
    ) -> Stateful<Div> {
        let Self {
            id,
            count,
            item,
            on_click,
            on_toggle,
            decorate,
            icon,
            glyph,
            heading,
            sticky,
            note,
            cell_click,
            note_share,
            columns,
            crumb,
            icon_bg,
            icon_bg_for,
            banded,
            guides,
            row,
            query,
        } = self;
        let scroll = window
            .use_keyed_state(id, cx, |_, _| UniformListScrollHandle::new())
            .read(cx)
            .clone();
        if let Some(target) = tree.scroll.take() {
            if sticky.is_some() {
                // Top-aligned a few rows above the target, so the offset is
                // a whole number of rows: centred, it fell between rows and
                // the row under the pinned band showed only its lower half.
                // About 40% down the list as last laid out, so a jump lands
                // in one place with neighbours on both sides; at least five
                // rows, clear of the deepest pinned band (three) with room.
                let shown = f32::from(scroll.0.borrow().base_handle.bounds().size.height) / row;
                let above = ((shown * 0.4) as usize).max(5);
                scroll.scroll_to_item(target.saturating_sub(above), ScrollStrategy::Top);
            } else {
                scroll.scroll_to_item(target, ScrollStrategy::Center);
            }
        }
        let entity = cx.entity().downgrade();
        let base = scroll.0.borrow().base_handle.clone();
        // One row element, for the list and for the rows pinned over it.
        let item_depth = item.clone();
        let build = Rc::new(move |i: usize| -> AnyElement {
            let it = item(i);
            let arrow = disclosure_arrow(it.disclosure);
            let arrow = match (&on_toggle, it.disclosure) {
                (Some(toggle), Disclosure::Collapsed | Disclosure::Expanded) => {
                    let (entity, toggle) = (entity.clone(), toggle.clone());
                    arrow
                        .id(("disclosure", i))
                        .on_click(move |_, _, cx| {
                            cx.stop_propagation();
                            let _ = entity.update(cx, |this, cx| {
                                toggle(this, i, cx);
                                cx.notify();
                            });
                        })
                        .into_any_element()
                }
                _ => arrow.into_any_element(),
            };
            let (entity, on_click) = (entity.clone(), on_click.clone());
            let picture = icon.as_ref().and_then(|f| f(i));
            let mark = glyph.as_ref().and_then(|f| f(i));
            let is_heading = heading.as_ref().is_some_and(|f| f(i));
            let cells = cell_click.as_ref().filter(|_| is_heading).map(|f| {
                let (f, entity) = (f.clone(), entity.clone());
                Rc::new(move |col: usize, cx: &mut App| {
                    let _ = entity.update(cx, |this, cx| {
                        f(this, i, col, cx);
                        cx.notify();
                    });
                }) as Rc<dyn Fn(usize, &mut App)>
            });
            let (selected, depth) = (it.selected, it.depth);
            let row = tree_row_with(
                (id, i),
                it.depth,
                arrow,
                picture,
                mark,
                it.label,
                it.detail,
                it.selected,
                row,
                is_heading,
                note.as_ref().and_then(|f| f(i)).map(|n| (n, note_share)),
                cells,
                columns.clone(),
                icon_bg_for.as_ref().and_then(|f| f(i)).or(icon_bg),
                &query,
            )
            .on_click(move |event, _, cx| {
                let _ = entity.update(cx, |this, cx| {
                    on_click(this, i, event, cx);
                    cx.notify();
                });
            });
            // Under each ancestor's arrow (10 px wide at the indent of its
            // depth): rows of one block draw the same x, so the lines join
            // down the block and stop where the depth steps back.
            let row = if guides {
                row.children((0..depth).map(|d| {
                    div()
                        .absolute()
                        .top_0()
                        .bottom_0()
                        .left(px(6. + d as f32 * 14. + 4.5))
                        .w(px(1.))
                        .bg(crate::theme::get().border.alpha(0.625))
                }))
            } else {
                row
            };
            let row = if banded && i % 2 == 1 && !selected {
                row.bg(crate::theme::get().band)
            } else {
                row
            };
            match &decorate {
                Some(decorate) => decorate(i, row),
                None => row,
            }
            .into_any_element()
        });
        let rows = build.clone();
        let list = uniform_list(id, count, move |range, _, _| {
            range.map(|i| rows(i)).collect()
        })
        .track_scroll(&scroll)
        .absolute()
        .inset_0();
        // The list fills a box the host sizes; the scrollbar sits over its
        // right edge.
        // The first row under the top edge, from the offset this frame
        // scrolls to; pinned rows cover the ones they replace.
        let pinned = sticky.and_then(|f| {
            let top = (-f32::from(base.offset().y) / row).max(0.) as usize;
            let pins = f(top.min(count.saturating_sub(1)));
            let pins: Vec<usize> = pins
                .into_iter()
                .enumerate()
                .filter(|&(k, r)| r < top + k)
                .map(|(_, r)| r)
                .collect();
            (!pins.is_empty()).then(|| {
                div()
                    .absolute()
                    .top_0()
                    .left_0()
                    .right_0()
                    // A step lighter than the rows, so pinned rows read as
                    // a header band, not as rows that stopped scrolling.
                    .bg(crate::theme::get().secondary)
                    // The floating border's stronger grey: in the rows'
                    // own rule colour the pinned header read as one more
                    // row with the next one flush under it.
                    .border_b_1()
                    .border_color(crate::theme::get().floating_border)
                    // With ancestors cut off, the first pinned row becomes
                    // their path ending in its own name ("data › ui ›
                    // 2kimages"), in its place: a row of its own above the
                    // pins made the block one row taller than the rows it
                    // replaced, and the row under it showed only its lower
                    // half.
                    .children({
                        let first = pins[0];
                        match crumb.as_ref().and_then(|f| f(first)) {
                            Some(c) => {
                                let it = item_depth(first);
                                let row_el = div()
                                    .h(px(row))
                                    .pl(px(6.))
                                    .flex()
                                    .items_center()
                                    .gap_1()
                                    .text_size(px(11.))
                                    .overflow_hidden()
                                    .whitespace_nowrap()
                                    .child(
                                        div()
                                            .text_color(crate::theme::get().muted)
                                            .child(format!("{c} ›")),
                                    )
                                    .pr(px(16.))
                                    .child(
                                        div()
                                            .flex_1()
                                            .min_w_0()
                                            .truncate()
                                            .text_color(crate::theme::get().text)
                                            .child(it.label),
                                    )
                                    // Its count too, so the pinned rows read as
                                    // one table.
                                    .child(
                                        div()
                                            .flex_shrink_0()
                                            .text_color(crate::theme::get().muted)
                                            .child(it.detail),
                                    );
                                std::iter::once(row_el.into_any_element())
                                    .chain(pins[1..].iter().map(|&r| build(r)))
                                    .collect::<Vec<_>>()
                            }
                            None => pins.iter().map(|&r| build(r)).collect(),
                        }
                    })
                    // A 6 px fade under it, so a row half under the pins
                    // reads as scrolling beneath, not as cut; a box shadow
                    // was too faint on the panel to see.
                    .child(
                        div()
                            .absolute()
                            .left_0()
                            .right_0()
                            .bottom(px(-7.))
                            .h(px(6.))
                            .bg(linear_gradient(
                                180.,
                                linear_color_stop(crate::theme::get().shadow, 0.),
                                linear_color_stop(crate::theme::get().shadow.alpha(0.), 1.),
                            )),
                    )
            })
        });
        div()
            .id(SharedString::from(format!("{id}-area")))
            .relative()
            .overflow_hidden()
            .child(list)
            .children(pinned)
            .child(crate::widgets::scrollbar(id, &base))
    }
}

/// One tile of a [`Grid`]: a picture over a name, with a muted second line.
pub struct Tile {
    pub picture: Option<Arc<RenderImage>>,
    pub label: SharedString,
    pub detail: SharedString,
    pub selected: bool,
    /// On hover: what the cut label and detail leave out (a whole path).
    pub tip: Option<SharedString>,
    /// Behind this picture instead of the grid's backdrop: a light one
    /// for dark artwork on transparency, say.
    pub backdrop: Option<Rgba>,
}

/// A virtualized grid of picture tiles, for browsing things by look (a
/// game's textures): rows of tiles in a `uniform_list`, as many columns as
/// the width it was last drawn at holds. A change of width that changes the
/// column count redraws the view that drew it.
pub struct Grid<V> {
    id: &'static str,
    count: usize,
    item: Rc<dyn Fn(usize) -> Tile>,
    on_click: Rc<Click<V>>,
    tile: f32,
    on_resize: Option<Rc<dyn Fn(&mut V, &mut Context<V>)>>,
    width: Option<Rc<Cell<f32>>>,
    backdrop: Rgba,
    query: SharedString,
}

/// The width a grid was last laid out at; element state, keyed by its id.
struct GridWidth(Rc<Cell<f32>>);

impl<V: 'static> Grid<V> {
    pub fn new(
        id: &'static str,
        count: usize,
        item: impl Fn(usize) -> Tile + 'static,
        on_click: impl Fn(&mut V, usize, &ClickEvent, &mut Context<V>) + 'static,
    ) -> Self {
        Self {
            id,
            count,
            item: Rc::new(item),
            on_click: Rc::new(on_click),
            tile: 104.,
            on_resize: None,
            width: None,
            backdrop: crate::theme::get().picture_bg,
            query: SharedString::default(),
        }
    }

    /// A width the host already measures (its pane's), so the first frame
    /// after switching to the grid lays out at the right width instead of
    /// a guess corrected a frame later.
    pub fn width(mut self, width: Rc<Cell<f32>>) -> Self {
        self.width = Some(width);
        self
    }

    /// Called when a new width changes the column count, for a host whose
    /// pane is a cached view the grid's own notify does not reach: it
    /// should redraw that pane.
    pub fn on_resize(mut self, f: impl Fn(&mut V, &mut Context<V>) + 'static) -> Self {
        self.on_resize = Some(Rc::new(f));
        self
    }

    /// Behind each picture: the host's backdrop choice, so
    /// transparent and letterboxed pictures read against it.
    pub fn backdrop(mut self, color: Rgba) -> Self {
        self.backdrop = color;
        self
    }

    /// The search the tiles were filtered by, marked in each name.
    pub fn highlight(mut self, query: impl Into<SharedString>) -> Self {
        self.query = query.into();
        self
    }

    /// Tile width in logical px; the picture is square inside it.
    pub fn tile_size(mut self, tile: f32) -> Self {
        self.tile = tile;
        self
    }

    pub fn render(self, window: &mut Window, cx: &mut Context<V>) -> Stateful<Div> {
        let Self {
            id,
            count,
            item,
            on_click,
            tile,
            on_resize,
            width: shared_width,
            backdrop,
            query,
        } = self;
        const GAP: f32 = 6.;
        const PAD: f32 = 8.;
        let scroll = window
            .use_keyed_state(id, cx, |_, _| UniformListScrollHandle::new())
            .read(cx)
            .clone();
        let width = match shared_width {
            Some(w) => w,
            None => window
                .use_keyed_state(SharedString::from(format!("{id}-width")), cx, |_, _| {
                    GridWidth(Rc::new(Cell::new(0.)))
                })
                .read(cx)
                .0
                .clone(),
        };
        // Columns for the last width; before the first layout, a guess the
        // first frame corrects.
        let columns_for = move |w: f32| {
            if w <= 0. {
                4
            } else {
                (((w - PAD * 2. - 12.) + GAP) / (tile + GAP))
                    .floor()
                    .max(1.) as usize
            }
        };
        let columns = columns_for(width.get());
        let rows = count.div_ceil(columns);
        // Tiles share the width out, so no gutter is left at the right.
        let tile = if width.get() > 0. {
            ((width.get() - PAD * 2. - 12. - GAP * (columns as f32 - 1.)) / columns as f32)
                .max(tile)
        } else {
            tile
        };
        let entity = cx.entity().downgrade();
        let base = scroll.0.borrow().base_handle.clone();
        let picture_side = tile - 8.;
        let list = uniform_list(id, rows, move |range, _, _| {
            range
                .map(|row| {
                    let mut line = div().flex().gap(px(GAP)).px(px(PAD)).pb(px(GAP));
                    let tiles: Vec<Tile> = (row * columns..((row + 1) * columns).min(count))
                        .map(|i| item(i))
                        .collect();
                    for (i, t) in (row * columns..).zip(tiles) {
                        let (entity, on_click) = (entity.clone(), on_click.clone());
                        line = line.child(
                            div()
                                .id((id, i))
                                .w(px(tile))
                                .flex_none()
                                .flex()
                                .flex_col()
                                .items_center()
                                .gap_1()
                                .p_1()
                                .rounded(px(4.))
                                .cursor_pointer()
                                .border_1()
                                .border_color(transparent_black())
                                .when(t.selected, |d| {
                                    d.bg(crate::theme::get().selected)
                                        .border_color(crate::theme::get().accent)
                                })
                                .when_some(t.tip, |d, tip| {
                                    d.tooltip(crate::widgets::Tooltip::text("", "", tip))
                                })
                                .when(!t.selected, |d| {
                                    d.hover(|s| s.bg(crate::theme::get().hover))
                                })
                                .child(
                                    div()
                                        .size(px(picture_side))
                                        .flex()
                                        .items_center()
                                        .justify_center()
                                        .rounded(px(3.))
                                        .relative()
                                        .overflow_hidden()
                                        .bg(t.backdrop.unwrap_or(backdrop))
                                        .when_some(t.picture, |d, p| {
                                            d.child(
                                                img(p).size_full().object_fit(ObjectFit::Contain),
                                            )
                                        })
                                        // A light edge, not the panel border:
                                        // black maps filled their tile to a
                                        // border as dark as the panel, and
                                        // read as gaps in the grid. Drawn over
                                        // the picture: under it, a square
                                        // thumbnail covered the edge along
                                        // part of its sides at small sizes.
                                        .child(
                                            div()
                                                .absolute()
                                                .inset_0()
                                                .rounded(px(3.))
                                                .border_1()
                                                .border_color(crate::theme::get().overlay_edge),
                                        ),
                                )
                                // Two lines, so names that differ past the middle
                                // ("…_diff_0" / "…_norm_0") stay apart: a one-line
                                // middle cut gave six tiles one label.
                                // Left-aligned, as the Library's tiles are:
                                // centred, a wrapped name's two lines started
                                // at different x and the grids differed.
                                // Name and detail together at the top of a
                                // fixed block, so a one-line name has its
                                // folder right under it and rows stay even.
                                .child(
                                    div()
                                        .w_full()
                                        .h(px(44.))
                                        .flex()
                                        .flex_col()
                                        .child(
                                            div()
                                                .w_full()
                                                // Its own height, so a one-line
                                                // name has its folder right under
                                                // it; the block's spare space
                                                // falls at the bottom, where a
                                                // gap reads as padding.
                                                .max_h(px(28.))
                                                .text_size(px(11.))
                                                .line_height(px(14.))
                                                .overflow_hidden()
                                                .line_clamp(2)
                                                .text_color(crate::theme::get().text)
                                                .child(crate::widgets::matched_text(
                                                    t.label, &query, t.selected,
                                                )),
                                        )
                                        .child(
                                            div()
                                                .w_full()
                                                .text_size(px(10.))
                                                .overflow_hidden()
                                                .whitespace_nowrap()
                                                // Cut from the start at the tile's
                                                // own width, once: a detail's end
                                                // is where siblings differ.
                                                .text_ellipsis_start()
                                                .text_color(crate::theme::get().muted)
                                                .child(t.detail),
                                        ),
                                )
                                .on_click(move |event, _, cx| {
                                    let _ = entity.update(cx, |this, cx| {
                                        on_click(this, i, event, cx);
                                        cx.notify();
                                    });
                                }),
                        );
                    }
                    line.into_any_element()
                })
                .collect()
        })
        .track_scroll(&scroll)
        .pt(px(PAD))
        .absolute()
        .inset_0();
        // Measures the width it was given; a change in columns redraws the
        // view being drawn now (panes are cached views, so the host's own
        // notify would not reach this one).
        let view = window.current_view();
        let host = cx.entity().downgrade();
        let measure = canvas(
            move |bounds, window, cx| {
                let w = f32::from(bounds.size.width);
                // A new width always restretches the tiles, so any change
                // redraws, not only one that changes the column count.
                let changed = (w - width.get()).abs() > 0.5;
                width.set(w);
                if changed {
                    // A notify while drawing only marks the view; the next
                    // frame has to be asked for, or it waits for input.
                    window.refresh();
                    cx.notify(view);
                    if let Some(f) = &on_resize {
                        let f = f.clone();
                        let _ = host.update(cx, |this, cx| f(this, cx));
                    }
                }
            },
            |_, _, _, _| {},
        )
        .absolute()
        .inset_0();
        div()
            .id(SharedString::from(format!("{id}-area")))
            .relative()
            .overflow_hidden()
            .child(measure)
            .child(list)
            .child(crate::widgets::scrollbar(id, &base))
    }
}

#[cfg(test)]
mod tests {
    use super::{Nav, Row, Tree};
    use crate::widgets::Disclosure;
    use gpui::{KeyDownEvent, Keystroke};

    // 0 ─┬─ 1 ─── 3
    //    └─ 2
    // 4
    fn children(n: &u32) -> Vec<u32> {
        match n {
            0 => vec![1, 2],
            1 => vec![3],
            _ => vec![],
        }
    }

    fn key(n: &u32) -> Option<u32> {
        (!children(n).is_empty()).then_some(*n)
    }

    fn rows(tree: &Tree<u32>) -> Vec<Row<u32>> {
        tree.flatten([0, 4], key, children)
    }

    fn press(tree: &mut Tree<u32>, k: &'static str, selected: Option<usize>) -> Option<Nav> {
        let event = KeyDownEvent {
            keystroke: Keystroke::parse(k).unwrap(),
            is_held: false,
            prefer_character_input: false,
        };
        let rows = rows(tree);
        tree.navigate(&event, &rows, selected, key)
    }

    #[test]
    fn flattening_walks_only_open_nodes_depth_first() {
        let mut tree = Tree::default();
        let nodes = |t: &Tree<u32>| {
            rows(t)
                .iter()
                .map(|r| (r.node, r.depth))
                .collect::<Vec<_>>()
        };
        assert_eq!(nodes(&tree), [(0, 0), (4, 0)]);
        assert_eq!(rows(&tree)[0].disclosure, Disclosure::Collapsed);
        tree.toggle(0);
        assert_eq!(nodes(&tree), [(0, 0), (1, 1), (2, 1), (4, 0)]);
        tree.reveal([1, 0]);
        assert_eq!(nodes(&tree), [(0, 0), (1, 1), (3, 2), (2, 1), (4, 0)]);
        assert_eq!(rows(&tree)[3].disclosure, Disclosure::Leaf);
        tree.collapse_all();
        assert_eq!(nodes(&tree).len(), 2);
    }

    #[test]
    fn searches_keep_matches_under_their_ancestors() {
        let found = |query: u32, limit| {
            Tree::<u32>::flatten_matching([0, 4], children, |n| *n == query || *n == 4, limit)
                .iter()
                .map(|r| (r.node, r.depth, r.disclosure))
                .collect::<Vec<_>>()
        };
        use Disclosure::{Expanded, Leaf};
        // 3 sits under 1 under 0; 2 is pruned. Expansion state is ignored.
        assert_eq!(
            found(3, 10),
            [
                (0, 0, Expanded),
                (1, 1, Expanded),
                (3, 2, Leaf),
                (4, 0, Leaf)
            ]
        );
        assert_eq!(found(0, 10), [(0, 0, Leaf), (4, 0, Leaf)]);
        // The limit bounds matches; 4 no longer fits.
        assert_eq!(
            found(3, 1),
            [(0, 0, Expanded), (1, 1, Expanded), (3, 2, Leaf)]
        );
    }

    #[test]
    fn arrows_move_open_close_and_step_to_parents() {
        let mut tree = Tree::default();
        assert_eq!(press(&mut tree, "down", None), Some(Nav::Select(0)));
        assert_eq!(press(&mut tree, "up", Some(0)), None);
        assert_eq!(press(&mut tree, "right", Some(0)), Some(Nav::Relayout));
        assert!(tree.is_expanded(0));
        assert_eq!(press(&mut tree, "right", Some(0)), Some(Nav::Select(1)));
        assert_eq!(press(&mut tree, "end", Some(1)), Some(Nav::Select(3)));
        // Row 2 is the leaf `2` under `0`: left goes to the parent.
        assert_eq!(press(&mut tree, "left", Some(2)), Some(Nav::Select(0)));
        assert_eq!(press(&mut tree, "left", Some(0)), Some(Nav::Relayout));
        assert!(!tree.is_expanded(0));
        assert_eq!(press(&mut tree, "left", Some(0)), None);
        assert_eq!(press(&mut tree, "ctrl-down", Some(0)), None);
    }
}
