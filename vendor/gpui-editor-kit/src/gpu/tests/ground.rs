use super::*;

// A ground with relief keeps normal pages, its normals shaded from its
// height, though its relief raises nothing.
#[test]
fn relief_with_no_displacement_keeps_normal_pages() {
    let (scene, _, half) = paged_ground_with(&flat_field(129), checker::Relief::Flat);
    let mut target = Offscreen::new(320, 240).unwrap();
    let renderer = Renderer::default();
    renderer.set_scene(&scene);
    let world = Extent {
        center: [0.; 3],
        size: 2. * half,
    };
    target.render(&renderer, Camera::default(), world).unwrap();
    let state = renderer.lock();
    let pages = (state.device.as_ref().unwrap().ground_pages.as_ref()).unwrap();
    assert!(state.slots[0].drawn.pages.baked > 0);
    assert!(pages.normal_view().is_some());
}

// The kit mounts no ground of its own: a terrain the scene gives no ground
// draws in its material, live, with no pages, while the same terrain with
// a ground bakes them.
#[test]
fn the_kits_default_ground_mounts_nothing() {
    let (mut scene, _, half) = paged_ground();
    let mut target = Offscreen::new(320, 240).unwrap();
    let camera = Camera::default();
    let world = Extent {
        center: [0.; 3],
        size: 2. * half,
    };
    let renderer = Renderer::default();
    renderer.set_scene(&scene);
    let mounted = target.render(&renderer, camera, world).unwrap();
    assert!(renderer.drawn().pages.baked > 0);
    scene.ground = GroundDraw::Kit;
    renderer.set_scene(&scene);
    let default = target.settle(&renderer).unwrap();
    assert_eq!(renderer.drawn().pages, PageCounts::default());
    assert!(
        renderer
            .state
            .lock()
            .unwrap()
            .device
            .as_ref()
            .unwrap()
            .surface
            .is_none()
    );
    // The ground paints the east blue; the terrain's own white colours
    // show there without it.
    let east = camera.project([half / 2., 0., 0.], target.bounds(), world);
    near(target.pixel(&mounted, east), [60, 60, 200], 3);
    near(target.pixel(&default, east), [255, 255, 255], 3);
}

use std::sync::atomic::Ordering;

pub(super) mod host {
    use crate::gpu::terrain::{Ground, GroundFactory, GroundPrepare};
    use std::sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    };

    // The test host owns group 3 and both shader functions. A ridge casts a shadow
    // west of its footprint. Paint changes only a square on the flat part,
    // and with `relief` raises a block there.
    const RIDGE: &str = r#"
