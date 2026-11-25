// Owns the island's water as the editor hands it to a game: rivers and
// lakes measured into one lattice of corners (`Inland`), each body with the
// look its record gives it, the `Water` pass a game draws them and its sea
// with, and what the games' seas share: the coast map measured from the
// ground (`coast`) and the engine's spectrum noise (`gauss`). The shading
// is each game's own (anno117::water, anno1800::water).
use crate::ambient::Sea;
use gpui_editor_kit::{gpu::Pass, heightfield::Heightfield};
use rayon::prelude::*;
use std::sync::Arc;

/// One river's or lake's look: its `River` or `Lake` record's float fields
/// as stored. The games' records hold different fields (117 colours and
/// depths, 1800 a packed colour, turbidity and flags), so each game's water
/// reads them with its own defaults.
#[derive(Clone, Debug, PartialEq)]
pub struct Look {
    pub lake: bool,
    pub fields: Vec<(String, Vec<f32>)>,
}

impl Look {
    /// A field's floats, `None` where the record leaves it out.
    pub fn field(&self, name: &str) -> Option<&[f32]> {
        (self.fields.iter())
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_slice())
    }

    /// A field's first value.
    pub fn number(&self, name: &str) -> Option<f32> {
        self.field(name)?.first().copied()
    }
}

/// The fastest flow a river record states (a path point's `u0`): the
/// engine's MAX_RIVER_FLOW_SPEED, 8 in both 117 and 1800 (1800's compiled
/// water_cluster_flow variants define it as 8.000000), which the flow map
/// stores speeds as a share of. The River tool's flow fields stop there.
pub const MAX_FLOW_SPEED: f32 = 8.;

/// A lattice corner: no water, or a surface height, the look it takes and
/// how the water flows there.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Corner {
    pub y: f32,
    /// Index into [`Inland::looks`]; [`Corner::DRY`] without water.
    pub look: u32,
    /// Direction of flow in world X and Z, unit length or zero.
    pub flow: [f32; 2],
    /// Flow speed as the record states it (a path point's `u0`).
    pub speed: f32,
    /// 1 on the water; toward 0 over the first and last stretch of a river
    /// whose record fades its ends (the games' vsRiver Alpha).
    pub fade: f32,
    /// The same toward every river's ends, whatever its record says: the
    /// flow map fades them all (psClusterFlow's BlendFactor).
    pub flow_fade: f32,
}

impl Corner {
    pub const DRY: u32 = u32::MAX;
    pub const fn dry() -> Self {
        Self {
            y: 0.,
            look: Self::DRY,
            flow: [0.; 2],
            speed: 0.,
            fade: 1.,
            flow_fade: 1.,
        }
    }
}

/// A vertex of a river drawn as its own strip of quads, as the games build
/// InlandWaterVertex strips, rather than on the lattice: 117's lava, whose
/// surface bows across the stream, and every 1800 river
/// ([`Water::river_strips`]).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, bytemuck::Pod, bytemuck::Zeroable)]
pub struct StripVertex {
    pub position: [f32; 3],
    /// Index into [`Inland::looks`].
    pub look: u32,
    /// Up from the strip as it slopes down the river (the vertex Normal).
    pub normal: [f32; 3],
    /// vsRiver's Alpha: 0 at an end its record fades.
    pub fade: f32,
    /// Across the strip from -0.5 at shore index 0 to 0.5 at shore index 1
    /// (the game's IVF_IS_RIGHT_SHORE).
    pub across: f32,
}

/// Rivers drawn as strips: their vertices and triangles.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Strips {
    pub vertices: Vec<StripVertex>,
    pub indices: Vec<u32>,
}

/// Every river and lake of an island as one lattice: `size[0]` by
/// `size[1]` corners `step` apart from `origin`, row by row. Cells with
/// four wet corners are water. The corners double as a flow map. Rivers a
/// game draws as their own strips come in `strips` too.
#[derive(Clone, Debug, PartialEq)]
pub struct Inland {
    pub origin: [f32; 2],
    pub step: f32,
    pub size: [u32; 2],
    pub corners: Vec<Corner>,
    pub looks: Vec<Look>,
    pub strips: Strips,
    /// `gpu_corners` (the flow map) and `triangles`, packed where the
    /// lattice is made: off the UI thread in the editor, which packing on
    /// a pass's first frame held for 8-11 ms on an 1800 island.
    packed: Vec<[u32; 6]>,
    indices: Vec<u32>,
    split: u32,
}

impl Inland {
    /// The lattice and its flow map over `ground` (see `gpu_corners`).
    pub fn new(
        origin: [f32; 2],
        step: f32,
        size: [u32; 2],
        corners: Vec<Corner>,
        looks: Vec<Look>,
        ground: Option<&Heightfield>,
    ) -> Inland {
        let mut inland = Inland {
            origin,
            step,
            size,
            corners,
            looks,
            strips: Strips::default(),
            packed: Vec::new(),
            indices: Vec::new(),
            split: 0,
        };
        inland.packed = inland.pack_corners(ground);
        (inland.indices, inland.split) = inland.pack_triangles();
        inland
    }

    /// Corners for the GPU with the river flow map: y, look, speed as a
    /// share of [`MAX_FLOW_SPEED`], fade, then the flow direction.
    pub fn gpu_corners(&self) -> &[[u32; 6]] {
        &self.packed
    }

    /// The water cells' triangles, lakes first, and how many indices
    /// the lakes take.
    pub fn triangles(&self) -> (&[u32], u32) {
        (&self.indices, self.split)
    }

