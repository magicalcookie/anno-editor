// Rivers and lakes: the River tool's curve drafts, and committing one as a
// single edit (the `River`/`Lake` EditorObject copied from a shipped
// island, a carved bed, River and Water cells, WaterFlow along a river).
use super::{Drag, IslandEditor, Item, Tool, grids, harbour};
use anno_formats::{
    grids::{Grids, Kind, Layer},
    objects::{Object, PathPoint},
};
use gpui_editor_kit::{
    gpu::{self, Mesh, MeshData},
    gpui::{Bounds, MouseDownEvent, Pixels, Point, Window, point, px, rgba},
    heightfield::Heightfield,
    math::Vec3,
    paint,
};
use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;

/// Width of a new river. Shipped river points are mostly 1–5 m wide.
pub const DEFAULT_WIDTH: f32 = 4.;
/// Drafts are resampled to a point about this often. Shipped rivers have a
/// point every 1–2.6 m, and the game draws long stretches between sparse
/// points as straight water with hard corners.
const SPACING: f32 = 2.;
/// How far (metres, width counted the same) a reopened curve may stray
/// from the carved one when it is thinned to editable knots.
const EDIT_TOLERANCE: f32 = 2.;
/// New points sit this far below the ground they are put on, so the water
/// stays between its banks.
const SURFACE_BELOW: f32 = 0.3;
/// Bed below the surface on the centre line. Shipped river beds sit only
/// 0.1–0.4 m under the water (median of their centre lines).
const BED_DEPTH: f32 = 0.6;
/// Ground at the water's edge relative to the surface. Shipped rivers have
/// it 0.1–0.3 m above: the bank cuts the water before its curve-drawn edge,
/// so the shoreline follows the terrain. Ground below the surface there
/// shows the edge of the water ribbon as a hard straight line.
const EDGE_ABOVE: f32 = 0.3;
/// Lakes keep their shore just under the water, as shipped lakes do.
const EDGE_DEPTH: f32 = 0.3;
/// Riverbed paint reaches this far onto the banks, fading out.
const BED_PAINT_BANK: f32 = 2.;
/// Banks rise from the water's edge with no slope at first, steepening
/// toward `BANK_SLOPE` (27°) over about half a width, until they meet the
/// ground. Carving stops `BANK_MAX` metres past the edge; a trench deeper
/// than the bank reaches there (about 9 m) keeps a step.
const BANK_SLOPE: f32 = 0.5;
const BANK_MAX: f32 = 20.;
/// Lake basins deepen by `LAKE_SLOPE` per metre from the shore down to
/// `LAKE_DEPTH`; their banks ease in over `LAKE_BANK` metres.
const LAKE_DEPTH: f32 = 2.;
const LAKE_SLOPE: f32 = 0.4;
const LAKE_BANK: f32 = 4.;
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
/// mouth. What it does is not known; the middle value rises on some steep
/// stretches.
const U0: [f32; 3] = [1., 3., 1.];
const U0_MOUTH: [f32; 3] = [1., 1., 1.];

/// One point of a draft curve, in island metres.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Knot {
    pub at: [f32; 2],
    /// River width; lakes ignore it.
    pub width: f32,
    /// Water surface; `None` follows the ground under `at`.
    pub surface: Option<f32>,
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
    fn of(o: &Object, lake: bool) -> Option<Draft> {
        let path = o.path.as_ref()?;
        // Carved curves hold a point every couple of metres. Editing keeps
        // only the points the shape needs (within `EDIT_TOLERANCE`, width
        // included), so straight runs show their ends and bends a few
        // handles; carving resamples again.
        let pts: Vec<[f32; 3]> = path
            .iter()
            .map(|p| [p.p[0], p.p[2], p.w.unwrap_or(DEFAULT_WIDTH)])
            .collect();
        let mut keep = simplify(&pts, EDIT_TOLERANCE);
        // A lake needs a shape, not just its ends.
        if lake && keep.len() < 5 {
            keep = (0..6).map(|k| k * path.len() / 6).collect();
        }
        let knots = keep
            .into_iter()
            .map(|i| Knot {
                at: [path[i].p[0], path[i].p[2]],
                width: path[i].w.unwrap_or(DEFAULT_WIDTH),
                surface: Some(path[i].p[1]),
            })
            .collect();
        Some(Draft {
            id: Some(o.id),
            lake,
            knots,
            picked: None,
        })
    }

    /// The curve as the object stores it. River surfaces never rise
    /// downstream, and the first point is the source, as on shipped
    /// rivers; a lake is flat at its lowest point.
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
            if ys[0] < ys[n - 1] {
                knots.reverse();
                ys.reverse();
            }
            for k in 1..n {
                ys[k] = ys[k].min(ys[k - 1]);
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
        (0..n)
            .map(|k| {
                let (prev, next) = if self.lake {
                    (p[(k + n - 1) % n], p[(k + 1) % n])
                } else {
                    (p[k.saturating_sub(1)], p[(k + 1).min(n - 1)])
                };
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
                    u0: (!self.lake).then_some(if k == n - 1 { U0_MOUTH } else { U0 }),
                    d: None,
                }
            })
            .collect()
    }
}

