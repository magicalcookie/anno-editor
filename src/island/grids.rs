// Painting the island's gameplay grids (water, river, trees, environment,
// grit detail …) and the overlay colors that show one of them on the ground.
// Storage and save live in `anno_formats::a7m::grids`. On 2070 islands, the
// blocker grids its codec keeps instead (end of file).
use super::Drag;
use anno_core::game::IslandRules;
use anno_core::island::{Blockers, PathLayer};
use anno_formats::a7m::grids::{Kind, Layer};
use gpui_editor_kit::{
    gpu,
    heightfield::{Brush, Rect},
};

/// Layers the Grid tool offers, in picker order. Water infos follow the
/// heights on save (`anno_formats::a7m::water`); fog is undecoded.
pub const PAINTABLE: [Kind; 12] = [
    Kind::AreaIds,
    Kind::Water,
    Kind::River,
    Kind::Ford,
    Kind::ActiveMap,
    Kind::Trees,
    Kind::Herds,
    Kind::ForestChunks,
    Kind::Environment,
    Kind::Marsh,
    Kind::Grit,
    Kind::WaterFlow,
];

/// Layers the scene panel can show over the ground: the paintable ones,
/// then derived ones.
pub const OVERLAYS: [Kind; 12] = [
    Kind::Water,
    Kind::River,
    Kind::Ford,
    Kind::ActiveMap,
    Kind::AreaIds,
    Kind::Trees,
    Kind::Herds,
    Kind::ForestChunks,
    Kind::Environment,
    Kind::Marsh,
    Kind::Grit,
    Kind::WaterFlow,
];

pub fn label(kind: Kind) -> &'static str {
    match kind {
        Kind::ActiveMap => "Active area",
        Kind::Water => "Land / sea",
        Kind::River => "River",
        Kind::Ford => "Ford",
        Kind::Trees => "Tree cells",
        Kind::ForestChunks => "Forest chunks",
        Kind::Herds => "Herds",
        Kind::Environment => "Environment",
        // Construction area vs base area: where players may build.
        Kind::AreaIds => "Buildable",
        Kind::Grit => "Detail (grit)",
        Kind::GritAux => "Grit aux",
        Kind::WaterInfos => "Water infos",
        Kind::WaterFlow => "Water flow",
        Kind::Fog => "Fog",
        Kind::Marsh => "Marsh (irrigation)",
    }
}

/// Continuous layers blend toward the value with brush weight; the rest are
/// classes and set whole cells.
pub fn is_continuous(kind: Kind) -> bool {
    matches!(kind, Kind::Grit)
}

/// The value range a picker offers.
pub fn max_value(rules: &IslandRules, kind: Kind) -> u32 {
    match kind {
        Kind::Environment => (rules.environment_types.len() as u32).saturating_sub(1),
        Kind::ForestChunks | Kind::Grit => 255,
        Kind::Marsh => rules.marsh_tile,
        _ => 1,
    }
}

/// Legend for the value pickers: (value, label). Empty for continuous
/// layers; environment types are the game's own, in stored order.
pub fn legend(rules: &IslandRules, kind: Kind) -> Vec<(u32, &'static str)> {
    let bits = |off: &'static str, on: &'static str| vec![(0, off), (1, on)];
    match kind {
        Kind::ActiveMap => bits("outside", "active"),
        Kind::Water => bits("open sea", "island ground"),
        Kind::River => bits("no river", "river"),
        Kind::Ford => bits("no ford", "ford"),
        Kind::Trees => bits("no tree", "tree"),
        Kind::Herds => bits("no herd", "herd area"),
        Kind::Marsh => vec![(0, "dry"), (rules.marsh_tile, "marsh")],
        Kind::Environment => (rules.environment_types.iter())
            .enumerate()
            .map(|(i, &(name, _))| (i as u32, name))
            .collect(),
        _ => Vec::new(),
    }
}