    /// Corners for the GPU: y, look, then the flow map's speed share and
    /// fade, then its direction. The flow map is the river cluster's
    /// (water_cluster_flow.fx, the same in 117 and 1800), one texel per
    /// corner as both games keep one texel per world unit
    /// (CInlandWaterCluster sizes its texture to its bounds, Anno1800.exe
    /// build 16367353 0x141e41340): psClusterFlow writes each river corner's direction,
    /// zeroed with the speed where the surface lies on or under `ground`
    /// (sampled an eighth of a unit back, as the shader does) and faded at
    /// every river's ends (the direction only: its alpha blend state leaves
    /// the first speed written), then one csExpandFlow pass and
    /// csDilateFlow. Without `ground` every corner counts as under water.
    /// TODO: the flow target's clear colour was not traced; texels no strip
    /// covers are taken as still water, so they never feed a neighbour.
    fn pack_corners(&self, ground: Option<&Heightfield>) -> Vec<[u32; 6]> {
        let share = |speed: f32| (speed / MAX_FLOW_SPEED).clamp(0., 1.);
        let [w, d] = self.size.map(|v| v as usize);
        let lake = |c: &Corner| (self.looks.get(c.look as usize)).is_none_or(|l| l.lake);
        // psClusterFlow: direction and speed share per texel, and whether
        // it lies under water (the shader's depth, which nothing else here
        // reads). Lakes and dry corners write nothing.
        let mut dirs = vec![[0f32; 2]; w * d];
        let mut speeds = vec![0f32; w * d];
        let mut wet = vec![0u64; (w * d).div_ceil(64)];
        let rows = dirs.par_chunks_mut(w).zip(speeds.par_chunks_mut(w));
        let under: Vec<Vec<usize>> = rows
            .enumerate()
            .map(|(z, (dirs, speeds))| {
                let mut under = Vec::new();
                for x in 0..w {
                    let c = &self.corners[z * w + x];
                    if c.look == Corner::DRY || lake(c) {
                        continue;
                    }
                    if let Some(f) = ground {
                        let at = [
                            self.origin[0] + x as f32 * self.step - 0.125,
                            self.origin[1] + z as f32 * self.step - 0.125,
                        ];
                        if c.y - f.height(at) <= 0. {
                            continue;
                        }
                    }
                    let l = c.flow[0].hypot(c.flow[1]).max(1e-6);
                    dirs[x] = [c.flow[0] / l * c.flow_fade, c.flow[1] / l * c.flow_fade];
                    speeds[x] = share(c.speed);
                    under.push(x);
                }
                under
            })
            .collect();
        for (z, xs) in under.iter().enumerate() {
            for &x in xs {
                let i = z * w + x;
                wet[i / 64] |= 1 << (i % 64);
            }
        }
        let is_wet = |i: usize| wet[i / 64] & (1 << (i % 64)) != 0;
        // csExpandFlow, its one iteration: every wet texel takes its wet
        // neighbours' mean direction, those flowing toward it weighing most.
        // Its own does not count, so the flow shortens where it turns. Dry
        // neighbours write a zero direction, which never flows in.
        let mut expanded = vec![[0f32; 2]; w * d];
        expanded.par_chunks_mut(w).enumerate().for_each(|(z, row)| {
            for (x, out) in row.iter_mut().enumerate() {
                if !is_wet(z * w + x) {
                    *out = dirs[z * w + x];
                    continue;
                }
                let (mut sum, mut weight) = ([0f32; 2], 0f32);
                for nz in z.saturating_sub(1)..(z + 2).min(d) {
                    for nx in x.saturating_sub(1)..(x + 2).min(w) {
                        let n = nz * w + nx;
                        if (nx, nz) == (x, z) || !is_wet(n) {
                            continue;
                        }
                        let (dx, dz) = (x as f32 - nx as f32, z as f32 - nz as f32);
                        let l = dx.hypot(dz);
                        let dir = dirs[n];
                        let k = ((dir[0] * dx + dir[1] * dz) / l).clamp(0., 1.) * 0.9 + 0.1;
                        sum = [sum[0] + dir[0] * k, sum[1] + dir[1] * k];
                        weight += k;
                    }
                }
                if weight > 0. {
                    *out = [sum[0] / weight, sum[1] / weight];
                }
            }
        });
        // csDilateFlow: still texels (speed 0) take their flowing
        // neighbours' mean, so the flow blends into the banks instead of to
        // standstill.
        let mut out = vec![[0u32; 6]; w * d];
        out.par_chunks_mut(w).enumerate().for_each(|(z, row)| {
            for (x, out) in row.iter_mut().enumerate() {
                let i = z * w + x;
                let (mut dir, mut speed) = (expanded[i], speeds[i]);
                if speed <= 0. {
                    let (mut sum, mut sum_speed, mut n) = ([0f32; 2], 0f32, 0f32);
                    for nz in z.saturating_sub(1)..(z + 2).min(d) {
                        for nx in x.saturating_sub(1)..(x + 2).min(w) {
                            let k = nz * w + nx;
                            if speeds[k] > 0. {
                                sum = [sum[0] + expanded[k][0], sum[1] + expanded[k][1]];
                                sum_speed += speeds[k];
                                n += 1.;
                            }
                        }
                    }
                    if n > 0. {
                        (dir, speed) = ([sum[0] / n, sum[1] / n], sum_speed / n);
                    }
                }
                let c = &self.corners[i];
                *out = [
                    c.y.to_bits(),
                    c.look,
                    speed.to_bits(),
                    c.fade.to_bits(),
                    dir[0].to_bits(),
                    dir[1].to_bits(),
                ];
            }
        });
        out
    }

