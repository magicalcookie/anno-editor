// Tests of island model motion (motion.rs): which sequence shipped cfgs
// play by default, and the poses their pieces load with.
use super::models::{Cache, Piece, load};
use super::test_support::{install, install_1800};
use anno_core::files::ModelLook;
use anno_formats::model::Model;
use gpui_editor_kit::math;

fn pieces(install: &anno_formats::vfs::Install, cfg: &str) -> Vec<Piece> {
    load(install, &Cache::new(&ModelLook::PLAIN), cfg, 1.).unwrap()
}

/// Where a pose's frame `f` takes the mesh-space point `p` on joint `j`.
fn moved(piece: &Piece, f: usize, j: usize, p: [f32; 3]) -> [f32; 3] {
    let pose = piece.pose.as_ref().unwrap();
    let m = pose.frames[f * pose.joints as usize + j];
    std::array::from_fn(|r| m[r][0] * p[0] + m[r][1] * p[1] + m[r][2] * p[2] + m[r][3])
}

// Needs the game: a 117 resident plays its Neutral Idle 1 on the island.
// Its skinned body carries four joints a vertex and a pose over its
// skeleton's 32 joints, looping that sequence, each placement at a phase
// of its own; frames move its joints, and the pose's box holds every frame.
#[test]
fn a_resident_plays_its_neutral_idle() {
    let Some(install) = install() else {
        eprintln!("skipping: ANNO117_DIR unset");
        return;
    };
    let cfg = "data/base/graphics/roman/units/inhabitants/residents/resident_tier01_lean_f_01/resident_tier01_lean_f_01.cfg";
    let model = Model::load(&install, cfg).unwrap();
    let idle = (model.sequences.iter())
        .find(|s| s.id == 100_691_457)
        .expect("Neutral Idle 1");
    let pieces = pieces(&install, cfg);
    let body = (pieces.iter())
        .find(|p| p.pose.as_ref().is_some_and(|pose| pose.joints > 1))
        .expect("a skinned piece with a pose");
    let pose = body.pose.as_ref().unwrap();
    assert_eq!(pose.joints, 32);
    assert!(pose.phased);
    assert!(
        (pose.period - idle.length() / idle.speed).abs() < 1e-3,
        "{} s against the idle's {} s",
        pose.period,
        idle.length() / idle.speed
    );
    let skin = &body.mesh.data().skin;
    assert!(!skin.is_empty(), "the body's vertices carry their joints");
    assert!(skin.iter().all(|s| s[..4].iter().all(|&j| j < 32)));
    // Some joint moves between the loop's start and its middle.
    let frames = pose.frames.len() / 32;
    let half = frames / 2;
    let travel = (0..32)
        .map(|j| {
            math::length(math::sub(
                moved(body, half, j, [0.; 3]),
                moved(body, 0, j, [0.; 3]),
            ))
        })
        .fold(0f32, f32::max);
    assert!(travel > 0.005, "the idle moves a joint {travel} m");
    // Every frame's joints stay in the box (their origins at least).
    let [lo, hi] = pose.bounds;
    let inside = |p: [f32; 3]| (0..3).all(|a| p[a] >= lo[a] - 0.5 && p[a] <= hi[a] + 0.5);
    assert!((0..frames).all(|f| (0..32).all(|j| inside(moved(body, f, j, [0.; 3])))));
    // Shipped detail levels draw from vertices with joints too.
    for level in &body.lods {
        let own = level.data();
        assert!(own.skin.is_empty() || own.skin.len() == level.geometry().positions.len());
    }
}

// Needs the game: the freight of the continental island's harbour crane
// swings ±5 degrees (Oscillation 10) about its own origin, all placements
// in step: at rest at the loop's start and halfway, swung out a quarter in.
#[test]
fn a_harbour_cranes_freight_swings() {
    let Some(install) = install() else {
        eprintln!("skipping: ANNO117_DIR unset");
        return;
    };
    let cfg = "data/base/graphics/roman/buildings/harbor/pier/rotating_crane_animated.cfg";
    let model = Model::load(&install, cfg).unwrap();
    let motion = (model.parts.iter())
        .find_map(|p| p.rig.motion)
        .expect("a part that moves");
    let pieces = pieces(&install, cfg);
    let arm = (pieces.iter())
        .find(|p| p.pose.is_some())
        .expect("a posed piece");
    let pose = arm.pose.as_ref().unwrap();
    assert_eq!(pose.joints, 1);
    assert!(!pose.phased, "motion starts at the clock's zero");
    assert_eq!(Some(pose.period), motion.period());
    // Corners of the freight's box, how far each frame takes them, and
    // that every frame stays in the pose's box.
    let frames = pose.frames.len();
    let corners = math::corners(arm.mesh.bounds());
    let farthest = |f: usize| {
        (corners.iter())
            .map(|&c| math::length(math::sub(moved(arm, f, 0, c), c)))
            .fold(0f32, f32::max)
    };
    assert!(farthest(0) < 1e-3, "{}", farthest(0));
    assert!(farthest(frames / 2) < 0.02, "{}", farthest(frames / 2));
    assert!(farthest(frames / 4) > 0.05, "{}", farthest(frames / 4));
    let [lo, hi] = pose.bounds;
    let inside = |p: [f32; 3]| (0..3).all(|a| p[a] >= lo[a] - 1e-3 && p[a] <= hi[a] + 1e-3);
    assert!((0..frames).all(|f| corners.iter().all(|&c| inside(moved(arm, f, 0, c)))));
}