/// Indices of the points Douglas-Peucker keeps: ends, and whatever lies
/// further than `tolerance` from the line between kept neighbours. Points
/// are (x, z, width).
fn simplify(pts: &[[f32; 3]], tolerance: f32) -> Vec<usize> {
    let n = pts.len();
    if n <= 2 {
        return (0..n).collect();
    }
    let mut keep = vec![false; n];
    keep[0] = true;
    keep[n - 1] = true;
    let mut stack = vec![(0, n - 1)];
    while let Some((a, b)) = stack.pop() {
        let (pa, pb) = (pts[a], pts[b]);
        let len2: f32 = (0..3).map(|k| (pb[k] - pa[k]).powi(2)).sum();
        let far = (a + 1..b)
            .map(|i| {
                let p = pts[i];
                let t = if len2 > 0. {
                    ((0..3)
                        .map(|k| (p[k] - pa[k]) * (pb[k] - pa[k]))
                        .sum::<f32>()
                        / len2)
                        .clamp(0., 1.)
                } else {
                    0.
                };
                let d: f32 = (0..3)
                    .map(|k| (p[k] - pa[k] - t * (pb[k] - pa[k])).powi(2))
                    .sum::<f32>()
                    .sqrt();
                (i, d)
            })
            .max_by(|x, y| x.1.total_cmp(&y.1));
        if let Some((i, d)) = far
            && d > tolerance
        {
            keep[i] = true;
            stack.push((a, i));
            stack.push((i, b));
        }
    }
    (0..n).filter(|&i| keep[i]).collect()
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
        for s in 0..steps {
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
                    _ if s == 0 => a.surface,
                    _ => None,
                },
            });
        }
    }
    if !closed {
        out.push(knots[n - 1]);
    }
    out
}

/// A point on the curve: position (y is the water surface), river width,
/// and the ground direction of travel.
#[derive(Clone, Copy, Debug)]
struct Sample {
    p: [f32; 3],
    w: f32,
    dir: [f32; 2],
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
        let steps = (chord.ceil() as usize).max(1);
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
    surface: f32,
    width: f32,
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
            out.insert(
                cell,
                Near {
                    d,
                    surface: a.p[1] + (b.p[1] - a.p[1]) * t,
                    width,
                    dir: a.dir,
                },
            );
        }
    }
    out
}

/// Lattice points inside a closed polygon or within `reach` of its edge:
/// (point, inside, distance to the edge).
fn near_lake(
    polygon: &[[f32; 2]],
    lattice: Lattice,
    reach: f32,
) -> Vec<((usize, usize), bool, f32)> {
    let (mut lo, mut hi) = ([f32::MAX; 2], [f32::MIN; 2]);
    for p in polygon {
        lo = [lo[0].min(p[0] - reach), lo[1].min(p[1] - reach)];
        hi = [hi[0].max(p[0] + reach), hi[1].max(p[1] + reach)];
    }
    let n = polygon.len();
    lattice
        .within(lo, hi)
        .filter_map(|cell| {
            let [x, z] = lattice.at(cell.0, cell.1);
            let mut inside = false;
            let mut d = f32::MAX;
            for k in 0..n {
                let (a, b) = (polygon[k], polygon[(k + 1) % n]);
                if (a[1] > z) != (b[1] > z) && x < a[0] + (z - a[1]) / (b[1] - a[1]) * (b[0] - a[0])
                {
                    inside = !inside;
                }
                let (dx, dz) = (b[0] - a[0], b[1] - a[1]);
                let l2 = dx * dx + dz * dz;
                let t = if l2 > 0. {
                    (((x - a[0]) * dx + (z - a[1]) * dz) / l2).clamp(0., 1.)
                } else {
                    0.
                };
                d = d.min((x - a[0] - t * dx).hypot(z - a[1] - t * dz));
            }
            (inside || d <= reach).then_some((cell, inside, d))
        })
        .collect()
}

