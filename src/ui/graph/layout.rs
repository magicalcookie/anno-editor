// Where the graph's nodes sit: the layout in design units (bands, columns,
// pin order), nodes an edit adds placed among the held ones, and what the
// pane shows of it (places held per view, fit, reveal, zoom).
use super::view::GraphView;
use super::{Graph, Kind, Node, Pin, Wire};
use crate::ui::Bits;
use std::collections::HashMap;

/// Design units between columns, and between nodes in one.
const COLUMN_GAP: f32 = 80.;
const ROW_GAP: f32 = 14.;

/// Between the bands of All in file (`Graph::layout`).
const BAND_GAP: f32 = 48.;

/// A node's title bar and each pin's row, design units.
pub(super) const TITLE_H: f32 = 22.;
pub(super) const LINE_H: f32 = 17.;

/// Below this, node text is too small to read: a fit stops here.
pub(super) const MIN_FIT_ZOOM: f32 = 0.6;

/// A fit zooms a small graph in up to this, so a maximized pane reads
/// larger instead of framing the docked size in empty space (round 11: a
/// fit capped at 100% looked the same docked and maximized).
pub(super) const MAX_FIT_ZOOM: f32 = 1.6;

/// The least zoom anything goes to: an overview Fit of a whole file's
/// graph, and the wheel from there (a wheel floor above the overview's
/// turned the first step out into a jump in).
pub(super) const MIN_ZOOM: f32 = 0.03;

/// Views whose places are held at once: a modder flips between a few
/// (this widget, what it feeds, the whole file, a neighbour or two).
const HELD_VIEWS: usize = 8;

/// Where a node showed when its view was held, and its pins' rows by
/// what they stand for (`Pin::id`).
#[derive(Clone, Debug)]
pub(super) struct Held {
    at: [f32; 2],
    ins: Vec<String>,
    outs: Vec<String>,
}

/// What a fit framed: the file, the widget asked about (None in All in
/// file: a selection there outlines and reveals, it does not frame anew),
/// whether the whole file shows, What it feeds, and the pane's size.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(in crate::ui) struct FitKey {
    pub(super) file: Option<anno_formats::phx::Guid>,
    pub(super) widget: Option<usize>,
    pub(super) all: bool,
    pub(super) feeds: bool,
    pub(super) size: [i32; 2],
}

/// A file of the screen as the tables know it: the template (None: the
/// open file) and its instance key.
pub(super) type FileKey = (Option<anno_formats::phx::Guid>, u64);
/// A graph view: file, widget (None: the whole file), What it feeds.
pub(super) type ViewKey = (FileKey, Option<usize>, bool);

/// The root of `x`'s set in a union-find over `p`, halving the path.
fn find(p: &mut [usize], mut x: usize) -> usize {
    while p[x] != x {
        p[x] = p[p[x]];
        x = p[x];
    }
    x
}

/// Whether node rects `a` and `b` (x, y, width, height) overlap.
fn covers(a: [f32; 4], b: [f32; 4]) -> bool {
    a[0] < b[0] + b[2] && b[0] < a[0] + a[2] && a[1] < b[1] + b[3] && b[1] < a[1] + a[3]
}

/// Tops for a column's nodes, in their order, as near `wanted` as the
/// order and the gap between them allow: least squares, by pooling
/// neighbours that would overlap (pool adjacent violators).
fn spread(wanted: &[f32], heights: &[f32]) -> Vec<f32> {
    let mut offset = Vec::with_capacity(wanted.len());
    let mut at = 0.;
    for h in heights {
        offset.push(at);
        at += h + ROW_GAP;
    }
    // Each block: sum of targets, count; its nodes share one shift.
    let mut blocks: Vec<(f32, usize)> = Vec::new();
    for (w, o) in wanted.iter().zip(&offset) {
        blocks.push((w - o, 1));
        while let [.., (s0, n0), (s1, n1)] = blocks[..] {
            if s0 / n0 as f32 <= s1 / n1 as f32 {
                break;
            }
            blocks.pop();
            *blocks.last_mut().expect("two") = (s0 + s1, n0 + n1);
        }
    }
    blocks
        .iter()
        .flat_map(|&(s, n)| std::iter::repeat_n(s / n as f32, n))
        .zip(&offset)
        .map(|(z, o)| z + o)
        .collect()
}

