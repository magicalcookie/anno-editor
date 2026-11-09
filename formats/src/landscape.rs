// Owns the rules that read an island's landscape (heights, island ground,
// tree props) into its gameplay grids: tree cells and forest chunks, the
// ambience environment and the herd area. Rules were fitted on the shipped
// islands.

use crate::game::Game;
use crate::grids::{Grids, Kind, Layer};
use crate::island::Island;

/// `AreaIDs` key of the construction area (where players build). Tree cells
/// and herds only exist there on shipped islands.
const CONSTRUCTION: u32 = 0x2001;
/// Shipped construction areas stop at about 15°; used for tree cells when
/// the island has no `AreaIDs`.
const MAX_BUILD_SLOPE: f32 = 15.;

/// Harvestable species: every live tree under `vegetation/trees/` except
/// the game's [`Profile::tree_exclusions`](crate::game::Profile).
pub fn is_harvestable_tree(game: Game, cfg: &str) -> bool {
    cfg.contains("/vegetation/trees/")
        && !(game.profile().tree_exclusions.iter()).any(|d| cfg.contains(d))
}

/// What the grid rules read: the ground heights, where the harvestable trees
/// stand, and whether the material set has marsh ground (celtic sets).
pub struct Landscape<'a> {
    /// Metres, `width × depth` samples `spacing` apart from the island origin,
    /// row-major in the grid frame (`x` east, rows down island `z`).
    pub heights: &'a [f32],
    pub width: usize,
    pub depth: usize,
    pub spacing: f32,
    /// Island metres `(x, z)` of every harvestable tree prop.
    pub trees: Vec<[f32; 2]>,
    pub marsh: bool,
}

impl<'a> Landscape<'a> {
    /// The landscape of a parsed island; `marsh` comes from the material set.
    pub fn from_island(island: &'a Island, marsh: bool) -> Self {
        let h = island.heights();
        let files = island.prop_files();
        Self {
            heights: &h.data,
            width: h.width,
            depth: h.height,
            spacing: island.world_size()[0] as f32 / (h.width - 1).max(1) as f32,
            trees: island
                .props()
                .iter()
                .filter(|p| {
                    files
                        .get(p.file as usize)
                        .is_some_and(|f| is_harvestable_tree(island.game(), f))
                })
                .map(|p| [p.position[0], p.position[2]])
                .collect(),
            marsh,
        }
    }

    /// Bilinear height at island metres, clamped to the grid.
    fn height(&self, x: f32, z: f32) -> f32 {
        let fx = (x / self.spacing).clamp(0., (self.width - 1) as f32);
        let fz = (z / self.spacing).clamp(0., (self.depth - 1) as f32);
        let (x0, z0) = (fx as usize, fz as usize);
        let (x1, z1) = ((x0 + 1).min(self.width - 1), (z0 + 1).min(self.depth - 1));
        let (tx, tz) = (fx - x0 as f32, fz - z0 as f32);
        let at = |x: usize, z: usize| self.heights[z * self.width + x];
        let top = at(x0, z0) + (at(x1, z0) - at(x0, z0)) * tx;
        let bottom = at(x0, z1) + (at(x1, z1) - at(x0, z1)) * tx;
        top + (bottom - top) * tz
    }

    /// Slope in degrees at island metres, central differences 1 m apart.
    fn slope(&self, x: f32, z: f32) -> f32 {
        let dx = (self.height(x + 1., z) - self.height(x - 1., z)) / 2.;
        let dz = (self.height(x, z + 1.) - self.height(x, z - 1.)) / 2.;
        dx.hypot(dz).atan().to_degrees()
    }

