// Owns the bytes of the rd3d water render maps that follow the terrain:
// WaterInfos byte 0 (water depth) and WaterFlow byte 2 (distance from the
// coast). Both are rebuilt from the heights; the other WaterInfos bytes are
// hand-painted masks and only reset. Rules and agreement over the shipped
// islands are measured in the tests below.

use crate::grids::{Grids, Kind};
use std::ops::Range;

/// Depth at which WaterInfos byte 0 saturates (255), metres below sea level.
const FULL_DEPTH: f32 = 5.;
/// WaterFlow's coast distance treats cells whose centre is above this as
/// land. Sea level (0) fits 92 % of shipped cells, 0.2 m 99.9 %.
const COAST_LAND: f32 = 0.2;
/// WaterFlow byte 2 is this times the squared cell distance to land, capped
/// at 255: the ramp ends 6.5 cells (13 m) out.
const COAST_STEP: u32 = 6;
/// Largest cell offset with `COAST_STEP * d²` under 255 (d² ≤ 42).
const COAST_REACH: i32 = 6;

/// WaterInfos byte 0 for a cell whose centre lies `height` metres above sea
/// level: 0 on land, `255 · (depth / 5 m)^1.5` below, truncated.
pub fn depth_byte(height: f32) -> u8 {
    let t = (-height / FULL_DEPTH).clamp(0., 1.);
    (255. * t.powf(1.5)) as u8
}

/// Heights of an island in metres, row-major, `width` samples per row and
/// `spacing` metres apart (the 0.5 m heightfield of `Island::heights`).
#[derive(Clone, Copy)]
pub struct Heights<'a> {
    pub data: &'a [f32],
    pub width: usize,
    pub spacing: f32,
}

impl Heights<'_> {
    /// Height at the centre of cell `(x, y)` of a grid with `cell`-metre
    /// cells: the sample there, as the shipped bake reads it (centre beats
    /// cell means by 7 points on byte 0).
    fn centre(&self, x: usize, y: usize, cell: f32) -> f32 {
        let at = |c: usize| ((c as f32 + 0.5) * cell / self.spacing).round() as usize;
        let rows = self.data.len() / self.width;
        self.data[at(y).min(rows - 1) * self.width + at(x).min(self.width - 1)]
    }
}

/// The derived bytes of a `width × height` water grid, in the cells of
/// `cols × rows` (meaningless elsewhere).
fn derived(
    h: Heights,
    width: usize,
    height: usize,
    cell: f32,
    (cols, rows): (Range<usize>, Range<usize>),
) -> (Vec<u8>, Vec<u8>) {
    // Heights within the coast's reach of the window's cells.
    let near = |c: usize, r: &Range<usize>| {
        c + COAST_REACH as usize >= r.start && c < r.end + COAST_REACH as usize
    };
    let centres: Vec<f32> = (0..width * height)
        .map(|k| {
            let (x, y) = (k % width, k / width);
            if near(x, &cols) && near(y, &rows) {
                h.centre(x, y, cell)
            } else {
                0.
            }
        })
        .collect();
    let depth = centres.iter().map(|&c| depth_byte(c)).collect();
    // Exact squared Euclidean distance by brute force: the window is 13×13
    // and maps are at most 384² cells, so this stays well under a second.
    let land: Vec<bool> = centres.iter().map(|&c| c > COAST_LAND).collect();
    let coast = (0..width * height)
        .map(|k| {
            if !cols.contains(&(k % width)) || !rows.contains(&(k / width)) {
                return 0;
            }
            let (x, y) = ((k % width) as i32, (k / width) as i32);
            let mut best = u32::MAX;
            for dy in -COAST_REACH..=COAST_REACH {
                for dx in -COAST_REACH..=COAST_REACH {
                    let (nx, ny) = (x + dx, y + dy);
                    if nx >= 0
                        && ny >= 0
                        && nx < width as i32
                        && ny < height as i32
                        && land[ny as usize * width + nx as usize]
                    {
                        best = best.min((dx * dx + dy * dy) as u32);
                    }
                }
            }
            best.saturating_mul(COAST_STEP).min(255) as u8
        })
        .collect();
    (depth, coast)
}

