// Island load time as the app opens one, phase by phase: the document
// (`IslandEditor::load_with` and its laps), then the background jobs on
// threads as the app's executor runs them, with frames drawn into a 5K
// window's pane at a display's pace and a window's upload budget, until
// the view is complete: every job landed, nothing uploading, every texture
// level and ground page the view wants in. The view records on a thread of
// its own, as the app's panes do, and each frame reports how long the UI
// thread was blocked on the renderer's lock. Not a pass/fail test.
//
// `ANNO117_DIR=… ANNO1800_DIR=… cargo test --release load_bench --
// --ignored --nocapture`. LOAD_ISLANDS=pirate,continental,moderate picks
// islands (all by default; moderate skips without ANNO1800_DIR). LOAD_RUNS
// (3) opens each that many times in a row in one process, as switching
// back and forth does. LOAD_COLD=1 drops the game's files from the OS file
// cache before every open (posix_fadvise, no root needed). LOAD_HZ (60)
// paces frames as the display does; 0 draws them back to back.
// LOAD_OPENING=0 keeps opening renderers to the interactive upload budget
// (gpu::OPENING_UPLOADS). LOAD_WARM=0 opens each game's first island
// without the warm-up a window device runs as GPUI makes it
// (gpu::warm_passes: the view's pipelines and the game's water and sky passes').
// LOAD_PROFILE=1 prints the kit's spans over each open, LOAD_FRAMES=1 those
// of each frame over 16.7 ms. Each open reports resident memory: before,
// its peak until complete, and settled after LOAD_SETTLE (3) more seconds
// of frames.
use super::IslandEditor;
use super::gpu_bench::{CONTINENTAL_01, MODERATE_C_01};
use super::test_support::{PIRATE, assets, install, install_1800};
use gpui::{Bounds, point, px, size};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

/// A finished background job, as the app's executor hands it back.
enum Landed {
    Models(Vec<(String, Option<super::models::Loaded>)>),
    Sky(super::SkyBaked),
    Inland(super::InlandBuilt),
    Placement(super::placements::Built),
    Rails(super::RailsBuilt),
    Ground(anyhow::Result<super::Ground>),
}

const JOBS: [&str; 6] = ["models", "sky", "inland", "placement", "rails", "ground"];

impl Landed {
    fn kind(&self) -> usize {
        match self {
            Landed::Models(_) => 0,
            Landed::Sky(_) => 1,
            Landed::Inland(_) => 2,
            Landed::Placement(_) => 3,
            Landed::Rails(_) => 4,
            Landed::Ground(_) => 5,
        }
    }
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1e3
}

/// How long a window waits for its pane's frame before drawing the last
/// one (the kit's `HANDOFF_DEADLINE`).
const HANDOFF_DEADLINE: Duration = Duration::from_millis(10);

/// One frame the view's thread recorded, and what the renderer said after.
struct ViewFrame {
    frame: usize,
    /// When the thread had submitted it and asked the renderer after.
    at: Instant,
    cpu: Duration,
    gpu: Duration,
    pages: gpui_editor_kit::gpu::PageCounts,
    waiting: Vec<&'static str>,
    pending: bool,
}

/// Drops every file under `dir` from the OS file cache: the next open reads
/// the disk, as after a reboot (clean pages only, which is all a game
/// install has).
#[cfg(target_os = "linux")]
fn evict(dir: &std::path::Path) -> usize {
    use std::os::fd::AsRawFd;
    let mut files = 0;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&d) else {
            continue;
        };
        for e in entries.flatten() {
            let Ok(kind) = e.file_type() else { continue };
            if kind.is_dir() {
                stack.push(e.path());
            } else if kind.is_file()
                && let Ok(f) = std::fs::File::open(e.path())
            {
                // SAFETY: a valid descriptor; advice only.
                unsafe { libc::posix_fadvise(f.as_raw_fd(), 0, 0, libc::POSIX_FADV_DONTNEED) };
                files += 1;
            }
        }
    }
    files
}

/// Elsewhere there's no `posix_fadvise`: nothing is evicted, so "cold"
/// opens read a warm cache.
#[cfg(not(target_os = "linux"))]
fn evict(_dir: &std::path::Path) -> usize {
    0
}

/// The process's resident memory and its peak since the last `reset_peak`,
/// in MB (Linux: /proc/self/status).
fn rss() -> Option<[f64; 2]> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    let kb = |key: &str| -> Option<f64> {
        let line = status.lines().find(|l| l.starts_with(key))?;
        line.split_whitespace().nth(1)?.parse::<f64>().ok()
    };
    Some([kb("VmRSS:")? / 1024., kb("VmHWM:")? / 1024.])
}