@group(3) @binding(0) var<uniform> paint: vec4<u32>;
fn ground_displacement(uv: vec2<f32>, world: vec3<f32>, n: vec3<f32>, dw: array<vec3<f32>, 2>) -> f32 {
    let block = paint.z > 0u && all(abs(world.xz + vec2<f32>(12.0)) < vec2<f32>(1.0));
    return clamp(5.0 - abs(world.x - 4.0), 0.0, 4.0) + select(0.0, 3.0, block);
}
fn ground(uv: vec2<f32>, world: vec3<f32>, n: vec3<f32>, dw: array<vec3<f32>, 2>) -> GroundSample {
    var color = vec3<f32>(0.08, 0.65, 0.2);
    if paint.y > 0u { color = color.brg; }
    if paint.x > 0u && all(abs(world.xz + vec2<f32>(12.0)) < vec2<f32>(1.0)) {
        color = vec3<f32>(0.9, 0.05, 0.08);
    }
    return GroundSample(vec4<f32>(color, 1.0), n, 0.0, ground_displacement(uv,world,n,dw), ridge_glow(world));
}
fn ridge_glow(world: vec3<f32>) -> f32 {
    return 0.0; // glow
}
"#;

    /// The ridge glowing fully in a square on its flat part, west of the
    /// paint square.
    pub(super) fn glowing_wgsl() -> &'static str {
        let glow =
            "return select(0.0, 1.0, all(abs(world.xz + vec2<f32>(18.0, 14.0)) < vec2<f32>(2.0)));";
        Box::leak(RIDGE.replace("return 0.0; // glow", glow).into_boxed_str())
    }

    #[derive(Default)]
    pub(in crate::gpu::tests) struct Ink {
        pub(super) ready: AtomicBool,
        pub(super) version: AtomicU64,
        pub(super) stroke: AtomicU64,
        /// Strokes that moved the ridge's relief, reported only while
        /// `tracks` is set (`Ground::relief_moves`).
        pub(super) relief: AtomicU64,
        pub(super) tracks: AtomicBool,
        /// Reports no relief at all (`Ground::relief_bound` 0), as a
        /// ground whose layers displace nothing does.
        pub(super) flat: AtomicBool,
    }

    struct Ridge {
        wgsl: &'static str,
        ink: Arc<Ink>,
        layout: wgpu::BindGroupLayout,
        group: wgpu::BindGroup,
        uniform: wgpu::Buffer,
        uploaded: Option<[u64; 3]>,
        glows: bool,
    }

    impl Ground for Ridge {
        fn wgsl(&self) -> &str {
            self.wgsl
        }
        fn layout(&self) -> &wgpu::BindGroupLayout {
            &self.layout
        }
        fn bind_group(&self) -> Option<&wgpu::BindGroup> {
            self.uploaded.map(|_| &self.group)
        }
        fn prepare(&mut self, frame: &mut GroundPrepare<'_>) -> bool {
            if !self.ink.ready.load(Ordering::Relaxed) || frame.budget.spent() {
                return false;
            }
            let state = [
                self.ink.stroke.load(Ordering::Relaxed),
                self.ink.version.load(Ordering::Relaxed),
                self.ink.relief.load(Ordering::Relaxed),
            ];
            if self.uploaded != Some(state) {
                frame.queue().write_buffer(
                    &self.uniform,
                    0,
                    bytemuck::cast_slice(&[state[0] as u32, state[1] as u32, state[2] as u32, 0]),
                );
                frame.budget.charge(16);
                self.uploaded = Some(state);
            }
            true
        }
        fn version(&self) -> u64 {
            self.uploaded.unwrap_or_default()[1]
        }
        fn edits(&self, since: u64, out: &mut Vec<[f32; 4]>) -> u64 {
            let stroke = self.uploaded.unwrap_or_default()[0];
            if since != stroke {
                out.push([-13., -13., -11., -11.]);
            }
            stroke
        }
        fn relief_moves(&self) -> Option<u64> {
            (self.ink.tracks.load(Ordering::Relaxed)).then(|| self.uploaded.unwrap_or_default()[2])
        }
        fn relief_bound(&self) -> f32 {
            if self.ink.flat.load(Ordering::Relaxed) {
                0.
            } else {
                4.
            }
        }
        fn rough(&self, area: [f32; 4]) -> bool {
            area[0] < 9. && area[2] > -1.
        }
        fn glow(&self) -> bool {
            self.glows
        }
    }

    pub(super) fn factory(ink: Arc<Ink>) -> GroundFactory {
        shader_factory(ink, RIDGE)
    }

    /// The ridge's group 3 layout, one per device as a warmed ground's must be.
    pub(super) fn shared_layout(d: &wgpu::Device) -> wgpu::BindGroupLayout {
        crate::gpu::per_device(d, "test ridge paint", || layout(d))
    }

    fn layout(d: &wgpu::Device) -> wgpu::BindGroupLayout {
        d.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("ridge paint"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            }],
        })
    }

    /// The ridge with `wgsl` appended to its shader (another program), over
    /// the per-device layout.
    pub(super) fn warmable_factory(ink: Arc<Ink>, wgsl: &'static str) -> GroundFactory {
        make(ink, wgsl, true, false)
    }

    /// The ridge with `wgsl`, declaring glow when `glows`.
    pub(super) fn glow_factory(ink: Arc<Ink>, wgsl: &'static str, glows: bool) -> GroundFactory {
        make(ink, wgsl, false, glows)
    }

    pub(super) fn ridge_wgsl(extra: &str) -> String {
        format!("{RIDGE}{extra}")
    }

    pub(super) fn ridge_wgsl_static() -> &'static str {
        RIDGE
    }

    /// The ridge's resources with another shader, as a host with a typo has.
    pub(super) fn shader_factory(ink: Arc<Ink>, wgsl: &'static str) -> GroundFactory {
        make(ink, wgsl, false, false)
    }

    fn make(ink: Arc<Ink>, wgsl: &'static str, shared: bool, glows: bool) -> GroundFactory {
        Arc::new(move |init| {
            let d = init.device;
            let layout = if shared { shared_layout(d) } else { layout(d) };
            let uniform = d.create_buffer(&wgpu::BufferDescriptor {
                label: Some("ridge paint"),
                size: 16,
                usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            let group = d.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("ridge paint"),
                layout: &layout,
                entries: &[wgpu::BindGroupEntry {
                    binding: 0,
                    resource: uniform.as_entire_binding(),
                }],
            });
            Box::new(Ridge {
                wgsl,
                ink: ink.clone(),
                layout,
                group,
                uniform,
                uploaded: None,
                glows,
            })
        })
    }
}

use host::Ink;

