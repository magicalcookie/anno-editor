// Build blockers: invisible cubes that keep players from building over
// decoration. Shipped islands keep rocks off buildable land instead (97% of
// rock objects and props stand in the base area), and where designers left
// set pieces on buildable land they covered them with CampaignBuildBlocker
// (101128): axis-aligned 1–16 m cubes whose `.ifo` BuildBlocker is the
// cube's square. The editor does the same for rocks and decoration placed
// on buildable land.
use super::{IslandEditor, Item, harbour};
use anno_formats::{grids::Kind, model::Model, vfs::Install};
use gpui_editor_kit::math::{self, Mat4};
use std::collections::{HashMap, HashSet};

/// A cfg's `MeshCenter` and `MeshExtent`.
type Box3 = ([f32; 3], [f32; 3]);

/// `StaticBlockingObject` "CampaignBuildBlocker". `EditorBlocking` (92981,
/// Voada Wall Blocking) is a destructible palisade, not a footprint.
pub const BLOCKER: u32 = 101128;
/// Cube side in metres per `Variation`: the asset lists cube_1x1, 2x2, 4x4,
/// 8x8 and 16x16 in this order.
pub const SIZES: [u32; 5] = [1, 2, 4, 8, 16];
/// One model's footprint is clipped to this side; a mountain kit piece is
/// not a building site anyway and would cost thousands of cells.
const MAX_SIDE: f32 = 64.;
/// Bound on the cubes one run writes; a dense decorated island needs a few
/// hundred.
const MAX_BLOCKERS: usize = 4000;

/// Whether a model blocks building. Props only when solid (rock, cliff,
/// stone, ruin): buildings clear vegetation in game. Decoration objects
/// unless they are plants, decals or effects.
pub fn blocks(model: &str, decoration: bool) -> bool {
    let m = model.to_ascii_lowercase().replace('\\', "/");
    const SOFT: [&str; 11] = [
        "tree",
        "bush",
        "grass",
        "reed",
        "plant",
        "flower",
        "fern",
        "decal",
        "/effects/",
        "waterfall",
        "/placeholder/",
    ];
    const SOLID: [&str; 6] = ["rock", "cliff", "mountain", "stone", "boulder", "ruin"];
    let solid = SOLID.iter().any(|s| m.contains(s));
    if SOFT.iter().any(|s| m.contains(s)) || (m.contains("/vegetation/") && !solid) {
        return false;
    }
    decoration || solid
}

/// 1 m cells whose centre lies inside the model's ground footprint: the
/// cfg's `MeshCenter ± MeshExtent` box in x/z, placed with `matrix`. The
/// cell under the origin always counts, so small props still get a cube.
pub fn footprint(center: [f32; 3], extent: [f32; 3], matrix: &Mat4, cells: &mut HashSet<[i32; 2]>) {
    let e = [extent[0].min(MAX_SIDE / 2.), extent[2].min(MAX_SIDE / 2.)];
    let quad = [[-1., -1.], [1., -1.], [1., 1.], [-1., 1.]].map(|[sx, sz]| {
        let p = math::transform(
            matrix,
            [center[0] + sx * e[0], center[1], center[2] + sz * e[1]],
        );
        [p[0], p[2]]
    });
    let inside = |x: f32, z: f32| {
        let mut sign = 0.;
        for k in 0..4 {
            let (a, b) = (quad[k], quad[(k + 1) % 4]);
            let c = (b[0] - a[0]) * (z - a[1]) - (b[1] - a[1]) * (x - a[0]);
            if c != 0. {
                if sign != 0. && c.signum() != sign {
                    return false;
                }
                sign = c.signum();
            }
        }
        true
    };
    let lo = quad
        .iter()
        .fold([f32::MAX; 2], |m, p| [m[0].min(p[0]), m[1].min(p[1])]);
    let hi = quad
        .iter()
        .fold([f32::MIN; 2], |m, p| [m[0].max(p[0]), m[1].max(p[1])]);
    for z in lo[1].floor() as i32..=hi[1].floor() as i32 {
        for x in lo[0].floor() as i32..=hi[0].floor() as i32 {
            if inside(x as f32 + 0.5, z as f32 + 0.5) {
                cells.insert([x, z]);
            }
        }
    }
    cells.insert([matrix[12].floor() as i32, matrix[14].floor() as i32]);
}

/// Covers `cells` with as few grid-aligned cubes as the greedy largest-first
/// pass finds: (variation, centre x, centre z). An `n` m cube sits on a
/// multiple of `n`, as shipped ones do (1 m cubes centred on .5, larger
/// ones on whole metres).
pub fn cover(cells: &HashSet<[i32; 2]>) -> Vec<(u32, [f32; 2])> {
    let mut left = cells.clone();
    let mut out = Vec::new();
    for (variation, &size) in SIZES.iter().enumerate().rev() {
        let s = size as i32;
        let mut blocks: Vec<[i32; 2]> = left
            .iter()
            .map(|c| [c[0].div_euclid(s), c[1].div_euclid(s)])
            .collect::<HashSet<_>>()
            .into_iter()
            .collect();
        blocks.sort();
        for [bx, bz] in blocks {
            let full = (0..s).all(|dz| (0..s).all(|dx| left.contains(&[bx * s + dx, bz * s + dz])));
            if !full {
                continue;
            }
            for dz in 0..s {
                for dx in 0..s {
                    left.remove(&[bx * s + dx, bz * s + dz]);
                }
            }
            let half = size as f32 / 2.;
            out.push((
                variation as u32,
                [(bx * s) as f32 + half, (bz * s) as f32 + half],
            ));
        }
    }
    out
}

