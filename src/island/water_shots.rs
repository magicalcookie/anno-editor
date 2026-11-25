// Looking harness for the water of either game, not a pass/fail test:
// renders an island from named cameras after its water has run a while,
// into SHOT_DIR, to compare with the game.
//
// `ANNO117_DIR=… SHOT_DIR=… cargo test --release water_shots -- --ignored
// --nocapture`, or ANNO1800_DIR with SHOT_GAME=1800, or ANNO2070_DIR with
// SHOT_GAME=2070. SHOT_ISLAND picks the island (117's pirate island,
// 1800's moderate_c_01 and 2070's n_m02 by default).
// SHOTS='name:x,z,zoom,tilt,yaw;…' names the cameras, a negative x keeping
// the island's centre and a zoom of 1 framing the island; `rivers` among
// them adds each lake and the two longest rivers' sharpest bend, mouth and
// source. SHOT_SECONDS (4) of water run before each shot; SHOT_FRAMES more
// shots SHOT_EVERY seconds apart show motion, at exactly those water
// times with SHOT_CLOCK=1 (the physical waves then hold; for measuring
// speed). SHOT_HOURS=12,19.5 shoots each hour, suffixing the files with
// it. SHOT_MOD mounts a mod folder for SHOT_ISLAND. SHOT_RAIN=1 rains on the water, SHOT_FLOOD=1 shows Egypt in flood, SHOT_HARBOURS=0 and SHOT_OBJECTS=0
// hide those, SHOT_STILL=1 holds the waves (for comparing two builds pixel
// for pixel).
use super::IslandEditor;
use super::test_support::{PIRATE, assets, install, install_1800};
use anno_core::island::objects::PathPoint;
use std::sync::Arc;

const MODERATE_C_01: &str = "data/dlc01/sessions/islands/pool/moderate_c_01/moderate_c_01.a7m";
const N_M02: &str = "data/levels/islands/normal/n_m02.isd";

/// A camera: name, centre (`None` keeps the island's), zoom, tilt, yaw.
type View = (String, Option<[f32; 3]>, f32, f32, f32);

