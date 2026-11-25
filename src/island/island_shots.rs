// Screenshot regression over every shipped island of each game: each
// island opens as the Islands pane opens it, its background jobs run to the
// end, one frame from the opening camera is drawn offscreen and compared
// with the reference image of the last accepted run.
//
// `ANNO117_DIR=… ANNO1800_DIR=… ISLAND_SHOTS=<dir>
// cargo test --release island_shots -- --ignored --nocapture`, ANNO2070_DIR
// for 2070. A game is skipped without its *_DIR, or left out of
// ISLAND_GAMES (117,1800,2070). References live in `$ISLAND_SHOTS/ref/<game>/
// <virtual path>.png`; an island without one gets it written and passes.
// A mismatch writes expected, actual, diff and one side-by-side compare
// image under `$ISLAND_SHOTS/fail/`, and the test fails at the end listing
// them all. ISLAND_SHOTS_UPDATE=1 rewrites every reference (accept the
// current look). ISLAND_FILTER keeps paths containing it. ISLAND_SHOTS_SIZE
// (1280x720) is the target in pixels. ISLAND_SHOTS_TOLERANCE (0.5) is the
// share of pixels in percent allowed to move by more than 8 of 255 in any
// channel, the same "an eye could see it" cutoff gpu_bench_compare prints.
//
// Not run with the suite: it loads every island's models, minutes per game.
// The renderer clock is a process global (HELD_CLOCK), so this test must
// not share the process with gpu_bench or water_shots.
use super::IslandEditor;
use super::gpu_bench::settle;
use super::test_support::{assets, install, install_1800};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// How far two images are apart.
struct Diff {
    /// Percent of pixels any channel of which moved by more than 8.
    visible: f64,
    mean: f64,
    max: u8,
    /// Per-channel difference ×8, to make small shifts visible.
    image: image::RgbImage,
}

fn diff(a: &image::RgbImage, b: &image::RgbImage) -> Diff {
    let mut image = image::RgbImage::new(a.width(), a.height());
    let (mut sum, mut max, mut visible) = (0f64, 0u8, 0usize);
    for ((p, q), d) in a.pixels().zip(b.pixels()).zip(image.pixels_mut()) {
        let mut worst = 0u8;
        for ch in 0..3 {
            let e = p[ch].abs_diff(q[ch]);
            sum += f64::from(e);
            worst = worst.max(e);
            d[ch] = e.saturating_mul(8);
        }
        max = max.max(worst);
        visible += usize::from(worst > 8);
    }
    let pixels = f64::from(a.width() * a.height());
    Diff {
        visible: visible as f64 / pixels * 100.,
        mean: sum / (pixels * 3.),
        max,
        image,
    }
}

/// Expected | actual | diff in one image, for opening a single file.
fn side_by_side(
    expected: &image::RgbImage,
    actual: &image::RgbImage,
    diff: &image::RgbImage,
) -> image::RgbImage {
    let (w, h) = actual.dimensions();
    let mut out = image::RgbImage::new(w * 3, h);
    image::imageops::replace(&mut out, expected, 0, 0);
    image::imageops::replace(&mut out, actual, i64::from(w), 0);
    image::imageops::replace(&mut out, diff, i64::from(w) * 2, 0);
    out
}

/// One island drawn from its opening camera, in the reference conditions:
/// noon, clock and waves held, the sea at 4 s, default graphics but for
/// animation.
fn shoot(
    install: &Arc<anno_formats::vfs::Install>,
    assets: &Arc<anno_formats::assets::Assets>,
    target: &mut gpui_editor_kit::gpu::Offscreen,
    (w, h): (u32, u32),
    path: &str,
) -> anyhow::Result<image::RgbImage> {
    let hold = |t: f32| {
        gpui_editor_kit::gpu::HELD_CLOCK.store(t.to_bits(), std::sync::atomic::Ordering::Relaxed);
    };
    let mut editor = IslandEditor::load_with(
        crate::games::module(crate::games::of(install)),
        install,
        path,
        Some((install.clone(), assets.clone())),
    )?;
    // Defaults, not the user's graphics.json: two machines must agree.
    // Motion off, as the baselines draw (baselines.rs `shot`).
    editor.set_graphics(crate::graphics::Graphics {
        animation: false,
        ..Default::default()
    });
    editor.layers.harbours = true;
    hold(4.);
    editor.time_of_day = 12.;
    editor.sky = None;
    editor.scene_stale = true;
    settle(&mut editor, install);
    let extent = editor.extent();
    let camera = editor.views[0].camera;
    let renderer = editor.renderer.clone();
    // From here on as gpu_bench's reference frame, which is stable build to
    // build: pipelines and uploads first, then the sea held, a fixed number
    // of frames for the ground pages, and exposure taken after pauses.
    target.render(&renderer, camera, extent)?;
    for _ in 0..8 {
        target.frame(&renderer)?;
    }
    editor.layers.waves = false;
    editor.scene_stale = true;
    editor.refresh_scene();
    super::water_shots::show_water_at(&editor, 4.);
    for _ in 0..48 {
        target.frame(&renderer)?;
    }
    let p = renderer.drawn().pages;
    if p.baked < p.wanted {
        eprintln!("{path}: {} of {} ground pages baked", p.baked, p.wanted);
    }
    let mut pixels = Vec::new();
    for _ in 0..4 {
        std::thread::sleep(std::time::Duration::from_millis(150));
        pixels = target.frame(&renderer)?;
    }
    hold(f32::NAN);
    let rgba =
        image::RgbaImage::from_raw(w, h, pixels).ok_or_else(|| anyhow::anyhow!("readback size"))?;
    Ok(image::DynamicImage::ImageRgba8(rgba).to_rgb8())
}

