// Tests of what a game plugs into the whole scene beside its families: the
// fog its passes take, the environment cube, view modes and a family's own
// vertex stage.
use super::*;

/// A plugged pass covering the view at reversed depth 0.5 in what WGSL
/// expression `color` gives, built on `prelude` (`FRAME_WGSL` or a family
/// set's `pass_prelude`). `own` goes before the fragment: helpers and
/// bindings past group 0.
pub(super) struct Quad {
    stage: Stage,
    source: String,
    pipeline: std::sync::Mutex<OnDevice<wgpu::RenderPipeline>>,
}

impl Quad {
    pub(super) fn new(stage: Stage, prelude: &str, own: &str, color: &str) -> Arc<Self> {
        let fragment = if stage == Stage::Opaque {
            format!(
                "@fragment fn fs_quad(@builtin(position) p: vec4<f32>) -> Opaque {{
                    let c = {color};
                    return Opaque(vec4<f32>(c, 1.0), 0u, pack_rgb9e5(c));
                }}"
            )
        } else {
            format!(
                "@fragment fn fs_quad(@builtin(position) p: vec4<f32>) -> @location(0) vec4<f32> {{
                    return vec4<f32>({color}, 1.0);
                }}"
            )
        };
        let source = format!(
            "{prelude}
            {own}
            @vertex fn vs_quad(@builtin(vertex_index) v: u32) -> @builtin(position) vec4<f32> {{
                let p = vec2<f32>(f32((v << 1u) & 2u), f32(v & 2u)) * 2.0 - 1.0;
                return vec4<f32>(p, 0.5, 1.0);
            }}
            {fragment}"
        );
        Arc::new(Self {
            stage,
            source,
            pipeline: Default::default(),
        })
    }
}

impl Pass for Quad {
    fn stage(&self) -> Stage {
        self.stage
    }

    fn prepare(&self, frame: &mut Prepare<'_>) -> bool {
        let mut pipeline = self.pipeline.lock().unwrap();
        if pipeline.get(frame.device).is_some() {
            return true;
        }
        let d = frame.device;
        let module = d.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("test quad"),
            source: wgpu::ShaderSource::Wgsl(self.source.as_str().into()),
        });
        let layout = d.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("test quad"),
            bind_group_layouts: &[Some(frame.frame_layout)],
            immediate_size: 0,
        });
        let opaque = self.stage == Stage::Opaque;
        let targets: Vec<Option<wgpu::ColorTargetState>> = if opaque {
            OPAQUE_TARGETS.iter().map(|&f| Some(f.into())).collect()
        } else {
            vec![Some(HDR_FORMAT.into())]
        };
        let made = d.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("test quad"),
            layout: Some(&layout),
            vertex: wgpu::VertexState {
                module: &module,
                entry_point: Some("vs_quad"),
                buffers: &[],
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &module,
                entry_point: Some("fs_quad"),
                targets: &targets,
                compilation_options: Default::default(),
            }),
            primitive: Default::default(),
            depth_stencil: Some(wgpu::DepthStencilState {
                format: DEPTH_FORMAT,
                depth_write_enabled: Some(opaque),
                depth_compare: Some(wgpu::CompareFunction::GreaterEqual),
                stencil: Default::default(),
                bias: Default::default(),
            }),
            multisample: Default::default(),
            multiview_mask: None,
            cache: None,
        });
        pipeline.set(d, made);
        true
    }

    fn record(&self, frame: &Record<'_>, pass: &mut wgpu::RenderPass<'_>) {
        if let Some(pipeline) = self.pipeline.lock().unwrap().get(frame.device) {
            pass.set_pipeline(pipeline);
            pass.draw(0..3, 0..1);
        }
    }
}

fn top() -> (Camera, Extent) {
    let camera = Camera {
        tilt: 1.2,
        ..Default::default()
    };
    let world = Extent {
        center: [0.; 3],
        size: 10.,
    };
    (camera, world)
}