pub(super) fn ridge(ready: bool) -> (Scene, Arc<Ink>) {
    let field = flat_field(129);
    let ink = Arc::new(Ink::default());
    ink.ready.store(ready, Ordering::Relaxed);
    let scene = Scene {
        terrain: Some(Arc::new(Terrain {
            heights: Arc::new(HeightMap::new(&field).unwrap()),
            colors: Arc::new(ControlMap::new(129, 129, [255; 4].repeat(129 * 129)).unwrap()),
            material: Material::Lit,
            texture: None,
            tints: false,
            color_gain: 1.,
        })),
        ground: super::GroundDraw::Surface(host::factory(ink.clone())),
        light: neutral(),
        ..Default::default()
    };
    (scene, ink)
}

fn paint_view() -> (Camera, Extent) {
    (
        Camera {
            zoom: 8.,
            ..Default::default()
        },
        Extent {
            center: [-16., 0., -16.],
            size: 64.,
        },
    )
}

#[test]
fn a_ground_version_change_rebakes_every_page() {
    let (scene, ink) = ridge(true);
    let mut target = Offscreen::new(320, 240).unwrap();
    let renderer = Renderer::default();
    let (camera, world) = paint_view();
    renderer.set_scene(&scene);
    let before = target.render(&renderer, camera, world).unwrap();
    target.frame(&renderer).unwrap();
    assert_eq!(renderer.drawn().pages.new, 0);
    ink.version.store(1, Ordering::Relaxed);
    let after = target.frame(&renderer).unwrap();
    let p = renderer.drawn().pages;
    assert!(p.wanted > 1 && p.new == p.wanted, "{p:?}");
    assert_ne!(
        target.pixel(&before, point(px(160.), px(120.))),
        target.pixel(&after, point(px(160.), px(120.)))
    );
}

#[test]
fn two_views_take_one_ground_stroke_on_separate_frames() {
    let (scene, ink) = ridge(true);
    let mut small = Offscreen::new(240, 180).unwrap();
    let mut big = Offscreen::new(480, 360).unwrap();
    let renderer = Renderer::default();
    renderer.set_scene(&scene);
    let (camera, world) = paint_view();
    small.aim_in(&renderer, 0, camera, world);
    big.aim_in(&renderer, 1, camera, world);
    for _ in 0..12 {
        small.frame_in(&renderer, 0).unwrap();
        big.frame_in(&renderer, 1).unwrap();
    }
    let a = small.frame_in(&renderer, 0).unwrap();
    let b = big.frame_in(&renderer, 1).unwrap();
    ink.stroke.store(1, Ordering::Relaxed);
    for (target, i, before) in [(&mut small, 0, a), (&mut big, 1, b)] {
        let frame = target.frame_in(&renderer, i).unwrap();
        let at = camera.project([-12., 0., -12.], target.bounds(), world);
        let old = target.pixel(&before, at);
        let new = target.pixel(&frame, at);
        assert!(
            old[1] > old[0] && new[0] > new[1],
            "view {i}: {old:?} -> {new:?}"
        );
        let p = renderer.lock().slots[i].drawn.pages;
        assert_eq!(p.baked, p.wanted, "view {i}: {p:?}");
        // Both views use the pool. The second can reuse the first view's fresh page.
        if i == 0 {
            assert!(p.new > 0 && p.new < p.wanted, "{p:?}");
        }
    }
}

#[test]
fn a_host_ground_draws_shadows_and_bakes_pages() {
    let (mut scene, _) = ridge(true);
    scene.light = Light {
        sun: [1., 0.5, 0.],
        sun_color: [0.8; 3],
        sky: [0.15; 3],
        ground: [0.15; 3],
        shadows: Some(Shadows {
            resolution: 1024,
            ..Default::default()
        }),
        fog: None,
    };
    let mut target = Offscreen::new(320, 240).unwrap();
    let renderer = Renderer::default();
    renderer.set_scene(&scene);
    let camera = Camera::default();
    let world = Extent {
        size: 42.,
        center: [0.; 3],
    };
    let shadow = target.render(&renderer, camera, world).unwrap();
    let pages = renderer.drawn().pages;
    assert!(pages.wanted > 0 && pages.wanted == pages.baked, "{pages:?}");
    scene.light.shadows = None;
    renderer.set_scene(&scene);
    let lit = target.settle(&renderer).unwrap();
    let at = camera.project([-3., 0., 0.], target.bounds(), world);
    let dark = target.pixel(&shadow, at);
    let green = target.pixel(&lit, at);
    assert!(
        green[1] > green[0] && green[1] > dark[1] + 12,
        "unshadowed {green:?}, shadowed {dark:?}"
    );
}

