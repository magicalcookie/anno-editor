// Tests of a renderer drawing on after its device is lost: the next frames
// come on a new device, and what the renderer and its passes made on the
// lost one is made again there from what they keep on the CPU.
use super::scene::Quad;
use super::*;

/// Draws until the frame holds still: settled, then until two frames in a
/// row match (the impostor bake lands a frame after the arena), at most a
/// few. One frame first: until a frame comes on the new device, the
/// renderer can't know its copies are gone, and reports nothing pending.
fn still(target: &mut Offscreen, renderer: &Renderer, camera: Camera, world: Extent) -> Vec<u8> {
    target.aim(renderer, camera, world);
    target.frame(renderer).unwrap();
    let mut pixels = target.settle(renderer).unwrap();
    for _ in 0..8 {
        let again = target.frame(renderer).unwrap();
        if again == pixels {
            break;
        }
        pixels = again;
    }
    pixels
}

/// Draws `renderer` still on `target`, loses the target's device, and
/// draws it still again on a target on a new device, which it returns for
/// the next case: the two frames must match byte for byte.
fn across_a_loss(
    target: Offscreen,
    renderer: &Renderer,
    (camera, world): (Camera, Extent),
    case: &str,
) -> Offscreen {
    let mut target = target;
    let before = still(&mut target, renderer, camera, world);
    let (w, h) = (target.bounds().size.width, target.bounds().size.height);
    target.lose();
    drop(target);
    let mut target = Offscreen::on_new_device(f32::from(w) as u32, f32::from(h) as u32).unwrap();
    let after = still(&mut target, renderer, camera, world);
    let (off, most) = difference(&before, &after);
    dump(&format!("loss_{case}_before"), &before, 320, 240);
    dump(&format!("loss_{case}_after"), &after, 320, 240);
    assert!(
        before == after,
        "{case}: {off} pixels off by up to {most} levels on the new device"
    );
    assert!(
        before.chunks(4).any(|p| p[3] > 0),
        "{case}: drew nothing to compare"
    );
    target
}

// The arena scene: placements packed into the arena, their
// meshes and textures, thinning, impostors and their atlas, decals, the
// kit's own ground, sun shadows, and the still view's kept layers.
#[test]
fn a_renderer_draws_the_same_frame_on_a_new_device() {
    let _clock = CLOCK.lock().unwrap_or_else(|e| e.into_inner());
    HELD_CLOCK.store(1f32.to_bits(), std::sync::atomic::Ordering::Relaxed);
    let target = Offscreen::on_new_device(320, 240).unwrap();
    let (renderer, camera, world) = baselines::arena_scene();
    across_a_loss(target, &renderer, (camera, world), "arena");
    HELD_CLOCK.store(f32::NAN.to_bits(), std::sync::atomic::Ordering::Relaxed);
}

// A host's ground and passes: the mounted ground's pages bake again, and
// its shadow draws; the mirror view draws again for a plugged pass that
// reads it, whose own pipeline is made again on the new device.
#[test]
fn a_hosts_ground_and_passes_draw_the_same_on_a_new_device() {
    let (mut scene, _ink) = ground::ridge(true);
    scene.light.sun_color = [0.6; 3];
    scene.light.sun = [1., 0.7, 0.3];
    scene.light.shadows = Some(Shadows {
        resolution: 1024,
        ..Default::default()
    });
    let renderer = Renderer::default();
    renderer.set_scene(&scene);
    let mut target = Offscreen::on_new_device(320, 240).unwrap();
    target = across_a_loss(
        target,
        &renderer,
        (
            Camera::default(),
            Extent {
                size: 42.,
                center: [0.; 3],
            },
        ),
        "ground",
    );
    let pages = renderer.drawn().pages;
    assert!(pages.wanted > 0 && pages.baked == pages.wanted, "{pages:?}");

    let mirrored = Renderer::default();
    reflection::cubes(&mirrored, false);
    let shown: Arc<dyn Pass> = Quad::new(
        Stage::Water,
        FRAME_WGSL,
        "",
        "select(vec3<f32>(1.0, 0.0, 1.0), mirror_reflection(p.xy).rgb, mirror_reflection(p.xy).a > 0.5)",
    );
    mirrored.set_scene(&Scene {
        light: neutral(),
        reflection: Some(Reflection { level: 0. }),
        passes: vec![shown],
        ..Default::default()
    });
    across_a_loss(target, &mirrored, reflection::view(), "mirror");
}