    /// Two triangles per wet cell, lake cells first, so each game draws
    /// lakes and rivers with one range each: (indices, lake index count).
    /// A cell belongs to its first corner's look.
    fn pack_triangles(&self) -> (Vec<u32>, u32) {
        let [w, d] = self.size.map(|v| v as usize);
        let (mut lakes, mut rivers) = (Vec::new(), Vec::new());
        for z in 0..d.saturating_sub(1) {
            for x in 0..w.saturating_sub(1) {
                let q = [
                    z * w + x,
                    z * w + x + 1,
                    (z + 1) * w + x + 1,
                    (z + 1) * w + x,
                ];
                if q.iter().any(|&i| self.corners[i].look == Corner::DRY) {
                    continue;
                }
                let look = self.corners[q[0]].look as usize;
                let lake = self.looks.get(look).is_some_and(|l| l.lake);
                let into = if lake { &mut lakes } else { &mut rivers };
                into.extend([0, 2, 1, 0, 3, 2].map(|k| q[k] as u32));
            }
        }
        let split = lakes.len() as u32;
        lakes.append(&mut rivers);
        (lakes, split)
    }
}

/// A game's water pass for one island view (`Module::water`): made once
/// and kept, so what it built on the GPU (a wave simulation, uploaded maps)
/// lasts across scenes. The view lists it in its scene's passes while it
/// shows a sea.
pub trait Water: Pass {
    /// What the next frames draw: `sea`'s preset (`None` draws nothing)
    /// along the coast of `field`, and `inland`'s rivers and lakes, waves
    /// held at `time` seconds or on the clock. While `hold` (a sculpt
    /// stroke) the coast measured last stays: measuring takes tens of
    /// milliseconds, so the stroke's end brings a new one.
    fn show(
        &self,
        sea: Option<&Arc<dyn Sea>>,
        field: &Arc<Heightfield>,
        hold: bool,
        inland: Option<Arc<Inland>>,
        time: Option<f32>,
    );

    /// Whether this pass draws `show`'s rivers and lakes; when not, the
    /// island draws them as the kit's flat water.
    fn shades_inland(&self) -> bool {
        true
    }

    /// Whether this pass draws every river from its own strip
    /// ([`Inland::strips`]), as 1800 draws each River record as its own
    /// InlandWaterVB mesh, instead of from the lattice; the lattice then
    /// still carries the flow map and the lakes. Rivers at different heights
    /// that meet at a waterfall stay apart, where the lattice would join
    /// them with walls.
    fn river_strips(&self) -> bool {
        false
    }

    /// The island's own water render maps (the rd3d `WaterInfos` and
    /// `WaterFlow` grids), for games whose water reads them; `None` for an
    /// island without. Kept by pointer: the island replaces the `Arc` on
    /// every edit.
    fn island_maps(&self, _grids: Option<&Arc<anno_formats::a7m::grids::Grids>>) {}

    /// The island's file as loaded (`Island::native`), for games whose
    /// water reads what only the file holds (2070's lakes); `None` for an
    /// island without one. Kept by pointer, as `island_maps`.
    fn island_file(&self, _file: Option<&Arc<[u8]>>) {}

    /// How hard it rains on the water, 0 dry to 1, for games that draw
    /// rain on it.
    fn rain(&self, _rain: f32) {}

    /// The island at `path`'s seasons (`SceneParts::seasons`: how high the
    /// dry season and the flood raise the rivers) and how far its season
    /// has come, 0 dry to 1 flood, for games whose water changes with them
    /// (117's Egypt, whose rivers also flow by a map of the whole session).
    fn season(&self, _path: &str, _river_heights: Option<[f32; 2]>, _flood: f32) {}

    /// What it may leave out from the next frame on (reflections, the
    /// finest waves).
    fn detail(&self, _detail: crate::ambient::Detail) {}

    /// The mirror the scene draws for this water to reflect under `detail`
    /// (`gpu::Scene::reflection`): one more opaque scene draw a view, so
    /// none unless a game's sea reads it.
    fn reflection(
        &self,
        _detail: crate::ambient::Detail,
    ) -> Option<gpui_editor_kit::gpu::Reflection> {
        None
    }
}

/// An island's rd3d water render maps as the GPU samples them, cell (0, 0)
/// at the island's corner, `cell` world units a side: WaterInfos (depth,
/// coast waves, beach or on lakes duckweed, painted foam) and WaterFlow
/// (flow X and Z biased by 127, distance from the coast), one byte each,
/// as stored. Both games' water reads them the same way (GetWaterInfos,
/// GetWaterFlow); each uploads them as RGBA8.
#[derive(Debug, PartialEq)]
pub struct IslandMaps {
    pub width: u32,
    pub height: u32,
    pub cell: f32,
    pub info: Vec<[u8; 4]>,
    pub flow: Vec<[u8; 4]>,
}

impl IslandMaps {
    /// The maps of `grids`; `None` when it has no WaterInfos. An island
    /// without WaterFlow reads still water at full coast distance.
    pub fn read(grids: &anno_formats::a7m::grids::Grids) -> Option<Self> {
        use anno_formats::a7m::grids::Kind;
        let info = grids.layer(Kind::WaterInfos)?;
        let flow = grids.layer(Kind::WaterFlow);
        let (w, h) = (info.width, info.height);
        if w == 0 || h == 0 {
            return None;
        }
        let bytes = |v: u32| v.to_le_bytes();
        let mut out = Self {
            width: w as u32,
            height: h as u32,
            cell: info.cell_metres,
            info: Vec::with_capacity(w * h),
            flow: Vec::with_capacity(w * h),
        };
        let flow = flow.filter(|f| f.width == w && f.height == h);
        for y in 0..h {
            for x in 0..w {
                out.info.push(bytes(info.get(x, y)));
                out.flow
                    .push(bytes(flow.map_or(0x00ff_7f7f, |f| f.get(x, y))));
            }
        }
        Some(out)
    }