// A stroke that moves no relief, as paint over layers that displace
// nothing does, keeps the shadow map, and the frame after it
// keeps its opaque layers as before. A stroke that moves relief redraws
// them, and so does every stroke of a ground that doesn't tell.
#[test]
fn only_strokes_that_move_relief_redraw_the_shadows() {
    let (mut scene, ink) = ridge(true);
    ink.tracks.store(true, Ordering::Relaxed);
    scene.light = Light {
        sun: [1., 0.5, 0.],
        sun_color: [0.8; 3],
        sky: [0.15; 3],
        ground: [0.15; 3],
        shadows: Some(Shadows {
            resolution: 1024,
            ..Default::default()
        }),
        fog: None,
    };
    // The occlusion keeps a depth copy, which kept layers need.
    scene.occlusion = Some(Occlusion::default());
    let mut target = Offscreen::new(320, 240).unwrap();
    let renderer = Renderer::default();
    renderer.set_scene(&scene);
    let (camera, world) = paint_view();
    target.render(&renderer, camera, world).unwrap();
    let mut stroke = |n: u64, relief: u64| {
        ink.relief.store(relief, Ordering::Relaxed);
        ink.stroke.store(n, Ordering::Relaxed);
        target.frame(&renderer).unwrap();
        let drawn = renderer.drawn();
        let mut kept = Reuse::Nothing;
        for _ in 0..4 {
            target.frame(&renderer).unwrap();
            kept = renderer.drawn().reuse;
            if kept == Reuse::Whole {
                break;
            }
        }
        (drawn, kept)
    };
    let (flat, kept) = stroke(1, 0);
    assert!(
        flat.pages.new > 0,
        "the stroke's pages bake again: {flat:?}"
    );
    assert!(!flat.shadow && flat.cascades == 0, "{flat:?}");
    assert_eq!(flat.reuse, Reuse::Nothing, "the stroke's own frame");
    assert_eq!(kept, Reuse::Whole, "frames after it");
    let (raised, _) = stroke(2, 1);
    assert!(raised.shadow, "{raised:?}");
    ink.tracks.store(false, Ordering::Relaxed);
    let (untold, _) = stroke(3, 1);
    assert!(untold.shadow, "{untold:?}");
}

// A stroke that raises the ground draws the shadow map and the view's
// cascades again only over the texels under it, and the frame then shades
// as one after a whole redraw does, byte for byte:
// with a block and a placement east of the stroke casting into it, and
// another placement far off. Without cascades the view shades from the
// scene's map alone.
#[test]
fn a_partial_shadow_redraw_draws_what_a_whole_one_does() {
    partial_shadow_redraw(false, false);
}

// The same for a sculpt dab under the stroke, the heights raised a metre:
// with a row that rides the ground at its origin there (`adapting`'s
// `follow`), 8 m across, so it moves past the dab and past the cell the
// ground's own triangles lean over.
#[test]
fn a_partial_shadow_redraw_after_a_sculpt_draws_what_a_whole_one_does() {
    partial_shadow_redraw(true, false);
}

// Both again under a `ShadowHold`, as a brush stroke takes one: two edits
// leave every map's texels as they were, and the hold's drop draws each map
// once, in part, over both, to what a whole redraw draws. An edit after the
// stroke, as its undo, draws at once.
#[test]
fn a_held_stroke_draws_its_shadows_once_on_release() {
    partial_shadow_redraw(false, true);
}

#[test]
fn a_held_sculpt_stroke_draws_its_shadows_once_on_release() {
    partial_shadow_redraw(true, true);
}

/// The scene's shadow map and view 0's cascades as drawn, texel bytes in
/// order: to compare maps where a frame would show only what it samples.
fn shadow_maps(target: &Offscreen, renderer: &Renderer) -> Vec<u8> {
    let textures: Vec<wgpu::Texture> = {
        let state = renderer.lock();
        let map = (state.device.as_ref()).and_then(|d| d.shadow_map.as_ref());
        let cascades = (state.slots.first())
            .and_then(|s| s.targets.as_ref())
            .and_then(|t| t.cascades.as_ref());
        (map.map(|m| m.texture.clone()).into_iter())
            .chain(cascades.map(|c| c.texture.clone()))
            .collect()
    };
    let gpu = &target.gpu;
    let mut out = Vec::new();
    for texture in textures {
        let size = texture.size();
        // Depth16Unorm, rows padded to the copy's 256 bytes.
        let row = (size.width * 2).next_multiple_of(256);
        let buffer = gpu.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("shadow readback"),
            size: u64::from(row * size.height * size.depth_or_array_layers),
            usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let mut encoder = gpu.device.create_command_encoder(&Default::default());
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture: &texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::DepthOnly,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &buffer,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(row),
                    rows_per_image: Some(size.height),
                },
            },
            size,
        );
        gpu.queue.submit([encoder.finish()]);
        let slice = buffer.slice(..);
        slice.map_async(wgpu::MapMode::Read, |_| {});
        gpu.device
            .poll(wgpu::PollType::Wait {
                submission_index: None,
                timeout: Some(std::time::Duration::from_secs(10)),
            })
            .unwrap();
        for texels in slice.get_mapped_range().unwrap().chunks(row as usize) {
            out.extend_from_slice(&texels[..size.width as usize * 2]);
        }
    }
    assert!(!out.is_empty(), "no shadow maps drawn");
    out
}

