// Rivers and lakes: the River tool's curve drafts, and committing one as a
// single edit (the `River`/`Lake` EditorObject copied from a shipped
// island, a carved bed, River and Water cells, WaterFlow along a river).
// Engine addresses here are Anno117.exe build 24988830 (Steam).
use super::{Drag, IslandEditor, Item, Tool, grids, harbour};
use anno_core::harbour::Piece;
use anno_core::island::objects::{Object, PathPoint};
use anno_core::water;
use anno_formats::a7m::grids::{Grids, Kind, Layer};
use gpui_editor_kit::{
    gizmo,
    gpu::{self, Mesh, MeshData},
    gpui::{Bounds, MouseDownEvent, Pixels, Point, Window, point, px, rgba},
    heightfield::Heightfield,
    math::Vec3,
    paint,
};
use rayon::prelude::*;
use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;

mod bed;
mod lake;
pub(super) use bed::Bed;

/// Width of a new river. Shipped river points are mostly 1–5 m wide.
pub const DEFAULT_WIDTH: f32 = 4.;
/// Drafts are resampled to a point about this often. Shipped rivers have a
/// point every 1–2.6 m, and the game draws long stretches between sparse
/// points as straight water with hard corners.
const SPACING: f32 = 2.;
/// New points sit this far below the ground they are put on, so the water
/// stays between its banks.
const SURFACE_BELOW: f32 = 0.3;
/// Lake shore paint reaches this far onto the banks, fading out.
const BED_PAINT_BANK: f32 = 2.;
/// The preview water reaches this far past a curve's edge, metres. Banks
/// rise above the surface there and cut the shoreline, so it follows the
/// terrain instead of the curve, as in game.
const WATER_OVERLAP: f32 = 1.5;
/// Lake water is filled in cells this size, metres.
const WATER_CELL: f32 = 1.;

/// Bezier tangents are a sixth of the neighbouring chord on every shipped
/// river and lake.
const TANGENT: f32 = 1. / 6.;
/// `u0` of shipped river points: mostly `(1, ~3, 1)`, `(1, 1, 1)` at the
/// mouth. The flow speed at either bank and in the middle (vsClusterFlow
/// reads the vertex's three speeds the same way); it tops out at 8 and
/// rises on steep stretches.
pub(super) const U0: [f32; 3] = [1., 3., 1.];
const U0_MOUTH: [f32; 3] = [1., 1., 1.];

/// One point of a draft curve, in island metres.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Knot {
    pub at: [f32; 2],
    /// River width; lakes ignore it.
    pub width: f32,
    /// Water surface; `None` follows the ground under `at`.
    pub surface: Option<f32>,
    /// Flow speed at the left bank, centre and right bank looking
    /// downstream (`u0`, 0 to [`water::MAX_FLOW_SPEED`]); `None` takes the
    /// shipped default, slowing to `U0_MOUTH` at the mouth.
    pub flow: Option<[f32; 3]>,
    /// The stored in and out tangents of a point reopened from the island
    /// and not moved since: it and stretches between such points are
    /// written back exactly as they were.
    pub tangents: Option<([f32; 3], [f32; 3])>,
}

/// The curve the River tool is drawing or re-editing. Not part of the
/// document: finishing it is the undo step.
#[derive(Clone, Debug, PartialEq)]
pub struct Draft {
    /// The river or lake object being re-edited, if any.
    pub id: Option<u64>,
    /// A closed curve at one height.
    pub lake: bool,
    pub knots: Vec<Knot>,
    /// The knot picked for Delete and dragging.
    pub picked: Option<usize>,
}

impl Draft {
    /// A river or lake reopened with every stored point a knot, so the
    /// curve stays as it is until a point moves.
    fn of(o: &Object, lake: bool) -> Option<Draft> {
        let path = o.path.as_ref()?;
        let knots = path
            .iter()
            .map(|q| Knot {
                at: [q.p[0], q.p[2]],
                width: q.w.unwrap_or(DEFAULT_WIDTH),
                surface: Some(q.p[1]),
                flow: q.u0,
                tangents: q.i.zip(q.o),
            })
            .collect();
        Some(Draft {
            id: Some(o.id),
            lake,
            knots,
            picked: None,
        })
    }

    /// The curve as the object stores it. Untouched reopened points come
    /// back exactly; new and moved ones are resampled between their
    /// neighbours and follow the ground. Ground-following surfaces never
    /// rise downstream, and a new river's first point is its source, as on
    /// shipped rivers; a lake is flat at its lowest point.
    pub fn curve(&self, field: &Heightfield) -> Vec<PathPoint> {
        let mut knots = densify(&self.knots, self.lake);
        let n = knots.len();
        // Ground-following points sit under the lowest ground across the
        // width, so on a sideways slope the low bank stays above the water.
        let ys: Vec<f32> = (0..n)
            .map(|k| {
                knots[k].surface.unwrap_or_else(|| {
                    let (prev, next) =
                        (knots[k.saturating_sub(1)].at, knots[(k + 1).min(n - 1)].at);
                    let (dx, dz) = (next[0] - prev[0], next[1] - prev[1]);
                    let l = dx.hypot(dz);
                    let half = if self.lake { 0. } else { knots[k].width / 2. };
                    let [x, z] = knots[k].at;
                    let ground = if l > 1e-6 {
                        let (nx, nz) = (-dz / l * half, dx / l * half);
                        field
                            .height([x, z])
                            .min(field.height([x + nx, z + nz]))
                            .min(field.height([x - nx, z - nz]))
                    } else {
                        field.height([x, z])
                    };
                    ground - SURFACE_BELOW
                })
            })
            .collect();
        let mut ys = ys;
        if n == 0 {
            return Vec::new();
        }
        if self.lake {
            let level = ys.iter().copied().fold(f32::MAX, f32::min);
            ys.fill(level);
        } else {
            // A reopened river keeps its direction; stored surfaces stay,
            // shipped ones rise a little in places.
            if self.id.is_none() && ys[0] < ys[n - 1] {
                knots.reverse();
                ys.reverse();
            }
            for k in 1..n {
                if self.id.is_none() || knots[k].surface.is_none() {
                    ys[k] = ys[k].min(ys[k - 1]);
                }
            }
        }
        let p: Vec<[f32; 3]> = knots
            .iter()
            .zip(&ys)
            .map(|(k, &y)| [k.at[0], y, k.at[1]])
            .collect();
        let dist = |a: [f32; 3], b: [f32; 3]| {
            ((a[0] - b[0]).powi(2) + (a[1] - b[1]).powi(2) + (a[2] - b[2]).powi(2)).sqrt()
        };
        let neighbours = |k: usize| {
            if self.lake {
                ((k + n - 1) % n, (k + 1) % n)
            } else {
                (k.saturating_sub(1), (k + 1).min(n - 1))
            }
        };
        (0..n)
            .map(|k| {
                let (a, b) = neighbours(k);
                // Stored tangents hold while the point and both neighbours
                // stand where they were.
                if let Some((i, o)) = knots[k].tangents
                    && knots[a].tangents.is_some()
                    && knots[b].tangents.is_some()
                {
                    return PathPoint {
                        p: p[k],
                        i: Some(i),
                        o: Some(o),
                        w: (!self.lake).then_some(knots[k].width),
                        u0: (!self.lake).then(|| knots[k].flow.unwrap_or(U0)),
                        d: None,
                    };
                }
                let (prev, next) = (p[a], p[b]);
                // Catmull-Rom direction; each side's length follows its own
                // chord, and curve ends reuse the one chord they have.
                let span = dist(prev, next).max(1e-6);
                let dir: [f32; 3] = std::array::from_fn(|a| (next[a] - prev[a]) / span);
                let (back, ahead) = (dist(prev, p[k]), dist(p[k], next));
                let (back, ahead) = (
                    if back == 0. { ahead } else { back },
                    if ahead == 0. { back } else { ahead },
                );
                PathPoint {
                    p: p[k],
                    i: Some(dir.map(|v| -v * back * TANGENT)),
                    o: Some(dir.map(|v| v * ahead * TANGENT)),
                    w: (!self.lake).then_some(knots[k].width),
                    u0: (!self.lake).then(|| {
                        knots[k]
                            .flow
                            .unwrap_or(if k == n - 1 { U0_MOUTH } else { U0 })
                    }),
                    d: None,
                }
            })
            .collect()
    }
}

/// The knots resampled every `SPACING` metres along a Catmull-Rom curve
/// through them. Widths and fixed surfaces interpolate; points between two
/// ground-following knots follow the ground too.
fn densify(knots: &[Knot], closed: bool) -> Vec<Knot> {
    let n = knots.len();
    if n < 2 {
        return knots.to_vec();
    }
    let at = |k: isize| -> [f32; 2] {
        let k = if closed {
            k.rem_euclid(n as isize) as usize
        } else {
            k.clamp(0, n as isize - 1) as usize
        };
        knots[k].at
    };
    let segments = if closed { n } else { n - 1 };
    let mut out = Vec::new();
    for k in 0..segments {
        let (a, b) = (&knots[k], &knots[(k + 1) % n]);
        // Between two untouched reopened points, and between points already
        // about `SPACING` apart, there is nothing to add.
        let chord = (b.at[0] - a.at[0]).hypot(b.at[1] - a.at[1]);
        if (a.tangents.is_some() && b.tangents.is_some()) || chord <= 1.5 * SPACING {
            out.push(*a);
            continue;
        }
        let p = [at(k as isize - 1), a.at, b.at, at(k as isize + 2)];
        let eval = |t: f32| -> [f32; 2] {
            let (t2, t3) = (t * t, t * t * t);
            std::array::from_fn(|x| {
                0.5 * (2. * p[1][x]
                    + (p[2][x] - p[0][x]) * t
                    + (2. * p[0][x] - 5. * p[1][x] + 4. * p[2][x] - p[3][x]) * t2
                    + (3. * p[1][x] - p[0][x] - 3. * p[2][x] + p[3][x]) * t3)
            })
        };
        // Steps by length along the curve, not by t: between knots far apart
        // the curve's speed varies, and even t steps leave gaps.
        const FINE: usize = 64;
        let mut lengths = vec![0f32; FINE + 1];
        let mut last = eval(0.);
        for i in 1..=FINE {
            let q = eval(i as f32 / FINE as f32);
            lengths[i] = lengths[i - 1] + (q[0] - last[0]).hypot(q[1] - last[1]);
            last = q;
        }
        let total = lengths[FINE];
        let steps = ((total / SPACING).round() as usize).max(1);
        out.push(*a);
        for s in 1..steps {
            let want = total * s as f32 / steps as f32;
            let i = lengths.partition_point(|&l| l < want).clamp(1, FINE);
            let span = (lengths[i] - lengths[i - 1]).max(1e-6);
            let t = ((i - 1) as f32 + (want - lengths[i - 1]) / span) / FINE as f32;
            let point = eval(t);
            out.push(Knot {
                at: point,
                width: a.width + (b.width - a.width) * t,
                surface: match (a.surface, b.surface) {
                    (Some(ya), Some(yb)) => Some(ya + (yb - ya) * t),
                    _ => None,
                },
                tangents: None,
                // An edited knot's speeds blend into its unedited
                // neighbour's defaults.
                flow: match (a.flow, b.flow) {
                    (None, None) => None,
                    (fa, fb) => {
                        let (fa, fb) = (fa.unwrap_or(U0), fb.unwrap_or(U0));
                        Some(std::array::from_fn(|i| fa[i] + (fb[i] - fa[i]) * t))
                    }
                },
            });
        }
    }
    if !closed {
        out.push(knots[n - 1]);
    }
    out
}

/// A river's or lake's course as its outline draws it, sampled off the UI
/// thread with its surface (`inland_job`).
pub(super) struct Course {
    pub id: u64,
    pub lake: bool,
    samples: Vec<Sample>,
}

/// The courses of `curves` (object id, path, lake).
pub(super) fn courses<'a>(
    curves: impl IntoIterator<Item = (u64, &'a [PathPoint], bool)>,
) -> Vec<Course> {
    (curves.into_iter())
        .map(|(id, path, lake)| Course {
            id,
            lake,
            samples: samples(path, lake),
        })
        .collect()
}

/// A point on the curve: position (y is the water surface), river width and
/// the ground direction of travel.
#[derive(Clone, Copy, Debug)]
struct Sample {
    p: [f32; 3],
    w: f32,
    dir: [f32; 2],
}

/// The stretches of `curve` that differ from `old`: points not stored
/// there as they are now, with the segments either side, by distance along
/// `samples` (the curve's own), feathered over `feather` metres.
fn changed(
    curve: &[PathPoint],
    old: &[PathPoint],
    samples: &[Sample],
    feather: f32,
) -> bed::Stretches {
    // Every segment's samples start exactly on its first point.
    let mut along = Vec::with_capacity(curve.len());
    let (mut s, mut run) = (0, 0.);
    for c in curve {
        while s < samples.len() && samples[s].p != c.p {
            if s + 1 < samples.len() {
                let (a, b) = (samples[s].p, samples[s + 1].p);
                run += (b[0] - a[0]).hypot(b[2] - a[2]);
            }
            s += 1;
        }
        along.push(run);
    }
    let same = |c: &PathPoint| {
        old.iter()
            .any(|q| q.p == c.p && q.w == c.w && q.i == c.i && q.o == c.o)
    };
    let n = curve.len();
    let mut core: Vec<(f32, f32)> = Vec::new();
    for k in (0..n).filter(|&k| !same(&curve[k])) {
        let (a, b) = (along[k.saturating_sub(1)], along[(k + 1).min(n - 1)]);
        match core.last_mut() {
            Some(last) if a <= last.1 => last.1 = last.1.max(b),
            _ => core.push((a, b)),
        }
    }
    bed::Stretches { core, feather }
}

/// The Bezier curve sampled about every metre; closed curves end where
/// they start.
fn samples(curve: &[PathPoint], closed: bool) -> Vec<Sample> {
    let n = curve.len();
    let segments = if closed { n } else { n.saturating_sub(1) };
    let mut out = Vec::new();
    for k in 0..segments {
        let (a, b) = (&curve[k], &curve[(k + 1) % n]);
        let o = a.o.unwrap_or_default();
        let i = b.i.unwrap_or_default();
        let c: [[f32; 3]; 4] = [
            a.p,
            std::array::from_fn(|x| a.p[x] + o[x]),
            std::array::from_fn(|x| b.p[x] + i[x]),
            b.p,
        ];
        let chord = (b.p[0] - a.p[0]).hypot(b.p[2] - a.p[2]);
        let steps = steps(a, b);
        let last = !closed && k + 1 == segments;
        for s in 0..steps + usize::from(last) {
            let t = s as f32 / steps as f32;
            let u = 1. - t;
            let at: [f32; 3] = std::array::from_fn(|x| {
                u * u * u * c[0][x]
                    + 3. * u * u * t * c[1][x]
                    + 3. * u * t * t * c[2][x]
                    + t * t * t * c[3][x]
            });
            let d: [f32; 3] = std::array::from_fn(|x| {
                3. * u * u * (c[1][x] - c[0][x])
                    + 6. * u * t * (c[2][x] - c[1][x])
                    + 3. * t * t * (c[3][x] - c[2][x])
            });
            let l = d[0].hypot(d[2]);
            let dir = if l > 1e-6 {
                [d[0] / l, d[2] / l]
            } else {
                let l = chord.max(1e-6);
                [(b.p[0] - a.p[0]) / l, (b.p[2] - a.p[2]) / l]
            };
            let w = a.w.unwrap_or(0.) * u + b.w.unwrap_or(0.) * t;
            out.push(Sample { p: at, w, dir });
        }
    }
    out
}

/// Samples `samples` takes on the segment from `a` to `b`, about one a
/// metre (an open curve adds its last point).
fn steps(a: &PathPoint, b: &PathPoint) -> usize {
    let chord = (b.p[0] - a.p[0]).hypot(b.p[2] - a.p[2]);
    (chord.ceil() as usize).max(1)
}

/// A regular grid of points: heightfield samples or grid cell centres.
#[derive(Clone, Copy)]
struct Lattice {
    step: f32,
    /// Position of point (0, 0); cell centres sit half a cell in.
    origin: [f32; 2],
    size: [usize; 2],
}

impl Lattice {
    fn heights(field: &Heightfield) -> Lattice {
        let g = field.grid;
        Lattice {
            step: g.spacing,
            origin: g.origin,
            size: [g.width, g.depth],
        }
    }

    fn cells(layer: &Layer) -> Lattice {
        let c = layer.cell_metres;
        Lattice {
            step: c,
            origin: [c / 2., c / 2.],
            size: [layer.width, layer.height],
        }
    }

    fn at(&self, x: usize, z: usize) -> [f32; 2] {
        [
            self.origin[0] + x as f32 * self.step,
            self.origin[1] + z as f32 * self.step,
        ]
    }

    /// Points inside `[lo, hi]` metres on one axis.
    fn span(&self, axis: usize, lo: f32, hi: f32) -> std::ops::Range<usize> {
        let first = ((lo - self.origin[axis]) / self.step).ceil().max(0.) as usize;
        let end = (((hi - self.origin[axis]) / self.step).floor() + 1.).max(0.) as usize;
        first.min(self.size[axis])..end.min(self.size[axis])
    }

    fn within(&self, lo: [f32; 2], hi: [f32; 2]) -> impl Iterator<Item = (usize, usize)> + use<> {
        let (xs, zs) = (self.span(0, lo[0], hi[0]), self.span(1, lo[1], hi[1]));
        zs.flat_map(move |z| xs.clone().map(move |x| (x, z)))
    }
}

/// The curve as seen from one lattice point.
#[derive(Clone, Copy)]
struct Near {
    /// Ground distance to the centre line.
    d: f32,
    dir: [f32; 2],
}