/// The middle pixel of `renderer` drawing `passes` under a plain light.
fn middle(renderer: &Renderer, passes: Vec<Arc<dyn Pass>>) -> [u8; 3] {
    let (camera, world) = top();
    let mut target = Offscreen::new(64, 64).unwrap();
    renderer.set_scene(&Scene {
        light: neutral(),
        passes,
        ..Default::default()
    });
    let pixels = target.render(renderer, camera, world).unwrap();
    let i = (32 * 64 + 32) * 4;
    [pixels[i], pixels[i + 1], pixels[i + 2]]
}

/// A host fog that lifts every colour by a fixed amount: unmistakable.
const LIFTING_FOG: &str = "fn fog(color: vec3<f32>, world: vec3<f32>) -> vec3<f32> { return color + vec3<f32>(0.25, 0.5, 0.75); }\n";

// A pass built on its family set's prelude fogs by that set's fog, as the
// set's placements and ground do: a host's fog where the host replaced it,
// the kit's distance fog where not, and the kit's prelude is exactly
// `FRAME_WGSL`.
#[test]
fn a_pass_fogs_by_its_family_sets_fog() {
    let host = Families::with_lighting(
        Vec::new(),
        Lighting {
            fog: Some(LIFTING_FOG.into()),
            ..Default::default()
        },
    )
    .unwrap();
    let kit = Families::kit();
    assert_eq!(kit.pass_prelude(), FRAME_WGSL);
    let draw = |families: &Arc<Families>, color: &str| {
        let renderer = Renderer::with_families(families.clone());
        let pass = Quad::new(Stage::Water, &families.pass_prelude(), "", color);
        middle(&renderer, vec![pass])
    };
    let fog = "fog(vec3<f32>(0.1), vec3<f32>(0.0))";
    let lifted = draw(&host, fog);
    assert_eq!(lifted, draw(&host, "vec3<f32>(0.35, 0.6, 0.85)"));
    // The scene has no fog of the kit's (`Light::fog` off): untouched.
    assert_eq!(draw(&kit, fog), draw(&kit, "vec3<f32>(0.1)"));
    assert_ne!(lifted, draw(&kit, fog));
}

/// A cube whose faces each hold one colour: +X, −X, +Y, −Y, +Z, −Z.
const FACES: [[f32; 3]; 6] = [
    [0.8, 0.1, 0.1],
    [0.1, 0.6, 0.1],
    [0.1, 0.1, 0.9],
    [0.5, 0.5, 0.1],
    [0.1, 0.5, 0.5],
    [0.5, 0.1, 0.5],
];

fn faces() -> Environment {
    Environment::from_fn(4, |d| {
        let a = d.map(f32::abs);
        let (axis, _) =
            (a.iter().enumerate()).fold((0, 0.), |m, (k, &v)| if v > m.1 { (k, v) } else { m });
        FACES[axis * 2 + usize::from(d[axis] < 0.)]
    })
}

/// The middle pixel of `renderer` drawing `passes` with `environment`.
fn middle_in(
    renderer: &Renderer,
    environment: Option<Environment>,
    passes: Vec<Arc<dyn Pass>>,
) -> [u8; 3] {
    let (camera, world) = top();
    let mut target = Offscreen::new(64, 64).unwrap();
    renderer.set_scene(&Scene {
        light: neutral(),
        environment,
        passes,
        ..Default::default()
    });
    let pixels = target.render(renderer, camera, world).unwrap();
    let i = (32 * 64 + 32) * 4;
    [pixels[i], pixels[i + 1], pixels[i + 2]]
}

