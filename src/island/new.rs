// New islands: the sizes the game ships, the wizard's settings, and turning
// a shipped island of the chosen size into a blank one under a new path.
//
// A shipped island serves as the skeleton because gamedata/rd3d carry many
// size-dependent managers we do not generate yet; every visible and
// gameplay layer is replaced, and saves write every terrain file because
// nothing under the new path is shipped.
use super::{IslandEditor, Shading, grids, tint};
use crate::terrain;
use anno_formats::{
    game::Game,
    grids::{Grids, Kind},
    tint::TintMap,
    vfs::Install,
    water,
};
use anyhow::{Context as _, Result, ensure};
use gpui_editor_kit::{
    heightfield::{Brush, Falloff, Grid, Heightfield},
    history::History,
    texture::Texture,
};
use std::{collections::BTreeMap, sync::Arc};

/// A shipped island usable as the skeleton for a size.
#[derive(Clone, Debug)]
pub struct Skeleton {
    pub path: String,
    pub world: [u32; 2],
    /// Province folder, e.g. `roman`.
    pub region: String,
}

/// How the terrain starts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Start {
    /// Sea floor everywhere; sculpt or import from scratch.
    Sea,
    /// A low rounded plateau with a beach, ready to sculpt.
    Plateau,
    /// The plateau with rolling hills.
    Hills,
}