/// Every lattice point within `reach(width)` of a river's centre line, with
/// the nearest place on it.
fn near_river(
    samples: &[Sample],
    lattice: Lattice,
    reach: impl Fn(f32) -> f32,
) -> HashMap<(usize, usize), Near> {
    let mut out: HashMap<(usize, usize), Near> = HashMap::new();
    for pair in samples.windows(2) {
        let (a, b) = (pair[0], pair[1]);
        let r = reach(a.w.max(b.w));
        let lo = [a.p[0].min(b.p[0]) - r, a.p[2].min(b.p[2]) - r];
        let hi = [a.p[0].max(b.p[0]) + r, a.p[2].max(b.p[2]) + r];
        let (dx, dz) = (b.p[0] - a.p[0], b.p[2] - a.p[2]);
        let l2 = dx * dx + dz * dz;
        for cell in lattice.within(lo, hi) {
            let [x, z] = lattice.at(cell.0, cell.1);
            let t = if l2 > 0. {
                (((x - a.p[0]) * dx + (z - a.p[2]) * dz) / l2).clamp(0., 1.)
            } else {
                0.
            };
            let d = (x - a.p[0] - t * dx).hypot(z - a.p[2] - t * dz);
            let width = a.w + (b.w - a.w) * t;
            if d > reach(width) || out.get(&cell).is_some_and(|n| n.d <= d) {
                continue;
            }
            out.insert(cell, Near { d, dir: a.dir });
        }
    }
    out
}

/// River, Water and Ford for one water cell: setting adds River and Water,
/// clearing drops River and Ford. Keeps `Ford ⊆ River ⊆ Water`; Water
/// stays on clear, the cell is still island ground.
fn mark(layer: &mut Layer, x: usize, y: usize, on: bool) {
    match (layer.kind, on) {
        (Kind::River, _) | (Kind::Water, true) | (Kind::Ford, false) => layer.set(x, y, on as u32),
        _ => {}
    }
}

/// Sets (`on`) or clears a river's cells, and its WaterFlow current.
fn paint_river(grids: &mut Grids, samples: &[Sample], on: bool) {
    for kind in [Kind::River, Kind::Water, Kind::Ford] {
        if let Some(layer) = grids.layer_mut(kind) {
            for ((x, y), _) in near_river(samples, Lattice::cells(layer), |w| w / 2.) {
                mark(layer, x, y, on);
            }
        }
    }
    // Shipped rivers leave WaterFlow still; we write
    // the current anyway so the game has it if it reads it. Cells are 2 m,
    // so reach half a cell past the edge to catch thin rivers.
    if let Some(flow) = grids.layer_mut(Kind::WaterFlow) {
        let cell = flow.cell_metres;
        for ((x, y), n) in near_river(samples, Lattice::cells(flow), |w| w / 2. + cell / 2.) {
            let v = if on {
                [n.dir[0] * grids::FLOW_SPEED, n.dir[1] * grids::FLOW_SPEED]
            } else {
                [0.; 2]
            };
            flow.set(x, y, grids::with_flow(flow.get(x, y), v));
        }
    }
}

/// Distance from `p` to the segment `a`-`b`.
fn segment_distance(p: [f32; 2], a: [f32; 2], b: [f32; 2]) -> f32 {
    let (dx, dz) = (b[0] - a[0], b[1] - a[1]);
    let l2 = dx * dx + dz * dz;
    let t = if l2 > 0. {
        (((p[0] - a[0]) * dx + (p[1] - a[1]) * dz) / l2).clamp(0., 1.)
    } else {
        0.
    };
    (p[0] - a[0] - t * dx).hypot(p[1] - a[1] - t * dz)
}

/// Even-odd test: is `p` inside the closed polygon?
fn inside(poly: &[[f32; 2]], p: [f32; 2]) -> bool {
    let n = poly.len();
    let mut on = false;
    for k in 0..n {
        let (a, b) = (poly[k], poly[(k + 1) % n]);
        if (a[1] > p[1]) != (b[1] > p[1])
            && p[0] < a[0] + (p[1] - a[1]) / (b[1] - a[1]) * (b[0] - a[0])
        {
            on = !on;
        }
    }
    on
}

fn polygon(samples: &[Sample]) -> Vec<[f32; 2]> {
    samples.iter().map(|s| [s.p[0], s.p[2]]).collect()
}

/// The nearest water at one corner of the water lattice: how far past its
/// edge the corner lies (negative on the water), its surface, which curve
/// it is, and how it flows there.
#[derive(Clone, Copy, Debug)]
struct Wet {
    past: f32,
    y: f32,
    curve: usize,
    flow: [f32; 2],
    /// `u0` across the river at this corner.
    speed: f32,
    /// 1 on the water, falling to 0 at a river's start or end where its
    /// record fades it (vsRiver's Alpha).
    fade: f32,
    /// The same at every river's ends (psClusterFlow's).
    flow_fade: f32,
}

/// `RenderFlags` bits of a `River` record (rd3d::InlandWaterFlag): the
/// water fades in over its first stretch, and out over its last. A record
/// without the field takes both (the engine's default 3); shipped rivers
/// that run into the sea store only the end.
pub(super) const FADE_AT_START: u32 = 1;
pub(super) const FADE_AT_END: u32 = 2;
/// IWF_REVERSE: the river flows from its last point to its first. Only
/// lava sets it in shipped islands: the continental island's streams from
/// the volcano's top are stored from the sea up.
pub(super) const REVERSE: u32 = 8;

/// IWF_LAVA: the river is molten rock, which 117 draws as its own strip
/// (anno117 water's fs_lava) rather than on the water lattice.
pub(super) const LAVA: u32 = 4;
/// Rows the game makes of each stretch of a lava river (Anno117.exe
/// 0x142440B10 cuts each into fifths before building the strip), so its
/// ends fade over a fifth of their last stretch.
const LAVA_ROWS: usize = 5;
/// Columns a lava strip is cut into from bank to bank: its surface bows
/// (dsRiverLava, which tessellates the strip), and eight columns keep the
/// bow within 2 cm of the curve.
const LAVA_COLUMNS: usize = 8;

/// One row of a river's strip as the game builds its InlandWaterVertex
/// mesh (Anno117.exe 0x142440B10): the two shore vertices at a path point,
/// straight to the next row, with the point's surface, flow direction and
/// `u0`.
#[derive(Clone, Copy)]
struct Row {
    /// By shore index: 1 is the game's IVF_IS_RIGHT_SHORE, which it puts
    /// at the point less half the width times (dir Z, -dir X) as the
    /// points are stored.
    shores: [[f32; 2]; 2],
    y: f32,
    dir: [f32; 2],
    u0: [f32; 3],
    fade: f32,
    /// 0 at either end whatever the record says (the flow map's fade).
    flow_fade: f32,
}

/// The rows of a river through `curve`, from where it starts flowing, as
/// the game builds them: one per path point, straight ribbons with corners
/// at the points, not the Bezier curve the bed is carved along. `flags`
/// are the record's RenderFlags: which ends fade, whether it flows
/// backwards (the game then builds from the last point, so the fades
/// follow the flow) and whether it is lava (each stretch cut into
/// [`LAVA_ROWS`] rows).
fn strip(curve: &[PathPoint], flags: u32) -> Vec<Row> {
    struct Point {
        p: [f32; 3],
        w: f32,
        u0: [f32; 3],
        /// From the in handle to the out handle, as stored.
        handles: Option<[f32; 2]>,
    }
    let unit = |v: [f32; 2]| {
        let l = v[0].hypot(v[1]);
        (l > 1e-6).then(|| [v[0] / l, v[1] / l])
    };
    let lava = flags & LAVA != 0;
    let reverse = flags & REVERSE != 0;
    let mut points: Vec<Point> = curve
        .iter()
        .map(|c| Point {
            p: c.p,
            w: c.w.unwrap_or(0.),
            u0: c.u0.unwrap_or(U0),
            handles: match (c.i, c.o) {
                (Some(i), Some(o)) => unit([o[0] - i[0], o[2] - i[2]]),
                _ => None,
            },
        })
        .collect();
    if lava && points.len() >= 2 {
        let lerp = |a: f32, b: f32, t: f32| a + (b - a) * t;
        let mut cut = Vec::with_capacity((points.len() - 1) * LAVA_ROWS + 1);
        for pair in points.windows(2) {
            let (a, b) = (&pair[0], &pair[1]);
            for k in 0..LAVA_ROWS {
                let t = k as f32 / LAVA_ROWS as f32;
                cut.push(Point {
                    p: [0, 1, 2].map(|i| lerp(a.p[i], b.p[i], t)),
                    w: lerp(a.w, b.w, t),
                    u0: [0, 1, 2].map(|i| lerp(a.u0[i], b.u0[i], t)),
                    handles: None,
                });
            }
        }
        let mut last = points.pop().unwrap();
        last.handles = None;
        cut.push(last);
        points = cut;
    }
    if reverse {
        points.reverse();
    }
    let n = points.len();
    let xz = |k: usize| [points[k].p[0], points[k].p[2]];
    (0..n)
        .map(|k| {
            let c = &points[k];
            let (a, b) = (xz(k.saturating_sub(1)), xz((k + 1).min(n - 1)));
            // Lava rows turn by the mean of the stretches either side, as
            // the game turns every row; water rows by the point's handles,
            // else the chord between its neighbours (the editor's choice;
            // TODO: the game turns water rows by the stretches too).
            let dir = if lava {
                let ins = unit([c.p[0] - a[0], c.p[2] - a[1]]).unwrap_or([0.; 2]);
                let outs = unit([b[0] - c.p[0], b[1] - c.p[2]]).unwrap_or([0.; 2]);
                unit([ins[0] + outs[0], ins[1] + outs[1]])
            } else {
                let flow = if reverse { -1. } else { 1. };
                c.handles.map(|h| h.map(|v| v * flow))
            }
            .or(unit([b[0] - a[0], b[1] - a[1]]))
            .unwrap_or([1., 0.]);
            let half = c.w / 2.;
            let perp = [dir[1], -dir[0]];
            let p = xz(k);
            let plus = [p[0] + perp[0] * half, p[1] + perp[1] * half];
            let minus = [p[0] - perp[0] * half, p[1] - perp[1] * half];
            let (first, last) = (k == 0, k + 1 == n);
            let fade = (first && flags & FADE_AT_START != 0) || (last && flags & FADE_AT_END != 0);
            Row {
                // Reversed, the game swaps which shore it flags, so each
                // bank keeps its index.
                shores: if reverse {
                    [minus, plus]
                } else {
                    [plus, minus]
                },
                y: c.p[1],
                dir,
                u0: c.u0,
                fade: if fade { 0. } else { 1. },
                flow_fade: if first || last { 0. } else { 1. },
            }
        })
        .collect()
}

