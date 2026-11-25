// Owns a lake's basin: its shore filed for inside and nearest-edge lookups,
// carving the basin and banks, the region an outline edit can change, and
// filling the basin an edit moved away from.
use super::{Lattice, Sample, mark, segment_distance, steps};
use anno_formats::a7m::{
    grids::{Grids, Kind},
    objects::PathPoint,
};
use gpui_editor_kit::heightfield::Heightfield;
use std::ops::Range;

/// Lakes keep their shore just under the water, as shipped lakes do.
const EDGE_DEPTH: f32 = 0.3;
/// Lake banks rise from the water's edge with no slope at first,
/// steepening toward `BANK_SLOPE` (27°), until they meet the ground.
/// Carving stops `BANK_MAX` metres past the edge; a trench deeper than the
/// bank reaches there (about 9 m) keeps a step.
const BANK_SLOPE: f32 = 0.5;
pub(super) const BANK_MAX: f32 = 20.;
/// Lake basins deepen by `LAKE_SLOPE` per metre from the shore down to
/// `LAKE_DEPTH`; their banks ease in over `LAKE_BANK` metres. Past
/// `(LAKE_DEPTH - EDGE_DEPTH) / LAKE_SLOPE` (4.25 m) from the shore the
/// basin is flat, so nothing farther from an edge than `BANK_MAX` depends
/// on where that edge runs.
const LAKE_DEPTH: f32 = 2.;
const LAKE_SLOPE: f32 = 0.4;
const LAKE_BANK: f32 = 4.;
/// Side of the buckets shore edges are filed in, metres.
const BUCKET: f32 = 8.;

/// Height of a bank `x` metres from the water's edge above the edge, for
/// banks that ease in over about `ease` metres.
fn bank(x: f32, ease: f32) -> f32 {
    BANK_SLOPE * x * x / (x + ease.max(0.5))
}

/// A lake's shore, the closed polygon through its samples (edge `k` runs
/// from point `k` to the next), with its edges filed by bucket for
/// nearest-edge lookups out to `reach` metres.
pub(super) struct Shore {
    poly: Vec<[f32; 2]>,
    lo: [f32; 2],
    hi: [f32; 2],
    reach: f32,
    origin: [f32; 2],
    size: [usize; 2],
    /// Edges per bucket, compressed rows.
    starts: Vec<u32>,
    edges: Vec<u32>,
}

impl Shore {
    pub fn new(samples: &[Sample], reach: f32) -> Option<Self> {
        let poly: Vec<[f32; 2]> = samples.iter().map(|s| [s.p[0], s.p[2]]).collect();
        let n = poly.len();
        if n == 0 {
            return None;
        }
        let (mut lo, mut hi) = ([f32::MAX; 2], [f32::MIN; 2]);
        for p in &poly {
            lo = [lo[0].min(p[0]), lo[1].min(p[1])];
            hi = [hi[0].max(p[0]), hi[1].max(p[1])];
        }
        let origin = [lo[0] - reach, lo[1] - reach];
        let size = [0, 1].map(|a| ((hi[a] - lo[a] + 2. * reach) / BUCKET).ceil() as usize + 1);
        let bucket =
            |v: f32, a: usize| (((v - origin[a]) / BUCKET).max(0.) as usize).min(size[a] - 1);
        // Two passes: count per bucket, then fill.
        let each = |fill: &mut dyn FnMut(usize, u32)| {
            for k in 0..n {
                let (a, b) = (poly[k], poly[(k + 1) % n]);
                for z in bucket(a[1].min(b[1]) - reach, 1)..=bucket(a[1].max(b[1]) + reach, 1) {
                    for x in bucket(a[0].min(b[0]) - reach, 0)..=bucket(a[0].max(b[0]) + reach, 0) {
                        fill(z * size[0] + x, k as u32);
                    }
                }
            }
        };
        let mut counts = vec![0u32; size[0] * size[1] + 1];
        each(&mut |i, _| counts[i + 1] += 1);
        for i in 1..counts.len() {
            counts[i] += counts[i - 1];
        }
        let starts = counts.clone();
        let mut edges = vec![0u32; *counts.last().unwrap_or(&0) as usize];
        let mut next = counts;
        each(&mut |i, k| {
            edges[next[i] as usize] = k;
            next[i] += 1;
        });
        Some(Self {
            poly,
            lo,
            hi,
            reach,
            origin,
            size,
            starts,
            edges,
        })
    }