    /// Rewrites `Trees` and `ForestChunks`: a cell is a tree cell when a
    /// harvestable tree stands in it on construction ground. Shipped trees
    /// on base-area cliffs and shores are decoration (0 of 378k construction
    /// trees lack their cell; 97 % of base-area trees do).
    pub fn derive_trees(&self, grids: &mut Grids) {
        let areas = grids.layer(Kind::AreaIds).cloned();
        let Some(trees) = grids.layer_mut(Kind::Trees) else {
            return;
        };
        clear(trees);
        for &[x, z] in &self.trees {
            let Some((cx, cy)) = trees.cell_at(x, z) else {
                continue;
            };
            let buildable = match &areas {
                Some(a) => a
                    .cell_at(x, z)
                    .is_some_and(|(ax, ay)| a.get(ax, ay) == CONSTRUCTION),
                None => self.slope(x, z) <= MAX_BUILD_SLOPE,
            };
            if buildable {
                trees.set(cx, cy, 1);
            }
        }
        derive_chunks(grids);
    }

    /// Rewrites `Environment` from land, heights, slope, tree cells and the
    /// active map (rule constants below). Rare authored types (harbours,
    /// river crossings, residences, fields) are not derived.
    pub fn derive_environment(&self, grids: &mut Grids) {
        let layer = |k| grids.layer(k).cloned();
        let (Some(water), Some(trees)) = (layer(Kind::Water), layer(Kind::Trees)) else {
            return;
        };
        let active = layer(Kind::ActiveMap);
        let profile = grids.game().profile();
        let e = &profile.environments;
        let code = |name| profile.environment_value(name);
        let (coast_rock, coast_sand) = (code(e.coast_rock), code(e.coast_sand));
        let (forest, marsh, meadow) = (code(e.forest), code(e.marsh), code(e.meadow));
        let (mountain, mountain_top) = (code(e.mountain), code(e.mountain_top));
        let (sea, sea_close) = (code(e.sea), code(e.sea_close));
        let Some(env) = grids.layer_mut(Kind::Environment) else {
            return;
        };
        let (w, h, cell) = (env.width, env.height, env.cell_metres);
        // Fine (1 m) cells per environment cell along one side.
        let n = ((cell / water.cell_metres).round() as usize).max(1);
        let mut mean = vec![0f32; w * h];
        let mut steep = vec![0f32; w * h];
        for (k, (mean, steep)) in mean.iter_mut().zip(&mut steep).enumerate() {
            let (x0, z0) = ((k % w) as f32 * cell, (k / w) as f32 * cell);
            for j in 0..n {
                for i in 0..n {
                    let (x, z) = (x0 + i as f32 + 0.5, z0 + j as f32 + 0.5);
                    *mean += self.height(x, z) / (n * n) as f32;
                    *steep = steep.max(self.slope(x, z));
                }
            }
        }
        // `Water` also covers the shallow coast band below sea level; shipped
        // Sea/SeaClose cells sit below 0 m (99.8 %), so land needs both.
        let land: Vec<bool> = (0..w * h)
            .map(|k| count(&water, k % w * n, k / w * n, n) * 2 >= n * n && mean[k] >= 0.)
            .collect();
        let to_sea = distance(&land, w, h, false);
        let to_land = distance(&land, w, h, true);
        let peak = self.heights.iter().copied().fold(0f32, f32::max);
        for k in 0..w * h {
            let (x, y) = (k % w, k / w);
            let coast = if land[k] {
                to_sea[k] <= 1 && mean[k] < COAST_TOP
            } else {
                to_land[k] <= 1 && mean[k] > COAST_BOTTOM
            };
            let value = if coast {
                if steep[k] < SAND_SLOPE {
                    coast_sand
                } else {
                    coast_rock
                }
            } else if !land[k] {
                // SeaClose is the island's active region over sea (88 % of
                // shipped SeaClose lies in it, 99 % of Sea outside).
                let centre = ((x as f32 + 0.5) * cell, (y as f32 + 0.5) * cell);
                let inside = active.as_ref().is_some_and(|a| {
                    a.cell_at(centre.0, centre.1)
                        .is_some_and(|(ax, ay)| a.get(ax, ay) != 0)
                });
                if inside { sea_close } else { sea }
            } else if count(&trees, x * n, y * n, n) >= FOREST_TREES {
                forest
            } else if steep[k] >= MOUNTAIN_SLOPE {
                if mean[k] >= TOP_SHARE * peak {
                    mountain_top
                } else {
                    mountain
                }
            } else if self.marsh && mean[k] < MARSH_HEIGHT && steep[k] < MARSH_SLOPE {
                marsh
            } else {
                meadow
            };
            env.set(x, y, value);
        }
    }