/// A river's strip for the GPU: its rows cut into `columns` from bank to
/// bank ([`LAVA_COLUMNS`] for lava, whose surface bows; one for water),
/// each vertex with the strip's normal (up from the slope down the river,
/// as the game's), where it lies across the strip, and its fade.
fn strip_mesh(rows: &[Row], look: u32, columns: usize, out: &mut water::Strips) {
    let n = rows.len();
    if n < 2 {
        return;
    }
    let mid = |r: &Row| {
        [
            (r.shores[0][0] + r.shores[1][0]) / 2.,
            r.y,
            (r.shores[0][1] + r.shores[1][1]) / 2.,
        ]
    };
    let unit = |v: [f32; 3]| {
        let l = (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt();
        if l > 1e-6 { v.map(|c| c / l) } else { [0.; 3] }
    };
    let sub = |a: [f32; 3], b: [f32; 3]| [a[0] - b[0], a[1] - b[1], a[2] - b[2]];
    let base = out.vertices.len() as u32;
    for (k, row) in rows.iter().enumerate() {
        // The game's normal: across the row crossed with the strip's slope,
        // the mean of the stretches either side.
        let p = mid(row);
        let ins = if k > 0 {
            unit(sub(p, mid(&rows[k - 1])))
        } else {
            [0.; 3]
        };
        let outs = if k + 1 < n {
            unit(sub(mid(&rows[k + 1]), p))
        } else {
            [0.; 3]
        };
        let down = unit([ins[0] + outs[0], ins[1] + outs[1], ins[2] + outs[2]]);
        let [a, b] = row.shores;
        let across = unit([b[0] - a[0], 0., b[1] - a[1]]);
        let mut normal = unit([
            across[1] * down[2] - across[2] * down[1],
            across[2] * down[0] - across[0] * down[2],
            across[0] * down[1] - across[1] * down[0],
        ]);
        if normal[1] < 0. {
            normal = normal.map(|v| -v);
        }
        if normal[1] <= 0. {
            normal = [0., 1., 0.];
        }
        for c in 0..=columns {
            let t = c as f32 / columns as f32;
            out.vertices.push(water::StripVertex {
                position: [a[0] + (b[0] - a[0]) * t, row.y, a[1] + (b[1] - a[1]) * t],
                look,
                normal,
                fade: row.fade,
                across: t - 0.5,
            });
        }
    }
    let w = columns as u32 + 1;
    for k in 0..n as u32 - 1 {
        for c in 0..columns as u32 {
            let l0 = base + k * w + c;
            let (r0, l1, r1) = (l0 + 1, l0 + w, l0 + w + 1);
            out.indices.extend([l0, r0, l1, r0, r1, l1]);
        }
    }
}

/// The strip's vertices as the GPU interpolates them over one triangle.
#[derive(Clone, Copy)]
struct Vertex {
    at: [f32; 2],
    y: f32,
    dir: [f32; 2],
    shore: f32,
    u0: [f32; 3],
    fade: f32,
    flow_fade: f32,
}

impl Vertex {
    fn of(row: &Row, shore: usize) -> Self {
        Self {
            at: row.shores[shore],
            y: row.y,
            dir: row.dir,
            shore: shore as f32,
            u0: row.u0,
            fade: row.fade,
            flow_fade: row.flow_fade,
        }
    }
}

/// Barycentric weights of `p` in triangle `t`, of the triangle's nearest
/// point when `p` lies outside, and how far outside it is.
fn barycentric(t: &[Vertex; 3], p: [f32; 2]) -> Option<([f32; 3], f32)> {
    let [a, b, c] = t.map(|v| v.at);
    let sub = |u: [f32; 2], v: [f32; 2]| [u[0] - v[0], u[1] - v[1]];
    let cross = |u: [f32; 2], v: [f32; 2]| u[0] * v[1] - u[1] * v[0];
    let area = cross(sub(b, a), sub(c, a));
    if area.abs() < 1e-8 {
        return None;
    }
    let w = [
        cross(sub(b, p), sub(c, p)) / area,
        cross(sub(c, p), sub(a, p)) / area,
        cross(sub(a, p), sub(b, p)) / area,
    ];
    if w.iter().all(|&x| x >= 0.) {
        return Some((w, 0.));
    }
    // Outside: the nearest point on the three edges.
    let mut best = (f32::MAX, [0f32; 3]);
    for (i, j) in [(0, 1), (1, 2), (2, 0)] {
        let (u, v) = ([a, b, c][i], [a, b, c][j]);
        let d = sub(v, u);
        let l2 = d[0] * d[0] + d[1] * d[1];
        let t = if l2 > 0. {
            (((p[0] - u[0]) * d[0] + (p[1] - u[1]) * d[1]) / l2).clamp(0., 1.)
        } else {
            0.
        };
        let q = [u[0] + d[0] * t, u[1] + d[1] * t];
        let dist = (p[0] - q[0]).hypot(p[1] - q[1]);
        if dist < best.0 {
            let mut w = [0.; 3];
            w[i] = 1. - t;
            w[j] = t;
            best = (dist, w);
        }
    }
    Some((best.1, best.0))
}

/// Every river and lake on one lattice of `WATER_CELL` corners, each corner
/// taking the nearest water, so where a river meets a lake, or a wide river
/// bends, there is one surface and no two layers of water overlap.
struct WaterLattice {
    lo: [f32; 2],
    size: [usize; 2],
    corners: Vec<Option<Wet>>,
}

impl WaterLattice {
    /// `curves` with whether each is a lake and, for rivers, their
    /// RenderFlags ([`FADE_AT_START`], [`FADE_AT_END`], [`REVERSE`]).
    fn new(curves: &[(&[PathPoint], bool, u32)]) -> Option<Self> {
        let shapes: Vec<(usize, Vec<Sample>, bool)> = curves
            .iter()
            .enumerate()
            .map(|(i, &(c, lake, _))| (i, samples(c, lake), lake))
            .filter(|(_, s, _)| s.len() >= 2)
            .collect();
        // Each river's strip as two triangles per pair of rows, in the
        // game's triangle strip order (left, right, next left, next right).
        let triangles: Vec<Vec<[Vertex; 3]>> = shapes
            .iter()
            .map(|&(i, _, lake)| {
                let (curve, _, flags) = curves[i];
                if lake {
                    return Vec::new();
                }
                let rows = strip(curve, flags);
                rows.windows(2)
                    .flat_map(|r| {
                        let [l0, r0] = [0, 1].map(|s| Vertex::of(&r[0], s));
                        let [l1, r1] = [0, 1].map(|s| Vertex::of(&r[1], s));
                        [[l0, r0, l1], [r0, l1, r1]]
                    })
                    .collect()
            })
            .collect();
        let (mut lo, mut hi) = ([f32::MAX; 2], [f32::MIN; 2]);
        for s in shapes.iter().flat_map(|(_, s, _)| s) {
            let r = s.w / 2. + WATER_OVERLAP + WATER_CELL;
            lo = [lo[0].min(s.p[0] - r), lo[1].min(s.p[2] - r)];
            hi = [hi[0].max(s.p[0] + r), hi[1].max(s.p[2] + r)];
        }
        // A strip's mitred corners can reach past the curve's samples.
        for v in triangles.iter().flatten().flatten() {
            let r = WATER_OVERLAP + WATER_CELL;
            lo = [lo[0].min(v.at[0] - r), lo[1].min(v.at[1] - r)];
            hi = [hi[0].max(v.at[0] + r), hi[1].max(v.at[1] + r)];
        }
        if shapes.is_empty() {
            return None;
        }
        let lo = lo.map(|v| (v / WATER_CELL).floor() * WATER_CELL);
        let [w, d] = [0, 1].map(|a| ((hi[a] - lo[a]) / WATER_CELL).ceil() as usize + 1);
        let corner =
            |x: usize, z: usize| [lo[0] + x as f32 * WATER_CELL, lo[1] + z as f32 * WATER_CELL];
        let span =
            |v: f32, a: usize, n: usize| (((v - lo[a]) / WATER_CELL).max(0.) as usize).min(n - 1);
        // Lake outlines and their bounds, measured once for every row.
        let outlines: Vec<_> = (shapes.iter())
            .map(|(_, s, lake)| {
                lake.then(|| {
                    let poly = polygon(s);
                    let (mut a, mut b) = ([f32::MAX; 2], [f32::MIN; 2]);
                    for q in &poly {
                        a = [a[0].min(q[0]), a[1].min(q[1])];
                        b = [b[0].max(q[0]), b[1].max(q[1])];
                    }
                    (poly, a, b)
                })
            })
            .collect();
        // Row by row across cores, each row taking the shapes in the same
        // order, so ties go as they did one thread at a time: a lake's
        // corners each measure every edge of its shore, 22 ms of an 1800
        // island's first frame on one thread.
        let mut best: Vec<Option<Wet>> = vec![None; w * d];
        gpui_editor_kit::task::interactive(|| {
            best.par_chunks_mut(w).enumerate().for_each(|(z, row)| {
                let mut offer = |x: usize, wet: Wet| {
                    if wet.past < WATER_OVERLAP && row[x].is_none_or(|b| wet.past < b.past) {
                        row[x] = Some(wet);
                    }
                };
                for (((curve, s, _), outline), tris) in shapes.iter().zip(&outlines).zip(&triangles)
                {
                    if let Some((poly, a, b)) = outline {
                        let r = WATER_OVERLAP;
                        if z < span(a[1] - r, 1, d) || z > span(b[1] + r, 1, d) {
                            continue;
                        }
                        for x in span(a[0] - r, 0, w)..=span(b[0] + r, 0, w) {
                            let c = corner(x, z);
                            let edge = poly
                                .iter()
                                .zip(poly.iter().cycle().skip(1))
                                .map(|(p, q)| segment_distance(c, *p, *q))
                                .fold(f32::MAX, f32::min);
                            // Inside its shore a lake beats any river, whose
                            // surface would stand a little above it as a ledge.
                            let past = if inside(poly, c) { -edge - 1e4 } else { edge };
                            let still = Wet {
                                past,
                                y: s[0].p[1],
                                curve: *curve,
                                flow: [0.; 2],
                                speed: 0.,
                                fade: 1.,
                                flow_fade: 1.,
                            };
                            offer(x, still);
                        }
                    } else {
                        // The strip's triangles, the corner taking what the
                        // GPU interpolates there (vsRiver, vsClusterFlow);
                        // corners just past the banks take the nearest edge.
                        let zc = lo[1] + z as f32 * WATER_CELL;
                        for t in tris {
                            let (mut a, mut b) = ([f32::MAX; 2], [f32::MIN; 2]);
                            for v in t {
                                a = [a[0].min(v.at[0]), a[1].min(v.at[1])];
                                b = [b[0].max(v.at[0]), b[1].max(v.at[1])];
                            }
                            let r = WATER_OVERLAP;
                            if zc < a[1] - r || zc > b[1] + r {
                                continue;
                            }
                            for x in span(a[0] - r, 0, w)..=span(b[0] + r, 0, w) {
                                let c = corner(x, z);
                                let Some((k, outside)) = barycentric(t, c) else {
                                    continue;
                                };
                                if outside > WATER_OVERLAP {
                                    continue;
                                }
                                let mix = |f: &dyn Fn(&Vertex) -> f32| {
                                    k[0] * f(&t[0]) + k[1] * f(&t[1]) + k[2] * f(&t[2])
                                };
                                let shore = mix(&|v| v.shore);
                                // On the water: how far in from the nearer bank,
                                // by the strip's own width here.
                                let width = (0..3)
                                    .map(|i| {
                                        let j = (i + 1) % 3;
                                        (t[i].at[0] - t[j].at[0]).hypot(t[i].at[1] - t[j].at[1])
                                    })
                                    .fold(0f32, f32::max);
                                let past = if outside > 0. {
                                    outside
                                } else {
                                    -(0.5 - (shore - 0.5).abs()) * width.min(1e3)
                                };
                                let dir = [mix(&|v| v.dir[0]), mix(&|v| v.dir[1])];
                                let l = dir[0].hypot(dir[1]);
                                let flow = if l > 1e-6 {
                                    [dir[0] / l, dir[1] / l]
                                } else {
                                    [0.; 2]
                                };
                                // GetFlowSpeed: left bank, centre, right bank.
                                let mask = [
                                    (shore * 2. - 1.).clamp(0., 1.),
                                    1. - 2. * (shore - 0.5).abs(),
                                    (1. - 2. * shore).clamp(0., 1.),
                                ];
                                let speed = (0..3).map(|i| mix(&|v| v.u0[i]) * mask[i]).sum();
                                offer(
                                    x,
                                    Wet {
                                        past,
                                        y: mix(&|v| v.y),
                                        curve: *curve,
                                        flow,
                                        speed,
                                        fade: mix(&|v| v.fade),
                                        flow_fade: mix(&|v| v.flow_fade),
                                    },
                                );
                            }
                        }
                    }
                }
            });
        });
        Some(Self {
            lo,
            size: [w, d],
            corners: best,
        })
    }

    fn corner(&self, i: usize) -> [f32; 2] {
        let w = self.size[0];
        [
            self.lo[0] + (i % w) as f32 * WATER_CELL,
            self.lo[1] + (i / w) as f32 * WATER_CELL,
        ]
    }
}

/// One translucent surface for every river and lake, so they read as water
/// in the viewport without 117's ocean: a stand-in for [`inland`].
pub(super) fn water_mesh(curves: &[(&[PathPoint], bool)]) -> Option<Arc<Mesh>> {
    let curves: Vec<_> = curves.iter().map(|&(c, lake)| (c, lake, 0)).collect();
    let lattice = WaterLattice::new(&curves)?;
    let [w, d] = lattice.size;
    let mut positions: Vec<Vec3> = Vec::new();
    let mut index = vec![u32::MAX; w * d];
    for (i, b) in lattice.corners.iter().enumerate() {
        if let Some(b) = b {
            index[i] = positions.len() as u32;
            let [x, z] = lattice.corner(i);
            positions.push([x, b.y, z]);
        }
    }
    let mut indices = Vec::new();
    for z in 0..d - 1 {
        for x in 0..w - 1 {
            let q = [
                z * w + x,
                z * w + x + 1,
                (z + 1) * w + x + 1,
                (z + 1) * w + x,
            ]
            .map(|i| index[i]);
            if q.iter().all(|&i| i != u32::MAX) {
                indices.extend([0, 2, 1, 0, 3, 2].map(|k| q[k]));
            }
        }
    }
    if indices.is_empty() {
        return None;
    }
    let colors = vec![0xffffff; positions.len()];
    Mesh::new(MeshData {
        positions,
        colors,
        indices,
        material: gpu::Material::Blend(gpu::Blend {
            // Greener and clearer than the sea: shallow fresh water.
            color: 0x2f7d7a,
            opacity: 0.72,
            fade: 1.,
            fresnel: 0.4,
            depth: 0.,
            additive: false,
            unlit: false,
        }),
        ..Default::default()
    })
    .map_err(|e| eprintln!("inland water mesh: {e:#}"))
    .ok()
    .map(Arc::new)
}

/// Rivers and lakes for a game's water pass to shade, each curve with its
/// record's look, on the same lattice as [`water_mesh`]; its corners are
/// the flow map, which measures the water's depth over `ground`. With
/// `strips` every river also comes as its own strip
/// (`water::Water::river_strips`).
pub(super) fn inland(
    curves: &[(&[PathPoint], water::Look)],
    ground: Option<&Heightfield>,
    strips: bool,
) -> Option<Arc<water::Inland>> {
    // RenderFlags is a u32 the record's floats carry bit for bit.
    let flags = |l: &water::Look| {
        l.number("RenderFlags")
            .map_or(FADE_AT_START | FADE_AT_END, f32::to_bits)
    };
    let shapes: Vec<_> = (curves.iter())
        .map(|(c, l)| (*c, l.lake, flags(l)))
        .collect();
    let lattice = WaterLattice::new(&shapes)?;
    let corners = lattice
        .corners
        .iter()
        .map(|c| match c {
            Some(c) => water::Corner {
                y: c.y,
                look: c.curve as u32,
                flow: c.flow,
                speed: c.speed,
                fade: c.fade,
                flow_fade: c.flow_fade,
            },
            None => water::Corner::dry(),
        })
        .collect();
    let mut inland = water::Inland::new(
        lattice.lo,
        WATER_CELL,
        lattice.size.map(|v| v as u32),
        corners,
        curves.iter().map(|(_, l)| l.clone()).collect(),
        ground,
    );
    // Lava flows on the lattice's flow map but draws as its own strips,
    // which keep their coordinates where two streams overlap; with `strips`
    // so does every river, flat from bank to bank as the game's
    // InlandWaterVertex rows.
    for (k, &(curve, lake, flags)) in shapes.iter().enumerate() {
        if lake {
            continue;
        }
        if flags & LAVA != 0 {
            strip_mesh(
                &strip(curve, flags),
                k as u32,
                LAVA_COLUMNS,
                &mut inland.strips,
            );
        } else if strips {
            strip_mesh(&strip(curve, flags), k as u32, 1, &mut inland.strips);
        }
    }
    Some(Arc::new(inland))
}

/// Writes a curve's cells (or clears them) into the grids.
pub(super) fn paint_curve(grids: &mut Grids, curve: &[PathPoint], lake: bool, on: bool) {
    let s = samples(curve, lake);
    if lake {
        if let Some(shore) = lake::Shore::new(&s, 0.) {
            lake::paint(grids, &shore, on, None);
        }
    } else {
        paint_river(grids, &s, on);
    }
}

/// Which handle of a draft knot: the point, or its width (rivers).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Handle {
    Point,
    Width,
}

/// Undo while the River tool drafts: the draft's own edits, one step each
/// (a point added or deleted, a handle dragged), and every draft written,
/// by the document revision its write made, so undoing the write reopens
/// it where the editing left off. Without these, undo walked the document
/// past an open draft and left the draft as it was.
#[derive(Default)]
pub(super) struct DraftHistory {
    /// The draft before each edit; `None` before the first point.
    undo: Vec<Option<Draft>>,
    redo: Vec<Option<Draft>>,
    /// The document revision the steps were taken at: edits anywhere else
    /// end them.
    at: u64,
    written: Vec<(u64, Draft, Vec<Option<Draft>>)>,
    /// The draft as undoing its write reopened it: redo writes it again
    /// while it is unchanged.
    reopened: Option<Draft>,
}

/// Drafts kept for undoing their writes; older writes undo plainly.
const WRITTEN: usize = 32;

impl DraftHistory {
    pub(super) fn close(&mut self) {
        self.undo.clear();
        self.redo.clear();
    }
}

impl IslandEditor {
    /// Records the draft before an edit to it.
    pub(super) fn river_step(&mut self) {
        self.flow_step = None;
        let h = &mut self.river_history;
        if h.at != self.history.revision() {
            h.close();
            h.at = self.history.revision();
        }
        h.undo.push(self.river.clone());
        h.redo.clear();
    }

    /// A handle drag ended; one that moved nothing is no step.
    pub(super) fn river_moved(&mut self) {
        let h = &mut self.river_history;
        let knots = |d: &Option<Draft>| d.as_ref().map(|d| d.knots.clone());
        if h.undo
            .last()
            .is_some_and(|last| knots(last) == knots(&self.river))
        {
            h.undo.pop();
        }
    }

    /// Undo or redo within the draft: `Some` while a draft is open (or
    /// closed by undo and redoable), `None` to undo the document.
    pub(super) fn river_undo(&mut self, redo: bool) -> Option<bool> {
        let h = &mut self.river_history;
        if h.at != self.history.revision() {
            h.close();
        }
        let (from, to) = if redo {
            (&mut h.redo, &mut h.undo)
        } else {
            (&mut h.undo, &mut h.redo)
        };
        if self.river.is_none() && (!redo || from.is_empty()) {
            return None;
        }
        if redo && from.is_empty() && h.reopened.is_some() && h.reopened == self.river {
            return None;
        }
        // An opened river with no edits yet undoes to closed.
        let Some(next) = from.pop().or_else(|| (!redo).then_some(None)) else {
            return Some(false);
        };
        to.push(std::mem::replace(&mut self.river, next));
        Some(true)
    }

    /// After the document undid away from revision `left` or redid onto
    /// the current one: a draft written there reopens, or closes again.
    pub(super) fn river_written(&mut self, left: u64, redo: bool) {
        let revision = self.history.revision();
        let h = &mut self.river_history;
        h.close();
        h.at = revision;
        if redo {
            if h.written.iter().any(|(r, ..)| *r == revision) {
                self.river = None;
            }
        } else if let Some((_, draft, steps)) = h.written.iter().find(|(r, ..)| *r == left) {
            self.river = Some(draft.clone());
            h.undo = steps.clone();
            h.reopened = self.river.clone();
            self.tool = Tool::River;
            return;
        }
        h.reopened = None;
    }

    /// `Some(lake)` for river and lake objects.
    pub(super) fn water_kind(&self, guid: u32) -> Option<bool> {
        water_kind(self.island.harbour.as_deref(), self.assets.as_deref(), guid)
    }

    /// Adds a knot at the end of the draft, or starts one there.
    pub(super) fn river_add(&mut self, at: [f32; 2]) {
        let lake = self.lake_mode;
        self.river_step();
        let draft = self.river.get_or_insert_with(|| Draft {
            id: None,
            lake,
            knots: Vec::new(),
            picked: None,
        });
        let width = draft.knots.last().map_or(DEFAULT_WIDTH, |k| k.width);
        let flow = draft.knots.last().and_then(|k| k.flow);
        if let Some(last) = draft.knots.last_mut() {
            last.tangents = None;
        }
        draft.knots.push(Knot {
            at,
            width,
            surface: None,
            flow,
            tangents: None,
        });
        draft.picked = Some(draft.knots.len() - 1);
    }

    /// Opens a river or lake for editing in the River tool.
    pub(super) fn edit_water(&mut self, id: u64) {
        if self.edit_river(id) {
            self.tool = Tool::River;
            self.say(
                "Editing: drag points and width handles, click to extend, Enter applies, Esc cancels",
                false,
            );
        }
    }

    /// Opens an existing river or lake as the draft.
    pub(super) fn edit_river(&mut self, id: u64) -> bool {
        let Some(o) = self.island.doc.objects.iter().find(|o| o.id == id) else {
            return false;
        };
        let Some(draft) = self.water_kind(o.guid).and_then(|lake| Draft::of(o, lake)) else {
            return false;
        };
        self.river = Some(draft);
        self.river_history.close();
        self.river_history.at = self.history.revision();
        self.selection = BTreeSet::from([Item::Object(id)]);
        true
    }

    /// Press with the River tool: grab a handle, finish on a double click,
    /// open a river under the cursor, or add a point.
    pub(super) fn river_press(
        &mut self,
        view: usize,
        event: &MouseDownEvent,
        at: Vec3,
    ) -> Option<Drag> {
        if let Some((index, handle)) = self.river_handle_at(view, event.position) {
            // One step per drag; `river_moved` drops it if nothing moved.
            self.river_step();
            let d = self.river.as_mut()?;
            d.picked = Some(index);
            let start = d.knots.iter().map(|k| k.at).collect();
            return Some(Drag::RiverKnot {
                index,
                handle,
                start,
            });
        }
        if event.click_count >= 2 && self.river.is_some() {
            // The first click of the pair already added the last point.
            self.finish_river();
            return None;
        }
        if self.river.is_none()
            && let Some(id) = self.water_at(view, event.position)
        {
            self.edit_water(id);
            return None;
        }
        if self.river.as_ref().is_some_and(|d| d.knots.len() >= 2) {
            self.river_insert([at[0], at[2]]);
        } else {
            self.river_add([at[0], at[2]]);
        }
        None
    }

    /// A click with a curve open: on the water a point goes in there,
    /// between its neighbours, at their surface; elsewhere the curve
    /// extends from its nearer end (a lake's nearest edge).
    pub(super) fn river_insert(&mut self, at: [f32; 2]) {
        let Some(d) = &self.river else { return };
        let n = d.knots.len();
        let segments = if d.lake { n } else { n - 1 };
        let (k, t, dist) = (0..segments)
            .map(|k| {
                let (a, b) = (d.knots[k].at, d.knots[(k + 1) % n].at);
                let (dx, dz) = (b[0] - a[0], b[1] - a[1]);
                let l2 = dx * dx + dz * dz;
                let t = if l2 > 0. {
                    (((at[0] - a[0]) * dx + (at[1] - a[1]) * dz) / l2).clamp(0., 1.)
                } else {
                    0.
                };
                (k, t, (at[0] - a[0] - t * dx).hypot(at[1] - a[1] - t * dz))
            })
            .min_by(|x, y| x.2.total_cmp(&y.2))
            .expect("two knots");
        let (a, b) = (d.knots[k], d.knots[(k + 1) % n]);
        let on_water = dist <= (a.width.max(b.width) / 2.).max(2.);
        if !d.lake && !on_water {
            let gap = |p: [f32; 2]| (at[0] - p[0]).hypot(at[1] - p[1]);
            if gap(d.knots[n - 1].at) <= gap(d.knots[0].at) {
                return self.river_add(at);
            }
        }
        self.river_step();
        let Some(d) = self.river.as_mut() else { return };
        let lerp = |x: f32, y: f32| x + (y - x) * t;
        let (index, knot) = if on_water || d.lake {
            let knot = Knot {
                at,
                width: lerp(a.width, b.width),
                surface: a.surface.zip(b.surface).map(|(x, y)| lerp(x, y)),
                flow: a.flow.or(b.flow),
                tangents: None,
            };
            (k + 1, knot)
        } else {
            let first = d.knots[0];
            let knot = Knot {
                at,
                surface: None,
                tangents: None,
                ..first
            };
            (0, knot)
        };
        d.knots.insert(index, knot);
        // Its neighbours now meet it along new chords.
        let len = d.knots.len();
        // A lake's ends meet; a river's do not.
        let neighbours = if d.lake {
            [Some((index + len - 1) % len), Some((index + 1) % len)]
        } else {
            [index.checked_sub(1), (index + 1 < len).then_some(index + 1)]
        };
        for j in neighbours.into_iter().flatten() {
            d.knots[j].tangents = None;
        }
        d.picked = Some(index);
    }

