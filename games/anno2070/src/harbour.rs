// 2070's harbours: areas of 1 m tiles of one asset (`harboursystem`), each
// a variation picked by which of its four neighbours are harbour too and
// whether its open sides face water, turned by quarter turns. A survey of
// the 12,900 shipped tiles on 57 islands (examples/survey.rs `harbour`):
// tiles sit on cell middles at the land plateau's 0.70 m (all but the
// 113 on underwater islands), sea edges hold harbour on three sides with the sea at their
// local +z, corners on two, ground on four, and the area runs two to five
// tiles deep behind its sea edge. Laid from a coast path, copying a shipped
// record of the asset.

use anno_core::island::objects as neutral;
use anyhow::{Context, Result};
use gpui_editor_kit::heightfield::Heightfield;

/// The harbour tile asset.
pub const HARBOUR: u32 = 10044;

// Variations of `HARBOUR`, by the survey's neighbour counts.
/// Ground with harbour on all four sides (`ground_var1`, 5,702 shipped).
const GROUND: u32 = 3;
/// A sea edge: the sea at local +z (`straight`, 2,603).
const SEA_EDGE: u32 = 2;
/// An edge onto land (`straight_land`, 1,193): the back of a harbour.
const LAND_EDGE: u32 = 7;
/// An outer corner with the sea at local +x and +z (`right_corner`, 509).
const SEA_CORNER: u32 = 10;
/// An outer corner onto land (`corner_land`, 197).
const LAND_CORNER: u32 = 6;

/// The land plateau shipped harbour tiles stand on, in metres: buildable
/// land's gameplay height band is 0.66 to 0.75 m.
const PLATEAU: f32 = 0.70;

/// Tiles from the sea edge inland, edge and back included: the shipped
/// areas' commonest depth behind a sea edge is three, and their commonest
/// end a land edge (edge, ground, ground, land edge).
pub const DEPTH: usize = 4;

/// The four axis directions in the island's (x, z), as a tile's local +x
/// turns to at `quarter` quarter turns: `(cos, sin)` of its yaw.
const AXES: [[i32; 2]; 4] = [[1, 0], [0, 1], [-1, 0], [0, -1]];