#[test]
#[ignore]
fn island_shots() {
    let env = |k: &str| std::env::var(k).ok();
    let root = PathBuf::from(env("ISLAND_SHOTS").expect("ISLAND_SHOTS=dir"));
    let update = env("ISLAND_SHOTS_UPDATE").is_some_and(|v| v == "1");
    let filter = env("ISLAND_FILTER").unwrap_or_default();
    let tolerance: f64 = env("ISLAND_SHOTS_TOLERANCE").map_or(0.5, |s| s.parse().unwrap());
    let (w, h): (u32, u32) = env("ISLAND_SHOTS_SIZE").map_or((1280, 720), |s| {
        let (w, h) = s.split_once('x').expect("WxH");
        (w.parse().unwrap(), h.parse().unwrap())
    });
    let fail_dir = root.join("fail");
    // Last run's failures would read as this run's.
    let _ = std::fs::remove_dir_all(&fail_dir);
    let mut target = gpui_editor_kit::gpu::Offscreen::new(w, h).unwrap();
    let mut failures: Vec<String> = Vec::new();
    let (mut passed, mut recorded) = (0usize, 0usize);
    // ISLAND_GAMES (117,1800,2070) picks the games; each needs its *_DIR.
    let games = env("ISLAND_GAMES").unwrap_or_else(|| "117,1800,2070".into());
    let install_2070 = || {
        let dir = anno_formats::vfs::install_dir("ANNO2070_DIR")?;
        let families = crate::games::profile(anno_core::game::Game::Anno2070).archive_families;
        Some(anno_formats::vfs::Install::open(dir, families).unwrap())
    };
    for (name, install) in [
        ("117", install as fn() -> Option<anno_formats::vfs::Install>),
        ("1800", install_1800),
        ("2070", install_2070),
    ] {
        if !games.split(',').any(|g| g == name) {
            continue;
        }
        let Some(install) = install() else {
            eprintln!("{name}: skipped, no install");
            continue;
        };
        let install = Arc::new(install);
        let assets = assets(&install);
        let game = crate::games::of(&install);
        // The list the Islands pane shows.
        let listed: Vec<String> =
            (super::islands(crate::games::module(game), &install, Some(assets.as_ref())))
                .into_iter()
                .map(|s| s.path)
                .collect();
        let mut islands: Vec<String> = listed.into_iter().filter(|p| p.contains(&filter)).collect();
        islands.sort();
        islands.dedup();
        eprintln!("{name}: {} islands", islands.len());
        for (i, path) in islands.iter().enumerate() {
            let rel = Path::new(name).join(path.trim_end_matches(".a7m").trim_end_matches(".isd"));
            let reference = root.join("ref").join(&rel).with_extension("png");
            let started = std::time::Instant::now();
            let actual = match shoot(&install, &assets, &mut target, (w, h), path) {
                Ok(img) => img,
                Err(e) => {
                    failures.push(format!("{name} {path}: did not load or draw: {e:#}"));
                    continue;
                }
            };
            let took = started.elapsed().as_secs_f32();
            let expected = (!update)
                .then(|| image::open(&reference).ok())
                .flatten()
                .map(|i| i.to_rgb8());
            let Some(expected) = expected else {
                std::fs::create_dir_all(reference.parent().unwrap()).unwrap();
                actual.save(&reference).unwrap();
                recorded += 1;
                eprintln!(
                    "[{}/{}] {path}: recorded ({took:.1} s)",
                    i + 1,
                    islands.len()
                );
                continue;
            };
            let out = |kind: &str| {
                let p = fail_dir.join(&rel);
                let stem = p.file_name().unwrap().to_string_lossy().into_owned();
                p.with_file_name(format!("{stem}.{kind}.png"))
            };
            if expected.dimensions() != actual.dimensions() {
                let a = out("actual");
                std::fs::create_dir_all(a.parent().unwrap()).unwrap();
                actual.save(&a).unwrap();
                failures.push(format!(
                    "{name} {path}: size {:?}, reference {:?} (ISLAND_SHOTS_SIZE changed? \
                     rerun with ISLAND_SHOTS_UPDATE=1)\n  expected {}\n  actual   {}",
                    actual.dimensions(),
                    expected.dimensions(),
                    reference.display(),
                    a.display()
                ));
                continue;
            }
            let d = diff(&expected, &actual);
            eprintln!(
                "[{}/{}] {path}: {:.3}% visible, mean {:.3}, max {} ({took:.1} s)",
                i + 1,
                islands.len(),
                d.visible,
                d.mean,
                d.max
            );
            if d.visible <= tolerance {
                passed += 1;
                continue;
            }
            let (e, a, x, c) = (out("expected"), out("actual"), out("diff"), out("compare"));
            std::fs::create_dir_all(e.parent().unwrap()).unwrap();
            expected.save(&e).unwrap();
            actual.save(&a).unwrap();
            d.image.save(&x).unwrap();
            side_by_side(&expected, &actual, &d.image).save(&c).unwrap();
            failures.push(format!(
                "{name} {path}: {:.3}% of pixels differ (allowed {tolerance}%), mean {:.3}, max {}\n  \
                 expected {}\n  actual   {}\n  diff     {}\n  compare  {}",
                d.visible,
                d.mean,
                d.max,
                e.display(),
                a.display(),
                x.display(),
                c.display()
            ));
        }
    }
    eprintln!(
        "{passed} matched, {recorded} recorded, {} failed",
        failures.len()
    );
    assert!(
        failures.is_empty(),
        "{} islands differ from their references:\n{}",
        failures.len(),
        failures.join("\n")
    );
}