fn partial_shadow_redraw(sculpt: bool, held: bool) {
    for cascades in [2, 0] {
        let at = format!("{cascades} sculpt {sculpt} held {held}");
        let (mut scene, ink) = ridge(true);
        ink.tracks.store(true, Ordering::Relaxed);
        // Its 4 m of relief would pad a sculpt's region past the rider.
        ink.flat.store(sculpt, Ordering::Relaxed);
        scene.light = Light {
            sun: [1., 0.6, 0.2],
            sun_color: [0.8; 3],
            sky: [0.15; 3],
            ground: [0.15; 3],
            shadows: Some(Shadows {
                resolution: 1024,
                cascades,
                ..Default::default()
            }),
            fog: None,
        };
        let mut tall = math::translate([-9., 2., -12.5]);
        tall[5] = 4.;
        scene.batches = vec![single(cube(0xd0a040, Material::Lit).data().clone(), tall)];
        let renderer = Renderer::default();
        renderer.set_scene(&scene);
        renderer.set_pieces(vec![Piece {
            levels: vec![(Arc::new(cube(0x40c060, Material::Lit)), 0.)],
            impostor: None,
            pose: None,
        }]);
        let mut post = math::translate([-9.5, 1.5, -11.]);
        post[5] = 3.;
        let mut rider = math::translate([-12., 0., -13.]);
        rider[0] = 8.;
        let rider = adapting(rider, true, true, 0.5);
        renderer.set_placements(&[0, 0, 0], &[post, math::translate([20., 0.5, 20.]), rider]);
        let mut target = Offscreen::new(320, 240).unwrap();
        let camera = Camera {
            fov: 0.9,
            zoom: 3.,
            tilt: 0.9,
            ..Camera::perspective()
        };
        let world = Extent {
            center: [-12., 0., -12.],
            size: 64.,
        };
        target.aim(&renderer, camera, world);
        let before = target.settle(&renderer).unwrap();
        let partial = |r: &Renderer| (r.lock().device.as_ref()).map_or(0, |d| d.partial_shadows);
        let counted = partial(&renderer);
        let unedited = shadow_maps(&target, &renderer);
        let mut field = super::flat_field(129);
        let map = scene.terrain.as_ref().unwrap().heights.clone();
        // Dab `n`: a sculpt raises world -13..-11 on the ridge's grid (0.5 m
        // from -32), the second dab 2 m east of it; paint strokes the
        // ridge's square again.
        let mut dab = |n: u64| {
            if sculpt {
                let x0 = 38 + 4 * (n as usize - 1);
                let rect = crate::heightfield::Rect {
                    x0,
                    z0: 38,
                    x1: x0 + 4,
                    z1: 42,
                };
                super::raise(&mut field, &map, rect, 1.);
            } else {
                ink.relief.store(n, Ordering::Relaxed);
                ink.stroke.store(n, Ordering::Relaxed);
            }
        };
        if held {
            let hold = renderer.hold_shadows();
            for n in [1, 2] {
                dab(n);
                target.frame(&renderer).unwrap();
                let drawn = renderer.drawn();
                assert!(
                    !drawn.shadow && drawn.cascades == 0,
                    "{at}: dab {n}: {drawn:?}"
                );
                let maps = shadow_maps(&target, &renderer);
                assert!(maps == unedited, "{at}: dab {n} moved held texels");
            }
            assert_eq!(partial(&renderer), counted, "{at}: nothing drawn held");
            drop(hold);
        } else {
            dab(1);
        }
        target.frame(&renderer).unwrap();
        let drawn = renderer.drawn();
        assert!(drawn.shadow, "{at}: {drawn:?}");
        let parts = partial(&renderer) - counted;
        // The rider's reach widens a sculpt's region past half of a fine
        // cascade, which draws whole; the scene's map draws in part.
        if sculpt {
            assert!(parts >= 1, "{at}: {drawn:?}");
        } else {
            assert_eq!(parts as usize, 1 + drawn.cascades, "{at}: {drawn:?}");
        }
        let kept = target.settle(&renderer).unwrap();
        assert_ne!(kept, before, "{at}: the edit shows");
        let kept_maps = shadow_maps(&target, &renderer);
        // A new scene draws every map whole.
        renderer.set_scene(&scene);
        let whole = target.settle(&renderer).unwrap();
        assert_eq!(partial(&renderer) - counted, parts, "{at}");
        assert!(
            kept_maps == shadow_maps(&target, &renderer),
            "{at}: maps differ from whole ones"
        );
        assert_eq!(super::difference(&kept, &whole), (0, 0), "{at}");
        assert!(kept == whole, "{at}: kept maps shade differently");
        if held {
            // The undo: no hold, so it draws in the frame it lands.
            dab(3);
            target.frame(&renderer).unwrap();
            let drawn = renderer.drawn();
            assert!(drawn.shadow, "{at}: the edit after the stroke: {drawn:?}");
        }
    }
}

