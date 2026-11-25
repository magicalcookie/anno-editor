// What makes an island settleable, as read from every shipped pool island:
// harbour spots (a `Coastline` curve on the waterline with a pad of
// construction area reaching ~29 m into the sea), resource slots, a player
// island marker, and an `AreaIDs` grid where construction land meets open
// sea only at those pads. Records are copied from the province's shipped
// islands, since a new island has none to copy from; the same library
// carries the River and Lake records the River tool copies.
use super::doc::Doc;
use anno_core::game::{IslandRules, Module};
use anno_core::harbour::Piece;
pub use anno_core::harbour::{BASE, CONSTRUCTION, Library, Resource, template_of};
use anno_core::island::objects::{Object, Objects, PathPoint, Template};
use anno_formats::{
    a7m::{
        a7minfo::{RandomSlotGroup, RandomSlotObject},
        grids::{Kind, Layer},
    },
    adf::Adf,
    assets::Assets,
    rda::Archive,
    vfs::Install,
};
use anyhow::Result;
use gpui_editor_kit::heightfield::Heightfield;
use std::collections::{HashMap, VecDeque};
use std::ops::Range;

/// Shipped pads reach 27–31 m from their curve into the sea.
const PAD_RADIUS: f32 = 29.;
/// Base-area band between construction land and sea outside the pads, in
/// 1 m cells; shipped islands never let the two touch.
const RIM: u32 = 3;
/// Shipped harbour curves: typically 25–75 m long, points 5–15 m apart.
const COAST_LENGTH: f32 = 48.;
const COAST_STEP: f32 = 10.;
/// Shipped curves sit on the waterline.
const COAST_HEIGHT: [f32; 2] = [0.2, 0.8];

/// Shipped records for `guids`, from the first island of any province that
/// has each. For objects whose record carries nothing regional (sound spots,
/// build blockers): large pool islands first, where most of them are, so a
/// handful of islands are read (~50 ms each). Missing GUIDs are left out.
pub fn find_records(
    rules: &IslandRules,
    install: &Install,
    guids: &[u32],
) -> Result<HashMap<u32, Template>> {
    let mut paths: Vec<String> = install
        .search(".a7m", 10_000)
        .into_iter()
        .map(|id| install.file_path(id).to_owned())
        .filter(|p| p.ends_with(".a7m") && p.contains("/islands/"))
        .collect();
    paths.sort_by_key(|p| (!rules.is_pool(p), !rules.is_large(p), p.clone()));
    let mut found: HashMap<u32, Template> = HashMap::new();
    for path in paths {
        if guids.iter().all(|g| found.contains_key(g)) {
            break;
        }
        let read = || -> Result<Objects> {
            let archive = Archive::parse(install.read(&path)?, &path)?;
            Objects::read(&Adf::parse(&archive.read_path("gamedata.data")?)?)
        };
        let Ok(store) = read() else { continue };
        for (i, o) in store.objects.iter().enumerate() {
            // A record without an editor folder, when there is one: a
            // folder ID means nothing on another island.
            let better = match found.get(&o.guid) {
                None => true,
                Some(t) => t.object.folder.is_some() && o.folder.is_none(),
            };
            if guids.contains(&o.guid) && better {
                found.insert(o.guid, store.template(i));
            }
        }
    }
    Ok(found)
}

/// Which piece an object of `guid` plays on `module`'s islands, by its
/// asset's template and name: how the island converter recognises slots,
/// coasts and water on an island of the other game.
pub fn piece_of(module: &dyn Module, assets: &Assets, guid: u32) -> Option<Piece> {
    let name = &assets.get(guid)?.name;
    module.islands()?.piece_of(&template_of(assets, guid), name)
}

/// `.a7minfo` `RandomSlotObjects`: every slot object (template `Slot`,
/// random or fixed) by asset GUID, ascending as shipped. The map generator
/// picks mines and river spots from this list.
pub fn slot_groups(objects: &[Object], is_slot: impl Fn(u32) -> bool) -> Vec<RandomSlotGroup> {
    let mut groups: std::collections::BTreeMap<u32, Vec<RandomSlotObject>> = Default::default();
    for o in objects {
        if is_slot(o.guid) {
            groups.entry(o.guid).or_default().push(RandomSlotObject {
                object_id: o.id,
                position: o.position,
            });
        }
    }
    groups
        .into_iter()
        .map(|(guid, objects)| RandomSlotGroup { guid, objects })
        .collect()
}

/// `.a7minfo` `StartCoastDirection`: the angle from the land's centre to
/// the start coast, `atan2(z, x)` in 0..2π (within 0.1 rad of the stored
/// value on most shipped start islands).
pub fn start_coast_direction(curve: &[PathPoint], centre: [f32; 2]) -> f32 {
    let mid = curve[curve.len() / 2].p;
    (mid[2] - centre[1])
        .atan2(mid[0] - centre[0])
        .rem_euclid(std::f32::consts::TAU)
}

/// The waterline on the 1 m cell grid: land cells (height above 0) with a
/// 4-neighbour at or below sea level.
struct Shore<'a> {
    field: &'a Heightfield,
    size: [i32; 2],
}

impl<'a> Shore<'a> {
    fn new(field: &'a Heightfield) -> Self {
        let [w, d] = field.grid.size();
        Shore {
            field,
            size: [w as i32, d as i32],
        }
    }

    fn land(&self, x: i32, z: i32) -> bool {
        self.field.height([x as f32 + 0.5, z as f32 + 0.5]) > 0.
    }

    fn inside(&self, x: i32, z: i32) -> bool {
        x >= 0 && z >= 0 && x < self.size[0] && z < self.size[1]
    }

    fn is_shore(&self, x: i32, z: i32) -> bool {
        self.inside(x, z)
            && self.land(x, z)
            && [(1, 0), (-1, 0), (0, 1), (0, -1)]
                .iter()
                .any(|(dx, dz)| !self.land(x + dx, z + dz))
    }

    /// Shore cell nearest `at` within `reach` cells.
    fn nearest(&self, at: [f32; 2], reach: i32) -> Option<(i32, i32)> {
        let (cx, cz) = (at[0] as i32, at[1] as i32);
        (-reach..=reach)
            .flat_map(|dz| (-reach..=reach).map(move |dx| (cx + dx, cz + dz)))
            .filter(|&(x, z)| self.is_shore(x, z))
            .min_by_key(|&(x, z)| (x - cx).pow(2) + (z - cz).pow(2))
    }

    fn neighbours(&self, (x, z): (i32, i32)) -> impl Iterator<Item = (i32, i32)> + '_ {
        (-1..=1)
            .flat_map(move |dz| (-1..=1).map(move |dx| (x + dx, z + dz)))
            .filter(move |&n| n != (x, z) && self.is_shore(n.0, n.1))
    }

    /// Shortest run of shore cells from `a` to `b`, up to `max` cells.
    fn path(&self, a: (i32, i32), b: (i32, i32), max: usize) -> Option<Vec<(i32, i32)>> {
        let mut prev: HashMap<(i32, i32), (i32, i32)> = HashMap::from([(a, a)]);
        let mut queue = VecDeque::from([(a, 0usize)]);
        while let Some((at, n)) = queue.pop_front() {
            if at == b {
                let mut out = vec![b];
                let mut c = b;
                while c != a {
                    c = prev[&c];
                    out.push(c);
                }
                out.reverse();
                return Some(out);
            }
            if n >= max {
                continue;
            }
            for next in self.neighbours(at) {
                if let std::collections::hash_map::Entry::Vacant(e) = prev.entry(next) {
                    e.insert(at);
                    queue.push_back((next, n + 1));
                }
            }
        }
        None
    }

    /// Cells walked from `start` both ways along the shore, `half` metres
    /// each way, preferring to keep direction.
    fn around(&self, start: (i32, i32), half: f32) -> Vec<(i32, i32)> {
        let mut seen = std::collections::HashSet::from([start]);
        let walk = |seen: &mut std::collections::HashSet<(i32, i32)>| {
            let mut out = Vec::new();
            let (mut at, mut dir) = (start, (0, 0));
            let mut length = 0.;
            while length < half {
                let next = self
                    .neighbours(at)
                    .filter(|n| !seen.contains(n))
                    .max_by_key(|&(x, z)| (x - at.0) * dir.0 + (z - at.1) * dir.1);
                let Some(n) = next else { break };
                length += (((n.0 - at.0).pow(2) + (n.1 - at.1).pow(2)) as f32).sqrt();
                dir = (n.0 - at.0, n.1 - at.1);
                seen.insert(n);
                out.push(n);
                at = n;
            }
            out
        };
        let ahead = walk(&mut seen);
        let behind = walk(&mut seen);
        behind
            .into_iter()
            .rev()
            .chain(std::iter::once(start))
            .chain(ahead)
            .collect()
    }
}

/// A harbour curve along the waterline through the shore nearest `at`, or
/// None when there is no shore within reach.
pub fn trace_coast(field: &Heightfield, at: [f32; 2]) -> Option<Vec<PathPoint>> {
    let shore = Shore::new(field);
    let start = shore.nearest(at, 24)?;
    curve_from_cells(field, &shore.around(start, COAST_LENGTH / 2.))
}