    fn edge(&self, k: usize) -> ([f32; 2], [f32; 2]) {
        (self.poly[k], self.poly[(k + 1) % self.poly.len()])
    }

    /// The nearest edge to `p` among those `keep` accepts, with its
    /// distance. Exact whenever that edge is within `reach`; past it, any
    /// distance returned is at least the true one.
    fn nearest(&self, p: [f32; 2], keep: impl Fn(usize) -> bool) -> Option<(f32, usize)> {
        let b = [0, 1].map(|a| (p[a] - self.origin[a]) / BUCKET);
        if b[0] < 0. || b[1] < 0. || b[0] as usize >= self.size[0] || b[1] as usize >= self.size[1]
        {
            return None;
        }
        let i = b[1] as usize * self.size[0] + b[0] as usize;
        let mut best: Option<(f32, usize)> = None;
        for &k in &self.edges[self.starts[i] as usize..self.starts[i + 1] as usize] {
            let k = k as usize;
            if !keep(k) {
                continue;
            }
            let (a, c) = self.edge(k);
            let d = segment_distance(p, a, c);
            if best.is_none_or(|(bd, _)| d < bd) {
                best = Some((d, k));
            }
        }
        best
    }

    /// Where the row at `z` crosses the shore, sorted: a point on the row
    /// is inside when an odd number lie past it (the even-odd rule, as
    /// `river::inside` tests it).
    fn crossings(&self, z: f32, out: &mut Vec<f32>) {
        out.clear();
        for k in 0..self.poly.len() {
            let (a, b) = self.edge(k);
            if (a[1] > z) != (b[1] > z) {
                out.push(a[0] + (z - a[1]) / (b[1] - a[1]) * (b[0] - a[0]));
            }
        }
        out.sort_by(f32::total_cmp);
    }

    pub fn inside(&self, p: [f32; 2]) -> bool {
        if p[0] < self.lo[0] || p[0] > self.hi[0] || p[1] < self.lo[1] || p[1] > self.hi[1] {
            return false;
        }
        let mut xs = Vec::new();
        self.crossings(p[1], &mut xs);
        odd_past(&xs, p[0])
    }

    /// Calls `f` with every lattice point inside the shore or within
    /// `reach` of it (no more than the filing's), and in `region` if one
    /// is given: (point, position, inside, distance to the shore, exact out
    /// to the filing's reach). Without `reach`, only points inside, with
    /// no distance measured (0).
    fn visit(
        &self,
        lattice: Lattice,
        reach: Option<f32>,
        region: Option<&Region>,
        mut f: impl FnMut((usize, usize), [f32; 2], bool, f32),
    ) {
        assert!(
            reach.is_none_or(|r| r <= self.reach),
            "visited past the shore's filing"
        );
        let measure = reach.is_some();
        let reach = reach.unwrap_or(0.);
        let (lo, hi) = (
            [self.lo[0] - reach, self.lo[1] - reach],
            [self.hi[0] + reach, self.hi[1] + reach],
        );
        let xs = lattice.span(0, lo[0], hi[0]);
        let mut crossings = Vec::new();
        let mut spans = Vec::new();
        for z in lattice.span(1, lo[1], hi[1]) {
            spans.clear();
            match region {
                Some(r) => r.spans(lattice, z, &mut spans),
                None => spans.push(xs.clone()),
            }
            if spans.is_empty() {
                continue;
            }
            let pz = lattice.at(0, z)[1];
            self.crossings(pz, &mut crossings);
            for span in &spans {
                for x in span.start.max(xs.start)..span.end.min(xs.end) {
                    let p = lattice.at(x, z);
                    let inside = odd_past(&crossings, p[0]);
                    if !inside && !measure {
                        continue;
                    }
                    let d = match measure {
                        true => self.nearest(p, |_| true).map_or(f32::MAX, |(d, _)| d),
                        false => 0.,
                    };
                    if inside || d <= reach {
                        f((x, z), p, inside, d);
                    }
                }
            }
        }
    }
}

/// Whether an odd number of the sorted `crossings` lie past `x`.
fn odd_past(crossings: &[f32], x: f32) -> bool {
    (crossings.len() - crossings.partition_point(|&c| c <= x)) % 2 == 1
}

/// One bit per shore edge.
#[derive(Clone, Debug, Default)]
pub(super) struct Bits(Vec<u64>);