impl Graph {
    /// All in file: a node at either end of the flow (a source such as
    /// the inputs, a data object or a widget's values, or a widget the
    /// bindings set) that separate chains run through is drawn once per
    /// chain, with that chain's pins. Each chain then lays out as a band of
    /// its own (`layout`) instead of one knot: a file's 85 bindings ran
    /// through one Inputs node and a few widgets and read as spaghetti.
    /// Plain copies between two such nodes go with the chain of the node
    /// they set, or a band of their own when it has none.
    pub(super) fn split_hubs(&mut self) {
        let n = self.nodes.len();
        let (mut fed, mut feeds) = (vec![0usize; n], vec![0usize; n]);
        for w in &self.wires {
            if w.from.0 != w.to.0 {
                feeds[w.from.0] += 1;
                fed[w.to.0] += 1;
            }
        }
        let end = |v: usize| fed[v] == 0 || feeds[v] == 0;
        // Union-find over the nodes, and past them one place per end node
        // for the plain copies into it.
        let mut parent: Vec<usize> = (0..2 * n).collect();
        let union = |p: &mut Vec<usize>, a: usize, b: usize| {
            let (a, b) = (find(p, a), find(p, b));
            p[a] = b;
        };
        let place = |w: &Wire| {
            if !end(w.from.0) {
                w.from.0
            } else if !end(w.to.0) {
                w.to.0
            } else {
                n + w.to.0
            }
        };
        for w in &self.wires {
            if !end(w.from.0) && !end(w.to.0) {
                union(&mut parent, w.from.0, w.to.0);
            }
        }
        for w in &self.wires {
            if end(w.from.0) && !end(w.to.0) {
                continue;
            }
            if let Some(chain) = self
                .wires
                .iter()
                .find(|c| c.to.0 == w.to.0 && !end(c.from.0))
                .filter(|_| end(w.from.0) && end(w.to.0))
            {
                // A copy into a widget some chain also sets joins it.
                union(&mut parent, n + w.to.0, chain.from.0);
            }
        }
        let band: Vec<usize> = self
            .wires
            .iter()
            .map(|w| find(&mut parent, place(w)))
            .collect();
        // Each band's name for copy keys: its least non-end node key, or
        // the node a band of plain copies sets.
        let mut anchor: HashMap<usize, String> = HashMap::new();
        for (k, w) in self.wires.iter().enumerate() {
            let named = [w.from.0, w.to.0]
                .into_iter()
                .filter(|&v| !end(v))
                .map(|v| self.nodes[v].key.clone())
                .min()
                .unwrap_or_else(|| self.nodes[w.to.0].key.clone());
            let a = anchor.entry(band[k]).or_insert(named.clone());
            if named < *a {
                *a = named;
            }
        }
        for v in 0..n {
            if !end(v) {
                continue;
            }
            // Its bands, in the order its wires come.
            let mut bands: Vec<usize> = Vec::new();
            for (k, w) in self.wires.iter().enumerate() {
                if (w.from.0 == v || w.to.0 == v) && !bands.contains(&band[k]) {
                    bands.push(band[k]);
                }
            }
            if bands.len() < 2 {
                continue;
            }
            let source = feeds[v] > 0;
            let pin_of = |w: &Wire| if source { w.from.1 } else { w.to.1 };
            // The first band keeps the node (and its unwired pins).
            let original = self.nodes[v].clone();
            let pins = |node: &Node| {
                if source {
                    node.outs.clone()
                } else {
                    node.ins.clone()
                }
            };
            for (i, &b) in bands.iter().enumerate() {
                let mut used: Vec<usize> = Vec::new();
                for (k, w) in self.wires.iter().enumerate() {
                    let here = if source { w.from.0 == v } else { w.to.0 == v };
                    if here && band[k] == b && !used.contains(&pin_of(w)) {
                        used.push(pin_of(w));
                    }
                }
                let all = pins(&original);
                let wired: Vec<bool> = (0..all.len())
                    .map(|p| {
                        self.wires.iter().any(|w| {
                            (if source { w.from.0 == v } else { w.to.0 == v }) && pin_of(w) == p
                        })
                    })
                    .collect();
                let keep: Vec<usize> = (0..all.len())
                    .filter(|&p| used.contains(&p) || (i == 0 && !wired[p]))
                    .collect();
                let mut node = original.clone();
                let kept: Vec<Pin> = keep.iter().map(|&p| all[p].clone()).collect();
                if source {
                    node.outs = kept;
                } else {
                    node.ins = kept;
                }
                let at = if i == 0 {
                    self.nodes[v] = node;
                    v
                } else {
                    node.key = format!("{} @ {}", original.key, anchor[&b]);
                    self.nodes.push(node);
                    self.nodes.len() - 1
                };
                for (k, w) in self.wires.iter_mut().enumerate() {
                    let here = if source { w.from.0 == v } else { w.to.0 == v };
                    if !here || band[k] != b {
                        continue;
                    }
                    let p = if source { &mut w.from } else { &mut w.to };
                    *p = (at, keep.iter().position(|&q| q == p.1).expect("kept"));
                }
            }
        }
    }