// A row that rides the ground draws where the ground took it, however far
// a sculpt moved it: raised 10 m, past the 4 m the cull once allowed for,
// it draws as a rigid row placed up there does, after the
// sculpt's partial shadow redraw and after a whole one. Seen from the side
// it draws in the view; seen close with only its shadow in view, it casts
// in the nearest cascade, which reaches no lower than the plateau's top.
#[test]
fn a_row_on_ground_raised_past_its_cull_box_still_draws_and_casts() {
    // The plateau: world -16..0 each way on the ridge's grid (0.5 m from
    // -32), the row standing on it at (-6, -8), 2 m across.
    let plateau = crate::heightfield::Rect {
        x0: 32,
        z0: 32,
        x1: 65,
        z1: 65,
    };
    let scaled = |y: f32| {
        let mut m = math::translate([-6., y, -8.]);
        for i in [0, 5, 10] {
            m[i] = 2.;
        }
        m
    };
    let views = [
        // From the side, level with the plateau's top.
        (
            Camera {
                fov: 0.9,
                tilt: 1.5,
                yaw: 0.4,
                ..Camera::perspective()
            },
            Extent {
                center: [-6., 11.5, -8.],
                size: 3.,
            },
            500.,
        ),
        // From just above the plateau west of the row, where its shadow
        // falls and the row is out of view. Cascades reaching 6 m keep the
        // nearest to the plateau's top, as close views' are.
        (
            Camera {
                fov: 0.9,
                tilt: 0.3,
                ..Camera::perspective()
            },
            Extent {
                center: [-8.7, 10., -8.],
                size: 1.5,
            },
            6.,
        ),
    ];
    for (v, &(camera, world, reach)) in views.iter().enumerate() {
        let at = format!("view {v}");
        let (mut scene, ink) = ridge(true);
        ink.tracks.store(true, Ordering::Relaxed);
        ink.flat.store(true, Ordering::Relaxed);
        scene.light = Light {
            sun: [1., 0.6, 0.2],
            sun_color: [0.8; 3],
            sky: [0.15; 3],
            ground: [0.15; 3],
            shadows: Some(Shadows {
                resolution: 1024,
                cascades: 2,
                reach,
                ..Default::default()
            }),
            fog: None,
        };
        // A peak far off holds the heights' range over the plateau's,
        // so the sculpt keeps the shadow fit and redraws in part.
        let mut field = super::flat_field(129);
        let map = scene.terrain.as_ref().unwrap().heights.clone();
        let peak = crate::heightfield::Rect {
            x0: 120,
            z0: 120,
            x1: 124,
            z1: 124,
        };
        super::raise(&mut field, &map, peak, 12.);
        let draw = |row: Mat4| {
            let renderer = Renderer::default();
            renderer.set_scene(&scene);
            renderer.set_pieces(vec![Piece {
                levels: vec![(Arc::new(cube(0x40c060, Material::Lit)), 0.)],
                impostor: None,
                pose: None,
            }]);
            renderer.set_placements(&[0], &[row]);
            renderer
        };
        // Its origin 1 m up at the bottom of a 1 m rise, so it stands
        // on the ground it rides; up there the rigid row stands as it does.
        let renderer = draw(adapting(scaled(1.), true, true, 1.));
        let mut target = Offscreen::new(320, 240).unwrap();
        target.aim(&renderer, camera, world);
        target.settle(&renderer).unwrap();
        let partial = |r: &Renderer| (r.lock().device.as_ref()).map_or(0, |d| d.partial_shadows);
        let counted = partial(&renderer);
        super::raise(&mut field, &map, plateau, 10.);
        let kept = target.settle(&renderer).unwrap();
        assert!(
            partial(&renderer) > counted,
            "{at}: the sculpt redraws in part"
        );
        renderer.set_scene(&scene);
        let whole = target.settle(&renderer).unwrap();

        let reference = draw(scaled(11.));
        let mut other = Offscreen::new(320, 240).unwrap();
        other.aim(&reference, camera, world);
        let expected = other.settle(&reference).unwrap();
        dump(&format!("raised_rider_{v}"), &kept, 320, 240);
        dump(&format!("raised_rigid_{v}"), &expected, 320, 240);
        // Bent and rigid rows take their vertices by different paths: a
        // level apart here and there, none past `difference`'s 2.
        assert_eq!(difference(&kept, &expected).0, 0, "{at}: after the sculpt");
        assert_eq!(difference(&whole, &expected).0, 0, "{at}: drawn whole");
    }
}