// Needs Anno 1800: the mine's eight wheels and pulleys spin (Motion), and
// a piece without motion has no pose.
#[test]
fn an_1800_mines_wheels_spin() {
    let Some(install) = install_1800() else {
        eprintln!("skipping: ANNO1800_DIR unset");
        return;
    };
    let cfg = "data/graphics/buildings/production/mining_02_slot_01/mining_02_slot_01.cfg";
    let model = Model::load(&install, cfg).unwrap();
    let moving = (model.parts.iter())
        .filter(|p| p.rig.motion.is_some_and(|m| m.period().is_some()))
        .count();
    assert_eq!(moving, 8);
    let pieces = pieces(&install, cfg);
    let posed = pieces.iter().filter(|p| p.pose.is_some()).count();
    assert!(posed >= 8, "{posed} posed pieces");
    assert!(pieces.iter().any(|p| p.pose.is_none()));
}

// ANNO117_DIR, ANNO1800_DIR: which sequence shipped cfgs with an animated
// skinned part offer as their default (motion.rs `IDLE`). Counts, not a
// pass/fail check: `cargo test default_sequences -- --ignored --nocapture`.
#[test]
#[ignore]
fn default_sequences_in_shipped_cfgs() {
    use anno_formats::model::Play;
    use anno_formats::vfs::FileId;
    use std::collections::BTreeMap;
    for install in [install(), install_1800()].into_iter().flatten() {
        let paths: Vec<String> = (0..install.file_count())
            .map(|i| install.file_path(FileId(i as u32)).to_string())
            .filter(|p| p.ends_with(".cfg") && p.starts_with("data/"))
            .collect();
        let tally = std::sync::Mutex::new(BTreeMap::<String, usize>::new());
        super::models::parallel(&paths, |path| {
            let Ok(m) = Model::load(&install, path) else {
                return;
            };
            let skinned: Vec<usize> = (0..m.parts.len())
                .filter(|&k| {
                    let p = &m.parts[k];
                    let skinned = p.mesh.as_ref().is_some_and(|r| {
                        r.skeleton.is_some() && r.joints.len() == r.positions.len()
                    });
                    skinned && !p.animations.is_empty()
                })
                .collect();
            if skinned.is_empty() {
                return;
            }
            let plays = |s: &anno_formats::model::Sequence| {
                s.clips.iter().any(|c| {
                    matches!(c.what, Play::Animation { model, .. } if skinned.contains(&(model as usize)))
                })
            };
            let mut t = tally.lock().unwrap();
            let mut add = |k: String| *t.entry(k).or_default() += 1;
            add("cfgs with an animated skinned part".into());
            for id in [100_691_457, 1000] {
                if m.sequences.iter().any(|s| s.id == id) {
                    add(format!("  with sequence {id}"));
                }
                if m.sequences
                    .iter()
                    .any(|s| s.id == id && s.looped && plays(s))
                {
                    add(format!("  playing it looped: {id}"));
                }
            }
            if !m.sequences.iter().any(|s| s.looped && plays(s)) {
                add("  no looped sequence plays the part".into());
            }
        });
        eprintln!("{}:", install.dir().display());
        for (k, v) in tally.into_inner().unwrap() {
            eprintln!("{v:7} {k}");
        }
    }
}

// ANNO117_DIR: what the pirate and continental islands' poses hold: posed
// pieces, distinct poses, and their frames' bytes as the arena packs them,
// 48 a joint a frame. load_bench's LOAD_PROFILE=1 times the baking
// ("models: bake clips"). `cargo test --release pose_cost -- --ignored
// --nocapture`.
#[test]
#[ignore]
fn pose_cost() {
    use super::gpu_bench::CONTINENTAL_01;
    use super::test_support::{PIRATE, assets};
    use std::collections::HashSet;
    let Some(install) = install() else {
        return;
    };
    let install = std::sync::Arc::new(install);
    let assets = assets(&install);
    for path in [PIRATE, CONTINENTAL_01] {
        // With the asset registry, so objects (figures, buildings) place
        // their models.
        let mut editor = super::IslandEditor::load_with(
            crate::games::module(crate::games::of(&install)),
            &install,
            path,
            Some((install.clone(), assets.clone())),
        )
        .unwrap();
        while let Some(job) = editor.models_job(install.clone()) {
            editor.models_loaded(job());
        }
        let mut seen = HashSet::new();
        let (mut pieces, mut bytes, mut skinned) = (0, 0, 0);
        for model in editor.library.models.loaded.values() {
            for p in model.iter() {
                let Some(pose) = &p.pose else { continue };
                pieces += 1;
                if seen.insert(std::sync::Arc::as_ptr(pose)) {
                    bytes += pose.frames.len() * 48;
                    skinned += usize::from(pose.joints > 1);
                }
            }
        }
        eprintln!(
            "{path}: {pieces} posed pieces, {} poses ({skinned} skinned), {:.1} MB of frames",
            seen.len(),
            bytes as f64 / 1e6,
        );
    }
}