    /// Bands, one per set of wired nodes, stacked top to bottom in the
    /// order of the widgets they set, right-aligned so the widgets line up
    /// in the last column (`layout_band` lays each out).
    pub(super) fn layout(&mut self) {
        let n = self.nodes.len();
        if n == 0 {
            return;
        }
        // Sizes from the text they hold, at the pane's 11 px.
        for node in &mut self.nodes {
            let chars = |s: &str| s.chars().count() as f32;
            let pin = |p: &Pin| {
                (chars(&p.name) + p.value.as_deref().map_or(0., |v| chars(v).min(18.) + 2.)) * 6.
                    + 30.
            };
            // Title and subtitle share the header line ("Icon_Group wi…"
            // and "2 bindin…" were cut when each was measured alone).
            let gap = if node.sub.is_empty() { 0. } else { 8. };
            let widest = [
                chars(&node.title) * 7.5 + chars(&node.sub) * 6. + gap + 16.,
                node.ins
                    .iter()
                    .chain(&node.outs)
                    .map(pin)
                    .fold(0., f32::max),
            ]
            .into_iter()
            .fold(0., f32::max);
            // A note gets two lines: "not run: a source has no value" cut
            // to "not run: a source has n…" said nothing.
            let lines = node.ins.len() + node.outs.len() + 2 * usize::from(node.note.is_some());
            node.rect[2] = widest.clamp(110., 280.);
            node.rect[3] = TITLE_H + lines as f32 * LINE_H + 6.;
        }
        let mut parent: Vec<usize> = (0..n).collect();
        for w in &self.wires {
            let (a, b) = (find(&mut parent, w.from.0), find(&mut parent, w.to.0));
            parent[a] = b;
        }
        let mut bands: Vec<Vec<usize>> = Vec::new();
        let mut of: HashMap<usize, usize> = HashMap::new();
        for v in 0..n {
            let root = find(&mut parent, v);
            let i = *of.entry(root).or_insert_with(|| {
                bands.push(Vec::new());
                bands.len() - 1
            });
            bands[i].push(v);
        }
        // The widgets' order in the file, then key: a band of template
        // properties after the widgets'.
        let order = |band: &Vec<usize>| {
            let widget = band
                .iter()
                .filter_map(|&v| self.nodes[v].kind.widget())
                .min()
                .unwrap_or(usize::MAX);
            let key = band.iter().map(|&v| &self.nodes[v].key).min().cloned();
            (widget, key)
        };
        bands.sort_by_cached_key(order);
        // (size, where its last column starts): every band's last column
        // starts at one x, so the widgets read as one column.
        let laid: Vec<([f32; 2], f32)> = bands.iter().map(|b| self.layout_band(b)).collect();
        let last = laid.iter().map(|l| l.1).fold(0., f32::max);
        let (mut y, mut width) = (0., 0f32);
        for (band, (size, at)) in bands.iter().zip(&laid) {
            for &v in band {
                self.nodes[v].rect[0] += last - at;
                self.nodes[v].rect[1] += y;
            }
            width = width.max(last - at + size[0]);
            y += size[1] + BAND_GAP;
        }
        // The lanes line up across bands and hold nothing else: what is
        // read left of every other node, what is written right of them (a
        // wider band's first column sat inside the read lane).
        let read = |n: &Node| matches!(n.kind, Kind::Data) && n.ins.is_empty();
        let written = |n: &Node| matches!(n.kind, Kind::Data) && !n.ins.is_empty();
        let model = |n: &Node| read(n) || written(n);
        let lane = (self.nodes.iter().filter(|n| read(n)))
            .map(|n| n.rect[2])
            .fold(0., f32::max);
        let start = (self.nodes.iter().filter(|n| !model(n)))
            .map(|n| n.rect[0])
            .fold(f32::MAX, f32::min);
        if start < f32::MAX {
            for n in self.nodes.iter_mut().filter(|n| read(n)) {
                n.rect[0] = start - COLUMN_GAP - lane;
            }
            let right = (self.nodes.iter().filter(|n| !model(n)))
                .map(|n| n.rect[0] + n.rect[2])
                .fold(0., f32::max);
            for n in self.nodes.iter_mut().filter(|n| written(n)) {
                n.rect[0] = right + COLUMN_GAP;
            }
        }
        let left = (self.nodes.iter().map(|n| n.rect[0])).fold(f32::MAX, f32::min);
        for n in &mut self.nodes {
            n.rect[0] -= left;
        }
        width = (self.nodes.iter().map(|n| n.rect[0] + n.rect[2])).fold(0., f32::max);
        self.size = [width, y - BAND_GAP];
        self.order_pins();
    }

    /// The view-model lanes as drawn: (left, right) of the view models the
    /// file reads, then of those it writes, each with its heading; none
    /// when the graph has none of that side.
    pub(super) fn lanes(&self) -> Vec<([f32; 2], &'static str)> {
        let span = |pick: &dyn Fn(&Node) -> bool| {
            self.nodes
                .iter()
                .filter(|n| matches!(n.kind, Kind::Data) && pick(n))
                .map(|n| [n.rect[0], n.rect[0] + n.rect[2]])
                .reduce(|a, b| [a[0].min(b[0]), a[1].max(b[1])])
        };
        [
            (span(&|n| n.ins.is_empty()), "View models · read"),
            (span(&|n| !n.ins.is_empty()), "View models · written"),
        ]
        .into_iter()
        .filter_map(|(s, label)| Some((s?, label)))
        .collect()
    }