    /// Moves a knot, or sets its width from the cursor's distance.
    pub(super) fn river_drag(
        &mut self,
        index: usize,
        handle: Handle,
        start: &[[f32; 2]],
        at: Vec3,
    ) {
        match handle {
            Handle::Point => self.river_move(index, start, [at[0], at[2]]),
            Handle::Width => {
                if let Some(k) = self.river.as_mut().and_then(|d| d.knots.get_mut(index)) {
                    k.width = (2. * (at[0] - k.at[0]).hypot(at[2] - k.at[1])).clamp(1., 60.);
                }
            }
        }
    }

    /// Puts knot `index` at `to`, its neighbours within the falloff
    /// following by how near along the curve they are, all from `start`
    /// (the knots at the press). Dense reopened curves bend instead of
    /// kinking at one point; moved knots and their neighbours lose their
    /// stored tangents.
    pub(super) fn river_move(&mut self, index: usize, start: &[[f32; 2]], to: [f32; 2]) {
        let Some(follow) = self.river_follow(index, start) else {
            return;
        };
        let Some(d) = self.river.as_mut() else { return };
        let delta = [to[0] - start[index][0], to[1] - start[index][1]];
        for (k, w) in follow.into_iter().enumerate() {
            if w <= 0. {
                continue;
            }
            d.knots[k].at = [start[k][0] + delta[0] * w, start[k][1] + delta[1] * w];
            d.knots[k].tangents = None;
        }
        // Neighbours' tangents point at a moved knot.
        for k in [index.wrapping_sub(1), index + 1] {
            if let Some(knot) = d.knots.get_mut(k) {
                knot.tangents = None;
            }
        }
    }

    /// Sets the water at knot `index` `dy` above where it was at the press
    /// (`surfaces`, every knot's then), its neighbours within the falloff
    /// following as they do a move: one point alone made a step in the
    /// water of a dense reopened river.
    /// A lake's water rises or falls as one.
    pub(super) fn river_raise(
        &mut self,
        index: usize,
        start: &[[f32; 2]],
        surfaces: &[f32],
        dy: f32,
    ) {
        let Some(follow) = self.river_follow(index, start) else {
            return;
        };
        let Some(d) = self.river.as_mut() else { return };
        if surfaces.len() != d.knots.len() {
            return;
        }
        // A lake is flat at its lowest point: the whole of it rises or
        // falls, or raising one point would do nothing at all.
        if d.lake {
            for (k, s) in d.knots.iter_mut().zip(surfaces) {
                k.surface = Some(s + dy);
            }
            return;
        }
        for (k, w) in follow.into_iter().enumerate() {
            if w > 0. {
                d.knots[k].surface = Some(surfaces[k] + dy * w);
            }
        }
    }

    /// Each knot's water at the press of the gizmo: stored, else where
    /// the curve puts it.
    fn river_surfaces(&self) -> Vec<f32> {
        let Some(d) = &self.river else {
            return Vec::new();
        };
        (d.knots.iter())
            .map(|k| {
                k.surface
                    .unwrap_or_else(|| self.island.doc.field.height(k.at) - SURFACE_BELOW)
            })
            .collect()
    }

    /// How far each knot follows a drag of knot `index`: all the way for
    /// it, easing to none at the falloff distance along the curve (from
    /// `start`, the knots at the press). `None` when `start` is stale.
    fn river_follow(&self, index: usize, start: &[[f32; 2]]) -> Option<Vec<f32>> {
        let falloff = self.river_falloff;
        let d = self.river.as_ref()?;
        if index >= start.len() || start.len() != d.knots.len() {
            return None;
        }
        let n = start.len();
        let mut along = vec![0f32; n];
        for k in 1..n {
            along[k] =
                along[k - 1] + (start[k][0] - start[k - 1][0]).hypot(start[k][1] - start[k - 1][1]);
        }
        let total = along[n - 1]
            + if d.lake {
                (start[0][0] - start[n - 1][0]).hypot(start[0][1] - start[n - 1][1])
            } else {
                0.
            };
        let follow = (0..n).map(|k| {
            let mut gap = (along[k] - along[index]).abs();
            if d.lake {
                gap = gap.min(total - gap);
            }
            if k == index {
                1.
            } else if falloff > 0. {
                let t = (gap / falloff).min(1.);
                1. - t * t * (3. - 2. * t)
            } else {
                0.
            }
        });
        Some(follow.collect())
    }

    /// The picked knot on its water (or ground), where the gizmo sits.
    pub(super) fn river_pivot(&self) -> Option<Vec3> {
        if self.tool != Tool::River {
            return None;
        }
        let d = self.river.as_ref()?;
        let k = d.knots.get(d.picked?)?;
        let y = k
            .surface
            .unwrap_or_else(|| self.island.doc.field.height(k.at) - SURFACE_BELOW);
        Some([k.at[0], y, k.at[1]])
    }

    /// A press on the gizmo: drags the picked knot.
    pub(super) fn river_gizmo_drag(&mut self, gesture: gizmo::Gesture) -> Option<Drag> {
        self.river_pivot()?;
        self.river_step();
        let surfaces = self.river_surfaces();
        let d = self.river.as_ref()?;
        Some(Drag::RiverGizmo {
            gesture,
            index: d.picked?,
            start: d.knots.iter().map(|k| k.at).collect(),
            surfaces,
        })
    }

    pub(super) fn remove_river_knot(&mut self) {
        if self
            .river
            .as_ref()
            .is_some_and(|d| d.picked.is_some_and(|i| i < d.knots.len()))
        {
            self.river_step();
        }
        if let Some(d) = &mut self.river
            && let Some(i) = d.picked.take()
            && i < d.knots.len()
        {
            d.knots.remove(i);
            // The two it joined meet along a new chord.
            for k in [i.wrapping_sub(1), i] {
                if let Some(knot) = d.knots.get_mut(k) {
                    knot.tangents = None;
                }
            }
        }
    }

    /// Paints the riverbed's materials along `only` its changed stretches
    /// (all of it without), and refreshes the splat there.
    fn paint_bed(&mut self, samples: &[Sample], only: Option<&bed::Stretches>) {
        let names = &self.rules().ground_names;
        let (Some(palette), Some(m)) = (self.palette.clone(), &mut self.island.doc.materials)
        else {
            return;
        };
        let painted =
            (self.river_bed).paint(m, names, &palette, &self.island.doc.field, samples, only);
        if let Some(area) = painted {
            self.show_paint(area);
        }
    }

    /// Paints the lake bed material along the shore, fading out over
    /// `BED_PAINT_BANK` metres of bank, at `only` the changed edges' samples
    /// (every sample without), and refreshes the splat there.
    fn paint_shore(&mut self, samples: &[Sample], only: Option<&lake::Bits>) {
        let names = &self.rules().ground_names;
        let (Some(palette), Some(m)) = (self.palette.clone(), &mut self.island.doc.materials)
        else {
            return;
        };
        let Some(id) = bed::sand(names, &palette) else {
            return;
        };
        let (mut lo, mut hi) = ([f32::MAX; 2], [f32::MIN; 2]);
        // Lakes have no width; their dabs cover the shore band.
        let half = 1.5;
        let radius = half + BED_PAINT_BANK;
        let brush = gpui_editor_kit::heightfield::Brush {
            radius,
            inner: half / radius,
            strength: 0.8,
            ..Default::default()
        };
        for (i, s) in samples.iter().enumerate() {
            if only.is_some_and(|b| !b.get(i)) {
                continue;
            }
            m.paint([s.p[0], s.p[2]], &brush, id);
            lo = [lo[0].min(s.p[0] - radius), lo[1].min(s.p[2] - radius)];
            hi = [hi[0].max(s.p[0] + radius), hi[1].max(s.p[2] + radius)];
        }
        if lo[0] <= hi[0] {
            self.show_paint([lo[0], lo[1], hi[0], hi[1]]);
        }
    }

    /// After a carve: vegetation standing in the water (`in_water`) goes,
    /// as shipped rivers have none in them, and props that follow the
    /// ground keep their height above it, as sculpting keeps them.
    fn settle_props(&mut self, old: &Heightfield, in_water: impl Fn([f32; 2]) -> bool) {
        let meshes = self.island.doc.meshes.clone();
        let vegetation = |file: u32| {
            meshes
                .get(file as usize)
                .is_some_and(|m| m.replace('\\', "/").contains("/vegetation/"))
        };
        let at = |p: &super::doc::Prop| [p.matrix[12], p.matrix[14]];
        let drowned = |p: &super::doc::Prop| vegetation(p.source.file) && in_water(at(p));
        let field = Arc::clone(&self.island.doc.field);
        let moved = |p: &super::doc::Prop| {
            p.source.adapt_terrain_height.is_some() && field.height(at(p)) != old.height(at(p))
        };
        if !self.island.doc.props.iter().any(|p| drowned(p) || moved(p)) {
            return;
        }
        let props = &mut self.island.doc.props;
        props.retain(|p| !drowned(p));
        props.edit_where(&moved, |p| {
            p.matrix[13] += field.height(at(p)) - old.height(at(p))
        });
        self.scene_stale = true;
    }

    /// A river's cells, bed, paint and props; a reopened river only along
    /// the stretches that changed, filling what it moved away from.
    fn write_river(&mut self, curve: &[PathPoint], s: &[Sample], old: Option<&[PathPoint]>) {
        let before = Arc::clone(&self.island.doc.field);
        // A reopened curve reshapes the ground only where it changed; with
        // Carve ground off, not at all.
        let only = match old {
            _ if !self.carve_ground => Some(bed::Stretches::default()),
            Some(old) => Some(changed(curve, old, s, self.river_bed.reach * 2. + 2.)),
            None => None,
        };
        if let Some(g) = &mut self.island.doc.grids {
            let g = Arc::make_mut(g);
            // The old shape's cells go first, so a moved river leaves no
            // stray water behind; its old bed is filled below.
            if let Some(old) = old {
                paint_curve(g, old, false, false);
            }
            paint_curve(g, curve, false, true);
        }
        if let Some(old) = old.filter(|_| self.carve_ground) {
            let old_samples = samples(old, false);
            let gone = changed(old, curve, &old_samples, self.river_bed.reach * 2. + 2.);
            if !gone.is_empty() {
                let field = Arc::make_mut(&mut self.island.doc.field);
                self.river_bed.fill(field, &old_samples, &gone);
            }
        }
        if only.as_ref().is_some_and(|o| o.is_empty()) {
            return;
        }
        let field = Arc::make_mut(&mut self.island.doc.field);
        self.river_bed.carve(field, s, only.as_ref());
        self.paint_bed(s, only.as_ref());
        let channel = bed::Channel::new(s, 1.);
        self.settle_props(&before, |p| {
            (channel.as_ref().and_then(|c| c.at(p))).is_some_and(|h| {
                h.across.abs() <= h.half + 0.5
                    && only.as_ref().is_none_or(|o| o.weight(h.along) > 0.)
            })
        });
    }

    /// A lake's cells, basin, shore paint and props. A reopened lake at the
    /// same level changes only the region its edit can reach
    /// (`lake::Edit`), filling what it moved away from; a new lake, or one
    /// raised or lowered (every point changes), is carved whole.
    fn write_lake(&mut self, curve: &[PathPoint], s: &[Sample], old: Option<&[PathPoint]>) {
        let Some(shore) = lake::Shore::new(s, lake::BANK_MAX) else {
            return;
        };
        let old = old.and_then(|o| {
            let shore = lake::Shore::new(&samples(o, true), lake::BANK_MAX)?;
            Some((o, shore))
        });
        let edit =
            (old.as_ref()).map(|(o, old_shore)| lake::Edit::new(o, curve, old_shore, &shore));
        let region = edit.as_ref().map(|e| &e.region);
        if region.is_some_and(lake::Region::is_empty) {
            return;
        }
        if let Some(g) = &mut self.island.doc.grids {
            let g = Arc::make_mut(g);
            if let Some((_, old_shore)) = &old {
                lake::paint(g, old_shore, false, region);
            }
            lake::paint(g, &shore, true, region);
        }
        if !self.carve_ground {
            return;
        }
        let before = Arc::clone(&self.island.doc.field);
        let field = Arc::make_mut(&mut self.island.doc.field);
        if let (Some((o, old_shore)), Some(e)) = (&old, &edit) {
            lake::fill(field, old_shore, o[0].p[1], &shore, e);
        }
        lake::carve(field, &shore, curve[0].p[1], region);
        self.paint_shore(s, edit.as_ref().map(|e| &e.new));
        self.settle_props(&before, |p| {
            region.is_none_or(|r| r.contains(p)) && shore.inside(p)
        });
    }

    /// Writes the draft as one undo step: object, carved bed, grids.
    pub(super) fn finish_river(&mut self) {
        let Some(draft) = self.river.clone() else {
            return;
        };
        let (need, what) = if draft.lake {
            (3, "Lake")
        } else {
            (2, "River")
        };
        if draft.knots.len() < need {
            return self.say(format!("A {what} needs at least {need} points"), true);
        }
        let Some(lib) = self.island.harbour.clone() else {
            return self.say_no_records("River");
        };
        let piece = if draft.lake {
            Piece::Lake
        } else {
            Piece::River
        };
        let existing = draft
            .id
            .and_then(|id| self.island.doc.objects.iter().position(|o| o.id == id));
        if existing.is_none() && (lib.guid(piece).is_none() || self.island.object_store.is_none()) {
            return self.say(format!("No shipped {what} record to copy"), true);
        }
        let curve = draft.curve(&self.island.doc.field);
        let before = self.island.doc.clone();
        let old = existing.and_then(|i| self.island.doc.objects[i].path.clone());
        let s = samples(&curve, draft.lake);
        if draft.lake {
            self.write_lake(&curve, &s, old.as_deref());
        } else {
            self.write_river(&curve, &s, old.as_deref());
        }
        let id = match existing {
            Some(i) => {
                let o = &mut Arc::make_mut(&mut self.island.doc.objects)[i];
                o.position = curve[0].p;
                o.path = Some(curve);
                o.id
            }
            None => {
                let store = self.island.object_store.as_mut().expect("checked above");
                store.objects.clone_from(&self.island.doc.objects);
                let areas = self
                    .island
                    .doc
                    .grids
                    .as_ref()
                    .and_then(|g| g.layer(Kind::AreaIds));
                let mut o = lib
                    .place(store, piece, areas, curve[0].p)
                    .expect("checked above");
                o.path = Some(curve);
                let id = o.id;
                Arc::make_mut(&mut self.island.doc.objects).push(o);
                id
            }
        };
        self.checkpoint_owned(before);
        let h = &mut self.river_history;
        let steps = std::mem::take(&mut h.undo);
        h.close();
        h.at = self.history.revision();
        if h.written.len() == WRITTEN {
            h.written.remove(0);
        }
        h.written.push((self.history.revision(), draft, steps));
        self.river = None;
        self.selection = BTreeSet::from([Item::Object(id)]);
        self.rebuild_tiles(None);
        // A game that names no bed material leaves the ground's paint.
        let unnamed = (self.palette.as_ref())
            .filter(|_| self.island.doc.materials.is_some())
            .is_some_and(|p| bed::sand(&self.rules().ground_names, p).is_none());
        let note = if unnamed {
            "; no bed material named for this game, ground paint left as it was"
        } else {
            ""
        };
        self.say(
            format!("{what} written: bed carved, River and Water cells painted{note}"),
            false,
        );
    }

    /// Screen positions of the draft's point and width handles.
    fn river_handles(&self, view: usize) -> Vec<(usize, Handle, Point<Pixels>)> {
        let Some(d) = &self.river else {
            return Vec::new();
        };
        let v = &self.views[view];
        let extent = self.extent();
        let ground = |p: [f32; 2]| [p[0], self.island.doc.field.height(p) + 0.3, p[1]];
        let n = d.knots.len();
        let mut out = Vec::new();
        for (i, k) in d.knots.iter().enumerate() {
            out.push((
                i,
                Handle::Point,
                v.camera.project(ground(k.at), v.bounds, extent),
            ));
            // Reopened curves hold a point every metre or two: only the
            // picked one offers its width.
            if d.lake || d.picked != Some(i) {
                continue;
            }
            let (a, b) = (
                d.knots[i.saturating_sub(1)].at,
                d.knots[(i + 1).min(n - 1)].at,
            );
            let (tx, tz) = (b[0] - a[0], b[1] - a[1]);
            let l = tx.hypot(tz);
            let (nx, nz) = if l > 1e-3 {
                (-tz / l, tx / l)
            } else {
                (0., 1.)
            };
            let side = [k.at[0] + nx * k.width / 2., k.at[1] + nz * k.width / 2.];
            out.push((
                i,
                Handle::Width,
                v.camera.project(ground(side), v.bounds, extent),
            ));
        }
        out
    }

    fn river_handle_at(&self, view: usize, at: Point<Pixels>) -> Option<(usize, Handle)> {
        // Handles are drawn 7 px across; give the cursor a little more.
        const HIT: f32 = 9.;
        self.river_handles(view)
            .into_iter()
            .map(|(i, h, p)| (i, h, f32::from(p.x - at.x).hypot(f32::from(p.y - at.y))))
            .filter(|&(.., d)| d < HIT)
            .min_by(|a, b| a.2.total_cmp(&b.2))
            .map(|(i, h, _)| (i, h))
    }

    /// River and lake objects with their curves sampled on screen.
    fn river_lines(&self, view: usize) -> Vec<(u64, Vec<Point<Pixels>>)> {
        let v = &self.views[view];
        let extent = self.extent();
        self.island
            .doc
            .objects
            .iter()
            .filter_map(|o| {
                let lake = self.water_kind(o.guid)?;
                let s = samples(o.path.as_deref()?, lake);
                let mut pts: Vec<_> = s
                    .iter()
                    .map(|s| {
                        v.camera
                            .project([s.p[0], s.p[1] + 0.1, s.p[2]], v.bounds, extent)
                    })
                    .collect();
                if lake && let Some(&first) = pts.first() {
                    pts.push(first);
                }
                Some((o.id, pts))
            })
            .collect()
    }