/// Paints `value` into `layer` under the brush; `erase` paints 0. Class
/// layers set every cell whose centre is inside the radius and ignore the
/// falloff, so edges stay predictable on 1 m and 8 m grids.
pub fn paint(layer: &mut Layer, center: [f32; 2], brush: &Brush, value: u32, erase: bool) -> bool {
    let target = if erase { 0 } else { value };
    let cell = layer.cell_metres;
    let span = |c: f32, n: usize| {
        let lo = ((c - brush.radius) / cell).floor().max(0.) as usize;
        let hi = (((c + brush.radius) / cell).ceil().max(0.) as usize).min(n);
        lo..hi
    };
    let mut changed = false;
    for y in span(center[1], layer.height) {
        for x in span(center[0], layer.width) {
            let (wx, wz) = ((x as f32 + 0.5) * cell, (y as f32 + 0.5) * cell);
            let w = brush.weight((wx - center[0]).hypot(wz - center[1]));
            if w <= 0. {
                continue;
            }
            let old = layer.get(x, y);
            let new = if is_continuous(layer.kind) {
                let t = (w * brush.strength).clamp(0., 1.);
                (old as f32 + (target as f32 - old as f32) * t).round() as u32
            } else {
                target
            };
            if new != old {
                layer.set(x, y, new);
                changed = true;
            }
        }
    }
    changed
}

/// `WaterFlow` cells: bytes 0/1 are the flow x/z biased by 127, so still
/// water is `0x7f7f`. Shipped sea currents run at 10 or 20 per axis.
const FLOW_BIAS: f32 = 127.;
/// What a river and the flow brush write: as fast as the strongest shipped
/// sea current.
pub const FLOW_SPEED: f32 = 20.;

/// Flow vector of a `WaterFlow` word, x/z in the word's units.
pub fn flow(word: u32) -> [f32; 2] {
    [
        (word & 255) as f32 - FLOW_BIAS,
        ((word >> 8) & 255) as f32 - FLOW_BIAS,
    ]
}

/// `word` with its flow replaced by `v` (x/z), keeping byte 2 (a coast
/// mask, 0 on land) and byte 3.
pub fn with_flow(word: u32, v: [f32; 2]) -> u32 {
    let byte = |f: f32| (f + FLOW_BIAS).round().clamp(0., 255.) as u32;
    word & 0xffff_0000 | byte(v[1]) << 8 | byte(v[0])
}

/// Combs `WaterFlow` under the brush toward `dir` (a unit vector, the drag
/// direction) at [`FLOW_SPEED`], weighted by falloff and strength; `None`
/// calms it back to still water.
pub fn paint_flow(
    layer: &mut Layer,
    center: [f32; 2],
    brush: &Brush,
    dir: Option<[f32; 2]>,
) -> bool {
    let target = dir.map_or([0.; 2], |d| [d[0] * FLOW_SPEED, d[1] * FLOW_SPEED]);
    let cell = layer.cell_metres;
    let span = |c: f32, n: usize| {
        let lo = ((c - brush.radius) / cell).floor().max(0.) as usize;
        let hi = (((c + brush.radius) / cell).ceil().max(0.) as usize).min(n);
        lo..hi
    };
    let mut changed = false;
    for y in span(center[1], layer.height) {
        for x in span(center[0], layer.width) {
            let (wx, wz) = ((x as f32 + 0.5) * cell, (y as f32 + 0.5) * cell);
            let w = brush.weight((wx - center[0]).hypot(wz - center[1]));
            if w <= 0. {
                continue;
            }
            let t = (w * brush.strength).clamp(0., 1.);
            let old = layer.get(x, y);
            let [fx, fz] = flow(old);
            let new = with_flow(old, [fx + (target[0] - fx) * t, fz + (target[1] - fz) * t]);
            if new != old {
                layer.set(x, y, new);
                changed = true;
            }
        }
    }
    changed
}