    /// Lays out the wired nodes `band` from (0, 0), returning its size and
    /// where its widget column (the last before the written view models)
    /// starts.
    /// Columns by longest path from the sources (two-way bindings' back
    /// edges ignored); a source sits just left of what first reads it and
    /// what only is set (a widget, a written field, a clip) in the last
    /// column. Each
    /// column is ordered by where its neighbours sit, a few sweeps each
    /// way, then each node is moved as near as the order allows to the
    /// height of the pins it is wired to, so wires run level.
    fn layout_band(&mut self, band: &[usize]) -> ([f32; 2], f32) {
        let m = band.len();
        let local: HashMap<usize, usize> = band.iter().enumerate().map(|(i, &v)| (v, i)).collect();
        let wires: Vec<usize> = (0..self.wires.len())
            .filter(|&k| {
                let w = &self.wires[k];
                w.from.0 != w.to.0 && local.contains_key(&w.from.0)
            })
            .collect();
        let mut succ: Vec<Vec<usize>> = vec![Vec::new(); m];
        let mut pred: Vec<Vec<usize>> = vec![Vec::new(); m];
        for &k in &wires {
            let w = &self.wires[k];
            let (a, b) = (local[&w.from.0], local[&w.to.0]);
            if !succ[a].contains(&b) {
                succ[a].push(b);
                pred[b].push(a);
            }
        }
        // Depth-first: finishing order, and the edges that close a loop.
        let (mut state, mut order, mut back) = (vec![0u8; m], Vec::with_capacity(m), Vec::new());
        for root in 0..m {
            if state[root] != 0 {
                continue;
            }
            let mut stack = vec![(root, 0usize)];
            state[root] = 1;
            while let Some(&mut (v, ref mut k)) = stack.last_mut() {
                if let Some(&u) = succ[v].get(*k) {
                    *k += 1;
                    match state[u] {
                        0 => {
                            state[u] = 1;
                            stack.push((u, 0));
                        }
                        1 => back.push((v, u)),
                        _ => {}
                    }
                } else {
                    state[v] = 2;
                    order.push(v);
                    stack.pop();
                }
            }
        }
        let mut col = vec![0usize; m];
        for &v in order.iter().rev() {
            for &u in &succ[v] {
                if !back.contains(&(v, u)) {
                    col[u] = col[u].max(col[v] + 1);
                }
            }
        }
        let last = col.iter().max().copied().unwrap_or(0);
        for v in 0..m {
            if pred[v].is_empty() {
                // Next to what reads it: a source in the first column drew
                // wires across the whole band.
                col[v] = succ[v]
                    .iter()
                    .map(|&u| col[u])
                    .min()
                    .map_or(0, |c| c.max(1) - 1);
            } else if succ[v].is_empty() && !matches!(self.nodes[band[v]].kind, Kind::Logic { .. })
            {
                // What is set (a widget, a written field, a clip): a
                // converter's own result stays where its inputs put it.
                col[v] = last;
            }
        }
        // View models keep to lanes of their own, apart from the widgets
        // (user, round 17): what the file reads alone in the first column,
        // what it writes in a column after the widgets.
        let model = |v: usize| matches!(self.nodes[band[v]].kind, Kind::Data);
        let read = |v: usize| model(v) && self.nodes[band[v]].ins.is_empty();
        if (0..m).any(read) {
            for v in 0..m {
                col[v] = if read(v) { 0 } else { col[v] + 1 };
            }
        }
        let widgets = (0..m)
            .filter(|&v| !model(v))
            .map(|v| col[v])
            .max()
            .unwrap_or(0);
        for v in (0..m).filter(|&v| model(v) && !read(v)) {
            col[v] = widgets + 1;
        }
        let columns = col.iter().max().copied().unwrap_or(0) + 1;
        let mut lanes: Vec<Vec<usize>> = vec![Vec::new(); columns];
        for v in 0..m {
            lanes[col[v]].push(v);
        }
        // Start each column in key order, not binding order: an edit that
        // moves one binding to the end then leaves the others in place.
        let key = |v: usize| &self.nodes[band[v]].key;
        for lane in &mut lanes {
            lane.sort_by(|&a, &b| key(a).cmp(key(b)));
        }
        let mut rank = vec![0f32; m];
        for lane in &lanes {
            for (i, &v) in lane.iter().enumerate() {
                rank[v] = i as f32;
            }
        }
        // A pin's place in the column: its node's rank plus how far down
        // the node it sits, so wires into a node's first pin pull toward
        // the top.
        let node = |v: usize| &self.nodes[band[v]];
        let rows = |v: usize| (node(v).ins.len() + node(v).outs.len()).max(1) as f32;
        let mut wires_in: Vec<Vec<usize>> = vec![Vec::new(); m];
        let mut wires_out: Vec<Vec<usize>> = vec![Vec::new(); m];
        for &k in &wires {
            let w = &self.wires[k];
            wires_in[local[&w.to.0]].push(k);
            wires_out[local[&w.from.0]].push(k);
        }
        let out_frac = |k: usize| {
            let w = &self.wires[k];
            let v = local[&w.from.0];
            (node(v).ins.len() + w.from.1) as f32 / rows(v)
        };
        let in_frac = |k: usize| {
            let w = &self.wires[k];
            w.to.1 as f32 / rows(local[&w.to.0])
        };
        for sweep in 0..12 {
            let forward = sweep % 2 == 0;
            for c in 0..columns {
                let c = if forward { c } else { columns - 1 - c };
                let mut keyed: Vec<(f32, usize)> = lanes[c]
                    .iter()
                    .map(|&v| {
                        let (mut sum, mut count) = (0., 0.);
                        let near = if forward { &wires_in[v] } else { &wires_out[v] };
                        for &k in near {
                            let w = &self.wires[k];
                            sum += if forward {
                                rank[local[&w.from.0]] + out_frac(k)
                            } else {
                                rank[local[&w.to.0]] + in_frac(k)
                            };
                            count += 1.;
                        }
                        let k = if count > 0. { sum / count } else { rank[v] };
                        (k, v)
                    })
                    .collect();
                keyed.sort_by(|a, b| a.0.total_cmp(&b.0));
                lanes[c] = keyed.into_iter().map(|(_, v)| v).collect();
                for (i, &v) in lanes[c].iter().enumerate() {
                    rank[v] = i as f32;
                }
            }
        }
        // Heights: stacked first, then each column in turn moved as near
        // as its order allows to where its wires come from (or go), a few
        // passes each way.
        let height = |v: usize| node(v).rect[3];
        let in_y = |k: usize| TITLE_H + self.wires[k].to.1 as f32 * LINE_H + LINE_H / 2.;
        let out_y = |k: usize| {
            let w = &self.wires[k];
            TITLE_H + (self.nodes[w.from.0].ins.len() + w.from.1) as f32 * LINE_H + LINE_H / 2.
        };
        let mut y = vec![0f32; m];
        for lane in &lanes {
            let mut at = 0.;
            for &v in lane {
                y[v] = at;
                at += height(v) + ROW_GAP;
            }
        }
        for pass in 0..8 {
            let forward = pass % 2 == 0;
            for c in 0..columns {
                let c = if forward { c } else { columns - 1 - c };
                let wanted: Vec<f32> = lanes[c]
                    .iter()
                    .map(|&v| {
                        let from = |ks: &[usize], other: &dyn Fn(usize) -> f32| {
                            (!ks.is_empty()).then(|| {
                                ks.iter().map(|&k| other(k)).sum::<f32>() / ks.len() as f32
                            })
                        };
                        let by_in = || {
                            from(&wires_in[v], &|k| {
                                y[local[&self.wires[k].from.0]] + out_y(k) - in_y(k)
                            })
                        };
                        let by_out = || {
                            from(&wires_out[v], &|k| {
                                y[local[&self.wires[k].to.0]] + in_y(k) - out_y(k)
                            })
                        };
                        if forward {
                            by_in().or_else(by_out)
                        } else {
                            by_out().or_else(by_in)
                        }
                        .unwrap_or(y[v])
                    })
                    .collect();
                let heights: Vec<f32> = lanes[c].iter().map(|&v| height(v)).collect();
                for (&v, at) in lanes[c].iter().zip(spread(&wanted, &heights)) {
                    y[v] = at;
                }
            }
        }
        let top = y.iter().copied().fold(f32::MAX, f32::min);
        let (mut x, mut last_x) = (0., 0.);
        let mut bottom: f32 = 0.;
        for (c, lane) in lanes.iter().enumerate() {
            if c <= widgets {
                last_x = x;
            }
            let width = lane
                .iter()
                .map(|&v| self.nodes[band[v]].rect[2])
                .fold(0., f32::max);
            for &v in lane {
                let r = &mut self.nodes[band[v]].rect;
                r[0] = x;
                r[1] = y[v] - top;
                bottom = bottom.max(r[1] + r[3]);
            }
            x += width + COLUMN_GAP;
        }
        ([x - COLUMN_GAP, bottom], last_x)
    }