    /// 1 / the maps' extent in world units (cWaterInfoCoord's scale).
    pub fn scale(&self) -> [f32; 2] {
        [self.width, self.height].map(|n| 1. / (n as f32 * self.cell).max(1e-3))
    }
}

/// The island's water maps a water pass draws by, read off the UI thread
/// whenever the island's grids change (a sculpt stroke's end, a paint).
/// What was read last stays drawn meanwhile; grids asked for while one
/// read runs are read next, and ones in between are skipped.
#[derive(Default)]
pub struct IslandMapsReader {
    /// The grids asked for last, by pointer.
    asked: usize,
    drawn: Option<Arc<IslandMaps>>,
    reading: Option<std::sync::mpsc::Receiver<Option<IslandMaps>>>,
    next: Option<Arc<anno_formats::a7m::grids::Grids>>,
}

impl IslandMapsReader {
    pub fn want(&mut self, grids: Option<&Arc<anno_formats::a7m::grids::Grids>>) {
        let key = grids.map_or(0, |g| Arc::as_ptr(g) as usize);
        if key == self.asked {
            return;
        }
        self.asked = key;
        match grids {
            None => {
                self.drawn = None;
                self.next = None;
            }
            Some(g) if self.reading.is_some() => self.next = Some(g.clone()),
            Some(g) => self.read(g.clone()),
        }
    }

    /// Takes a finished read, starting the one asked for since; true when
    /// new maps landed.
    pub fn land(&mut self) -> bool {
        use std::sync::mpsc::TryRecvError;
        let Some(rx) = &self.reading else {
            return false;
        };
        let landed = match rx.try_recv() {
            Ok(maps) => {
                self.drawn = maps.map(Arc::new);
                true
            }
            Err(TryRecvError::Empty) => return false,
            Err(TryRecvError::Disconnected) => false,
        };
        self.reading = None;
        if let Some(g) = self.next.take() {
            self.read(g);
        }
        landed
    }

    /// The maps to draw by; `None` until the island's first land, or for
    /// an island without.
    pub fn maps(&self) -> Option<&Arc<IslandMaps>> {
        self.drawn.as_ref()
    }

    /// Whether a read is still to land: a pass keeps frames coming
    /// meanwhile, so the new maps show without waiting for input.
    pub fn reading(&self) -> bool {
        self.reading.is_some()
    }

    fn read(&mut self, grids: Arc<anno_formats::a7m::grids::Grids>) {
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        let sent = grids.clone();
        let spawned = std::thread::Builder::new()
            .name("water maps".into())
            .spawn(move || {
                let _ = tx.send(IslandMaps::read(&sent));
            });
        match spawned {
            Ok(_) => self.reading = Some(rx),
            Err(_) => self.drawn = IslandMaps::read(&grids).map(Arc::new),
        }
    }
}

/// The coast as the ocean sees it, per texel of a grid over the island:
/// signed distance to the coast in world units (positive at sea, negative
/// on land and in lakes, where waves stop), the water's depth as the game
/// normalises it (0 at the waterline, 1 deep), how much a wave may break
/// there, and how much of a beach (1) rather than a cliff (0) the shore is.
#[derive(Clone, Debug, PartialEq)]
pub struct Coast {
    pub width: u32,
    pub height: u32,
    /// World X and Z of the grid's corner, and its extent.
    pub origin: [f32; 2],
    pub size: [f32; 2],
    pub texels: Vec<[f32; 4]>,
    /// World units of water the depth channel counts as fully deep, so the
    /// physical waves can tell the ground's height from it.
    pub depth_scale: f32,
}

/// The coast map a water pass draws by, measured off the UI thread:
/// [`coast`] took 30–140 ms of an island's first frame on a busy machine,
/// and again at the end of every sculpt stroke. One measurement runs at a
/// time; a newer heightfield asked for meanwhile is measured next, and
/// ones in between are skipped. Until a measurement lands the last one
/// stays drawn.
#[derive(Default)]
pub struct CoastMap {
    /// The coast drawn, and the heightfield it was measured on (by
    /// pointer: the island replaces its `Arc` on every edit).
    drawn: Option<(usize, Arc<Coast>)>,
    measuring: Option<(usize, std::sync::mpsc::Receiver<Coast>)>,
    next: Option<(usize, Arc<Heightfield>)>,
}

impl CoastMap {
    /// Asks for `field`'s coast. While `hold` (a sculpt stroke) a drawn
    /// coast stays and nothing is measured: the stroke's end asks again.
    pub fn want(&mut self, field: &Arc<Heightfield>, hold: bool) {
        let key = Arc::as_ptr(field) as usize;
        let drawn = self.drawn.as_ref().map(|d| d.0);
        if hold && drawn.is_some() {
            self.next = None;
            return;
        }
        match &self.measuring {
            Some((k, _)) if *k == key => self.next = None,
            // Even the drawn heightfield (an undo): what measures now would
            // replace it on landing.
            Some(_) => self.next = Some((key, field.clone())),
            None if drawn == Some(key) => {}
            None => self.measure(key, field.clone()),
        }
    }

    /// Takes a finished measurement, starting the one asked for since;
    /// true when a new coast landed.
    pub fn land(&mut self) -> bool {
        use std::sync::mpsc::TryRecvError;
        let Some((key, rx)) = &self.measuring else {
            return false;
        };
        let landed = match rx.try_recv() {
            Ok(coast) => {
                self.drawn = Some((*key, Arc::new(coast)));
                true
            }
            Err(TryRecvError::Empty) => return false,
            // The thread died (a panic, already reported): ask again with
            // the next heightfield.
            Err(TryRecvError::Disconnected) => false,
        };
        self.measuring = None;
        if let Some((key, field)) = self.next.take() {
            self.measure(key, field);
        }
        landed
    }