    /// The river or lake under the cursor: on its water (within half its
    /// width plus `GRACE`, or inside a lake's shore), else within a few
    /// pixels of its line on screen, which is what is left to aim at when
    /// zoomed far out.
    pub(super) fn water_at(&self, view: usize, at: Point<Pixels>) -> Option<u64> {
        self.ground(view, at)
            .and_then(|g| self.water_near(g[0], g[2]))
            .or_else(|| self.river_object_at(view, at))
    }

    /// The river or lake whose water covers (x, z), or comes within
    /// `GRACE` of it; the nearest if several do.
    pub(super) fn water_near(&self, x: f32, z: f32) -> Option<u64> {
        // Metres past the water's edge that still count as on the river.
        const GRACE: f32 = 2.;
        let mut best: Option<(u64, f32)> = None;
        for o in self.island.doc.objects.iter() {
            let (Some(lake), Some(path)) = (self.water_kind(o.guid), o.path.as_deref()) else {
                continue;
            };
            let s = samples(path, lake);
            // Distance past the edge; negative is on the water.
            let past = if lake {
                let poly = polygon(&s);
                let edge = poly
                    .iter()
                    .zip(poly.iter().cycle().skip(1))
                    .map(|(a, b)| segment_distance([x, z], *a, *b))
                    .fold(f32::MAX, f32::min);
                if inside(&poly, [x, z]) { -edge } else { edge }
            } else {
                s.windows(2)
                    .map(|w| {
                        let d = segment_distance(
                            [x, z],
                            [w[0].p[0], w[0].p[2]],
                            [w[1].p[0], w[1].p[2]],
                        );
                        d - w[0].w.max(w[1].w) / 2.
                    })
                    .fold(f32::MAX, f32::min)
            };
            if past < GRACE && best.is_none_or(|(_, b)| past < b) {
                best = Some((o.id, past));
            }
        }
        best.map(|(id, _)| id)
    }

    fn river_object_at(&self, view: usize, at: Point<Pixels>) -> Option<u64> {
        const HIT: f32 = 8.;
        let (cx, cy) = (f32::from(at.x), f32::from(at.y));
        self.river_lines(view)
            .into_iter()
            .filter_map(|(id, pts)| {
                let d = pts
                    .windows(2)
                    .map(|w| {
                        let (ax, ay) = (f32::from(w[0].x), f32::from(w[0].y));
                        let (dx, dy) = (f32::from(w[1].x) - ax, f32::from(w[1].y) - ay);
                        let l2 = dx * dx + dy * dy;
                        let t = if l2 > 0. {
                            (((cx - ax) * dx + (cy - ay) * dy) / l2).clamp(0., 1.)
                        } else {
                            0.
                        };
                        (cx - ax - t * dx).hypot(cy - ay - t * dy)
                    })
                    .fold(f32::MAX, f32::min);
                (d < HIT).then_some((id, d))
            })
            .min_by(|a, b| a.1.total_cmp(&b.1))
            .map(|(id, _)| id)
    }

    /// River and lake courses for the scene, drawn by the renderer over
    /// the ground; the hovered and selected ones wider with their banks,
    /// every river's banks with the River tool. The river being edited is
    /// the draft's (`paint_rivers`).
    /// From the courses `inland_job` sampled last: sampling every river
    /// was up to 20 ms of an 1800 island's first scene.
    pub(super) fn water_lines(&self) -> Vec<gpu::Line> {
        let editing = self.river.as_ref().and_then(|d| d.id);
        let lit =
            |id: u64| self.hover_water == Some(id) || self.selection.contains(&Item::Object(id));
        let mut lines = Vec::new();
        for o in &self.water_courses {
            let (lake, s) = (o.lake, &o.samples);
            if Some(o.id) == editing {
                continue;
            }
            let mut points: Vec<Vec3> = s.iter().map(|s| [s.p[0], s.p[1] + 0.1, s.p[2]]).collect();
            if lake && let Some(&first) = points.first() {
                points.push(first);
            }
            // Lit when hovered or selected, plain with the River tool, and
            // only a faint hint otherwise: an outline that is always loud
            // reads as a selection.
            let (width, color) = if lit(o.id) {
                (
                    4.,
                    crate::colors::with_alpha(crate::colors::RIVER_LIT, 0xff),
                )
            } else if self.tool == Tool::River {
                (2., crate::colors::with_alpha(crate::colors::RIVER, 0xd0))
            } else {
                (1.5, crate::colors::with_alpha(crate::colors::RIVER, 0x60))
            };
            lines.push(gpu::Line {
                points,
                width,
                color,
            });
            if !lake && (self.tool == Tool::River || lit(o.id)) {
                let color = if lit(o.id) {
                    crate::colors::with_alpha(crate::colors::RIVER_LIT, 0xd0)
                } else {
                    crate::colors::with_alpha(crate::colors::RIVER, 0x80)
                };
                lines.extend(self.banks(s).map(|points| gpu::Line {
                    points,
                    width: 1.,
                    color,
                }));
            }
        }
        lines
    }

    /// A river's two width outlines, draped on the ground (or the water,
    /// where it is above the ground).
    fn banks<'a>(&'a self, s: &'a [Sample]) -> impl Iterator<Item = Vec<Vec3>> + 'a {
        [-0.5f32, 0.5].into_iter().map(move |side| {
            s.iter()
                .map(|s| {
                    let (x, z) = (
                        s.p[0] - s.dir[1] * s.w * side,
                        s.p[2] + s.dir[0] * s.w * side,
                    );
                    [x, self.island.doc.field.height([x, z]).max(s.p[1]) + 0.1, z]
                })
                .collect()
        })
    }

    /// With the River tool, the draft and its handles. Settled rivers and
    /// lakes are scene lines (`water_lines`).
    pub(super) fn paint_rivers(&self, view: usize, bounds: Bounds<Pixels>, window: &mut Window) {
        let v = &self.views[view];
        let extent = self.extent();
        let outline = |s: &[Sample], color: u32, window: &mut Window| {
            for points in self.banks(s) {
                let pts: Vec<_> = (points.iter())
                    .map(|&p| v.camera.project(p, bounds, extent))
                    .collect();
                paint::line(&pts, 1., rgba(color), window);
            }
        };
        if self.tool != Tool::River {
            return;
        }
        let Some(d) = &self.river else { return };
        let curve = d.curve(&self.island.doc.field);
        let s = samples(&curve, d.lake);
        let mut pts: Vec<_> = s
            .iter()
            .map(|s| {
                v.camera
                    .project([s.p[0], s.p[1] + 0.1, s.p[2]], bounds, extent)
            })
            .collect();
        if d.lake
            && let Some(&first) = pts.first()
        {
            pts.push(first);
        }
        paint::line(
            &pts,
            3.,
            rgba(crate::colors::with_alpha(crate::colors::PICK, 0xe6)),
            window,
        );
        if !d.lake {
            outline(
                &s,
                crate::colors::with_alpha(crate::colors::PICK, 0xa0),
                window,
            );
        }
        for (i, handle, p) in self.river_handles(view) {
            let picked = d.picked == Some(i);
            let color = rgba(if picked {
                crate::colors::with_alpha(crate::colors::PICK, 0xff)
            } else {
                crate::colors::with_alpha(crate::colors::HANDLE, 0xe6)
            });
            match handle {
                Handle::Point => {
                    let (x, y) = (f32::from(p.x), f32::from(p.y));
                    let r = if picked { 5. } else { 2.5 };
                    let square: Vec<_> = [[-r, -r], [r, -r], [r, r], [-r, r], [-r, -r]]
                        .iter()
                        .map(|[dx, dy]| point(px(x + dx), px(y + dy)))
                        .collect();
                    paint::line(&square, 2., color, window);
                }
                Handle::Width => paint::circle(p, 3., 1.5, color, window),
            }
        }
    }

    /// Flow arrows around the cursor while the WaterFlow overlay shows.
    pub(super) fn paint_flow_arrows(
        &self,
        view: usize,
        bounds: Bounds<Pixels>,
        window: &mut Window,
    ) {
        let (Some(at), Some(layer)) = (
            self.cursor,
            self.island
                .doc
                .grids
                .as_ref()
                .and_then(|g| g.layer(Kind::WaterFlow)),
        ) else {
            return;
        };
        let v = &self.views[view];
        let extent = self.extent();
        // Every other 2 m cell out a little past the brush ring.
        let reach = self.brush.radius + 8.;
        let lattice = Lattice::cells(layer);
        let lo = [at[0] - reach, at[2] - reach];
        let hi = [at[0] + reach, at[2] + reach];
        let ground = |x: f32, z: f32| {
            v.camera.project(
                [x, self.island.doc.field.height([x, z]) + 0.2, z],
                bounds,
                extent,
            )
        };
        for (x, y) in lattice.within(lo, hi).filter(|(x, y)| (x + y) % 2 == 0) {
            let [fx, fz] = grids::flow(layer.get(x, y));
            let speed = fx.hypot(fz);
            if speed < 1. {
                continue;
            }
            // 3 m arrows at full speed.
            let scale = 3. * (speed / grids::FLOW_SPEED).min(1.5) / speed;
            let [cx, cz] = lattice.at(x, y);
            let (dx, dz) = (fx * scale, fz * scale);
            let (ex, ez) = (cx + dx, cz + dz);
            let (bx, bz) = (ex - dx * 0.35, ez - dz * 0.35);
            let (sx, sz) = (-dz * 0.2, dx * 0.2);
            paint::line(
                &[
                    ground(cx, cz),
                    ground(ex, ez),
                    ground(bx + sx, bz + sz),
                    ground(ex, ez),
                    ground(bx - sx, bz - sz),
                ],
                1.5,
                rgba(crate::colors::with_alpha(crate::colors::HANDLE, 0xd0)),
                window,
            );
        }
    }
}