    /// Lists each value node's pins in the order of what they connect to,
    /// top to bottom, so wires into one node do not cross on the way in.
    /// A converter's inputs keep their meaning's order.
    fn order_pins(&mut self) {
        for n in 0..self.nodes.len() {
            if matches!(self.nodes[n].kind, Kind::Logic { .. }) {
                continue;
            }
            for inputs in [true, false] {
                let len = if inputs {
                    self.nodes[n].ins.len()
                } else {
                    self.nodes[n].outs.len()
                };
                let mut key: Vec<(f32, usize)> = (0..len)
                    .map(|p| {
                        let ys: Vec<f32> = self
                            .wires
                            .iter()
                            .filter_map(|w| match inputs {
                                true if w.to == (n, p) => Some(self.out_at(w.from.0, w.from.1)[1]),
                                false if w.from == (n, p) => Some(self.in_at(w.to.0, w.to.1)[1]),
                                _ => None,
                            })
                            .collect();
                        // Unwired pins (open ones) go last, in their order.
                        let y = if ys.is_empty() {
                            f32::MAX
                        } else {
                            ys.iter().sum::<f32>() / ys.len() as f32
                        };
                        (y, p)
                    })
                    .collect();
                key.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)));
                let order: Vec<usize> = key.iter().map(|&(_, p)| p).collect();
                self.reorder_pins(n, inputs, &order);
            }
        }
    }

    /// Puts node `n`'s input (or output) pins in `order` (old indices, new
    /// order), and the wires' ends with them.
    fn reorder_pins(&mut self, n: usize, inputs: bool, order: &[usize]) {
        if order.iter().enumerate().all(|(i, &o)| i == o) {
            return;
        }
        let mut new_of = vec![0; order.len()];
        for (new, &old) in order.iter().enumerate() {
            new_of[old] = new;
        }
        let node = &mut self.nodes[n];
        let pins = if inputs {
            &mut node.ins
        } else {
            &mut node.outs
        };
        let mut moved: Vec<Option<Pin>> = std::mem::take(pins).into_iter().map(Some).collect();
        *pins = order
            .iter()
            .map(|&o| moved[o].take().expect("each pin once"))
            .collect();
        for w in &mut self.wires {
            if inputs && w.to.0 == n {
                w.to.1 = new_of[w.to.1];
            } else if !inputs && w.from.0 == n {
                w.from.1 = new_of[w.from.1];
            }
        }
    }

    /// Where output pin `pin` of node `n` sits (its right edge), and input
    /// pin `pin` (its left edge). Inputs list first, outputs under them.
    pub(super) fn out_at(&self, n: usize, pin: usize) -> [f32; 2] {
        let node = &self.nodes[n];
        let r = node.rect;
        let row = node.ins.len() + pin;
        [
            r[0] + r[2],
            r[1] + TITLE_H + row as f32 * LINE_H + LINE_H / 2.,
        ]
    }

    /// What a view first shows, as (origin, size): the whole graph, or in
    /// All in file with a widget selected (`subject`, its nodes: what it
    /// reads out and what is set on it) the band of the node most of it is
    /// set on, when that band fits `pane` at a zoom text reads at, else that
    /// node with what it reads and feeds, else the node alone. A widget set
    /// from several chains has a copy in each band; their union is mostly
    /// the gap between the bands (round 15: the icon row opened on empty
    /// space), and the other copies stay outlined (round 12: All in file
    /// opened at the top left, the widget nowhere).
    pub(super) fn opening(
        &self,
        subject: &[usize],
        pane: [f32; 2],
    ) -> Option<([f32; 2], [f32; 2])> {
        /// Below this the pins' text is not legible (10.5 px design).
        const READABLE: f32 = 0.9;
        let around = |nodes: &mut dyn Iterator<Item = usize>| {
            let (lo, hi) = nodes.fold(([f32::MAX; 2], [f32::MIN; 2]), |(lo, hi), i| {
                let r = self.nodes[i].rect;
                (
                    [lo[0].min(r[0]), lo[1].min(r[1])],
                    [hi[0].max(r[0] + r[2]), hi[1].max(r[1] + r[3])],
                )
            });
            (lo[0] <= hi[0]).then(|| (lo, [hi[0] - lo[0], hi[1] - lo[1]]))
        };
        if subject.is_empty() {
            return around(&mut (0..self.nodes.len()));
        }
        let main = subject
            .iter()
            .copied()
            .max_by_key(|&i| self.nodes[i].ins.len())
            .expect("a subject");
        // Its band: everything wired to it, either way, however far.
        let mut seen = Bits::new(self.nodes.len());
        seen.set(main, true);
        let mut todo = vec![main];
        while let Some(n) = todo.pop() {
            for w in &self.wires {
                let next = match (w.from.0 == n, w.to.0 == n) {
                    (true, _) => w.to.0,
                    (_, true) => w.from.0,
                    _ => continue,
                };
                if !seen.get(next) {
                    seen.set(next, true);
                    todo.push(next);
                }
            }
        }
        let fits = |(_, size): &([f32; 2], [f32; 2])| {
            ((pane[0] - 40.) / size[0].max(1.)).min((pane[1] - 40.) / size[1].max(1.)) >= READABLE
        };
        let near = self
            .wires
            .iter()
            .filter_map(|w| match (w.from.0 == main, w.to.0 == main) {
                (true, _) => Some(w.to.0),
                (_, true) => Some(w.from.0),
                _ => None,
            });
        [
            around(&mut (0..self.nodes.len()).filter(|&i| seen.get(i))),
            around(&mut std::iter::once(main).chain(near)),
        ]
        .into_iter()
        .flatten()
        .find(fits)
        .or_else(|| around(&mut std::iter::once(main)))
    }

    pub(super) fn in_at(&self, n: usize, pin: usize) -> [f32; 2] {
        let r = self.nodes[n].rect;
        [r[0], r[1] + TITLE_H + pin as f32 * LINE_H + LINE_H / 2.]
    }

    /// Moves `target` and the placed nodes downstream of it `dx` to the
    /// right, as Blender's auto-offset does; when that would cover a node
    /// left in place, every placed node from the target's column right
    /// moves instead. Round 10: the column-wide shift alone moved a whole
    /// file's graph for one insert.
    fn make_room(&mut self, target: usize, dx: f32, placed: &Bits) {
        let n = self.nodes.len();
        let mut down = Bits::new(n);
        let mut queue = vec![target];
        while let Some(i) = queue.pop() {
            if down.get(i) {
                continue;
            }
            down.set(i, true);
            queue.extend(
                self.wires
                    .iter()
                    .filter(|w| w.from.0 == i && placed.get(w.to.0))
                    .map(|w| w.to.0),
            );
        }
        let shift = |g: &mut Graph, set: &dyn Fn(usize) -> bool, dx: f32| {
            for j in 0..n {
                if set(j) {
                    g.nodes[j].rect[0] += dx;
                }
            }
        };
        shift(self, &|j| down.get(j), dx);
        let clash = (0..n).any(|j| {
            down.get(j)
                && (0..n).any(|k| {
                    placed.get(k) && !down.get(k) && covers(self.nodes[j].rect, self.nodes[k].rect)
                })
        });
        if clash {
            shift(self, &|j| down.get(j), -dx);
            let column = self.nodes[target].rect[0];
            let mut wide = Bits::new(n);
            for j in 0..n {
                wide.set(j, placed.get(j) && self.nodes[j].rect[0] >= column);
            }
            shift(self, &|j| wide.get(j), dx);
        }
    }

    /// Places the nodes not `placed` among those that are: each left of
    /// the first placed node it feeds, level with that pin, but right of its
    /// own placed sources (what it feeds, and every placed node from there
    /// right, moves over to make room), then down past any placed node it
    /// would cover. The layout's own spot for a node an edit adds is often
    /// one a held node kept (round 8: a bound Equal drew exactly over the
    /// old one), and the spot before what it feeds can be left of what
    /// feeds it (round 9: an inverted copy's "not" sat under Inputs, its
    /// wire running back through it). Rightmost first, so what a new node
    /// feeds is placed before it. Whether it placed any.
    fn place_new(&mut self, placed: &mut Bits) -> bool {
        let mut new: Vec<usize> = (0..self.nodes.len()).filter(|&i| !placed.get(i)).collect();
        if new.is_empty() {
            return false;
        }
        new.sort_by(|&a, &b| self.nodes[b].rect[0].total_cmp(&self.nodes[a].rect[0]));
        for i in new {
            if let Some((from, to)) = self
                .wires
                .iter()
                .find(|w| w.from.0 == i && placed.get(w.to.0))
                .map(|w| (w.from, w.to))
            {
                let target = to.0;
                let to = self.in_at(to.0, to.1);
                let dy = self.out_at(i, from.1)[1] - self.nodes[i].rect[1];
                let mut x = to[0] - COLUMN_GAP - self.nodes[i].rect[2];
                let after = self
                    .wires
                    .iter()
                    .filter(|s| s.to.0 == i && s.from.0 != i && placed.get(s.from.0))
                    .map(|s| {
                        let r = self.nodes[s.from.0].rect;
                        r[0] + r[2] + COLUMN_GAP
                    })
                    .fold(f32::MIN, f32::max);
                if x < after {
                    self.make_room(target, after - x, placed);
                    x = after;
                }
                self.nodes[i].rect[0] = x;
                self.nodes[i].rect[1] = to[1] - dy;
            }
            // Each step lands below the node it hit, so it never hits that
            // one again: at most one step per node.
            while let Some(j) = (0..self.nodes.len()).find(|&j| {
                j != i && placed.get(j) && covers(self.nodes[i].rect, self.nodes[j].rect)
            }) {
                self.nodes[i].rect[1] = self.nodes[j].rect[1] + self.nodes[j].rect[3] + ROW_GAP;
            }
            placed.set(i, true);
        }
        true
    }
}