impl Start {
    pub const ALL: [(Start, &'static str); 3] = [
        (Start::Sea, "Empty sea"),
        (Start::Plateau, "Plateau"),
        (Start::Hills, "Hills"),
    ];
}

/// What the wizard collects.
#[derive(Clone, Debug)]
pub struct Spec {
    pub name: String,
    pub skeleton: Skeleton,
    pub start: Start,
    pub listing: crate::project::Listing,
}

/// Sea floor of a blank island, metres. Shipped open sea sits around −6 to −10.
const SEA_FLOOR: f32 = -8.;
/// Plateau top above sea level, metres.
const PLATEAU: f32 = 4.;
/// Water and active area extend this far beyond land, metres: the shallow
/// coast band shipped islands mark as island ground.
const COAST_BAND: f32 = 12.;

/// Every island size shipped with the game, one skeleton each (the first by
/// path), keyed by region then size.
pub fn skeletons(
    install: &Install,
    assets: Option<&anno_formats::assets::Assets>,
) -> Vec<Skeleton> {
    let mut by_size: BTreeMap<(String, [u32; 2]), Skeleton> = BTreeMap::new();
    for s in islands(install, assets) {
        by_size.entry((s.region.clone(), s.world)).or_insert(s);
    }
    by_size.into_values().collect()
}

/// Every regular square island the game ships (1800: the `Normal` kind),
/// by path.
pub fn islands(install: &Install, assets: Option<&anno_formats::assets::Assets>) -> Vec<Skeleton> {
    let from_assets = install.game().profile().islands_from_assets;
    let loaded;
    let assets = if from_assets && assets.is_none() {
        loaded = anno_formats::assets::Assets::load(install).ok();
        loaded.as_ref()
    } else {
        assets
    };
    let normal: std::collections::HashSet<_> = assets
        .into_iter()
        .flat_map(|a| a.iter())
        .filter_map(|a| a.island.as_ref())
        .filter(|i| i.kind.is_empty() || i.kind.split(';').any(|k| k == "Normal"))
        .map(|i| i.path.as_str())
        .collect();
    let mut out = Vec::new();
    for id in install.search(".a7minfo", 100_000) {
        let info = install.file_path(id);
        let Some(a7m) = info
            .strip_suffix("info")
            .filter(|p| install.file(p).is_some())
        else {
            continue;
        };
        if from_assets && !normal.contains(a7m) {
            continue;
        }
        let Ok(bytes) = install.read_file(id) else {
            continue;
        };
        let Ok(info) = anno_formats::a7minfo::A7mInfo::parse(&bytes) else {
            continue;
        };
        let world = info.map_size;
        let region = install.game().island_region(a7m).to_ascii_lowercase();
        if !(64..=4096).contains(&world[0]) || world[0] != world[1] {
            continue;
        }
        out.push(Skeleton {
            path: a7m.to_owned(),
            world,
            region,
        });
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    out
}

/// A file-safe island name: lowercase ASCII letters, digits and `_`.
pub fn file_name(name: &str) -> String {
    let mut out: String = name
        .trim()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect();
    out.truncate(48);
    out
}

/// Virtual path of a new island, next to the shipped ones of its region.
pub fn island_path(game: Game, region: &str, name: &str) -> String {
    let name = file_name(name);
    let [before, after] = game.profile().island_folder;
    format!("{before}{region}{after}/{name}/{name}.a7m")
}

impl IslandEditor {
    pub(super) fn land_environment(&self) -> &'static str {
        let profile = self.game.profile();
        let region = self.region();
        (profile.land_environments.iter())
            .find(|(r, _)| *r == region)
            .map_or(profile.environments.meadow, |&(_, env)| env)
    }

    /// A blank island for `spec`, built on its skeleton.
    pub fn create(install: &Install, spec: &Spec) -> Result<Self> {
        let name = file_name(&spec.name);
        ensure!(
            !name.is_empty() && name.chars().any(|c| c != '_'),
            "the island needs a name"
        );
        let path = island_path(install.game(), &spec.skeleton.region, &spec.name);
        ensure!(
            !install.exists(&path),
            "{path} already exists in the game or the mod project"
        );
        let mut editor = Self::load(install, &spec.skeleton.path)
            .with_context(|| format!("skeleton {}", spec.skeleton.path))?;
        editor.blank(path, spec.start);
        if install.game().profile().regional_prop_set
            && let Some(sets) = &editor.prop_sets
            && let Some(name) = sets
                .choices(&editor.region())
                .into_iter()
                .find(|n| !matches!(*n, "Default" | "None"))
        {
            editor.doc.session.vegetation_prop_set = name.to_owned();
        }
        editor.listing = spec.listing.clone();
        Ok(editor)
    }

    /// Replaces everything the player sees or plays on with a blank island
    /// under `path`.
    fn blank(&mut self, path: String, start: Start) {
        self.path = path;
        self.source.mark_unshipped();
        let size = self.doc.field.grid.size()[0];
        shape(Arc::make_mut(&mut self.doc.field), start);

        Arc::make_mut(&mut self.doc.props).clear();
        // TODO(alex): shipped islands carry markers (spawn points, pier
        // positions, coast paths) the game may require; place them once the
        // registration research says which are mandatory.
        Arc::make_mut(&mut self.doc.objects).clear();
        self.selection.clear();

        if let Some(m) = &mut self.doc.materials {
            // One ground material everywhere: plain grass, else the set's
            // first non-rock one. Id 0 is beach on celtic sets and mud on
            // roman ones, which read as snow and dirt in game.
            let base = self
                .palette
                .as_ref()
                .and_then(|p| {
                    let stem = |f: &str| f.rsplit('/').next().unwrap_or(f).to_owned();
                    p.materials
                        .iter()
                        .find(|(_, m)| stem(&m.file).starts_with("grass_01"))
                        .or_else(|| {
                            p.materials.iter().find(|(_, m)| {
                                let name = stem(&m.file);
                                self.game.profile().regional_materials
                                    && (name.contains("grass")
                                        || name.contains("gras")
                                        || name.contains("snow_"))
                            })
                        })
                        .or_else(|| p.materials.iter().find(|(_, m)| !m.file.contains("/rock/")))
                        .map(|(&id, _)| id)
                })
                .unwrap_or(1);
            let all = Brush {
                radius: size * 2.,
                strength: 1.,
                falloff: Falloff::Constant,
                ..Brush::default()
            };
            m.paint([size / 2., size / 2.], &all, base);
            self.paint = base;
        }

        self.doc.tint = Some(Arc::new(TintMap::neutral(tint::WORK_LEVEL, self.world())));
        // Nothing shipped to compare against: save always bakes tint.
        self.tint_loaded = None;

        if let Some(g) = &mut self.doc.grids {
            let g = Arc::make_mut(g);
            for kind in grids::PAINTABLE {
                // Still water is 0x7f7f; `derive_land` sets the coast byte.
                let blank = if kind == Kind::WaterFlow { 0x7f7f } else { 0 };
                if let Some(layer) = g.layer_mut(kind) {
                    for y in 0..layer.height {
                        for x in 0..layer.width {
                            layer.set(x, y, blank);
                        }
                    }
                }
            }
            // The template's shore paint would follow its old coast.
            water::clear_paint(g);
        }
        let environment = self.land_environment();
        derive_land(&mut self.doc, environment);
        // Areas follow the land too, or the template island's stay:
        // buildable where the slope allows.
        self.derive_areas_from_slope();

        self.shipped = terrain::ShippedMeshes::empty(size);
        self.shipped_meshes.clear();
        self.layers.shipped = false;
        self.map = None;
        if self.shading == Shading::Map {
            self.shading = Shading::Ground;
        }
        self.ground_stale = true;
        // The splat still shows the template island's materials; without
        // this the editor shows ground the saved island does not have.
        self.splat_stale = true;
        self.history = History::new(false);
        self.next_id = 1;
        self.is_new = true;
        self.load_log.clear();
        self.rebuild_tiles(None);
        self.say(
            "New island: sculpt or import terrain, then save with Ctrl S. Land and buildable areas follow the terrain (Terrain → Rebuild land and buildable areas)",
            false,
        );
    }

    /// Re-derives land/sea, the active area and the ambience environment
    /// from the heights, as one undo step.
    pub fn derive_land(&mut self) {
        if self.doc.grids.is_none() {
            return self.say("This island has no grids", true);
        }
        self.checkpoint();
        let environment = self.land_environment();
        derive_land(&mut self.doc, environment);
        // Areas follow the land too, or the template island's stay:
        // buildable where the slope allows.
        self.derive_areas_from_slope();
        self.rebuild_tiles(None);
        self.say(
            "Land, active area and environment follow the terrain",
            false,
        );
    }
}

/// Starting heights. Shapes are radial with a little low-frequency wobble
/// so coastlines are not perfect circles.
fn shape(field: &mut Heightfield, start: Start) {
    let g = field.grid;
    let size = g.size()[0];
    for z in 0..g.depth {
        for x in 0..g.width {
            let [wx, wz] = g.world(x, z);
            field.heights[z * g.width + x] = start_height(wx, wz, size, start);
        }
    }
}

/// The starting height at world `wx, wz` on a `size` metre map; `shape`
/// and the wizard's preview both sample it, so the sketch is the terrain.
fn start_height(wx: f32, wz: f32, size: f32, start: Start) -> f32 {
    let c = size / 2.;
    let (dx, dz) = (wx - c, wz - c);
    let angle = dz.atan2(dx);
    let wobble = 0.06 * (3. * angle).sin() + 0.04 * (5. * angle + 1.3).sin();
    // 0 at the centre, 1 at the intended coast.
    let d = (dx.hypot(dz) / (size * 0.36)) * (1. - wobble);
    let land = 1. - smoothstep(0.85, 1.1, d);
    let hills = if start == Start::Hills {
        let n = (wx * 0.035).sin() * (wz * 0.03 + 0.7).cos()
            + 0.5 * (wx * 0.07 + 2.).cos() * (wz * 0.08).sin();
        (n * 0.5 + 0.5) * 9. * (1. - smoothstep(0.3, 0.85, d))
    } else {
        0.
    };
    match start {
        Start::Sea => SEA_FLOOR,
        Start::Plateau | Start::Hills => SEA_FLOOR + (PLATEAU - SEA_FLOOR) * land + hills,
    }
}

/// A `side` × `side` RGBA sketch of the starting terrain on a `size` metre
/// map, north up: sea by depth, beach, then grass lit from the north-west
/// so plateau and hills read apart.
/// The preview's colour for ground `y` metres above the sea, unshaded:
/// beach sand, then low ground green, high ground olive to dry brown. The
/// wizard's height key draws the same ramp.
pub fn land_color(y: f32) -> [f32; 3] {
    if y < 1.2 {
        return [210., 192., 140.];
    }
    let lift = ((y - 1.2) / (PLATEAU + 8.)).clamp(0., 1.);
    [92. + 70. * lift, 138. - 10. * lift, 66. - 6. * lift]
}

pub fn start_preview(start: Start, size: f32, side: u32) -> Vec<u8> {
    let n = side as usize;
    let step = size / side as f32;
    let h = |x: usize, z: usize| {
        start_height(
            (x as f32 + 0.5) * step,
            (z as f32 + 0.5) * step,
            size,
            start,
        )
    };
    let heights: Vec<f32> = (0..n * n).map(|i| h(i % n, i / n)).collect();
    let at = |x: usize, z: usize| heights[z.min(n - 1) * n + x.min(n - 1)];
    let mut rgba = Vec::with_capacity(n * n * 4);
    for z in 0..n {
        for x in 0..n {
            let y = at(x, z);
            let [r, g, b] = if y < -0.3 {
                // Deeper is darker; the sea floor is the darkest blue.
                let t = ((y - SEA_FLOOR) / -SEA_FLOOR).clamp(0., 1.);
                [
                    (22. + 30. * t) as u8,
                    (60. + 50. * t) as u8,
                    (88. + 50. * t) as u8,
                ]
            } else if y < 1.2 {
                [210, 192, 140]
            } else {
                // Lit from the north-west, slopes exaggerated: at true
                // scale a plateau and gentle hills both read as one flat
                // green, and the preview is how the start is chosen.
                let dx = at(x + 1, z) - at(x.saturating_sub(1), z);
                let dz = at(x, z + 1) - at(x, z.saturating_sub(1));
                // Saturating smoothly: clamped, steep hillsides hit the
                // limit and left a hard straight seam along each ridge.
                let shade = 1. - 0.5 * ((dx + dz) * 1.2 / step).tanh();
                let mut c = land_color(y).map(|v| v * shade);
                // A contour every 2 m: rings at a plateau's edge, rings
                // around each hill, none on flat ground.
                let band = |y: f32| (y / 2.).floor();
                if band(y) != band(at(x + 1, z)) || band(y) != band(at(x, z + 1)) {
                    c = c.map(|v| v * 0.72);
                }
                c.map(|v| v.clamp(0., 255.) as u8)
            };
            rgba.extend_from_slice(&[r, g, b, 255]);
        }
    }
    rgba
}

fn smoothstep(a: f32, b: f32, x: f32) -> f32 {
    let t = ((x - a) / (b - a)).clamp(0., 1.);
    t * t * (3. - 2. * t)
}

/// Water (island ground incl. the coast band), active area, environment and
/// the water render bytes (`anno_formats::water`) from the heights.
pub(super) fn derive_land(doc: &mut super::Doc, land_env: &str) {
    doc.land_heights = doc.field.clone();
    let Some(g) = &mut doc.grids else { return };
    let g = Arc::make_mut(g);
    derive_land_grids(g, &doc.field, land_env, None);
}

/// How far a height change reaches into the land grids, metres: the
/// active area's band (twice the coast band, plus a cell) and the water
/// grid's coast distance (13 m), with room to spare.
const LAND_REACH: f32 = COAST_BAND * 2. + 16.;
/// How far it reaches into the areas: slope patches under `MIN_PATCH` m²
/// flip, so a patch the edit touches can span that far.
const AREA_REACH: f32 = LAND_REACH + super::harbour::MIN_PATCH as f32 + 8.;

/// Merge terrain-derived changes while retaining cells painted elsewhere.
pub(super) fn sync_land(
    doc: &mut super::Doc,
    coasts: &[Vec<anno_formats::objects::PathPoint>],
    land_env: &str,
) -> bool {
    let Some(grids) = &mut doc.grids else {
        return false;
    };
    let Some(changes) = LandChanges::between(
        doc.field.grid,
        &doc.land_heights.heights,
        &doc.field.heights,
        grids,
        coasts,
        land_env,
    ) else {
        return false;
    };
    changes.apply(Arc::make_mut(grids));
    doc.land_heights = doc.field.clone();
    true
}

/// The layers `sync_land` derives from the heights.
const DERIVED: [Kind; 6] = [
    Kind::Water,
    Kind::ActiveMap,
    Kind::Environment,
    Kind::WaterFlow,
    Kind::WaterInfos,
    Kind::AreaIds,
];

/// Land-grid cells that follow a height change, as (layer, x, y, value):
/// computed from snapshots off the UI thread, applied on it.
#[derive(Default)]
pub(crate) struct LandChanges(Vec<(Kind, usize, usize, u32)>);

impl LandChanges {
    /// What deriving the land grids from heights `now` instead of `was`
    /// changes in `grids`; `None` when the heights are the same.
    ///
    /// Each derived cell depends only on heights within a reach of it, so
    /// only the changed heights' bounds plus that reach derive: over the
    /// whole island, twice, a stroke's release took 375 ms on a 1 km island.
    pub(super) fn between(
        grid: Grid,
        was: &[f32],
        now: &[f32],
        grids: &Grids,
        coasts: &[Vec<anno_formats::objects::PathPoint>],
        land_env: &str,
    ) -> Option<Self> {
        let (mut lo, mut hi) = ([f32::MAX; 2], [f32::MIN; 2]);
        for (k, (a, b)) in was.iter().zip(now).enumerate() {
            if a != b {
                let p = grid.world(k % grid.width, k / grid.width);
                lo = [lo[0].min(p[0]), lo[1].min(p[1])];
                hi = [hi[0].max(p[0]), hi[1].max(p[1])];
            }
        }
        if lo[0] > hi[0] {
            return None;
        }
        let around = |r: f32| Some([lo[0] - r, lo[1] - r, hi[0] + r, hi[1] + r]);
        Some(Self::within(
            grid,
            was,
            now,
            grids,
            coasts,
            land_env,
            around(LAND_REACH),
            around(AREA_REACH),
        ))
    }