impl Bits {
    fn new(n: usize) -> Self {
        Self(vec![0; n.div_ceil(64)])
    }

    fn set(&mut self, i: usize) {
        self.0[i / 64] |= 1 << (i % 64);
    }

    pub fn get(&self, i: usize) -> bool {
        self.0.get(i / 64).is_some_and(|w| w >> (i % 64) & 1 == 1)
    }

    fn any(&self) -> bool {
        self.0.iter().any(|&w| w != 0)
    }
}

/// Boxes (x0, z0, x1, z1 metres) where an outline edit can change the
/// lake's basin, banks or cells; everywhere else a whole carve leaves
/// exactly what was there.
#[derive(Clone, Debug, Default)]
pub(super) struct Region {
    boxes: Vec<[f32; 4]>,
}

impl Region {
    pub fn is_empty(&self) -> bool {
        self.boxes.is_empty()
    }

    pub fn contains(&self, p: [f32; 2]) -> bool {
        (self.boxes.iter()).any(|b| b[0] <= p[0] && p[0] <= b[2] && b[1] <= p[1] && p[1] <= b[3])
    }

    /// The lattice columns row `z` has in the region, merged.
    fn spans(&self, lattice: Lattice, z: usize, out: &mut Vec<Range<usize>>) {
        let pz = lattice.at(0, z)[1];
        let start = out.len();
        for b in self.boxes.iter().filter(|b| b[1] <= pz && pz <= b[3]) {
            let xs = lattice.span(0, b[0], b[2]);
            if !xs.is_empty() {
                out.push(xs);
            }
        }
        out[start..].sort_by_key(|r| r.start);
        let mut merged: Vec<Range<usize>> = Vec::new();
        for r in out.drain(start..) {
            match merged.last_mut() {
                Some(last) if r.start <= last.end => last.end = last.end.max(r.end),
                _ => merged.push(r),
            }
        }
        out.extend(merged);
    }
}

/// What an edit changed of a lake's outline: the shore edges of curve
/// segments not stored as they are now, on the old shore and the new, and
/// the region that can change with them.
///
/// Segments both curves hold between the same two points are sampled
/// identically, so the shores share those edges exactly. Where shores
/// differ, the old and new changed edges together close loops; a point
/// outside every loop's box is inside both shores or neither, and a point
/// more than `BANK_MAX` from every changed edge measures the same nearest
/// edge on both (or none that the carve reads). So past the boxes padded
/// by `BANK_MAX` the basin a whole carve makes is the one already there.
pub(super) struct Edit {
    pub old: Bits,
    pub new: Bits,
    pub region: Region,
}

impl Edit {
    pub fn new(old: &[PathPoint], new: &[PathPoint], old_shore: &Shore, new_shore: &Shore) -> Self {
        let (old_bits, mut boxes) = changed_edges(old, new, old_shore);
        let (new_bits, more) = changed_edges(new, old, new_shore);
        boxes.extend(more);
        // Runs that share a point belong to one loop: merge their boxes.
        let mut merged: Vec<[f32; 4]> = Vec::new();
        for mut b in boxes {
            while let Some(i) = merged
                .iter()
                .position(|m| m[0] <= b[2] && b[0] <= m[2] && m[1] <= b[3] && b[1] <= m[3])
            {
                let m = merged.swap_remove(i);
                b = [
                    b[0].min(m[0]),
                    b[1].min(m[1]),
                    b[2].max(m[2]),
                    b[3].max(m[3]),
                ];
            }
            merged.push(b);
        }
        // One metre more than the carve reads, for the fill's rays.
        let pad = BANK_MAX + 1.;
        let boxes = (merged.into_iter())
            .map(|b| [b[0] - pad, b[1] - pad, b[2] + pad, b[3] + pad])
            .collect();
        Self {
            old: old_bits,
            new: new_bits,
            region: Region { boxes },
        }
    }
}