// The ground mounts with the first scene and prepares while its meshes
// still stream, rather than once it presents: its layers' texels would
// wait in memory through everything else the open loads.
#[test]
fn the_first_scenes_ground_prepares_before_the_scene_presents() {
    let (mut scene, _) = ridge(true);
    // More batches than one frame uploads (UPLOAD_BATCHES).
    scene.batches = (0..300)
        .map(|i| {
            single(
                quad(0.25, 0.1, Material::Lit),
                math::translate([i as f32 * 0.1, 0., 0.]),
            )
        })
        .collect();
    let renderer = Renderer::default();
    renderer.set_scene(&scene);
    let mut target = Offscreen::new(64, 48).unwrap();
    let (camera, world) = paint_view();
    target.aim(&renderer, camera, world);
    target.frame(&renderer).unwrap();
    {
        let state = renderer.lock();
        let device = state.device.as_ref().unwrap();
        assert!(
            device.presented.is_none(),
            "the scene takes more than a frame"
        );
        let surface = device
            .surface
            .as_ref()
            .expect("mounted before the scene presents");
        assert!(surface.ground.bind_group().is_some(), "and prepared");
    }
    target.settle(&renderer).unwrap();
    assert!(renderer.drawn().pages.baked > 0);
}

#[test]
fn a_loading_ground_leaves_the_default_drawn() {
    let (mut scene, ink) = ridge(false);
    let ground = std::mem::take(&mut scene.ground);
    let renderer = Renderer::default();
    let mut target = Offscreen::new(240, 180).unwrap();
    let (camera, world) = paint_view();
    renderer.set_scene(&scene);
    let default = target.render(&renderer, camera, world).unwrap();
    scene.ground = ground;
    renderer.set_scene(&scene);
    let loading = target.frame(&renderer).unwrap();
    assert!(renderer.pending());
    assert_eq!(difference(&loading, &default), (0, 0));
    ink.ready.store(true, Ordering::Relaxed);
    let ready = target.settle(&renderer).unwrap();
    assert_ne!(ready, default);
    assert!(renderer.drawn().pages.baked > 0);
}

#[test]
fn a_ground_whose_shader_fails_leaves_the_default_drawn() {
    let (mut scene, ink) = ridge(true);
    let ground = std::mem::take(&mut scene.ground);
    let renderer = Renderer::default();
    let mut target = Offscreen::new(240, 180).unwrap();
    let (camera, world) = paint_view();
    renderer.set_scene(&scene);
    let default = target.render(&renderer, camera, world).unwrap();
    // `ground` returns a type the kit has no definition for.
    scene.ground = super::GroundDraw::Surface(host::shader_factory(
        ink,
        "fn ground(uv: vec2<f32>, world: vec3<f32>, n: vec3<f32>, dw: array<vec3<f32>, 2>) -> Missing { }",
    ));
    renderer.set_scene(&scene);
    let broken = target.settle(&renderer).unwrap();
    assert_eq!(difference(&broken, &default), (0, 0));
    // The same device still takes a working ground afterwards.
    scene.ground = ground;
    renderer.set_scene(&scene);
    assert_ne!(target.settle(&renderer).unwrap(), default);
}

#[test]
fn warmed_ground_programs_are_not_built_on_the_render_thread() {
    // Each shader is this test's alone, so no other test's programs match.
    let warmed: &'static str = Box::leak(host::ridge_wgsl("// warmed\n").into_boxed_str());
    let cold: &'static str = Box::leak(host::ridge_wgsl("// cold\n").into_boxed_str());
    let builds = |wgsl: &'static str, warm: bool| {
        let (mut scene, ink) = ridge(true);
        scene.ground = super::GroundDraw::Surface(host::warmable_factory(ink, wgsl));
        let mut target = Offscreen::new(160, 120).unwrap();
        if warm {
            // The ridge reports relief and no normal footprint.
            // Under the kit's lighting, which `Renderer::default` draws with.
            let kit = Families::kit();
            target.warm(|w| {
                w.ground(
                    &kit,
                    wgsl,
                    &host::shared_layout(w.device),
                    false,
                    super::Specular::FULL,
                    &[(true, None)],
                )
            });
        }
        let renderer = Renderer::default();
        renderer.set_scene(&scene);
        let (camera, world) = paint_view();
        target.render(&renderer, camera, world).unwrap();
        assert!(renderer.drawn().pages.baked > 0);
        let state = renderer.lock();
        state.device.as_ref().unwrap().ground_builds
    };
    assert_eq!(builds(cold, false), 1);
    assert_eq!(builds(warmed, true), 0);
}