#[test]
#[ignore]
fn water_shots() {
    let game = std::env::var("SHOT_GAME").unwrap_or_default();
    let (game_1800, game_2070) = (game == "1800", game == "2070");
    let install = if game_1800 {
        install_1800().expect("ANNO1800_DIR")
    } else if game_2070 {
        let families = crate::games::profile(anno_core::game::Game::Anno2070).archive_families;
        anno_formats::vfs::Install::open(
            std::env::var_os("ANNO2070_DIR").expect("ANNO2070_DIR"),
            families,
        )
        .unwrap()
    } else {
        install().expect("ANNO117_DIR")
    };
    let install = Arc::new(install);
    // SHOT_MOD: a mod folder to mount, for an island a mod ships.
    if let Some(dir) = std::env::var_os("SHOT_MOD") {
        install
            .mount_mods(&[std::path::Path::new(&dir)], &["data"])
            .expect("SHOT_MOD");
    }
    let default_island = match (game_1800, game_2070) {
        (true, _) => MODERATE_C_01,
        (_, true) => N_M02,
        _ => PIRATE,
    };
    let path = std::env::var("SHOT_ISLAND").unwrap_or_else(|_| default_island.into());
    let dir = std::path::PathBuf::from(std::env::var("SHOT_DIR").expect("SHOT_DIR"));
    std::fs::create_dir_all(&dir).unwrap();
    let env = |k: &str| std::env::var(k).ok();
    let number = |k: &str, d: f32| env(k).and_then(|s| s.parse().ok()).unwrap_or(d);
    // 2070 has no asset database yet: its objects draw without names.
    let shared = (!game_2070).then(|| (install.clone(), assets(&install)));
    let mut editor = IslandEditor::load_with(
        crate::games::module(crate::games::of(&install)),
        &install,
        &path,
        shared,
    )
    .unwrap();
    editor.refresh_scene();
    while let Some(job) = editor.models_job(install.clone()) {
        editor.models_loaded(job());
    }
    while let Some(job) = editor.sky_job() {
        editor.sky_baked(job());
    }
    while let Some(job) = editor.inland_job() {
        editor.inland_built(job());
    }
    // The ground bake the app runs off the UI thread: splat and 117's
    // sinking marsh.
    if let Some(job) = editor.bake_job() {
        editor.set_ground(job().unwrap());
    }
    editor.layers.rain = env("SHOT_RAIN").is_some();
    editor.layers.flood = env("SHOT_FLOOD").is_some();
    editor.layers.waves = env("SHOT_STILL").is_none();
    editor.layers.harbours = env("SHOT_HARBOURS").is_none_or(|v| v != "0");
    editor.layers.objects = env("SHOT_OBJECTS").is_none_or(|v| v != "0");
    let extent = editor.extent();
    let mut views: Vec<View> = Vec::new();
    let shots = env("SHOTS").unwrap_or_else(|| "overview:-1,-1,1,0.9,0.6".into());
    for shot in shots.split(';').filter(|s| !s.is_empty()) {
        if shot == "rivers" {
            views.extend(river_views(&editor, extent.size));
            continue;
        }
        let (name, v) = shot.split_once(':').expect("name:x,z,zoom,tilt,yaw");
        let v: Vec<f32> = v.split(',').map(|s| s.trim().parse().unwrap()).collect();
        let centre = (v[0] >= 0.).then(|| {
            let y = editor.island.doc.field.height([v[0], v[1]]).max(0.);
            [v[0], y, v[1]]
        });
        views.push((name.into(), centre, v[2], v[3], v[4]));
    }
    let hours: Vec<Option<f32>> = match env("SHOT_HOURS") {
        Some(h) => h.split(',').map(|h| Some(h.parse().unwrap())).collect(),
        None => vec![None],
    };
    let seconds = number("SHOT_SECONDS", 4.);
    let (frames, every) = (number("SHOT_FRAMES", 1.) as usize, number("SHOT_EVERY", 1.));
    let clock = env("SHOT_CLOCK").is_some();
    let (w, h) = (1600u32, 900u32);
    let mut target = gpui_editor_kit::gpu::Offscreen::new(w, h).unwrap();
    let renderer = editor.renderer.clone();
    for hour in hours {
        if let Some(hour) = hour {
            editor.time_of_day = hour;
            // The sky baked for the last hour lit this one too.
            editor.sky = None;
            while let Some(job) = editor.sky_job() {
                editor.sky_baked(job());
            }
        }
        editor.scene_stale = true;
        editor.refresh_scene();
        for (name, centre, zoom, tilt, yaw) in &views {
            let mut camera = editor.views[0].camera;
            if let Some(c) = centre {
                camera.center = Some(*c);
                camera.pan = [0.; 2];
            }
            camera.zoom *= zoom;
            camera.tilt = *tilt;
            camera.yaw = *yaw;
            target.render(&renderer, camera, extent).unwrap();
            // The coast measures and the waves run on the clock: frames
            // until the water's time has passed. With SHOT_CLOCK the wall
            // clock only warms up the coast and spectrum, and each shot
            // shows the water at exactly its time, for measuring speed.
            let started = std::time::Instant::now();
            for f in 0..frames.max(1) {
                let until = seconds + f as f32 * every;
                let mut pixels = Vec::new();
                if clock {
                    while started.elapsed().as_secs_f32() < seconds || pixels.is_empty() {
                        pixels = target.frame(&renderer).unwrap();
                    }
                    show_water_at(&editor, until);
                    // Once to step the waves, once to draw them.
                    target.frame(&renderer).unwrap();
                    pixels = target.frame(&renderer).unwrap();
                }
                while started.elapsed().as_secs_f32() < until || pixels.is_empty() {
                    pixels = target.frame(&renderer).unwrap();
                }
                let hour = hour.map(|h| format!("_{h:02}")).unwrap_or_default();
                let file = match f {
                    0 => format!("{name}{hour}.png"),
                    f => format!("{name}{hour}_{f}.png"),
                };
                image::RgbaImage::from_raw(w, h, pixels)
                    .unwrap()
                    .save(dir.join(file))
                    .unwrap();
            }
            eprintln!(
                "{name} (zoom {}): {:?}",
                camera.zoom,
                renderer.stats().passes
            );
        }
    }
}