    /// Rewrites `Herds`: construction-area land that is not a tree cell or
    /// river. Shipped herd grids follow the construction area and avoid
    /// trees and rivers; no herd cell lies in the base area.
    pub fn derive_herds(&self, grids: &mut Grids) {
        let layer = |k| grids.layer(k).cloned();
        let (Some(water), Some(areas)) = (layer(Kind::Water), layer(Kind::AreaIds)) else {
            return;
        };
        let (river, trees) = (layer(Kind::River), layer(Kind::Trees));
        let Some(herds) = grids.layer_mut(Kind::Herds) else {
            return;
        };
        for y in 0..herds.height {
            for x in 0..herds.width {
                let (mx, mz) = (
                    (x as f32 + 0.5) * herds.cell_metres,
                    (y as f32 + 0.5) * herds.cell_metres,
                );
                let at = |l: &Layer| l.cell_at(mx, mz).map_or(0, |(cx, cy)| l.get(cx, cy));
                let roam = at(&water) != 0
                    && at(&areas) == CONSTRUCTION
                    && river.as_ref().is_none_or(|r| at(r) == 0)
                    && trees.as_ref().is_none_or(|t| at(t) == 0);
                herds.set(x, y, roam as u32);
            }
        }
    }

    /// Every rule in dependency order: tree cells and chunks, environment
    /// (reads tree cells), herds.
    pub fn derive_all(&self, grids: &mut Grids) {
        self.derive_trees(grids);
        self.derive_environment(grids);
        self.derive_herds(grids);
    }
}

// Fitted on the shipped islands. Slopes are
// the steepest 1 m central difference inside the 4 m cell, in degrees.
/// Coast is the first cell on either side of the shoreline, on land below
/// `COAST_TOP` m and on sea above `COAST_BOTTOM` m (deeper is open water).
const COAST_TOP: f32 = 2.;
const COAST_BOTTOM: f32 = -1.;
/// Flatter coast is sand, steeper rock. Shipped coast is 89 % rock.
const SAND_SLOPE: f32 = 12.;
/// Steeper land is mountain; shipped meadow and mountain cross over here.
const MOUNTAIN_SLOPE: f32 = 23.;
/// Mountain whose mean height reaches this share of the island's peak is a
/// mountain top. Weak: shipped tops look hand-painted.
const TOP_SHARE: f32 = 0.8;
/// Tree cells (of 16 per 4 m cell) that make a forest.
const FOREST_TREES: usize = 4;
/// Celtic marsh: low, flat land.
const MARSH_HEIGHT: f32 = 1.;
const MARSH_SLOPE: f32 = 5.;

/// Up to 10 chunks; shipped islands use 10 from about 2000 tree cells, fewer
/// on small ones (about 200 cells per chunk).
const MAX_CHUNKS: usize = 10;
const CELLS_PER_CHUNK: usize = 200;
/// Chunks are dealt out in tiles of this many metres: shipped chunks are
/// 80–90 % one id per 16 m tile but spread over the whole island, so random
/// chunk removal thins the forest evenly instead of clearing one corner.
const CHUNK_TILE: usize = 16;

