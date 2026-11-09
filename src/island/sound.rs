// Sound spots: invisible `AudioSpots` objects that loop a sound where they
// stand. Shipped islands put Spot_Waterfall_* at the foot of waterfall
// models and Spot_Cliff at the waterline under steep coast. Forest, meadow
// and beach ambience come from the EnvironmentGRid, not from spots, so none
// are placed for them.
use super::IslandEditor;
use anno_formats::game::Game;
use gpui_editor_kit::heightfield::Heightfield;

pub const WATERFALL_SMALL: u32 = 69133;
pub const WATERFALL_MEDIUM: u32 = 69134;
pub const WATERFALL_BIG: u32 = 69135;
pub const CLIFF: u32 = 102416;
/// Every spot asset the auto placement writes, and so replaces.
pub const GUIDS: [u32; 4] = [WATERFALL_SMALL, WATERFALL_MEDIUM, WATERFALL_BIG, CLIFF];

const WATERFALLS_1800: [[u32; 3]; 3] = [
    [102191, 102192, 102193],
    [102194, 102195, 102196],
    [102197, 102198, 102199],
];
const GUIDS_1800: [u32; 10] = [
    102191, 102192, 102193, 102194, 102195, 102196, 102197, 102198, 102199, CLIFF,
];

pub fn guids(game: Game) -> &'static [u32] {
    match game {
        Game::Anno117 => &GUIDS,
        Game::Anno1800 => &GUIDS_1800,
    }
}

/// Waterfall models within this many metres are one fall (a fall is a top,
/// a curtain and a splash model a few metres apart).
const FALL_CLUSTER: f32 = 8.;
/// Shipped waterfall spots by the ground rising within 12 m of them: Small
/// median 6.8 m, Medium 9.3, Big 13.5. Cut halfway between.
const FALL_RISE_RADIUS: f32 = 12.;
const MEDIUM_RISE: f32 = 8.;
const BIG_RISE: f32 = 11.5;
/// Spot_Cliff stands where the ground rises at least 4 m within 10 m: 70%
/// of such shipped coast has one within 20 m, against 37% of flatter coast.
const CLIFF_RISE_RADIUS: f32 = 10.;
const CLIFF_RISE: f32 = 4.;
/// Nearest other cliff spot: median 27 m, 10th percentile 17 m.
const CLIFF_GAP: f32 = 25.;
/// Coast sampling step. Shipped spots sit a median 2 m from open sea.
const COAST_STEP: f32 = 2.;
/// Bound on cliff spots; shipped islands carry 4–53.
const MAX_CLIFF_SPOTS: usize = 80;

/// One spot to place: asset and position.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Spot {
    pub guid: u32,
    pub position: [f32; 3],
}

/// Whether an object or prop model is part of a waterfall: the `waterfall*`
/// assets (51202–51205, 100961) and their curtain, foot and splash cfgs.
pub fn is_waterfall(asset_name: &str, model: Option<&str>) -> bool {
    let model = model.unwrap_or("").to_ascii_lowercase();
    (asset_name.to_ascii_lowercase().contains("waterfall") || model.contains("waterfall"))
        && !asset_name.starts_with("Spot_")
}

/// Highest ground within `radius` of `at`, above the ground at `at` (sea
/// counts as height 0).
fn rise(field: &Heightfield, at: [f32; 2], radius: f32) -> f32 {
    let here = field.height(at).max(0.);
    let mut top = here;
    let steps = radius as i32;
    for dz in -steps..=steps {
        for dx in -steps..=steps {
            let (dx, dz) = (dx as f32, dz as f32);
            if dx.hypot(dz) <= radius {
                top = top.max(field.height([at[0] + dx, at[1] + dz]));
            }
        }
    }
    top - here
}