    /// The coast to draw by; `None` until the island's first lands.
    pub fn coast(&self) -> Option<&Arc<Coast>> {
        self.drawn.as_ref().map(|d| &d.1)
    }

    /// Whether a measurement is still to land: a pass keeps frames coming
    /// meanwhile, so the new coast shows without waiting for input.
    pub fn measuring(&self) -> bool {
        self.measuring.is_some()
    }

    fn measure(&mut self, key: usize, field: Arc<Heightfield>) {
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        // On the pool kept for interactive work: the global one may be
        // busy streaming models for minutes after an island opens.
        let sent = field.clone();
        let spawned = std::thread::Builder::new()
            .name("coast map".into())
            .spawn(move || {
                let _ = tx.send(gpui_editor_kit::task::interactive(|| coast(&sent)));
            });
        match spawned {
            Ok(_) => self.measuring = Some((key, rx)),
            // No thread to be had: measure here rather than never.
            Err(_) => {
                let coast = coast(&field);
                self.drawn = Some((key, Arc::new(coast)));
            }
        }
    }
}

/// World units per coast map texel.
const TEXEL: f32 = 2.;
/// Open sea around the heightfield, so offshore waves (52 to 82 units out
/// in the shipped presets) are reached before the map's edge.
const MARGIN: f32 = 128.;
/// Depth, in world units, the coast map counts as fully deep. TODO: the
/// game normalises WaterInfo's depth by a scale not found; 4 units (16 m)
/// lets the two longest bands grow over the first few metres of slope.
const DEPTH_SCALE: f32 = 4.;
/// Signed distance given to lakes, at which the game stops ocean waves.
const LAKE: f32 = -10.;
/// Ground height read past the heightfield's edge, in the margin: open sea.
/// `Heightfield::height` clamps to the edge sample, so an island whose land
/// reaches its border carried that land out through the margin, the map's
/// border read as land, and the ocean (which samples the map clamped) cut
/// the sea out in strips from the island to the horizon.
const OPEN_SEA: f32 = -100.;

/// The coast map for `field`: per texel the signed distance to the coast
/// in world units (positive at sea, negative on land), lakes the sea does
/// not reach set to [`LAKE`], the water's depth over [`DEPTH_SCALE`], and
/// how much of a beach the ground is (flat 1, cliff 0), in the third and
/// fourth channels as the ocean reads them (WaterInfo Waves and Beach).
pub fn coast(field: &Heightfield) -> Coast {
    let [w0, d0] = field.grid.size();
    let origin = [field.grid.origin[0] - MARGIN, field.grid.origin[1] - MARGIN];
    let width = ((w0 + 2. * MARGIN) / TEXEL).ceil() as usize;
    let height = ((d0 + 2. * MARGIN) / TEXEL).ceil() as usize;
    let at = |x: usize, z: usize| {
        [
            origin[0] + (x as f32 + 0.5) * TEXEL,
            origin[1] + (z as f32 + 0.5) * TEXEL,
        ]
    };
    let (lo, size) = (field.grid.origin, [w0, d0]);
    let ground = |p: [f32; 2]| {
        let inside = (0..2).all(|k| p[k] >= lo[k] && p[k] <= lo[k] + size[k]);
        if inside { field.height(p) } else { OPEN_SEA }
    };
    // Each pass below but the flood fill is per texel, column or row, and
    // runs across cores: one thread took 10 ms of the UI thread's frame
    // on a 117 island's first scene, and again after every stroke.
    let heights: Vec<f32> = (0..width * height)
        .into_par_iter()
        .map(|i| ground(at(i % width, i / width)))
        .collect();
    // Sea is the water the map's border reaches; other water is a lake.
    let mut sea = vec![0u64; (width * height).div_ceil(64)];
    let set = |bits: &mut [u64], i: usize| bits[i / 64] |= 1 << (i % 64);
    let has = |bits: &[u64], i: usize| bits[i / 64] & (1 << (i % 64)) != 0;
    let mut stack: Vec<usize> = (0..width * height)
        .filter(|&i| {
            let (x, z) = (i % width, i / width);
            (x == 0 || z == 0 || x == width - 1 || z == height - 1) && heights[i] <= 0.
        })
        .collect();
    while let Some(i) = stack.pop() {
        if has(&sea, i) {
            continue;
        }
        set(&mut sea, i);
        let (x, z) = (i % width, i / width);
        let mut push = |j: usize| {
            if heights[j] <= 0. && !has(&sea, j) {
                stack.push(j);
            }
        };
        if x > 0 {
            push(i - 1);
        }
        if x + 1 < width {
            push(i + 1);
        }
        if z > 0 {
            push(i - width);
        }
        if z + 1 < height {
            push(i + width);
        }
    }
    // Squared distances in texels to the nearest land, and to the nearest
    // sea, from which the signed distance follows.
    let far = 1e12f32;
    let (to_land, to_sea) = rayon::join(
        || {
            let seeds: Vec<f32> = (heights.iter())
                .map(|&h| if h > 0. { 0. } else { far })
                .collect();
            distance_transform(&seeds, width, height)
        },
        || {
            let seeds: Vec<f32> = (0..width * height)
                .map(|i| if has(&sea, i) { 0. } else { far })
                .collect();
            distance_transform(&seeds, width, height)
        },
    );
    let texels = (0..width * height)
        .into_par_iter()
        .map(|i| {
            let h = heights[i];
            let signed = if has(&sea, i) {
                to_land[i].sqrt().min(1000.) * TEXEL
            } else if h > 0. {
                -to_sea[i].sqrt().min(1000.) * TEXEL
            } else {
                LAKE
            };
            let depth = (-h / DEPTH_SCALE).clamp(0., 1.);
            let p = at(i % width, i / width);
            let dx = ground([p[0] + 1., p[1]]) - ground([p[0] - 1., p[1]]);
            let dz = ground([p[0], p[1] + 1.]) - ground([p[0], p[1] - 1.]);
            let slope = (dx * dx + dz * dz).sqrt() * 0.5;
            // TODO: the game paints Beach per session by a rule not found;
            // gentle ground counts as beach, steep as cliff.
            let t = ((slope - 0.2) / 0.6).clamp(0., 1.);
            let beach = 1. - t * t * (3. - 2. * t);
            // Waves spawn only at sea: on land the coast map reads no
            // depth, which the physical waves would take for a waterline.
            let waves = if signed > 0. { beach } else { 0. };
            [signed, depth, waves, beach]
        })
        .collect();
    Coast {
        width: width as u32,
        height: height as u32,
        origin,
        size: [width as f32 * TEXEL, height as f32 * TEXEL],
        texels,
        depth_scale: DEPTH_SCALE,
    }
}

