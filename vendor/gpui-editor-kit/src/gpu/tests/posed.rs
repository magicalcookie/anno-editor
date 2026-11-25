// Tests of posed placements (`Pose`): they move by the clock, skinned
// vertices follow their joints, the cull takes the
// pose's box, and a still view keeps its layers while they move over them.
use super::*;

/// A pose of one joint over 4 s: the whole mesh at `x` metres along x in
/// each of its frames, one a second.
fn slide(xs: &[f32]) -> Arc<Pose> {
    let at = |x: f32| [[1., 0., 0., x], [0., 1., 0., 0.], [0., 0., 1., 0.]];
    let (lo, hi) = (
        xs.iter().fold(0f32, |a, &b| a.min(b)),
        xs.iter().fold(0f32, |a, &b| a.max(b)),
    );
    Arc::new(Pose {
        frames: xs.iter().map(|&x| at(x)).collect(),
        joints: 1,
        period: xs.len() as f32,
        phased: false,
        origin: [0.; 3],
        bounds: [[lo - 0.5, -0.5, -0.5], [hi + 0.5, 0.5, 0.5]],
    })
}

fn hold(t: f32) {
    HELD_CLOCK.store(t.to_bits(), std::sync::atomic::Ordering::Relaxed);
}

/// Red stands out of the lit grey ground and the green cube.
fn red(p: [u8; 3]) -> bool {
    p[0] > p[1].saturating_add(40) && p[0] > p[2].saturating_add(40)
}

/// `still_scene` without sun shadows, a green cube placed still and a red
/// one at (3, 0.5, 1) posed with `pose`, in view.
fn posed_view(pose: Arc<Pose>) -> Still {
    let renderer = Renderer::default();
    let mut scene = still_scene(0., [0.4, 1., 0.3]);
    scene.light.shadows = None;
    renderer.set_scene(&scene);
    renderer.set_pieces(vec![
        Piece {
            levels: vec![(Arc::new(cube(0x40c060, Material::Lit)), 0.)],
            impostor: None,
            pose: None,
        },
        Piece {
            levels: vec![(Arc::new(cube(0xff2020, Material::Lit)), 0.)],
            impostor: None,
            pose: Some(pose),
        },
    ]);
    let rows = [
        math::translate([-3., 0.5, -2.]),
        math::translate([3., 0.5, 1.]),
    ];
    renderer.set_placements(&[0, 1], &rows);
    let mut target = Offscreen::new(240, 180).unwrap();
    let world = Extent {
        center: [0.; 3],
        size: 16.,
    };
    let camera = Camera {
        fov: 0.9,
        zoom: 1.5,
        ..Camera::perspective()
    };
    target.aim(&renderer, camera, world);
    target.settle(&renderer).unwrap();
    Still {
        renderer,
        target,
        camera,
        world,
        scene,
    }
}

// A posed cube slides by the clock. Once the view rests it
// keeps its layers and draws the cube alone again: the cube still moves,
// picks find it where it is, what it left shows the ground the layers
// hold, and the frame is the one a full redraw draws.
#[test]
fn posed_placements_move_over_the_layers_a_still_view_keeps() {
    let _clock = CLOCK.lock().unwrap_or_else(|e| e.into_inner());
    hold(0.);
    let mut still = posed_view(slide(&[0., 1., 0., -1.]));
    let pixel = |s: &Still, pixels: &[u8], p: Vec3| {
        s.target
            .pixel(pixels, s.camera.project(p, s.target.bounds(), s.world))
    };
    let (here, there) = ([3., 1., 1.], [4.2, 1., 1.]);
    let pixels = still.rest();
    assert!(red(pixel(&still, &pixels, here)), "at rest at 0 s");
    assert!(!red(pixel(&still, &pixels, there)));
    hold(1.);
    let (moved, reuse) = still.frame();
    assert_eq!(reuse, Reuse::Whole, "the view keeps its layers");
    assert!(red(pixel(&still, &moved, there)), "slid at 1 s");
    assert!(!red(pixel(&still, &moved, here)), "left the ground");
    // Picked where it now is, the kept ground where it was.
    let (camera, bounds, world) = (still.camera, still.target.bounds(), still.world);
    let at = |p: Vec3| camera.project(p, bounds, world);
    let hit = still.target.pick(&still.renderer, at(there)).unwrap();
    assert_eq!(hit.and_then(|h| h.placement), Some(1));
    let hit = still.target.pick(&still.renderer, at(here)).unwrap();
    assert_eq!(hit.and_then(|h| h.placement), None);
    assert_eq!(still.renderer.drawn().reuse, Reuse::Whole);
    // Halfway between frames, halfway between places.
    hold(0.5);
    let (half, _) = still.frame();
    assert!(red(pixel(&still, &half, [3.6, 1., 1.])));
    hold(1.);
    let (kept, _) = still.frame();
    let full = still.redrawn();
    let (off, most) = difference(&kept, &full);
    assert!(
        off == 0,
        "kept layers under the posed cube drew {off} pixels apart, by up to {most}"
    );
    hold(f32::NAN);
}