impl GraphView {
    pub(super) fn to_screen(&self, p: [f32; 2]) -> [f32; 2] {
        [
            p[0] * self.zoom + self.pan[0],
            p[1] * self.zoom + self.pan[1],
        ]
    }

    /// Holds every node of the graph on screen where it is now; `relayout`
    /// calls it on every revision (see `held`). The cached graph is the one
    /// drawn (placed), so this is what the screen shows; places held before
    /// stay for nodes not drawn now (one an undo brings back returns to
    /// where it was). Round 5: holding only the last build's layout let the
    /// second edit reflow everything.
    pub(in crate::ui) fn hold_places(&mut self) {
        let Some(cache) = self.cache.take() else {
            return;
        };
        let view = (cache.0.0, cache.0.1, cache.0.2);
        self.hold(view, &cache.1);
        self.cache = Some(cache);
    }

    /// Holds every node of `g`, drawn for `view`, where it is.
    fn hold(&mut self, view: ViewKey, g: &Graph) {
        let mut places = match self.held.iter().position(|(v, _)| *v == view) {
            Some(i) => self.held.remove(i).1,
            None => HashMap::new(),
        };
        for n in &g.nodes {
            let names = |pins: &[Pin]| pins.iter().map(|p| p.id().to_owned()).collect();
            places.insert(
                n.key.clone(),
                Held {
                    at: [n.rect[0], n.rect[1]],
                    ins: names(&n.ins),
                    outs: names(&n.outs),
                },
            );
        }
        // The view held last goes last; the oldest beyond the bound go.
        self.held.push((view, places));
        if self.held.len() > HELD_VIEWS {
            self.held.remove(0);
        }
    }