/// Squared Euclidean distance to the nearest zero of `seeds` (0 at a
/// target, a large value elsewhere), Felzenszwalb and Huttenlocher's
/// separable transform: columns, then rows.
fn distance_transform(seeds: &[f32], width: usize, height: usize) -> Vec<f32> {
    // Columns into a column-major buffer, so each column is one slice.
    let mut columns = vec![0f32; width * height];
    columns.par_chunks_mut(height).enumerate().for_each_init(
        || (Vec::new(), Vec::new()),
        |(line, result), (x, column)| {
            line.clear();
            line.extend((0..height).map(|z| seeds[z * width + x]));
            lower_envelope(line, result);
            column.copy_from_slice(result);
        },
    );
    let mut out = vec![0f32; width * height];
    out.par_chunks_mut(width).enumerate().for_each_init(
        || (Vec::new(), Vec::new()),
        |(line, result), (z, row)| {
            line.clear();
            line.extend((0..width).map(|x| columns[x * height + z]));
            lower_envelope(line, result);
            row.copy_from_slice(result);
        },
    );
    out
}

/// One line of the transform: the lower envelope of the parabolas rooted
/// at each sample. f64: unreached samples hold 1e12, beside which f32
/// loses the squared offsets.
fn lower_envelope(f: &[f32], out: &mut Vec<f32>) {
    let n = f.len();
    let f: Vec<f64> = f.iter().map(|&v| f64::from(v)).collect();
    let mut v = vec![0usize; n];
    let mut z = vec![0f64; n + 1];
    let cross = |q: usize, p: usize| {
        ((f[q] + (q * q) as f64) - (f[p] + (p * p) as f64)) / (2. * (q as f64 - p as f64))
    };
    let mut k = 0;
    z[0] = f64::NEG_INFINITY;
    z[1] = f64::INFINITY;
    for q in 1..n {
        let mut s = cross(q, v[k]);
        while s <= z[k] {
            k -= 1;
            s = cross(q, v[k]);
        }
        k += 1;
        v[k] = q;
        z[k] = s;
        z[k + 1] = f64::INFINITY;
    }
    out.clear();
    k = 0;
    for q in 0..n {
        while z[k + 1] < q as f64 {
            k += 1;
        }
        let d = q as f64 - v[k] as f64;
        out.push((d * d + f[v[k]]) as f32);
    }
}

/// TeaRand (functions.fxh, the same in 117 and 1800): one round of the Tiny Encryption Algorithm.
pub fn tea(mut v0: u32, mut v1: u32) -> u32 {
    let delta = 0x9e37_79b9u32;
    let k = [0xa341_316cu32, 0xc801_3ea4, 0xad90_777d, 0x7e95_761e];
    v0 = v0.wrapping_add(
        (v1 << 4).wrapping_add(k[0]) ^ v1.wrapping_add(delta) ^ (v1 >> 5).wrapping_add(k[1]),
    );
    v1 = v1.wrapping_add(
        (v0 << 4).wrapping_add(k[2]) ^ v0.wrapping_add(delta) ^ (v0 >> 5).wrapping_add(k[3]),
    );
    v1
}

/// Gauss (functions_water.fxh): Box-Muller over two TEA draws taken modulo
/// `seed`.
pub fn gauss(x: u32, y: u32, seed: u32) -> f32 {
    let u1 = ((tea(x, y) % seed) as f32 / seed as f32).max(1e-6);
    let u2 = (tea(y, x) % seed) as f32 / seed as f32;
    (-2. * u1.ln()).sqrt() * (2. * std::f32::consts::PI * u2).cos()
}

/// IEEE half-float bits of `v`, rounded to nearest; overflow saturates to
/// infinity and values below the smallest normal flush to zero.
pub fn half(v: f32) -> u16 {
    let bits = v.to_bits();
    let sign = ((bits >> 16) & 0x8000) as u16;
    let exp = ((bits >> 23) & 0xff) as i32 - 127 + 15;
    if v.is_nan() {
        return sign | 0x7e00;
    }
    if exp <= 0 {
        return sign;
    }
    if exp >= 31 {
        return sign | 0x7c00;
    }
    let mantissa = bits & 0x7f_ffff;
    // Round half up on the 13 dropped bits; a carry out of the mantissa
    // bumps the exponent, which is the right result.
    let h = ((exp as u32) << 10 | mantissa >> 13) + ((mantissa >> 12) & 1);
    sign | (h.min(0x7c00) as u16)
}