/// `Some(lake)` for river and lake objects: by the harbour records' River
/// and Lake GUIDs, else by the asset's template.
pub(super) fn water_kind(
    lib: Option<&harbour::Library>,
    assets: Option<&anno_formats::assets::Assets>,
    guid: u32,
) -> Option<bool> {
    if let Some(lib) = lib {
        if lib.guid(Piece::River) == Some(guid) {
            return Some(false);
        }
        if lib.guid(Piece::Lake) == Some(guid) {
            return Some(true);
        }
    }
    match harbour::template_of(assets?, guid).as_str() {
        "River" => Some(false),
        "Lake" => Some(true),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::{Bed, Draft, Knot, WATER_OVERLAP, inland, samples, water, water_mesh};
    use crate::island::test_support::{
        assets, harbour_library, install, layer, new_island, objects, saved, staged,
    };
    use anno_core::harbour::Piece;
    use gpui_editor_kit::heightfield::{Grid, Heightfield};

    /// 128 m square sloping down toward +x, 10 m at x = 0.
    fn slope() -> Heightfield {
        let grid = Grid {
            width: 257,
            depth: 257,
            spacing: 0.5,
            origin: [0.; 2],
        };
        let heights = (0..grid.len())
            .map(|i| 10. - (i % grid.width) as f32 * 0.5 * 0.05)
            .collect();
        Heightfield::new(grid, heights)
    }

    fn knot(x: f32, z: f32) -> Knot {
        Knot {
            at: [x, z],
            width: 6.,
            surface: None,
            flow: None,
            tangents: None,
        }
    }

    fn draft(lake: bool, knots: Vec<Knot>) -> Draft {
        Draft {
            id: None,
            lake,
            knots,
            picked: None,
        }
    }

    #[test]
    fn rivers_run_downhill_from_their_source() {
        let field = slope();
        // Drawn from the mouth up, with a point raised far above the rest.
        let mut d = draft(
            false,
            vec![
                knot(100., 60.),
                knot(70., 64.),
                knot(40., 60.),
                knot(10., 64.),
            ],
        );
        d.knots[1].surface = Some(30.);
        let curve = d.curve(&field);
        assert_eq!(curve[0].p[0], 10., "the source comes first");
        assert!(curve.windows(2).all(|w| w[1].p[1] <= w[0].p[1]));
        assert!(curve[0].p[1] < field.height([10., 64.]), "below the ground");
        for (k, p) in curve.iter().enumerate() {
            let (i, o) = (p.i.unwrap(), p.o.unwrap());
            // Out tangents point downstream, a sixth of the chord long.
            if k + 1 < curve.len() {
                let next = curve[k + 1].p;
                let chord = (next[0] - p.p[0]).hypot(next[2] - p.p[2]);
                assert!(o[0] > 0. && (o[0].hypot(o[2]) - chord / 6.).abs() < 0.2);
            }
            assert!(i[0] < 0.);
            assert_eq!(p.w, Some(6.));
            assert!(p.u0.is_some());
        }
    }

    // Needs the game: undo while drafting steps through the draft's own
    // edits and leaves the document alone; undoing the write reopens the
    // draft, and redo writes it again.
    #[test]
    fn draft_edits_undo_one_by_one_and_writes_reopen() {
        use super::super::{IslandEditor, Start, Tool};
        let Some(install) = install() else {
            return;
        };
        let assets = assets(&install);
        let mut editor = new_island(&install, "roman", 320, Start::Hills, "River Undo Test");
        editor.tool = Tool::River;
        editor.island.harbour = Some(harbour_library(&install, &assets, &editor));
        let c = editor.island.doc.field.grid.size()[0] / 2.;
        let knots = |e: &IslandEditor| e.river.as_ref().map(|d| d.knots.len());
        let objects = |e: &IslandEditor| e.island.doc.objects.len();
        let (revision, count) = (editor.history.revision(), objects(&editor));
        for k in 0..3 {
            editor.river_add([c - 30. + k as f32 * 30., c]);
        }
        // Points undo and redo one at a time; past the first, the draft
        // closes. The document never moves.
        assert!(editor.undo(false));
        assert_eq!(knots(&editor), Some(2));
        assert!(editor.undo(true));
        assert_eq!(knots(&editor), Some(3));
        assert!(!editor.undo(true), "nothing to redo in the draft");
        // A handle drag is one step; one that moves nothing is none.
        editor.river_step();
        let start: Vec<_> = editor
            .river
            .as_ref()
            .unwrap()
            .knots
            .iter()
            .map(|k| k.at)
            .collect();
        editor.river_drag(0, super::Handle::Point, &start, [c - 40., 0., c + 5.]);
        editor.river_moved();
        editor.river_step();
        editor.river_moved();
        assert!(editor.undo(false));
        assert_eq!(editor.river.as_ref().unwrap().knots[0].at, [c - 30., c]);
        for _ in 0..3 {
            assert!(editor.undo(false));
        }
        assert_eq!(knots(&editor), None, "closed past its first point");
        assert_eq!(editor.history.revision(), revision);
        for _ in 0..3 {
            assert!(editor.undo(true));
        }
        assert_eq!(knots(&editor), Some(3), "redo reopens it");

        // Undoing the write reopens the draft; its steps still undo.
        editor.finish_river();
        assert_eq!((knots(&editor), objects(&editor)), (None, count + 1));
        assert!(editor.undo(false));
        assert_eq!((knots(&editor), objects(&editor)), (Some(3), count));
        assert!(editor.undo(false));
        assert_eq!(knots(&editor), Some(2));
        assert!(editor.undo(true));
        // Unchanged again, so redo writes it.
        assert!(editor.undo(true));
        assert_eq!((knots(&editor), objects(&editor)), (None, count + 1));
        // Edited after reopening, redo stays in the draft.
        assert!(editor.undo(false));
        editor.river_add([c + 60., c]);
        assert!(!editor.undo(true));
        assert_eq!((knots(&editor), objects(&editor)), (Some(4), count));
    }

    // Needs the game: a written river reopens exactly as stored; moving
    // one of its points bends its neighbours within the falloff and leaves
    // the rest of the curve and the ground along it as they were, while
    // the ground near the moved point is carved anew.
    #[test]
    fn reopened_rivers_keep_their_curve_and_reshape_only_what_moved() {
        use super::super::{Start, Tool};
        let Some(install) = install() else {
            return;
        };
        let assets = assets(&install);
        let mut editor = new_island(&install, "roman", 320, Start::Hills, "Reopen Test");
        editor.tool = Tool::River;
        editor.island.harbour = Some(harbour_library(&install, &assets, &editor));
        let c = editor.island.doc.field.grid.size()[0] / 2.;
        for k in 0..4 {
            editor.river_add([c - 60. + k as f32 * 40., c + (k % 2) as f32 * 8.]);
        }
        editor.finish_river();
        let id = editor.island.doc.objects.last().unwrap().id;
        let stored = editor
            .island
            .doc
            .objects
            .last()
            .unwrap()
            .path
            .clone()
            .unwrap();
        assert!(editor.edit_river(id));
        let draft = editor.river.clone().unwrap();
        assert_eq!(
            draft.knots.len(),
            stored.len(),
            "every stored point is a knot"
        );
        assert_eq!(
            draft.curve(&editor.island.doc.field),
            stored,
            "reopening changes nothing"
        );

        // Move the point nearest the middle 3 m sideways.
        let mid = draft.knots.len() / 2;
        let start: Vec<_> = draft.knots.iter().map(|k| k.at).collect();
        editor.river_step();
        let to = [start[mid][0], start[mid][1] + 3.];
        editor.river_move(mid, &start, to);
        editor.river_moved();
        let moved = editor.river.clone().unwrap();
        let shift = |k: usize| moved.knots[k].at[1] - start[k][1];
        assert_eq!(shift(mid), 3.);
        let near = (0..start.len())
            .find(|&k| {
                k != mid && {
                    let d = (start[k][0] - start[mid][0]).hypot(start[k][1] - start[mid][1]);
                    d > 2. && d < editor.river_falloff * 0.6
                }
            })
            .expect("a neighbour within the falloff");
        assert!(shift(near) > 0. && shift(near) < 3., "{}", shift(near));
        assert_eq!(shift(0), 0.);
        assert!(
            moved.knots[0].tangents.is_some(),
            "far points keep their tangents"
        );

        let before = editor.island.doc.field.clone();
        editor.finish_river();
        let after = &editor.island.doc.field;
        let path = editor
            .island
            .doc
            .objects
            .iter()
            .find(|o| o.id == id)
            .unwrap()
            .path
            .clone()
            .unwrap();
        assert_eq!(path[0], stored[0], "the far end is written as stored");
        let ground = |f: &gpui_editor_kit::heightfield::Heightfield, k: usize| {
            f.height([stored[k].p[0], stored[k].p[2]])
        };
        assert_eq!(
            ground(after, 0),
            ground(&before, 0),
            "untouched stretch keeps its ground"
        );
        let changed = (0..after.heights.len())
            .filter(|&i| after.heights[i] != before.heights[i])
            .count();
        assert!(changed > 0, "the moved stretch is carved");
    }

    // Needs the game: moving a carved river's middle well aside fills
    // its old trench back up to about the ground it was cut from.
    #[test]
    fn a_river_moved_aside_fills_its_old_bed() {
        use super::super::{Start, Tool};
        let Some(install) = install() else {
            return;
        };
        let assets = assets(&install);
        let mut editor = new_island(&install, "roman", 320, Start::Plateau, "Fill Test");
        editor.tool = Tool::River;
        editor.island.harbour = Some(harbour_library(&install, &assets, &editor));
        let c = editor.island.doc.field.grid.size()[0] / 2.;
        for k in 0..5 {
            editor.river_add([c - 60. + k as f32 * 30., c]);
        }
        let mid = [c, c];
        let original = editor.island.doc.field.height(mid);
        editor.finish_river();
        let carved = editor.island.doc.field.height(mid);
        assert!(carved < original - 0.2, "carved {carved} from {original}");

        let id = editor.island.doc.objects.last().unwrap().id;
        assert!(editor.edit_river(id));
        let draft = editor.river.clone().unwrap();
        let start: Vec<_> = draft.knots.iter().map(|k| k.at).collect();
        let k = (0..start.len())
            .min_by(|&a, &b| {
                let d = |k: usize| (start[k][0] - mid[0]).hypot(start[k][1] - mid[1]);
                d(a).total_cmp(&d(b))
            })
            .unwrap();
        editor.river_step();
        editor.river_move(k, &start, [start[k][0], start[k][1] + 16.]);
        editor.finish_river();
        let filled = editor.island.doc.field.height(mid);
        assert!(
            (filled - original).abs() < (carved - original).abs() / 2.,
            "filled to {filled}, carved {carved}, ground was {original}"
        );
    }

    // Needs the game: with a curve open, a click on its water puts a point
    // there between its neighbours; a click away from it extends the
    // nearer end, not always the last one.
    #[test]
    fn clicks_insert_on_the_water_and_extend_the_nearer_end() {
        use super::super::{Start, Tool};
        let Some(install) = install() else {
            return;
        };
        let mut editor = new_island(&install, "roman", 320, Start::Hills, "Insert Test");
        editor.tool = Tool::River;
        let c = 160.;
        editor.river_add([c - 40., c]);
        editor.river_add([c + 40., c]);
        let xs = |e: &super::super::IslandEditor| -> Vec<f32> {
            e.river
                .as_ref()
                .unwrap()
                .knots
                .iter()
                .map(|k| k.at[0])
                .collect()
        };
        editor.river_insert([c, c + 1.]);
        assert_eq!(xs(&editor), [c - 40., c, c + 40.]);
        assert_eq!(editor.river.as_ref().unwrap().picked, Some(1));
        editor.river_insert([c - 80., c + 30.]);
        assert_eq!(xs(&editor), [c - 80., c - 40., c, c + 40.], "prepended");
        editor.river_insert([c + 90., c - 30.]);
        assert_eq!(xs(&editor).last(), Some(&(c + 90.)), "appended");
        assert!(editor.undo(false));
        assert_eq!(xs(&editor).len(), 4);
    }

    // Needs the game: a knot's flow speeds edit as one undo step per knot
    // touched, and the carved curve stores them; unedited knots keep the
    // shipped `(1, 3, 1)` and the mouth slows to 1.
    #[test]
    fn flow_speeds_edit_per_knot_and_reach_the_curve() {
        use super::super::{Field, Start, Tool};
        let Some(install) = install() else {
            return;
        };
        let mut editor = new_island(&install, "roman", 320, Start::Hills, "Flow Test");
        editor.tool = Tool::River;
        let c = 160.;
        for x in [c - 60., c, c + 60.] {
            editor.river_add([x, c]);
        }
        editor.river.as_mut().unwrap().picked = Some(1);
        editor.set_field(Field::Flow(1), 5.);
        editor.set_field(Field::Flow(1), 6.);
        editor.set_field(Field::Flow(0), 2.);
        editor.set_field(Field::Flow(2), 99.);
        let flow = |e: &super::super::IslandEditor| e.river.as_ref().unwrap().knots[1].flow;
        assert_eq!(flow(&editor), Some([2., 6., 8.]));
        assert!(editor.undo(false));
        assert_eq!(flow(&editor), None);
        assert!(editor.undo(true));
        let draft = editor.river.clone().unwrap();
        let curve = draft.curve(&editor.island.doc.field);
        let at_knot = curve
            .iter()
            .find(|p| (p.p[0] - c).abs() < 1e-3 && (p.p[2] - c).abs() < 1e-3)
            .expect("the edited knot is a curve point");
        assert_eq!(at_knot.u0, Some([2., 6., 8.]));
        assert_eq!(curve.last().unwrap().u0, Some(super::U0_MOUTH));
        assert!(curve.iter().any(|p| p.u0 == Some(super::U0)));
    }

    // Raising the water at one point of a dense river raises its
    // neighbours within the falloff less the farther they are, and leaves
    // the rest where the curve puts it.
    #[test]
    fn raised_water_eases_into_its_neighbours() {
        use super::super::{Start, Tool};
        let Some(install) = install() else {
            return;
        };
        let mut editor = new_island(&install, "roman", 320, Start::Hills, "Raise Test");
        editor.tool = Tool::River;
        let c = 160.;
        for k in 0..21 {
            editor.river_add([c - 20. + k as f32 * 2., c]);
        }
        let mid = 10;
        editor.river.as_mut().unwrap().picked = Some(mid);
        // What the gizmo's press takes.
        let start: Vec<_> = (editor.river.as_ref().unwrap().knots.iter())
            .map(|k| k.at)
            .collect();
        let surfaces = editor.river_surfaces();
        editor.river_raise(mid, &start, &surfaces, 1.);
        let knots = &editor.river.as_ref().unwrap().knots;
        let rise = |k: usize| knots[k].surface.map(|s| s - surfaces[k]);
        assert_eq!(rise(mid), Some(1.));
        let near = rise(mid + 2).expect("4 m away follows");
        let farther = rise(mid + 4).expect("8 m away follows");
        assert!(
            1. > near && near > farther && farther > 0.,
            "{near} {farther}"
        );
        let other = rise(mid - 2).expect("both sides follow");
        assert!(
            (other - near).abs() < 1e-4,
            "both sides alike: {other} {near}"
        );
        assert_eq!(knots[0].surface, None, "20 m away stays on the curve");
    }

    /// A lake of `n` points `r` metres round (c, c), drawn and written.
    fn written_lake(editor: &mut super::super::IslandEditor, c: f32, r: f32, n: usize) -> u64 {
        editor.lake_mode = true;
        for k in 0..n {
            let a = k as f32 / n as f32 * std::f32::consts::TAU;
            editor.river_add([c + a.cos() * r, c + a.sin() * r]);
        }
        editor.finish_river();
        let msg = editor.message.take().unwrap();
        assert!(!msg.1, "{}", msg.0);
        editor.island.doc.objects.last().unwrap().id
    }

    fn lake_path(editor: &super::super::IslandEditor, id: u64) -> Vec<super::PathPoint> {
        (editor.island.doc.objects.iter())
            .find(|o| o.id == id)
            .and_then(|o| o.path.clone())
            .unwrap()
    }

    // Needs the game: pulling one side of a carved lake inward fills the
    // basin it left back up to about the ground it was cut from, leaves
    // the ground away from the edit bit for bit as it was, gives the
    // ground a whole carve of the edited lake gives, and undoes and redoes
    // exactly.
    #[test]
    fn a_lake_moved_in_fills_its_old_bed() {
        use super::super::{Start, Tool};
        use super::lake;
        let Some(install) = install() else {
            return;
        };
        let assets = assets(&install);
        let mut editor = new_island(&install, "roman", 320, Start::Plateau, "Lake Fill Test");
        editor.tool = Tool::River;
        editor.island.harbour = Some(harbour_library(&install, &assets, &editor));
        let c = editor.island.doc.field.grid.size()[0] / 2.;
        let (r, inward) = (40., 12.);
        // Inside the old shore, outside the new one.
        let gone = [c + r - 4., c];
        let original = editor.island.doc.field.height(gone);
        let id = written_lake(&mut editor, c, r, 12);
        let carved = editor.island.doc.field.height(gone);
        assert!(carved < original - 1., "carved {carved} from {original}");

        let stored = lake_path(&editor, id);
        assert!(editor.edit_river(id));
        let start: Vec<_> = (editor.river.as_ref().unwrap().knots.iter())
            .map(|k| k.at)
            .collect();
        let k = (0..start.len())
            .min_by(|&a, &b| {
                let d = |k: usize| (start[k][0] - c - r).hypot(start[k][1] - c);
                d(a).total_cmp(&d(b))
            })
            .unwrap();
        editor.river_step();
        editor.river_move(k, &start, [start[k][0] - inward, start[k][1]]);
        let before = editor.island.doc.field.clone();
        editor.finish_river();
        let after = editor.island.doc.field.clone();
        let filled = after.height(gone);
        assert!(
            (filled - original).abs() < (carved - original).abs() / 2.,
            "filled to {filled}, carved {carved}, ground was {original}"
        );

        // Away from the moved stretch (the falloff, the move, the bank
        // and the region's square corners) nothing changed.
        let far = std::f32::consts::SQRT_2 * (editor.river_falloff + inward + lake::BANK_MAX + 4.);
        let g = after.grid;
        let mut untouched = 0;
        for z in 0..g.depth {
            for x in 0..g.width {
                let p = [
                    x as f32 * g.spacing + g.origin[0],
                    z as f32 * g.spacing + g.origin[1],
                ];
                if (p[0] - start[k][0]).hypot(p[1] - start[k][1]) > far {
                    let i = g.index(x, z);
                    assert_eq!(
                        after.heights[i].to_bits(),
                        before.heights[i].to_bits(),
                        "at {p:?}"
                    );
                    untouched += 1;
                }
            }
        }
        assert!(untouched > g.len() / 2);

        // The same ground as filling, then carving the edited lake whole.
        let curve = lake_path(&editor, id);
        let shore =
            |c: &[super::PathPoint]| lake::Shore::new(&samples(c, true), lake::BANK_MAX).unwrap();
        let (old, new) = (shore(&stored), shore(&curve));
        let edit = lake::Edit::new(&stored, &curve, &old, &new);
        assert!(
            !edit.region.contains([c - r, c]),
            "the far shore is not re-carved"
        );
        let mut whole = (*before).clone();
        lake::fill(&mut whole, &old, stored[0].p[1], &new, &edit);
        lake::carve(&mut whole, &new, curve[0].p[1], None);
        assert!(
            whole.heights == after.heights,
            "incremental differs from a whole carve"
        );

        // Undo puts the carved lake back; redo the edit.
        assert!(editor.undo(false));
        assert!(editor.island.doc.field.heights == before.heights);
        assert!(editor.undo(true));
        assert!(editor.island.doc.field.heights == after.heights);
    }

    // Needs the game: raising the water at one point of a lake raises the
    // whole lake, which stays flat; lowering it re-carves the whole basin
    // deeper.
    #[test]
    fn raised_lake_water_lifts_the_whole_lake() {
        use super::super::{Start, Tool};
        let Some(install) = install() else {
            return;
        };
        let assets = assets(&install);
        let mut editor = new_island(&install, "roman", 320, Start::Plateau, "Lake Raise Test");
        editor.tool = Tool::River;
        editor.island.harbour = Some(harbour_library(&install, &assets, &editor));
        let c = editor.island.doc.field.grid.size()[0] / 2.;
        let id = written_lake(&mut editor, c, 30., 10);
        let level = lake_path(&editor, id)[0].p[1];
        assert!(editor.edit_river(id));
        let picked = 3;
        editor.river.as_mut().unwrap().picked = Some(picked);
        let start: Vec<_> = (editor.river.as_ref().unwrap().knots.iter())
            .map(|k| k.at)
            .collect();
        let surfaces = editor.river_surfaces();
        editor.river_raise(picked, &start, &surfaces, 1.);
        let knots = &editor.river.as_ref().unwrap().knots;
        assert!(
            (knots.iter().zip(&surfaces)).all(|(k, s)| k.surface == Some(s + 1.)),
            "every point rises"
        );
        let curve = editor
            .river
            .as_ref()
            .unwrap()
            .curve(&editor.island.doc.field);
        assert!(
            curve.iter().all(|p| p.p[1] == level + 1.),
            "flat, a metre up"
        );

        // Lowered instead: the whole basin goes down with it.
        editor.river_raise(picked, &start, &surfaces, -0.5);
        let centre = editor.island.doc.field.height([c, c]);
        let shore = editor.island.doc.field.height([c - 18., c + 18.]);
        editor.finish_river();
        assert_eq!(lake_path(&editor, id)[0].p[1], level - 0.5);
        assert_eq!(editor.island.doc.field.height([c, c]), centre - 0.5);
        assert!(editor.island.doc.field.height([c - 18., c + 18.]) <= shore - 0.5 + 1e-4);
    }

    // On a drafted river down a slope: the bed sits its depth under the
    // water on the centre line, the plain far off is untouched, and
    // carving again changes nothing.
    #[test]
    fn carving_shapes_the_same_bed_every_time() {
        let mut field = slope();
        let before = field.clone();
        let curve =
            draft(false, vec![knot(10., 64.), knot(60., 70.), knot(110., 64.)]).curve(&field);
        let s = samples(&curve, false);
        let bed = Bed {
            bends: 0.,
            pools: 0.,
            ..Bed::default()
        };
        bed.carve(&mut field, &s, None);
        let mid = s[s.len() / 2];
        let centre = field.height([mid.p[0], mid.p[2]]);
        assert!(
            (centre - (mid.p[1] - bed.depth)).abs() < 0.15,
            "bed {centre} surface {}",
            mid.p[1]
        );
        assert_eq!(field.height([60., 10.]), before.height([60., 10.]));
        let once = field.heights.clone();
        bed.carve(&mut field, &s, None);
        assert_eq!(field.heights, once, "carving again changes nothing");
    }

    // Needs the game: a river drawn across a new island and a lake beside
    // it save as River/Lake objects with their curves, a carved bed, River
    // and Water cells and (for the river) WaterFlow, each one undo step.
    #[test]
    fn rivers_and_lakes_carve_and_save_on_a_new_island() {
        use super::super::{IslandEditor, Start, Tool};
        use anno_formats::a7m::grids::{Grids, Kind};
        let Some(install) = install() else {
            return;
        };
        let assets = assets(&install);
        let mut editor = new_island(&install, "roman", 320, Start::Hills, "River Test");
        editor.tool = Tool::River;
        assert!(editor.needs_harbour_library());
        editor.island.harbour = Some(harbour_library(&install, &assets, &editor));
        let lib = editor.island.harbour.clone().unwrap();
        let (river_guid, lake_guid) = (
            lib.guid(Piece::River).expect("a shipped river"),
            lib.guid(Piece::Lake).expect("a shipped lake"),
        );

        // West to east across the middle, 5 points.
        let c = editor.island.doc.field.grid.size()[0] / 2.;
        let knots: Vec<[f32; 2]> = (0..5)
            .map(|k| [c - 60. + k as f32 * 30., c - 30. + (k % 2) as f32 * 8.])
            .collect();
        let before = editor.island.doc.clone();
        for &k in &knots {
            assert!(editor.island.doc.field.height(k) > 0.5, "on land: {k:?}");
            editor.river_add(k);
        }
        editor.finish_river();
        let msg = editor.message.take().unwrap();
        assert!(!msg.1, "{}", msg.0);
        assert!(editor.river.is_none());
        let rivers = |e: &IslandEditor, guid: u32| {
            e.island
                .doc
                .objects
                .iter()
                .filter(|o| o.guid == guid)
                .count()
        };
        let id_of = |e: &IslandEditor, guid: u32| {
            e.island
                .doc
                .objects
                .iter()
                .find(|o| o.guid == guid)
                .unwrap()
                .id
        };
        assert_eq!(rivers(&editor, river_guid), 1);
        // One undo step takes object, bed and cells back.
        let carved = editor.island.doc.field.heights.clone();
        editor.undo(false);
        assert_eq!(rivers(&editor, river_guid), 0);
        assert!(editor.island.doc.field.heights == before.field.heights);
        let river_at = |e: &IslandEditor, p: [f32; 2]| {
            super::grids::value_at(layer(&e.island.doc, Kind::River), p[0], p[1])
        };
        assert_eq!(river_at(&editor, knots[2]), 0);
        editor.undo(true);
        assert!(editor.island.doc.field.heights == carved);

        // Re-editing keeps the object and moves its cells.
        assert!(editor.edit_river(id_of(&editor, river_guid)));
        // Five clicks were carved into a point every 2 m; editing gets every
        // one of them back, so the curve stays as written.
        let stored = (editor.island.doc.objects.iter())
            .find(|o| o.guid == river_guid)
            .and_then(|o| o.path.clone())
            .unwrap();
        let draft = editor.river.as_mut().unwrap();
        assert_eq!(draft.knots.len(), stored.len());
        let last = draft.knots.len() - 1;
        let old_end = draft.knots[last].at;
        draft.knots[last].at[1] += 25.;
        draft.knots[last].surface = None;
        draft.knots[last].tangents = None;
        draft.knots[last - 1].tangents = None;
        let new_end = draft.knots[last].at;
        editor.finish_river();
        assert_eq!(rivers(&editor, river_guid), 1);
        assert_eq!(river_at(&editor, old_end), 0, "the old end dried up");
        assert_eq!(river_at(&editor, new_end), 1);

        // A lake north of the river.
        editor.lake_mode = true;
        let lake_centre = [c, c + 40.];
        for k in 0..6 {
            let a = k as f32 / 6. * std::f32::consts::TAU;
            editor.river_add([
                lake_centre[0] + a.cos() * 14.,
                lake_centre[1] + a.sin() * 14.,
            ]);
        }
        editor.finish_river();
        let msg = editor.message.take().unwrap();
        assert!(!msg.1, "{}", msg.0);
        assert_eq!(rivers(&editor, lake_guid), 1);

        // Picking by water: off the centre line but on the river, at the
        // bank, inside the lake; nothing in open country.
        let (river_id, lake_id) = (id_of(&editor, river_guid), id_of(&editor, lake_guid));
        let river = editor
            .island
            .doc
            .objects
            .iter()
            .find(|o| o.id == river_id)
            .unwrap();
        let mid = river.path.as_ref().unwrap()[10];
        assert_eq!(editor.water_near(mid.p[0] + 1.5, mid.p[2]), Some(river_id));
        assert_eq!(
            editor.water_near(mid.p[0], mid.p[2] + mid.w.unwrap() / 2. + 1.),
            Some(river_id)
        );
        assert_eq!(
            editor.water_near(lake_centre[0], lake_centre[1]),
            Some(lake_id)
        );
        assert_eq!(editor.water_near(c + 100., c - 100.), None);

        let (_config, project) = staged(&install, &mut editor);
        let island = saved(&project, &editor.path);
        let objects = objects(&island);
        let river = objects.iter().find(|o| o.guid == river_guid).unwrap();
        let path = river.path.as_ref().unwrap();
        assert!(path.len() >= 3);
        assert_eq!(river.position, path[0].p);
        assert!(
            path.iter()
                .all(|p| p.i.is_some() && p.o.is_some() && p.w.is_some() && p.u0.is_some())
        );
        assert!(path.windows(2).all(|w| w[1].p[1] <= w[0].p[1]), "downhill");
        // Resampled like shipped rivers: a point every couple of metres, not
        // only where the user clicked.
        assert!(path.len() > 50, "{} points", path.len());
        assert!(
            path.windows(2)
                .all(|w| (w[1].p[0] - w[0].p[0]).hypot(w[1].p[2] - w[0].p[2]) < 3.),
            "dense points"
        );
        let grids = Grids::read(&island).unwrap();
        let at = |k: Kind, x: f32, z: f32| super::grids::value_at(grids.layer(k).unwrap(), x, z);
        let heights = island.heights();
        let saved =
            |x: f32, z: f32| heights.get((x * 2.).round() as usize, (z * 2.).round() as usize);
        let flow = |x: f32, z: f32| super::grids::flow(at(Kind::WaterFlow, x, z));
        let bed = super::bed::sand(
            &editor.rules().ground_names,
            editor.palette.as_ref().unwrap(),
        )
        .expect("a bed material");
        let materials = editor.island.doc.materials.as_ref().unwrap();
        for p in path {
            let [x, y, z] = p.p;
            assert_eq!((at(Kind::River, x, z), at(Kind::Water, x, z)), (1, 1));
            assert_eq!(materials.dominant([x, z]), bed, "riverbed paint at {x},{z}");
            assert!(
                saved(x, z) < before.field.height([x, z]) - 0.5,
                "carved at {x},{z}"
            );
            assert!(saved(x, z) < y, "the bed is under the water");
            let [fx, fz] = flow(x, z);
            assert!(fx.hypot(fz) > 5., "WaterFlow along the river at {x},{z}");
        }
        // The bank at the water's edge stands above the surface, so the
        // terrain draws the shoreline, as on shipped rivers.
        for w in path.windows(2).step_by(5) {
            let (a, b) = (w[0], w[1]);
            let (dx, dz) = (b.p[0] - a.p[0], b.p[2] - a.p[2]);
            let l = dx.hypot(dz).max(1e-3);
            let half = a.w.unwrap() / 2.;
            let (x, z) = (a.p[0] - dz / l * half, a.p[2] + dx / l * half);
            assert!(saved(x, z) > a.p[1], "bank above the water at {x},{z}");
        }
        let lake = objects.iter().find(|o| o.guid == lake_guid).unwrap();
        let lp = lake.path.as_ref().unwrap();
        assert!(lp.len() > 6, "resampled lake shore");
        assert!(lp.iter().all(|p| p.p[1] == lp[0].p[1] && p.w.is_none()));
        let [lx, lz] = lake_centre;
        assert_eq!(at(Kind::River, lx, lz), 1);
        assert!(saved(lx, lz) < lp[0].p[1] - 1.);
    }

    #[test]
    fn lakes_are_flat_and_closed() {
        let field = slope();
        let curve = draft(
            true,
            vec![
                knot(40., 40.),
                knot(60., 40.),
                knot(60., 60.),
                knot(40., 60.),
            ],
        )
        .curve(&field);
        assert!(curve.iter().all(|p| p.p[1] == curve[0].p[1]));
        assert!(curve.iter().all(|p| p.w.is_none() && p.u0.is_none()));
        let s = samples(&curve, true);
        let (first, last) = (s[0].p, s[s.len() - 1].p);
        assert!(
            (first[0] - last[0]).hypot(first[2] - last[2]) < 2.,
            "ends meet"
        );
    }

    // Timing, not a pass/fail test: applying a small edit to the biggest
    // shipped lake (celtic_island_large_02, 13 000 m²), one point moved
    // 3 m outward, as the editor applies it.
    // ANNO117_DIR=… cargo test --release lake_edit_timing -- --ignored --nocapture
    #[test]
    #[ignore]
    fn lake_edit_timing() {
        use super::super::{IslandEditor, Tool};
        use std::sync::Arc;
        let install = Arc::new(install().expect("ANNO117_DIR"));
        let path = std::env::var("LAKE_ISLAND").unwrap_or_else(|_| {
            "data/base/provinces/celtic/islands/pool/celtic_island_large_02/celtic_island_large_02.a7m"
                .into()
        });
        let assets = assets(&install);
        let mut editor = IslandEditor::load_with(
            crate::games::module(crate::games::of(&install)),
            &install,
            &path,
            Some((install.clone(), assets.clone())),
        )
        .unwrap();
        editor.island.harbour = Some(harbour_library(&install, &assets, &editor));
        editor.tool = Tool::River;
        let (id, area) = (editor.island.doc.objects.iter())
            .filter(|o| editor.water_kind(o.guid) == Some(true))
            .filter_map(|o| {
                let s = samples(o.path.as_deref()?, true);
                let n = s.len();
                let area = (0..n)
                    .map(|k| {
                        let (a, b) = (s[k].p, s[(k + 1) % n].p);
                        a[0] * b[2] - b[0] * a[2]
                    })
                    .sum::<f32>()
                    .abs()
                    / 2.;
                Some((o.id, area))
            })
            .max_by(|a, b| a.1.total_cmp(&b.1))
            .expect("a lake");
        let mut times = Vec::new();
        for round in 0..6 {
            assert!(editor.edit_river(id));
            let d = editor.river.clone().unwrap();
            let start: Vec<_> = d.knots.iter().map(|k| k.at).collect();
            let n = start.len();
            let k = round * n / 6;
            let centre = start.iter().fold([0.; 2], |c, p| {
                [c[0] + p[0] / n as f32, c[1] + p[1] / n as f32]
            });
            let (dx, dz) = (start[k][0] - centre[0], start[k][1] - centre[1]);
            let l = dx.hypot(dz);
            editor.river_step();
            editor.river_move(
                k,
                &start,
                [start[k][0] + dx / l * 3., start[k][1] + dz / l * 3.],
            );
            let t = std::time::Instant::now();
            editor.finish_river();
            times.push(t.elapsed().as_secs_f64() * 1e3);
        }
        // The first edit flattens a shipped lake whose points stand at
        // different heights to its lowest: a new level, carved whole.
        let first = times.remove(0);
        times.sort_by(f64::total_cmp);
        println!(
            "lake {area:.0} m²: first edit {first:.1} ms, then {:.1} ms median, {:.1}-{:.1} ms",
            times[times.len() / 2],
            times[0],
            times[times.len() - 1]
        );
    }

    // Survey, not a pass/fail test: ground material and terrain height
    // relative to the water across shipped rivers.
    // ANNO117_DIR=… cargo test --release river_cross_sections -- --ignored --nocapture
    // Survey behind `bed`'s shape and paint: shipped rivers' ground across
    // their width (fractions of the half width inside, metres past the
    // edge outside), narrow and wide apart, with the dominant materials.
    // RIVER_ISLANDS=path,path ANNO117_DIR=… cargo test --release
    // river_bed_survey -- --ignored --nocapture
    #[test]
    #[ignore]
    fn river_bed_survey() {
        use crate::island::IslandEditor;
        use std::collections::BTreeMap;
        let install = install().expect("ANNO117_DIR");
        let list = std::env::var("RIVER_ISLANDS").unwrap();
        // Inside: a fraction of the half width; outside: 10 + metres past
        // the edge.
        let bins: [f32; 12] = [0., 0.2, 0.4, 0.6, 0.8, 0.9, 1.0, 10.5, 11., 12., 14., 18.];
        for class in 0..2 {
            let mut heights: Vec<Vec<f32>> = vec![Vec::new(); bins.len()];
            let mut names: Vec<BTreeMap<String, usize>> = vec![BTreeMap::new(); bins.len()];
            let mut slopes = Vec::new();
            for path in list.split(',') {
                let Ok(editor) = IslandEditor::load(
                    crate::games::module(crate::games::of(&install)),
                    &install,
                    path,
                ) else {
                    continue;
                };
                let (Some(palette), Some(m)) =
                    (editor.palette.clone(), editor.island.doc.materials.clone())
                else {
                    continue;
                };
                let dominant = |x: f32, z: f32| -> Option<String> {
                    let (ax, az) = ((x * m.per_metre) as usize, (z * m.per_metre) as usize);
                    let [aw, ah] = m.paint.size();
                    if ax >= aw || az >= ah {
                        return None;
                    }
                    let id = m.paint.dominant(ax, az);
                    Some(palette.materials.get(&id).map_or(format!("#{id}"), |mat| {
                        mat.file.rsplit('/').next().unwrap_or(&mat.file).to_owned()
                    }))
                };
                for o in editor
                    .island
                    .doc
                    .objects
                    .iter()
                    .filter(|o| o.guid == 1001608)
                {
                    let Some(curve) = &o.path else { continue };
                    let ss = samples(curve, false);
                    for w in ss.windows(2) {
                        let d = (w[1].p[0] - w[0].p[0]).hypot(w[1].p[2] - w[0].p[2]);
                        if d > 0.1 {
                            slopes.push((w[0].p[1] - w[1].p[1]) / d);
                        }
                    }
                    for s in ss {
                        if (s.w < 5.) != (class == 0) {
                            continue;
                        }
                        let n = [-s.dir[1], s.dir[0]];
                        for (b, &off) in bins.iter().enumerate() {
                            for side in [-1f32, 1.] {
                                let d = if off < 10. {
                                    s.w / 2. * off
                                } else {
                                    s.w / 2. + off - 10.
                                };
                                let (x, z) = (s.p[0] + n[0] * d * side, s.p[2] + n[1] * d * side);
                                if let Some(name) = dominant(x, z) {
                                    *names[b].entry(name).or_default() += 1;
                                }
                                heights[b].push(editor.island.doc.field.height([x, z]) - s.p[1]);
                            }
                        }
                    }
                }
            }
            eprintln!(
                "== {} rivers",
                if class == 0 {
                    "narrow (<5 m)"
                } else {
                    "wide (>=5 m)"
                }
            );
            for (b, off) in bins.iter().enumerate() {
                let mut h = heights[b].clone();
                h.sort_by(f32::total_cmp);
                let q = |f: f32| {
                    h.get(((h.len() as f32 - 1.) * f) as usize)
                        .copied()
                        .unwrap_or(f32::NAN)
                };
                let total: usize = names[b].values().sum();
                let mut top: Vec<_> = names[b].iter().collect();
                top.sort_by(|a, b| b.1.cmp(a.1));
                let top: Vec<String> = top
                    .iter()
                    .take(3)
                    .map(|(k, v)| format!("{k} {}%", **v * 100 / total.max(1)))
                    .collect();
                let label = if *off < 10. {
                    format!("u={off}")
                } else {
                    format!("edge+{}", off - 10.)
                };
                eprintln!(
                    "{label:>9} n={:5} q25 {:+.2} med {:+.2} q75 {:+.2}  {}",
                    h.len(),
                    q(0.25),
                    q(0.5),
                    q(0.75),
                    top.join(", ")
                );
            }
            slopes.sort_by(f32::total_cmp);
            let q = |f: f32| {
                slopes
                    .get(((slopes.len() as f32 - 1.) * f) as usize)
                    .copied()
                    .unwrap_or(f32::NAN)
            };
            eprintln!(
                "surface fall per m: q50 {:.3} q90 {:.3} q99 {:.3}",
                q(0.5),
                q(0.9),
                q(0.99)
            );
        }
    }

    #[test]
    #[ignore]
    fn river_cross_sections() {
        use crate::island::IslandEditor;
        use std::collections::BTreeMap;
        let install = install().expect("ANNO117_DIR");
        // RIVER_ISLANDS=path,path picks others.
        let list = std::env::var("RIVER_ISLANDS").unwrap_or_else(|_| {
            [
                "data/base/provinces/roman/islands/pool/roman_island_large_01/roman_island_large_01.a7m",
                "data/base/provinces/roman/islands/pool/roman_island_small_07/roman_island_small_07.a7m",
                "data/base/provinces/roman/islands/pool/roman_island_medium_01/roman_island_medium_01.a7m",
                "data/base/provinces/celtic/islands/campaign/celtic_island_campaign_player_01/celtic_island_campaign_player_01.a7m",
            ]
            .join(",")
        });
        let islands: Vec<&str> = list.split(',').collect();
        // Offsets from the water's edge: negative is inside the river.
        let bins: [(&str, f32); 7] = [
            ("centre", f32::NAN),
            ("edge-0.5", -0.5),
            ("bank+0.5", 0.5),
            ("bank+1.5", 1.5),
            ("bank+3", 3.),
            ("bank+6", 6.),
            ("far+12", 12.),
        ];
        let mut names: Vec<BTreeMap<String, usize>> = vec![BTreeMap::new(); bins.len()];
        let mut heights: Vec<Vec<f32>> = vec![Vec::new(); bins.len()];
        for path in islands {
            let Ok(editor) = IslandEditor::load(
                crate::games::module(crate::games::of(&install)),
                &install,
                path,
            ) else {
                eprintln!("skip {path}");
                continue;
            };
            let (Some(palette), Some(m)) =
                (editor.palette.clone(), editor.island.doc.materials.clone())
            else {
                continue;
            };
            let dominant = |x: f32, z: f32| -> Option<String> {
                let (ax, az) = ((x * m.per_metre) as usize, (z * m.per_metre) as usize);
                let [aw, ah] = m.paint.size();
                if ax >= aw || az >= ah {
                    return None;
                }
                let id = m.paint.dominant(ax, az);
                Some(palette.materials.get(&id).map_or(format!("#{id}"), |mat| {
                    mat.file.rsplit('/').next().unwrap_or(&mat.file).to_owned()
                }))
            };
            for o in editor
                .island
                .doc
                .objects
                .iter()
                .filter(|o| o.guid == 1001608)
            {
                let Some(curve) = &o.path else { continue };
                for s in samples(curve, false) {
                    let n = [-s.dir[1], s.dir[0]];
                    for (b, &(_, off)) in bins.iter().enumerate() {
                        let sides: &[f32] = if off.is_nan() { &[0.] } else { &[-1., 1.] };
                        for &side in sides {
                            let d = if off.is_nan() { 0. } else { s.w / 2. + off };
                            let (x, z) = (s.p[0] + n[0] * d * side, s.p[2] + n[1] * d * side);
                            if let Some(name) = dominant(x, z) {
                                *names[b].entry(name).or_default() += 1;
                            }
                            heights[b].push(editor.island.doc.field.height([x, z]) - s.p[1]);
                        }
                    }
                }
            }
        }
        for (b, (label, _)) in bins.iter().enumerate() {
            let mut h = heights[b].clone();
            h.sort_by(f32::total_cmp);
            let q = |f: f32| {
                h.get(((h.len() as f32 - 1.) * f) as usize)
                    .copied()
                    .unwrap_or(f32::NAN)
            };
            let total: usize = names[b].values().sum();
            let mut top: Vec<_> = names[b].iter().collect();
            top.sort_by(|a, b| b.1.cmp(a.1));
            let top: Vec<String> = top
                .iter()
                .take(4)
                .map(|(n, c)| format!("{n} {:.0}%", **c as f32 * 100. / total.max(1) as f32))
                .collect();
            eprintln!(
                "{label:>9}: ground-surface p10 {:+.2} p50 {:+.2} p90 {:+.2} | {}",
                q(0.1),
                q(0.5),
                q(0.9),
                top.join(", ")
            );
        }
    }

    /// How many layers of the mesh's water lie over (x, z). Probe off the
    /// lattice and its cell diagonals (x and z fractions differ), where a
    /// point sits on a shared edge and counts for neither triangle.
    fn layers(mesh: &gpui_editor_kit::gpu::Mesh, p: [f32; 2]) -> usize {
        let d = mesh.data();
        d.indices
            .chunks(3)
            .filter(|t| {
                let v: [[f32; 3]; 3] = std::array::from_fn(|k| d.positions[t[k] as usize]);
                let side = |a: [f32; 3], b: [f32; 3]| {
                    (b[0] - a[0]) * (p[1] - a[2]) - (b[2] - a[2]) * (p[0] - a[0])
                };
                let s = [side(v[0], v[1]), side(v[1], v[2]), side(v[2], v[0])];
                s.iter().all(|&x| x > 0.) || s.iter().all(|&x| x < 0.)
            })
            .count()
    }

    fn covers(mesh: &gpui_editor_kit::gpu::Mesh, p: [f32; 2]) -> bool {
        layers(mesh, p) > 0
    }

    #[test]
    fn rivers_and_lakes_get_water_out_to_their_banks() {
        let field = slope();
        let ring: Vec<Knot> = (0..12)
            .map(|k| {
                let a = k as f32 / 12. * std::f32::consts::TAU;
                knot(64. + 15. * a.cos(), 64. + 15. * a.sin())
            })
            .collect();
        let lake = draft(true, ring).curve(&field);
        let river =
            draft(false, vec![knot(10., 20.), knot(60., 24.), knot(110., 20.)]).curve(&field);
        // Runs into the lake from the west.
        let inflow = draft(false, vec![knot(20., 64.3), knot(56., 64.3)]).curve(&field);
        let mesh = water_mesh(&[(&lake, true), (&river, false), (&inflow, false)]).unwrap();

        // The lake is flat at its level and stops just past its shore.
        assert!(covers(&mesh, [64.3, 64.6]) && covers(&mesh, [64.3, 78.6]));
        assert!(!covers(&mesh, [64.3, 64.6 + 15. + WATER_OVERLAP + 1.5]));
        let level = lake[0].p[1];
        let d = mesh.data();
        let flat = (d.positions.iter())
            .filter(|p| p[2] > 40. && (p[1] - level).abs() < 1e-4)
            .count();
        assert!(flat > 700, "{flat} lake corners at its level");
        // One layer where the inflow meets the lake, so it does not darken.
        for x in [30.4, 48.4, 50.4, 52.4, 60.4] {
            assert_eq!(layers(&mesh, [x, 64.6]), 1, "x = {x}");
        }
        // Inside the shore the inflow takes the lake's level.
        let ledge = (d.positions.iter())
            .filter(|p| (52.0..=56.).contains(&p[0]) && (p[2] - 64.).abs() < 1.5)
            .all(|p| (p[1] - level).abs() < 1e-4);
        assert!(ledge, "river surface stands above the lake");
        // The river spans its 6 m plus the overlap on each side (to within a
        // cell), no more.
        assert!(
            covers(&mesh, [60.3, 24.6]) && covers(&mesh, [60.3, 24. + 3. + WATER_OVERLAP - 1.1])
        );
        assert!(!covers(&mesh, [60.3, 24.6 + 3. + WATER_OVERLAP + 1.]));
        assert!(water_mesh(&[]).is_none());
    }

    // 117's lattice: the river's corners flow along it, fastest in the
    // middle by `u0` (8 is the flow map's top speed), slowest at the
    // banks; the lake's corners are still and take the lake's look.
    #[test]
    fn shaded_rivers_flow_by_u0_across_their_width() {
        let field = slope();
        let ring: Vec<Knot> = (0..12)
            .map(|k| {
                let a = k as f32 / 12. * std::f32::consts::TAU;
                knot(64. + 15. * a.cos(), 64. + 15. * a.sin())
            })
            .collect();
        let lake = draft(true, ring).curve(&field);
        let mut river =
            draft(false, vec![knot(10., 20.), knot(60., 24.), knot(110., 20.)]).curve(&field);
        for p in &mut river {
            p.u0 = Some([1., 8., 1.]);
        }
        let look = |lake| water::Look {
            lake,
            fields: Vec::new(),
        };
        let water = inland(&[(&lake, look(true)), (&river, look(false))], None, false).unwrap();
        let at = |x: f32, z: f32| {
            let i = ((x - water.origin[0]) / water.step).round() as usize;
            let j = ((z - water.origin[1]) / water.step).round() as usize;
            water.corners[j * water.size[0] as usize + i]
        };
        let middle = at(60., 24.);
        assert_eq!(middle.look, 1);
        assert!(middle.speed > 7.2, "{middle:?}");
        assert!(middle.flow[0] > 0.99, "flows east: {middle:?}");
        let bank = at(60., 24. + 2.5);
        assert!(bank.speed < 3.2 && bank.speed > 0., "{bank:?}");
        let still = at(64., 64.);
        assert_eq!((still.look, still.speed), (0, 0.));
        assert!(water.looks[0].lake && !water.looks[1].lake);
    }

    // 1800 (water::Water::river_strips): a pool above a waterfall and the
    // river below that starts under its end each draw from their own strip
    // at their own height, one span from bank to bank, while the lattice
    // still joins them for the flow map. The lake stays on the lattice.
    #[test]
    fn river_strips_keep_each_river_at_its_own_height() {
        let point = |x: f32, y: f32| super::PathPoint {
            p: [x, y, 0.],
            d: None,
            i: Some([-1., 0., 0.]),
            o: Some([1., 0., 0.]),
            w: Some(6.),
            u0: Some([1., 3., 1.]),
        };
        let pool = vec![point(0., 19.5), point(10., 19.5), point(20., 19.5)];
        let below = vec![point(18., 7.), point(30., 6.5), point(40., 6.)];
        let ring: Vec<_> = (0..8)
            .map(|k| {
                let a = k as f32 / 8. * std::f32::consts::TAU;
                super::PathPoint {
                    p: [80. + 5. * a.cos(), 3., 5. * a.sin()],
                    d: None,
                    i: None,
                    o: None,
                    w: None,
                    u0: None,
                }
            })
            .collect();
        let look = |lake| water::Look {
            lake,
            fields: Vec::new(),
        };
        let curves = [
            (&pool[..], look(false)),
            (&below[..], look(false)),
            (&ring[..], look(true)),
        ];
        let water = inland(&curves, None, true).unwrap();
        let strips = &water.strips;
        // Three rows per river, two vertices a row, two triangles a stretch.
        assert_eq!(strips.vertices.len(), 2 * 3 * 2);
        assert_eq!(strips.indices.len(), 2 * 2 * 6);
        for v in &strips.vertices {
            let want = match v.look {
                0 => 19.5,
                1 if v.position[0] < 25. => 7.,
                1 => v.position[1],
                other => panic!("look {other} drew a strip"),
            };
            assert!((v.position[1] - want).abs() < 1e-4, "{v:?}");
            assert!(v.position[1] > 5.9, "{v:?}");
        }
        // Without strips (117's water) only lava would come as a strip.
        assert!(
            inland(&curves, None, false)
                .unwrap()
                .strips
                .vertices
                .is_empty()
        );
    }

    // As the game's river strip: straight between path points, the surface
    // sloping linearly from one point's height to the next, the flow turning
    // from one point's tangent to the next through a bend, and the water
    // fading in over its first stretch and out over its last when the
    // record says so (no RenderFlags: both ends; 2: only the end).
    #[test]
    fn shaded_rivers_follow_the_games_strip() {
        let point = |x: f32, z: f32, y: f32, dir: [f32; 2]| super::PathPoint {
            p: [x, y, z],
            d: None,
            i: Some([-dir[0], 0., -dir[1]]),
            o: Some([dir[0], 0., dir[1]]),
            w: Some(4.),
            u0: Some([1., 3., 1.]),
        };
        let diag = std::f32::consts::FRAC_1_SQRT_2;
        // East from (0, 0) through (10, 0) to (30, 0), then a right angle
        // north to (30, 20).
        let river = vec![
            point(0., 0., 4., [1., 0.]),
            point(10., 0., 3., [1., 0.]),
            point(30., 0., 2., [diag, diag]),
            point(30., 20., 0., [0., 1.]),
        ];
        let look = |flags: Option<u32>| water::Look {
            lake: false,
            fields: flags
                .map(|f| vec![("RenderFlags".into(), vec![f32::from_bits(f)])])
                .unwrap_or_default(),
        };
        let water = inland(&[(&river, look(None))], None, false).unwrap();
        let at = |water: &water::Inland, x: f32, z: f32| {
            let i = ((x - water.origin[0]) / water.step).round() as usize;
            let j = ((z - water.origin[1]) / water.step).round() as usize;
            water.corners[j * water.size[0] as usize + i]
        };
        let between = at(&water, 20., 0.);
        assert!((between.y - 2.5).abs() < 0.1, "{between:?}");
        assert!((between.fade - 1.).abs() < 1e-3, "{between:?}");
        // Four fifths of the way to the bend the flow has turned as far
        // toward its diagonal: (0.8, 0.6).
        let turning = at(&water, 26., 0.);
        assert!((turning.flow[1] - 0.6).abs() < 0.1, "{turning:?}");
        // Past it the flow turns on toward north.
        let north = at(&water, 30., 15.);
        assert!(north.flow[1] > 0.9, "{north:?}");
        assert!((north.y - 0.5).abs() < 0.1, "{north:?}");
        // The fade runs over the whole first and last stretch, as the
        // game's vertex alpha does.
        let (source, mouth) = (at(&water, 0., 0.), at(&water, 30., 20.));
        assert!(
            source.fade < 0.05 && mouth.fade < 0.05,
            "{source:?} {mouth:?}"
        );
        assert!((at(&water, 5., 0.).fade - 0.5).abs() < 0.1);
        // A river into the sea fades only at its mouth.
        let water = inland(&[(&river, look(Some(super::FADE_AT_END)))], None, false).unwrap();
        assert_eq!(at(&water, 0., 0.).fade, 1.);
        assert!(at(&water, 30., 20.).fade < 0.05);
        // Reversed, it flows from its last point to its first over the
        // same water, fading at the same ends. The game builds the strip
        // from the last point then, so its quads split the other way and
        // the surface between rows differs by centimetres (7 here, where
        // the mitred row at the bend skews the quad).
        let flags = super::FADE_AT_START | super::FADE_AT_END | super::REVERSE;
        let back = inland(&[(&river, look(Some(flags)))], None, false).unwrap();
        let (fore, rev) = (at(&water, 30., 15.), at(&back, 30., 15.));
        assert!(
            rev.flow[1] < -0.9 && (rev.y - fore.y).abs() < 0.1,
            "{rev:?} {fore:?}"
        );
        // Halfway to the bend it has already turned toward the diagonal,
        // backwards: the same flow, negated, but for the quads' other split.
        let (fore, rev) = (at(&water, 20., 0.), at(&back, 20., 0.));
        assert!(
            (rev.flow[0] + fore.flow[0]).abs() < 0.06
                && (rev.flow[1] + fore.flow[1]).abs() < 0.06
                && rev.flow[0] < -0.9,
            "{rev:?} {fore:?}"
        );
        assert!(at(&back, 0., 0.).fade < 0.05 && at(&back, 30., 20.).fade < 0.05);
    }

    // A lava stream 6 wide east from x 0 to 30 still flows on the lattice
    // but comes as its own strip: its one stretch cut into five rows of
    // eight columns, each vertex knowing where it lies across (the game's
    // IVF_IS_RIGHT_SHORE bank, +0.5, at +Z flowing east), fading over the
    // first fifth. Reversed, it starts flowing at x 30, where a record that
    // fades only the start now fades it, and each bank keeps its side.
    #[test]
    fn lava_streams_draw_as_strips() {
        let point = |x: f32| super::PathPoint {
            p: [x, 1., 0.],
            d: None,
            i: None,
            o: None,
            w: Some(6.),
            u0: Some([1., 3., 1.]),
        };
        let stream = vec![point(0.), point(30.)];
        let look = |flags: u32| water::Look {
            lake: false,
            fields: vec![("RenderFlags".into(), vec![f32::from_bits(flags)])],
        };
        let lava = super::LAVA | super::FADE_AT_START | super::FADE_AT_END;
        let water = inland(&[(&stream, look(lava))], None, false).unwrap();
        let i = ((15. - water.origin[0]) / water.step).round() as usize;
        let j = ((0. - water.origin[1]) / water.step).round() as usize;
        assert!(water.corners[j * water.size[0] as usize + i].flow[0] > 0.99);
        let strips = &water.strips;
        assert_eq!(
            (strips.vertices.len(), strips.indices.len()),
            (6 * 9, 5 * 8 * 6)
        );
        let find = |strips: &water::Strips, x: f32, z: f32| {
            *strips
                .vertices
                .iter()
                .find(|v| (v.position[0] - x).abs() < 1e-3 && (v.position[2] - z).abs() < 1e-3)
                .unwrap()
        };
        let (right, left) = (find(strips, 12., 3.), find(strips, 12., -3.));
        assert!((right.across - 0.5).abs() < 1e-6 && (left.across + 0.5).abs() < 1e-6);
        assert!((right.normal[1] - 1.).abs() < 1e-6);
        assert_eq!(
            (find(strips, 0., 0.).fade, find(strips, 6., 0.).fade),
            (0., 1.)
        );
        let start = super::LAVA | super::FADE_AT_START | super::REVERSE;
        let back = inland(&[(&stream, look(start))], None, false).unwrap();
        let (source, mouth) = (find(&back.strips, 30., 3.), find(&back.strips, 0., 3.));
        assert!((source.across - 0.5).abs() < 1e-6, "{source:?}");
        assert_eq!((source.fade, mouth.fade), (0., 1.));
    }
}

#[cfg(test)]
mod material_roles {
    use crate::island::{IslandEditor, test_support::install};

    // Survey: where retail islands put each ground material (by file
    // name): share of land, slope, height, underwater share, river share.
    // ANNO117_DIR=… cargo test --release material_role_census -- --ignored --nocapture
    #[test]
    #[ignore]
    fn material_role_census() {
        use std::collections::BTreeMap;
        let install = install().expect("ANNO117_DIR");
        let mut paths: Vec<String> = install
            .search(".a7m", 100_000)
            .into_iter()
            .map(|id| install.file_path(id).to_owned())
            .filter(|p| p.ends_with(".a7m") && p.contains("/islands/pool/"))
            .collect();
        paths.sort();
        let region = std::env::var("REGION").unwrap_or_else(|_| "roman".into());
        paths.retain(|p| p.contains(&format!("/{region}/")));
        // name -> (weight, slope*w, height*w, under*w, river*w)
        let mut acc: BTreeMap<String, [f64; 5]> = BTreeMap::new();
        let mut total = 0f64;
        for path in paths.iter().take(12) {
            let Ok(e) = IslandEditor::load(
                crate::games::module(crate::games::of(&install)),
                &install,
                path,
            ) else {
                continue;
            };
            let (Some(p), Some(m)) = (e.palette.clone(), e.island.doc.materials.clone()) else {
                continue;
            };
            let river = e
                .island
                .doc
                .grids
                .as_ref()
                .and_then(|g| g.layer(anno_formats::a7m::grids::Kind::River).cloned());
            let [w, h] = m.paint.size();
            let per = m.per_metre;
            let paint = m.paint.a7m().unwrap();
            let f = &e.island.doc.field;
            for y in (0..h).step_by(8) {
                for x in (0..w).step_by(8) {
                    let (wx, wz) = (x as f32 / per, y as f32 / per);
                    let ht = f.height([wx, wz]);
                    if ht < -6. {
                        continue;
                    }
                    let gx = f.height([wx + 1., wz]) - f.height([wx - 1., wz]);
                    let gz = f.height([wx, wz + 1.]) - f.height([wx, wz - 1.]);
                    let slope = (gx.hypot(gz) / 2.).atan().to_degrees() as f64;
                    let near_river = river
                        .as_ref()
                        .is_some_and(|r| crate::island::grids::value_at(r, wx, wz) != 0);
                    let (ids, a) = paint.slots(x, y);
                    let s: f64 = a.iter().map(|&v| v as f64).sum();
                    if s == 0. {
                        continue;
                    }
                    for k in 0..6 {
                        if a[k] == 0 {
                            continue;
                        }
                        let wgt = a[k] as f64 / s;
                        let name = p
                            .materials
                            .get(&ids[k])
                            .map_or(format!("#{}", ids[k]), |mt| {
                                mt.file
                                    .rsplit('/')
                                    .next()
                                    .unwrap()
                                    .trim_end_matches(".xml")
                                    .trim_end_matches("_dummy")
                                    .to_owned()
                            });
                        let e = acc.entry(name).or_default();
                        e[0] += wgt;
                        e[1] += wgt * slope;
                        e[2] += wgt * ht as f64;
                        e[3] += wgt * (ht < 0.) as u8 as f64;
                        e[4] += wgt * near_river as u8 as f64;
                    }
                    total += 1.;
                }
            }
        }
        let mut rows: Vec<_> = acc.into_iter().collect();
        rows.sort_by(|a, b| b.1[0].total_cmp(&a.1[0]));
        eprintln!("{region}: material, share, mean slope, mean height, underwater, on river");
        for (n, v) in rows {
            eprintln!(
                "{n:26} {:5.1}%  {:5.1}°  {:6.1} m  {:4.0}%  {:4.1}%",
                v[0] / total * 100.,
                v[1] / v[0],
                v[2] / v[0],
                v[3] / v[0] * 100.,
                v[4] / v[0] * 100.
            );
        }
    }

    // Which palette material each template channel gets on a shipped
    // roman and celtic island.
    #[test]
    #[ignore]
    fn template_channels_on_shipped_palettes() {
        use crate::island::worldmachine::template::{CHANNELS, channel_materials};
        let install = install().expect("ANNO117_DIR");
        for path in [
            "data/base/provinces/roman/islands/pool/roman_island_small_07/roman_island_small_07.a7m",
            "data/base/provinces/celtic/islands/pool/celtic_island_small_07/celtic_island_small_07.a7m",
        ] {
            let e = IslandEditor::load(
                crate::games::module(crate::games::of(&install)),
                &install,
                path,
            )
            .unwrap();
            let p = e.palette.clone().unwrap();
            let mats: Vec<(u8, String)> = p
                .materials
                .iter()
                .map(|(&i, m)| (i, m.file.clone()))
                .collect();
            let c = channel_materials(
                &crate::games::rules(crate::games::of(&install)).ground_names,
                &mats,
            );
            eprintln!("{}", path.rsplit('/').next().unwrap());
            for (k, role) in CHANNELS.iter().enumerate() {
                let name = c[k / 4][k % 4]
                    .and_then(|id| p.materials.get(&id))
                    .map_or("-".to_owned(), |m| {
                        m.file.rsplit('/').next().unwrap().to_owned()
                    });
                eprintln!("  {:2} {role:14} {name}", k + 1);
            }
        }
    }
}