/// Overlay color and opacity of a cell value; `None` leaves the ground as is.
pub fn overlay(rules: &IslandRules, kind: Kind, value: u32) -> Option<([f32; 3], f32)> {
    let rgb = |c: u32| [(c >> 16) & 255, (c >> 8) & 255, c & 255].map(|v| v as f32 / 255.);
    if kind == Kind::WaterFlow {
        // Hue is the direction (east red, south green-yellow, …), opacity
        // the speed; still water shows nothing.
        let [x, z] = flow(value);
        let speed = x.hypot(z);
        if speed < 1. {
            return None;
        }
        let hue = z.atan2(x).rem_euclid(std::f32::consts::TAU);
        let channel = |o: f32| ((hue - o).cos() * 0.5 + 0.5).clamp(0., 1.);
        let tau3 = std::f32::consts::TAU / 3.;
        return Some((
            [channel(0.), channel(tau3), channel(2. * tau3)],
            0.3 + 0.5 * (speed / FLOW_SPEED).min(1.),
        ));
    }
    if value == 0 {
        return None;
    }
    // Over the ground texture (mod.rs ground_color): enough to read on
    // grass and dark rock, with the ground's relief still showing through.
    let bit = |c: u32| Some((rgb(c), 0.75));
    match kind {
        Kind::ActiveMap => bit(crate::colors::LAYER_ACTIVE_MAP),
        Kind::Water => bit(crate::colors::LAYER_WATER),
        Kind::River => bit(crate::colors::LAYER_RIVER),
        Kind::Ford => bit(crate::colors::LAYER_FORD),
        Kind::Trees => bit(crate::colors::LAYER_TREES),
        Kind::Herds => bit(crate::colors::LAYER_HERDS),
        Kind::Marsh => bit(crate::colors::LAYER_MARSH),
        Kind::Environment => {
            let (_, argb) = rules.environment_types.get(value as usize)?;
            // Types without a debug color in assets.xml still need to show.
            let color = if *argb == 0 {
                crate::colors::LAYER_UNCOLORED
            } else {
                *argb
            };
            Some((rgb(color), 0.75))
        }
        Kind::AreaIds => match value {
            0x2001 => Some((rgb(crate::colors::AREA_CONSTRUCTION), 0.7)),
            0x0001 => Some((rgb(crate::colors::AREA_BASE), 0.5)),
            _ => Some((rgb(crate::colors::AREA_OTHER), 0.6)),
        },
        Kind::ForestChunks => {
            // Neighbouring chunk ids get far-apart hues.
            let h = (value.wrapping_mul(2_654_435_761) >> 8) & 0xffffff;
            Some((rgb(h | 0x404040), 0.75))
        }
        _ => Some(([1., 1., 1.], value as f32 / 255. * 0.8)),
    }
}

/// The layer's value under island metres `(x, z)`, 0 outside.
pub fn value_at(layer: &Layer, x: f32, z: f32) -> u32 {
    layer.cell_at(x, z).map_or(0, |(cx, cy)| layer.get(cx, cy))
}

// --- 2070's blocker grids -----------------------------------------------------
//
// On a 2070 island the Grid tool edits the two authored grids its codec
// keeps (`anno_core::island::Blockers`): a brush on either 0.5 m path
// layer, and the build-blocker polygons, whose points are added, dragged
// and removed. Both are in the document, so undo restores them.

/// What the Grid tool edits on a 2070 island.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BlockerEdit {
    Paths(PathLayer),
    Shapes,
}

impl BlockerEdit {
    pub const ALL: [BlockerEdit; 3] = [
        BlockerEdit::Paths(PathLayer::Blocked),
        BlockerEdit::Paths(PathLayer::HighGround),
        BlockerEdit::Shapes,
    ];

    pub fn label(self) -> &'static str {
        match self {
            BlockerEdit::Paths(PathLayer::Blocked) => "Path blockers",
            BlockerEdit::Paths(PathLayer::HighGround) => "High ground",
            BlockerEdit::Shapes => "Build blockers",
        }
    }
}

/// How close, in metres, a click must land to a polygon point to drag it,
/// or to the first point of the outline to close it.
const GRAB: f32 = 1.5;

/// Overlay color and opacity of a set path-blocker cell of `layer`.
pub fn path_overlay(layer: PathLayer) -> ([f32; 3], f32) {
    let rgb = |c: u32| [(c >> 16) & 255, (c >> 8) & 255, c & 255].map(|v| v as f32 / 255.);
    match layer {
        PathLayer::Blocked => (rgb(crate::colors::BUILD_BLOCKED), 0.6),
        PathLayer::HighGround => (rgb(crate::colors::AREA_ROUND), 0.6),
    }
}

/// Whether path cell (x, z) of `layer` under island metres is set.
pub fn path_at(blockers: &Blockers, layer: PathLayer, x: f32, z: f32) -> bool {
    let (cols, rows) = blockers.size();
    let (cx, cz) = ((x * 2.).floor(), (z * 2.).floor());
    cx >= 0.
        && cz >= 0.
        && (cx as usize) < cols
        && (cz as usize) < rows
        && blockers.get(layer, cx as usize, cz as usize)
}