// The scene's environment cube reaches passes and families through
// `environment(d)`: each axis reads its own face, laid out as
// `Environment::direction` says, a family's surface reads it the same, and
// a scene without one reflects the sky in its place.
#[test]
fn an_environment_cube_reaches_passes_and_families_and_the_sky_stands_in() {
    let renderer = Renderer::default();
    let quad = |color: &str| Quad::new(Stage::Water, FRAME_WGSL, "", color) as Arc<dyn Pass>;
    let wgsl = |v: [f32; 3]| format!("vec3<f32>({:?}, {:?}, {:?})", v[0], v[1], v[2]);
    let axes: [[f32; 3]; 6] = [
        [1., 0., 0.],
        [-1., 0., 0.],
        [0., 1., 0.],
        [0., -1., 0.],
        [0., 0., 1.],
        [0., 0., -1.],
    ];
    for (axis, face) in axes.iter().zip(FACES) {
        let read = format!("environment({})", wgsl(*axis));
        let drawn = middle_in(&renderer, Some(faces()), vec![quad(&read)]);
        let want = middle_in(&renderer, None, vec![quad(&wgsl(face))]);
        near(drawn, want, 1);
        let sky = format!("sky_radiance({})", wgsl(*axis));
        assert_eq!(
            middle_in(&renderer, None, vec![quad(&read)]),
            middle_in(&renderer, None, vec![quad(&sky)])
        );
    }
    // A family that gives off what the cube holds straight up.
    let shine = Family {
        name: "shine".into(),
        wgsl: "fn shine_coverage(material: GpuDraw, c: Cover) -> f32 { return 1.0; }
fn shine_surface(material: GpuDraw, p: LitPixel, t: LitTexels) -> Lit {
    return Lit(plain(vec3<f32>(0.0), vec3<f32>(0.0, 1.0, 0.0)), environment(vec3<f32>(0.0, 1.0, 0.0)));
}"
        .into(),
        light: false,
        vertex: false,
        coverage_slots: 0,
        bake: false,
    };
    let families = Families::new(vec![shine]).unwrap();
    let renderer = Renderer::with_families(families.clone());
    let mut mesh = super::quad(4., 0., Material::Lit);
    mesh.family = families.id("shine").unwrap();
    renderer.set_pieces(vec![Piece {
        levels: vec![(Arc::new(Mesh::new(mesh).unwrap()), 0.)],
        impostor: None,
        pose: None,
    }]);
    renderer.set_placements(&[0], &[math::translate([0.; 3])]);
    let lit = middle_in(&renderer, Some(faces()), Vec::new());
    let plain = Renderer::default();
    near(lit, middle_in(&plain, None, vec![quad(&wgsl(FACES[2]))]), 2);
}

// A device knows the cube it uploaded by the texels' `Arc`: a new cube the
// allocator places where a dropped one lay is still uploaded, not taken
// for the old one by its address.
#[test]
fn a_cube_allocated_where_a_dropped_one_lay_uploads() {
    let target = Offscreen::new(4, 4).unwrap();
    let gpu = Gpu {
        device: target.device().clone(),
        queue: target.queue().clone(),
    };
    let mut device = Device::new(&gpu, wgpu::TextureFormat::Rgba8Unorm, &Families::kit());
    let first = faces();
    device.upload_environment(&gpu, Some(&first));
    let uploaded = device.environment.as_ref().unwrap().1.clone();
    let address = Arc::as_ptr(&first.texels);
    drop(first);
    // The allocator hands a freed block of the same size out again, most
    // often at once; held ones keep later tries off it.
    let mut held = Vec::new();
    let again = loop {
        let cube = faces();
        if Arc::as_ptr(&cube.texels) == address {
            break Some(cube);
        }
        if held.len() == 64 {
            break None;
        }
        held.push(cube);
    };
    let Some(again) = again else {
        eprintln!("skipped: no cube landed where the dropped one lay");
        return;
    };
    device.upload_environment(&gpu, Some(&again));
    assert_ne!(device.environment.as_ref().unwrap().1, uploaded);
}