/// Height of a bank `x` metres from the water's edge above the edge, for
/// banks that ease in over about `ease` metres.
fn bank(x: f32, ease: f32) -> f32 {
    BANK_SLOPE * x * x / (x + ease.max(0.5))
}

/// Lowers the ground into a river bed: a parabola from `BED_DEPTH` under
/// the surface on the centre line to `EDGE_ABOVE` over it at the edge, so
/// the waterline lands on the bank at about 80 % of the width, then banks
/// until they meet the ground. The profile does not depend on the ground
/// and only ever lowers, so carving the same river again changes nothing.
fn carve_river(field: &mut Heightfield, samples: &[Sample]) {
    let lattice = Lattice::heights(field);
    for ((x, z), n) in near_river(samples, lattice, |w| w / 2. + BANK_MAX) {
        let i = field.grid.index(x, z);
        let old = field.heights[i];
        let half = n.width / 2.;
        let edge = n.surface + EDGE_ABOVE;
        let target = if n.d <= half {
            let u = n.d / half.max(1e-3);
            edge - (BED_DEPTH + EDGE_ABOVE) * (1. - u * u)
        } else {
            edge + bank(n.d - half, half)
        };
        if target < old {
            field.heights[i] = target;
        }
    }
}

/// Lowers a basin under a lake at `level`, deepening away from the shore.
fn carve_lake(field: &mut Heightfield, polygon: &[[f32; 2]], level: f32) {
    let lattice = Lattice::heights(field);
    let edge = level - EDGE_DEPTH;
    for ((x, z), inside, d) in near_lake(polygon, lattice, BANK_MAX) {
        let i = field.grid.index(x, z);
        let old = field.heights[i];
        let target = if inside {
            level - LAKE_DEPTH.min(EDGE_DEPTH + d * LAKE_SLOPE)
        } else {
            edge + bank(d, LAKE_BANK)
        };
        if target < old {
            field.heights[i] = target;
        }
    }
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

/// Sets or clears River and Water inside a lake, as [`paint_river`].
fn paint_lake(grids: &mut Grids, polygon: &[[f32; 2]], on: bool) {
    for kind in [Kind::River, Kind::Water, Kind::Ford] {
        if let Some(layer) = grids.layer_mut(kind) {
            for ((x, y), inside, _) in near_lake(polygon, Lattice::cells(layer), 0.) {
                if inside {
                    mark(layer, x, y, on);
                }
            }
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

/// One translucent surface for every river and lake, so they read as water
/// in the viewport. A stand-in: the game draws them with its own shader
/// and flow maps. Built on one lattice with the nearest curve's surface at
/// each corner, so where a river meets a lake, or a wide river bends, no
/// two layers of water overlap and darken.
pub(super) fn water_mesh(curves: &[(&[PathPoint], bool)]) -> Option<Arc<Mesh>> {
    let shapes: Vec<(Vec<Sample>, bool)> = curves
        .iter()
        .map(|&(c, lake)| (samples(c, lake), lake))
        .filter(|(s, _)| s.len() >= 2)
        .collect();
    let (mut lo, mut hi) = ([f32::MAX; 2], [f32::MIN; 2]);
    for s in shapes.iter().flat_map(|(s, _)| s) {
        let r = s.w / 2. + WATER_OVERLAP + WATER_CELL;
        lo = [lo[0].min(s.p[0] - r), lo[1].min(s.p[2] - r)];
        hi = [hi[0].max(s.p[0] + r), hi[1].max(s.p[2] + r)];
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
    // Per corner: (distance past the water's edge, surface). Negative is on
    // the water; the smallest wins.
    let mut best: Vec<Option<(f32, f32)>> = vec![None; w * d];
    let mut offer = |i: usize, past: f32, y: f32| {
        if past < WATER_OVERLAP && best[i].is_none_or(|(b, _)| past < b) {
            best[i] = Some((past, y));
        }
    };
    for (s, lake) in &shapes {
        if *lake {
            let poly = polygon(s);
            let (mut a, mut b) = ([f32::MAX; 2], [f32::MIN; 2]);
            for q in &poly {
                a = [a[0].min(q[0]), a[1].min(q[1])];
                b = [b[0].max(q[0]), b[1].max(q[1])];
            }
            let r = WATER_OVERLAP;
            for z in span(a[1] - r, 1, d)..=span(b[1] + r, 1, d) {
                for x in span(a[0] - r, 0, w)..=span(b[0] + r, 0, w) {
                    let c = corner(x, z);
                    let edge = poly
                        .iter()
                        .zip(poly.iter().cycle().skip(1))
                        .map(|(p, q)| segment_distance(c, *p, *q))
                        .fold(f32::MAX, f32::min);
                    // Inside its shore a lake beats any river, whose
                    // surface would stand a little above it as a ledge.
                    let past = if inside(&poly, c) { -edge - 1e4 } else { edge };
                    offer(z * w + x, past, s[0].p[1]);
                }
            }
        } else {
            // Samples are about a metre apart, so discs at each one join
            // into the ribbon.
            for k in s {
                let r = k.w / 2. + WATER_OVERLAP;
                for z in span(k.p[2] - r, 1, d)..=span(k.p[2] + r, 1, d) {
                    for x in span(k.p[0] - r, 0, w)..=span(k.p[0] + r, 0, w) {
                        let c = corner(x, z);
                        let past = (c[0] - k.p[0]).hypot(c[1] - k.p[2]) - k.w / 2.;
                        offer(z * w + x, past, k.p[1]);
                    }
                }
            }
        }
    }
    let mut positions: Vec<Vec3> = Vec::new();
    let mut index = vec![u32::MAX; w * d];
    for (i, b) in best.iter().enumerate() {
        if let Some((_, y)) = b {
            index[i] = positions.len() as u32;
            let [x, z] = corner(i % w, i / w);
            positions.push([x, *y, z]);
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
        }),
        ..Default::default()
    })
    .map_err(|e| eprintln!("inland water mesh: {e:#}"))
    .ok()
    .map(Arc::new)
}

/// Writes a curve's cells (or clears them) into the grids.
pub(super) fn paint_curve(grids: &mut Grids, curve: &[PathPoint], lake: bool, on: bool) {
    let s = samples(curve, lake);
    if lake {
        paint_lake(grids, &polygon(&s), on);
    } else {
        paint_river(grids, &s, on);
    }
}

/// The palette material for river beds: roman sets have
/// `river_pebbles_01`; else what shipped islands put under and beside
/// rivers (`gravel_round_01`/`beach_01` celtic, `beach_04` roman), else mud.
fn bed_material(palette: &crate::terrain::Palette) -> Option<u8> {
    [
        "river_pebbles",
        "gravel_round",
        "beach_04",
        "beach_01",
        "gravel",
        "beach",
        "mud",
    ]
    .iter()
    .find_map(|want| {
        palette.materials.iter().find_map(|(&id, m)| {
            let stem = m.file.rsplit('/').next().unwrap_or(&m.file);
            stem.starts_with(want).then_some(id)
        })
    })
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
    fn river_step(&mut self) {
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
        if let Some(lib) = &self.harbour {
            if lib.guid(harbour::Piece::River) == Some(guid) {
                return Some(false);
            }
            if lib.guid(harbour::Piece::Lake) == Some(guid) {
                return Some(true);
            }
        }
        match harbour::template_of(self.assets.as_deref()?, guid).as_str() {
            "River" => Some(false),
            "Lake" => Some(true),
            _ => None,
        }
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
        draft.knots.push(Knot {
            at,
            width,
            surface: None,
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
        let Some(o) = self.doc.objects.iter().find(|o| o.id == id) else {
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
            if let Some(d) = &mut self.river {
                d.picked = Some(index);
            }
            return Some(Drag::RiverKnot { index, handle });
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
        self.river_add([at[0], at[2]]);
        None
    }

    /// Moves a knot, or sets its width from the cursor's distance.
    pub(super) fn river_drag(&mut self, index: usize, handle: Handle, at: Vec3) {
        let Some(k) = self.river.as_mut().and_then(|d| d.knots.get_mut(index)) else {
            return;
        };
        match handle {
            Handle::Point => {
                k.at = [at[0], at[2]];
                // Back on the ground it now stands on.
                k.surface = None;
            }
            Handle::Width => {
                k.width = (2. * (at[0] - k.at[0]).hypot(at[2] - k.at[1])).clamp(1., 60.);
            }
        }
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
        }
    }

    /// Paints the riverbed material under the water, fading out over
    /// `BED_PAINT_BANK` metres of bank, and refreshes the splat there.
    fn paint_bed(&mut self, samples: &[Sample], lake: bool) {
        let (Some(palette), Some(m)) = (self.palette.clone(), &mut self.doc.materials) else {
            return;
        };
        let Some(id) = bed_material(&palette) else {
            return;
        };
        let (mut lo, mut hi) = ([f32::MAX; 2], [f32::MIN; 2]);
        for s in samples {
            // Lakes have no width; their dabs cover the shore band.
            let half = if lake { 1.5 } else { s.w / 2. };
            let radius = half + BED_PAINT_BANK;
            let brush = gpui_editor_kit::heightfield::Brush {
                radius,
                inner: half / radius,
                strength: 0.8,
                ..Default::default()
            };
            m.paint([s.p[0], s.p[2]], &brush, id);
            lo = [lo[0].min(s.p[0] - radius), lo[1].min(s.p[2] - radius)];
            hi = [hi[0].max(s.p[0] + radius), hi[1].max(s.p[2] + radius)];
        }
        if let Some(splat) = &self.splat {
            let size = self.doc.field.grid.size()[0];
            palette.update_splat(splat, &m.ids, &m.alphas, size, [lo[0], lo[1], hi[0], hi[1]]);
        }
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
        let Some(lib) = self.harbour.clone() else {
            return self.say_no_records("River");
        };
        let piece = if draft.lake {
            harbour::Piece::Lake
        } else {
            harbour::Piece::River
        };
        let existing = draft
            .id
            .and_then(|id| self.doc.objects.iter().position(|o| o.id == id));
        if existing.is_none() && (lib.guid(piece).is_none() || self.object_store.is_none()) {
            return self.say(format!("No shipped {what} record to copy"), true);
        }
        let curve = draft.curve(&self.doc.field);
        let before = self.doc.clone();
        if let Some(g) = &mut self.doc.grids {
            let g = Arc::make_mut(g);
            // The old shape's cells go first, so a moved river leaves no
            // stray water behind. Its old bed stays.
            if let Some(old) = existing.and_then(|i| self.doc.objects[i].path.as_deref()) {
                paint_curve(g, old, draft.lake, false);
            }
            paint_curve(g, &curve, draft.lake, true);
        }
        let s = samples(&curve, draft.lake);
        if draft.lake {
            carve_lake(
                Arc::make_mut(&mut self.doc.field),
                &polygon(&s),
                curve[0].p[1],
            );
        } else {
            carve_river(Arc::make_mut(&mut self.doc.field), &s);
        }
        self.paint_bed(&s, draft.lake);
        let id = match existing {
            Some(i) => {
                let o = &mut Arc::make_mut(&mut self.doc.objects)[i];
                o.position = curve[0].p;
                o.path = Some(curve);
                o.id
            }
            None => {
                let store = self.object_store.as_mut().expect("checked above");
                store.objects.clone_from(&self.doc.objects);
                let areas = self.doc.grids.as_ref().and_then(|g| g.layer(Kind::AreaIds));
                let mut o = lib
                    .place(store, piece, areas, curve[0].p)
                    .expect("checked above");
                o.path = Some(curve);
                let id = o.id;
                Arc::make_mut(&mut self.doc.objects).push(o);
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
        self.say(
            format!("{what} written: bed carved, River and Water cells painted"),
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
        let ground = |p: [f32; 2]| [p[0], self.doc.field.height(p) + 0.3, p[1]];
        let n = d.knots.len();
        let mut out = Vec::new();
        for (i, k) in d.knots.iter().enumerate() {
            out.push((
                i,
                Handle::Point,
                v.camera.project(ground(k.at), v.bounds, extent),
            ));
            if d.lake {
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
        self.doc
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
        for o in self.doc.objects.iter() {
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
    pub(super) fn water_lines(&self) -> Vec<gpu::Line> {
        let editing = self.river.as_ref().and_then(|d| d.id);
        let lit =
            |id: u64| self.hover_water == Some(id) || self.selection.contains(&Item::Object(id));
        let mut lines = Vec::new();
        for o in self.doc.objects.iter() {
            let (Some(lake), Some(path)) = (self.water_kind(o.guid), &o.path) else {
                continue;
            };
            if Some(o.id) == editing {
                continue;
            }
            let s = samples(path, lake);
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
                lines.extend(self.banks(&s).map(|points| gpu::Line {
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
                    [x, self.doc.field.height([x, z]).max(s.p[1]) + 0.1, z]
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
        let curve = d.curve(&self.doc.field);
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
                    let r = if picked { 5. } else { 3.5 };
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
            self.doc
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
            v.camera
                .project([x, self.doc.field.height([x, z]) + 0.2, z], bounds, extent)
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

#[cfg(test)]
mod tests {
    use super::{BED_DEPTH, Draft, Knot, WATER_OVERLAP, carve_river, samples, water_mesh};
    use crate::island::test_support::{
        assets, harbour_library, install, layer, new_island, objects, saved, staged,
    };
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
        editor.harbour = Some(harbour_library(&install, &assets, &editor));
        let c = editor.doc.field.grid.size()[0] / 2.;
        let knots = |e: &IslandEditor| e.river.as_ref().map(|d| d.knots.len());
        let objects = |e: &IslandEditor| e.doc.objects.len();
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
        editor.river_drag(0, super::Handle::Point, [c - 40., 0., c + 5.]);
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

    #[test]
    fn carving_lowers_a_bed_and_never_raises() {
        let mut field = slope();
        let before = field.clone();
        let curve =
            draft(false, vec![knot(10., 64.), knot(60., 70.), knot(110., 64.)]).curve(&field);
        let s = samples(&curve, false);
        carve_river(&mut field, &s);
        assert!(
            field
                .heights
                .iter()
                .zip(&before.heights)
                .all(|(a, b)| a <= b)
        );
        let mid = s[s.len() / 2];
        let bed = field.height([mid.p[0], mid.p[2]]);
        assert!(
            (bed - (mid.p[1] - BED_DEPTH)).abs() < 0.15,
            "bed {bed} surface {}",
            mid.p[1]
        );
        assert_eq!(field.height([60., 10.]), before.height([60., 10.]));
        let once = field.heights.clone();
        carve_river(&mut field, &s);
        assert_eq!(field.heights, once, "carving again changes nothing");
    }

    // Needs the game: a river drawn across a new island and a lake beside
    // it save as River/Lake objects with their curves, a carved bed, River
    // and Water cells and (for the river) WaterFlow, each one undo step.
    #[test]
    fn rivers_and_lakes_carve_and_save_on_a_new_island() {
        use super::super::{IslandEditor, Start, Tool, harbour};
        use anno_formats::grids::{Grids, Kind};
        let Some(install) = install() else {
            return;
        };
        let assets = assets(&install);
        let mut editor = new_island(&install, "roman", 320, Start::Hills, "River Test");
        editor.tool = Tool::River;
        assert!(editor.needs_harbour_library());
        editor.harbour = Some(harbour_library(&install, &assets, &editor));
        let lib = editor.harbour.clone().unwrap();
        let (river_guid, lake_guid) = (
            lib.guid(harbour::Piece::River).expect("a shipped river"),
            lib.guid(harbour::Piece::Lake).expect("a shipped lake"),
        );

        // West to east across the middle, 5 points.
        let c = editor.doc.field.grid.size()[0] / 2.;
        let knots: Vec<[f32; 2]> = (0..5)
            .map(|k| [c - 60. + k as f32 * 30., c - 30. + (k % 2) as f32 * 8.])
            .collect();
        let before = editor.doc.clone();
        for &k in &knots {
            assert!(editor.doc.field.height(k) > 0.5, "on land: {k:?}");
            editor.river_add(k);
        }
        editor.finish_river();
        let msg = editor.message.take().unwrap();
        assert!(!msg.1, "{}", msg.0);
        assert!(editor.river.is_none());
        let rivers =
            |e: &IslandEditor, guid: u32| e.doc.objects.iter().filter(|o| o.guid == guid).count();
        let id_of =
            |e: &IslandEditor, guid: u32| e.doc.objects.iter().find(|o| o.guid == guid).unwrap().id;
        assert_eq!(rivers(&editor, river_guid), 1);
        // One undo step takes object, bed and cells back.
        let carved = editor.doc.field.heights.clone();
        editor.undo(false);
        assert_eq!(rivers(&editor, river_guid), 0);
        assert!(editor.doc.field.heights == before.field.heights);
        let river_at = |e: &IslandEditor, p: [f32; 2]| {
            super::grids::value_at(layer(&e.doc, Kind::River), p[0], p[1])
        };
        assert_eq!(river_at(&editor, knots[2]), 0);
        editor.undo(true);
        assert!(editor.doc.field.heights == carved);

        // Re-editing keeps the object and moves its cells.
        assert!(editor.edit_river(id_of(&editor, river_guid)));
        let draft = editor.river.as_mut().unwrap();
        // Five clicks were carved into a point every 2 m; editing gets back
        // about as many handles as the shape needs, not dozens.
        assert!(
            (5..=14).contains(&draft.knots.len()),
            "{} knots",
            draft.knots.len()
        );
        let last = draft.knots.len() - 1;
        let old_end = draft.knots[last].at;
        draft.knots[last].at[1] += 25.;
        draft.knots[last].surface = None;
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
        let bed = super::bed_material(editor.palette.as_ref().unwrap()).expect("a bed material");
        let materials = editor.doc.materials.as_ref().unwrap();
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

    // Survey, not a pass/fail test: ground material and terrain height
    // relative to the water across shipped rivers.
    // ANNO117_DIR=… cargo test --release river_cross_sections -- --ignored --nocapture
    #[test]
    #[ignore]
    fn river_cross_sections() {
        use crate::island::IslandEditor;
        use anno_formats::island::unpack_slots;
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
            let Ok(editor) = IslandEditor::load(&install, path) else {
                eprintln!("skip {path}");
                continue;
            };
            let (Some(palette), Some(m)) = (editor.palette.clone(), editor.doc.materials.clone())
            else {
                continue;
            };
            let dominant = |x: f32, z: f32| -> Option<String> {
                let (ax, az) = ((x * m.per_metre) as usize, (z * m.per_metre) as usize);
                let [aw, ah] = m.alphas.size();
                if ax >= aw || az >= ah {
                    return None;
                }
                let w = unpack_slots(m.alphas.get(ax, az));
                let ids = unpack_slots(m.ids.get(ax / 2, az / 2));
                let k = (0..6).max_by_key(|&k| w[k])?;
                let id = ids[k];
                Some(palette.materials.get(&id).map_or(format!("#{id}"), |mat| {
                    mat.file.rsplit('/').next().unwrap_or(&mat.file).to_owned()
                }))
            };
            for o in editor.doc.objects.iter().filter(|o| o.guid == 1001608) {
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
                            heights[b].push(editor.doc.field.height([x, z]) - s.p[1]);
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
        use anno_formats::island::unpack_slots;
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
            let Ok(e) = IslandEditor::load(&install, path) else {
                continue;
            };
            let (Some(p), Some(m)) = (e.palette.clone(), e.doc.materials.clone()) else {
                continue;
            };
            let river = e
                .doc
                .grids
                .as_ref()
                .and_then(|g| g.layer(anno_formats::grids::Kind::River).cloned());
            let [w, h] = m.alphas.size();
            let per = m.per_metre;
            let f = &e.doc.field;
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
                    let ids = unpack_slots(m.ids.get(x / 2, y / 2));
                    let a = unpack_slots(m.alphas.get(x, y));
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
            let e = IslandEditor::load(&install, path).unwrap();
            let p = e.palette.clone().unwrap();
            let mats: Vec<(u8, String)> = p
                .materials
                .iter()
                .map(|(&i, m)| (i, m.file.clone()))
                .collect();
            let c = channel_materials(install.game(), &mats);
            eprintln!("{}", path.rsplit('/').next().unwrap());
            for (k, (role, _)) in CHANNELS.iter().enumerate() {
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