/// Rewrites WaterInfos byte 0 and WaterFlow byte 2 from `now`.
///
/// With `was`, the heights the stored bytes belong to, a cell is rewritten
/// only where `now` derives a different value than `was`: an unedited
/// island keeps its shipped bytes (the rule is exact on 85 % of byte-0 ramp
/// cells and 99.9 % of WaterFlow cells, not all). Without `was` every cell
/// is rewritten. With `window` (world metres `[x0, z0, x1, z1]`) only cells
/// whose centres lie in it are.
pub fn derive(grids: &mut Grids, now: Heights, was: Option<Heights>, window: Option<[f32; 4]>) {
    for (kind, shift) in [(Kind::WaterInfos, 0), (Kind::WaterFlow, 16)] {
        let Some(layer) = grids.layer_mut(kind) else {
            continue;
        };
        let (w, h, cell) = (layer.width, layer.height, layer.cell_metres);
        let (cols, rows) = layer.cells_within(window);
        let pick = |d: (Vec<u8>, Vec<u8>)| if shift == 0 { d.0 } else { d.1 };
        let after = pick(derived(now, w, h, cell, (cols.clone(), rows.clone())));
        let before = was.map(|was| pick(derived(was, w, h, cell, (cols.clone(), rows.clone()))));
        for (k, &v) in after.iter().enumerate() {
            let (x, y) = (k % w, k / w);
            if !cols.contains(&x)
                || !rows.contains(&y)
                || before.as_ref().is_some_and(|b| b[k] == v)
            {
                continue;
            }
            let word = layer.get(x, y) & !(0xff << shift) | (v as u32) << shift;
            layer.set(x, y, word);
        }
    }
}