/// A harbour curve following the shore from near `a` to near `b`.
pub fn trace_between(field: &Heightfield, a: [f32; 2], b: [f32; 2]) -> Option<Vec<PathPoint>> {
    // 300 m of shore is far longer than any shipped harbour (≤ 120 m).
    const MAX_CELLS: usize = 300;
    let shore = Shore::new(field);
    let (from, to) = (shore.nearest(a, 24)?, shore.nearest(b, 24)?);
    curve_from_cells(field, &shore.path(from, to, MAX_CELLS)?)
}

/// `at` moved onto the nearest waterline cell within 12 m.
pub fn snap_to_shore(field: &Heightfield, at: [f32; 2]) -> Option<[f32; 2]> {
    let (x, z) = Shore::new(field).nearest(at, 12)?;
    Some([x as f32 + 0.5, z as f32 + 0.5])
}

fn curve_from_cells(field: &Heightfield, cells: &[(i32, i32)]) -> Option<Vec<PathPoint>> {
    let cells: Vec<[f32; 2]> = cells
        .iter()
        .map(|&(x, z)| [x as f32 + 0.5, z as f32 + 0.5])
        .collect();
    if cells.len() < 8 {
        return None;
    }
    // Smooth the staircase, then resample at even spacing.
    let smooth: Vec<[f32; 2]> = (0..cells.len())
        .map(|i| {
            let r = &cells[i.saturating_sub(3)..(i + 4).min(cells.len())];
            let n = r.len() as f32;
            [
                r.iter().map(|c| c[0]).sum::<f32>() / n,
                r.iter().map(|c| c[1]).sum::<f32>() / n,
            ]
        })
        .collect();
    let mut points = vec![smooth[0]];
    let mut carried = 0.;
    for w in smooth.windows(2) {
        carried += (w[1][0] - w[0][0]).hypot(w[1][1] - w[0][1]);
        if carried >= COAST_STEP {
            points.push(w[1]);
            carried = 0.;
        }
    }
    if carried > COAST_STEP / 2. {
        points.push(*smooth.last().unwrap());
    }
    (points.len() >= 3).then(|| with_directions(field, &points))
}

/// Curve points through `points`: waterline height, and `d` toward the sea.
pub fn with_directions(field: &Heightfield, points: &[[f32; 2]]) -> Vec<PathPoint> {
    let sea = |x: f32, z: f32| field.height([x, z]) <= 0.;
    (0..points.len())
        .map(|i| {
            let (a, b) = (
                points[i.saturating_sub(1)],
                points[(i + 1).min(points.len() - 1)],
            );
            let (tx, tz) = (b[0] - a[0], b[1] - a[1]);
            let l = tx.hypot(tz).max(1e-3);
            let (mut nx, mut nz) = (tz / l, -tx / l);
            let [x, z] = points[i];
            if !sea(x + nx * 6., z + nz * 6.) {
                (nx, nz) = (-nx, -nz);
            }
            PathPoint {
                p: [
                    x,
                    field.height([x, z]).clamp(COAST_HEIGHT[0], COAST_HEIGHT[1]),
                    z,
                ],
                d: Some(snap_direction(nx, nz)),
                ..Default::default()
            }
        })
        .collect()
}

/// `d` of shipped curve points: the seaward direction snapped to 45°.
fn snap_direction(x: f32, z: f32) -> [f32; 2] {
    let step = std::f32::consts::FRAC_PI_4;
    let a = (z.atan2(x) / step).round() * step;
    let r = |v: f32| if v.abs() < 1e-6 { 0. } else { v };
    [r(a.cos()), r(a.sin())]
}

/// A region of the island to auto-place in, in metres: a dragged
/// rectangle, or a lasso outline traced along a ridge or a river bank.
#[derive(Clone, Debug, PartialEq)]
pub struct Region {
    pub min: [f32; 2],
    pub max: [f32; 2],
    /// Lasso outline, closed back to its first point; `min`/`max` are its
    /// bounds. `None` is the whole rectangle.
    pub outline: Option<Vec<[f32; 2]>>,
}

impl Region {
    pub fn spanning(a: [f32; 2], b: [f32; 2]) -> Region {
        Region {
            min: [a[0].min(b[0]), a[1].min(b[1])],
            max: [a[0].max(b[0]), a[1].max(b[1])],
            outline: None,
        }
    }

    /// The area inside a traced outline; `None` under three points, which
    /// enclose nothing.
    pub fn lasso(points: &[[f32; 2]]) -> Option<Region> {
        if points.len() < 3 {
            return None;
        }
        let mut min = [f32::MAX; 2];
        let mut max = [f32::MIN; 2];
        for p in points {
            min = [min[0].min(p[0]), min[1].min(p[1])];
            max = [max[0].max(p[0]), max[1].max(p[1])];
        }
        Some(Region {
            min,
            max,
            outline: Some(points.to_vec()),
        })
    }

    fn contains(&self, p: [f32; 2]) -> bool {
        let in_box = (self.min[0]..=self.max[0]).contains(&p[0])
            && (self.min[1]..=self.max[1]).contains(&p[1]);
        in_box && self.outline.as_deref().is_none_or(|o| inside(o, p))
    }

    /// Cell centres every `step` metres inside the region.
    fn samples(&self, step: f32) -> impl Iterator<Item = [f32; 2]> + '_ {
        let nx = ((self.max[0] - self.min[0]) / step) as usize;
        let nz = ((self.max[1] - self.min[1]) / step) as usize;
        (0..=nz)
            .flat_map(move |j| {
                (0..=nx)
                    .map(move |i| [self.min[0] + i as f32 * step, self.min[1] + j as f32 * step])
            })
            .filter(|&p| self.contains(p))
    }
}

/// Even-odd point in polygon; the outline closes itself. A lasso that
/// crosses itself leaves its doubly-wrapped loops out, as paint programs do.
fn inside(outline: &[[f32; 2]], p: [f32; 2]) -> bool {
    let mut odd = false;
    let mut j = outline.len() - 1;
    for (i, a) in outline.iter().enumerate() {
        let b = outline[j];
        if (a[1] > p[1]) != (b[1] > p[1])
            && p[0] < (b[0] - a[0]) * (p[1] - a[1]) / (b[1] - a[1]) + a[0]
        {
            odd = !odd;
        }
        j = i;
    }
    odd
}

/// Harbours must stand at least this far apart, centre to centre.
const HARBOUR_GAP: f32 = 70.;
/// Ships need water at least this deep 25 m out from a harbour.
const HARBOUR_DEPTH: f32 = 2.;

/// Up to `count` harbour curves on the shore inside `region`, best first:
/// flat ground behind, deep water in front, apart from each other and from
/// `existing` curves.
pub fn auto_harbours(
    field: &Heightfield,
    region: &Region,
    existing: &[&[PathPoint]],
    count: usize,
) -> Vec<Vec<PathPoint>> {
    let shore = Shore::new(field);
    let h = |x: f32, z: f32| field.height([x, z]);
    let mut candidates: Vec<([f32; 2], f32)> = region
        .samples(3.)
        .filter_map(|at| {
            let (x, z) = shore.nearest(at, 1)?;
            let c = [x as f32 + 0.5, z as f32 + 0.5];
            // Seaward: the mean direction to sea cells around.
            let (mut sx, mut sz) = (0., 0.);
            for k in 0..16 {
                let a = k as f32 / 16. * std::f32::consts::TAU;
                if h(c[0] + a.cos() * 8., c[1] + a.sin() * 8.) <= 0. {
                    sx += a.cos();
                    sz += a.sin();
                }
            }
            let l = f32::hypot(sx, sz);
            if l < 1e-3 {
                return None;
            }
            let (sx, sz) = (sx / l, sz / l);
            let depth = -h(c[0] + sx * 25., c[1] + sz * 25.);
            if depth < HARBOUR_DEPTH {
                return None;
            }
            // Lower and flatter ground behind reads as a beach.
            let behind = h(c[0] - sx * 12., c[1] - sz * 12.).max(0.)
                + h(c[0] - sx * 24., c[1] - sz * 24.).max(0.) * 0.5;
            Some((c, behind))
        })
        .collect();
    candidates.sort_by(|a, b| a.1.total_cmp(&b.1));
    // Curves keep their distance end to end, not only centre to centre.
    let clear = |a: &[PathPoint], b: &[PathPoint]| {
        a.iter().all(|p| {
            b.iter()
                .all(|q| (p.p[0] - q.p[0]).hypot(p.p[2] - q.p[2]) >= HARBOUR_GAP / 2.)
        })
    };
    let mut out: Vec<Vec<PathPoint>> = Vec::new();
    for (c, _) in candidates {
        if out.len() >= count {
            break;
        }
        let Some(curve) = trace_coast(field, c) else {
            continue;
        };
        let mid = curve[curve.len() / 2].p;
        if region.contains([mid[0], mid[2]])
            && existing.iter().all(|e| clear(&curve, e))
            && out.iter().all(|e| clear(&curve, e))
        {
            out.push(curve);
        }
    }
    out
}