/// Shows the water as refresh_scene does, held at `seconds`.
pub(super) fn show_water_at(editor: &IslandEditor, seconds: f32) {
    let Some(pass) = &editor.water_pass else {
        return;
    };
    let inland = match &editor.inland_water {
        Some(super::Inland::Shaded(inland)) if editor.layers.water => Some(inland.clone()),
        _ => None,
    };
    let sea = editor.sea_water();
    pass.show(
        sea.as_ref(),
        &editor.island.doc.field,
        false,
        inland,
        Some(seconds),
    );
}

/// Each lake from above, and the two longest rivers' sharpest bend (close,
/// mid, wide and low), mouth and source.
fn river_views(editor: &IslandEditor, island: f32) -> Vec<View> {
    // Zoom framing `across` metres, as a share of the view's own.
    let zoom = |across: f32| island / across / editor.views[0].camera.zoom;
    let mut views = Vec::new();
    let mut rivers = Vec::new();
    for o in editor.island.doc.objects.iter() {
        let (Some(p), Some(lake)) = (o.path.as_deref(), editor.water_kind(o.guid)) else {
            continue;
        };
        if p.is_empty() {
            continue;
        }
        if lake {
            let c = (p.iter()).fold([0.; 3], |a, q| {
                [a[0] + q.p[0], a[1] + q.p[1], a[2] + q.p[2]]
            });
            let n = p.len() as f32;
            let name = format!("lake{}", views.len());
            views.push((name, Some(c.map(|v| v / n)), zoom(90.), 0.75, 0.5));
        } else {
            let len: f32 = (p.windows(2))
                .map(|w| (w[1].p[0] - w[0].p[0]).hypot(w[1].p[2] - w[0].p[2]))
                .sum();
            rivers.push((len, p));
        }
    }
    rivers.sort_by(|a, b| b.0.total_cmp(&a.0));
    for (k, (_, p)) in rivers.iter().take(2).enumerate() {
        if let Some(i) = bend(p) {
            let at = Some(p[i].p);
            views.push((format!("river{k}_bend"), at, zoom(50.), 0.7, 0.5));
            views.push((format!("river{k}_bend_close"), at, zoom(22.), 0.6, 0.5));
            views.push((format!("river{k}_bend_wide"), at, zoom(120.), 0.75, 0.5));
            // Low along the water, where the banks reflect.
            views.push((format!("river{k}_bend_low"), at, zoom(60.), 1.2, 0.5));
        }
        let (mouth, source) = (p[p.len() - 1].p, p[0].p);
        views.push((format!("river{k}_mouth"), Some(mouth), zoom(90.), 0.8, 0.5));
        views.push((
            format!("river{k}_source"),
            Some(source),
            zoom(60.),
            0.8,
            0.5,
        ));
    }
    views
}

/// The path point where the river turns most, two points either side.
fn bend(path: &[PathPoint]) -> Option<usize> {
    (2..path.len().saturating_sub(2))
        .map(|i| {
            let d = |a: usize, b: usize| {
                let (p, q) = (path[a].p, path[b].p);
                let l = (q[0] - p[0]).hypot(q[2] - p[2]).max(1e-3);
                [(q[0] - p[0]) / l, (q[2] - p[2]) / l]
            };
            let (a, b) = (d(i - 2, i), d(i, i + 2));
            (i, 1. - (a[0] * b[0] + a[1] * b[1]))
        })
        .max_by(|a, b| a.1.total_cmp(&b.1))
        .map(|(i, _)| i)
}