/// The engine's line FFT as both games' oceans run it (csFFTRadix2*), for
/// a compute shader to append: it expects `N`, `HALF` (N / 2) and a
/// workgroup `line` of N complex values, with N / 2 threads, and brings
/// `cmul`.
pub const FFT_WGSL: &str = r#"
fn cmul(a: vec2<f32>, b: vec2<f32>) -> vec2<f32> {
    return vec2<f32>(a.x * b.x - a.y * b.y, a.x * b.y + a.y * b.x);
}

// csFFTRadix2*: a radix-2 Stockham transform of `line` in place, forward
// sign, no scaling, natural order out. Thread t combines t and t + N/2.
fn fft(t: u32) {
    var part = 1u;
    loop {
        if part >= N { break; }
        let a = line[t];
        let b = line[t + HALF];
        let k = t % part;
        let angle = -6.2831853 / f32(2u * part) * f32(k);
        let wb = cmul(vec2<f32>(cos(angle), sin(angle)), b);
        workgroupBarrier();
        let base = (t / part) * 2u * part + k;
        line[base] = a + wb;
        line[base + part] = a - wb;
        workgroupBarrier();
        part = part * 2u;
    }
}
"#;

#[cfg(test)]
mod tests {
    use super::*;
    use gpui_editor_kit::heightfield::Grid;

    fn look(lake: bool) -> Look {
        Look {
            lake,
            fields: Vec::new(),
        }
    }

    // A 4x3 lattice: a river cell, a lake cell and a dry column. Lake cells
    // come first so each pipeline draws one range, and dry corners beside
    // the river take its flow.
    #[test]
    fn lattice_splits_lakes_from_rivers_and_dilates_flow() {
        let wet = |look, flow: [f32; 2]| Corner {
            y: 1.,
            look,
            flow,
            speed: 4.,
            fade: 1.,
            flow_fade: 1.,
        };
        let r = wet(0, [1., 0.]);
        let l = Corner {
            speed: 0.,
            ..wet(1, [0., 0.])
        };
        let dry = Corner::dry();
        #[rustfmt::skip]
        let corners = vec![
            r, r, l, l, dry,
            r, r, l, l, dry,
            dry, dry, dry, dry, dry,
        ];
        let inland = Inland::new(
            [0.; 2],
            1.,
            [5, 3],
            corners,
            vec![look(false), look(true)],
            None,
        );
        let (indices, split) = inland.triangles();
        assert_eq!((indices.len(), split), (18, 6));
        assert_eq!(inland.corners[indices[0] as usize].look, 1);
        assert_eq!(inland.corners[indices[6] as usize].look, 0);
        let packed = inland.gpu_corners();
        // Below the river: the mean of its two wet neighbours above, its
        // speed (4 of the 8 the flow map tops out at) as a share.
        let below = packed[10];
        assert_eq!(f32::from_bits(below[4]), 1.);
        assert_eq!(f32::from_bits(below[2]), 0.5);
        // Beside the lake: as still as the lake.
        assert_eq!(f32::from_bits(packed[14][2]), 0.);
    }

    // A river turning from +X to +Z halfway across: csExpandFlow leaves
    // straight water flowing at full length, shortens the flow where it
    // turns, and keeps the speed share; the dry ring around the water takes
    // its neighbours' flow, and water under the ground stands still.
    #[test]
    fn the_flow_map_shortens_where_rivers_turn() {
        let (w, d) = (12u32, 8u32);
        let corners = (0..w * d)
            .map(|i| {
                let (x, z) = (i % w, i / w);
                let wet = (1..w - 1).contains(&x) && (1..d - 1).contains(&z);
                Corner {
                    y: 1.,
                    look: if !wet {
                        Corner::DRY
                    } else if x == 1 {
                        1
                    } else {
                        0
                    },
                    flow: if x < 6 { [1., 0.] } else { [0., 1.] },
                    speed: 4.,
                    fade: 1.,
                    flow_fade: 1.,
                }
            })
            .collect::<Vec<_>>();
        let looks = vec![look(false), look(true)];
        let inland = Inland::new([0.; 2], 1., [w, d], corners.clone(), looks.clone(), None);
        let map = inland.gpu_corners();
        let at = |x: u32, z: u32| {
            let c = map[(z * w + x) as usize];
            let dir = [f32::from_bits(c[4]), f32::from_bits(c[5])];
            (dir, f32::from_bits(c[2]))
        };
        let length = |(dir, _): ([f32; 2], f32)| dir[0].hypot(dir[1]);
        assert!((length(at(3, 4)) - 1.).abs() < 1e-4, "{:?}", at(3, 4));
        assert!(length(at(6, 4)) < 0.95, "turning: {:?}", at(6, 4));
        assert_eq!(at(3, 4).1, 0.5, "4 of 8 as a share");
        assert!(length(at(5, 0)) > 0.5, "the bank takes the flow");
        // Ground at 2, over the surface at 1: no flow anywhere.
        let grid = Grid {
            width: 13,
            depth: 9,
            spacing: 1.,
            origin: [-0.5; 2],
        };
        let ground = Heightfield::new(grid, vec![2.; 13 * 9]);
        let buried = Inland::new([0.; 2], 1., [w, d], corners, looks, Some(&ground));
        assert!(
            buried
                .gpu_corners()
                .iter()
                .all(|c| f32::from_bits(c[2]) == 0.)
        );
    }