/// How far apart auto placement keeps slots of `resource`. Shipped
/// neighbours (both games' pool islands): mines 10–25 m apart (median
/// about 20), 117 river slots 5–20 m, marsh slots 9–35 m.
fn slot_gap(resource: Resource) -> f32 {
    match resource {
        Resource::Mine | Resource::NewWorldMine => 15.,
        Resource::River => 12.,
        Resource::Marsh => MARSH_GAP,
        Resource::Clay | Resource::Oil | Resource::Gas => 30.,
    }
}

/// Shipped mine slots stand at the foot of a slope: the ground 8 m on
/// climbs 3–6 m (median 4.5) and falls at most 1–2 m behind.
const MINE_RISE: [f32; 3] = [2.5, 4.5, 7.5];
const MINE_DROP: f32 = 2.;
/// Shipped river slots sit on the bank: river cells are 30–60% of the
/// 13 × 13 m around them, one within 2 m.
const RIVER_SHARE: [f32; 3] = [0.25, 0.45, 0.7];

/// Height samples on 16 rays `reach` metres from `(x, z)`: the angle and
/// height of the highest, and the lowest height.
fn ring(field: &Heightfield, x: f32, z: f32, reach: f32) -> (f32, f32, f32) {
    let mut top = (0., f32::MIN);
    let mut low = f32::MAX;
    for k in 0..16 {
        let a = k as f32 / 16. * std::f32::consts::TAU;
        let y = field.height([x + a.cos() * reach, z + a.sin() * reach]);
        if y > top.1 {
            top = (a, y);
        }
        low = low.min(y);
    }
    (top.0, top.1, low)
}

/// River cells among the 13 × 13 m around `(x, z)`: share, and the mean
/// offset towards them.
fn river_around(river: &Layer, x: f32, z: f32) -> (f32, [f32; 2]) {
    let (mut n, mut sx, mut sz) = (0, 0., 0.);
    for dz in -6..=6 {
        for dx in -6..=6 {
            let wet = river
                .cell_at(x + dx as f32, z + dz as f32)
                .is_some_and(|(i, j)| river.get(i, j) != 0);
            if wet {
                n += 1;
                sx += dx as f32;
                sz += dz as f32;
            }
        }
    }
    (n as f32 / 169., [sx, sz])
}

/// The yaw a slot of `resource` at `at` faces, as shipped ones do, snapped
/// to `step`: mines face up the slope into the mountain; river slots stand
/// with the river on their right (yaw 90° past it); marsh slots face the
/// sea. `None` (clay, oil, gas, or nothing to face) keeps the record's.
pub fn slot_yaw(
    field: &Heightfield,
    river: Option<&Layer>,
    resource: Resource,
    at: [f32; 2],
    step: f32,
) -> Option<f32> {
    let [x, z] = at;
    let yaw = match resource {
        Resource::Mine | Resource::NewWorldMine => {
            // Each of 16 rays 11.4 m out, where an 1800 mountain work area
            // stands, pulls by the square of how far it climbs; falls
            // don't pull. Against shipped mine slots this matches 87% in
            // 1800 and 82% in 117. The highest ray 8 m out matched 63% and
            // 58%: its 22.5° steps tie at 45° from a 90° snap and always
            // rounded up, and a lone knoll outvoted the slope. A plain
            // pull, falls and all, turned slots off a hill whenever the
            // ground behind them fell to the sea.
            const REACH: f32 = 11.4;
            let h = field.height(at);
            let (mut px, mut pz, mut top) = (0., 0., f32::MIN);
            for k in 0..16 {
                let a = k as f32 / 16. * std::f32::consts::TAU;
                let rise = field.height([x + a.cos() * REACH, z + a.sin() * REACH]) - h;
                top = top.max(rise);
                let pull = rise.max(0.).powi(2);
                px += a.cos() * pull;
                pz += a.sin() * pull;
            }
            if top <= 0.5 {
                return None;
            }
            pz.atan2(px)
        }
        Resource::River => {
            let (share, [sx, sz]) = river_around(river?, x, z);
            if share <= 0. || sx.hypot(sz) < 1e-3 {
                return None;
            }
            sz.atan2(sx) + std::f32::consts::FRAC_PI_2
        }
        Resource::Marsh => {
            let (mut sx, mut sz) = (0., 0.);
            for reach in [4., 8., 16., 24.] {
                for k in 0..16 {
                    let a = k as f32 / 16. * std::f32::consts::TAU;
                    if field.height([x + a.cos() * reach, z + a.sin() * reach]) <= 0. {
                        sx += a.cos() / reach;
                        sz += a.sin() / reach;
                    }
                }
            }
            if f32::hypot(sx, sz) < 1e-4 {
                return None;
            }
            sz.atan2(sx)
        }
        Resource::Clay | Resource::Oil | Resource::Gas => return None,
    };
    Some(((yaw / step).round() * step).rem_euclid(std::f32::consts::TAU))
}
/// Shipped marsh slots (94 on 20 Celtic islands, `marsh_slots` example):
/// 0.2–0.7 m above sea level, 2–57 m (median 8, 90% within 21) from open
/// sea, never near rivers, 9 m apart at the closest tenth (median 31).
const MARSH_HEIGHT: [f32; 2] = [0.2, 0.8];
const MARSH_SEA_REACH: f32 = 24.;
const MARSH_GAP: f32 = 20.;
/// `EnvironmentGRid` value of the Marsh ambience; under 46% of shipped
/// marsh slots (most of the rest sit on the default type 1).
pub const MARSH_ENVIRONMENT: u32 = 7;

/// Up to `count` slot positions inside `region`: mines at the foot of a
/// slope, where shipped ones stand; river slots on the bank; marsh slots on
/// flat ground just above sea level near the coast, Marsh ambience first.
/// Keeps clear of `existing` slots and of each other by the shipped
/// spacing. `slot_yaw` turns each to face the way shipped ones do.
pub fn auto_slots(
    field: &Heightfield,
    river: Option<&Layer>,
    environment: Option<&Layer>,
    region: &Region,
    resource: Resource,
    existing: &[[f32; 2]],
    count: usize,
) -> Vec<[f32; 3]> {
    let h = |x: f32, z: f32| field.height([x, z]);
    let cells_near = |layer: Option<&Layer>, x: f32, z: f32, hit: &dyn Fn(u32) -> bool| {
        let Some(r) = layer else { return 0 };
        (-4..=4)
            .flat_map(|dz| (-4..=4).map(move |dx| (dx, dz)))
            .filter(|&(dx, dz)| {
                r.cell_at(x + dx as f32, z + dz as f32)
                    .is_some_and(|(i, j)| hit(r.get(i, j)))
            })
            .count()
    };
    // Metres to the nearest sea sample on 16 rays, up to `reach`.
    let sea_within = |x: f32, z: f32, reach: f32| {
        (1..=(reach / 2.) as usize)
            .map(|k| k as f32 * 2.)
            .find(|&r| {
                (0..16).any(|k| {
                    let a = k as f32 / 16. * std::f32::consts::TAU;
                    h(x + a.cos() * r, z + a.sin() * r) <= 0.
                })
            })
    };
    // 2 m: a mine's foot or a river's edge is a few metres wide.
    let mut candidates: Vec<([f32; 3], f32)> = region
        .samples(2.)
        .filter_map(|[x, z]| {
            let y = h(x, z);
            let slope = (h(x + 2., z) - h(x - 2., z)).hypot(h(x, z + 2.) - h(x, z - 2.)) / 4.;
            let score = match resource {
                Resource::Marsh => {
                    if !(MARSH_HEIGHT[0]..=MARSH_HEIGHT[1]).contains(&y)
                        || cells_near(river, x, z, &|v| v != 0) > 0
                    {
                        return None;
                    }
                    let sea = sea_within(x, z, MARSH_SEA_REACH)?;
                    // Shipped slots sit at a median 8 m from the sea.
                    let marshy = cells_near(environment, x, z, &|v| v == MARSH_ENVIRONMENT);
                    marshy as f32 * 0.1 - (sea - 8.).abs() * 0.1 - slope * 20.
                }
                _ => {
                    // Mines and river slots sit inland: near the waterline
                    // the drop into the sea would read as a steep mountain.
                    if y <= 1. || sea_within(x, z, 8.).is_some() {
                        return None;
                    }
                    match resource {
                        Resource::Mine | Resource::NewWorldMine => {
                            let (_, top, low) = ring(field, x, z, 8.);
                            let (rise, drop) = (top - y, y - low);
                            if !(MINE_RISE[0]..=MINE_RISE[2]).contains(&rise) || drop > MINE_DROP {
                                return None;
                            }
                            // A mountain that goes on beyond 8 m, not a bump.
                            let (_, far, _) = ring(field, x, z, 16.);
                            -(rise - MINE_RISE[1]).abs() - drop + (far - top).clamp(0., 8.) * 0.25
                        }
                        Resource::Clay | Resource::Oil | Resource::Gas => {
                            -slope * 20. + y.min(20.) * 0.1
                        }
                        Resource::River | Resource::Marsh => {
                            let (share, _) = river_around(river?, x, z);
                            let near = (-2..=2).any(|dz| {
                                (-2..=2).any(|dx| {
                                    river
                                        .and_then(|r| {
                                            r.cell_at(x + dx as f32, z + dz as f32)
                                                .map(|(i, j)| r.get(i, j))
                                        })
                                        .is_some_and(|v| v != 0)
                                })
                            });
                            if !near || !(RIVER_SHARE[0]..=RIVER_SHARE[2]).contains(&share) {
                                return None;
                            }
                            -(share - RIVER_SHARE[1]).abs() * 10. - slope * 4.
                        }
                    }
                }
            };
            Some(([x, y, z], score))
        })
        .collect();
    candidates.sort_by(|a, b| b.1.total_cmp(&a.1));
    let gap = slot_gap(resource);
    let mut out: Vec<[f32; 3]> = Vec::new();
    for (p, _) in candidates {
        if out.len() >= count {
            break;
        }
        let clear = |q: [f32; 2]| (q[0] - p[0]).hypot(q[1] - p[2]) >= gap;
        if existing.iter().all(|&q| clear(q)) && out.iter().all(|q| clear([q[0], q[2]])) {
            out.push(p);
        }
    }
    out
}

