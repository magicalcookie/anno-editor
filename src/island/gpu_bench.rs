// GPU frame-time benchmark of the island view, not a pass/fail test: fixed
// cameras on fixed islands, drawn as the app's 3D pane on a 5K screen at
// scale 2, timed per pass from GPU timestamps with the camera still and
// moving, and saved as reference images. Still is timed twice: as the
// app runs, and with the clock held (at 4 s) so nothing but exposure
// animates.
//
// `ANNO117_DIR=… ANNO1800_DIR=… ANNO2070_DIR=… BENCH_DIR=… cargo test
// --release gpu_bench -- --ignored --nocapture`.
// BENCH_VIEWS=albion,pirate,moderate,overview picks views (all by default;
// a game's skip without its *_DIR). Each view
// ends with the device's memory by allocation label, BENCH_VRAM (12) rows.
// BENCH_FRAMES (120) timed frames per phase. BENCH_KEEP=0 draws every
// frame's opaque layers (gpu::KEEP_LAYERS); BENCH_SPLIT_GROUND=0 draws paged
// ground in one draw (gpu::SPLIT_GROUND); BENCH_CHUNK_GROUND=0 draws ground
// tiles whole (gpu::CHUNK_GROUND). BENCH_SIZE=WxH is the pane in
// logical pixels (1830x1312, the screenshot's), BENCH_SCALE (2) the
// window's scale factor, BENCH_WINDOW=0 draws into a pane-sized target
// instead of a 5120x2880 window, BENCH_RENDER_SCALE (1) the share of the
// pane's pixels the scene draws at. BENCH_CAMERA=x,z,zoom,tilt,yaw overrides
// every view's camera (x < 0 keeps the island's centre), for framing;
// BENCH_HOUR (each view's, noon) the time of day, 0 to 24. BENCH_PROPS=
// cfg@x,z;… sets models on the ground first. BENCH_RESIDENCY=0 keeps every
// level of placement textures (gpu::TEXTURE_RESIDENCY). BENCH_FLY=frames
// ends each view with a fly-in: from a tenth of the zoom to the view's
// camera over that many frames, then frames until every texture level the
// view asks for is in, timed per frame (CPU recording and GPU wait).
// BENCH_FLICKER=frames ends each view with flicker measures (`flicker`): the
// clouds drifting over a still view, then a steady orbit, that many frames
// each, read back and compared frame to frame.
//
// Each reference image waits until the sky, the ground pages and the
// renderer have settled (Renderer::settled: nothing uploading, every
// texture level the view asks for in and none held past it), then draws a
// fixed count of frames with the waves held, so streaming does not decide
// what an image holds. Models still load in parallel, and a texture two
// models share takes the size of whichever asks first (models.rs
// Textures): a few models' detail can still differ between runs, as near
// cliffs on the pirate island do.
//
// `BENCH_COMPARE=base_dir,new_dir cargo test --release gpu_bench_compare --
// --ignored --nocapture` prints PSNR, mean and max per-pixel difference
// of each image both hold, and writes `<name>_diff.png` (difference ×8)
// into new_dir.
use super::IslandEditor;
use super::test_support::{PIRATE, SMALL, assets, install, install_1800, install_2070};
use anno_core::game::Game;
use gpui::{Bounds, point, px, size};
use std::sync::Arc;

const ALBION_LARGE_02: &str =
    "data/base/provinces/celtic/islands/pool/celtic_island_large_02/celtic_island_large_02.a7m";
pub(super) const MODERATE_C_01: &str =
    "data/dlc01/sessions/islands/pool/moderate_c_01/moderate_c_01.a7m";
/// The biggest 117 island: 45k props, 4k objects.
pub(super) const CONTINENTAL_01: &str = "data/dlc01/provinces/roman/islands/pool/roman_dlc01_island_continental_01/roman_dlc01_island_continental_01.a7m";

struct View {
    name: &'static str,
    game: Game,
    island: &'static str,
    /// Ground cover and lighting presets by their shown names ("Wetlands
    /// 01"); `None` keeps the island's.
    cover: Option<&'static str>,
    ambient: Option<&'static str>,
    hour: f32,
    /// Pivot x, z (x < 0 keeps the island's centre), zoom as a multiple of
    /// the editor's opening one, tilt from straight down, yaw.
    camera: [f32; 5],
}

const VIEWS: &[View] = &[
    // The user's screenshot: low over the rocky south shore, looking
    // north-north-west over the forest to the marsh, the two hills and
    // the horizon haze (matched by eye; the screenshot's cove sits just
    // under this frame).
    View {
        name: "albion",
        game: Game::Anno117,
        island: ALBION_LARGE_02,
        cover: Some("Wetlands 01"),
        ambient: Some("Egypt 013"),
        hour: 12.,
        camera: [238., 198., 3.2, 1.17, 0.21],
    },
    View {
        name: "pirate",
        game: Game::Anno117,
        island: PIRATE,
        cover: None,
        ambient: None,
        hour: 12.,
        camera: [-1., -1., 1.8, 1.15, 0.6],
    },
    View {
        name: "moderate",
        game: Game::Anno1800,
        island: MODERATE_C_01,
        cover: None,
        ambient: None,
        hour: 12.,
        camera: [-1., -1., 1.8, 1.15, 0.6],
    },
    // The whole Albion island and its sea from high up, horizon in view.
    View {
        name: "overview",
        game: Game::Anno117,
        island: ALBION_LARGE_02,
        cover: Some("Wetlands 01"),
        ambient: Some("Egypt 013"),
        hour: 12.,
        camera: [-1., -1., 1., 1.1, 0.6],
    },
    // The biggest island, close and whole: where VRAM peaks.
    View {
        name: "continental",
        game: Game::Anno117,
        island: CONTINENTAL_01,
        cover: None,
        ambient: None,
        hour: 12.,
        camera: [-1., -1., 1.8, 1.15, 0.6],
    },
    View {
        name: "continental_overview",
        game: Game::Anno117,
        island: CONTINENTAL_01,
        cover: None,
        ambient: None,
        hour: 12.,
        camera: [-1., -1., 1., 1.1, 0.6],
    },
    // The user's DLC 3 screenshot: an Egyptian quadrant whose land runs
    // off its south edge, closed by splat-mapped terrain blockers.
    View {
        name: "quadrant",
        game: Game::Anno117,
        island: QUADRANT_01_X1_Y0,
        cover: None,
        ambient: None,
        hour: 12.,
        camera: [560., 260., 1.25, 1., 0.9],
    },
    // The user's pyramid dune screenshot: the dunes around the two
    // pyramids, whose sand layers tile in world X/Z (cSplatWorldTiling).
    View {
        name: "pyramid",
        game: Game::Anno117,
        island: QUADRANT_01_X0_Y1,
        cover: None,
        ambient: None,
        hour: 12.,
        camera: [208.8, 425.4, 5., 1.0, 0.6],
    },
    // 2070's medium island from the editor's opening height: its sea
    // blends the sky cube into the kit's planar mirror, which draws the
    // island again from below the waterline.
    View {
        name: "n_m02",
        game: Game::Anno2070,
        island: "data/levels/islands/normal/n_m02.isd",
        cover: None,
        ambient: None,
        hour: 12.,
        camera: [-1., -1., 1.8, 1.15, 0.6],
    },
];

const QUADRANT_01_X1_Y0: &str = "data/dlc03/provinces/egypt/islands/pool/egypt_island_quadrant_01_x1_y0/egypt_island_quadrant_01_x1_y0.a7m";
const QUADRANT_01_X0_Y1: &str = "data/dlc03/provinces/egypt/islands/pool/egypt_island_quadrant_01_x0_y1/egypt_island_quadrant_01_x0_y1.a7m";