/// Renumbers `ForestChunks` from `Trees`: tree cells get a chunk id, every
/// other cell 0. Chunk 1, which the game never removes, gets a double share
/// (shipped roman islands: chunk 1 is the largest, about a fifth of all tree
/// cells).
pub fn derive_chunks(grids: &mut Grids) {
    let Some(trees) = grids.layer(Kind::Trees).cloned() else {
        return;
    };
    let Some(chunks) = grids.layer_mut(Kind::ForestChunks) else {
        return;
    };
    let (w, h) = (
        chunks.width.min(trees.width),
        chunks.height.min(trees.height),
    );
    let tiles_x = w.div_ceil(CHUNK_TILE);
    let mut tiles = vec![0usize; tiles_x * h.div_ceil(CHUNK_TILE)];
    let mut total = 0;
    for y in 0..h {
        for x in 0..w {
            if trees.get(x, y) != 0 {
                tiles[(y / CHUNK_TILE) * tiles_x + x / CHUNK_TILE] += 1;
                total += 1;
            }
        }
    }
    let count = (total / CELLS_PER_CHUNK).clamp(1, MAX_CHUNKS);
    // Deal tiles in a scrambled but stable order to the chunk furthest below
    // its share, so ids interleave across the island.
    let mut order: Vec<usize> = (0..tiles.len()).filter(|&t| tiles[t] > 0).collect();
    order.sort_by_key(|&t| (t as u32).wrapping_mul(2_654_435_761).rotate_left(13));
    let weight = |c: usize| if c == 0 { 2. } else { 1. };
    let mut fill = vec![0usize; count];
    let mut id = vec![0u32; tiles.len()];
    for t in order {
        let c = (0..count)
            .min_by(|&a, &b| (fill[a] as f32 / weight(a)).total_cmp(&(fill[b] as f32 / weight(b))))
            .unwrap_or(0);
        fill[c] += tiles[t];
        id[t] = c as u32 + 1;
    }
    for y in 0..chunks.height {
        for x in 0..chunks.width {
            let tree = x < w && y < h && trees.get(x, y) != 0;
            let v = if tree {
                id[(y / CHUNK_TILE) * tiles_x + x / CHUNK_TILE]
            } else {
                0
            };
            chunks.set(x, y, v);
        }
    }
}

/// Chebyshev distance in cells from each cell to the nearest cell whose
/// `land` equals `from`.
fn distance(land: &[bool], w: usize, h: usize, from: bool) -> Vec<u32> {
    let mut d = vec![u32::MAX; w * h];
    let mut queue = std::collections::VecDeque::new();
    for (k, &l) in land.iter().enumerate() {
        if l == from {
            d[k] = 0;
            queue.push_back(k);
        }
    }
    while let Some(k) = queue.pop_front() {
        let (x, y) = ((k % w) as i32, (k / w) as i32);
        for (dx, dy) in [
            (1, 0),
            (-1, 0),
            (0, 1),
            (0, -1),
            (1, 1),
            (1, -1),
            (-1, 1),
            (-1, -1),
        ] {
            let (nx, ny) = (x + dx, y + dy);
            if nx < 0 || ny < 0 || nx >= w as i32 || ny >= h as i32 {
                continue;
            }
            let n = ny as usize * w + nx as usize;
            if d[n] == u32::MAX {
                d[n] = d[k] + 1;
                queue.push_back(n);
            }
        }
    }
    d
}

fn clear(layer: &mut Layer) {
    for y in 0..layer.height {
        for x in 0..layer.width {
            layer.set(x, y, 0);
        }
    }
}