/// Shipped construction areas stop at about 15°: 98% of their land is
/// gentler, and none is steeper than 45°.
const MAX_BUILD_SLOPE: f32 = 15.;
/// Buildable patches smaller than this many m² are dropped, and enclosed
/// unbuildable specks smaller than `MIN_HOLE` filled, so slope noise does
/// not speckle the area like no shipped island is.
pub const MIN_PATCH: usize = 150;
const MIN_HOLE: usize = 40;

/// What land away from the coast becomes when areas are derived.
pub enum Interior<'a> {
    /// Buildable up to `MAX_BUILD_SLOPE`, from the terrain.
    Slope(&'a Heightfield),
    /// What the grid already says (hand-painted), land without an area
    /// becoming buildable.
    Keep,
}

/// Rederives `doc`'s `AreaIDs` from its `Water` grid and the curves of its
/// objects of `coasts` (the coastline GUIDs): the coast and pads always,
/// and with `from_slope` buildable land from the slope too, replacing what
/// was painted. The land grids must already follow the heights.
pub fn refresh_areas(doc: &mut Doc, coasts: &[u32], from_slope: bool) {
    let paths: Vec<Vec<PathPoint>> = (doc.objects.iter())
        .filter(|o| coasts.contains(&o.guid))
        .filter_map(|o| o.path.clone())
        .collect();
    let Some(grids) = &mut doc.grids else {
        return;
    };
    let grids = std::sync::Arc::make_mut(grids);
    let Some(ground) = grids.layer(Kind::Water).cloned() else {
        return;
    };
    let Some(areas) = grids.layer_mut(Kind::AreaIds) else {
        return;
    };
    let refs: Vec<&[PathPoint]> = paths.iter().map(Vec::as_slice).collect();
    let interior = if from_slope {
        Interior::Slope(&doc.field)
    } else {
        Interior::Keep
    };
    derive_areas(&ground, areas, &refs, interior, None);
}

/// Rewrites `areas` from `ground` (the `Water` grid: island ground, 0 is
/// open sea) and the harbour curves. Sea within the pad radius of a curve
/// joins the construction area; land near sea outside the pads gets a
/// base-area rim; other land follows `interior`. Other sea leaves the
/// island. With `window` (world metres `[x0, z0, x1, z1]`) only the cells
/// in it: patch cleanup sees the window's edge as the map's.
pub fn derive_areas(
    ground: &Layer,
    areas: &mut Layer,
    coasts: &[&[PathPoint]],
    interior: Interior,
    window: Option<[f32; 4]>,
) {
    let (w, h) = (areas.width, areas.height);
    let cells = areas.cells_within(window);
    let is_ground = |x: usize, y: usize| {
        let (gx, gy) = (x * ground.width / w, y * ground.height / h);
        ground.get(gx.min(ground.width - 1), gy.min(ground.height - 1)) != 0
    };
    let inner: Vec<u32> = match interior {
        Interior::Slope(field) => buildable_from_slope(field, w, h, &cells),
        Interior::Keep => (0..w * h)
            .map(|k| match areas.get(k % w, k / w) {
                BASE => BASE,
                _ => CONSTRUCTION,
            })
            .collect(),
    };
    let values = area_values(is_ground, w, h, coasts, &inner, &cells);
    for y in cells.1 {
        for x in cells.0.clone() {
            areas.set(x, y, values[y * w + x]);
        }
    }
}

/// Columns and rows of a derivation's window.
type Cells = (Range<usize>, Range<usize>);

/// Cells outside a derivation's window; never flipped or seeded.
const OUTSIDE: u32 = u32::MAX;

/// Construction or base per 1 m cell from the terrain slope, with small
/// patches and holes cleaned up, in `cells` (`OUTSIDE` elsewhere).
fn buildable_from_slope(field: &Heightfield, w: usize, h: usize, cells: &Cells) -> Vec<u32> {
    let limit = MAX_BUILD_SLOPE.to_radians().tan();
    let height = |x: f32, z: f32| field.height([x, z]);
    let mut out = vec![OUTSIDE; w * h];
    for y in cells.1.clone() {
        for x in cells.0.clone() {
            let (fx, fz) = (x as f32 + 0.5, y as f32 + 0.5);
            let dx = (height(fx + 1., fz) - height(fx - 1., fz)) / 2.;
            let dz = (height(fx, fz + 1.) - height(fx, fz - 1.)) / 2.;
            out[y * w + x] = if dx.hypot(dz) <= limit {
                CONSTRUCTION
            } else {
                BASE
            };
        }
    }
    // Flip components of `value` smaller than `min` to the other value.
    let mut clean = |value: u32, min: usize, other: u32| {
        let mut seen = vec![false; w * h];
        for start in cells
            .1
            .clone()
            .flat_map(|y| cells.0.clone().map(move |x| y * w + x))
        {
            if seen[start] || out[start] != value {
                continue;
            }
            let mut component = vec![start];
            seen[start] = true;
            let mut i = 0;
            while i < component.len() {
                let k = component[i];
                let (x, y) = (k % w, k / w);
                let next = [
                    (x > 0).then(|| k - 1),
                    (x + 1 < w).then(|| k + 1),
                    (y > 0).then(|| k - w),
                    (y + 1 < h).then(|| k + w),
                ];
                for n in next.into_iter().flatten() {
                    if !seen[n] && out[n] == value {
                        seen[n] = true;
                        component.push(n);
                    }
                }
                i += 1;
            }
            if component.len() < min {
                for k in component {
                    out[k] = other;
                }
            }
        }
    };
    clean(CONSTRUCTION, MIN_PATCH, BASE);
    clean(BASE, MIN_HOLE, CONSTRUCTION);
    out
}

/// [`derive_areas`] on plain cells: 1 m cells, `is_ground(x, y)`, and the
/// interior value of each land cell; only `cells` are meaningful.
fn area_values(
    is_ground: impl Fn(usize, usize) -> bool,
    w: usize,
    h: usize,
    coasts: &[&[PathPoint]],
    inner: &[u32],
    cells: &Cells,
) -> Vec<u32> {
    // Each segment marks the sea cells within the pad radius of it; testing
    // every cell against every segment took most of a stroke's release.
    let mut pad = vec![false; w * h];
    for s in coasts.iter().flat_map(|c| c.windows(2)) {
        let (a, b) = (s[0].p, s[1].p);
        let span = |lo: f32, hi: f32, within: &Range<usize>| {
            let first = ((lo - PAD_RADIUS - 0.5).floor().max(0.) as usize).max(within.start);
            let end = ((hi + PAD_RADIUS - 0.5).ceil().max(0.) as usize + 1).min(within.end);
            first.min(end)..end
        };
        let xs = span(a[0].min(b[0]), a[0].max(b[0]), &cells.0);
        for y in span(a[2].min(b[2]), a[2].max(b[2]), &cells.1) {
            for x in xs.clone() {
                let k = y * w + x;
                if !pad[k]
                    && !is_ground(x, y)
                    && segment_distance([x as f32 + 0.5, y as f32 + 0.5], a, b) <= PAD_RADIUS
                {
                    pad[k] = true;
                }
            }
        }
    }
    // Distance in cells from sea that is not a pad.
    let mut dist = vec![u32::MAX; w * h];
    let mut queue = VecDeque::new();
    for y in cells.1.clone() {
        for x in cells.0.clone() {
            if !is_ground(x, y) && !pad[y * w + x] {
                dist[y * w + x] = 0;
                queue.push_back((x, y));
            }
        }
    }
    while let Some((x, y)) = queue.pop_front() {
        let next = dist[y * w + x] + 1;
        if next > RIM {
            continue;
        }
        for (dx, dy) in [(1i32, 0i32), (-1, 0), (0, 1), (0, -1)] {
            let (nx, ny) = (x as i32 + dx, y as i32 + dy);
            if nx < 0 || ny < 0 || nx >= w as i32 || ny >= h as i32 {
                continue;
            }
            let k = ny as usize * w + nx as usize;
            if dist[k] > next {
                dist[k] = next;
                queue.push_back((nx as usize, ny as usize));
            }
        }
    }
    let mut out = vec![OUTSIDE; w * h];
    for y in cells.1.clone() {
        for x in cells.0.clone() {
            let k = y * w + x;
            out[k] = if pad[k] {
                CONSTRUCTION
            } else if !is_ground(x, y) {
                0
            } else if dist[k] <= RIM {
                BASE
            } else {
                inner[k]
            };
        }
    }
    out
}