/// One spot per waterfall, at its lowest model on the ground, sized by how
/// far the ground rises around it. Falls with a spot of any size within
/// `FALL_CLUSTER` already (`keep`, hand-placed ones) get none.
pub fn waterfall_spots(
    game: Game,
    field: &Heightfield,
    models: &[[f32; 3]],
    keep: &[[f32; 3]],
) -> Vec<Spot> {
    let mut falls: Vec<Vec<[f32; 3]>> = Vec::new();
    for &m in models {
        match falls.iter_mut().find(|f| {
            f.iter()
                .any(|p| (p[0] - m[0]).hypot(p[2] - m[2]) < FALL_CLUSTER)
        }) {
            Some(f) => f.push(m),
            None => falls.push(vec![m]),
        }
    }
    let mut out = Vec::new();
    for f in &falls {
        let foot = *f.iter().min_by(|a, b| a[1].total_cmp(&b[1])).unwrap();
        let at = [foot[0], foot[2]];
        if keep.iter().any(|k| {
            f.iter()
                .any(|p| (p[0] - k[0]).hypot(p[2] - k[2]) < FALL_CLUSTER)
        }) {
            continue;
        }
        let r = rise(field, at, FALL_RISE_RADIUS);
        let size = if r >= BIG_RISE {
            2
        } else if r >= MEDIUM_RISE {
            1
        } else {
            0
        };
        let bottom = [at[0], field.height(at).max(0.), at[1]];
        match game {
            Game::Anno117 => out.push(Spot {
                guid: GUIDS[size],
                position: bottom,
            }),
            Game::Anno1800 => {
                // 1800 has separate emitters for the top, curtain and foot.
                // Follow the terrain rise around the lowest waterfall part.
                let mut top = bottom;
                let extent = field.grid.size();
                for dz in -12..=12 {
                    for dx in -12..=12 {
                        if dx * dx + dz * dz > 144 {
                            continue;
                        }
                        let p = [at[0] + dx as f32, at[1] + dz as f32];
                        if !(0. ..=extent[0]).contains(&p[0]) || !(0. ..=extent[1]).contains(&p[1])
                        {
                            continue;
                        }
                        let h = field.height(p);
                        let distance = |p: [f32; 3]| (p[0] - at[0]).hypot(p[2] - at[1]);
                        if h > top[1]
                            || (h == top[1] && (dx as f32).hypot(dz as f32) < distance(top))
                        {
                            top = [p[0], h, p[1]];
                        }
                    }
                }
                let middle = std::array::from_fn(|i| (top[i] + bottom[i]) / 2.);
                out.extend(
                    WATERFALLS_1800[size]
                        .into_iter()
                        .zip([top, middle, bottom])
                        .map(|(guid, position)| Spot { guid, position }),
                );
            }
        }
    }
    out
}

/// Spot_Cliff along steep coast: land samples next to the sea where the
/// ground climbs `CLIFF_RISE` within `CLIFF_RISE_RADIUS`, steepest first,
/// `CLIFF_GAP` apart and from the `keep` ones. They stand on the waterline
/// (shipped y is 0.0 for 90% of them).
pub fn cliff_spots(field: &Heightfield, keep: &[[f32; 3]]) -> Vec<Spot> {
    let [w, d] = field.grid.size();
    let sea = |x: f32, z: f32| x < 0. || z < 0. || x > w || z > d || field.height([x, z]) <= 0.;
    let mut candidates = Vec::new();
    let mut z = COAST_STEP;
    while z < d - COAST_STEP {
        let mut x = COAST_STEP;
        while x < w - COAST_STEP {
            let coast = field.height([x, z]) > 0.
                && [(1., 0.), (-1., 0.), (0., 1.), (0., -1.)]
                    .iter()
                    .any(|(dx, dz)| sea(x + dx * COAST_STEP, z + dz * COAST_STEP));
            if coast {
                let r = rise(field, [x, z], CLIFF_RISE_RADIUS);
                if r >= CLIFF_RISE {
                    candidates.push((r, [x, z]));
                }
            }
            x += COAST_STEP;
        }
        z += COAST_STEP;
    }
    candidates.sort_by(|a, b| b.0.total_cmp(&a.0));
    let mut taken: Vec<[f32; 2]> = keep.iter().map(|k| [k[0], k[2]]).collect();
    let mut out = Vec::new();
    for (_, at) in candidates {
        if out.len() == MAX_CLIFF_SPOTS {
            break;
        }
        if taken
            .iter()
            .all(|t| (t[0] - at[0]).hypot(t[1] - at[1]) >= CLIFF_GAP)
        {
            taken.push(at);
            out.push(Spot {
                guid: CLIFF,
                position: [at[0], 0., at[1]],
            });
        }
    }
    out
}