impl IslandEditor {
    /// Replaces the generated build blockers with cubes over every rock and
    /// decoration on buildable land; one undo step. Footprints come from
    /// the models' cfgs, read through `install`.
    pub fn auto_blockers(&mut self, install: &Install) {
        let Some(records) = self.spot_records.clone() else {
            return self.say("Blocker records are still loading", true);
        };
        let Some(record) = records.get(&BLOCKER) else {
            return self.say("No shipped island has a build blocker to copy", true);
        };
        let assets = self.assets.clone();
        let templates = self.game.profile().decoration_templates;
        let decoration = |guid: u32| {
            assets
                .as_ref()
                .is_some_and(|a| templates.contains(&harbour::template_of(a, guid).as_str()))
        };
        let mut bounds: HashMap<String, Option<Box3>> = HashMap::new();
        let mut cells = HashSet::new();
        let mut covered = 0;
        for (item, cfg, matrix) in self.item_cfgs() {
            let Some(cfg) = cfg else { continue };
            let is_decor = match item {
                Item::Prop(_) => false,
                Item::Object(id) => self
                    .doc
                    .objects
                    .iter()
                    .find(|o| o.id == id)
                    .is_some_and(|o| decoration(o.guid)),
            };
            if !blocks(&cfg, is_decor) {
                continue;
            }
            let b = bounds.entry(cfg.clone()).or_insert_with(|| {
                let m = Model::load(install, &cfg).ok()?;
                Some((m.mesh_center, m.mesh_extent))
            });
            let Some((center, extent)) = *b else { continue };
            footprint(center, extent, &matrix, &mut cells);
            covered += 1;
        }
        // Only buildable land needs blocking; the rest is closed already.
        if let Some(areas) = self.doc.grids.as_ref().and_then(|g| g.layer(Kind::AreaIds)) {
            cells.retain(|&[x, z]| {
                areas
                    .cell_at(x as f32 + 0.5, z as f32 + 0.5)
                    .is_some_and(|(i, j)| areas.get(i, j) == harbour::CONSTRUCTION)
            });
        }
        let mut cubes = cover(&cells);
        cubes.truncate(MAX_BLOCKERS);
        let placed: Vec<_> = cubes
            .iter()
            .map(|&(variation, [x, z])| (record, [x, self.doc.field.height([x, z]), z], variation))
            .collect();
        let n = self.replace_generated(&[BLOCKER], placed);
        self.layers.spots = true;
        self.say(
            format!(
                "{} over {} on buildable land",
                crate::project::plural(n, "build blocker", "build blockers"),
                crate::project::plural(
                    covered,
                    "rock or decoration model",
                    "rock and decoration models"
                )
            ),
            false,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn yaw_matrix(yaw: f32, at: [f32; 2]) -> Mat4 {
        let (s, c) = yaw.sin_cos();
        [
            c, 0., -s, 0., 0., 1., 0., 0., s, 0., c, 0., at[0], 0., at[1], 1.,
        ]
    }

    #[test]
    fn rocks_and_decoration_block_plants_do_not() {
        assert!(blocks(
            "data/dlc03/graphics/props/terrain_props/vegetation/rocks/rocks_ice/rock_ice_17.prp",
            false
        ));
        assert!(blocks(
            "data/base/graphics/roman/environment/props/rocks/roman_rock_02/roman_rock_02_b.cfg",
            false
        ));
        assert!(!blocks(
            "data/base/graphics/roman/environment/props/vegetation/trees/olive_tree_07.cfg",
            false
        ));
        assert!(blocks(
            "data/base/graphics/library/props/bench_05.cfg",
            true
        ));
        assert!(!blocks(
            "data/base/graphics/library/props/bench_05.cfg",
            false
        ));
        assert!(!blocks(
            "data/base/graphics/library/effects/water/waterfall_01.cfg",
            true
        ));
    }

    #[test]
    fn footprints_cover_with_aligned_cubes() {
        // A 4 x 2 m box turned 90° at (10.5, 20.5) stands 2 m along x, 4 m
        // along z.
        let mut cells = HashSet::new();
        footprint(
            [0.; 3],
            [2., 1., 1.],
            &yaw_matrix(std::f32::consts::FRAC_PI_2, [10., 20.]),
            &mut cells,
        );
        let xs: HashSet<i32> = cells.iter().map(|c| c[0]).collect();
        let zs: HashSet<i32> = cells.iter().map(|c| c[1]).collect();
        assert_eq!(xs.len(), 2, "{cells:?}");
        assert_eq!(zs.len(), 4, "{cells:?}");
        let cubes = cover(&cells);
        let area: u32 = cubes.iter().map(|(v, _)| SIZES[*v as usize].pow(2)).sum();
        assert_eq!(area as usize, cells.len());
        // 2 m cubes on whole metres, 1 m ones on halves.
        for (v, [x, z]) in &cubes {
            let half = SIZES[*v as usize] as f32 / 2.;
            assert_eq!((x - half).fract(), 0.);
            assert_eq!((z - half).fract(), 0.);
        }
        // A 16 m square of cells is one cube.
        let square: HashSet<[i32; 2]> = (0..16)
            .flat_map(|z| (0..16).map(move |x| [x + 32, z]))
            .collect();
        assert_eq!(cover(&square), vec![(4, [40., 8.])]);
    }
}