/// Twice the signed area of a polygon in (x, z); positive is the winding
/// every shipped build blocker has.
fn area(points: &[[f64; 2]]) -> f64 {
    (0..points.len())
        .map(|i| {
            let (a, b) = (points[i], points[(i + 1) % points.len()]);
            a[0] * b[1] - b[0] * a[1]
        })
        .sum()
}

/// Whether `p` lies inside the polygon (even-odd).
pub(super) fn inside(points: &[[f64; 2]], p: [f64; 2]) -> bool {
    let mut on = false;
    for i in 0..points.len() {
        let (a, b) = (points[i], points[(i + 1) % points.len()]);
        if (a[1] > p[1]) != (b[1] > p[1])
            && p[0] < a[0] + (p[1] - a[1]) / (b[1] - a[1]) * (b[0] - a[0])
        {
            on = !on;
        }
    }
    on
}

impl super::IslandEditor {
    /// Whether the Grid tool edits 2070's blocker grids on this island.
    pub fn edits_blockers(&self) -> bool {
        self.island.doc.blockers().is_some()
    }

    /// A Grid dab on a path-blocker layer: sets the cells under the brush,
    /// or clears them with `clear`: whole cells, those whose centre lies
    /// under the brush.
    pub(super) fn path_dab(&mut self, center: [f32; 2], layer: PathLayer, clear: bool) -> bool {
        let radius = self.brush.radius;
        let Some(b) = self.island.doc.blockers_mut() else {
            return false;
        };
        if b.brush(layer, center[0], center[1], radius, !clear) == 0 {
            return false;
        }
        let rect = (self.island.doc.field.grid).footprint(center, radius + 0.5);
        self.dirty = self.dirty.union(rect);
        true
    }

    /// The corners a sculpt dab at `center` may move, `reach` metres out,
    /// and a ring of corners around them, as they stand before it: what
    /// [`Self::block_lifted`] measures the dab against. `None` where the
    /// island has no path blockers or the codec's rule is off.
    pub(super) fn lift_baseline(&self, center: [f32; 2], reach: f32) -> Option<(Rect, Vec<f32>)> {
        if !lift_rule(&self.island.doc) {
            return None;
        }
        let field = &self.island.doc.field;
        let g = field.grid;
        let r = g.footprint(center, reach);
        let area = Rect {
            x0: r.x0.saturating_sub(1),
            z0: r.z0.saturating_sub(1),
            x1: (r.x1 + 1).min(g.width),
            z1: (r.z1 + 1).min(g.depth),
        };
        let was = (area.z0..area.z1)
            .flat_map(|z| &field.heights[z * g.width + area.x0..z * g.width + area.x1])
            .copied()
            .collect();
        Some((area, was))
    }

    /// Blocks every 1 m cell a sculpt dab lifted from buildable height or
    /// below to above it, in the path blockers, as the stroke goes.
    /// `baseline` is [`Self::lift_baseline`] from before the dab and
    /// `moved` the corners it changed. Lowering ground blocks nothing and
    /// clears nothing; the Grid tool does. Gives how many cells it blocked.
    pub(super) fn block_lifted(&mut self, baseline: &(Rect, Vec<f32>), moved: Rect) -> usize {
        let (area, was) = baseline;
        let w = area.x1 - area.x0;
        // Cells with a moved corner whose four corners the baseline holds.
        let cells = Rect {
            x0: moved.x0.saturating_sub(1).max(area.x0),
            z0: moved.z0.saturating_sub(1).max(area.z0),
            x1: moved.x1.min(area.x1 - 1),
            z1: moved.z1.min(area.z1 - 1),
        };
        let before = |x: usize, z: usize| was[(z - area.z0) * w + x - area.x0];
        let n = block_lifted(&mut self.island.doc, before, cells);
        if n > 0 {
            self.dirty = self.dirty.union(Rect {
                x1: cells.x1 + 1,
                z1: cells.z1 + 1,
                ..cells
            });
        }
        n
    }