/// Starts the peak (VmHWM) over from the memory resident now.
fn reset_peak() {
    let _ = std::fs::write("/proc/self/clear_refs", "5");
}

/// One open of `path`, timed until complete; prints a line per phase.
fn open(
    install: &Arc<anno_formats::vfs::Install>,
    assets: &Arc<anno_formats::assets::Assets>,
    target: &mut gpui_editor_kit::gpu::Offscreen,
    path: &str,
    name: &str,
    hz: f32,
) {
    // LOAD_PROFILE=1: the kit's spans over the open, printed after it.
    let profile = std::env::var("LOAD_PROFILE").is_ok_and(|v| v == "1");
    // LOAD_FRAMES=1: the kit's spans of every frame over 16.7 ms.
    let per_frame = std::env::var("LOAD_FRAMES").is_ok_and(|v| v == "1");
    if per_frame {
        gpui_editor_kit::profile::enable();
        gpui_editor_kit::profile::take();
    }
    if profile {
        gpui_editor_kit::profile::enable();
        gpui_editor_kit::profile::report();
    }
    // LOAD_HEAP=n: the live heap's n largest owners now and at its peak,
    // after the open (crate::heap; names need the dev profile's debug info).
    let heap: Option<usize> = std::env::var("LOAD_HEAP").ok().map(|n| n.parse().unwrap());
    if heap.is_some() {
        crate::heap::start();
    }
    reset_peak();
    let before = rss();
    let started = Instant::now();
    let mut editor = IslandEditor::load_with(
        crate::games::module(crate::games::of(install)),
        install,
        path,
        Some((install.clone(), assets.clone())),
    )
    .unwrap();
    let document = started.elapsed();
    // The peak while the document loaded, and the frame resident memory
    // peaked at after.
    let document_peak = rss().map_or(0., |r| r[1]);
    let mut frame_peak = (0., 0usize);
    editor.fit_island();
    let renderer = editor.renderer.clone();
    target.aim(&renderer, editor.views[0].camera, editor.extent());

    let (tx, rx) = mpsc::channel::<Landed>();
    let spawn = |job: Box<dyn FnOnce() -> Landed + Send>| {
        let tx = tx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(job());
        });
    };
    // Bit per JOBS entry: running now.
    let mut running = 0u8;
    // Per job: started, how many, when the last landed.
    let mut jobs = [(None::<Duration>, 0usize, Duration::ZERO); 6];
    let started_job =
        |running: &mut u8, k: usize, jobs: &mut [(Option<Duration>, usize, Duration); 6]| {
            *running |= 1 << k;
            jobs[k].0.get_or_insert(started.elapsed());
            jobs[k].1 += 1;
        };
    let mut frames = 0usize;
    let mut first_frame = None;
    let mut timeline: Vec<(Duration, usize, String)> = Vec::new();
    let (mut worst_cpu, mut worst_gpu, mut worst_refresh) = (0f64, 0f64, 0f64);
    let (mut sum_cpu, mut sum_refresh) = (0f64, 0f64);
    // Per frame: the UI thread's milliseconds landing jobs, refreshing the
    // scene, blocked on the renderer's lock (inside the first two) and
    // waiting for the view's frame at the handoff.
    let mut ui: Vec<[f64; 4]> = Vec::new();
    // Frames the view had not finished by the handoff's deadline.
    let mut late = 0usize;
    // Per job, the longest its landing held the UI thread.
    let mut landings = [0f64; 6];
    // The view records on a thread of its own, as a window's pane does
    // (gpu::Viewport): each frame asks for one as it starts, the UI thread
    // lands jobs and refreshes the scene meanwhile, then waits for it up
    // to the deadline a window does and draws the last one past it.
    renderer.take_blocked();
    let (ask, asked) = mpsc::sync_channel::<usize>(1);
    let (answer, answers) = mpsc::channel::<ViewFrame>();
    let complete = std::thread::scope(|scope| {
        let view = scope.spawn({
            let renderer = renderer.clone();
            let target = &mut *target;
            move || {
                gpui_editor_kit::gpu::mark_view_thread();
                while let Ok(frame) = asked.recv() {
                    let [cpu, gpu] = target.time_frame_parts(&renderer).unwrap();
                    let _ = answer.send(ViewFrame {
                        frame,
                        at: Instant::now(),
                        cpu,
                        gpu,
                        pages: renderer.drawn().pages,
                        waiting: renderer.waiting_on(),
                        pending: renderer.pending(),
                    });
                }
            }
        });
        let mut last: Option<ViewFrame> = None;
        // The UI frame since which no job runs or lands and nothing loads.
        let mut idle_from: Option<usize> = None;
        let complete = loop {
            let frame_start = Instant::now();
            frames += 1;
            // A frame the thread is still recording keeps it busy: the ask
            // waits for it, as a window's does.
            let _ = ask.try_send(frames);
            while let Ok(landed) = rx.try_recv() {
                let k = landed.kind();
                running &= !(1 << k);
                jobs[k].2 = started.elapsed();
                let t = Instant::now();
                match landed {
                    Landed::Models(m) => editor.models_loaded(m),
                    Landed::Sky(s) => editor.sky_baked(s),
                    Landed::Inland(i) => editor.inland_built(i),
                    Landed::Placement(p) => editor.placement_built(p),
                    Landed::Rails(r) => editor.rails_built(r),
                    Landed::Ground(g) => {
                        editor.baking = false;
                        editor.set_ground(g.unwrap());
                    }
                }
                landings[k] = landings[k].max(ms(t.elapsed()));
            }
            // As the app's island_background_work: models, sky, ground bake,
            // then the scene refresh, then what it planned; the rows and rails
            // a refresh plans build on threads, as there.
            editor.background_scene = true;
            if let Some(job) = editor.models_job(install.clone()) {
                started_job(&mut running, 0, &mut jobs);
                spawn(Box::new(move || Landed::Models(job())));
            }
            if let Some(job) = editor.sky_job() {
                started_job(&mut running, 1, &mut jobs);
                spawn(Box::new(move || Landed::Sky(job())));
            }
            if editor.ground_stale
                && !editor.baking
                && let Some(job) = {
                    editor.baking = true;
                    editor.bake_job()
                }
            {
                started_job(&mut running, 5, &mut jobs);
                spawn(Box::new(move || Landed::Ground(job())));
            }
            let landing = ms(frame_start.elapsed());
            let t = Instant::now();
            editor.refresh_scene();
            editor.push_overlay();
            // As the app's frame asks whether the island still loads.
            let loading = editor.loading();
            let refresh = ms(t.elapsed());
            if let Some(job) = editor.inland_job() {
                started_job(&mut running, 2, &mut jobs);
                spawn(Box::new(move || Landed::Inland(job())));
            }
            if let Some(job) = editor.placement_job() {
                started_job(&mut running, 3, &mut jobs);
                spawn(Box::new(move || Landed::Placement(job())));
            }
            if let Some(job) = editor.rails_job() {
                started_job(&mut running, 4, &mut jobs);
                spawn(Box::new(move || Landed::Rails(job())));
            }
            let blocked = ms(renderer.take_blocked());
            let t = Instant::now();
            let deadline = t + HANDOFF_DEADLINE;
            let mut shown = false;
            while let Ok(a) =
                answers.recv_timeout(deadline.saturating_duration_since(Instant::now()))
            {
                let (cpu, gpu) = (ms(a.cpu), ms(a.gpu));
                worst_cpu = worst_cpu.max(cpu);
                worst_gpu = worst_gpu.max(gpu);
                sum_cpu += cpu;
                shown = a.frame == frames;
                last = Some(a);
                if shown {
                    break;
                }
            }
            late += usize::from(!shown);
            ui.push([landing, refresh, blocked, ms(t.elapsed())]);
            first_frame.get_or_insert(started.elapsed());
            if let Some([now, _]) = rss()
                && now > frame_peak.0
            {
                frame_peak = (now, frames);
            }
            if per_frame {
                let spans = gpui_editor_kit::profile::take();
                if landing + refresh > 16.7 {
                    let spans: Vec<String> = (spans.iter())
                        .filter(|s| s.2.as_secs_f64() > 1e-3)
                        .map(|(n, k, t)| {
                            format!(
                                "{n} {:.1}{}",
                                ms(*t),
                                if *k > 1 {
                                    format!(" x{k}")
                                } else {
                                    String::new()
                                }
                            )
                        })
                        .collect();
                    eprintln!(
                        "    frame {frames}: jobs {landing:.1}, refresh {refresh:.1} ms: {}",
                        spans.join(", ")
                    );
                }
            }
            worst_refresh = worst_refresh.max(refresh);
            sum_refresh += refresh;
            let Some(a) = &last else { continue };
            let p = a.pages;
            // What the frame still waits on, noted as it changes.
            let mut waiting = a.waiting.clone();
            if p.baked < p.wanted || p.new > 0 {
                waiting.push("ground pages");
            }
            waiting.extend(
                (0..JOBS.len())
                    .filter(|k| running & 1 << k != 0)
                    .map(|k| JOBS[k]),
            );
            let key = waiting.join(", ");
            if timeline.last().is_none_or(|(_, _, last)| *last != key) {
                timeline.push((started.elapsed(), frames, key));
            }
            let idle = running == 0
                && !editor.ground_stale
                && editor.placement_plan.is_none()
                && !editor.inland_wanted
                && !editor.rails_wanted;
            if !(idle && !loading) {
                idle_from = None;
            } else if idle_from.is_none() {
                idle_from = Some(frames);
            }
            // Complete as the view's thread finished the first whole frame
            // asked after the host's last write, on time for its window
            // frame or not: a late one shows in the window frame after.
            if idle_from.is_some_and(|f| a.frame >= f)
                && !a.pending
                && p.baked >= p.wanted
                && p.new == 0
            {
                break Some(a.at.saturating_duration_since(started));
            }
            if started.elapsed() > Duration::from_secs(60) {
                break None;
            }
            if hz > 0. {
                let next = frame_start + Duration::from_secs_f32(1. / hz);
                std::thread::sleep(next.saturating_duration_since(Instant::now()));
            }
        };
        drop(ask);
        view.join().unwrap();
        complete
    });
    let at_complete = rss();
    // Frames on for LOAD_SETTLE (3) seconds: texels nobody asks for go after
    // their hold, as in the app.
    let settle: f32 = std::env::var("LOAD_SETTLE").map_or(3., |s| s.parse().unwrap());
    let settling = Instant::now();
    while settling.elapsed().as_secs_f32() < settle {
        let frame_start = Instant::now();
        target.time_frame(&renderer).unwrap();
        if hz > 0. {
            let next = frame_start + Duration::from_secs_f32(1. / hz);
            std::thread::sleep(next.saturating_duration_since(Instant::now()));
        }
    }
    eprintln!(
        "  {name}: document {:.0} ms, first frame {:.0} ms, complete {} after {frames} frames",
        ms(document),
        first_frame.map_or(0., ms),
        complete.map_or("not within 60 s".into(), |c| format!("{:.0} ms", ms(c))),
    );
    if let (Some(b), Some(c), Some(s)) = (before, at_complete, rss()) {
        eprintln!(
            "    RSS: {:.0} MB before, peak {:.0} during the open (+{:.0}), {:.0} at complete, {:.0} settled ({settle} s on); peak over settled {:.0} MB",
            b[0],
            c[1],
            c[1] - b[0],
            c[0],
            s[0],
            c[1] - s[0]
        );
        eprintln!(
            "    RSS peaks: {document_peak:.0} MB by the document's end, {:.0} at frame {} (sampled after each frame)",
            frame_peak.0, frame_peak.1
        );
    }
    if let Some(top) = heap {
        crate::heap::report(top);
    }
    eprintln!("    document laps: {}", editor.load_log.join(", "));
    let jobs: Vec<String> = (JOBS.iter().zip(&jobs))
        .filter_map(|(n, (from, count, last))| {
            Some(format!(
                "{n} x{count} {:.0}..{:.0}",
                ms((*from)?),
                ms(*last)
            ))
        })
        .collect();
    eprintln!("    jobs (ms from open): {}", jobs.join(", "));
    for (at, frame, waiting) in &timeline {
        eprintln!(
            "    {:>6.0} ms frame {frame:>3}: waiting on {waiting}",
            ms(*at)
        );
    }
    if profile {
        gpui_editor_kit::profile::report();
    }
    // Frames a 60 Hz display would show late.
    let total = |f: &[f64; 4]| f[0] + f[1] + f[3];
    let over = ui.iter().filter(|f| total(f) > 16.7).count();
    let mut worst: Vec<(usize, &[f64; 4])> = ui.iter().enumerate().collect();
    worst.sort_by(|a, b| total(b.1).total_cmp(&total(a.1)));
    let worst: Vec<String> = (worst.iter().take(5))
        .map(|(i, f)| {
            format!(
                "frame {} {:.1} ms (jobs {:.1}, refresh {:.1}, of them blocked {:.1}, wait on 3D {:.1})",
                i + 1,
                total(f),
                f[0],
                f[1],
                f[2],
                f[3]
            )
        })
        .collect();
    eprintln!(
        "    UI thread: {over} of {} frames over 16.7 ms; worst {}",
        ui.len(),
        worst.join(", ")
    );
    // The UI thread blocked on the renderer's lock, every frame that was.
    let mut blocked: Vec<f64> = ui.iter().map(|f| f[2]).collect();
    let stalls: Vec<String> = (blocked.iter().enumerate())
        .filter(|(_, b)| **b >= 0.05)
        .map(|(i, b)| format!("{}:{b:.1}", i + 1))
        .collect();
    blocked.sort_by(f64::total_cmp);
    eprintln!(
        "    UI blocked on the renderer: max {:.2} ms, {} of {} frames over 2 ms; per frame (frame:ms, 0.05 ms and up) [{}]",
        blocked.last().copied().unwrap_or(0.),
        blocked.iter().filter(|b| **b > 2.).count(),
        ui.len(),
        stalls.join(" ")
    );
    eprintln!(
        "    view frames past the handoff deadline: {late} of {}",
        ui.len()
    );
    let landings: Vec<String> = (JOBS.iter().zip(landings))
        .filter(|(_, t)| *t > 0.)
        .map(|(n, t)| format!("{n} {t:.1} ms"))
        .collect();
    eprintln!("    longest landing: {}", landings.join(", "));
    eprintln!(
        "    UI thread per frame: refresh worst {worst_refresh:.1} ms (sum {sum_refresh:.0}); view thread: recording worst {worst_cpu:.1} ms (sum {sum_cpu:.0}), GPU worst {worst_gpu:.1} ms"
    );
}