/// Set cells in the `n × n` block at `(x0, y0)` of a fine layer.
fn count(layer: &Layer, x0: usize, y0: usize, n: usize) -> usize {
    (y0..(y0 + n).min(layer.height))
        .flat_map(|y| (x0..(x0 + n).min(layer.width)).map(move |x| (x, y)))
        .filter(|&(x, y)| layer.get(x, y) != 0)
        .count()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{rda::Archive, vfs::Install};

    #[test]
    fn decoration_trees_are_not_harvestable() {
        let tree = |p: &str| {
            is_harvestable_tree(
                Game::Anno117,
                &format!("data/base/graphics/roman/environment/props/vegetation/trees/{p}"),
            )
        };
        assert!(tree("tree_cypress_01/tree_cypress_01_a.cfg"));
        assert!(tree(
            "tree_maritime_pine_01/tree_maritime_pine_01_a_arid.cfg"
        ));
        assert!(!tree("tree_cypress_01_a/tree_cypress_01_a.cfg"));
        assert!(!tree("olive_trees/olive_tree_05.cfg"));
        assert!(!tree("tree_dead_02/tree_dead_02_a.cfg"));
        assert!(!is_harvestable_tree(
            Game::Anno117,
            "data/base/graphics/roman/environment/props/vegetation/bushes/bush_03/bush_03_a.cfg"
        ));
    }

    fn set(l: &Layer) -> usize {
        (0..l.width * l.height)
            .filter(|&k| l.get(k % l.width, k / l.width) != 0)
            .count()
    }

    // Needs the game: clearing the derived grids of shipped islands and
    // deriving them again lands close to what shipped. Thresholds sit a few
    // points under the agreement measured when the rules were fitted.
    #[test]
    fn derived_grids_match_shipped_islands() {
        let Some(dir) = std::env::var_os("ANNO117_DIR") else {
            eprintln!("skip: ANNO117_DIR unset");
            return;
        };
        let install = Install::open(dir).unwrap();
        for name in [
            "roman_island_medium_01",
            "roman_island_small_07",
            "celtic_island_large_01",
        ] {
            let path = install
                .search(&format!("{name}.a7m"), 10)
                .into_iter()
                .map(|id| install.file_path(id).to_owned())
                .find(|p| p.ends_with(".a7m"))
                .unwrap();
            let a = Archive::from_bytes(install.read(&path).unwrap(), &path).unwrap();
            let island = Island::parse(
                &a.read_path("gamedata.data").unwrap(),
                &a.read_path("rd3d.data").unwrap(),
            )
            .unwrap();
            let shipped = Grids::read(&island).unwrap();
            let mut derived = shipped.clone();
            for kind in [
                Kind::Trees,
                Kind::ForestChunks,
                Kind::Environment,
                Kind::Herds,
            ] {
                clear(derived.layer_mut(kind).unwrap());
            }
            Landscape::from_island(&island, name.starts_with("celtic")).derive_all(&mut derived);
            let pair = |k| (shipped.layer(k).unwrap(), derived.layer(k).unwrap());

            let (s, d) = pair(Kind::Trees);
            let both = (0..s.width * s.height)
                .filter(|&k| {
                    s.get(k % s.width, k / s.width) != 0 && d.get(k % s.width, k / s.width) != 0
                })
                .count();
            assert!(
                both * 100 >= set(s) * 99 && both * 100 >= set(d) * 99,
                "{name} trees"
            );

            let (_, chunks) = pair(Kind::ForestChunks);
            let mut ids = std::collections::BTreeSet::new();
            for y in 0..d.height {
                for x in 0..d.width {
                    assert_eq!(
                        d.get(x, y) != 0,
                        chunks.get(x, y) != 0,
                        "{name} chunk at ({x}, {y})"
                    );
                    ids.insert(chunks.get(x, y));
                }
            }
            assert!(ids.contains(&1) && ids.len() >= 5, "{name} chunks {ids:?}");

            let (s, d) = pair(Kind::Herds);
            let both = (0..s.width * s.height)
                .filter(|&k| {
                    s.get(k % s.width, k / s.width) != 0 && d.get(k % s.width, k / s.width) != 0
                })
                .count();
            let (precision, recall) = (both as f64 / set(d) as f64, both as f64 / set(s) as f64);
            assert!(
                precision > 0.85 && recall > 0.9,
                "{name} herds {precision:.3} {recall:.3}"
            );

            let (s, d) = pair(Kind::Environment);
            let same = (0..s.width * s.height)
                .filter(|&k| s.get(k % s.width, k / s.width) == d.get(k % s.width, k / s.width))
                .count();
            let share = same as f64 / (s.width * s.height) as f64;
            assert!(share > 0.85, "{name} environment {share:.3}");
        }
    }
}