/// The edges of `shore` (sampled from `curve`) whose segment `other` does
/// not hold between the same two points, and a box around each run of
/// them.
fn changed_edges(curve: &[PathPoint], other: &[PathPoint], shore: &Shore) -> (Bits, Vec<[f32; 4]>) {
    let same = |a: &PathPoint, b: &PathPoint| a.p == b.p && a.w == b.w && a.i == b.i && a.o == b.o;
    // `other`'s points sorted by position, to find each point's match.
    let key = |p: &PathPoint| p.p.map(f32::to_bits);
    let mut sorted: Vec<u32> = (0..other.len() as u32).collect();
    sorted.sort_by_key(|&j| key(&other[j as usize]));
    let m = other.len();
    let kept = |a: &PathPoint, b: &PathPoint| {
        let first = sorted.partition_point(|&j| key(&other[j as usize]) < key(a));
        (sorted[first..].iter())
            .take_while(|&&j| key(&other[j as usize]) == key(a))
            .any(|&j| {
                let j = j as usize;
                same(&other[j], a) && same(&other[(j + 1) % m], b)
            })
    };
    let n = curve.len();
    let mut bits = Bits::new(shore.poly.len());
    let mut e = 0;
    for k in 0..n {
        let (a, b) = (&curve[k], &curve[(k + 1) % n]);
        let count = steps(a, b);
        if !kept(a, b) {
            for i in e..e + count {
                bits.set(i);
            }
        }
        e += count;
    }
    debug_assert_eq!(e, shore.poly.len(), "a sample per step");
    // Runs of changed edges, wrapping past the last.
    let len = shore.poly.len();
    let mut boxes = Vec::new();
    if bits.any() {
        let first = (0..len).find(|&i| !bits.get(i)).unwrap_or(0);
        let mut run: Option<[f32; 4]> = None;
        for step in 1..=len {
            let i = (first + step) % len;
            if bits.get(i) {
                let (a, b) = shore.edge(i);
                let r = run.get_or_insert([a[0], a[1], a[0], a[1]]);
                for p in [a, b] {
                    *r = [
                        r[0].min(p[0]),
                        r[1].min(p[1]),
                        r[2].max(p[0]),
                        r[3].max(p[1]),
                    ];
                }
            } else if let Some(r) = run.take() {
                boxes.push(r);
            }
        }
        boxes.extend(run);
    }
    (bits, boxes)
}

/// Lowers a basin under a lake at `level`, deepening away from the shore,
/// and cuts its banks; in `region` only, if given. Only ever lowers, so
/// carving again changes nothing.
pub(super) fn carve(field: &mut Heightfield, shore: &Shore, level: f32, region: Option<&Region>) {
    let lattice = Lattice::heights(field);
    let edge = level - EDGE_DEPTH;
    shore.visit(lattice, Some(BANK_MAX), region, |(x, z), _, inside, d| {
        let i = field.grid.index(x, z);
        let target = if inside {
            level - LAKE_DEPTH.min(EDGE_DEPTH + d * LAKE_SLOPE)
        } else {
            edge + bank(d, LAKE_BANK)
        };
        if target < field.heights[i] {
            field.heights[i] = target;
        }
    });
}