    /// After a change of the whole height field from `before` (an import),
    /// blocks the ground it lifted off buildable land, as a sculpt does.
    pub(super) fn block_lifted_from(&mut self, before: &gpui_editor_kit::heightfield::Heightfield) {
        if lift_rule(&self.island.doc) {
            let g = before.grid;
            let cells = Rect {
                x0: 0,
                z0: 0,
                x1: g.width - 1,
                z1: g.depth - 1,
            };
            block_lifted(&mut self.island.doc, |x, z| before.at(x, z), cells);
        }
    }

    /// The Grid tool's press on the build-blocker polygons, at ground
    /// point `at`: with `remove`, takes out the polygon under it; on a
    /// polygon's point, starts dragging it; otherwise adds a point to the
    /// outline being drawn, closing it on its first point or a double click.
    pub(super) fn shape_press(&mut self, at: [f32; 2], remove: bool, double: bool) -> Option<Drag> {
        let Some(shapes) = self.island.doc.blockers().and_then(|b| b.shapes.as_deref()) else {
            self.say("This island holds no build-blocker list to edit", true);
            return None;
        };
        let p = at.map(f64::from);
        if remove {
            let hit = (0..shapes.len())
                .rev()
                .find(|&s| inside(&shapes.points(s).collect::<Vec<_>>(), p));
            match hit {
                Some(shape) => self.remove_shape(shape),
                None => self.say("Shift-click inside a build blocker to remove it", true),
            }
            return None;
        }
        if self.outline.is_empty() {
            let near = (0..shapes.len()).find_map(|s| {
                let point = shapes
                    .points(s)
                    .position(|q| (q[0] - p[0]).hypot(q[1] - p[1]) <= f64::from(GRAB))?;
                Some((s, point))
            });
            if let Some((shape, point)) = near {
                return Some(Drag::ShapePoint {
                    shape,
                    point,
                    before: Box::new(self.island.doc.clone()),
                    changed: false,
                });
            }
        }
        let closes = self.outline.len() >= 3
            && (double || {
                let f = self.outline[0];
                (f[0] - at[0]).hypot(f[1] - at[1]) <= GRAB
            });
        if closes {
            self.close_outline();
        } else {
            self.outline.push(at);
            self.markers_stale = true;
        }
        None
    }

    /// Adds the outline being drawn as a build-blocker polygon, as one undo
    /// step. An outline drawn clockwise is stored the shipped way round.
    pub fn close_outline(&mut self) {
        let outline: Vec<[f64; 2]> = std::mem::take(&mut self.outline)
            .iter()
            .map(|p| p.map(f64::from))
            .collect();
        self.markers_stale = true;
        if outline.len() < 3 {
            return self.say("A build blocker needs three points", true);
        }
        let before = self.island.doc.clone();
        let added = (self.island.doc.blockers_mut())
            .and_then(|b| b.shapes_mut())
            .map(|s| s.add(&outline));
        match added {
            Some(Ok(_)) => {
                self.checkpoint_owned(before);
                self.say("Added a build blocker", false);
            }
            Some(Err(e)) => {
                self.island.doc = before;
                self.say(format!("Build blocker not added: {e:#}"), true);
            }
            None => self.island.doc = before,
        }
    }

    /// Drops the outline being drawn, as switching what the tool edits does.
    pub fn drop_outline(&mut self) {
        if !self.outline.is_empty() {
            self.outline.clear();
            self.markers_stale = true;
        }
    }

    /// Removes build-blocker polygon `shape`, as one undo step.
    pub fn remove_shape(&mut self, shape: usize) {
        self.checkpoint();
        if let Some(s) = self.island.doc.blockers_mut().and_then(|b| b.shapes_mut()) {
            s.remove(shape);
        }
        self.markers_stale = true;
        self.say("Removed a build blocker", false);
    }

    /// Moves point `point` of polygon `shape` to `to`, unless that would
    /// turn it inside out: the game's fill and the codec's writer need
    /// every polygon wound the shipped way. Gives whether it moved.
    pub fn move_shape_point(&mut self, shape: usize, point: usize, to: [f32; 2]) -> bool {
        let Some(shapes) = self.island.doc.blockers().and_then(|b| b.shapes.as_deref()) else {
            return false;
        };
        let mut points: Vec<[f64; 2]> = shapes.points(shape).collect();
        points[point] = to.map(f64::from);
        if area(&points) <= 0. {
            return false;
        }
        if let Some(s) = self.island.doc.blockers_mut().and_then(|b| b.shapes_mut()) {
            s.move_point(shape, point, to.map(f64::from));
        }
        self.markers_stale = true;
        true
    }