    /// `between`, deriving the land grids within `land_window` and the
    /// areas within `area_window` (every cell for `None`).
    #[allow(clippy::too_many_arguments)]
    fn within(
        grid: Grid,
        was: &[f32],
        now: &[f32],
        grids: &Grids,
        coasts: &[Vec<anno_formats::objects::PathPoint>],
        land_env: &str,
        land_window: Option<[f32; 4]>,
        area_window: Option<[f32; 4]>,
    ) -> Self {
        let before = Heightfield::new(grid, was.to_vec());
        let after = Heightfield::new(grid, now.to_vec());
        // Only the layers derived here; the island has 14.
        let mut old = grids.only(&DERIVED);
        let mut new = old.clone();
        derive_land_grids(&mut old, &before, land_env, land_window);
        derive_land_grids(&mut new, &after, land_env, land_window);
        let paths: Vec<_> = coasts.iter().map(Vec::as_slice).collect();
        for (grids, heights) in [(&mut old, &before), (&mut new, &after)] {
            if let Some(ground) = grids.layer(Kind::Water).cloned()
                && let Some(areas) = grids.layer_mut(Kind::AreaIds)
            {
                super::harbour::derive_areas(
                    &ground,
                    areas,
                    &paths,
                    super::harbour::Interior::Slope(heights),
                    area_window,
                );
            }
        }
        let mut out = Vec::new();
        for kind in DERIVED {
            if let (Some(was), Some(now)) = (old.layer(kind), new.layer(kind)) {
                // The areas' window holds the land grids' too.
                let (cols, rows) = now.cells_within(area_window);
                for y in rows {
                    for x in cols.clone() {
                        if was.get(x, y) != now.get(x, y) {
                            out.push((kind, x, y, now.get(x, y)));
                        }
                    }
                }
            }
        }
        Self(out)
    }

    pub(super) fn apply(&self, grids: &mut Grids) {
        for &(kind, x, y, value) in &self.0 {
            if let Some(layer) = grids.layer_mut(kind) {
                layer.set(x, y, value);
            }
        }
    }
}

/// The land grids from `field`, in the cells within `window` (world metres
/// `[x0, z0, x1, z1]`, every cell without).
fn derive_land_grids(g: &mut Grids, field: &Heightfield, land_env: &str, window: Option<[f32; 4]>) {
    let profile = g.game().profile();
    // Land within `reach` metres of (x, z), checked on a coarse ring.
    let near_land = |x: f32, z: f32, reach: f32| {
        let steps = 8;
        (0..steps).any(|k| {
            let a = k as f32 / steps as f32 * std::f32::consts::TAU;
            [0., 0.5, 1.]
                .iter()
                .any(|&f| field.height([x + a.cos() * reach * f, z + a.sin() * reach * f]) > 0.)
        })
    };
    let env = |name| profile.environment_value(name);
    for (kind, reach) in [
        (Kind::Water, COAST_BAND),
        (Kind::ActiveMap, COAST_BAND * 2.),
        (Kind::Environment, 0.),
    ] {
        let Some(layer) = g.layer_mut(kind) else {
            continue;
        };
        let cell = layer.cell_metres;
        let (cols, rows) = layer.cells_within(window);
        for y in rows {
            for x in cols.clone() {
                let (wx, wz) = ((x as f32 + 0.5) * cell, (y as f32 + 0.5) * cell);
                let value = match kind {
                    Kind::Environment => {
                        if field.height([wx, wz]) > 0. {
                            env(land_env)
                        } else {
                            env(profile.environments.sea)
                        }
                    }
                    _ => near_land(wx, wz, reach + cell) as u32,
                };
                layer.set(x, y, value);
            }
        }
    }
    // The water render bytes that follow the coast (depth, coast distance),
    // every cell: this runs for new islands and when asked to re-derive.
    water::derive(g, heights(field), None, window);
}

/// The editor heights as the water rules read them.
pub(super) fn heights(field: &Heightfield) -> water::Heights<'_> {
    water::Heights {
        data: &field.heights,
        width: field.grid.width,
        spacing: field.grid.spacing,
    }
}

/// The four map images every shipped island carries, as `.png` and
/// `_0.dds`: ground color with hill shading and transparent sea. One render
/// serves all four.
// TODO(alex): shipped DDS are BC7 and `activemapimage` is cropped to the
// active area; these are uncompressed RGBA8 at 512², unverified in game.
pub fn map_images(ground: Option<&Texture>, field: &Heightfield) -> Vec<(String, Vec<u8>)> {
    const SIDE: usize = 512;
    let size = field.grid.size()[0];
    let mut rgba = vec![0u8; SIDE * SIDE * 4];
    for y in 0..SIDE {
        for x in 0..SIDE {
            let (u, v) = (
                (x as f32 + 0.5) / SIDE as f32,
                (y as f32 + 0.5) / SIDE as f32,
            );
            let (wx, wz) = (u * size, v * size);
            let h = field.height([wx, wz]);
            let step = size / SIDE as f32;
            let slope = field.height([wx + step, wz + step]) - field.height([wx - step, wz - step]);
            let shade = (1. + slope * 0.6).clamp(0.6, 1.3);
            let base = ground.map_or([150, 140, 110, 255], |t| t.sample([u, v]));
            let i = (y * SIDE + x) * 4;
            for c in 0..3 {
                rgba[i + c] = (base[c] as f32 * shade).clamp(0., 255.) as u8;
            }
            rgba[i + 3] = if h > 0. { 255 } else { 0 };
        }
    }
    let mut png = Vec::new();
    image::RgbaImage::from_raw(SIDE as u32, SIDE as u32, rgba.clone())
        .expect("buffer matches the image size")
        .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
        .expect("PNG encodes into memory");
    // The game's DDS map images are stored bottom row first.
    let row = SIDE * 4;
    let flipped: Vec<u8> = rgba.chunks(row).rev().flatten().copied().collect();
    let dds = rgba_dds(SIDE, flipped);
    [
        "activemapimage",
        "gamemapimage",
        "mapimage",
        "provincemapimage",
    ]
    .into_iter()
    .flat_map(|name| {
        [
            (format!("{name}.png"), png.clone()),
            (format!("{name}_0.dds"), dds.clone()),
        ]
    })
    .collect()
}

/// The starting land a wizard choice makes: the side of its bounding square
/// in metres (what the size class follows) and its highest point above sea
/// level. `None` for open sea. Sampled on a 128 grid, as close as the class
/// thresholds need.
pub fn start_relief(start: Start, size: f32) -> Option<(u32, f32)> {
    const N: usize = 128;
    let step = size / N as f32;
    let (mut lo, mut hi, mut top) = ([usize::MAX; 2], [0usize; 2], f32::MIN);
    for z in 0..N {
        for x in 0..N {
            let y = start_height(
                (x as f32 + 0.5) * step,
                (z as f32 + 0.5) * step,
                size,
                start,
            );
            if y > 0. {
                lo = [lo[0].min(x), lo[1].min(z)];
                hi = [hi[0].max(x + 1), hi[1].max(z + 1)];
                top = top.max(y);
            }
        }
    }
    (hi[0] > 0).then(|| {
        let side = (hi[0] - lo[0]).max(hi[1] - lo[1]) as f32 * step;
        (side.round() as u32, top)
    })
}