/// A non-Anno view mode for the tests: the kit's light dimmed, a ripple from
/// the host's texture lighting the albedo on every lit pixel, emissive
/// added after both (a glow that skips the mode's air), and a fog that adds
/// host word 0's colour.
fn tide() -> ViewMode {
    ViewMode {
        name: "tide".into(),
        shade: Some(
            "fn tide_shade(world: vec3<f32>, s: Surface, emissive: vec3<f32>, has_normals: bool) -> vec3<f32> {
    let ripple = host_texture(world.xz * 0.5, 1u).rgb;
    return kit_shade(world, s, has_normals) * 0.3 + s.albedo * ripple * 0.5 + emissive;
}
"
            .into(),
        ),
        fog: Some(
            "fn tide_fog(color: vec3<f32>, world: vec3<f32>) -> vec3<f32> { return color + lights.host[0].rgb; }\n"
                .into(),
        ),
        unlit: Some(
            "fn tide_unlit(color: vec3<f32>, world: vec3<f32>) -> vec3<f32> { return color * vec3<f32>(0.25, 1.0, 1.0); }\n"
                .into(),
        ),
    }
}

// A view mode swaps the scene's shade and fog together, frame by frame,
// through the family set's own pipelines: the plain mode draws exactly what
// a set without modes draws, the tide mode draws otherwise (its ripple from
// the host's texture on lit pixels, while a glowing surface's light skips
// it), its fog tints only once its word says so, a Blend-stage pass listed
// with it changes the frame after, and switching back to the plain mode
// restores the frame byte for byte.
#[test]
fn a_view_mode_swaps_shade_and_fog_and_switching_back_restores_the_frame() {
    use crate::texture::Texture;
    let moded = Families::with_lighting(
        Vec::new(),
        Lighting {
            modes: vec![tide()],
            ..Default::default()
        },
    )
    .unwrap();
    // Layer 0 black, layer 1 a two-tone ripple.
    let mut texels = vec![0u8; 4 * 4 * 4];
    for i in 0..16 {
        let v = if (i + i / 4) % 2 == 0 { 255 } else { 64 };
        texels.extend([v, v, v, 255]);
    }
    let ripple = HostTexture {
        width: 4,
        height: 4,
        layers: 2,
        texels: Arc::new(texels),
    };
    // A grey lit square on the left, a glowing one on the right.
    let mut lit = super::quad(1.5, 0., Material::Lit);
    lit.texture = Some(Texture::solid([128, 128, 128, 255]).into());
    let mut glowing = super::quad(1.5, 0., Material::Lit);
    glowing.texture = Some(Texture::solid([255; 4]).into());
    glowing.normal_map = Some(Texture::solid([128, 128, 255, 255]).into());
    glowing.emissive = Some([0.6, 0.4, 0.2]);
    let (camera, world) = top();
    // Where pixels are looked up; each frame draws into a target of its own.
    let target = Offscreen::new(96, 64).unwrap();
    let draw = |renderer: &Renderer, mode: u32, word: [f32; 3], passes: Vec<Arc<dyn Pass>>| {
        let mut target = Offscreen::new(96, 64).unwrap();
        let mut glow = Glow::default();
        glow.host[..3].copy_from_slice(&word);
        renderer.set_scene(&Scene {
            light: neutral(),
            view_mode: mode,
            host_texture: Some(ripple.clone()),
            glow,
            passes,
            ..Default::default()
        });
        target.render(renderer, camera, world).unwrap()
    };
    let place = |renderer: &Renderer| {
        renderer.set_pieces(
            [&lit, &glowing]
                .map(|m| Piece {
                    levels: vec![(Arc::new(Mesh::new(m.clone()).unwrap()), 0.)],
                    impostor: None,
                    pose: None,
                })
                .into(),
        );
        let at = [
            math::translate([-2., 0., 0.]),
            math::translate([2., 0., 0.]),
        ];
        renderer.set_placements(&[0, 1], &at);
    };
    let renderer = Renderer::with_families(moded.clone());
    place(&renderer);
    let kit = Renderer::default();
    place(&kit);
    let pixel = |pixels: &[u8], p: [f32; 3]| {
        target.pixel(pixels, camera.project(p, target.bounds(), world))
    };
    let (left, right) = ([-2., 0., 0.], [2., 0., 0.]);

    let plain = draw(&renderer, 0, [0.; 3], Vec::new());
    assert!(
        plain == draw(&kit, 0, [0.; 3], Vec::new()),
        "the plain mode draws as no modes"
    );
    let tide = draw(&renderer, 1, [0.; 3], Vec::new());
    assert_ne!(
        pixel(&tide, left),
        pixel(&plain, left),
        "the tide's shade on the lit square"
    );
    near(pixel(&tide, right), pixel(&plain, right), 1);
    let tinted = draw(&renderer, 1, [0.3, 0., 0.], Vec::new());
    let (a, b) = (pixel(&tinted, left), pixel(&tide, left));
    assert!(
        a[0] > b[0] + 20 && a[1] == b[1],
        "the tide's fog: {a:?} against {b:?}"
    );
    // Its after-frame step, a Blend-stage pass the scene lists with it.
    let after: Arc<dyn Pass> = Quad::new(Stage::Blend, FRAME_WGSL, "", "vec3<f32>(0.0, 0.2, 0.4)");
    let graded = draw(&renderer, 1, [0.; 3], vec![after]);
    assert_ne!(graded, tide);
    assert!(
        draw(&renderer, 0, [0.; 3], Vec::new()) == plain,
        "switching back changed the frame"
    );
}