    /// The build-blocker polygons and the outline being drawn, as guides on
    /// the ground, while the Grid tool edits them.
    pub(super) fn shape_lines(&self, lines: &mut Vec<gpu::Line>) {
        if self.tool != super::Tool::Grid || self.blocker != BlockerEdit::Shapes {
            return;
        }
        let Some(shapes) = self.island.doc.blockers().and_then(|b| b.shapes.as_deref()) else {
            return;
        };
        let on_ground = |p: [f32; 2]| [p[0], self.island.doc.field.height(p) + 0.2, p[1]];
        let color = crate::colors::with_alpha(crate::colors::BUILD_BLOCKED, 0xe6);
        for s in 0..shapes.len() {
            let mut points: Vec<_> = shapes
                .points(s)
                .map(|p| on_ground(p.map(|c| c as f32)))
                .collect();
            if let Some(&first) = points.first() {
                points.push(first);
            }
            lines.push(gpu::Line {
                points,
                width: 3.,
                color,
            });
        }
        if !self.outline.is_empty() {
            let mut points: Vec<_> = self.outline.iter().map(|&p| on_ground(p)).collect();
            if let Some(at) = self.cursor {
                points.push(on_ground([at[0], at[2]]));
            }
            lines.push(gpu::Line {
                points,
                width: 2.,
                color: crate::colors::with_alpha(crate::colors::HANDLE, 0xe6),
            });
        }
    }
}

/// Whether height edits on `doc` block the ground they lift: an island with
/// path blockers (2070) while the brush option is on.
pub(super) fn lift_rule(doc: &anno_core::island::Doc) -> bool {
    anno2070::island::SCULPT.block_lifted_above.is_some() && doc.blockers().is_some()
}

/// Blocks, in `doc`'s path blockers, every 1 m cell of `cells` (cell
/// coordinates) that a height change took from a buildable band or below
/// to above it: the land's, and on an island under water the Deep Ocean
/// plateau's too (`Blockers::under_water`); `was` gives a corner's height before the change. The cells
/// live in the paint, so undo takes them back with the heights. The
/// document is what a 2070 save writes, so the save blocks exactly these
/// and never re-derives them from the file (a Grid stroke may clear them
/// afterwards). Gives how many cells it blocked; a change that lifts
/// nothing copies no grid.
pub(super) fn block_lifted(
    doc: &mut anno_core::island::Doc,
    was: impl Fn(usize, usize) -> f32,
    cells: Rect,
) -> usize {
    use anno_formats::isd::grids::gameplay_height;
    use anno2070::island::{BUILDABLE, UNDERWATER_BUILDABLE};
    let Some(under_water) = doc.blockers().map(|b| b.under_water) else {
        return 0;
    };
    let tops = [
        Some(*BUILDABLE.end()),
        under_water.then_some(*UNDERWATER_BUILDABLE.end()),
    ];
    let g = doc.field.grid;
    let heights = &doc.field.heights;
    let now = |x: usize, z: usize| heights[z * g.width + x];
    let lifted: Vec<(usize, usize)> = (cells.z0..cells.z1)
        .flat_map(|z| (cells.x0..cells.x1).map(move |x| (x, z)))
        .filter(|&(x, z)| {
            let before = [was(x, z), was(x + 1, z), was(x, z + 1), was(x + 1, z + 1)];
            let after = [now(x, z), now(x + 1, z), now(x, z + 1), now(x + 1, z + 1)];
            let (before, after) = (gameplay_height(before), gameplay_height(after));
            tops.iter()
                .flatten()
                .any(|&top| before <= top && after > top)
        })
        .collect();
    if lifted.is_empty() {
        return 0;
    }
    let Some(b) = doc.blockers_mut() else {
        return 0;
    };
    for &(x, z) in &lifted {
        let [mx, mz] = g.world(x, z);
        // A 1 m cell is the four 0.5 m ones within 0.4 m of its middle.
        let half = g.spacing / 2.;
        b.brush(PathLayer::Blocked, mx + half, mz + half, 0.4, true);
    }
    lifted.len()
}
