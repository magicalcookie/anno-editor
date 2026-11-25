// Tests of the mirror view a scene asks for (`Reflection`): what passes read
// of it, and that only a scene asking for one draws it.
use super::scene::Quad;
use super::*;

/// Coloured cubes above the plane at y = 0, and with `under` one more
/// lying wholly below it.
pub(super) fn cubes(renderer: &Renderer, under: bool) {
    let mut at = vec![
        math::translate([-1.5, 1.2, 0.5]),
        math::translate([1.8, 2.5, -1.]),
        math::translate([0.2, 0.6, -2.5]),
    ];
    if under {
        at.push(math::translate([0.3, -2.5, 0.8]));
    }
    renderer.set_pieces(vec![Piece {
        levels: vec![(Arc::new(cube(0xff_44_88_cc, Material::Lit)), 0.)],
        impostor: None,
        pose: None,
    }]);
    renderer.set_placements(&vec![0; at.len()], &at);
}

pub(super) fn view() -> (Camera, Extent) {
    let camera = Camera {
        fov: 0.9,
        tilt: 0.95,
        yaw: 0.35,
        ..Default::default()
    };
    let world = Extent {
        center: [0.; 3],
        size: 8.,
    };
    (camera, world)
}

// A view whose scene has a mirror at y = 0 shows, wherever a pass draws
// `mirror_reflection(pixel)`, the scene from the mirrored camera turned upside
// down: a Water-stage pass covering the view with it draws, row for row
// flipped, what the mirrored camera draws of the cubes above the plane at
// MIRROR_SCALE, filtered. A cube under the plane, which the mirrored
// camera would see first, isn't in it: the mirror clips at its plane.
// Where the mirror holds only sky it reads 0, alpha included. Pixels whose
// four mirror texels differ (edges) are left out.
#[test]
fn a_mirror_view_shows_the_scene_from_the_mirrored_camera() {
    let (camera, world) = view();
    let (w, h) = (128u32, 128u32);
    let mirrored = Renderer::default();
    let reflected = Renderer::default();
    cubes(&mirrored, false);
    cubes(&reflected, true);
    mirrored.set_scene(&Scene {
        light: neutral(),
        ..Default::default()
    });
    let target = Offscreen::new(w, h).unwrap();
    // The cube under the plane lies in the mirrored camera's view, in
    // front of its eye: unclipped, it would show.
    let below = camera
        .mirrored(0., world)
        .project([0.3, -2.5, 0.8], target.bounds(), world);
    assert!(target.bounds().contains(&below), "{below:?}");
    let (mw, mh) = (
        (w as f32 * MIRROR_SCALE) as u32,
        (h as f32 * MIRROR_SCALE) as u32,
    );
    let mut small = Offscreen::new(mw, mh).unwrap();
    let want = small
        .render(&mirrored, camera.mirrored(0., world), world)
        .unwrap();
    let shown: Arc<dyn Pass> = Quad::new(
        Stage::Water,
        FRAME_WGSL,
        "",
        "select(vec3<f32>(1.0, 0.0, 1.0), mirror_reflection(p.xy).rgb, mirror_reflection(p.xy).a > 0.5)",
    );
    reflected.set_scene(&Scene {
        light: neutral(),
        reflection: Some(Reflection { level: 0. }),
        passes: vec![shown],
        ..Default::default()
    });
    let mut target = Offscreen::new(w, h).unwrap();
    let got = target.render(&reflected, camera, world).unwrap();
    let px = |p: &[u8], w: u32, x: u32, y: u32| {
        let i = ((y * w + x) * 4) as usize;
        [p[i], p[i + 1], p[i + 2], p[i + 3]]
    };
    let (mut surfaces, mut sky, mut worst) = (0, 0, 0u8);
    for y in 0..h {
        for x in 0..w {
            let g = px(&got, w, x, y);
            // The texels the filtered read takes, about the pixel's centre
            // in the flipped, smaller mirror.
            let at = [x as f32 + 0.5, h as f32 - y as f32 - 0.5].map(|v| v * MIRROR_SCALE - 0.5);
            let [x0, y0] = at.map(|v| v.floor().max(0.) as u32);
            let texels = [(0, 0), (1, 0), (0, 1), (1, 1)]
                .map(|(dx, dy)| px(&want, mw, (x0 + dx).min(mw - 1), (y0 + dy).min(mh - 1)));
            let m = texels[0];
            if texels
                .iter()
                .any(|t| (0..4).any(|c| t[c].abs_diff(m[c]) > 1))
            {
                continue;
            }
            if m[3] == 0 {
                // Nothing in the mirror: magenta, from alpha 0.
                assert_eq!(&g[..3], &[255, 0, 255], "{x},{y}");
                sky += 1;
            } else {
                let off = (0..3).map(|c| g[c].abs_diff(m[c])).max().unwrap();
                worst = worst.max(off);
                surfaces += 1;
            }
        }
    }
    eprintln!("{surfaces} surface pixels, worst {worst} levels; {sky} sky");
    assert!(surfaces > 300 && sky > 1500, "{surfaces} {sky}");
    assert!(worst <= 2, "the mirror drew {worst} levels apart");
}