    /// Puts the nodes of `graph`, built for `view`, where they show: where
    /// they were held for an edit in this view, then where they were
    /// dragged; in a held view a node neither kept (one an edit just added)
    /// beside what it feeds; the rest where the layout put them.
    pub(super) fn place(&mut self, view: ViewKey, graph: &mut Graph, layout: &[[f32; 2]]) {
        let places = self.held.iter().find(|(v, _)| *v == view).map(|(_, p)| p);
        // Nodes held or dragged: where the screen shows them.
        let mut kept = Bits::new(graph.nodes.len());
        for (i, &laid) in layout.iter().enumerate().take(graph.nodes.len()) {
            let n = &graph.nodes[i];
            let held = places.and_then(|p| p.get(&n.key)).cloned();
            let dragged = self.moved.get(&(view.0.0, n.key.clone()));
            kept.set(i, held.is_some() || dragged.is_some());
            let [x, y] = dragged
                .copied()
                .or(held.as_ref().map(|h| h.at))
                .unwrap_or(laid);
            graph.nodes[i].rect[0] = x;
            graph.nodes[i].rect[1] = y;
            // Its pins keep their rows: a held name first where it was,
            // new pins after, in the layout's order.
            if let Some(h) = held {
                for (inputs, names) in [(true, &h.ins), (false, &h.outs)] {
                    let pins = if inputs {
                        &graph.nodes[i].ins
                    } else {
                        &graph.nodes[i].outs
                    };
                    let mut taken = Bits::new(names.len());
                    let mut key: Vec<(usize, usize)> = pins
                        .iter()
                        .enumerate()
                        .map(|(p, pin)| {
                            // By what it stands for: a pin bound from its
                            // "+ " row keeps that row.
                            let at =
                                (0..names.len()).find(|&j| !taken.get(j) && names[j] == pin.id());
                            if let Some(j) = at {
                                taken.set(j, true);
                            }
                            (at.unwrap_or(names.len() + p), p)
                        })
                        .collect();
                    key.sort_unstable();
                    let order: Vec<usize> = key.into_iter().map(|(_, p)| p).collect();
                    graph.reorder_pins(i, inputs, &order);
                }
            }
        }
        // What it placed is held at once: otherwise it follows what it
        // feeds until the next edit (round 9), and is placed every frame.
        let new: Vec<usize> = (0..graph.nodes.len()).filter(|&i| !kept.get(i)).collect();
        if places.is_some() && graph.place_new(&mut kept) {
            self.reveal_nodes(graph, &new);
            // Dragged nodes the room-making moved keep their new place: a
            // stale drag put the target back over what was inserted
            // before it (round 10).
            for n in &graph.nodes {
                if let Some(at) = self.moved.get_mut(&(view.0.0, n.key.clone())) {
                    *at = [n.rect[0], n.rect[1]];
                }
            }
            self.hold(view, graph);
        }
    }

    /// Shows `nodes` with what they feed, then what they read: an inserted
    /// "not" shown without its source looked fed from nowhere (round 11).
    pub(super) fn reveal_nodes(&mut self, graph: &Graph, nodes: &[usize]) {
        let mut added = Bits::new(graph.nodes.len());
        for &i in nodes {
            added.set(i, true);
        }
        let added = &added;
        let hop = |out: bool| {
            graph.wires.iter().filter_map(move |w| {
                let (near, far) = if out {
                    (w.from.0, w.to.0)
                } else {
                    (w.to.0, w.from.0)
                };
                (added.get(near) && !added.get(far)).then_some(far)
            })
        };
        let mut wanted: Vec<[f32; 4]> = nodes.iter().map(|&i| graph.nodes[i].rect).collect();
        wanted.extend(hop(true).map(|i| graph.nodes[i].rect));
        let least = wanted.len();
        wanted.extend(hop(false).map(|i| graph.nodes[i].rect));
        self.reveal(&wanted, least);
    }

    /// The nodes of a fresh build of `view` that the last build of it did
    /// not have; none for the first build of a view.
    pub(super) fn appeared(&mut self, view: ViewKey, graph: &Graph) -> Vec<usize> {
        use std::hash::{BuildHasher, BuildHasherDefault, DefaultHasher};
        let hash = |k: &str| BuildHasherDefault::<DefaultHasher>::default().hash_one(k);
        let keys: std::collections::HashSet<u64> =
            graph.nodes.iter().map(|n| hash(&n.key)).collect();
        let appeared = match &self.last_keys {
            Some((v, before)) if *v == view => (0..graph.nodes.len())
                .filter(|&i| !before.contains(&hash(&graph.nodes[i].key)))
                .collect(),
            _ => Vec::new(),
        };
        self.last_keys = Some((view, keys));
        appeared
    }