/// A DX10 DDS of square RGBA8 with a full box-filtered mip chain.
fn rgba_dds(side: usize, top: Vec<u8>) -> Vec<u8> {
    let mut mips = vec![top];
    while mips.last().map_or(0, |m| m.len()) > 4 {
        let prev = mips.last().unwrap();
        let s = (prev.len() / 4).isqrt();
        let h = s / 2;
        let mut next = vec![0u8; h * h * 4];
        for y in 0..h {
            for x in 0..h {
                for c in 0..4 {
                    let at =
                        |dx: usize, dy: usize| prev[((2 * y + dy) * s + 2 * x + dx) * 4 + c] as u32;
                    next[(y * h + x) * 4 + c] =
                        ((at(0, 0) + at(1, 0) + at(0, 1) + at(1, 1) + 2) / 4) as u8;
                }
            }
        }
        mips.push(next);
    }
    let mut out = Vec::new();
    let u32s = |out: &mut Vec<u8>, v: &[u32]| v.iter().for_each(|x| out.extend(x.to_le_bytes()));
    out.extend(b"DDS ");
    // size, flags (caps|height|width|pixelformat|mipcount|linearsize),
    // height, width, pitch, depth, mips, 11 reserved.
    u32s(
        &mut out,
        &[
            124,
            0x000A_1007,
            side as u32,
            side as u32,
            (side * side * 4) as u32,
            0,
        ],
    );
    u32s(&mut out, &[mips.len() as u32]);
    u32s(&mut out, &[0; 11]);
    // Pixel format: size, FOURCC flag, "DX10", 5 unused.
    u32s(&mut out, &[32, 4]);
    out.extend(b"DX10");
    u32s(&mut out, &[0; 5]);
    // caps (texture|mipmap|complex), caps2..4, reserved.
    u32s(&mut out, &[0x0040_1008, 0, 0, 0, 0]);
    // DX10: DXGI_FORMAT_R8G8B8A8_UNORM, 2D, misc, array size 1, alpha mode.
    u32s(&mut out, &[28, 3, 0, 1, 0]);
    for m in &mips {
        out.extend(m);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{
        Grids, IslandEditor, Kind, Spec, Start, file_name, island_path, rgba_dds, shape,
        start_preview, start_relief,
    };
    use crate::island::test_support::{
        PIRATE, assets, harbour_library, install, layer, new_island, objects, saved, staged,
    };
    use anno_formats::{adf::Adf, game::Game, grids::Layer, island, objects::Objects, rda};
    use gpui_editor_kit::{
        heightfield::{Brush, Grid, Heightfield},
        texture::Dds,
    };
    use std::sync::Arc;

    const OLIVE: &str =
        "data/base/graphics/roman/environment/props/vegetation/trees/olive_trees/olive_tree_07.cfg";

    /// The object store of shipped island `path`, to copy objects from.
    fn shipped_objects(install: &anno_formats::vfs::Install, path: &str) -> Objects {
        let a = rda::Archive::from_bytes(install.read(path).unwrap(), path).unwrap();
        Objects::read(&Adf::decode(&a.read_path("gamedata.data").unwrap()).unwrap()).unwrap()
    }

    /// Cells set in `l`.
    fn set_cells(l: &Layer) -> usize {
        (0..l.width * l.height)
            .filter(|&k| l.get(k % l.width, k / l.width) != 0)
            .count()
    }

    /// Cells where `a` and `b` differ.
    fn differ(a: &Layer, b: &Layer) -> usize {
        (0..a.height)
            .flat_map(|y| (0..a.width).map(move |x| (x, y)))
            .filter(|&(x, y)| a.get(x, y) != b.get(x, y))
            .count()
    }

    // A start's relief: open sea has no land, the plateau has land in the
    // middle and sea at the edge, hills keep its outline and rise above it,
    // and the wizard's preview draws the same.
    #[test]
    fn starts_put_land_in_the_middle_and_sea_at_the_edge() {
        assert_eq!(start_relief(Start::Sea, 256.), None);
        let (plateau_side, plateau_top) = start_relief(Start::Plateau, 256.).unwrap();
        let (hills_side, hills_top) = start_relief(Start::Hills, 256.).unwrap();
        assert!(plateau_side > 0 && plateau_side <= 256);
        assert_eq!(plateau_side, hills_side, "hills keep the plateau’s outline");
        assert!(hills_top > plateau_top);

        let grid = Grid {
            width: 129,
            depth: 129,
            spacing: 2.,
            origin: [0., 0.],
        };
        let mut field = Heightfield::new(grid, vec![0.; 129 * 129]);
        shape(&mut field, Start::Plateau);
        assert!(field.height([128., 128.]) > 3.);
        assert!(field.height([2., 2.]) < -7.);

        let side = 64;
        let px = |img: &[u8], x: usize, z: usize| {
            let i = (z * side + x) * 4;
            [img[i], img[i + 1], img[i + 2]]
        };
        let plateau = start_preview(Start::Plateau, 256., side as u32);
        let [r, g, b] = px(&plateau, 32, 32);
        assert!(g > r && g > b, "the middle is grass: {r} {g} {b}");
        let [r, _, b] = px(&plateau, 0, 0);
        assert!(b > r, "the corner is sea");
        let sea = start_preview(Start::Sea, 256., side as u32);
        let [r, _, b] = px(&sea, 32, 32);
        assert!(b > r, "empty sea has no land");
        let hills = start_preview(Start::Hills, 256., side as u32);
        assert_ne!(hills, plateau, "hills read apart from the plateau");
    }

    // Needs the game: the scatter set toggles models (registering ones the
    // island never used), and tool-bar sliders map their track onto the
    // field's range, logarithmic where it matters.
    #[test]
    fn scatter_sets_toggle_and_sliders_span_their_range() {
        use crate::island::Field;
        let Some(install) = install() else {
            return;
        };
        let mut editor = new_island(&install, "roman", 256, Start::Plateau, "Scatter Test");
        let meshes = editor.doc.meshes.len();
        assert!(editor.toggle_scatter(OLIVE));
        assert!(editor.in_scatter(OLIVE));
        assert_eq!(editor.doc.meshes.len(), meshes + 1, "new model registered");
        assert!(!editor.toggle_scatter(OLIVE));
        assert!(!editor.in_scatter(OLIVE) && editor.scatter.files.is_empty());
        assert!(editor.toggle_scatter(OLIVE));
        assert_eq!(editor.doc.meshes.len(), meshes + 1, "registered once");

        // Radius is logarithmic: the middle of the track is the geometric
        // middle of 1..128 m, about 11 m; the ends are the ends.
        editor.set_field_fraction(Field::Radius, 0.5);
        assert!((editor.brush.radius - 128f32.sqrt()).abs() < 0.01);
        editor.set_field_fraction(Field::Radius, 1.);
        assert_eq!(editor.brush.radius, 128.);
        for f in [0., 0.3, 0.8] {
            editor.set_field_fraction(Field::Inner, f);
            let back = editor.field_fraction(Field::Inner).unwrap();
            assert!((back - f).abs() < 0.02, "{f} -> {back}");
        }
    }

    // Needs the game: a new island is grass in both provinces, and the next
    // ground bake rebuilds the splat, so the viewport shows the island's own
    // ground instead of the template island's. Saved, its UsedMaterialBits
    // cover every material it paints: the game bakes no material whose bit
    // is missing; new islands kept their template's bits and drew white
    // ground.
    #[test]
    fn new_islands_are_grass_and_mark_every_painted_material_as_used() {
        let Some(install) = install() else {
            return;
        };
        for region in ["roman", "celtic"] {
            let mut editor = new_island(&install, region, 256, Start::Plateau, "Ground Test");
            let c = editor.doc.field.grid.size()[0] / 2.;
            let id = editor.doc.materials.as_ref().unwrap().dominant([c, c]);
            let palette = editor.palette.clone().unwrap();
            let file = &palette.materials[&id].file;
            assert!(file.contains("grass_01"), "{region}: base is {file}");
            let (_, splat) = editor.bake_job().unwrap()().unwrap();
            assert!(splat.is_some(), "{region}: the splat is rebuilt");

            // Paint a few materials in patches across the island.
            let brush = Brush {
                radius: 12.,
                strength: 1.,
                ..Default::default()
            };
            let m = editor.doc.materials.as_mut().unwrap();
            for (k, &id) in palette.materials.keys().take(4).enumerate() {
                m.paint([60. + 40. * k as f32, 128.], &brush, id);
            }
            let (_config, project) = staged(&install, &mut editor);
            let (base, name) = editor.path.rsplit_once('/').unwrap();
            let stem = name.trim_end_matches(".a7m");
            let map = |name: &str| {
                let key = format!("{base}/_gamedata/{stem}/materialmaps/0/{name}.dds");
                island::material_words(&project.files[&key]).unwrap()
            };
            let (short, all) = saved(&project, &editor.path)
                .used_bits_shortfall(&map("matid"), &map("matalpha"))
                .expect("bits stored");
            assert_eq!(
                short, 0,
                "{region}: {short} of {all} words lack a painted material"
            );
        }
    }

    // Needs the game: a new island whose coast was reshaped after creation
    // saves water render bytes that follow the new coast, not the template's
    // or the starting plateau's: depth from the heights, the coast ramp one
    // cell off the new shore, and no template shore paint.
    #[test]
    fn saved_new_islands_ring_their_own_coast_with_shore_water() {
        use anno_formats::water::depth_byte;
        let Some(install) = install() else {
            return;
        };
        let mut editor = new_island(&install, "roman", 256, Start::Plateau, "Shore Test");
        // Sculpt: the round plateau becomes a 4 m high square in the
        // north-west quarter, 2 m cell edges on whole cells.
        let g = editor.doc.field.grid;
        let heights = &mut Arc::make_mut(&mut editor.doc.field).heights;
        for z in 0..g.depth {
            for x in 0..g.width {
                let [wx, wz] = g.world(x, z);
                let land = (40. ..120.).contains(&wx) && (40. ..120.).contains(&wz);
                heights[z * g.width + x] = if land { 4. } else { -8. };
            }
        }
        let (_config, project) = staged(&install, &mut editor);
        let grids = Grids::read(&saved(&project, &editor.path)).unwrap();
        let (infos, flow) = (
            grids.layer(Kind::WaterInfos).unwrap(),
            grids.layer(Kind::WaterFlow).unwrap(),
        );
        assert_eq!(infos.cell_metres, 2.);
        for y in 0..infos.height {
            for x in 0..infos.width {
                let (wx, wz) = (x as f32 * 2. + 1., y as f32 * 2. + 1.);
                let land = |v: f32| (40. ..120.).contains(&v);
                let h = if land(wx) && land(wz) { 4. } else { -8. };
                let word = infos.get(x, y);
                assert_eq!(word & 0xff, depth_byte(h) as u32, "depth at ({x}, {y})");
                assert_eq!(word >> 8, 0xff, "shore paint at ({x}, {y})");
                // Squared cells to the square of land cells 20..60.
                let off = |c: usize| 20i32.saturating_sub(c as i32).max(c as i32 - 59).max(0);
                let d2 = (off(x).pow(2) + off(y).pow(2)) as u32;
                let coast = (flow.get(x, y) >> 16) & 0xff;
                assert_eq!(coast, (d2 * 6).min(255), "coast at ({x}, {y})");
            }
        }
        // The ring along the new west shore and none along the old plateau
        // edge (east of the square).
        assert_eq!((flow.get(19, 40) >> 16) & 0xff, 6);
        assert_eq!((flow.get(100, 64) >> 16) & 0xff, 255);
    }

    // Needs the game: a plateau island of a shipped size saves with every
    // terrain tile, normal page, tint tile and the minimap under its own
    // path, and reads back as land in the middle.
    #[test]
    fn new_island_saves_every_file_under_its_own_path() {
        let Some(install) = install() else {
            return;
        };
        let skeleton = (super::skeletons(&install, None).into_iter())
            .find(|s| s.region == "roman")
            .unwrap();
        let spec = Spec {
            name: "Test Isle".into(),
            skeleton: skeleton.clone(),
            start: Start::Plateau,
            listing: crate::project::Listing {
                kind: "Normal;Starter".into(),
                ..Default::default()
            },
        };
        let mut editor = IslandEditor::create(&install, &spec).unwrap();
        assert!(editor.dirty(), "a new island must be saveable");
        assert!(editor.doc.props.is_empty() && editor.doc.objects.is_empty());
        let (_config, mut project) = staged(&install, &mut editor);

        let path = island_path(Game::Anno117, "roman", "Test Isle");
        let base = crate::island::island_data_dir(&path).unwrap();
        let count = |dir: &str| {
            let dir = format!("{base}/{dir}/");
            project.files.keys().filter(|k| k.starts_with(&dir)).count()
        };
        let old = crate::island::island_data_dir(&skeleton.path).unwrap();
        for dir in ["meshes", "normalmaps"] {
            let shipped = install.search(&format!("{old}/{dir}/"), 100_000).len();
            assert!(
                count(dir) >= shipped,
                "{dir}: {} written, skeleton ships {shipped}",
                count(dir)
            );
        }
        assert!(count("tintmaps") > 0 && count("materialmaps") > 0);
        for name in [
            "activemapimage",
            "gamemapimage",
            "mapimage",
            "provincemapimage",
        ] {
            for ext in [".png", "_0.dds"] {
                let key = format!("{base}/{name}{ext}");
                assert!(project.files.contains_key(&key), "missing {key}");
            }
        }
        let stem = path.trim_end_matches(".a7m");
        let info =
            anno_formats::a7minfo::A7mInfo::parse(&project.files[&format!("{stem}.a7minfo")])
                .unwrap();
        assert_eq!(info.map_size, skeleton.world);
        assert!(
            info.active_rect[2] > info.active_rect[0],
            "empty active rect"
        );
        assert!(project.files.contains_key(&format!("{stem}.a7me")));
        let entry = &project.islands[0];
        assert_eq!(
            (entry.path.as_str(), entry.region.as_str()),
            (path.as_str(), "Roman")
        );
        // The written mod folder registers it with the pool.
        let folder = project.write().unwrap();
        let assets =
            std::fs::read_to_string(folder.join("data/base/config/export/assets.xml")).unwrap();
        assert!(assets.contains(&format!("<FilePath>{path}</FilePath>")));
        let island = saved(&project, &path);
        let w = island.world_size()[0] as f32;
        let h = island.heights();
        assert!(h.data[h.height / 2 * h.width + h.width / 2] > 3.);
        assert!(island.props().is_empty());
        let water = Grids::read(&island).unwrap();
        let water = water.layer(Kind::Water).unwrap();
        assert_eq!(super::grids::value_at(water, w / 2., w / 2.), 1);
        assert_eq!(super::grids::value_at(water, 1., 1.), 0);

        // Reopening through the mounted mod folder gives the same island,
        // which saves as a new one again with its settings.
        assert!(entry.listing.is_start(), "the wizard’s role is registered");
        assert!(!install.exists(&path));
        install.mount_mod(&folder).unwrap();
        assert!(install.exists(&path));
        assert!(
            anno_formats::tint::TintMap::read(
                &install,
                &base,
                super::tint::WORK_LEVEL,
                skeleton.world
            )
            .is_ok(),
            "tint tiles list through the mounted folder"
        );
        let mut back = IslandEditor::load(&install, &path).unwrap();
        back.adopt(entry);
        let (a, b) = (&back.doc.field.heights, &editor.doc.field.heights);
        assert_eq!(a.len(), b.len());
        let worst = (a.iter().zip(b))
            .map(|(x, y)| (x - y).abs())
            .fold(0f32, f32::max);
        assert!(
            worst < 0.05,
            "heights survive a save, up to the file’s quantization: {worst} m"
        );
        assert_eq!(back.land_class(), editor.land_class());
        assert_eq!(back.listing, entry.listing);
        back.set_listing(|l| l.in_pool = false);
        assert!(back.dirty(), "a settings change is saved with Ctrl S");
        back.stage(&install, &mut project).unwrap();
        assert!(!back.dirty());
        assert!(!project.islands[0].listing.in_pool);
        assert_eq!(project.islands.len(), 1, "same entry, same GUID");
    }

    // Needs the game: a harbour spot clicked on a new island saves as a
    // coastline curve with a construction-area pad into the sea, a player
    // marker, and no land outside an area — what shipped islands have.
    #[test]
    fn harbour_spot_makes_a_new_island_settleable() {
        use super::super::{Item, Tool, harbour};
        let Some(install) = install() else {
            return;
        };
        let assets = assets(&install);
        let mut editor = new_island(&install, "roman", 320, Start::Plateau, "Harbour Test");
        editor.tool = Tool::Harbour;
        assert!(editor.needs_harbour_library());
        editor.harbour = Some(harbour_library(&install, &assets, &editor));
        // Walk in from the north edge to the first land: the shore.
        let c = editor.doc.field.grid.size()[0] / 2.;
        let z = (0..320)
            .map(|z| z as f32)
            .find(|&z| editor.doc.field.height([c, z]) > 0.)
            .unwrap();
        editor.harbour_at([c, 0., z + 3.], false);
        let msg = editor.message.take().unwrap();
        assert!(!msg.1, "{}", msg.0);

        // Auto placement over the whole island adds more harbours, kept
        // apart, as one undo step.
        let coasts = |e: &IslandEditor| {
            let guids = e.coast_guids();
            e.doc
                .objects
                .iter()
                .filter(|o| guids.contains(&o.guid))
                .count()
        };
        assert_eq!(coasts(&editor), 1);
        editor.auto_count = 3;
        let size = editor.doc.field.grid.size()[0];
        editor.auto_place(harbour::Region::spanning([0., 0.], [size, size]));
        let msg = editor.message.take().unwrap();
        assert!(!msg.1, "{}", msg.0);
        let after = coasts(&editor);
        assert!(after > 1 && after <= 4, "{after} harbours");
        editor.undo(false);
        assert_eq!(coasts(&editor), 1, "one undo step removes the batch");
        editor.undo(true);
        assert_eq!(coasts(&editor), after);

        // Moving a harbour moves its whole curve.
        let guids = editor.coast_guids();
        let id = (editor.doc.objects.iter())
            .find(|o| guids.contains(&o.guid))
            .unwrap()
            .id;
        let item = Item::Object(id);
        let mut m = editor.doc.matrix(item).unwrap();
        m[12] += 5.;
        editor.doc.set_matrix(item, m);
        let o = editor.doc.objects.iter().find(|o| o.id == id).unwrap();
        assert_eq!(o.path.as_ref().unwrap()[0].p[0], o.position[0]);

        // A random and an always-active mine, and a start coast on the south
        // shore.
        let field = editor.doc.field.clone();
        let land = |x: f32, z: f32| [x, field.height([x, z]), z];
        editor.resource = harbour::Resource::Mine;
        editor.fixed_slots = false;
        assert!(editor.push_slot(land(c, c)));
        editor.fixed_slots = true;
        assert!(editor.push_slot(land(c + 40., c)));
        editor.start_coast = true;
        let south = (0..320)
            .map(|z| 319. - z as f32)
            .find(|&z| editor.doc.field.height([c, z]) > 0.)
            .unwrap();
        editor.harbour_at([c, 0., south - 3.], false);

        let (_config, project) = staged(&install, &mut editor);
        let info = anno_formats::a7minfo::A7mInfo::parse(
            &project.files[&editor.path.replace(".a7m", ".a7minfo")],
        )
        .unwrap();
        let lib = editor.harbour.clone().unwrap();
        let (random, fixed) = (
            lib.guid(harbour::Piece::MineSlot).unwrap(),
            lib.guid(harbour::Piece::FixedMineSlot).unwrap(),
        );
        assert_ne!(random, fixed);
        let listed: Vec<(u32, usize)> = (info.random_slots().iter())
            .map(|g| (g.guid, g.objects.len()))
            .collect();
        assert_eq!(listed, [(random, 1), (fixed, 1)], "slots listed by GUID");
        let slot = editor.doc.objects.iter().find(|o| o.guid == fixed).unwrap();
        assert_eq!(info.random_slots()[1].objects[0].object_id, slot.id);
        let dir = info.start_coast_direction.expect("start coast direction");
        // South of the centre: +z, about a quarter turn.
        assert!(
            (dir - std::f32::consts::FRAC_PI_2).abs() < 0.6,
            "direction {dir}"
        );
        let island = saved(&project, &editor.path);
        let objects = objects(&island);
        let coast = (objects.iter())
            .find(|o| Some(o.guid) == lib.guid(harbour::Piece::Coast))
            .expect("the coastline is saved");
        let path = coast.path.as_ref().unwrap();
        assert!(path.len() >= 3);
        assert_eq!(coast.position, path[0].p);
        assert_eq!(coast.area, harbour::CONSTRUCTION as u16);
        // Each work area's path runs from itself to its own slot, with the
        // shipped tangents, after a save and reload.
        let work_guid = lib.guid(harbour::Piece::MineWorkArea).unwrap();
        let works: Vec<_> = objects.iter().filter(|o| o.guid == work_guid).collect();
        assert_eq!(works.len(), 2);
        for w in works {
            let path = w.path.as_ref().expect("work area path");
            assert_eq!(path.len(), 2);
            assert_eq!(path[0].p, w.position);
            assert!(
                (objects.iter())
                    .any(|s| (s.guid == random || s.guid == fixed) && s.position == path[1].p),
                "work area path ends at a slot: {:?}",
                path[1].p
            );
            let i = path[0].i.expect("tangent");
            assert!((i[0] - (path[0].p[0] - path[1].p[0]) / 6.).abs() < 1e-4);
            assert!(path[0].d.is_none(), "work areas have no coast direction");
        }
        assert!(
            (objects.iter()).any(|o| Some(o.guid) == lib.guid(harbour::Piece::Marker)),
            "a player marker is added on save"
        );
        let grids = Grids::read(&island).unwrap();
        let (ground, areas) = (
            grids.layer(Kind::Water).unwrap(),
            grids.layer(Kind::AreaIds).unwrap(),
        );
        let (mut pad, mut homeless) = (0, 0);
        for y in 0..areas.height {
            for x in 0..areas.width {
                match (ground.get(x, y), areas.get(x, y)) {
                    (0, a) if a == harbour::CONSTRUCTION => pad += 1,
                    (1, 0) => homeless += 1,
                    _ => {}
                }
            }
        }
        assert!(pad > 500, "pad cells: {pad}");
        assert_eq!(homeless, 0, "land outside every area");
    }

    // Needs the game: a marsh slot on a new Celtic island saves without a
    // work area and is listed in the .a7minfo like shipped ones; a mine slot
    // given a deposit puts it in the test map's fixed slot.
    #[test]
    fn marsh_slots_and_test_map_resources_save() {
        use super::super::{Item, harbour};
        use anno_formats::{
            a7minfo::A7mInfo,
            template::{MapTemplate, SlotKind},
        };
        let Some(install) = install() else {
            return;
        };
        let assets = assets(&install);
        let mut editor = new_island(&install, "celtic", 320, Start::Plateau, "Marsh Test");
        let lib = harbour_library(&install, &assets, &editor);
        editor.harbour = Some(lib.clone());
        editor.assets = Some(assets.clone());
        let c = editor.doc.field.grid.size()[0] / 2.;
        let field = editor.doc.field.clone();
        let land = |x: f32, z: f32| [x, field.height([x, z]), z];

        editor.resource = harbour::Resource::Marsh;
        editor.fixed_slots = true; // ignored: marsh slots have no fixed kind
        assert!(editor.push_slot(land(c - 40., c)));
        let marsh = lib.guid(harbour::Piece::MarshSlot).unwrap();
        assert_eq!(marsh, 5282);
        let Some((_, kind)) = editor.selected_slot() else {
            panic!("the new slot is selected")
        };
        assert_eq!(kind, harbour::Resource::Marsh);
        editor.set_slot_resource(Some(2219)); // birds

        editor.resource = harbour::Resource::Mine;
        editor.fixed_slots = false;
        assert!(editor.push_slot(land(c + 20., c)));
        let (mine_id, _) = editor.selected_slot().unwrap();
        editor.set_slot_resource(Some(4063)); // copper
        // Clicking the slot again selects it rather than stacking another.
        editor.selection.clear();
        editor.slot_at(land(c + 21., c), false);
        assert_eq!(
            editor.selection.iter().copied().collect::<Vec<_>>(),
            [Item::Object(mine_id)]
        );
        assert_eq!(editor.slot_labels().len(), 2);

        let (_config, mut project) = staged(&install, &mut editor);
        let info =
            A7mInfo::parse(&project.files[&editor.path.replace(".a7m", ".a7minfo")]).unwrap();
        let listed: Vec<(u32, usize)> = (info.random_slots().iter())
            .map(|g| (g.guid, g.objects.len()))
            .collect();
        let mine = lib.guid(harbour::Piece::MineSlot).unwrap();
        let mut want = vec![(mine, 1), (marsh, 1)];
        want.sort();
        assert_eq!(listed, want, "both slots listed, ascending GUID");

        let objects = objects(&saved(&project, &editor.path));
        assert_eq!(objects.iter().filter(|o| o.guid == marsh).count(), 1);
        let work = lib.guid(harbour::Piece::MineWorkArea).unwrap();
        assert_eq!(
            objects.iter().filter(|o| o.guid == work).count(),
            1,
            "only the mine has a work area"
        );
        let entry = project.islands[0].clone();
        assert_eq!(entry.listing.slot_resources.get(&mine_id), Some(&4063));
        assert_eq!(entry.listing.slot_resources.len(), 2);

        // The test map's fixed slot carries the island's fertility list
        // with both deposits, and forces both slots on.
        let plans = crate::playtest::plan(&install, Some(&assets), &project).unwrap();
        crate::playtest::apply(plans, &mut project);
        let folder = project.write().unwrap();
        let mut fixed = 0;
        for m in &project.maps {
            let t = MapTemplate::parse(
                &std::fs::read(folder.join(m.path.replace(".a7t", ".a7tinfo"))).unwrap(),
            )
            .unwrap();
            for s in &t.slots {
                if !matches!(&s.kind, SlotKind::Fixed { path } if *path == editor.path) {
                    continue;
                }
                fixed += 1;
                assert!(s.fertilities.contains(&4063) && s.fertilities.contains(&2219));
                assert!(s.fertilities.windows(2).all(|w| w[0] < w[1]));
                assert!(s.fertilities.len() >= 4, "{:?}", s.fertilities);
                let mut forced: Vec<u64> = s.mine_slots.iter().map(|m| m.0).collect();
                forced.sort();
                let mut ids: Vec<u64> = entry.listing.slot_resources.keys().copied().collect();
                ids.sort();
                assert_eq!(forced, ids);
                assert!(s.mine_slots.iter().all(|m| m.1));
            }
        }
        assert!(fixed > 0, "the island is on a test map");

        // Deleting the slot drops its resource on the next save.
        editor.selection = std::collections::BTreeSet::from([Item::Object(mine_id)]);
        editor.delete_selected();
        editor.stage(&install, &mut project).unwrap();
        assert!(!(project.islands[0].listing.slot_resources).contains_key(&mine_id));
    }

    // Needs the game and a Vulkan device: a moved prop is on screen within
    // a few frames. The scene used to re-key every terrain tile on each
    // refresh, so a drag waited ~100 frames for re-uploads and only showed
    // the item once released.
    #[test]
    fn moved_items_show_within_a_few_frames() {
        use crate::island::Item;
        use gpui_editor_kit::gpu::Offscreen;
        let Some(install) = install().map(Arc::new) else {
            return;
        };
        let mut editor = new_island(&install, "roman", 256, Start::Plateau, "Move Test");
        let c = editor.doc.field.grid.size()[0] / 2.;
        editor.choose_prop(OLIVE);
        editor.place_at([c, editor.doc.field.height([c, c]), c]);
        while let Some(job) = editor.models_job(install.clone()) {
            editor.models_loaded(job());
        }
        editor.refresh_scene();
        let mut target = Offscreen::new(64, 64).unwrap();
        let renderer = editor.renderer.clone();
        target
            .render(&renderer, editor.views[0].camera, editor.extent())
            .unwrap();

        let item = Item::Prop(editor.doc.props[0].id);
        let start = editor.doc.matrix(item).unwrap();
        for step in 1..=3 {
            let mut m = start;
            m[12] += step as f32;
            editor.doc.set_matrix(item, m);
            editor.moves += 1;
            editor.scene_stale = true;
            editor.refresh_scene();
            // One frame per changed batch at worst (the upload budget admits
            // one item per frame under load); the tree has a few pieces.
            let mut frames = 0;
            while renderer.pending() {
                target.frame(&renderer).unwrap();
                frames += 1;
                assert!(frames <= 5, "drag step {step} still uploading");
            }
        }
    }

    // Needs the game: library props and objects new to an island place
    // and save; new models join the island's prop file list.
    #[test]
    fn library_props_and_objects_place_and_save() {
        use crate::island::{Item, catalog::Catalog};
        let Some(install) = install().map(Arc::new) else {
            return;
        };
        let catalog = Catalog::build(&install).unwrap();
        let mut editor = new_island(&install, "roman", 256, Start::Plateau, "Library Test");
        let c = editor.doc.field.grid.size()[0] / 2.;
        let land = [c, editor.doc.field.height([c, c]), c];
        // A prop model the blank island never had.
        let model = &catalog.props[0].path;
        assert!(!editor.doc.meshes.iter().any(|m| m == model));
        editor.choose_prop(model);
        editor.place_at(land);
        assert_eq!(editor.doc.props.len(), 1);
        // A waterfall object, copied from its shipped island.
        let waterfall = catalog.objects.iter().find(|o| o.guid == 51204).unwrap();
        let store = shipped_objects(&install, &waterfall.island);
        let i = store.objects.iter().position(|o| o.guid == 51204).unwrap();
        editor.choose_object(store.template(i), None);
        editor.place_at([c + 10., land[1], c]);

        // The new model loads and draws like shipped ones.
        while let Some(job) = editor.models_job(install.clone()) {
            editor.models_loaded(job());
        }
        editor.refresh_scene();
        let key = model.replace('\\', "/");
        let placed = editor.doc.props.last().map(|p| Item::Prop(p.id));
        assert!(editor.placements.has(&key), "{key} loaded");
        assert!(editor.placements.item.contains(&placed), "{key} drawn");

        let (_config, project) = staged(&install, &mut editor);
        let island = saved(&project, &editor.path);
        let prop = &island.props()[0];
        assert_eq!(&island.prop_files()[prop.file as usize], model);
        assert!(
            objects(&island).iter().any(|o| o.guid == 51204),
            "the waterfall is saved"
        );
    }

    // Needs the game: on a new island, a waterfall gets a waterfall sound
    // spot and a decoration on buildable land gets blocker cubes over it;
    // reruns replace, both save and read back, and undo takes them away.
    #[test]
    fn sound_spots_and_blockers_follow_placed_objects() {
        use crate::island::{blockers, harbour, sound};
        let Some(install) = install() else {
            return;
        };
        let mut editor = new_island(&install, "roman", 256, Start::Plateau, "Sound Test");
        editor.assets = Some(assets(&install));
        // A waterfall and a bench, copied from a shipped island.
        let store = shipped_objects(&install, PIRATE);
        let waterfall = harbour::find_records(&install, &[51204]).unwrap()[&51204].clone();
        let bench = (store.objects.iter())
            .position(|o| {
                (o.config.as_deref())
                    .is_some_and(|c| c.contains("bench") && blockers::blocks(c, true))
            })
            .expect("pirate_01 has a bench");
        let bench_config = store.objects[bench].config.clone();
        let c = editor.doc.field.grid.size()[0] / 2.;
        let ground = |e: &IslandEditor, x: f32, z: f32| [x, e.doc.field.height([x, z]), z];
        editor.choose_object(waterfall, None);
        editor.place_at(ground(&editor, c - 20., c));
        editor.choose_object(store.template(bench), bench_config);
        let seat = ground(&editor, c + 20., c);
        editor.place_at(seat);
        let objects_before = editor.doc.objects.len();

        let editor_spots = |e: &mut IslandEditor| {
            e.spot_records = Some(Arc::new(
                harbour::find_records(
                    &install,
                    &[sound::GUIDS.as_slice(), &[blockers::BLOCKER]].concat(),
                )
                .unwrap(),
            ));
            e.auto_sound_spots();
            e.auto_blockers(&install);
        };
        editor_spots(&mut editor);
        let falls: Vec<_> = (editor.doc.objects.iter())
            .filter(|o| sound::GUIDS[..3].contains(&o.guid))
            .collect();
        assert_eq!(falls.len(), 1, "one waterfall, one spot");
        let d = (falls[0].position[0] - (c - 20.)).hypot(falls[0].position[2] - c);
        assert!(d < 1., "spot {d} m from the waterfall");
        let cubes: Vec<_> = (editor.doc.objects.iter())
            .filter(|o| o.guid == blockers::BLOCKER)
            .map(|o| {
                (
                    o.position,
                    blockers::SIZES[o.variation as usize] as f32 / 2.,
                )
            })
            .collect();
        assert!(
            cubes
                .iter()
                .any(|(p, h)| (p[0] - seat[0]).abs() <= *h && (p[2] - seat[2]).abs() <= *h),
            "no blocker over the bench at {seat:?}: {cubes:?}"
        );
        // A rerun replaces the generated ones instead of stacking more.
        let generated = editor.doc.objects.len() - objects_before;
        editor_spots(&mut editor);
        assert_eq!(editor.doc.objects.len() - objects_before, generated);

        let (_config, project) = staged(&install, &mut editor);
        let saved = objects(&saved(&project, &editor.path));
        let count = |guids: &[u32]| saved.iter().filter(|o| guids.contains(&o.guid)).count();
        assert_eq!(count(&sound::GUIDS[..3]), 1);
        assert_eq!(count(&[blockers::BLOCKER]), cubes.len());

        // Each auto run is one undo step: undoing both reruns keeps the
        // first run's objects, the third step takes its blockers, the
        // fourth its spots.
        editor.undo(false);
        editor.undo(false);
        assert_eq!(editor.doc.objects.len() - objects_before, generated);
        editor.undo(false);
        assert_eq!(
            editor.doc.objects.len() - objects_before,
            generated - cubes.len()
        );
        editor.undo(false);
        assert_eq!(editor.doc.objects.len(), objects_before);
    }

    // Needs the game: trees scattered on a new island become tree cells with
    // forest chunks, forest ambience and herds around them after "Auto-fill
    // gameplay grids", in one undo step, and the grids survive a save.
    #[test]
    fn scattered_trees_derive_gameplay_grids_and_save() {
        use crate::island::catalog::Catalog;
        use anno_formats::landscape;
        let Some(install) = install() else {
            return;
        };
        let catalog = Catalog::build(&install).unwrap();
        let mut editor = new_island(&install, "roman", 256, Start::Plateau, "Forest Test");
        let tree = (catalog.props.iter())
            .find(|p| landscape::is_harvestable_tree(install.game(), &p.path))
            .expect("a harvestable tree model")
            .path
            .clone();
        editor.choose_prop(&tree);
        let file = editor.doc.meshes.iter().position(|m| *m == tree).unwrap() as u32;
        editor.scatter.files = vec![file];
        // Shipped forests have a tree in about every fourth 1 m cell; the
        // default scatter density makes a grove that stays meadow.
        editor.scatter.spread.density = 30.;
        let c = editor.doc.field.grid.size()[0] / 2.;
        let mut brush = editor.brush;
        brush.radius = 30.;
        // A few dabs around the centre, like a short stroke.
        let mut placed = 0;
        for _ in 0..4 {
            for d in [[-20., 0.], [20., 0.], [0., -20.], [0., 20.], [0., 0.]] {
                placed += editor.scatter.fill(
                    &mut editor.doc,
                    [c + d[0], c + d[1]],
                    &brush,
                    &mut editor.next_id,
                );
            }
        }
        assert!(placed > 50, "{placed} trees scattered");

        let count = |e: &IslandEditor, kind| {
            (e.doc.grids.as_ref().unwrap())
                .layer(kind)
                .map_or(0, set_cells)
        };
        let before = (count(&editor, Kind::Trees), count(&editor, Kind::Herds));
        editor.history.checkpoint(&editor.doc);
        editor.derive_grids(None);
        let grids = editor.doc.grids.clone().unwrap();
        let (trees, chunks) = (
            grids.layer(Kind::Trees).unwrap(),
            grids.layer(Kind::ForestChunks).unwrap(),
        );
        // The plateau centre is flat buildable land: most trees get a cell
        // (several can share one).
        let cells = set_cells(trees);
        assert!(cells * 2 > placed, "{cells} tree cells for {placed} trees");
        for y in 0..trees.height {
            for x in 0..trees.width {
                assert_eq!(
                    trees.get(x, y) != 0,
                    chunks.get(x, y) != 0,
                    "chunk at ({x}, {y})"
                );
            }
        }
        let env = grids.layer(Kind::Environment).unwrap();
        let forest = (0..env.width * env.height)
            .filter(|&k| env.get(k % env.width, k / env.width) == 4)
            .count();
        assert!(forest > 10, "{forest} forest cells");
        assert!(count(&editor, Kind::Herds) > 1000);
        editor.undo(false);
        assert_eq!(
            (count(&editor, Kind::Trees), count(&editor, Kind::Herds)),
            before,
            "one undo step restores every derived grid"
        );
        editor.undo(true);

        let (_config, project) = staged(&install, &mut editor);
        let saved = Grids::read(&saved(&project, &editor.path)).unwrap();
        for kind in [
            Kind::Trees,
            Kind::ForestChunks,
            Kind::Environment,
            Kind::Herds,
        ] {
            assert_eq!(
                saved.layer(kind).unwrap().values,
                grids.layer(kind).unwrap().values,
                "{kind:?} after save"
            );
        }
    }

    // Needs the game: releasing a stroke derives the land grids around it
    // alone, exactly as deriving the whole island would. The stroke raises
    // the coast (water, active area, coast bytes) and roughens a slope
    // (area patches). Then on the editor: a released stroke's land job
    // lands what syncing on the UI thread would, and a sync that takes
    // over drops its result.
    #[test]
    fn stroke_release_derives_land_as_the_whole_island_would() {
        use super::{LandChanges, sync_land};
        let Some(install) = install() else {
            return;
        };
        let mut editor = new_island(&install, "roman", 256, Start::Hills, "Sync Test");
        // The first land along the middle row.
        let coast = |field: &Heightfield| {
            let (g, row) = (field.grid, field.grid.depth / 2);
            let x = (0..g.width).find(|&x| field.heights[row * g.width + x] > 0.);
            (row, x.unwrap())
        };
        let mut doc = editor.doc.clone();
        let g = doc.field.grid;
        // Raise and roughen around the coast.
        let (row, x0) = coast(&doc.field);
        let heights = &mut Arc::make_mut(&mut doc.field).heights;
        for z in row - 30..row + 30 {
            for x in x0 - 30..x0 + 30 {
                let bump = if (x + z) % 7 == 0 { 3. } else { 1.5 };
                heights[z * g.width + x] += bump;
            }
        }
        let mut whole = (**doc.grids.as_ref().unwrap()).clone();
        LandChanges::within(
            g,
            &doc.land_heights.heights,
            &doc.field.heights,
            &whole,
            &[],
            "Meadow",
            None,
            None,
        )
        .apply(&mut whole);
        assert!(sync_land(&mut doc, &[], "Meadow"));
        for kind in [Kind::Water, Kind::AreaIds] {
            let (was, now) = (layer(&editor.doc, kind), whole.layer(kind).unwrap());
            assert!(differ(was, now) > 0, "the stroke changes {kind:?}");
        }
        for kind in [
            Kind::Water,
            Kind::ActiveMap,
            Kind::Environment,
            Kind::WaterFlow,
            Kind::WaterInfos,
            Kind::AreaIds,
        ] {
            assert_eq!(
                differ(whole.layer(kind).unwrap(), layer(&doc, kind)),
                0,
                "{kind:?}"
            );
        }

        let raise = |editor: &mut IslandEditor, by: f32| {
            let g = editor.doc.field.grid;
            let (row, x0) = coast(&editor.doc.field);
            let heights = &mut Arc::make_mut(&mut editor.doc.field).heights;
            for z in row - 20..row + 20 {
                for x in x0 - 20..x0 + 20 {
                    heights[z * g.width + x] += by;
                }
            }
            editor.land_stale = true;
        };
        let layers = |g: &Grids| {
            [
                Kind::Water,
                Kind::ActiveMap,
                Kind::WaterInfos,
                Kind::AreaIds,
            ]
            .map(|k| g.layer(k).unwrap().clone())
        };
        raise(&mut editor, 2.);
        let mut synced = editor.doc.clone();
        let (coasts, environment) = (editor.coast_paths(), editor.land_environment());
        assert!(sync_land(&mut synced, &coasts, environment));
        let (job, work) = editor.land_job().unwrap();
        assert!(editor.land_job().is_none(), "one job at a time");
        editor.land_synced(job, work());
        let now = layers(editor.doc.grids.as_ref().unwrap());
        for (a, b) in now.iter().zip(&layers(synced.grids.as_ref().unwrap())) {
            assert_eq!(differ(a, b), 0, "{:?}", a.kind);
        }
        assert_eq!(editor.doc.land_heights.heights, editor.doc.field.heights);

        // A second stroke's job, overtaken by a sync (as a save does).
        raise(&mut editor, -4.);
        let (job, work) = editor.land_job().unwrap();
        editor.sync_land();
        let after = layers(editor.doc.grids.as_ref().unwrap());
        editor.land_synced(job, work());
        for (a, b) in layers(editor.doc.grids.as_ref().unwrap())
            .iter()
            .zip(&after)
        {
            assert_eq!(differ(a, b), 0, "late result dropped: {:?}", a.kind);
        }
        assert_eq!(editor.doc.land_heights.heights, editor.doc.field.heights);
    }

    // Needs the game: undo steps keep sculpted heights as patches, and
    // undo and redo walk back and forth through every state exactly,
    // sculpt and paint steps mixed, a new stroke after undos included.
    #[test]
    fn undo_steps_patch_heights_and_restore_them_exactly() {
        use super::super::{Drag, Stroke, Tool};
        let Some(install) = install() else {
            return;
        };
        let mut editor = new_island(&install, "roman", 256, Start::Hills, "Undo Heights Test");
        let stroke = |editor: &mut IslandEditor, tool: Tool, x: f32| {
            editor.tool = tool;
            let y = editor.doc.field.height([x, 128.]);
            let mut drag = Drag::Brush {
                stroke: Stroke::default(),
                before: Box::new(editor.doc.clone()),
                level: y,
                changed: false,
            };
            for i in 0..6 {
                editor.continue_brush(&mut drag, [x + i as f32, y, 128.], false);
            }
            editor.views[0].drag = Some(drag);
            editor.finish_drag(0);
            if let Some((job, work)) = editor.land_job() {
                editor.land_synced(job, work());
            }
        };
        let mut states = vec![editor.doc.field.heights.clone()];
        for (k, tool) in [
            Tool::Raise,
            Tool::Paint,
            Tool::Raise,
            Tool::Lower,
            Tool::Paint,
            Tool::Raise,
        ]
        .into_iter()
        .enumerate()
        {
            let revision = editor.history.revision();
            stroke(&mut editor, tool, 60. + k as f32 * 20.);
            // A stroke that changes nothing makes no step.
            if editor.history.revision() != revision {
                states.push(editor.doc.field.heights.clone());
            }
        }
        assert!(
            states.windows(2).filter(|w| w[0] != w[1]).count() >= 4,
            "strokes sculpt"
        );
        // Only patches stay in the history, each far smaller than the map.
        let full = editor.doc.field.heights.len();
        for snapshot in editor.history.past_mut() {
            assert!(snapshot.field.heights.is_empty(), "stored as a patch");
            let patch = snapshot.heights_patch.as_ref().unwrap();
            assert!(patch.values.len() * 8 < full);
        }
        for want in states.iter().rev().skip(1) {
            assert!(editor.undo(false));
            assert!(editor.doc.field.heights == *want, "undo");
        }
        for want in states.iter().skip(1) {
            assert!(editor.undo(true));
            assert!(editor.doc.field.heights == *want, "redo");
        }
        let n = states.len() - 1;
        editor.undo(false);
        editor.undo(false);
        stroke(&mut editor, Tool::Raise, 200.);
        assert!(editor.undo(false));
        assert!(
            editor.doc.field.heights == states[n - 2],
            "a new stroke after undos"
        );
        assert!(editor.undo(false));
        assert!(editor.doc.field.heights == states[n - 3]);
    }

    #[test]
    fn names_become_safe_paths() {
        assert_eq!(file_name(" My Isle-2! "), "my_isle_2_");
        assert_eq!(
            island_path(Game::Anno117, "roman", "Nova"),
            "data/base/provinces/roman/islands/custom/nova/nova.a7m"
        );
    }

    #[test]
    fn map_dds_decodes_with_every_mip() {
        let side = 8;
        let rgba: Vec<u8> = (0..side * side * 4).map(|i| (i % 251) as u8).collect();
        let dds = rgba_dds(side, rgba.clone());
        let header = Dds::parse(&dds).unwrap();
        let top = header.decode(&dds, 0).unwrap();
        assert_eq!((top.width, top.height), (8, 8));
        assert_eq!(top.rgba, rgba);
        assert_eq!(
            header.mip_for(1),
            3,
            "8, 4, 2, 1: the smallest mip is level 3"
        );
    }
}