fn segment_distance(p: [f32; 2], a: [f32; 3], b: [f32; 3]) -> f32 {
    let (ax, az, bx, bz) = (a[0], a[2], b[0], b[2]);
    let (dx, dz) = (bx - ax, bz - az);
    let l2 = dx * dx + dz * dz;
    let t = if l2 == 0. {
        0.
    } else {
        (((p[0] - ax) * dx + (p[1] - az) * dz) / l2).clamp(0., 1.)
    };
    (p[0] - ax - t * dx).hypot(p[1] - az - t * dz)
}

/// How far a work area's path may end from the slot it serves. On shipped
/// 1800 islands the end sits within 1 m of a slot; on 117 islands half the
/// paths end on their slot and all but two of the rest within 9 m, as if
/// the slot moved after the path was drawn (`slot_paths` example).
const LINK_REACH: f32 = 10.;

/// Which end of a slot link an asset is: a resource slot, or the work area
/// whose `BezierPath` runs to it. Slots carry no path of their own.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LinkEnd {
    Slot,
    WorkArea,
}

pub fn link_end(assets: &Assets, guid: u32) -> Option<LinkEnd> {
    match template_of(assets, guid).as_str() {
        "Slot" => Some(LinkEnd::Slot),
        "WorkAreaSlot" | "WorkAreaRiverSlot" => Some(LinkEnd::WorkArea),
        _ => None,
    }
}

/// A work area and the slot its path runs to, as they stood before an edit.
pub struct Link {
    pub work: u64,
    pub slot: u64,
    work_at: [f32; 3],
    path: Vec<PathPoint>,
}

/// The links touching an object `touches` accepts. A work area belongs to
/// the slot nearest its path's last point, within `LINK_REACH`.
pub fn links(
    objects: &[Object],
    end: impl Fn(u32) -> Option<LinkEnd>,
    touches: impl Fn(u64) -> bool,
) -> Vec<Link> {
    let ends: Vec<(&Object, LinkEnd)> = objects
        .iter()
        .filter_map(|o| Some((o, end(o.guid)?)))
        .collect();
    let slots = || {
        ends.iter()
            .filter(|(_, e)| *e == LinkEnd::Slot)
            .map(|(o, _)| o)
    };
    let ground = |a: [f32; 3], b: [f32; 3]| (a[0] - b[0]).hypot(a[2] - b[2]);
    ends.iter()
        .filter(|(_, e)| *e == LinkEnd::WorkArea)
        .filter_map(|(work, _)| {
            let last = work.path.as_deref()?.last()?.p;
            let slot = slots()
                .map(|s| (s, ground(s.position, last)))
                .filter(|&(_, d)| d <= LINK_REACH)
                .min_by(|a, b| a.1.total_cmp(&b.1))?
                .0;
            (touches(work.id) || touches(slot.id)).then(|| Link {
                work: work.id,
                slot: slot.id,
                work_at: work.position,
                path: work.path.clone().unwrap_or_default(),
            })
        })
        .collect()
}