/// Fills what an edit moved the lake away from, so a shrunk or moved lake
/// leaves no empty basin behind: ground inside `old` but not `new`, and
/// the old banks cut past changed edges, rises (never falls) to the ground
/// just past where the old carve stopped cutting, along each changed
/// edge's outward normal. The ground from before the first carve is kept
/// nowhere; this is the best guess level ground allows. Carve the new
/// shore after.
pub(super) fn fill(field: &mut Heightfield, old: &Shore, level: f32, new: &Shore, edit: &Edit) {
    let n = old.poly.len();
    let changed: Vec<usize> = (0..n).filter(|&k| edit.old.get(k)).collect();
    if changed.is_empty() {
        return;
    }
    let before = field.clone();
    let edge = level - EDGE_DEPTH;
    // Outward normals: the inside lies left of each edge on a
    // counter-clockwise shore (x right, z up), right on a clockwise one.
    let area: f32 = (0..n)
        .map(|k| {
            let (a, b) = old.edge(k);
            a[0] * b[1] - b[0] * a[1]
        })
        .sum();
    let turn = if area >= 0. { 1. } else { -1. };
    // Per changed edge: how far out the old carve cut, and the ground there.
    let mut ground = vec![(0f32, 0f32); n];
    for &k in &changed {
        let (a, b) = old.edge(k);
        let (dx, dz) = (b[0] - a[0], b[1] - a[1]);
        let l = dx.hypot(dz).max(1e-6);
        let out = [turn * dz / l, -turn * dx / l];
        let mid = [(a[0] + b[0]) / 2., (a[1] + b[1]) / 2.];
        let at = |x: f32| before.height([mid[0] + out[0] * x, mid[1] + out[1] * x]);
        // Cut ground stands at the bank's height; uncut ground below it.
        ground[k] = (1..BANK_MAX as u32)
            .map(|x| x as f32)
            .find(|&x| at(x) < edge + bank(x, LAKE_BANK) - 0.05)
            .map_or((BANK_MAX, at(BANK_MAX)), |x| (x, at(x)));
    }
    let lattice = Lattice::heights(field);
    let mut crossings = Vec::new();
    let mut spans = Vec::new();
    let span = |lo: f32, hi: f32, a: usize| lattice.span(a, lo, hi);
    for z in span(old.lo[1] - BANK_MAX, old.hi[1] + BANK_MAX, 1) {
        spans.clear();
        edit.region.spans(lattice, z, &mut spans);
        if spans.is_empty() {
            continue;
        }
        let pz = lattice.at(0, z)[1];
        old.crossings(pz, &mut crossings);
        let olds = crossings.clone();
        new.crossings(pz, &mut crossings);
        for x in spans.iter().flat_map(|s| s.clone()) {
            let p = lattice.at(x, z);
            if odd_past(&crossings, p[0]) {
                continue;
            }
            let target = if odd_past(&olds, p[0]) {
                // Left by the lake: the nearest changed edge's ground.
                let k = old
                    .nearest(p, |k| edit.old.get(k))
                    .map(|(_, k)| k)
                    .or_else(|| {
                        (changed.iter().copied()).min_by(|&a, &b| {
                            let d = |k| {
                                let (a, b) = old.edge(k);
                                segment_distance(p, a, b)
                            };
                            d(a).total_cmp(&d(b))
                        })
                    })
                    .expect("changed edges");
                ground[k].1
            } else {
                // An old bank, if a changed edge cut it.
                match old.nearest(p, |_| true) {
                    Some((d, k)) if edit.old.get(k) && d < ground[k].0 => ground[k].1,
                    _ => continue,
                }
            };
            let i = field.grid.index(x, z);
            field.heights[i] = field.heights[i].max(target);
        }
    }
}