// Flat colours see no shade and no fog, but a view mode's `unlit`: the
// plain mode draws them as a set without modes does, and the tide mode
// takes most of their red, placed (fs_placed_flat) and as a batch
// (fs_flat) alike.
#[test]
fn a_view_mode_turns_flat_colours_by_its_unlit() {
    let moded = Families::with_lighting(
        Vec::new(),
        Lighting {
            modes: vec![tide()],
            ..Default::default()
        },
    )
    .unwrap();
    let flat = super::quad(1.5, 0., Material::Flat);
    let place = |renderer: &Renderer| {
        renderer.set_pieces(vec![Piece {
            levels: vec![(Arc::new(Mesh::new(flat.clone()).unwrap()), 0.)],
            impostor: None,
            pose: None,
        }]);
        renderer.set_placements(&[0], &[math::translate([-2., 0., 0.])]);
    };
    let (camera, world) = top();
    let draw = |renderer: &Renderer, mode: u32| {
        let mut target = Offscreen::new(96, 64).unwrap();
        renderer.set_scene(&Scene {
            light: neutral(),
            view_mode: mode,
            batches: vec![super::single(flat.clone(), math::translate([2., 0., 0.]))],
            ..Default::default()
        });
        target.render(renderer, camera, world).unwrap()
    };
    let renderer = Renderer::with_families(moded);
    place(&renderer);
    let kit = Renderer::default();
    place(&kit);
    let target = Offscreen::new(96, 64).unwrap();
    let pixel = |pixels: &[u8], p: [f32; 3]| {
        target.pixel(pixels, camera.project(p, target.bounds(), world))
    };
    let plain = draw(&renderer, 0);
    assert!(plain == draw(&kit, 0), "the plain mode draws as no modes");
    let tide = draw(&renderer, 1);
    for at in [[-2., 0., 0.], [2., 0., 0.]] {
        let (p, t) = (pixel(&plain, at), pixel(&tide, at));
        assert!(
            u16::from(t[0]) + 40 < u16::from(p[0]),
            "red at {at:?}: {t:?} against {p:?}"
        );
        near([t[1], t[2], 0], [p[1], p[2], 0], 1);
    }
}