// The cull keeps a posed placement wherever its pose's box reaches: a cube
// that slides 6 m along x draws when the view sees only where it slid to.
#[test]
fn posed_placements_are_culled_by_their_poses_box() {
    let _clock = CLOCK.lock().unwrap_or_else(|e| e.into_inner());
    hold(1.);
    let mut still = posed_view(slide(&[0., 6.]));
    let world = Extent {
        center: [9., 0.5, 1.],
        size: 2.,
    };
    let camera = Camera {
        fov: 0.9,
        zoom: 1.,
        ..Camera::perspective()
    };
    still.target.aim(&still.renderer, camera, world);
    let pixels = still.target.settle(&still.renderer).unwrap();
    let at = camera.project([9., 1., 1.], still.target.bounds(), world);
    assert!(red(still.target.pixel(&pixels, at)), "the slid cube draws");
    hold(f32::NAN);
}

// A skinned mesh, two cubes on two joints: the one on joint 1 follows it
// 2 m along z at 1 s, the one on joint 0 stays.
#[test]
fn skinned_vertices_follow_their_joints() {
    let _clock = CLOCK.lock().unwrap_or_else(|e| e.into_inner());
    let one = cube(0xff2020, Material::Lit).data().clone();
    let mut data = MeshData {
        material: Material::Lit,
        ..Default::default()
    };
    for (joint, x) in [(0u8, -1f32), (1, 1.)] {
        let base = data.positions.len() as u32;
        data.positions
            .extend(one.positions.iter().map(|p| [p[0] + x, p[1], p[2]]));
        data.normals.extend(&one.normals);
        data.colors.extend(&one.colors);
        data.indices.extend(one.indices.iter().map(|i| base + i));
        data.skin.extend(std::iter::repeat_n(
            [joint, 0, 0, 0, 255, 0, 0, 0],
            one.positions.len(),
        ));
    }
    let still = [[1., 0., 0., 0.], [0., 1., 0., 0.], [0., 0., 1., 0.]];
    let along = [[1., 0., 0., 0.], [0., 1., 0., 0.], [0., 0., 1., 2.]];
    let pose = Arc::new(Pose {
        frames: vec![still, still, still, along],
        joints: 2,
        period: 2.,
        phased: false,
        origin: [0.; 3],
        bounds: [[-1.5, -0.5, -0.5], [1.5, 0.5, 2.5]],
    });
    hold(1.);
    let renderer = Renderer::default();
    renderer.set_scene(&Scene {
        light: neutral(),
        ..Default::default()
    });
    renderer.set_pieces(vec![Piece {
        levels: vec![(Arc::new(Mesh::compact(data.clone()).unwrap()), 0.)],
        impostor: None,
        pose: Some(pose.clone()),
    }]);
    renderer.set_placements(&[0], &[math::IDENTITY]);
    let mut target = Offscreen::new(200, 160).unwrap();
    let world = Extent {
        center: [0., 0., 1.],
        size: 6.,
    };
    let camera = Camera {
        fov: 0.9,
        zoom: 1.5,
        ..Camera::perspective()
    };
    target.aim(&renderer, camera, world);
    let pixels = target.settle(&renderer).unwrap();
    let at = |p: Vec3| target.pixel(&pixels, camera.project(p, target.bounds(), world));
    assert!(red(at([-1., 0.5, 0.])), "joint 0's cube stays");
    assert!(red(at([1., 0.5, 2.])), "joint 1's cube moved");
    assert!(!red(at([1., 0.5, 0.])), "and left its place");
    hold(f32::NAN);
}