impl IslandEditor {
    /// Replaces the generated sound spots with waterfall spots at every
    /// waterfall and cliff spots along steep coast; one undo step.
    /// Hand-placed and shipped spots stay, and keep generated ones away.
    pub fn auto_sound_spots(&mut self) {
        let Some(records) = self.spot_records.clone() else {
            return self.say("Sound spot records are still loading", true);
        };
        let name = |guid: u32| {
            self.assets
                .as_ref()
                .and_then(|a| a.get(guid))
                .map_or(String::new(), |a| a.name.clone())
        };
        let mut models: Vec<[f32; 3]> = self
            .doc
            .objects
            .iter()
            .filter(|o| is_waterfall(&name(o.guid), o.config.as_deref()))
            .map(|o| o.position)
            .collect();
        models.extend(
            self.doc
                .props
                .iter()
                .filter(|p| {
                    self.doc
                        .meshes
                        .get(p.source.file as usize)
                        .is_some_and(|m| is_waterfall("", Some(m)))
                })
                .map(|p| p.source.position),
        );
        let kept = |guids: &[u32]| -> Vec<[f32; 3]> {
            self.doc
                .objects
                .iter()
                .filter(|o| guids.contains(&o.guid) && !super::is_generated(o.id))
                .map(|o| o.position)
                .collect()
        };
        let falls = waterfall_spots(
            self.game,
            &self.doc.field,
            &models,
            &kept(&guids(self.game)[..guids(self.game).len() - 1]),
        );
        let cliffs = cliff_spots(&self.doc.field, &kept(&[CLIFF]));
        let (n_falls, n_cliffs) = (falls.len(), cliffs.len());
        let mut missing = Vec::new();
        let placed: Vec<_> = falls
            .iter()
            .chain(&cliffs)
            .filter_map(|s| match records.get(&s.guid) {
                Some(r) => Some((r, s.position, 0)),
                None => {
                    missing.push(name(s.guid));
                    None
                }
            })
            .collect();
        self.replace_generated(guids(self.game), placed);
        self.layers.spots = true;
        missing.dedup();
        let note = if missing.is_empty() {
            String::new()
        } else {
            format!("; no shipped record for {}", missing.join(", "))
        };
        self.say(
            format!(
                "{n_falls} waterfall and {n_cliffs} cliff sound {}{note}",
                if n_falls + n_cliffs == 1 {
                    "spot"
                } else {
                    "spots"
                }
            ),
            !missing.is_empty(),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui_editor_kit::heightfield::Grid;

    /// A 128 m square: sea west of x = 20, a 10 m cliff rising from the
    /// shore there for z < 60, and a separate flat islet (0.5 m) at the
    /// south edge.
    fn coast() -> Heightfield {
        let grid = Grid {
            width: 257,
            depth: 257,
            spacing: 0.5,
            origin: [0., 0.],
        };
        let heights = (0..257 * 257)
            .map(|i| {
                let (x, z) = ((i % 257) as f32 * 0.5, (i / 257) as f32 * 0.5);
                if (40.0..120.).contains(&x) && (100.0..120.).contains(&z) {
                    0.5
                } else if x < 20. || z > 60. {
                    -2.
                } else {
                    ((x - 20.) * 2.).min(10.)
                }
            })
            .collect();
        Heightfield::new(grid, heights)
    }

    #[test]
    fn cliff_spots_follow_steep_coast_apart() {
        let field = coast();
        let spots = cliff_spots(&field, &[]);
        assert!(spots.len() >= 3, "{spots:?}");
        for s in &spots {
            assert_eq!(s.position[1], 0.);
            // On the cliff shore, not the flat beach to the south.
            assert!((s.position[0] - 20.).abs() < 4., "{s:?}");
            assert!(s.position[2] <= 60., "{s:?}");
        }
        for (i, a) in spots.iter().enumerate() {
            for b in &spots[i + 1..] {
                let d = (a.position[0] - b.position[0]).hypot(a.position[2] - b.position[2]);
                assert!(d >= CLIFF_GAP, "{a:?} {b:?}");
            }
        }
        // A hand-placed spot keeps auto ones away from it.
        let keep = [[20.5, 0., 30.]];
        assert!(
            cliff_spots(&field, &keep)
                .iter()
                .all(|s| { (s.position[0] - 20.5).hypot(s.position[2] - 30.) >= CLIFF_GAP })
        );
    }

    #[test]
    fn waterfall_1800_has_top_stream_and_bottom_emitters() {
        let field = coast();
        let models = [[24., 8., 30.], [21., 0.5, 30.]];
        let spots = waterfall_spots(Game::Anno1800, &field, &models, &[]);
        assert_eq!(
            spots.iter().map(|s| s.guid).collect::<Vec<_>>(),
            WATERFALLS_1800[1]
        );
        assert!(spots[0].position[1] > spots[1].position[1]);
        assert!(spots[1].position[1] > spots[2].position[1]);
        assert_eq!(spots[2].position, [21., 2., 30.]);
        assert!(waterfall_spots(Game::Anno1800, &field, &models, &[[21., 2., 30.]]).is_empty());
    }

    #[test]
    fn waterfall_spots_sit_at_the_foot_sized_by_the_drop() {
        let field = coast();
        // Top on the cliff, foot on the shore: one fall, spot at the foot.
        let spots = waterfall_spots(
            Game::Anno117,
            &field,
            &[[24., 8., 30.], [21., 0.5, 30.]],
            &[],
        );
        assert_eq!(spots.len(), 1);
        assert_eq!(spots[0].guid, WATERFALL_MEDIUM, "10 m rise is medium");
        assert!((spots[0].position[0] - 21.).abs() < 1e-3);
        // On flat land the same fall is small; two falls apart get two spots.
        let flat = waterfall_spots(
            Game::Anno117,
            &field,
            &[[60., 0.5, 110.], [100., 0.5, 110.]],
            &[],
        );
        assert_eq!(flat.len(), 2);
        assert!(flat.iter().all(|s| s.guid == WATERFALL_SMALL));
        // An existing spot at a fall keeps it from getting another.
        assert!(
            waterfall_spots(
                Game::Anno117,
                &field,
                &[[60., 0.5, 110.]],
                &[[61., 0.5, 110.]]
            )
            .is_empty()
        );
    }
}