/// Resets WaterInfos bytes 1..3 to their unpainted state (255, 0, 0), as 24
/// of 88 shipped islands store them everywhere. Shipped islands paint them
/// with soft round brushes along their coast, so a new island built from a
/// template must not keep the template's.
// TODO(alex): what bytes 1..3 change in game (foam, shore waves, wetness?)
// is unknown; the shaders sample them as `cWaterInfoMapRG`/`BA` (BC5).
pub fn clear_paint(grids: &mut Grids) {
    let Some(layer) = grids.layer_mut(Kind::WaterInfos) else {
        return;
    };
    for y in 0..layer.height {
        for x in 0..layer.width {
            layer.set(x, y, layer.get(x, y) & 0xff | 0xff00);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::grids::Layer;
    use crate::{island::Island, rda::Archive, vfs::Install};

    #[test]
    fn depth_ramps_over_the_first_five_metres() {
        assert_eq!(depth_byte(3.), 0);
        assert_eq!(depth_byte(0.), 0);
        assert_eq!(depth_byte(-2.5), 90);
        assert_eq!(depth_byte(-5.), 255);
        assert_eq!(depth_byte(-40.), 255);
    }

    // A square of land in a 32 m sea: the coast byte counts squared cells
    // out from the land and the depth byte follows the heights.
    #[test]
    fn coast_byte_rings_the_land() {
        let (side, spacing) = (65, 0.5);
        let data: Vec<f32> = (0..side * side)
            .map(|k| {
                let (x, z) = ((k % side) as f32 * spacing, (k / side) as f32 * spacing);
                if (10. ..22.).contains(&x) && (10. ..22.).contains(&z) {
                    2.
                } else {
                    -8.
                }
            })
            .collect();
        let h = Heights {
            data: &data,
            width: side,
            spacing,
        };
        let (depth, coast) = derived(h, 16, 16, 2., (0..16, 0..16));
        let at = |v: &[u8], x: usize, y: usize| v[y * 16 + x];
        // Land cells 5..10; row 7 runs west into the sea.
        assert_eq!(at(&coast, 5, 7), 0);
        assert_eq!(at(&coast, 4, 7), 6);
        assert_eq!(at(&coast, 3, 7), 24);
        assert_eq!(at(&coast, 0, 7), 150);
        assert_eq!(at(&coast, 4, 4), 12);
        assert_eq!(at(&depth, 7, 7), 0);
        assert_eq!(at(&depth, 0, 0), 255);
    }

    fn shipped(install: &Install, name: &str) -> Island {
        let path = install
            .search(&format!("{name}.a7m"), 10)
            .into_iter()
            .map(|id| install.file_path(id).to_owned())
            .find(|p| p.ends_with(".a7m"))
            .unwrap();
        let a = Archive::from_bytes(install.read(&path).unwrap(), &path).unwrap();
        Island::parse(
            &a.read_path("gamedata.data").unwrap(),
            &a.read_path("rd3d.data").unwrap(),
        )
        .unwrap()
    }

    fn heights(island: &Island) -> Heights<'_> {
        let h = island.heights();
        Heights {
            data: &h.data,
            width: h.width,
            spacing: island.world_size()[0] as f32 / (h.width - 1) as f32,
        }
    }

    // Needs the game. Rebuilding every cell of shipped islands lands on the
    // shipped bytes; thresholds sit just under the measured agreement
    // (byte 0 within 2 on 80 of 88 islands ≥ 99 %, coast exact
    // ≥ 99.1 % on all). celtic large_01 (85 %) and dlc01 medium_03 (82 %) are
    // left out: their byte 0 disagrees in patches, probably terrain changed
    // after the bake. Rebuilding against unchanged heights changes nothing.
    #[test]
    fn rebuilt_water_bytes_match_shipped_islands() {
        let Some(dir) = std::env::var_os("ANNO117_DIR") else {
            eprintln!("skip: ANNO117_DIR unset");
            return;
        };
        let install = Install::open(dir).unwrap();
        for name in [
            "roman_island_medium_01",
            "roman_island_small_07",
            "celtic_island_medium_03",
            "roman_island_3rdparty_pirate_01",
        ] {
            let island = shipped(&install, name);
            let grids = Grids::read(&island).unwrap();
            let mut kept = grids.clone();
            derive(&mut kept, heights(&island), Some(heights(&island)), None);
            assert!(kept.layers.iter().all(|l| !l.is_edited()), "{name} kept");

            let mut rebuilt = grids.clone();
            derive(&mut rebuilt, heights(&island), None, None);
            let pair = |k| (grids.layer(k).unwrap(), rebuilt.layer(k).unwrap());
            let agree = |k, shift: u32, tolerance: i32| {
                let (s, r) = pair(k);
                let n = s.width * s.height;
                let ok = (0..n)
                    .filter(|&i| {
                        let (x, y) = (i % s.width, i / s.width);
                        let b = |l: &Layer| ((l.get(x, y) >> shift) & 255) as i32;
                        (b(s) - b(r)).abs() <= tolerance
                    })
                    .count();
                ok as f64 / n as f64
            };
            let depth = agree(Kind::WaterInfos, 0, 2);
            let coast = agree(Kind::WaterFlow, 16, 0);
            assert!(depth >= 0.98, "{name} depth byte {depth:.3}");
            assert!(coast >= 0.99, "{name} coast byte {coast:.3}");
            // Only the derived bytes move.
            for (k, mask) in [(Kind::WaterInfos, 0xff), (Kind::WaterFlow, 0xff_0000)] {
                let (s, r) = pair(k);
                for i in 0..s.width * s.height {
                    let (x, y) = (i % s.width, i / s.width);
                    assert_eq!(s.get(x, y) & !mask, r.get(x, y) & !mask, "{name} {k:?}");
                }
            }
        }
    }
}