    #[test]
    fn looks_read_their_record_fields() {
        let l = Look {
            lake: false,
            fields: vec![
                ("Turbidity".into(), vec![1.5]),
                ("ColorDeep".into(), vec![0.1, 0.2, 0.3]),
            ],
        };
        assert_eq!(l.number("Turbidity"), Some(1.5));
        assert_eq!(l.field("ColorDeep"), Some(&[0.1, 0.2, 0.3][..]));
        assert_eq!(l.number("ViewDepth"), None);
    }

    // A round island with a pond on it, in a sea that deepens away from
    // it: the coast map is positive at sea and grows outward, negative on
    // land, the pond a lake, and depth rises off the shore.
    #[test]
    fn coast_map_measures_from_the_shore_and_finds_lakes() {
        let grid = Grid {
            width: 129,
            depth: 129,
            spacing: 1.,
            origin: [-64., -64.],
        };
        let heights = (0..grid.len())
            .map(|i| {
                let (x, z) = ((i % 129) as f32 - 64., (i / 129) as f32 - 64.);
                let r = (x * x + z * z).sqrt();
                if r < 6. { -1. } else { 20. - r }
            })
            .collect();
        let field = Heightfield::new(grid, heights);
        let c = coast(&field);
        let sample = |x: f32, z: f32| {
            let tx = ((x - c.origin[0]) / TEXEL) as usize;
            let tz = ((z - c.origin[1]) / TEXEL) as usize;
            c.texels[tz * c.width as usize + tx]
        };
        assert_eq!(sample(0., 0.)[0], LAKE, "the pond");
        assert!(sample(12., 0.)[0] < -4., "inland {:?}", sample(12., 0.));
        let (near, far) = (sample(26., 0.), sample(60., 0.));
        assert!((near[0] - 6.).abs() < 3., "6 units out {near:?}");
        assert!((far[0] - 40.).abs() < 3., "40 units out {far:?}");
        // A unit under water is a quarter of the depth scale; 40 under is
        // all of it.
        let shallow = sample(21., 0.);
        assert!(
            (shallow[1] - 0.25).abs() < 0.1 && far[1] == 1.,
            "{shallow:?} {far:?}"
        );
        // The margin past the heightfield keeps counting.
        assert!(sample(150., 0.)[0] > 120.);
    }

    // Land that runs to the heightfield's edge stops there: the margin and
    // the map's border are open sea, so the ocean, which reads the map
    // clamped past its edge, does not cut the sea out beyond it.
    #[test]
    fn land_at_the_edge_leaves_the_margin_open_sea() {
        let grid = Grid {
            width: 65,
            depth: 65,
            spacing: 1.,
            origin: [-32., -32.],
        };
        let field = Heightfield::new(grid, vec![10.; grid.len()]);
        let c = coast(&field);
        let (w, h) = (c.width as usize, c.height as usize);
        let border = (0..w)
            .flat_map(|x| [x, (h - 1) * w + x])
            .chain((0..h).flat_map(|z| [z * w, z * w + w - 1]));
        for i in border {
            assert!(
                c.texels[i][0] > 100. && c.texels[i][1] == 1.,
                "{i}: {:?}",
                c.texels[i]
            );
        }
        let inland = c.texels[(h / 2) * w + w / 2];
        assert!(inland[0] < -20., "{inland:?}");
    }

    /// A round island `r` units across, as a fresh `Arc` the way an edit
    /// replaces the island's.
    fn round_island(r: f32) -> Arc<Heightfield> {
        let grid = Grid {
            width: 65,
            depth: 65,
            spacing: 1.,
            origin: [-32., -32.],
        };
        let heights = (0..grid.len())
            .map(|i| {
                let (x, z) = ((i % 65) as f32 - 32., (i / 65) as f32 - 32.);
                r - (x * x + z * z).sqrt()
            })
            .collect();
        Arc::new(Heightfield::new(grid, heights))
    }

    /// Lands measurements until none is left, at most 10 s.
    fn settle(map: &mut CoastMap) -> usize {
        let started = std::time::Instant::now();
        let mut landed = 0;
        while map.measuring() {
            assert!(started.elapsed().as_secs() < 10, "a coast never landed");
            landed += usize::from(map.land());
            std::thread::yield_now();
        }
        landed
    }

    // The coast measures off the asking thread: nothing to draw by until
    // the first lands, then the old one stays drawn while the next
    // measures, a stroke keeps it, and of several heightfields asked for
    // during one measurement only the last is measured next.
    #[test]
    fn coast_maps_measure_in_the_background_newest_last() {
        let (small, mid, big) = (round_island(6.), round_island(12.), round_island(20.));
        let mut map = CoastMap::default();
        map.want(&small, false);
        assert!(map.coast().is_none(), "nothing until the first lands");
        assert_eq!(settle(&mut map), 1);
        assert_eq!(**map.coast().unwrap(), coast(&small));

        // Mid-stroke the drawn coast stays and nothing measures.
        map.want(&big, true);
        assert!(!map.measuring());

        map.want(&small, false);
        assert!(!map.measuring(), "already drawn");
        map.want(&mid, false);
        assert_eq!(**map.coast().unwrap(), coast(&small), "old one meanwhile");
        // Asked for while `mid` measures: only the newest goes next.
        map.want(&big, false);
        map.want(&small, false);
        map.want(&big, false);
        assert_eq!(settle(&mut map), 2, "mid, then big; small skipped");
        assert_eq!(**map.coast().unwrap(), coast(&big));

        // Back to the drawn heightfield while another measures (an undo
        // mid-measurement): it is what ends up drawn.
        map.want(&mid, false);
        map.want(&big, false);
        settle(&mut map);
        assert_eq!(**map.coast().unwrap(), coast(&big));
    }
}