/// New harbour tiles along `coast`, points on `field`'s shore (x, z), for
/// an island whose objects are `placed`: every 1 m cell the coast crosses
/// becomes a sea edge facing its lowest neighbour, with `DEPTH - 1` tiles
/// behind it, then each new tile takes the variation and turn its
/// neighbours give it. Cells that already hold a harbour tile keep it. Ids
/// count up from one past `placed`'s largest. Fails where the install
/// ships no harbour tile to copy; empty where the coast meets no water.
pub fn along(
    install: &anno_formats::vfs::Install,
    field: &Heightfield,
    placed: &[neutral::Object],
    coast: &[[f32; 2]],
) -> Result<Vec<neutral::Object>> {
    let template = crate::objects::shipped_object(install, HARBOUR)?
        .context("no shipped 2070 island has a harbour tile to copy")?;
    let g = field.grid;
    let (cols, rows) = (g.width as i32 - 1, g.depth as i32 - 1);
    let inside = |[x, z]: [i32; 2]| (0..cols).contains(&x) && (0..rows).contains(&z);
    let ground = |[x, z]: [i32; 2]| field.height([x as f32 + 0.5, z as f32 + 0.5]);

    // Cells holding a tile already, which stay as they are.
    let mut held: Vec<[i32; 2]> = (placed.iter())
        .filter(|o| o.guid == HARBOUR)
        .map(|o| [o.position[0].floor() as i32, o.position[2].floor() as i32])
        .collect();
    held.sort_unstable();

    // The coast's cells, each with the axis toward its lowest neighbour.
    let mut edge: Vec<([i32; 2], usize)> = Vec::new();
    let segments: Vec<[[f32; 2]; 2]> = match coast {
        [p] => vec![[*p, *p]],
        _ => coast.windows(2).map(|w| [w[0], w[1]]).collect(),
    };
    for [a, b] in segments {
        let steps = ((b[0] - a[0]).hypot(b[1] - a[1]) * 4.).ceil().max(1.) as usize;
        for k in 0..=steps {
            let t = k as f32 / steps as f32;
            let c = [
                (a[0] + (b[0] - a[0]) * t).floor() as i32,
                (a[1] + (b[1] - a[1]) * t).floor() as i32,
            ];
            if !inside(c) || edge.last().is_some_and(|e| e.0 == c) {
                continue;
            }
            let low = (0..4)
                .min_by(|&i, &j| {
                    let at = |d: usize| ground([c[0] + AXES[d][0] * 2, c[1] + AXES[d][1] * 2]);
                    at(i).total_cmp(&at(j))
                })
                .unwrap();
            edge.push((c, low));
        }
    }
    // The tiles' height: the plateau behind the coast, as shipped tiles
    // stand on it.
    let mut band: Vec<[i32; 2]> = Vec::new();
    let mut heights: Vec<f32> = Vec::new();
    for &(c, sea) in &edge {
        let inland = AXES[(sea + 2) % 4];
        let back = [
            c[0] + inland[0] * (DEPTH as i32 - 1),
            c[1] + inland[1] * (DEPTH as i32 - 1),
        ];
        // The coast must meet water: ground ahead falls below the land
        // behind, as `tile` judges a sea edge against its plateau.
        let ahead = |k: i32| ground([c[0] + AXES[sea][0] * k, c[1] + AXES[sea][1] * k]);
        let floor = ground(back).min(PLATEAU);
        if !inside(back) || !(1..=REACH).any(|k| ahead(k) < floor - DROP) {
            continue;
        }
        heights.push(ground(back));
        for k in 0..DEPTH as i32 {
            let cell = [c[0] + inland[0] * k, c[1] + inland[1] * k];
            if held.binary_search(&cell).is_err() {
                band.push(cell);
            }
        }
    }
    band.sort_unstable();
    band.dedup();
    if band.is_empty() {
        return Ok(Vec::new());
    }
    heights.sort_unstable_by(f32::total_cmp);
    let behind = heights[heights.len() / 2];
    // On the land plateau, as 12,781 of the 12,900 shipped tiles stand,
    // whatever the slope behind this coast; an underwater island's harbour
    // (113 shipped, at -25.7 and -29.7 m) on the ground behind it.
    let y = if behind > PLATEAU - 10. {
        PLATEAU
    } else {
        behind
    };
    let harbour = |c: [i32; 2]| band.binary_search(&c).is_ok() || held.binary_search(&c).is_ok();

    let first = placed.iter().map(|o| o.id).max().unwrap_or(0) + 1;
    let tiles = (first..).zip(&band).map(|(id, &c)| {
        let (variation, quarter) = tile(c, y, &harbour, &ground);
        let mut o = template.copied(id);
        o.position = [c[0] as f32 + 0.5, y, c[1] as f32 + 0.5];
        o.variation = variation;
        o.set_direction(quarter as f32 * std::f32::consts::FRAC_PI_2);
        o
    });
    Ok(tiles.collect())
}

/// How far out from a tile its open side looks for water, and how far
/// below the tile the ground there must fall. The ground right in front of
/// a shipped quay is often only a little below it: 1 m out and 1 m down
/// took 414 of 2,603 sea edges for land edges. Within 4 m, 90% of sea edges
/// have ground more than 0.25 m down, and 3% of land edges do.
const REACH: i32 = 4;
const DROP: f32 = 0.25;

/// The variation and quarter turns of a harbour tile on cell `c` standing
/// at height `y`, from which neighbours hold `harbour` and whether the
/// `ground` beyond its open sides falls below the plateau into water; the
/// ground breaks ties toward the lowest side.
fn tile(
    c: [i32; 2],
    y: f32,
    harbour: &dyn Fn([i32; 2]) -> bool,
    ground: &dyn Fn([i32; 2]) -> f32,
) -> (u32, usize) {
    let side = |d: usize| [c[0] + AXES[d][0], c[1] + AXES[d][1]];
    let open: Vec<usize> = (0..4).filter(|&d| !harbour(side(d))).collect();
    let wet = |d: usize| {
        (1..=REACH).any(|k| ground([c[0] + AXES[d][0] * k, c[1] + AXES[d][1] * k]) < y - DROP)
    };
    // (variation, the axis local +z turns to)
    let (variation, z) = match open.as_slice() {
        [] => (GROUND, 1),
        &[e] => (if wet(e) { SEA_EDGE } else { LAND_EDGE }, e),
        // Adjacent open sides: an outer corner with local +x and +z on
        // them, local +z a quarter turn on from local +x.
        &[a, b] if (a + 1) % 4 == b || (b + 1) % 4 == a => {
            let x = if (a + 1) % 4 == b { a } else { b };
            let z = (x + 1) % 4;
            (
                if wet(x) || wet(z) {
                    SEA_CORNER
                } else {
                    LAND_CORNER
                },
                z,
            )
        }
        // A strip one tile wide, or a lone tile: an edge facing its lowest
        // open side.
        open => {
            let e = (open.iter().copied())
                .min_by(|&i, &j| ground(side(i)).total_cmp(&ground(side(j))))
                .expect("open sides");
            (if wet(e) { SEA_EDGE } else { LAND_EDGE }, e)
        }
    };
    // Local +z is local +x a quarter turn on, so local +x is the axis a
    // quarter turn back, and that axis's index is the tile's quarter turns.
    (variation, (z + 3) % 4)
}