/// Per-pass GPU milliseconds over a phase's frames.
#[derive(Default)]
struct Timings {
    /// Pass name and each frame's time (a name drawn twice in a frame,
    /// as the shadow cascades are, is summed).
    passes: Vec<(&'static str, Vec<f32>)>,
    sum: Vec<f32>,
    span: Vec<f32>,
    /// Frames that kept nothing, the geometry, the whole opaque layers.
    reuse: [usize; 3],
}

impl Timings {
    fn add(&mut self, renderer: &gpui_editor_kit::gpu::Renderer) {
        use gpui_editor_kit::gpu::Reuse;
        let stats = &renderer.stats();
        self.reuse[match renderer.drawn().reuse {
            Reuse::Nothing => 0,
            Reuse::Geometry => 1,
            Reuse::Whole => 2,
        }] += 1;
        let mut frame: Vec<(&'static str, f32)> = Vec::new();
        for &(name, ms) in stats.passes.iter().filter(|(n, _)| !n.is_empty()) {
            match frame.iter_mut().find(|(n, _)| *n == name) {
                Some((_, t)) => *t += ms,
                None => frame.push((name, ms)),
            }
        }
        // A view's mirror times apart (2070's sea): its passes as one row.
        if stats.mirror_ms > 0. {
            frame.push(("mirror", stats.mirror_ms));
        }
        // A pass missing from this frame counts as zero, so medians of
        // passes that only run sometimes (a shadow redraw) stay honest.
        let frames = self.sum.len();
        for (name, ms) in &frame {
            if !self.passes.iter().any(|(n, _)| n == name) {
                self.passes.push((name, vec![0.; frames]));
            }
            let _ = ms;
        }
        for (name, times) in &mut self.passes {
            times.push(frame.iter().find(|(n, _)| n == name).map_or(0., |f| f.1));
        }
        self.sum.push(frame.iter().map(|f| f.1).sum());
        self.span.push(stats.gpu_span_ms);
    }

    fn print(&self, title: &str) {
        let [n, g, w] = self.reuse;
        eprintln!(
            "  {title} ({} frames; kept geometry {g}, whole {w}, nothing {n})        median     p95",
            self.sum.len()
        );
        for (name, times) in &self.passes {
            let (m, p) = quantiles(times);
            eprintln!("    {name:<22} {m:>7.3} {p:>7.3}");
        }
        let (m, p) = quantiles(&self.sum);
        eprintln!("    {:<22} {m:>7.3} {p:>7.3}", "sum of passes");
        let (m, p) = quantiles(&self.span);
        eprintln!("    {:<22} {m:>7.3} {p:>7.3}", "first to last stamp");
    }
}

fn quantiles(v: &[f32]) -> (f32, f32) {
    let mut v = v.to_vec();
    v.sort_by(f32::total_cmp);
    let at = |q: f32| v[((v.len() - 1) as f32 * q).round() as usize];
    (at(0.5), at(0.95))
}

/// Runs the editor's background jobs until none is left, as the app does
/// over its first seconds.
pub(super) fn settle(editor: &mut IslandEditor, install: &Arc<anno_formats::vfs::Install>) {
    loop {
        let mut ran = false;
        while let Some(job) = editor.models_job(install.clone()) {
            editor.models_loaded(job());
            ran = true;
        }
        while let Some(job) = editor.sky_job() {
            editor.sky_baked(job());
            ran = true;
        }
        while let Some(job) = editor.inland_job() {
            editor.inland_built(job());
            ran = true;
        }
        while let Some(job) = editor.placement_job() {
            editor.placement_built(job());
            ran = true;
        }
        while let Some(job) = editor.rails_job() {
            editor.rails_built(job());
            ran = true;
        }
        if editor.ground_stale
            && let Some(job) = editor.bake_job()
        {
            editor.set_ground(job().unwrap());
            ran = true;
        }
        editor.refresh_scene();
        if !ran {
            break;
        }
    }
}

/// The preset whose shown name is `shown`, as the inspector names them.
fn preset<'a>(
    words: &anno_core::game::AssetWords,
    ids: impl IntoIterator<Item = &'a String>,
    shown: &str,
) -> String {
    let name = |id: &str| {
        let bare = ["ls_", "amb_"]
            .iter()
            .find_map(|p| id.strip_prefix(p))
            .unwrap_or(id);
        super::pretty(words, bare).to_lowercase()
    };
    let ids: Vec<&String> = ids.into_iter().collect();
    (ids.iter())
        .find(|id| name(id) == shown.to_lowercase())
        .map(|id| id.to_string())
        .unwrap_or_else(|| panic!("no preset {shown:?} among {ids:?}"))
}

#[test]
#[ignore]
fn gpu_bench() {
    let env = |k: &str| std::env::var(k).ok();
    let dir = std::path::PathBuf::from(env("BENCH_DIR").expect("BENCH_DIR"));
    std::fs::create_dir_all(&dir).unwrap();
    let frames: usize = env("BENCH_FRAMES").map_or(120, |s| s.parse().unwrap());
    let (w, h): (f32, f32) = env("BENCH_SIZE").map_or((1830., 1312.), |s| {
        let (w, h) = s.split_once('x').expect("WxH");
        (w.parse().unwrap(), h.parse().unwrap())
    });
    let scale: f32 = env("BENCH_SCALE").map_or(2., |s| s.parse().unwrap());
    let window = env("BENCH_WINDOW").is_none_or(|v| v != "0");
    let picked = env("BENCH_VIEWS");
    // BENCH_SPLIT=1: the opaque pass's parts timed on their own, under
    // their own names (meshes, placed depth, ground…), the pass's own time
    // less them.
    gpui_editor_kit::gpu::SPLIT_TIMING.store(
        env("BENCH_SPLIT").is_some_and(|v| v == "1"),
        std::sync::atomic::Ordering::Relaxed,
    );
    // BENCH_KEEP=0: still views draw their opaque layers every frame, as
    // before they kept them (gpu::KEEP_LAYERS).
    gpui_editor_kit::gpu::KEEP_LAYERS.store(
        env("BENCH_KEEP").is_none_or(|v| v != "0"),
        std::sync::atomic::Ordering::Relaxed,
    );
    // BENCH_RESIDENCY=0: placement textures keep every level.
    gpui_editor_kit::gpu::TEXTURE_RESIDENCY.store(
        env("BENCH_RESIDENCY").is_none_or(|v| v != "0"),
        std::sync::atomic::Ordering::Relaxed,
    );
    // BENCH_SPLIT_GROUND=0: paged ground in one draw (gpu::SPLIT_GROUND).
    gpui_editor_kit::gpu::SPLIT_GROUND.store(
        env("BENCH_SPLIT_GROUND").is_none_or(|v| v != "0"),
        std::sync::atomic::Ordering::Relaxed,
    );
    // BENCH_CHUNK_GROUND=0: ground tiles whole (gpu::CHUNK_GROUND).
    gpui_editor_kit::gpu::CHUNK_GROUND.store(
        env("BENCH_CHUNK_GROUND").is_none_or(|v| v != "0"),
        std::sync::atomic::Ordering::Relaxed,
    );
    let camera_override: Option<Vec<f32>> =
        env("BENCH_CAMERA").map(|s| s.split(',').map(|v| v.trim().parse().unwrap()).collect());
    // The pane where the screenshot has it: right of the 262-wide
    // islands dock, under the 64-high menu and tabs, in a 2560x1440 window.
    let (pw, ph) = ((w * scale).round() as u32, (h * scale).round() as u32);
    let (tw, th, origin) = if window {
        let (ww, wh) = (
            pw.max((2560. * scale) as u32),
            ph.max((1440. * scale) as u32),
        );
        (ww, wh, (262f32.min(ww as f32 / scale - w), 64f32))
    } else {
        (pw, ph, (0., 0.))
    };
    let mut target = gpui_editor_kit::gpu::Offscreen::new(tw, th)
        .unwrap()
        .with_scale(scale);
    if window {
        target = target.with_viewport(Bounds::new(
            point(px(origin.0), px(origin.1)),
            size(px(w), px(h)),
        ));
    }
    eprintln!(
        "pane {pw}x{ph} physical ({w}x{h} logical at scale {scale}), target {tw}x{th}, {frames} frames"
    );
    let crop = |pixels: &[u8]| -> image::RgbaImage {
        let (x0, y0) = (
            (origin.0 * scale).round() as u32,
            (origin.1 * scale).round() as u32,
        );
        let full = image::RgbaImage::from_raw(tw, th, pixels.to_vec()).unwrap();
        image::imageops::crop_imm(&full, x0, y0, pw, ph).to_image()
    };
    // The clouds drift with the renderer's clock, in the sky's bake and on
    // the GPU: held from each view's setup to its reference image.
    let hold = |t: f32| {
        gpui_editor_kit::gpu::HELD_CLOCK.store(t.to_bits(), std::sync::atomic::Ordering::Relaxed);
    };
    let mut loaded: Option<(&str, IslandEditor, Arc<anno_formats::vfs::Install>)> = None;
    for view in VIEWS {
        if picked
            .as_ref()
            .is_some_and(|p| !p.split(',').any(|n| n == view.name))
        {
            continue;
        }
        let install = match view.game {
            Game::Anno1800 => install_1800(),
            Game::Anno2070 => install_2070(),
            _ => install(),
        };
        let Some(install) = install else {
            eprintln!("{}: skipped, no install", view.name);
            continue;
        };
        if loaded.as_ref().is_none_or(|(p, _, _)| *p != view.island) {
            let install = Arc::new(install);
            let assets = assets(&install);
            let editor = IslandEditor::load_with(
                crate::games::module(crate::games::of(&install)),
                &install,
                view.island,
                Some((install.clone(), assets)),
            )
            .unwrap();
            loaded = Some((view.island, editor, install));
        }
        let (_, editor, install) = loaded.as_mut().unwrap();
        settle(editor, install);
        if let Some(cover) = view.cover {
            let sets = editor.prop_sets.clone().expect("ground cover presets");
            let names: Vec<String> = sets.sets.iter().map(|s| s.name.clone()).collect();
            let id = preset(&editor.module().text().asset_words, &names, cover);
            editor.edit_session(|s| s.vegetation_prop_set = id);
            editor.layers.vegetation = true;
        }
        if let Some(ambient) = view.ambient {
            let id = preset(
                &editor.module().text().asset_words,
                &editor.ambients,
                ambient,
            );
            editor.edit_session(|s| s.global_ambient = id);
        }
        // The screenshot shows the harbour coasts.
        editor.layers.harbours = true;
        // BENCH_PROPS=cfg@x,z;…: models set on the ground before the view
        // draws, to look at one the island does not place (1800's shipped
        // islands place no building that glows by night).
        // A step of its own, as the Place tool takes: models load per
        // history revision.
        if env("BENCH_PROPS").is_some() {
            editor.checkpoint();
        }
        for item in env("BENCH_PROPS").iter().flat_map(|s| s.split(';')) {
            let (cfg, at) = item.split_once('@').expect("BENCH_PROPS: cfg@x,z");
            let (x, z) = at.split_once(',').expect("BENCH_PROPS: cfg@x,z");
            let (x, z) = (x.trim().parse().unwrap(), z.trim().parse().unwrap());
            let position = [x, editor.island.doc.field.height([x, z]), z];
            let source = anno_formats::a7m::island::Prop {
                file: editor.island.doc.mesh_index(cfg),
                position,
                rotation: [0., 0., 0., 1.],
                scale: [1.; 3],
                color: [1.; 4],
                adapt_terrain_height: None,
            };
            let id = editor.island.next_id;
            editor.island.next_id += 1;
            editor.island.doc.props.push(super::doc::Prop {
                id,
                matrix: super::doc::trs(position, source.rotation, source.scale),
                source,
                auto_cliff: false,
            });
        }
        // BENCH_OFF=vegetation,props,objects,water,shadows: what a layer
        // costs, by its absence.
        for off in env("BENCH_OFF").iter().flat_map(|s| s.split(',')) {
            let l = &mut editor.layers;
            match off {
                "vegetation" => l.vegetation = false,
                "props" => l.props = false,
                "objects" => l.objects = false,
                "water" => l.water = false,
                "shadows" => l.shadows = false,
                other => panic!("BENCH_OFF: no layer {other}"),
            }
        }
        // BENCH_GRAPHICS: the Graphics pane's settings, a preset (high,
        // medium, low) and/or what it changes, comma separated: clouds,
        // haze, reflections, waves, occlusion (each off), shadows=medium|
        // low|off, distance=metres, cloudshadows=stepped|off, small=off,
        // foliage=near|game|far|off, animation (off).
        // BENCH_RENDER_SCALE (1) sets its render scale.
        {
            use crate::graphics::{
                CloudShadows, Foliage, Graphics, Preset, Shadows, SmallProps, Waves,
            };
            let mut g = Graphics::default();
            for opt in (env("BENCH_GRAPHICS").iter())
                .flat_map(|s| s.split(','))
                .filter(|o| !o.is_empty())
            {
                let (key, value) = opt.split_once('=').unwrap_or((opt, ""));
                let number = || value.parse::<f32>().expect("BENCH_GRAPHICS: a number");
                match (key, value) {
                    ("high", _) => g = Graphics::preset(Preset::High),
                    ("medium", _) => g = Graphics::preset(Preset::Medium),
                    ("low", _) => g = Graphics::preset(Preset::Low),
                    ("clouds", _) => g.clouds = false,
                    ("haze", _) => g.haze = false,
                    ("reflections", _) => g.reflections = false,
                    ("waves", _) => g.waves = Waves::Reduced,
                    ("occlusion", _) => g.occlusion = false,
                    ("shadows", "medium") => g.shadows = Shadows::Medium,
                    ("shadows", "low") => g.shadows = Shadows::Low,
                    ("shadows", "off") => g.shadows = Shadows::Off,
                    ("distance", _) => g.shadow_distance = number(),
                    ("cloudshadows", "stepped") => g.cloud_shadows = CloudShadows::Stepped,
                    ("cloudshadows", "off") => g.cloud_shadows = CloudShadows::Off,
                    ("small", "off") => g.small_props = SmallProps::Off,
                    ("foliage", "near") => g.foliage = Foliage::Near,
                    ("foliage", "game") => g.foliage = Foliage::Game,
                    ("foliage", "far") => g.foliage = Foliage::Far,
                    ("foliage", "off") => g.foliage = Foliage::Off,
                    ("animation", _) => g.animation = false,
                    other => panic!("BENCH_GRAPHICS: no option {other:?}"),
                }
            }
            if let Some(scale) = env("BENCH_RENDER_SCALE") {
                g.render_scale = scale.parse().unwrap();
            }
            editor.set_graphics(g);
        }
        hold(4.);
        editor.time_of_day = env("BENCH_HOUR").map_or(view.hour, |h| h.parse().unwrap());
        editor.sky = None;
        editor.scene_stale = true;
        settle(editor, install);
        let extent = editor.extent();
        let c = camera_override.as_deref().unwrap_or(&view.camera);
        let mut camera = editor.views[0].camera;
        if c[0] >= 0. {
            let y = editor.island.doc.field.height([c[0], c[1]]).max(0.);
            camera.center = Some([c[0], y, c[1]]);
            camera.pan = [0.; 2];
        }
        camera.zoom *= c[2];
        camera.tilt = c[3];
        camera.yaw = c[4];
        let renderer = editor.renderer.clone();
        // Pipelines, uploads and tables come up over the first frames.
        target.render(&renderer, camera, extent).unwrap();
        for _ in 0..8 {
            target.frame(&renderer).unwrap();
        }
        // The image waits until nothing is left to come in or go: the
        // game's sky, which comes up on threads with its clouds settling over
        // marches, the ground pages, and the renderer (Renderer::settled):
        // uploads, every texture level the view asks for, and none held past
        // that, which residency lets go on the wall clock two to four seconds
        // on. Else it held what the time allowed. Before the waves are held,
        // which counts the frames after.
        let sky = editor.sky_pass.clone();
        let waiting = std::time::Instant::now();
        loop {
            let p = renderer.drawn().pages;
            let sky_settled = sky.as_ref().is_none_or(|s| s.settled());
            if sky_settled && renderer.settled() && p.baked >= p.wanted {
                break;
            }
            if waiting.elapsed().as_secs() >= 60 {
                eprintln!(
                    "{}: not settled after 60 s: sky {sky_settled}, {:?}, pages {p:?}",
                    view.name,
                    renderer.waiting_on()
                );
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
            target.frame(&renderer).unwrap();
        }
        // The reference image: the waves held and the sea at 4 s, so two
        // builds' images compare pixel for pixel. Running, the shore waves
        // integrate over wall time and differ between runs.
        editor.layers.waves = false;
        editor.scene_stale = true;
        editor.refresh_scene();
        super::water_shots::show_water_at(editor, 4.);

        // Always the same number of frames: the shore's water still moves
        // frame by frame with the waves held, so a count that followed the
        // pages changed the cove between builds.
        for _ in 0..48 {
            target.frame(&renderer).unwrap();
        }
        let p = renderer.drawn().pages;
        if p.baked < p.wanted {
            eprintln!(
                "{}: {} of {} ground pages baked",
                view.name, p.baked, p.wanted
            );
        }
        // Automatic exposure adapts over wall time but takes what it
        // measures at once after a pause (gpu::exposure), so each frame
        // here follows one: else how far it had adapted varied per run.
        let paused = |target: &mut gpui_editor_kit::gpu::Offscreen| {
            std::thread::sleep(std::time::Duration::from_millis(150));
            target.frame(&renderer).unwrap()
        };
        for _ in 0..3 {
            paused(&mut target);
        }
        let pixels = paused(&mut target);
        eprintln!(
            "{}: reference frame kept {:?}, settled {}",
            view.name,
            renderer.drawn().reuse,
            renderer.settled()
        );
        hold(f32::NAN);
        editor.layers.waves = true;
        crop(&pixels)
            .save(dir.join(format!("{}.png", view.name)))
            .unwrap();
        // Timed with the water running, as the app shows it.
        editor.scene_stale = true;
        editor.refresh_scene();
        target.aim(&renderer, camera, extent);
        for _ in 0..10 {
            target.time_frame(&renderer).unwrap();
        }
        let mut still = Timings::default();
        for _ in 0..frames {
            target.time_frame(&renderer).unwrap();
            still.add(&renderer);
        }
        // Still with the clock held, after a few frames to settle: cloud
        // shadows, waves and drifting clouds stand still, so the view keeps
        // its opaque layers whole (gpu::Reuse) and draws the water, sky,
        // haze, exposure and overlays over them. Running, the clouds' shadows
        // drift over the ground and only its geometry is kept. Held at the
        // reference image's 4 s, not wherever the wall clock stood, so every
        // build and run freezes the same waves and clouds. That isn't what
        // split 1800's water at 0.65 or 0.86 ms here, though: identical
        // held frames in one process, trunk's too, read either, the same
        // as a GPU that other work shares. Compare medians over runs.
        hold(4.);
        for _ in 0..4 {
            target.time_frame(&renderer).unwrap();
        }
        let mut held = Timings::default();
        for _ in 0..frames {
            target.time_frame(&renderer).unwrap();
            held.add(&renderer);
        }
        hold(f32::NAN);
        // Moving: a slow orbit back and forth about the pivot, as a drag
        // turns the view; shadows and anything keyed on the eye redo.
        let mut moving = Timings::default();
        for i in 0..frames {
            let mut c = camera;
            c.yaw += 0.05 * (i as f32 * 0.1).sin();
            target.aim(&renderer, c, extent);
            target.time_frame(&renderer).unwrap();
            moving.add(&renderer);
        }
        let drawn = renderer.drawn();
        let eye = camera.distance(extent).unwrap_or(0.);
        eprintln!(
            "{} (camera {:?}, zoom {}, eye {eye:.0} m from the pivot): {} triangles, placements per level {:?} ({} triangles), ground pages {:?}, arena draws (shadow, opaque, visibility, decals) {:?}",
            view.name,
            c,
            camera.zoom,
            drawn.triangles,
            drawn.placed,
            drawn.placed_triangles,
            drawn.pages,
            drawn.arena_draws
        );
        still.print("still");
        held.print("still, clock held");
        moving.print("moving");
        let rows = env("BENCH_VRAM").map_or(12, |s| s.parse().unwrap());
        print_vram(view.name, target.device(), rows);
        if let Some(n) = env("BENCH_FLY").map(|s| s.parse::<usize>().unwrap()) {
            fly_in(&mut target, &renderer, camera, extent, n, view.name);
        }
        if let Some(n) = env("BENCH_FLICKER").map(|s| s.parse::<usize>().unwrap()) {
            let shot = Shot {
                dir: &dir,
                name: view.name,
                crop: &crop,
                hold: &hold,
            };
            flicker(&mut target, &renderer, camera, extent, n, &shot);
            hold(f32::NAN);
        }
    }
}

/// Settles far out, then zooms in to `camera` over `n` frames and draws on
/// until the renderer has every texture level the view asks for: how many
/// frames that takes after the camera stops, and the worst frame on the way.
fn fly_in(
    target: &mut gpui_editor_kit::gpu::Offscreen,
    renderer: &Arc<gpui_editor_kit::gpu::Renderer>,
    camera: gpui_editor_kit::camera::Camera,
    extent: gpui_editor_kit::camera::Extent,
    n: usize,
    name: &str,
) {
    let ms = |d: std::time::Duration| d.as_secs_f64() * 1e3;
    let far = |t: f32| gpui_editor_kit::camera::Camera {
        zoom: camera.zoom * 0.1f32.powf(1. - t),
        ..camera
    };
    target.aim(renderer, far(0.), extent);
    for _ in 0..600 {
        target.time_frame(renderer).unwrap();
        if !renderer.pending() {
            break;
        }
    }
    // Levels nobody asks for go after their hold: start from what far
    // away keeps.
    std::thread::sleep(std::time::Duration::from_secs(5));
    target.time_frame(renderer).unwrap();
    let mut frames: Vec<[f64; 2]> = Vec::new();
    for i in 1..=n {
        target.aim(renderer, far(i as f32 / n as f32), extent);
        let [cpu, gpu] = target.time_frame_parts(renderer).unwrap();
        frames.push([ms(cpu), ms(gpu)]);
    }
    let mut settled = None;
    for k in 1..=600 {
        let [cpu, gpu] = target.time_frame_parts(renderer).unwrap();
        frames.push([ms(cpu), ms(gpu)]);
        if !renderer.pending() {
            settled = Some(k);
            break;
        }
    }
    let (at, worst) = (frames.iter().copied().enumerate())
        .max_by(|a, b| (a.1[0] + a.1[1]).total_cmp(&(b.1[0] + b.1[1])))
        .unwrap_or_default();
    let mut sums: Vec<f64> = frames.iter().map(|f| f[0] + f[1]).collect();
    sums.sort_by(f64::total_cmp);
    let p95 = sums[((sums.len() - 1) as f64 * 0.95).round() as usize];
    eprintln!(
        "  {name} fly-in over {n} frames: full detail {} frames after it stops; frames median {:.2} ms, p95 {p95:.2} ms, worst (frame {}) {:.2} ms CPU + {:.2} ms GPU",
        settled.map_or("not within 600".into(), |k| k.to_string()),
        sums[sums.len() / 2],
        at + 1,
        worst[0],
        worst[1]
    );
    // Recording and the wait apart: what a change to submit-time checks or
    // to the passes' own work moves.
    let part = |i: usize| {
        let mut v: Vec<f64> = frames.iter().map(|f| f[i]).collect();
        v.sort_by(f64::total_cmp);
        (
            v[v.len() / 2],
            v[((v.len() - 1) as f64 * 0.95).round() as usize],
        )
    };
    let ((cpu, cpu95), (gpu, gpu95)) = (part(0), part(1));
    eprintln!(
        "  {name} fly-in frames: CPU recording median {cpu:.2} ms, p95 {cpu95:.2} ms; GPU wait median {gpu:.2} ms, p95 {gpu95:.2} ms"
    );
}

/// Where `flicker` saves its images and how it frames and clocks them.
struct Shot<'a> {
    dir: &'a std::path::Path,
    name: &'a str,
    crop: &'a dyn Fn(&[u8]) -> image::RgbaImage,
    hold: &'a dyn Fn(f32),
}

/// How much the clouds flicker and stray from what they settle on, two
/// scenes of `n` frames at 60 frames a second of the held clock, after
/// some to settle in, in 8-bit levels of luminance:
/// - drift: the clouds drifting over a still view. Per pixel, the distance
///   from the mean of the frames before and after (a second difference:
///   the drift and exposure cancel, a texel that jumps and jumps back
///   counts twice).
/// - orbit: a steady orbit as a drag turns the view. Seven pairs of
///   consecutive frames, each against the same view held still until its
///   clouds settle: the error (grain, ghosts, trails), and how much it
///   changes from one frame of a pair to the next (flicker). A second
///   difference would count the ground's texels sliding past.
///
/// Each as the mean and p99 over every pixel and the share above 4 levels.
/// Writes the drift's mean per pixel ×16 (`<view>_flicker_drift.png`),
/// four consecutive orbit frames from the middle (`<view>_orbit_<i>.png`),
/// the middle one's error ×8 (`<view>_orbit_error.png`) and its change to
/// the next ×8 (`<view>_orbit_jitter.png`). Run with
/// BENCH_GRAPHICS=clouds for what the rest of the image does, and with
/// cloudshadows=off to leave out the cloud shade, whose redraws lag the
/// held clock by a few frames (117 clouds.rs SHADE_SLICES).
fn flicker(
    target: &mut gpui_editor_kit::gpu::Offscreen,
    renderer: &Arc<gpui_editor_kit::gpu::Renderer>,
    camera: gpui_editor_kit::camera::Camera,
    extent: gpui_editor_kit::camera::Extent,
    n: usize,
    shot: &Shot<'_>,
) {
    // The clouds' history settles over the first frames of a scene.
    let warm = 24;
    let clock = |i: usize| 4. + i as f32 / 60.;
    let luma = |image: &image::RgbaImage| -> Vec<f32> {
        (image.pixels())
            .map(|p| 0.2126 * f32::from(p[0]) + 0.7152 * f32::from(p[1]) + 0.0722 * f32::from(p[2]))
            .collect()
    };
    let report = |what: &str, levels: &[u64; 256], mean: f64| {
        let samples: u64 = levels.iter().sum();
        let mut seen = 0;
        let p99 = (levels.iter().position(|&c| {
            seen += c;
            seen as f64 >= 0.99 * samples as f64
        }))
        .unwrap_or(255);
        let over = levels[5..].iter().sum::<u64>() as f64 / samples.max(1) as f64;
        eprintln!(
            "  {} flicker {what} ({n} frames): mean {mean:.3} levels, p99 {p99}, {:.3}% above 4",
            shot.name,
            100. * over
        );
    };
    let count = |levels: &mut [u64; 256], d: f32| levels[(d.round() as usize).min(255)] += 1;
    // Automatic exposure adapts over wall time, which read-backs stretch
    // unevenly: each frame read follows a pause, so it takes the exposure
    // its own image measures (gpu::exposure), the same in every run.
    let read = |target: &mut gpui_editor_kit::gpu::Offscreen| {
        std::thread::sleep(std::time::Duration::from_millis(150));
        (shot.crop)(&target.frame(renderer).unwrap())
    };

    // Drift.
    target.aim(renderer, camera, extent);
    let mut lumas: std::collections::VecDeque<Vec<f32>> = Default::default();
    let mut sum: Vec<f32> = Vec::new();
    let mut levels = [0u64; 256];
    let mut size = (0, 0);
    for i in 0..warm + n {
        (shot.hold)(clock(i));
        if i < warm {
            target.frame(renderer).unwrap();
            continue;
        }
        let image = read(target);
        size = image.dimensions();
        lumas.push_back(luma(&image));
        if lumas.len() < 3 {
            continue;
        }
        sum.resize(lumas[1].len(), 0.);
        for (k, s) in sum.iter_mut().enumerate() {
            let d = (lumas[1][k] - 0.5 * (lumas[0][k] + lumas[2][k])).abs();
            *s += d;
            count(&mut levels, d);
        }
        lumas.pop_front();
    }
    let frames = n.saturating_sub(2).max(1) as f32;
    let mean = sum.iter().map(|&v| f64::from(v)).sum::<f64>()
        / (sum.len().max(1) as f64)
        / f64::from(frames);
    report("drift", &levels, mean);
    let heat = image::GrayImage::from_fn(size.0, size.1, |x, y| {
        image::Luma([(sum[(y * size.0 + x) as usize] / frames * 16.).min(255.) as u8])
    });
    heat.save(shot.dir.join(format!("{}_flicker_drift.png", shot.name)))
        .unwrap();

    // Orbit: 0.004 rad a frame, some 20 pixels at the 5K window's middle.
    let pose = |i: usize| {
        let mut c = camera;
        c.yaw += 0.004 * (i as f32 - (warm + n / 2) as f32);
        c
    };
    // Seven pairs: three left the mean to whichever frames they caught.
    let pairs: Vec<usize> = (1..8).map(|k| warm + k * n / 8).collect();
    let mut kept: Vec<(usize, image::RgbaImage)> = Vec::new();
    for i in 0..warm + n {
        (shot.hold)(clock(i));
        target.aim(renderer, pose(i), extent);
        if i < warm {
            target.frame(renderer).unwrap();
            continue;
        }
        let image = read(target);
        if (warm + n / 2..warm + n / 2 + 4).contains(&i) {
            let path = format!("{}_orbit_{}.png", shot.name, i - warm);
            image.save(shot.dir.join(path)).unwrap();
        }
        if pairs.iter().any(|&p| i == p || i == p + 1) {
            kept.push((i, image));
        }
    }
    // Each kept frame's view held at its clock until the clouds settle.
    let mut error = [0u64; 256];
    let mut jitter = [0u64; 256];
    let (mut error_sum, mut jitter_sum, mut samples) = (0f64, 0f64, 0u64);
    let mut last: Option<(usize, Vec<f32>)> = None;
    for (i, image) in &kept {
        (shot.hold)(clock(*i));
        target.aim(renderer, pose(*i), extent);
        for _ in 0..80 {
            target.frame(renderer).unwrap();
        }
        let settled = read(target);
        let (now, truth) = (luma(image), luma(&settled));
        // The settled frame measures its exposure on settled clouds: it
        // is scaled to the moving one's brightness (least squares) before
        // they compare.
        let (fg, gg) = now
            .iter()
            .zip(&truth)
            .fold((0f64, 0f64), |(fg, gg), (a, b)| {
                (fg + f64::from(a * b), gg + f64::from(b * b))
            });
        let gain = (fg / gg.max(1.)) as f32;
        let e: Vec<f32> = now.iter().zip(&truth).map(|(a, b)| a - gain * b).collect();
        for &d in &e {
            count(&mut error, d.abs());
            error_sum += f64::from(d.abs());
        }
        samples += e.len() as u64;
        if *i == warm + n / 2 {
            let (w, h) = image.dimensions();
            let diff = image::GrayImage::from_fn(w, h, |x, y| {
                image::Luma([(e[(y * w + x) as usize].abs() * 8.).min(255.) as u8])
            });
            diff.save(shot.dir.join(format!("{}_orbit_error.png", shot.name)))
                .unwrap();
        }
        if let Some((j, before)) = &last
            && j + 1 == *i
        {
            let d: Vec<f32> = e.iter().zip(before).map(|(a, b)| (a - b).abs()).collect();
            for &d in &d {
                count(&mut jitter, d);
                jitter_sum += f64::from(d);
            }
            if *j == warm + n / 2 {
                let (w, h) = image.dimensions();
                let map = image::GrayImage::from_fn(w, h, |x, y| {
                    image::Luma([(d[(y * w + x) as usize] * 8.).min(255.) as u8])
                });
                map.save(shot.dir.join(format!("{}_orbit_jitter.png", shot.name)))
                    .unwrap();
            }
        }
        last = Some((*i, e));
    }
    let pairs_samples = samples / 2;
    report("orbit error", &error, error_sum / samples.max(1) as f64);
    report(
        "orbit jitter",
        &jitter,
        jitter_sum / pairs_samples.max(1) as f64,
    );
}

/// The device's allocations summed by label, biggest first: what holds
/// VRAM after a view's frames. Reserved counts the allocator's blocks,
/// used what is allocated in them.
fn print_vram(view: &str, device: &wgpu::Device, rows: usize) {
    let Some(report) = device.generate_allocator_report() else {
        eprintln!("{view}: no allocator report on this backend");
        return;
    };
    let mib = |b: u64| b as f64 / f64::from(1u32 << 20);
    let mut labels: Vec<(&str, u64, usize)> = Vec::new();
    for a in &report.allocations {
        match labels.iter_mut().find(|(n, _, _)| *n == a.name) {
            Some((_, size, count)) => {
                *size += a.size;
                *count += 1;
            }
            None => labels.push((&a.name, a.size, 1)),
        }
    }
    labels.sort_by(|a, b| b.1.cmp(&a.1));
    eprintln!(
        "  {view} VRAM: {:.1} MiB used, {:.1} MiB reserved in {} blocks, {} allocations",
        mib(report.total_allocated_bytes),
        mib(report.total_reserved_bytes),
        report.blocks.len(),
        report.allocations.len()
    );
    for (name, size, count) in labels.iter().take(rows) {
        eprintln!("    {:>9.1} MiB {count:>6}x {name}", mib(*size));
    }
    let rest: u64 = labels.iter().skip(rows).map(|l| l.1).sum();
    if rest > 0 {
        eprintln!(
            "    {:>9.1} MiB         {} more labels",
            mib(rest),
            labels.len() - rows
        );
    }
}

/// VRAM used and reserved on `device` now, MiB.
fn vram(device: &wgpu::Device) -> (f64, f64) {
    let report = device
        .generate_allocator_report()
        .expect("allocator report");
    let mib = |b: u64| b as f64 / f64::from(1u32 << 20);
    (
        mib(report.total_allocated_bytes),
        mib(report.total_reserved_bytes),
    )
}

/// `name` open in the editor, settled and drawn into `target` until every
/// upload and texture level its opening view asks for is in, then `hold`
/// seconds more, as the app draws an island it switched to.
pub(super) fn open_and_draw(
    target: &mut gpui_editor_kit::gpu::Offscreen,
    game: &(
        Arc<anno_formats::vfs::Install>,
        Arc<anno_formats::assets::Assets>,
    ),
    path: &str,
    hold: f64,
) -> IslandEditor {
    let (install, assets) = game;
    let mut editor = IslandEditor::load_with(
        crate::games::module(crate::games::of(install)),
        install,
        path,
        Some((install.clone(), assets.clone())),
    )
    .unwrap();
    settle(&mut editor, install);
    let mut camera = editor.views[0].camera;
    camera.zoom *= 1.8;
    camera.tilt = 1.15;
    let renderer = editor.renderer.clone();
    target.aim(&renderer, camera, editor.extent());
    let at = std::time::Instant::now();
    let mut settled: Option<std::time::Instant> = None;
    while settled.is_none_or(|s| s.elapsed().as_secs_f64() < hold) {
        target.time_frame(&renderer).unwrap();
        if renderer.pending() {
            settled = None;
        } else if settled.is_none() {
            settled = Some(std::time::Instant::now());
        }
        assert!(
            at.elapsed().as_secs() < 60,
            "{path}: still uploading after 60 s"
        );
    }
    editor
}

// Video memory as the app switches islands, not a pass/fail test: opens
// each island of VRAM_ISLANDS in turn (continental,pirate,moderate,pirate;
// also albion and small; 1800's skip without ANNO1800_DIR) into a
// VRAM_SIZE (5120x2880) target at scale 2, the island left dropped before
// the next loads, and prints used and reserved once each has drawn its
// opening view whole and VRAM_HOLD (5) seconds more, then with nothing
// open, by label (BENCH_VRAM rows, 12). `ANNO117_DIR=… ANNO1800_DIR=…
// cargo test --release vram_switch_bench -- --ignored --nocapture`.
#[test]
#[ignore]
fn vram_switch_bench() {
    let env = |k: &str| std::env::var(k).ok();
    let order = env("VRAM_ISLANDS").unwrap_or("continental,pirate,moderate,pirate".into());
    let hold: f64 = env("VRAM_HOLD").map_or(5., |s| s.parse().unwrap());
    let rows = env("BENCH_VRAM").map_or(12, |s| s.parse().unwrap());
    let (w, h): (u32, u32) = env("VRAM_SIZE").map_or((5120, 2880), |s| {
        let (w, h) = s.split_once('x').expect("WxH");
        (w.parse().unwrap(), h.parse().unwrap())
    });
    let mut target = gpui_editor_kit::gpu::Offscreen::new(w, h)
        .unwrap()
        .with_scale(2.);
    let (used, reserved) = vram(target.device());
    eprintln!("{w}x{h} target: {used:.1} MiB used, {reserved:.1} MiB reserved");
    type Game = (
        bool,
        (
            Arc<anno_formats::vfs::Install>,
            Arc<anno_formats::assets::Assets>,
        ),
    );
    let mut game: Option<Game> = None;
    let mut open: Option<IslandEditor> = None;
    for name in order.split(',') {
        let (game_1800, path) = match name {
            "continental" => (false, CONTINENTAL_01),
            "albion" => (false, ALBION_LARGE_02),
            "pirate" => (false, PIRATE),
            "small" => (false, SMALL),
            "moderate" => (true, MODERATE_C_01),
            other => panic!("VRAM_ISLANDS: no island {other}"),
        };
        if let Some(left) = open.take() {
            // On the UI thread in the app, as the next island lands.
            let renderer = left.renderer.clone();
            let at = std::time::Instant::now();
            drop(left);
            let island = at.elapsed();
            let at = std::time::Instant::now();
            drop(renderer);
            let dropped = island + at.elapsed();
            // Its GPU memory goes on a thread of its own (gpu::Renderer's
            // drop); how long that takes, off the caller's thread.
            gpui_editor_kit::gpu::wait_for_freed();
            eprintln!(
                "  dropped the island in {:.0} ms, its renderer in {:.0} ms, freed {:.0} ms later",
                island.as_secs_f64() * 1e3,
                (dropped - island).as_secs_f64() * 1e3,
                (at.elapsed() - (dropped - island)).as_secs_f64() * 1e3
            );
            // The device frees what finished frames still held once polled.
            target
                .device()
                .poll(wgpu::PollType::Wait {
                    submission_index: None,
                    timeout: Some(std::time::Duration::from_secs(10)),
                })
                .unwrap();
            let (used, reserved) = vram(target.device());
            eprintln!(
                "  closed in {:.0} ms: {used:.1} MiB used, {reserved:.1} MiB reserved",
                dropped.as_secs_f64() * 1e3
            );
            print_vram("closed", target.device(), rows);
        }
        if game.as_ref().is_none_or(|g| g.0 != game_1800) {
            let install = if game_1800 { install_1800() } else { install() };
            let Some(install) = install else {
                eprintln!("{name}: skipped, no install");
                continue;
            };
            let install = Arc::new(install);
            let assets = assets(&install);
            game = Some((game_1800, (install, assets)));
        }
        let at = std::time::Instant::now();
        let editor = open_and_draw(&mut target, &game.as_ref().unwrap().1, path, hold);
        let (used, reserved) = vram(target.device());
        eprintln!(
            "{name}: {used:.1} MiB used, {reserved:.1} MiB reserved ({:.1} s)",
            at.elapsed().as_secs_f64()
        );
        print_vram(name, target.device(), rows);
        open = Some(editor);
    }
}

#[test]
#[ignore]
fn gpu_bench_compare() {
    let dirs = std::env::var("BENCH_COMPARE").expect("BENCH_COMPARE=base,new");
    let (base, new) = dirs.split_once(',').expect("base,new");
    let (base, new) = (std::path::Path::new(base), std::path::Path::new(new));
    let mut names: Vec<_> = std::fs::read_dir(base)
        .unwrap()
        .filter_map(|e| e.ok()?.file_name().into_string().ok())
        .filter(|n| n.ends_with(".png") && !n.ends_with("_diff.png"))
        .collect();
    names.sort();
    for name in names {
        let Ok(b) = image::open(base.join(&name)) else {
            continue;
        };
        let Ok(n) = image::open(new.join(&name)) else {
            continue;
        };
        let (b, n) = (b.to_rgb8(), n.to_rgb8());
        if b.dimensions() != n.dimensions() {
            eprintln!("{name}: sizes differ");
            continue;
        }
        let mut sq = 0f64;
        let mut sum = 0f64;
        let mut max = 0u8;
        // Pixels any channel of which moved by more than 8 of 255: what
        // an eye could pick out side by side.
        let mut visible = 0usize;
        let mut diff = image::RgbImage::new(b.width(), b.height());
        for ((p, q), d) in b.pixels().zip(n.pixels()).zip(diff.pixels_mut()) {
            let mut worst = 0u8;
            for ch in 0..3 {
                let e = p[ch].abs_diff(q[ch]);
                sq += f64::from(e) * f64::from(e);
                sum += f64::from(e);
                worst = worst.max(e);
                d[ch] = e.saturating_mul(8);
            }
            max = max.max(worst);
            visible += usize::from(worst > 8);
        }
        let count = f64::from(b.width() * b.height() * 3);
        let mse = sq / count;
        let psnr = if mse == 0. {
            f64::INFINITY
        } else {
            10. * (255f64 * 255. / mse).log10()
        };
        let pixels = f64::from(b.width() * b.height());
        eprintln!(
            "{name}: PSNR {psnr:.2} dB, mean {:.3}, max {max}, {:.3}% of pixels off by more than 8",
            sum / count,
            visible as f64 / pixels * 100.
        );
        let stem = name.trim_end_matches(".png");
        diff.save(new.join(format!("{stem}_diff.png"))).unwrap();
    }
}

// A ground paint stroke, then Ctrl+Z and Ctrl+Y, timed by phase on a
// shipped island (cliff tops off and on) and a new 256 m one, as the app
// runs them: a frame drawn after each dab, two frames while the worker
// bakes, then the bake lands (`set_ground`, the scene refresh) and frames
// draw until every upload is in. UNDO_TOOL=raise makes the stroke a
// sculpt. Not a pass/fail test. `ANNO117_DIR=…
// cargo test --release undo_paint_bench -- --ignored --nocapture`.
#[test]
#[ignore]
fn undo_paint_bench() {
    use super::{Drag, Stroke, Tool};
    use std::time::Instant;
    let Some(install) = install() else {
        return;
    };
    let install = Arc::new(install);
    let assets = assets(&install);
    let game = crate::games::of(&install);
    let pirate = || {
        IslandEditor::load_with(
            crate::games::module(game),
            &install,
            PIRATE,
            Some((install.clone(), assets.clone())),
        )
        .unwrap()
    };
    let mut cliffs = pirate();
    cliffs.layers.shipped = true;
    let islands: Vec<(&str, IslandEditor)> = vec![
        ("pirate", pirate()),
        ("pirate, cliff tops", cliffs),
        (
            "new 256",
            super::test_support::new_island(&install, "roman", 256, super::Start::Hills, "Bench"),
        ),
    ];
    let mut target = gpui_editor_kit::gpu::Offscreen::new(1920, 1080).unwrap();
    let ms = |d: std::time::Duration| d.as_secs_f64() * 1e3;
    for (name, mut editor) in islands {
        settle(&mut editor, &install);
        let renderer = editor.renderer.clone();
        let extent = editor.extent();
        let mut camera = editor.views[0].camera;
        camera.zoom *= 1.8;
        camera.tilt = 1.15;
        target.render(&renderer, camera, extent).unwrap();
        // Frames until every upload is in and every page the view wants is
        // baked: their count, and the worst one's CPU recording and GPU
        // wait.
        let mut until_settled = |target: &mut gpui_editor_kit::gpu::Offscreen| {
            let mut worst = [0f64; 2];
            for n in 1..=400 {
                let [cpu, gpu] = target.time_frame_parts(&renderer).unwrap();
                if ms(cpu) + ms(gpu) > worst[0] + worst[1] {
                    worst = [ms(cpu), ms(gpu)];
                }
                let p = renderer.drawn().pages;
                if !renderer.pending() && p.baked >= p.wanted && p.new == 0 {
                    return format!(
                        "{n} frames, worst {:.1} ms CPU + {:.1} ms GPU",
                        worst[0], worst[1]
                    );
                }
            }
            "400 frames, not settled".into()
        };
        until_settled(&mut target);
        let [w, d] = editor.island.doc.field.grid.size();
        let (cx, cz) = (w / 2., d / 2.);
        // UNDO_TOOL=raise: the same stroke raising the ground, as a sculpt.
        if std::env::var("UNDO_TOOL").is_ok_and(|t| t == "raise") {
            editor.tool = Tool::Raise;
        } else {
            editor.tool = Tool::Paint;
            let palette = editor.palette.clone().expect("a material set");
            editor.paint = *palette.materials.keys().last().unwrap();
        }
        // A stroke across the middle, as a drag paints it.
        let y = editor.island.doc.field.height([cx, cz]);
        let mut drag = Drag::Brush {
            stroke: Stroke::default(),
            before: Box::new(editor.island.doc.clone()),
            level: y,
            changed: false,
            hold: None,
        };
        let mut dab = 0f64;
        let mut dab_frame = 0f64;
        // The press's dab draws the shadows at once; a stroke's later dabs
        // leave them for the release (`Renderer::hold_shadows`).
        let mut later_frame = 0f64;
        // GPU timestamps per pass: other work on the machine moves the wall
        // clock's worst frame by more than a shadow redraw costs.
        let mut dab_gpu = Timings::default();
        // Each dab's frame split into recording on the CPU and the wait for
        // the GPU, worst and median: the worst frame alone can't say which
        // side grew.
        let mut parts = [Vec::new(), Vec::new(), Vec::new(), Vec::new()];
        let spans = std::env::var("UNDO_SPANS").is_ok();
        if spans {
            gpui_editor_kit::profile::enable();
            gpui_editor_kit::profile::take();
        }
        // UNDO_DABS: more dabs, for a profiler to see the dab frames over
        // the island's load.
        let dabs: usize = std::env::var("UNDO_DABS").map_or(30, |n| n.parse().unwrap());
        for i in 0..dabs {
            let started = Instant::now();
            // Past the first 30, rows further down, so each dab still
            // paints ground it hasn't.
            let (x, z) = ((i % 30) as f32 * 2., (i / 30 % 40) as f32 * 2.);
            editor.continue_brush(&mut drag, [cx - 30. + x, y, cz + z], false);
            editor.refresh_scene();
            dab = dab.max(ms(started.elapsed()));
            let stages = target.time_frame_stages(&renderer).unwrap();
            let frame: f64 = stages.iter().map(|&d| ms(d)).sum();
            dab_frame = dab_frame.max(frame);
            if i > 0 {
                later_frame = later_frame.max(frame);
            }
            for (k, d) in stages.into_iter().enumerate() {
                parts[k].push(ms(d));
            }
            dab_gpu.add(&renderer);
        }
        dab_gpu.print(&format!("{name}: dab frames, GPU"));
        let [record, submit, wait, readbacks] = parts.map(|mut v| {
            v.sort_by(f64::total_cmp);
            v
        });
        eprintln!(
            "{name}: dab frames, median/worst ms: record {:.2}/{:.2}, submit {:.2}/{:.2}, wait {:.2}/{:.2}, readbacks {:.2}/{:.2}",
            record[dabs / 2],
            record[dabs - 1],
            submit[dabs / 2],
            submit[dabs - 1],
            wait[dabs / 2],
            wait[dabs - 1],
            readbacks[dabs / 2],
            readbacks[dabs - 1]
        );
        if spans {
            for (span, n, total) in gpui_editor_kit::profile::take() {
                eprintln!(
                    "  span {span}: {n}, {:.2} ms per dab",
                    ms(total) / dabs as f64
                );
            }
        }
        editor.views[0].drag = Some(drag);
        let started = Instant::now();
        editor.finish_drag(0);
        let release = ms(started.elapsed());
        eprintln!(
            "{name}: stroke: worst dab {dab:.1} ms, worst frame after one {dab_frame:.1} ms (after the first dab {later_frame:.1} ms), release {release:.1} ms"
        );
        let landed = landing(&mut editor, &install, &mut target, &mut until_settled);
        eprintln!("{name}: stroke's bake: {landed}");
        for redo in [false, true] {
            let started = Instant::now();
            assert!(editor.undo(redo));
            editor.refresh_scene();
            let undo_ms = ms(started.elapsed());
            let landed = landing(&mut editor, &install, &mut target, &mut until_settled);
            eprintln!(
                "{name}: {} {undo_ms:.1} ms; {landed}",
                if redo { "redo" } else { "undo" }
            );
        }
    }
}

/// The app's frames after an edit while its ground bake runs on a worker,
/// then the bake landing: the bake job, `set_ground`, the scene refresh,
/// the other jobs, the first frame after (CPU recording and GPU wait) and
/// the frames until everything is uploaded.
fn landing(
    editor: &mut IslandEditor,
    install: &Arc<anno_formats::vfs::Install>,
    target: &mut gpui_editor_kit::gpu::Offscreen,
    until_settled: &mut impl FnMut(&mut gpui_editor_kit::gpu::Offscreen) -> String,
) -> String {
    use std::time::Instant;
    let ms = |d: std::time::Duration| d.as_secs_f64() * 1e3;
    let renderer = editor.renderer.clone();
    let before: Vec<String> = (0..2)
        .map(|_| format!("{:.1}", ms(target.time_frame(&renderer).unwrap())))
        .collect();
    let mut out = format!("frames before the bake {} ms", before.join(", "));
    if editor.ground_stale {
        let splat = editor.splat_stale;
        let started = Instant::now();
        let job = editor.bake_job().unwrap();
        let ground = job().unwrap();
        let bake = ms(started.elapsed());
        let started = Instant::now();
        editor.set_ground(ground);
        let set = ms(started.elapsed());
        out += &format!("; bake job {bake:.1} ms (splat rebuilt: {splat}), set_ground {set:.1} ms");
    }
    let started = Instant::now();
    let stale = editor.scene_stale;
    editor.refresh_scene();
    out += &format!(
        ", refresh_scene {:.1} ms (stale: {stale})",
        ms(started.elapsed())
    );
    let started = Instant::now();
    settle(editor, install);
    out += &format!(", other jobs {:.1} ms", ms(started.elapsed()));
    let [cpu, gpu] = target.time_frame_parts(&renderer).unwrap();
    out += &format!(
        "; landing frame {:.1} ms CPU + {:.1} ms GPU; then {}",
        ms(cpu),
        ms(gpu),
        until_settled(target)
    );
    out
}

// Process memory with islands open, not a pass/fail test: RSS once each
// island has loaded, settled and drawn its first seconds at 5K, opened in
// turn as the app switches islands, then the live heap's largest owners.
// `ANNO117_DIR=… ANNO1800_DIR=… cargo test ram_bench -- --ignored
// --nocapture --exact island::gpu_bench::ram_bench`. RAM_ISLANDS picks the
// order (continental,pirate,moderate,continental; 1800's skip without
// ANNO1800_DIR; "closed" drops everything open), RAM_TOP (30) the owners
// listed, RAM_SECONDS (6) how long each island draws at least. It draws on until every texture level the
// view asks for is in, and 5 s more: residency lets placement texels go
// two to four seconds after their last growth. Build the dev profile: the
// owners' names come from its debug info.
#[test]
#[ignore]
fn ram_bench() {
    use crate::heap;
    // As the app sets it at startup.
    crate::tune_malloc();
    let env = |k: &str| std::env::var(k).ok();
    let top: usize = env("RAM_TOP").map_or(30, |s| s.parse().unwrap());
    let seconds: f64 = env("RAM_SECONDS").map_or(6., |s| s.parse().unwrap());
    let order = env("RAM_ISLANDS").unwrap_or("continental,pirate,moderate,continental".into());
    heap::start();
    eprintln!("start: {}", heap::resident());
    let mut target = gpui_editor_kit::gpu::Offscreen::new(5120, 2880)
        .unwrap()
        .with_scale(2.);
    eprintln!("5K target: {}", heap::resident());
    // The open game's install and asset registry, kept across its islands
    // as the app keeps them; another game's replace them.
    type Game = (
        bool,
        Arc<anno_formats::vfs::Install>,
        Arc<anno_formats::assets::Assets>,
    );
    let mut game: Option<Game> = None;
    let mut open: Option<IslandEditor> = None;
    for name in order.split(',') {
        // "closed": everything dropped, so the report's live heap is what
        // islands and installs leave behind.
        if name == "closed" {
            drop((open.take(), game.take()));
            heap::trim();
            eprintln!("closed: {}", heap::resident());
            continue;
        }
        let (game_1800, path) = match name {
            "continental" => (false, CONTINENTAL_01),
            "albion" => (false, ALBION_LARGE_02),
            "pirate" => (false, PIRATE),
            "moderate" => (true, MODERATE_C_01),
            other => panic!("RAM_ISLANDS: no island {other}"),
        };
        // The island left goes before the next loads, as in the app.
        drop(open.take());
        if game.as_ref().is_none_or(|g| g.0 != game_1800) {
            drop(game.take());
            heap::trim();
            let install = if game_1800 { install_1800() } else { install() };
            let Some(install) = install else {
                eprintln!("{name}: skipped, no install");
                continue;
            };
            let install = Arc::new(install);
            let assets = assets(&install);
            game = Some((game_1800, install, assets));
            let label = if game_1800 { "1800" } else { "117" };
            eprintln!("{label} install and assets: {}", heap::resident());
        }
        let (_, install, assets) = game.clone().unwrap();
        heap::trim();
        heap::reset_peak();
        let before = heap::resident();
        let (live_before, _) = heap::live();
        let watch = heap::Watch::start();
        let at = std::time::Instant::now();
        let mut editor = IslandEditor::load_with(
            crate::games::module(crate::games::of(&install)),
            &install,
            path,
            Some((install.clone(), assets)),
        )
        .unwrap();
        settle(&mut editor, &install);
        let loaded = at.elapsed();
        let extent = editor.extent();
        let mut camera = editor.views[0].camera;
        camera.zoom *= 1.8;
        camera.tilt = 1.15;
        let renderer = editor.renderer.clone();
        // Aimed, not rendered: a render reads the 5K frame back.
        target.aim(&renderer, camera, extent);
        let drawing = std::time::Instant::now();
        let mut frames = 0;
        // When every upload and texture level the view asks for was in;
        // drawn on past it until texels nobody grows have gone.
        let mut settled: Option<(usize, std::time::Duration)> = None;
        let held = |settled: Option<(usize, std::time::Duration)>| {
            settled.is_none_or(|(_, at)| (drawing.elapsed() - at).as_secs_f64() < 5.)
        };
        while drawing.elapsed().as_secs_f64() < seconds || held(settled) {
            target.time_frame(&renderer).unwrap();
            frames += 1;
            if renderer.pending() {
                settled = None;
            } else if settled.is_none() {
                settled = Some((frames, drawing.elapsed()));
            }
        }
        if let Some((n, at)) = settled {
            eprintln!(
                "{name}: full detail after {n} frames, {:.2} s",
                at.as_secs_f64()
            );
        }
        let drawn = heap::resident();
        let (top, top_live, top_at) = watch.stop();
        // What stays once glibc hands back what the load let go: the
        // app trims after uploads and edits (gpu::trim_heap).
        heap::trim();
        eprintln!(
            "{name}: loaded in {:.2} s, {frames} frames; drawn {drawn}; trimmed {}",
            loaded.as_secs_f64(),
            heap::resident()
        );
        // The sampled live heap's high against the RSS peak, both since the
        // load began: what the load itself held, apart from what glibc and
        // the driver kept.
        let (live, high) = heap::live();
        eprintln!(
            "{name}: before the load {before}; live heap {live_before:.0} MiB then, \
             {live:.0} now, at most {high:.0}"
        );
        eprintln!(
            "{name}: at the highest {top}; live heap then {top_live:.0} MiB, {:.2} s in \
             (loaded and settled at {:.2} s)",
            top_at.as_secs_f64(),
            loaded.as_secs_f64()
        );
        open = Some(editor);
    }
    // RAM_SMAPS=file: the process's mappings, to tell the heap from the
    // driver's and glibc's own.
    if let Some(file) = env("RAM_SMAPS") {
        std::fs::copy("/proc/self/smaps", file).unwrap();
    }
    heap::report(top);
}

// Anno 2070's n_m02 as the app draws it: frames with the sea's clock
// running (its prepare runs every frame), then a 30-dab paint stroke
// through the layer ground, each dab's edit and the frame after it timed.
// Not a pass/fail test. `ANNO2070_DIR=… cargo test --release
// a2070_bench -- --ignored --nocapture`.
#[test]
#[ignore]
fn a2070_bench() {
    use super::{Drag, Stroke, Tool};
    use std::time::Instant;
    let Some(dir) = anno_formats::vfs::install_dir("ANNO2070_DIR") else {
        return;
    };
    let families = crate::games::profile(anno_core::game::Game::Anno2070).archive_families;
    let install = Arc::new(anno_formats::vfs::Install::open(dir, families).unwrap());
    let path = "data/levels/islands/normal/n_m02.isd";
    let mut editor = IslandEditor::load(
        crate::games::module(crate::games::of(&install)),
        &install,
        path,
    )
    .unwrap();
    settle(&mut editor, &install);
    let renderer = editor.renderer.clone();
    let mut target = gpui_editor_kit::gpu::Offscreen::new(1920, 1080).unwrap();
    target
        .render(&renderer, editor.views[0].camera, editor.extent())
        .unwrap();
    for _ in 0..60 {
        target.time_frame(&renderer).unwrap();
    }
    let ms = |d: std::time::Duration| d.as_secs_f64() * 1e3;
    let stats = |mut v: Vec<f64>| {
        v.sort_by(f64::total_cmp);
        format!(
            "median {:.3}, p95 {:.3}, worst {:.3}",
            v[v.len() / 2],
            v[v.len() * 95 / 100],
            v[v.len() - 1]
        )
    };
    let mut record = Vec::new();
    for _ in 0..300 {
        let [cpu, _] = target.time_frame_parts(&renderer).unwrap();
        record.push(ms(cpu));
    }
    eprintln!("n_m02 sea frames, CPU recording ms: {}", stats(record));
    let field = editor.island.doc.field.clone();
    let side = field.grid.width;
    let (cx, cz) = ((side / 2) as f32, (side / 2) as f32);
    let shown = editor
        .island
        .doc
        .materials
        .as_ref()
        .unwrap()
        .dominant([cx, cz]);
    editor.paint = (editor.thumbs.iter().map(|t| t.0))
        .find(|&id| id != shown && id != 0)
        .unwrap();
    editor.tool = Tool::Paint;
    let y = field.height([cx, cz]);
    let mut drag = Drag::Brush {
        stroke: Stroke::default(),
        before: Box::new(editor.island.doc.clone()),
        level: y,
        changed: false,
        hold: None,
    };
    let (mut dabs, mut frames) = (Vec::new(), Vec::new());
    for i in 0..30 {
        let started = Instant::now();
        editor.continue_brush(&mut drag, [cx - 30. + i as f32 * 2., y, cz], false);
        editor.refresh_scene();
        dabs.push(ms(started.elapsed()));
        let [cpu, gpu] = target.time_frame_parts(&renderer).unwrap();
        frames.push(ms(cpu + gpu));
    }
    eprintln!("n_m02 paint dabs ms: {}", stats(dabs));
    eprintln!("n_m02 frame after a dab ms: {}", stats(frames));
}