/// A non-Anno family whose squares turn to face the camera: each vertex
/// keeps its offset from its part's centre (`Thinning::parts`), laid along
/// the view's right and up instead of the mesh's X and Y.
fn boards() -> Family {
    Family {
        name: "board".into(),
        wgsl: "fn board_coverage(material: GpuDraw, c: Cover) -> f32 { return lit_coverage(material, c); }
fn board_surface(material: GpuDraw, p: LitPixel, t: LitTexels) -> Lit { return lit_surface(material, p, t); }
fn board_vertex(material: GpuDraw, v: FamilyVertex) -> vec3<f32> {
    if v.part.w < 0.0 { return v.world; }
    let centre = (v.row * vec4<f32>(v.part.xyz, 1.0)).xyz;
    var forward = -globals.eye.xyz;
    if globals.eye.w > 0.5 { forward = normalize(centre - globals.eye.xyz); }
    var right = cross(vec3<f32>(0.0, 1.0, 0.0), forward);
    if dot(right, right) < 1e-8 { right = vec3<f32>(1.0, 0.0, 0.0); }
    right = normalize(right);
    let up = cross(forward, right);
    let o = (v.local - v.part.xyz) * length(v.row[0].xyz);
    return centre + right * o.x + up * o.y;
}
"
        .into(),
        light: false,
        vertex: true,
        coverage_slots: 0,
        bake: false,
    }
}

// A family's vertex stage places its placements' vertices: a vertical
// square seen from straight above stands edge on and covers nearly nothing
// in the kit's family, while the board family turns it to face the camera,
// a full square; a family with a vertex stage that would bake impostors is
// refused.
#[test]
fn a_familys_vertex_stage_turns_squares_to_face_the_camera() {
    use crate::texture::Texture;
    let families = Families::new(vec![boards()]).unwrap();
    assert!(
        Families::new(vec![Family {
            bake: true,
            ..boards()
        }])
        .is_err()
    );
    // A unit square standing in the XY plane, its part's centre its own.
    let mut square = MeshData {
        positions: vec![
            [-0.5, -0.5, 0.],
            [0.5, -0.5, 0.],
            [0.5, 0.5, 0.],
            [-0.5, 0.5, 0.],
        ],
        uvs: vec![[0., 1.], [1., 1.], [1., 0.], [0., 0.]],
        normals: vec![[0., 0., 1.]; 4],
        indices: vec![0, 1, 2, 0, 2, 3, 0, 2, 1, 0, 3, 2],
        material: Material::Lit,
        texture: Some(Texture::solid([40, 220, 60, 255]).into()),
        ..Default::default()
    };
    square.thinning = Some(Thinning {
        parts: vec![[0., 0., 0., 0.]; 4],
        // Rank 0 draws at every size: only the centre is wanted.
        full: 1.,
        least: 1.,
    });
    // Straight down, nearly.
    let camera = Camera {
        tilt: 1e-3,
        ..Default::default()
    };
    let world = Extent {
        center: [0.; 3],
        size: 6.,
    };
    let green = |pixels: &[u8]| pixels.chunks(4).filter(|p| p[1] > 120 && p[0] < 90).count();
    let draw = |families: &Arc<Families>, family: FamilyId| {
        let renderer = Renderer::with_families(families.clone());
        renderer.set_scene(&Scene {
            light: neutral(),
            ..Default::default()
        });
        let mut mesh = square.clone();
        mesh.family = family;
        renderer.set_pieces(vec![Piece {
            levels: vec![(Arc::new(Mesh::new(mesh).unwrap()), 0.)],
            impostor: None,
            pose: None,
        }]);
        let mut m = math::translate([0., 1., 0.]);
        (m[0], m[5], m[10]) = (2., 2., 2.);
        renderer.set_placements(&[0], &[m]);
        let mut target = Offscreen::new(96, 96).unwrap();
        target.render(&renderer, camera, world).unwrap()
    };
    let board = families.id("board").unwrap();
    let edge_on = green(&draw(&families, FamilyId::LIT));
    let facing = green(&draw(&families, board));
    eprintln!("edge on {edge_on} pixels; facing {facing}");
    // A 2 m square in a 6 m view of 96 pixels: about 32² pixels.
    assert!(edge_on < 100, "edge on: {edge_on} pixels");
    assert!(facing > 700, "facing: {facing} pixels");
}