impl Link {
    /// The path redrawn from where the work area is now to where the slot
    /// is now, or None when neither moved. Each point moves by a blend of
    /// the two ends' moves, by its share of the path's ground length, so a
    /// winding 1800 road keeps its shape. The end lands on the slot even
    /// when it started a few metres off. Two-point paths get the shipped
    /// tangents again (`link_work_area`); longer ones keep theirs.
    pub fn follow(&self, work_at: [f32; 3], slot_at: [f32; 3]) -> Option<Vec<PathPoint>> {
        let last = self.path.last()?.p;
        let start: [f32; 3] = std::array::from_fn(|k| work_at[k] - self.work_at[k]);
        let end: [f32; 3] = std::array::from_fn(|k| slot_at[k] - last[k]);
        if start.iter().chain(&end).all(|v| v.abs() < 1e-4) {
            return None;
        }
        let mut along = Vec::with_capacity(self.path.len());
        let mut total = 0.;
        for (k, p) in self.path.iter().enumerate() {
            if k > 0 {
                let q = self.path[k - 1].p;
                total += (p.p[0] - q[0]).hypot(p.p[2] - q[2]);
            }
            along.push(total);
        }
        let n = self.path.len();
        let mut path = self.path.clone();
        for (k, p) in path.iter_mut().enumerate() {
            let t = if total > 1e-4 {
                along[k] / total
            } else if n > 1 {
                k as f32 / (n - 1) as f32
            } else {
                1.
            };
            p.p = std::array::from_fn(|x| p.p[x] + start[x] * (1. - t) + end[x] * t);
        }
        if n == 2 {
            let span: [f32; 3] = std::array::from_fn(|k| (path[0].p[k] - path[1].p[k]) / 6.);
            for p in &mut path {
                p.i = Some(span);
                p.o = Some(span.map(|v| -v));
            }
        }
        Some(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::island::test_support::{assets, install, install_1800};
    use anno_core::game::Game;
    use gpui_editor_kit::heightfield::Grid;

    // A game the registry doesn't know names its own pieces: its survey
    // marker is the islands' marker, which 117's names (a PositionMarker
    // of "Player Islands") never call it.
    #[test]
    fn a_skeleton_asset_plays_the_piece_its_own_game_names() {
        use crate::games::fixtures::{SKELETON, SKELETON_FILES, install};
        let dir = tempfile::tempdir().unwrap();
        let install = install(dir.path(), SKELETON_FILES);
        let assets = crate::games::assets(&SKELETON, &install).unwrap();
        assert_eq!(piece_of(&SKELETON, &assets, 910_003), Some(Piece::Marker));
        assert_eq!(piece_of(&SKELETON, &assets, 910_001), None);
        assert_eq!(piece_of(&anno117::MODULE, &assets, 910_003), None);
    }

    /// A round island of radius 40 m in a 128 m square, 0.5 m samples.
    fn round_island() -> Heightfield {
        let grid = Grid {
            width: 257,
            depth: 257,
            spacing: 0.5,
            origin: [0.; 2],
        };
        let mut heights = vec![0.; grid.width * grid.depth];
        for z in 0..grid.depth {
            for x in 0..grid.width {
                let [wx, wz] = grid.world(x, z);
                let r = (wx - 64.).hypot(wz - 64.);
                heights[z * grid.width + x] = if r < 40. { 3. } else { -6. };
            }
        }
        Heightfield::new(grid, heights)
    }

    /// A round island like `round_island` with a hill in the middle and
    /// deep water around.
    fn hilly_island() -> Heightfield {
        let mut field = round_island();
        let grid = field.grid;
        for z in 0..grid.depth {
            for x in 0..grid.width {
                let [wx, wz] = grid.world(x, z);
                let r = (wx - 64.).hypot(wz - 64.);
                if r < 40. {
                    field.heights[z * grid.width + x] = 3. + (24. - r * 0.6).max(0.);
                }
            }
        }
        field
    }

    // The coast traced from a click follows the waterline and faces the
    // sea, and its pad is the only place construction land meets open sea.
    #[test]
    fn coast_follows_the_waterline_and_faces_the_sea() {
        let field = round_island();
        let curve = trace_coast(&field, [64., 20.]).expect("shore near the top");
        assert!(curve.len() >= 4, "{} points", curve.len());
        let length: f32 = curve
            .windows(2)
            .map(|w| (w[1].p[0] - w[0].p[0]).hypot(w[1].p[2] - w[0].p[2]))
            .sum();
        assert!((30. ..60.).contains(&length), "length {length}");
        for pt in &curve {
            let r = (pt.p[0] - 64.).hypot(pt.p[2] - 64.);
            assert!((37. ..42.).contains(&r), "on the shore: r = {r}");
            // Seaward is away from the centre.
            let out = [(pt.p[0] - 64.) / r, (pt.p[2] - 64.) / r];
            let d = pt.d.expect("coast direction");
            assert!(
                d[0] * out[0] + d[1] * out[1] > 0.6,
                "d {:?} at {:?}",
                pt.d,
                pt.p
            );
            assert!((COAST_HEIGHT[0]..=COAST_HEIGHT[1]).contains(&pt.p[1]));
        }
        assert!(
            trace_coast(&field, [64., 64.]).is_none(),
            "no shore near the centre"
        );

        let ground = |x: usize, y: usize| field.height([x as f32 + 0.5, y as f32 + 0.5]) > 0.;
        let areas = area_values(
            ground,
            128,
            128,
            &[&curve],
            &vec![CONSTRUCTION; 128 * 128],
            &(0..128, 0..128),
        );
        let at = |x: usize, y: usize| areas[y * 128 + x];
        let (mut pad, mut touching) = (0, 0);
        for y in 1..127 {
            for x in 1..127 {
                let a = at(x, y);
                if !ground(x, y) && a == CONSTRUCTION {
                    pad += 1;
                }
                if ground(x, y) {
                    assert_ne!(a, 0, "land outside every area at {x},{y}");
                }
                if ground(x, y) && a == CONSTRUCTION {
                    for (nx, ny) in [(x + 1, y), (x - 1, y), (x, y + 1), (x, y - 1)] {
                        if !ground(nx, ny) && at(nx, ny) != CONSTRUCTION {
                            touching += 1;
                        }
                    }
                }
            }
        }
        assert!(pad > 500, "the pad reaches into the sea: {pad} cells");
        assert_eq!(touching, 0);
        assert_eq!(at(64, 64), CONSTRUCTION, "the interior is buildable");
    }

    #[test]
    fn dragging_along_the_shore_follows_it() {
        let field = round_island();
        // Quarter turn from the top of the island to its right side.
        let curve = trace_between(&field, [64., 22.], [106., 64.]).unwrap();
        let (first, last) = (curve[0].p, curve[curve.len() - 1].p);
        assert!(
            (first[0] - 64.).abs() < 6. && (first[2] - 24.).abs() < 6.,
            "{first:?}"
        );
        assert!(
            (last[0] - 104.).abs() < 6. && (last[2] - 64.).abs() < 6.,
            "{last:?}"
        );
        for pt in &curve {
            let r = (pt.p[0] - 64.).hypot(pt.p[2] - 64.);
            assert!((36. ..43.).contains(&r), "on the shore: r = {r}");
        }
        let snapped = snap_to_shore(&field, [64., 30.]).unwrap();
        assert!(((snapped[0] - 64.).hypot(snapped[1] - 64.) - 39.5).abs() < 1.5);
    }

    #[test]
    fn auto_harbours_stay_apart_inside_the_region() {
        let field = round_island();
        let all = Region::spanning([0., 0.], [128., 128.]);
        let curves = auto_harbours(&field, &all, &[], 10);
        assert!(
            curves.len() >= 2 && curves.len() < 10,
            "{} harbours",
            curves.len()
        );
        let mid = |c: &[PathPoint]| c[c.len() / 2].p;
        let gap = |a: &[PathPoint], b: &[PathPoint]| {
            a.iter()
                .flat_map(|p| {
                    b.iter()
                        .map(move |q| (p.p[0] - q.p[0]).hypot(p.p[2] - q.p[2]))
                })
                .fold(f32::MAX, f32::min)
        };
        for (i, a) in curves.iter().enumerate() {
            for b in &curves[i + 1..] {
                assert!(
                    gap(a, b) >= HARBOUR_GAP / 2.,
                    "curves {} m apart",
                    gap(a, b)
                );
            }
        }
        // Only the top half: every harbour there, none below.
        let top = Region::spanning([0., 0.], [128., 50.]);
        let some = auto_harbours(&field, &top, &[], 4);
        assert!(!some.is_empty());
        assert!(some.iter().all(|c| mid(c)[2] <= 50.));
        // Existing harbours are kept clear of.
        let again = auto_harbours(&field, &top, &[&some[0]], 4);
        assert!(again.iter().all(|c| {
            (mid(c)[0] - mid(&some[0])[0]).hypot(mid(c)[2] - mid(&some[0])[2]) >= HARBOUR_GAP / 2.
        }));
    }

    /// A round island like `round_island`, flat at 3 m out to 40 m with
    /// a steep mountain inside 20 m: the foot shipped mines stand on.
    fn mountain_island() -> Heightfield {
        let mut field = round_island();
        let grid = field.grid;
        for z in 0..grid.depth {
            for x in 0..grid.width {
                let [wx, wz] = grid.world(x, z);
                let r = (wx - 64.).hypot(wz - 64.);
                if r < 40. {
                    field.heights[z * grid.width + x] = 3. + ((20. - r) * 0.8).max(0.);
                }
            }
        }
        field
    }

    #[test]
    fn auto_mines_stand_at_the_foot_facing_the_mountain() {
        let field = mountain_island();
        let all = Region::spanning([0., 0.], [128., 128.]);
        let mines = auto_slots(&field, None, None, &all, Resource::Mine, &[], 4);
        assert_eq!(mines.len(), 4);
        for m in &mines {
            let r = (m[0] - 64.).hypot(m[2] - 64.);
            assert!((18. ..=28.).contains(&r), "mine {r:.1} m from the peak");
            // Facing the peak, to the 45° step.
            let yaw = slot_yaw(
                &field,
                None,
                Resource::Mine,
                [m[0], m[2]],
                crate::games::rules(Game::Anno117).slot_yaw_step,
            )
            .unwrap();
            let to_peak = (64. - m[2]).atan2(64. - m[0]);
            let off = (yaw - to_peak).rem_euclid(std::f32::consts::TAU);
            let off = off.min(std::f32::consts::TAU - off);
            assert!(off <= std::f32::consts::FRAC_PI_4, "yaw {off} off the peak");
            assert_eq!(
                (yaw / std::f32::consts::FRAC_PI_4).fract().abs() < 1e-4,
                true
            );
        }
        for (i, a) in mines.iter().enumerate() {
            for b in &mines[i + 1..] {
                assert!((a[0] - b[0]).hypot(a[2] - b[2]) >= slot_gap(Resource::Mine));
            }
        }
        // A uniform cone has no foot inland: nothing fits there.
        assert!(
            auto_slots(&field, None, None, &all, Resource::River, &[], 3).is_empty(),
            "no rivers, no river slots"
        );
    }

    #[test]
    fn river_slots_take_the_bank_with_the_river_on_their_right() {
        let Some((_, editor)) = crate::island::test_support::open(
            "data/base/provinces/roman/islands/pool/roman_island_extralarge_01/roman_island_extralarge_01.a7m",
        ) else {
            return;
        };
        let field = &editor.island.doc.field;
        let grids = editor.island.doc.grids.as_ref().unwrap();
        let river = grids.layer(anno_formats::a7m::grids::Kind::River);
        let size = field.grid.width as f32 * field.grid.spacing;
        let all = Region::spanning([0., 0.], [size, size]);
        let spots = auto_slots(field, river, None, &all, Resource::River, &[], 8);
        assert_eq!(spots.len(), 8);
        let river = river.unwrap();
        for p in &spots {
            let (share, [sx, sz]) = river_around(river, p[0], p[2]);
            assert!(
                (RIVER_SHARE[0]..=RIVER_SHARE[2]).contains(&share),
                "share {share}"
            );
            // The river to the right: yaw is 90° past the way to it.
            let step = crate::games::rules(Game::Anno117).slot_yaw_step;
            let yaw = slot_yaw(field, Some(river), Resource::River, [p[0], p[2]], step).unwrap();
            let want = ((sz.atan2(sx) + std::f32::consts::FRAC_PI_2) / step).round() * step;
            let off = (yaw - want).rem_euclid(std::f32::consts::TAU);
            assert!(off.min(std::f32::consts::TAU - off) < 1e-3);
        }
    }

    #[test]
    fn marsh_slots_face_the_sea() {
        let field = round_island();
        // On the east shore the sea is +x.
        let yaw = slot_yaw(
            &field,
            None,
            Resource::Marsh,
            [100., 64.],
            crate::games::rules(Game::Anno117).slot_yaw_step,
        )
        .unwrap();
        let off = yaw.min(std::f32::consts::TAU - yaw);
        assert!(off < 1e-4, "yaw {yaw}");
        assert_eq!(
            slot_yaw(&field, None, Resource::Clay, [100., 64.], 1.),
            None
        );
    }

    #[test]
    fn a_lasso_keeps_auto_slots_inside_its_outline() {
        let field = mountain_island();
        let size = 128.;
        // A triangle over the left half: every slot lands inside it.
        let outline = [[0., 0.], [size / 2., size / 2.], [0., size]];
        let lasso = Region::lasso(&outline).unwrap();
        let picked = auto_slots(&field, None, None, &lasso, Resource::Mine, &[], 12);
        assert!(!picked.is_empty());
        assert!(picked.iter().all(|p| inside(&outline, [p[0], p[2]])));
        assert!(Region::lasso(&outline[..2]).is_none());
        assert!(lasso.contains([size / 8., size / 2.]));
        assert!(!lasso.contains([size * 0.45, size * 0.1]));
    }

    #[test]
    fn auto_marsh_slots_take_low_ground_by_the_sea() {
        // A round island whose rim drops to a 0.5 m flat 30–40 m from the
        // centre: marsh slots belong on that flat, not on the hill.
        let mut field = hilly_island();
        let grid = field.grid;
        for z in 0..grid.depth {
            for x in 0..grid.width {
                let [wx, wz] = grid.world(x, z);
                let r = (wx - 64.).hypot(wz - 64.);
                if (30. ..40.).contains(&r) {
                    field.heights[z * grid.width + x] = 0.5;
                }
            }
        }
        let all = Region::spanning([0., 0.], [128., 128.]);
        let marsh = auto_slots(&field, None, None, &all, Resource::Marsh, &[], 4);
        assert_eq!(marsh.len(), 4);
        for m in &marsh {
            let r = (m[0] - 64.).hypot(m[2] - 64.);
            assert!((30. ..40.).contains(&r), "on the flat: r = {r}");
            assert!((MARSH_HEIGHT[0]..=MARSH_HEIGHT[1]).contains(&m[1]));
        }
        for (i, a) in marsh.iter().enumerate() {
            for b in &marsh[i + 1..] {
                assert!((a[0] - b[0]).hypot(a[2] - b[2]) >= MARSH_GAP);
            }
        }
        assert!(
            auto_slots(&hilly_island(), None, None, &all, Resource::Marsh, &[], 4).is_empty(),
            "a 3 m cliff above the sea is no marsh"
        );
    }

    #[test]
    fn steep_ground_is_not_buildable() {
        // A cone: gentle top, 30° flanks.
        let grid = Grid {
            width: 257,
            depth: 257,
            spacing: 0.5,
            origin: [0.; 2],
        };
        let mut heights = vec![0.; grid.width * grid.depth];
        for z in 0..grid.depth {
            for x in 0..grid.width {
                let [wx, wz] = grid.world(x, z);
                let r = (wx - 64.).hypot(wz - 64.);
                heights[z * grid.width + x] = if r < 20. {
                    20.
                } else {
                    (20. - (r - 20.) * 0.58).max(-6.)
                };
            }
        }
        let field = Heightfield::new(grid, heights);
        let inner = buildable_from_slope(&field, 128, 128, &(0..128, 0..128));
        assert_eq!(inner[64 * 128 + 64], CONSTRUCTION, "flat top");
        assert_eq!(inner[64 * 128 + 64 + 30], BASE, "30° flank");
        // Hand-painted interiors survive a keep-derive.
        let ground = |x: usize, y: usize| field.height([x as f32 + 0.5, y as f32 + 0.5]) > 0.;
        let mut painted = vec![CONSTRUCTION; 128 * 128];
        painted[64 * 128 + 64] = BASE;
        let kept = area_values(ground, 128, 128, &[], &painted, &(0..128, 0..128));
        assert_eq!(kept[64 * 128 + 64], BASE);
    }

    // 1800 mountain work areas go up the hill with a rail ramp down to the
    // slot, cave ones stay close; 117 ones go down it, linked by two points.
    fn harbours(game: Game) -> &'static dyn anno_core::game::Harbours {
        crate::games::module(game)
            .islands()
            .and_then(|i| i.harbours())
            .expect("the shared pieces")
    }

    /// A new work area at `at`, linked to the slot at `slot` by `shape`.
    fn linked(
        shape: &anno_core::harbour::RoadShape,
        field: &Heightfield,
        at: [f32; 3],
        slot: [f32; 3],
    ) -> Object {
        use anno_core::island::objects::{Record, Section};
        let mut work = Object::of_record(Record::Source(0), Section::GameObject, 1, 1);
        work.position = at;
        shape.link(&mut work, field, slot);
        work
    }

    fn wrapped(a: f32) -> f32 {
        use std::f32::consts::{PI, TAU};
        (a + PI).rem_euclid(TAU) - PI
    }

    #[test]
    fn roads_take_each_games_shipped_shape() {
        let field = mountain_island();
        let ground = |a: [f32; 3], b: [f32; 3]| (a[0] - b[0]).hypot(a[2] - b[2]);
        // At the mountain's foot, 24 m east of its peak, facing it.
        let slot = [88., field.height([88., 64.]), 64.];
        let step = crate::games::rules(Game::Anno1800).slot_yaw_step;
        let yaw = slot_yaw(&field, None, Resource::Mine, [slot[0], slot[2]], step).unwrap();
        assert!(
            wrapped(yaw - std::f32::consts::PI).abs() < 1e-4,
            "yaw {yaw}"
        );

        let mountain = harbours(Game::Anno1800).road_shape(Some(Piece::MineWorkArea));
        let work = mountain.work_area_position(&field, slot, yaw);
        assert!((ground(work, slot) - 11.4).abs() < 1e-3);
        assert!(work[1] > slot[1] + 3., "uphill: {work:?} over {slot:?}");
        let placed = linked(&mountain, &field, work, slot);
        let rail = placed.path.unwrap();
        assert!((6..=10).contains(&rail.len()), "{} points", rail.len());
        // The rail leaves the model's dummy, 0.62 m behind its centre,
        // and runs through it to the slot.
        let start = rail[0].p;
        assert!((ground(start, work) - 0.62).abs() < 1e-3);
        assert!(ground(start, slot) > ground(work, slot));
        assert!((start[1] - work[1] - 0.18).abs() < 1e-4);
        assert_eq!(rail.last().unwrap().p, slot);
        for w in rail.windows(2) {
            let chord = ground(w[0].p, w[1].p);
            assert!((1.2..=2.).contains(&chord), "spacing {chord}");
        }
        for p in &rail[1..rail.len() - 1] {
            assert!(
                p.p[1] >= field.height([p.p[0], p.p[2]]) + 0.2 - 1e-4,
                "{p:?} under ground"
            );
        }

        let cave = harbours(Game::Anno1800).road_shape(Some(Piece::CaveWorkArea));
        let by = cave.work_area_position(&field, slot, yaw);
        assert!((ground(by, slot) - 5.3).abs() < 1e-3);
        let angle = |p: [f32; 3]| (p[2] - slot[2]).atan2(p[0] - slot[0]);
        assert!(
            wrapped(angle(by) - angle(work)).abs() < 0.5,
            "same side of the slot"
        );
        // Its dummy, where the rail leaves the cave, is 1.3 m on into the
        // hill and 1.5 m up.
        let rail = linked(&cave, &field, by, slot).path.unwrap();
        assert!(ground(rail[0].p, slot) > ground(by, slot));
        assert!((rail[0].p[1] - by[1] - 1.54).abs() < 1e-4);

        // 117's mountain work areas stand in front of their slot, which
        // faces up the slope: towards the peak, on a two-point path.
        let shape = harbours(Game::Anno117).road_shape(Some(Piece::MineWorkArea));
        let step = crate::games::rules(Game::Anno117).slot_yaw_step;
        let yaw = slot_yaw(&field, None, Resource::Mine, [slot[0], slot[2]], step).unwrap();
        let work = shape.work_area_position(&field, slot, yaw);
        let peak = [64., 0., 64.];
        assert!(
            ground(work, peak) < ground(slot, peak) - 2.,
            "uphill: {work:?}"
        );
        let link = linked(&shape, &field, work, slot).path.unwrap();
        assert_eq!(link.len(), 2);
        let span: [f32; 3] = std::array::from_fn(|k| (work[k] - slot[k]) / 6.);
        for p in &link {
            for (k, v) in span.iter().enumerate() {
                assert!((p.i.unwrap()[k] - v).abs() < 1e-4);
                assert!((p.o.unwrap()[k] + v).abs() < 1e-4);
            }
        }
        assert_eq!([link[0].p, link[1].p], [work, slot]);
    }

    // Each kind's shipped work areas, measured on every shipped island of
    // both games (2026-11-22): the yaw against the direction to the slot,
    // and the side of the slot against its yaw, as a median with the
    // 25th and 75th percentile around it, in degrees; then the span's 25th
    // and 75th percentile in metres. The 1800 cave ones turn their back on
    // the slot's direction, since the model opens along its -x; rivers
    // keep to their slot's frame within a degree.
    #[test]
    fn placed_work_areas_face_and_stand_as_shipped_ones_do() {
        type Spread = (f32, f32, f32);
        let shipped: [(Game, Piece, Spread, Spread, [f32; 2]); 6] = [
            (
                Game::Anno1800,
                Piece::MineWorkArea,
                (-5.5, -8.5, 10.9),
                (2.3, -17.9, 14.5),
                [10.14, 12.84],
            ),
            (
                Game::Anno1800,
                Piece::CaveWorkArea,
                (-179.9, -11.2, 15.0),
                (2.9, -7.3, 7.1),
                [4.98, 5.55],
            ),
            (
                Game::Anno1800,
                Piece::NewWorldMineWorkArea,
                (-2.8, -21.4, 18.9),
                (-2.4, -19.3, 13.3),
                [9.44, 11.96],
            ),
            (
                Game::Anno1800,
                Piece::RiverWorkArea,
                (-97.1, -1.0, 0.0),
                (-82.9, 0.0, 1.0),
                [7.07, 8.06],
            ),
            (
                Game::Anno117,
                Piece::MineWorkArea,
                (-6.6, -8.3, 8.4),
                (9.1, -5.5, 6.6),
                [2.07, 3.08],
            ),
            (
                Game::Anno117,
                Piece::RiverWorkArea,
                (171.7, -0.7, 0.7),
                (-81.7, -0.7, 0.7),
                [2.41, 2.74],
            ),
        ];
        let field = round_island();
        let slot = [70., field.height([70., 60.]), 60.];
        // Shipped slots turn every way; any yaw will do.
        let slot_yaw = 0.7f32;
        let within = |what: &str, got: f32, (median, low, high): Spread| {
            let off = wrapped(got - median.to_radians()).to_degrees();
            // A tenth of a degree for the rounded medians.
            assert!(
                (low - 0.1..=high + 0.1).contains(&off),
                "{what}: {off:.1}° off the shipped median {median}°, spread {low}..{high}"
            );
        };
        for (game, piece, facing, side, span) in shipped {
            let shape = harbours(game).road_shape(Some(piece));
            let at = shape.work_area_position(&field, slot, slot_yaw);
            let work = linked(&shape, &field, at, slot);
            let to_slot = (slot[2] - at[2]).atan2(slot[0] - at[0]);
            within(
                &format!("{game:?} {piece:?} yaw"),
                work.direction - to_slot,
                facing,
            );
            let bearing = (at[2] - slot[2]).atan2(at[0] - slot[0]);
            within(
                &format!("{game:?} {piece:?} side"),
                bearing - slot_yaw,
                side,
            );
            let d = (at[0] - slot[0]).hypot(at[2] - slot[2]);
            assert!(
                (span[0] - 1e-3..=span[1] + 1e-3).contains(&d),
                "{game:?} {piece:?}: {d:.2} m from the slot, shipped {span:?}"
            );
        }
    }

    // A slope rising 0.3 m a metre to +x, with a thin wall 5 m high
    // running out at 67.5°. The highest ray alone took the wall and
    // snapped to 90°; shipped mine slots face the slope.
    #[test]
    fn a_mine_slot_faces_the_slope_not_a_wall_on_it() {
        let mut field = round_island();
        let grid = field.grid;
        let (sx, sz) = (60., 64.);
        let (s, c) = 67.5f32.to_radians().sin_cos();
        for z in 0..grid.depth {
            for x in 0..grid.width {
                let [wx, wz] = grid.world(x, z);
                let (dx, dz) = (wx - sx, wz - sz);
                let (along, across) = (dx * c + dz * s, (dz * c - dx * s).abs());
                let wall = (6. ..14.).contains(&along) && across < 0.6;
                field.heights[z * grid.width + x] = 3. + 0.3 * dx + if wall { 5. } else { 0. };
            }
        }
        let step = crate::games::rules(Game::Anno1800).slot_yaw_step;
        let yaw = slot_yaw(&field, None, Resource::Mine, [sx, sz], step).unwrap();
        assert!(wrapped(yaw).abs() < 1e-4, "yaw {:.1}°", yaw.to_degrees());
    }

    // A mine rail laid from a plateau over its brink: the ground falls
    // 0.5 (x - 70)² m past x 70 to a floor at 1 m. Points alone kept 0.2 m
    // clear, but the bent track between two of them ran 0.26 m under the
    // brink, where the ground, in the editor as in the game, hides it.
    // The track keeps its clearance along the whole curve the rail bends
    // along, between its first and last spans, which meet the work area's
    // dummy and the slot's ground.
    #[test]
    fn a_rail_over_a_brink_keeps_its_track_over_the_ground() {
        let mut field = round_island();
        let grid = field.grid;
        for z in 0..grid.depth {
            for x in 0..grid.width {
                let [wx, _] = grid.world(x, z);
                field.heights[z * grid.width + x] = if wx <= 70. {
                    10.
                } else {
                    (10. - 0.5 * (wx - 70.) * (wx - 70.)).max(1.)
                };
            }
        }
        let shape = harbours(Game::Anno1800).road_shape(Some(Piece::MineWorkArea));
        let (work, slot) = ([60., 10., 64.], [84., 1., 64.]);
        let rail = linked(&shape, &field, work, slot).path.unwrap();
        let (first, last) = (rail[1].p[0], rail[rail.len() - 2].p[0]);
        let curve = super::super::rail::Samples::new(&rail);
        let (mut low, mut at) = (f32::MAX, 0.);
        let mut d = 0.;
        while d < curve.length() {
            let (p, _) = curve.frame(d);
            if (first..last).contains(&p[0]) {
                let over = p[1] - field.height([p[0], p[2]]);
                if over < low {
                    (low, at) = (over, p[0]);
                }
            }
            d += 0.05;
        }
        assert!(
            low >= shape.clearance - 0.01,
            "track {low:.2} m over the ground at x {at:.2}"
        );
    }

    // A cave work area on a 56° slope: its dummy, 1.3 m on into the hill,
    // starts 0.4 m inside the ground, as a tenth of shipped cave rails
    // do. Asking for the full clearance beside that fixed end lifted the
    // next point 4.8 m over the ground (Moderate s 02, 2026-11-22); the
    // rail ramps down to its slot instead.
    #[test]
    fn a_cave_rail_from_inside_the_hill_ramps_down_to_its_slot() {
        let mut field = round_island();
        let grid = field.grid;
        for z in 0..grid.depth {
            for x in 0..grid.width {
                let [wx, _] = grid.world(x, z);
                field.heights[z * grid.width + x] = 3. + 1.5 * (70. - wx).max(0.);
            }
        }
        let cave = harbours(Game::Anno1800).road_shape(Some(Piece::CaveWorkArea));
        let slot = [70., 3., 64.];
        let at = cave.work_area_position(&field, slot, std::f32::consts::PI);
        let rail = linked(&cave, &field, at, slot).path.unwrap();
        let start = rail[0].p;
        assert!(
            start[1] < field.height([start[0], start[2]]),
            "{start:?} not buried"
        );
        for p in &rail[1..rail.len() - 1] {
            let over = p.p[1] - field.height([p.p[0], p.p[2]]);
            assert!(over < 1.5, "{over:.2} m over the ground at {:?}", p.p);
        }
        for w in rail.windows(2) {
            assert!(
                w[1].p[1] <= w[0].p[1] + 1e-4,
                "climbs: {:?}",
                [w[0].p, w[1].p]
            );
        }
    }

    #[test]
    fn a_link_bends_its_road_to_meet_both_ends() {
        let point = |x: f32, z: f32| PathPoint {
            p: [x, 0., z],
            i: Some([0.1, 0., 0.]),
            o: Some([-0.1, 0., 0.]),
            ..Default::default()
        };
        let link = Link {
            work: 1,
            slot: 2,
            work_at: [0., 0., 0.],
            path: vec![point(0., 0.), point(5., 0.), point(10., 0.)],
        };
        assert!(link.follow([0., 0., 0.], [10., 0., 0.]).is_none());
        // The slot moved 4 m north: the middle goes halfway, the start stays.
        let road = link.follow([0., 0., 0.], [10., 0., 4.]).unwrap();
        let z: Vec<f32> = road.iter().map(|p| p.p[2]).collect();
        assert_eq!(z, [0., 2., 4.]);
        assert_eq!(
            road[1].i,
            Some([0.1, 0., 0.]),
            "long roads keep their tangents"
        );

        // A two-point link gets shipped tangents: a sixth of the span.
        let short = Link {
            path: vec![point(0., 0.), point(3., 0.)],
            ..link
        };
        let road = short.follow([0., 0., 0.], [6., 0., 0.]).unwrap();
        assert_eq!(road[1].p, [6., 0., 0.]);
        assert_eq!(road[0].i, Some([-1., 0., 0.]));
        assert_eq!(road[1].o, Some([1., 0., 0.]));
    }

    #[test]
    fn library_finds_every_piece_on_shipped_islands() {
        let Some(install) = install() else {
            return;
        };
        let assets = assets(&install);
        for region in ["Roman", "Celtic"] {
            let lib = Library::load(
                crate::games::module(crate::games::of(&install)),
                &install,
                &assets,
                region,
            )
            .unwrap();
            let missing: Vec<_> = Piece::ALL
                .iter()
                .filter(|p| lib.guid(**p).is_none())
                .collect();
            eprintln!("{region}: missing {missing:?}");
            assert!(lib.guid(Piece::Coast).is_some() && lib.guid(Piece::MineSlot).is_some());
            // Marsh slots ship on Celtic islands only.
            assert_eq!(
                lib.guid(Piece::MarshSlot),
                (region == "Celtic").then_some(5282),
                "{region}"
            );
        }
    }

    #[test]
    fn installed_1800_resource_templates_cover_all_regions() {
        let Some(install) = install_1800() else {
            return;
        };
        let assets = assets(&install);
        for (region, guids) in [
            ("Moderate", &[1000029, 100417, 100849][..]),
            ("Colony01", &[1000029, 100417, 100849, 614][..]),
            ("Arctic", &[116037, 113750][..]),
            ("Africa", &[117741][..]),
        ] {
            let lib = Library::load(
                crate::games::module(crate::games::of(&install)),
                &install,
                &assets,
                region,
            )
            .unwrap();
            for &guid in guids {
                assert!(
                    lib.resource_of(guid).is_some(),
                    "{region}: no tool for {guid}"
                );
            }
            if region == "Africa" {
                assert_eq!(lib.guid(Piece::RiverWorkArea), Some(117742));
            }
        }
    }
}