    /// Pans the least that shows `wanted` (graph units) in the pane, zoom
    /// as it is: a node an edit adds below a tall column landed outside
    /// the view, and the bind looked like it had done nothing (round 9).
    /// When all of them do not fit, only the first `least`.
    /// Nothing before the pane's size is known, or while a gesture runs.
    // TODO(alex): a source left out could get a tick at the pane's edge.
    pub(super) fn reveal(&mut self, wanted: &[[f32; 4]], least: usize) {
        const MARGIN: f32 = 24.;
        let (w, h) = (
            f32::from(self.bounds.size.width),
            f32::from(self.bounds.size.height),
        );
        if w < 20. || self.wiring.is_some() || self.moving.is_some() || self.drag.is_some() {
            return;
        }
        let around = |rects: &[[f32; 4]]| {
            let (mut lo, mut hi) = ([f32::MAX; 2], [f32::MIN; 2]);
            for r in rects {
                let a = self.to_screen([r[0], r[1]]);
                let b = self.to_screen([r[0] + r[2], r[1] + r[3]]);
                lo = [lo[0].min(a[0]), lo[1].min(a[1])];
                hi = [hi[0].max(b[0]), hi[1].max(b[1])];
            }
            (lo, hi)
        };
        // Per axis: a docked pane is often shorter than one widget node,
        // and the source still fits beside it.
        let (mut lo, mut hi) = around(wanted);
        let (few_lo, few_hi) = around(&wanted[..least.min(wanted.len())]);
        for (axis, size) in [(0, w), (1, h)] {
            if hi[axis] - lo[axis] > size - 2. * MARGIN {
                (lo[axis], hi[axis]) = (few_lo[axis], few_hi[axis]);
            }
        }
        if lo[0] > hi[0] {
            return;
        }
        // Per axis: bring the far side in, then the near side, so a box
        // larger than the pane shows its top left. A view panned to show an
        // edit stays where it shows it across a resize (round 13).
        let pan = self.pan;
        for (axis, size) in [(0, w), (1, h)] {
            if hi[axis] > size - MARGIN {
                let d = hi[axis] - (size - MARGIN);
                self.pan[axis] -= d;
                lo[axis] -= d;
            }
            if lo[axis] < MARGIN {
                self.pan[axis] += MARGIN - lo[axis];
            }
        }
        self.touched |= self.pan != pan;
    }

    /// Lets go of the places held in `file`'s views: the layout places
    /// every node again (Tidy).
    pub(in crate::ui) fn let_go(&mut self, file: Option<anno_formats::phx::Guid>) {
        self.held.retain(|(v, _)| v.0.0 != file);
    }

    /// Whether node rect `r` (graph units) is wholly on the pane.
    pub(super) fn on_pane(&self, r: [f32; 4]) -> bool {
        let (a, b) = (
            self.to_screen([r[0], r[1]]),
            self.to_screen([r[0] + r[2], r[1] + r[3]]),
        );
        a[0] >= 0.
            && a[1] >= 0.
            && b[0] <= f32::from(self.bounds.size.width)
            && b[1] <= f32::from(self.bounds.size.height)
    }

    /// One wheel step of `dy` pixels at pane point `at`, which stays put.
    pub(super) fn zoom_by(&mut self, dy: f32, at: [f32; 2]) {
        self.moved_by_hand();
        let old = self.zoom;
        self.zoom = (old * (dy * 0.002).exp()).clamp(MIN_ZOOM, 2.);
        for (pan, at) in self.pan.iter_mut().zip(at) {
            *pan = at - (at - *pan) * self.zoom / old;
        }
    }

    /// Frames the view for `key` once. A new view (file, widget, What it
    /// feeds) fits what `extent` gives (origin, size), and so does a new
    /// pane size while the view is the fit's. Once panned or zoomed by hand,
    /// a new pane size keeps the zoom and the graph point at the pane's
    /// centre (round 12: maximize, pan, restore refitted and the view was
    /// gone).
    pub(super) fn frame(
        &mut self,
        key: FitKey,
        extent: impl FnOnce() -> Option<([f32; 2], [f32; 2])>,
    ) {
        if self.fitted == Some(key) {
            return;
        }
        match self.fitted {
            Some(was)
                if self.touched
                    && FitKey {
                        size: key.size,
                        ..was
                    } == key =>
            {
                self.pan[0] += (key.size[0] - was.size[0]) as f32 / 2.;
                self.pan[1] += (key.size[1] - was.size[1]) as f32 / 2.;
            }
            _ => {
                if let Some((origin, size)) = extent() {
                    self.fit(origin, size);
                }
                self.touched = false;
            }
        }
        self.fitted = Some(key);
    }

    /// Zoom and pan so `size` fills the pane, no larger than `MAX_FIT_ZOOM` and, but
    /// for an overview, no smaller than readable. A graph too big for that
    /// starts at its top left; drag to see the rest.
    pub(super) fn fit(&mut self, origin: [f32; 2], size: [f32; 2]) {
        let least = if std::mem::take(&mut self.overview) {
            MIN_ZOOM
        } else {
            MIN_FIT_ZOOM
        };
        let (w, h) = (
            f32::from(self.bounds.size.width),
            f32::from(self.bounds.size.height),
        );
        if w < 20. || h < 20. {
            return;
        }
        self.zoom = ((w - 40.) / size[0].max(1.))
            .min((h - 40.) / size[1].max(1.))
            .clamp(least, MAX_FIT_ZOOM);
        let (gw, gh) = (size[0] * self.zoom, size[1] * self.zoom);
        let place = |pane: f32, graph: f32| {
            if graph + 40. <= pane {
                (pane - graph) / 2.
            } else {
                20.
            }
        };
        self.pan = [
            place(w, gw) - origin[0] * self.zoom,
            place(h, gh) - origin[1] * self.zoom,
        ];
    }
}