fn pool_bytes(renderer: &Renderer) -> (u64, bool, u32) {
    let state = renderer.lock();
    let pages = state
        .device
        .as_ref()
        .unwrap()
        .ground_pages
        .as_ref()
        .unwrap();
    (pages.bytes(), pages.glow_view().is_some(), pages.sheets)
}

// Page atlases: sheets of 7 by 7 pages, 288 texels a page with its
// border, 4 mips.
fn atlas_texels(sheets: u32) -> u64 {
    (0..4)
        .map(|m| u64::from(sheets * (288 * 7 >> m) * (288 * 7 >> m)))
        .sum()
}

#[test]
fn only_a_glowing_ground_pays_for_glow_pages() {
    // A splat without normal maps or relief, as Albion's set draws: its pool
    // holds BC3 albedo and the bake's staging, and no more than before glow
    // pages existed.
    let (scene, _, half) = paged_ground();
    let mut target = Offscreen::new(320, 240).unwrap();
    let renderer = Renderer::default();
    renderer.set_scene(&scene);
    let world = Extent {
        center: [0.; 3],
        size: 2. * half,
    };
    target.render(&renderer, Camera::default(), world).unwrap();
    assert!(renderer.drawn().pages.baked > 0);
    let (bytes, glow, sheets) = pool_bytes(&renderer);
    assert!(!glow);
    if bc_supported(target.device()) {
        let staging = (0..4)
            .map(|m| u64::from((288u32 * 8 >> m) * (288u32 >> m)))
            .sum::<u64>();
        // BC3 albedo a byte a texel; staging RGBA8 and RG8.
        assert_eq!(bytes, atlas_texels(sheets) + staging * 6);
    }

    // The same ridge with and without glow, in one view: glow adds an R8
    // atlas and its staging, nothing else.
    let pool = |glows: bool| {
        let (mut scene, ink) = ridge(true);
        let wgsl = if glows {
            host::glowing_wgsl()
        } else {
            host::ridge_wgsl_static()
        };
        scene.ground = super::GroundDraw::Surface(host::glow_factory(ink, wgsl, glows));
        // Dim, so the ground's own light stands out.
        scene.light = Light {
            sun_color: [0.; 3],
            sky: [0.2; 3],
            ground: [0.2; 3],
            ..Default::default()
        };
        let mut target = Offscreen::new(320, 240).unwrap();
        let renderer = Renderer::default();
        renderer.set_scene(&scene);
        let (camera, world) = paint_view();
        let pixels = target.render(&renderer, camera, world).unwrap();
        let at = camera.project([-18., 0., -14.], target.bounds(), world);
        let p = renderer.drawn().pages;
        assert!(p.baked > 0 && p.baked == p.wanted, "{p:?}");
        (pool_bytes(&renderer), target.pixel(&pixels, at))
    };
    let ((plain, plain_glow, sheets), dark) = pool(false);
    let ((glowing, has_glow, glow_sheets), bright) = pool(true);
    assert!(!plain_glow && has_glow);
    assert_eq!(sheets, glow_sheets);
    let staging = (0..4)
        .map(|m| u64::from((288u32 * 8 >> m) * (288u32 >> m)))
        .sum::<u64>();
    assert_eq!(glowing - plain, atlas_texels(sheets) + staging);
    // Lit by its own light, from its page.
    assert!(
        (0..3).all(|c| bright[c] > dark[c]) && bright[1] > dark[1] + 40,
        "glow {bright:?}, plain {dark:?}"
    );
}

// Two height maps synced in one frame share its serial. When the log makes
// room by dropping one of them, a shadow map drawn before that frame can no
// longer redraw just where heights moved: the log says it lost a move
// rather than hand back the frame's other rect alone.
#[test]
fn a_height_log_that_drops_part_of_a_frame_draws_whole() {
    let rect = |x: usize| Rect {
        x0: x,
        z0: 0,
        x1: x + 1,
        z1: 1,
    };
    let mut log = terrain::HeightMoves::default();
    log.push(1, 10, rect(0));
    log.push(1, 20, rect(1));
    let last = terrain::MOVES_KEPT as u64;
    for serial in 2..=last {
        log.push(serial, 10, rect(serial as usize));
    }
    // One entry past the cap: frame 1's first map went, its second stayed.
    assert!(log.since(0, last).is_none());
    assert_eq!(log.since(1, last).unwrap().count(), terrain::MOVES_KEPT - 1);
    assert_eq!(log.since(last - 1, last).unwrap().count(), 1);
}
