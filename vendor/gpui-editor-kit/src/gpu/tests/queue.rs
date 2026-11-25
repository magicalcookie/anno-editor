// Tests of the host's calls while a pane's thread holds the renderer's state
// to record a frame: writes queue and the next frame applies them, reads
// answer at once, and what the frames draw is what writing straight into
// the state drew.
use super::*;
use std::time::{Duration, Instant};

fn pieces() -> Vec<Piece> {
    [0xff0000, 0x2060ff]
        .map(|color| Piece {
            levels: vec![(Arc::new(cube(color, Material::Flat)), 0.)],
            impostor: None,
            pose: None,
        })
        .into()
}

fn view() -> (Camera, Extent) {
    let world = Extent {
        center: [0.; 3],
        size: 8.,
    };
    (Camera::perspective(), world)
}

const RING: Marker = Marker {
    position: [0.; 3],
    radius: 6.,
    color: 0xffd479,
    shape: Shape::Ring,
};

/// Holds `renderer`'s state on a thread of its own for `hold`, as a pane's
/// thread does while it records a heavy frame; returns once it is held.
fn hold(renderer: &Arc<Renderer>, hold: Duration) -> std::thread::JoinHandle<()> {
    let (held, wait) = std::sync::mpsc::channel();
    let renderer = renderer.clone();
    let thread = std::thread::spawn(move || {
        let state = renderer.state.lock().unwrap();
        held.send(()).unwrap();
        std::thread::sleep(hold);
        drop(state);
    });
    wait.recv().unwrap();
    thread
}

// Everything an island's opening and editing call on the UI thread, made
// while a frame holds the state for 400 ms: none of it waits, and the
// frames after draw what writing into a free state, each write applied at
// once, draws.
#[test]
fn the_hosts_calls_never_wait_for_a_frame_and_draw_as_written() {
    let (camera, world) = view();
    let mut target = Offscreen::new(160, 120).unwrap();
    let center = camera.project([0.; 3], target.bounds(), world);
    let rows = [math::IDENTITY, math::translate([1.5, 0., 0.])];
    let moved = [math::translate([-1.5, 0., 0.5])];
    let lit = || Scene {
        light: neutral(),
        lines: Arc::new(vec![Line {
            points: vec![[-2., 0., 2.], [2., 0., 2.]],
            width: 2.,
            color: 0xffffffff,
        }]),
        ..Default::default()
    };
    // Writes the host makes as an island opens and a selection drags; with
    // `apply`, each goes into the state before the next.
    let write = |r: &Renderer, apply: bool| {
        let writes: [&dyn Fn(); 9] = [
            &|| r.set_scene(&lit()),
            &|| r.set_pieces(pieces()),
            &|| r.set_placements(&[0, 1], &rows),
            &|| r.set_instance_words(0, &[3, 4]),
            &|| r.set_selection(vec![1], &[(1, [0.; 3])], &RING),
            &|| r.drag_selection(Some(math::translate([0., 0.5, 0.]))),
            &|| r.move_placements(1, &moved),
            &|| r.drag_selection(None),
            &|| {
                r.set_overlay(vec![Line {
                    points: vec![[-2., 0., -2.], [2., 0., -2.]],
                    width: 2.,
                    color: 0xff00ffff,
                }])
            },
        ];
        for w in writes {
            w();
            if apply {
                drop(r.lock());
            }
        }
    };
    // Both start from a frame of an empty scene, so reads have a frame's
    // answers to give and both frames have the same history behind them.
    let first = |r: &Renderer, target: &mut Offscreen| {
        r.set_scene(&Scene {
            light: neutral(),
            ..Default::default()
        });
        target.aim(r, camera, world);
        target.settle(r).unwrap();
    };

    let direct = Renderer::default();
    first(&direct, &mut target);
    write(&direct, true);
    let expected = target.settle(&direct).unwrap();

    let renderer = Arc::new(Renderer::default());
    first(&renderer, &mut target);
    renderer.take_blocked();
    let held = hold(&renderer, Duration::from_millis(400));
    let started = Instant::now();
    write(&renderer, false);
    assert!(renderer.pending(), "writes no frame took are pending");
    assert!(!renderer.settled());
    assert!(!renderer.waiting_on().is_empty());
    let _ = (renderer.stats(), renderer.drawn(), renderer.families());
    assert!(renderer.wants_frames());
    assert_eq!(
        renderer.pick(0, center),
        None,
        "a new cursor answers next frame"
    );
    let took = started.elapsed();
    held.join().unwrap();
    assert!(
        took < Duration::from_millis(100),
        "the host's calls took {took:?} while a frame held the state"
    );
    assert_eq!(renderer.take_blocked(), Duration::ZERO);

    let pixels = target.settle(&renderer).unwrap();
    let (off, most) = difference(&expected, &pixels);
    assert!(
        off == 0,
        "{off} pixels off by up to {most} from writing into a free state"
    );
    // The pick asked while the state was held was read back since.
    let hit = renderer.pick(0, center).expect("a pick under the cursor");
    assert_eq!((hit.placement, hit.id), (Some(0), None));
}

/// A small deterministic generator for the write sequences.
struct Lcg(u64);

impl Lcg {
    fn next(&mut self, below: u32) -> u32 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        ((self.0 >> 33) % u64::from(below.max(1))) as u32
    }

    fn offset(&mut self) -> Mat4 {
        let mut at = || self.next(9) as f32 * 0.5 - 2.;
        math::translate([at(), 0., at()])
    }
}