/// Sets or clears River and Water inside a lake (as `river::paint_river`
/// does along a river), in `region` only if given.
pub(super) fn paint(grids: &mut Grids, shore: &Shore, on: bool, region: Option<&Region>) {
    for kind in [Kind::River, Kind::Water, Kind::Ford] {
        if let Some(layer) = grids.layer_mut(kind) {
            shore.visit(Lattice::cells(layer), None, region, |(x, y), _, _, _| {
                mark(layer, x, y, on);
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::{Draft, Knot, samples};
    use super::*;
    use gpui_editor_kit::heightfield::Grid;

    /// 160 m square of rolling ground, 6-10 m up.
    fn hills() -> Heightfield {
        let grid = Grid {
            width: 321,
            depth: 321,
            spacing: 0.5,
            origin: [0.; 2],
        };
        let heights = (0..grid.len())
            .map(|i| {
                let (x, z) = ((i % grid.width) as f32 * 0.5, (i / grid.width) as f32 * 0.5);
                8. + 1.5 * (x * 0.07).sin() + (z * 0.05 + 1.).cos()
            })
            .collect();
        Heightfield::new(grid, heights)
    }

    /// A lake of `n` points round (80, 80), `r` metres out.
    fn ring(n: usize, r: f32) -> Draft {
        let knots = (0..n)
            .map(|k| {
                let a = k as f32 / n as f32 * std::f32::consts::TAU;
                Knot {
                    at: [80. + a.cos() * r, 80. + a.sin() * r],
                    width: 0.,
                    surface: Some(6.),
                    flow: None,
                    tangents: None,
                }
            })
            .collect();
        Draft {
            id: Some(1),
            lake: true,
            knots,
            picked: None,
        }
    }

    /// The draft reopened from `curve`, every point a knot, as the editor
    /// opens a written lake.
    fn reopened(curve: &[PathPoint]) -> Draft {
        Draft {
            id: Some(1),
            lake: true,
            knots: (curve.iter())
                .map(|q| Knot {
                    at: [q.p[0], q.p[2]],
                    width: 0.,
                    surface: Some(q.p[1]),
                    flow: None,
                    tangents: q.i.zip(q.o),
                })
                .collect(),
            picked: None,
        }
    }

    fn shore(curve: &[PathPoint]) -> Shore {
        Shore::new(&samples(curve, true), BANK_MAX).unwrap()
    }

    // The filed shore finds the same cells and basin as testing every edge
    // from every point, as lakes were carved before.
    #[test]
    fn filed_shore_carves_as_every_edge_does() {
        let mut field = hills();
        let curve = ring(9, 30.).curve(&field);
        let s = samples(&curve, true);
        let poly: Vec<[f32; 2]> = s.iter().map(|s| [s.p[0], s.p[2]]).collect();
        let mut slow = field.clone();
        let lattice = Lattice::heights(&slow);
        let level = curve[0].p[1];
        for z in 0..lattice.size[1] {
            for x in 0..lattice.size[0] {
                let p = lattice.at(x, z);
                let inside = super::super::inside(&poly, p);
                let d = (0..poly.len())
                    .map(|k| segment_distance(p, poly[k], poly[(k + 1) % poly.len()]))
                    .fold(f32::MAX, f32::min);
                if !inside && d > BANK_MAX {
                    continue;
                }
                let i = slow.grid.index(x, z);
                let target = if inside {
                    level - LAKE_DEPTH.min(EDGE_DEPTH + d * LAKE_SLOPE)
                } else {
                    level - EDGE_DEPTH + bank(d, LAKE_BANK)
                };
                slow.heights[i] = slow.heights[i].min(target);
            }
        }
        carve(&mut field, &shore(&curve), level, None);
        assert!(field.heights == slow.heights);
    }

    // Moving a few points of a reopened lake changes the ground only in
    // the edit's region, and exactly as carving the edited lake whole on
    // the same (filled) ground does.
    #[test]
    fn an_edited_lake_carves_as_a_whole_carve_does() {
        let mut field = hills();
        let first = ring(40, 30.).curve(&field);
        carve(&mut field, &shore(&first), first[0].p[1], None);
        let carved = field.clone();
        let mut d = reopened(&first);
        let n = d.knots.len();
        let start: Vec<[f32; 2]> = d.knots.iter().map(|k| k.at).collect();
        // Out on one side, in on the other.
        let (out, inn) = (n / 10, n / 10 + n / 2);
        for (k, push) in [
            (out, 6.),
            (out + 1, 7.),
            (out + 2, 5.),
            (inn, -9.),
            (inn + 1, -10.),
        ] {
            let at = start[k];
            let (dx, dz) = (at[0] - 80., at[1] - 80.);
            let l = dx.hypot(dz);
            d.knots[k].at = [at[0] + dx / l * push, at[1] + dz / l * push];
            for j in [k - 1, k, k + 1] {
                d.knots[j].tangents = None;
            }
        }
        let curve = d.curve(&field);
        assert_eq!(curve[0].p[1], first[0].p[1], "same level");
        let (old, new) = (shore(&first), shore(&curve));
        let edit = Edit::new(&first, &curve, &old, &new);
        assert_eq!(edit.region.boxes.len(), 2, "{:?}", edit.region);
        fill(&mut field, &old, first[0].p[1], &new, &edit);
        let filled = field.clone();
        let mut whole = filled.clone();
        carve(&mut whole, &new, curve[0].p[1], None);
        carve(&mut field, &new, curve[0].p[1], Some(&edit.region));
        assert!(
            field.heights == whole.heights,
            "incremental differs from whole"
        );
        let lattice = Lattice::heights(&field);
        for z in 0..lattice.size[1] {
            for x in 0..lattice.size[0] {
                let i = field.grid.index(x, z);
                if !edit.region.contains(lattice.at(x, z)) {
                    assert_eq!(field.heights[i].to_bits(), carved.heights[i].to_bits());
                }
            }
        }
        // Where the shore moved in, the old basin filled.
        let at = start[inn];
        let inward = [80. + (at[0] - 80.) * 0.93, 80. + (at[1] - 80.) * 0.93];
        assert!(!new.inside(inward) && old.inside(inward));
        assert!(
            field.height(inward) > carved.height(inward) + 1.,
            "filled from {} to {}",
            carved.height(inward),
            field.height(inward)
        );
    }

    // Nothing moved: no region, nothing to carve.
    #[test]
    fn an_unchanged_lake_has_no_region() {
        let field = hills();
        let curve = ring(12, 25.).curve(&field);
        let s = shore(&curve);
        let edit = Edit::new(&curve, &curve, &s, &s);
        assert!(edit.region.is_empty() && !edit.old.any() && !edit.new.any());
    }
}