#[test]
#[ignore]
fn load_bench() {
    let env = |k: &str| std::env::var(k).ok();
    let picked = env("LOAD_ISLANDS").unwrap_or_else(|| "pirate,continental,moderate".into());
    let runs: usize = env("LOAD_RUNS").map_or(3, |s| s.parse().unwrap());
    let cold = env("LOAD_COLD").is_some_and(|v| v == "1");
    let warm = env("LOAD_WARM").is_none_or(|v| v != "0");
    let hz: f32 = env("LOAD_HZ").map_or(60., |s| s.parse().unwrap());
    // LOAD_OPENING=0: opening renderers upload as interactive frames do.
    gpui_editor_kit::gpu::OPENING_UPLOADS.store(
        env("LOAD_OPENING").is_none_or(|v| v != "0"),
        std::sync::atomic::Ordering::Relaxed,
    );
    // The app's island pane on a 5K screen, as gpu_bench draws it.
    let (w, h, scale) = (1830f32, 1312f32, 2f32);
    let mut target = gpui_editor_kit::gpu::Offscreen::new(5120, 2880)
        .unwrap()
        .with_scale(scale)
        .with_viewport(Bounds::new(point(px(262.), px(64.)), size(px(w), px(h))))
        .with_window_budget();
    let mut games: Vec<(
        bool,
        Arc<anno_formats::vfs::Install>,
        Arc<anno_formats::assets::Assets>,
    )> = Vec::new();
    for name in picked.split(',') {
        let (game_1800, path) = match name {
            "pirate" => (false, PIRATE),
            "continental" => (false, CONTINENTAL_01),
            "moderate" => (true, MODERATE_C_01),
            other => panic!("LOAD_ISLANDS: no island {other}"),
        };
        // Once per game, as the app opens the install and its assets.
        if !games.iter().any(|g| g.0 == game_1800) {
            let t = Instant::now();
            let Some(install) = (if game_1800 { install_1800() } else { install() }) else {
                eprintln!("{name}: skipped, no install");
                continue;
            };
            let opened = t.elapsed();
            let t = Instant::now();
            let assets = assets(&install);
            eprintln!(
                "{} install: index {:.0} ms, assets {:.0} ms (once per game)",
                if game_1800 { "1800" } else { "117" },
                ms(opened),
                ms(t.elapsed())
            );
            if warm {
                // In the app this runs while islands are picked.
                let t = Instant::now();
                let module = crate::games::module(crate::games::of(&install));
                target.warm(|gpu| {
                    anno_render::a7m::warm(gpu);
                    if let Some(scene) = module.scene() {
                        scene.warm(&install, gpu)
                    }
                });
                eprintln!(
                    "  warm-up (while islands are picked): {:.0} ms",
                    ms(t.elapsed())
                );
            }
            games.push((game_1800, Arc::new(install), assets));
        }
        let (_, install, assets) = games.iter().find(|g| g.0 == game_1800).unwrap().clone();
        for run in 1..=runs {
            let evicted = cold.then(|| evict(install.dir()));
            let label = match evicted {
                Some(n) => format!("{name} run {run}, cold ({n} files evicted)"),
                None => format!("{name} run {run}"),
            };
            open(&install, &assets, &mut target, path, &label, hz);
        }
    }
}