// Only a scene that asks for a mirror draws one: without it the frame is
// what it was, no slot is added, and `mirror_reflection` reads 0.
#[test]
fn a_scene_without_a_mirror_draws_none() {
    let (camera, world) = view();
    let renderer = Renderer::default();
    cubes(&renderer, true);
    let shown: Arc<dyn Pass> = Quad::new(
        Stage::Water,
        FRAME_WGSL,
        "",
        "vec3<f32>(0.25) + mirror_reflection(p.xy).rgb + vec3<f32>(mirror_reflection(p.xy).a)",
    );
    renderer.set_scene(&Scene {
        light: neutral(),
        passes: vec![shown],
        ..Default::default()
    });
    let mut target = Offscreen::new(64, 64).unwrap();
    let pixels = target.render(&renderer, camera, world).unwrap();
    let first = [pixels[0], pixels[1], pixels[2]];
    assert!(pixels.chunks(4).all(|p| p[..3] == first), "{first:?}");
    assert_eq!(renderer.lock().slots.len(), 1);
}

/// What the view's mirror kept or drew in its last frame.
fn mirror_reuse(renderer: &Renderer) -> Reuse {
    let state = renderer.lock();
    let mirror = state.slots[0].mirror.expect("a mirror");
    state.slots[mirror].drawn.reuse
}

// A still view's mirror keeps what it drew: once it has drawn under one
// key it draws nothing more, and the view then shows what a fresh
// renderer shows, byte for byte. A moved view draws it again: it shows
// the mirror from there, not the kept one.
#[test]
fn a_still_views_mirror_keeps_what_it_drew() {
    let _clock = super::CLOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (camera, world) = view();
    let shown = || -> Arc<dyn Pass> {
        Quad::new(
            Stage::Water,
            FRAME_WGSL,
            "",
            "select(vec3<f32>(1.0, 0.0, 1.0), mirror_reflection(p.xy).rgb, mirror_reflection(p.xy).a > 0.5)",
        )
    };
    let renderer = |pass: Arc<dyn Pass>| {
        let renderer = Renderer::default();
        cubes(&renderer, false);
        renderer.set_scene(&Scene {
            light: neutral(),
            reflection: Some(Reflection { level: 0. }),
            passes: vec![pass],
            ..Default::default()
        });
        renderer
    };
    let still = renderer(shown());
    let mut target = Offscreen::new(96, 96).unwrap();
    target.render(&still, camera, world).unwrap();
    let mut kept = None;
    for _ in 0..4 {
        let pixels = target.frame(&still).unwrap();
        if mirror_reuse(&still) == Reuse::Whole {
            kept = Some(pixels);
            break;
        }
    }
    let kept = kept.expect("the mirror never kept what it drew");
    target.frame(&still).unwrap();
    assert_eq!(mirror_reuse(&still), Reuse::Whole, "it stays kept");

    let fresh = |camera: Camera| {
        let mut other = Offscreen::new(96, 96).unwrap();
        other.render(&renderer(shown()), camera, world).unwrap()
    };
    assert!(
        kept == fresh(camera),
        "the kept mirror showed another frame"
    );

    let moved = Camera {
        yaw: camera.yaw + 0.2,
        ..camera
    };
    let pixels = target.render(&still, moved, world).unwrap();
    assert!(pixels != kept, "the view didn't move");
    assert!(
        pixels == fresh(moved),
        "a moved view showed the kept mirror"
    );
}