/// The variation names of `HARBOUR` that a laid tile takes, for messages
/// and tests: what each is for.
pub fn kind(variation: u32) -> &'static str {
    match variation {
        GROUND => "ground",
        SEA_EDGE => "sea edge",
        LAND_EDGE => "land edge",
        SEA_CORNER => "sea corner",
        LAND_CORNER => "land corner",
        _ => "other",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anno_formats::isd::Doc;
    use anno_formats::isd::terrain::Terrain;
    use anno_formats::vfs::{FileId, Install};

    // Needs Anno 2070: the tile rule against every harbour tile the install
    // ships (12,900 on 57 islands). For each shipped sea edge, land edge
    // and outer corner on a cell middle, the rule, given which neighbours
    // the shipped harbour fills and where the ground lies below its
    // plateau, picks the same kind of piece (edge or corner, sea or land)
    // facing the same way on 3,718 of 4,496. The rest no rule on these
    // inputs reaches: 248 sea edges have harbour on all four sides (seams
    // inside larger harbours), 254 face ground at the plateau's height, and
    // about 150 corners sit where a harbour turns irregularly.
    #[test]
    fn the_tile_rule_matches_shipped_harbours() {
        use anno_core::game::Module;
        let Some(dir) = anno_formats::vfs::install_dir(crate::PROFILE.env_var) else {
            return;
        };
        let install = Install::open(&dir, crate::MODULE.profile().archive_families).unwrap();
        let (mut tiles, mut checked, mut agree) = (0, 0, 0);
        let mut islands = 0;
        let mut confusion = std::collections::BTreeMap::new();
        for i in 0..install.file_count() {
            let id = FileId(i as u32);
            let path = install.file_path(id).to_owned();
            if !path.ends_with(".isd") {
                continue;
            }
            let bytes = install.read_file(id).unwrap();
            let doc = Doc::parse(&bytes).unwrap();
            let records = anno_formats::isd::objects::read(&doc, &path).unwrap();
            let mut cells: Vec<([i32; 2], u32, i64, f32)> = (records.iter())
                .filter(|o| o.guid == HARBOUR)
                .filter(|o| (o.position[0].fract() - 0.5).abs() < 0.01)
                .map(|o| {
                    let c = [o.position[0].floor() as i32, o.position[2].floor() as i32];
                    (c, o.variation as u32, o.direction, o.position[1])
                })
                .collect();
            tiles += records.iter().filter(|o| o.guid == HARBOUR).count();
            if cells.is_empty() {
                continue;
            }
            islands += 1;
            cells.sort_unstable_by_key(|c| c.0);
            let corners = Terrain::read(&doc, &path).unwrap().corners();
            // The ground under a cell's middle: its four corners' mean.
            let ground = |[x, z]: [i32; 2]| {
                let at = |x: i32, z: i32| {
                    corners
                        .get(x.max(0) as usize, z.max(0) as usize)
                        .unwrap_or(-40.)
                };
                (at(x, z) + at(x + 1, z) + at(x, z + 1) + at(x + 1, z + 1)) / 4.
            };
            let harbour = |c: [i32; 2]| cells.binary_search_by_key(&c, |t| t.0).is_ok();
            for &(c, variation, direction, y) in &cells {
                if ![SEA_EDGE, LAND_EDGE, SEA_CORNER, LAND_CORNER].contains(&variation) {
                    continue;
                }
                checked += 1;
                let (got, quarter) = tile(c, y, &harbour, &ground);
                let shipped = (direction.rem_euclid(4 * 368_640) / 368_640) as usize;
                agree += usize::from(got == variation && quarter == shipped);
                *confusion
                    .entry((variation, got, quarter == shipped))
                    .or_insert(0) += 1;
            }
        }
        assert_eq!(tiles, 12_900);
        let share = agree as f64 / checked as f64;
        eprintln!("{agree} of {checked} edge and corner tiles on {islands} islands ({share:.3})");
        eprintln!("{confusion:?}");
        assert!(share > 0.8, "{share}");
    }
}