/// `n` placement writes the host could make in a row, valid for the
/// table each leaves: tables, moves, words, selections, drags, and a new
/// piece list now and then.
fn writes(seed: u64, n: usize) -> Vec<Box<dyn Fn(&Renderer)>> {
    let mut rng = Lcg(seed);
    let mut rows = 0u32;
    let mut out: Vec<Box<dyn Fn(&Renderer)>> = Vec::new();
    for _ in 0..n {
        let kind = if rows == 0 { 0 } else { rng.next(12) };
        match kind {
            0..=2 => {
                rows = 1 + rng.next(5);
                let piece_of: Vec<u32> = (0..rows).map(|_| rng.next(2)).collect();
                let transforms: Vec<Mat4> = (0..rows).map(|_| rng.offset()).collect();
                if rng.next(2) == 0 {
                    out.push(Box::new(move |r| r.set_placements(&piece_of, &transforms)));
                } else {
                    // Generous bounds, the same both ways.
                    let bounds = [[-4., -2., -4.], [4., 2., 4.]];
                    out.push(Box::new(move |r| {
                        r.set_placements_bounded(piece_of.clone(), transforms.clone(), bounds)
                    }));
                }
            }
            3 | 4 => {
                let first = rng.next(rows);
                let moved: Vec<Mat4> = (first..rows).map(|_| rng.offset()).collect();
                out.push(Box::new(move |r| r.move_placements(first, &moved)));
            }
            5 => {
                let first = rng.next(rows);
                let words: Vec<u32> = (first..rows).map(|_| rng.next(100)).collect();
                out.push(Box::new(move |r| r.set_instance_words(first, &words)));
            }
            6 | 7 => {
                let picked: Vec<u32> = (0..rows).filter(|_| rng.next(2) == 0).collect();
                let marks: Vec<(u32, Vec3)> = picked.iter().map(|&r| (r, [0.; 3])).collect();
                out.push(Box::new(move |r| {
                    r.set_selection(picked.clone(), &marks, &RING)
                }));
            }
            8 | 9 => {
                let delta = rng.offset();
                out.push(Box::new(move |r| r.drag_selection(Some(delta))));
            }
            10 => out.push(Box::new(|r| r.drag_selection(None))),
            _ => {
                rows = 0;
                out.push(Box::new(|r| r.set_pieces(pieces())));
            }
        }
    }
    out
}

// Writes a frame takes together, folded where a later one replaces an
// earlier (`Inputs::queue_placed`), leave the table, the selection and
// the frame as applying each at once does.
#[test]
fn queued_placement_writes_draw_as_applying_each_at_once_does() {
    let (camera, world) = view();
    let mut target = Offscreen::new(96, 64).unwrap();
    for seed in 0..24 {
        let mut draw = |queued: bool| {
            let renderer = Renderer::default();
            renderer.set_scene(&Scene {
                light: neutral(),
                ..Default::default()
            });
            renderer.set_pieces(pieces());
            drop(renderer.lock());
            for write in writes(seed, 14) {
                write(&renderer);
                if !queued {
                    drop(renderer.lock());
                }
            }
            let placed = {
                let state = renderer.lock();
                let p = &state.assets.placed;
                let s = &p.selected;
                format!(
                    "{} rows, bounds {:?}, adapting {:?}, selected {:?} {:?} {:?} {} drag {:?}",
                    p.count, p.bounds, p.adapting, s.rows, s.marks, s.color, s.radius, s.drag
                )
            };
            target.aim(&renderer, camera, world);
            (placed, target.settle(&renderer).unwrap())
        };
        let (each, each_pixels) = draw(false);
        let (queued, queued_pixels) = draw(true);
        assert_eq!(each, queued, "seed {seed}");
        let (off, most) = difference(&each_pixels, &queued_pixels);
        assert!(off == 0, "seed {seed}: {off} pixels off by up to {most}");
    }
}

// Writes queued when the device is lost go to the new device's first
// frame, which draws them as a renderer that never lost its device does.
#[test]
fn writes_queued_as_the_device_is_lost_draw_on_the_new_device() {
    let (camera, world) = view();
    let opening = |r: &Renderer| {
        r.set_scene(&Scene {
            light: neutral(),
            ..Default::default()
        });
        r.set_pieces(pieces());
        r.set_placements(&[0, 1], &[math::IDENTITY, math::translate([1.5, 0., 0.])]);
    };
    let edits = |r: &Renderer| {
        r.move_placements(1, &[math::translate([-1.5, 0., 0.5])]);
        r.set_selection(vec![0], &[(0, [0.; 3])], &RING);
    };
    let still = |target: &mut Offscreen, r: &Renderer| {
        target.aim(r, camera, world);
        target.frame(r).unwrap();
        let mut pixels = target.settle(r).unwrap();
        for _ in 0..8 {
            let again = target.frame(r).unwrap();
            if again == pixels {
                break;
            }
            pixels = again;
        }
        pixels
    };

    let renderer = Renderer::default();
    opening(&renderer);
    let mut lost = Offscreen::on_new_device(96, 64).unwrap();
    still(&mut lost, &renderer);
    edits(&renderer);
    assert!(renderer.pending(), "queued, not yet taken by a frame");
    lost.lose();
    drop(lost);
    let mut target = Offscreen::new(96, 64).unwrap();
    let after = still(&mut target, &renderer);

    let kept = Renderer::default();
    opening(&kept);
    still(&mut target, &kept);
    edits(&kept);
    let expected = still(&mut target, &kept);
    let (off, most) = difference(&expected, &after);
    assert!(
        off == 0,
        "{off} pixels off by up to {most} on the new device"
    );
}
