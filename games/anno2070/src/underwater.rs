// 2070's view from below the sea's surface, which Deep Ocean islands open
// in: its view mode, the caustic picture, and the passes it draws in place
// of the sky and the sea.
// Engine addresses here are Anno5.exe build 11514956 (Steam).

use std::sync::Arc;

use anno_core::ambient::View;
use anno_formats::vfs::Install;
use gpui_editor_kit::gpu::LockAny as _;
use gpui_editor_kit::gpu::{self, HostTexture, Pass, Stage, ViewMode};
use gpui_editor_kit::texture::Dds;

use crate::scene::WaterLight;

/// The view mode's index in 2070's lighting (`scene::lighting`'s modes).
///
/// The game runs this view while the camera's eye is below y = 0 (Anno5.exe
/// 0x14086E270, 0x140883820): then its lighting pass is
/// `DeferredShading*UnderWater` (deferredshading.fx:428-485, :599-643), the
/// sea surface isn't drawn at all (its from-below technique is commented out,
/// waterocean.fx:491-507), and empty pixels fog to the water's colour (the
/// under-water shader has no `clip`, unlike the surface's at :274). The
/// editor's camera stays free, so an island's view decides it instead: a
/// Deep Ocean island is seen from below, a surface island never.
pub const MODE: u32 = 1;

/// Where the mode finds its profile's water among the host words: two vec4
/// from vec4 0 (`scene::HOST_VEC4` says why there).
pub const HOST_VEC4: usize = 0;

/// The caustic picture deferredshading.fx's `cTexture4` holds (the water
/// manager loads it, Anno5.exe 0x140835A10): b the shallow caustics, g the
/// deep, a noise.
pub const CAUSTICS: &str = "data/graphics/effects/water/caustics.dds";

/// `cWaterCausticsOffset` per second of the renderer's clock.
/// TODO: the engine moves it by the clock at speeds set at run
/// time; the sea's caustics drift by the same estimate.
pub const CAUSTIC_DRIFT: [f32; 4] = [0.021, 0.033, 0.027, -0.017];

/// How far the eye the water is measured from may back off the camera's
/// anchor, and the highest it stands. The game's eye sits below the surface
/// and close in: terrain above it is squashed to stay under -7 m
/// (functions.fxh:527-549) and the far plane is 95 m (camera.xml
/// `m_BackClipUnderWater`), past every profile's view depth (48-58.6 m).
/// The editor's camera backs off hundreds of metres and looks from above,
/// so the water is measured from an eye held 40 m from the anchor and
/// below the surface.
/// TODO: both from a capture of the game's under-water camera.
pub const REACH: f32 = 40.;
pub const EYE_TOP: f32 = -1.;

/// The profile's water seen from below as the mode reads it: absorption
/// exponents and light depth, then view depth and its exponent
/// (`cWaterLightAbsorptionExp`, `cWaterLightDepth`, `cWaterViewDepth`).
pub fn words(w: &WaterLight) -> [[f32; 4]; 2] {
    let [r, g, b] = w.absorption;
    [
        [r, g, b, w.light_depth.max(1e-3)],
        [w.view_depth.max(1e-3), w.view_depth_exponent, 0., 0.],
    ]
}

/// What the mode's shade and the background pass share: the profile's
/// words, the held eye, the scattering colour and the caustics, in gamma
/// space as the game works them. Names start with the mode's.
const COMMON_WGSL: &str = r#"
// The profile's water (`underwater::words`).
fn underwater_absorb() -> vec4<f32> { return lights.host[underwater_host]; }
fn underwater_depths() -> vec4<f32> { return lights.host[underwater_host + 1]; }

fn underwater_gamma(c: vec3<f32>) -> vec3<f32> {
    return pow(max(c, vec3<f32>(1e-4)), vec3<f32>(1.0 / 2.2));
}

fn underwater_linear(c: vec3<f32>) -> vec3<f32> {
    return pow(max(c, vec3<f32>(1e-4)), vec3<f32>(2.2));
}

// The eye the water is measured from: the camera's, held within REACH of
// its anchor and below the surface (underwater.rs `REACH`). Orthographic
// views have none; w = 0 then.
fn underwater_eye() -> vec4<f32> {
    if globals.eye.w < 0.5 { return vec4<f32>(0.0); }
    var eye = globals.eye.xyz;
    let back = eye - globals.anchor.xyz;
    if length(back) > underwater_reach { eye = globals.anchor.xyz + back * (underwater_reach / length(back)); }
    eye.y = min(eye.y, underwater_eye_top);
    return vec4<f32>(eye, 1.0);
}

// The water's own light toward the eye along `to_eye` (deferredshading.fx:470),
// times the sun, gamma-encoded. `to_eye` is the camera's own view ray, never
// the held eye's, as the game's PixelToCam is for surfaces and empty pixels
// alike: then whatever lies past the view depth fogs to exactly the
// background behind it. From the held eye, the empty ocean floor around a
// Deep Ocean island fogged to a paler colour than the background and
// showed as a square.
fn underwater_scattering(to_eye: vec3<f32>, eye_y: f32) -> vec3<f32> {
    let a = underwater_absorb();
    let up = pow(vec3<f32>(clamp(1.0 - (to_eye.y * 0.5 + 0.5), 0.0, 1.0)), a.rgb);
    let s = clamp(up + eye_y / (a.w * 2.0), vec3<f32>(0.0), vec3<f32>(1.0)) * 0.6;
    return underwater_gamma(lights.sun_color.rgb) * s;
}

// ComputeCaustics (deferredshading.fx:73-94): two planar lookups moving
// against each other, the shallow layer (b) giving way to the deep (g) by
// 26 m down, times two noise lookups, yellowed.
fn underwater_caustics(p: vec3<f32>, light_depth: f32) -> vec3<f32> {
    let offset = underwater_drift * lights.view.y;
    let coords = (p.xzxz - p.yyyy * 0.1) * vec4<f32>(0.22, 0.22, -0.14, -0.14) + offset;
    let c0 = host_texture(coords.xy, 0u);
    let c1 = host_texture(coords.zw, 0u);
    let b = clamp(p.y / -26.0, 0.0, 1.0);
    let caustic = mix(c0.b, c0.g, b) * mix(c1.b, c1.g, b);
    let noise = host_texture(p.xz * 0.05 + offset.xy, 0u).a * host_texture(p.xz * 0.07 + offset.zw, 0u).a;
    return vec3<f32>(20.0, 20.0, 16.0) * caustic * light_depth * noise;
}
"#;

/// The mode's shade and fog (`ViewMode` "underwater"): 2070's light as on
/// the surface (`scene_light`), then deferredshading.fx:457-475 in gamma
/// space. The fog does nothing: the shade has fogged already, because glow
/// skips the fog by its share, which a fog after the shade can't tell.
fn shade_wgsl() -> String {
    format!(
        r#"// What the water does to light coming up from `world`, in gamma: lit
// colour, glow share and shadow from the surface's light.
fn underwater_shade(world: vec3<f32>, s: Surface, emissive: vec3<f32>, has_normals: bool) -> vec3<f32> {{
    let lit = scene_light(world, s, emissive, has_normals);
    let a = underwater_absorb();
    let d = underwater_depths();
    let eye = underwater_eye();
    // How far the light came down, and the view went through.
    var distance = 0.0;
    if eye.w > 0.5 {{ distance = length(world - eye.xyz); }}
    let view_depth = clamp(distance / d.x, 0.0, 1.0);
    let light_depth = max(clamp(-world.y / a.w, 0.0, 1.0), clamp(distance / (a.w * 2.0), 0.0, 1.0));
    let c = lit.color + underwater_caustics(world, light_depth) * (0.5 * s.normal.y + 0.5) * max(0.3, lit.shadow);
    let absorption = pow(vec3<f32>(max(1.0 - light_depth, 1e-6)), a.rgb);
    let fogged = mix(c * absorption, underwater_scattering(toward_viewer(world), eye.y), pow(view_depth, d.y));
    return underwater_linear(mix(fogged, c, lit.glow));
}}
{common}"#,
        common = common()
    )
}

/// The mode's unlit colours (flat helpers, `ViewMode::unlit`), as the
/// game's unlit draws under water: effects, lines and effect models take
/// `UnderWaterScattering`'s absorption over the light's depth and no
/// inscattering (effects.fxh:47-56), and fade out by `1 - (d / ViewDepth)²`
/// (`UnderwaterFade`, :38-42; effect_line.fx:121-128). They blend; a flat
/// draw is opaque, so it fades toward what the water shows behind it, the
/// background's colour (`fs_back`). In linear light, as the effects work.
const UNLIT_WGSL: &str = r#"
fn underwater_unlit(color: vec3<f32>, world: vec3<f32>) -> vec3<f32> {
    let a = underwater_absorb();
    let eye = underwater_eye();
    var distance = 0.0;
    if eye.w > 0.5 { distance = length(world - eye.xyz); }
    let light_depth = max(clamp(-world.y / a.w, 0.0, 1.0), clamp(distance / (a.w * 2.0), 0.0, 1.0));
    let absorbed = color * pow(vec3<f32>(max(1.0 - light_depth, 1e-6)), a.rgb);
    let faded = clamp(distance / underwater_depths().x, 0.0, 1.0);
    let behind = underwater_linear(underwater_scattering(toward_viewer(world), eye.y));
    return mix(absorbed, behind, faded * faded);
}
"#;

fn common() -> String {
    let [a, b, c, d] = CAUSTIC_DRIFT;
    format!(
        "const underwater_host: u32 = {HOST_VEC4}u;\n\
         const underwater_reach: f32 = {REACH:?};\n\
         const underwater_eye_top: f32 = {EYE_TOP:?};\n\
         const underwater_drift: vec4<f32> = vec4<f32>({a:?}, {b:?}, {c:?}, {d:?});\n\
         {COMMON_WGSL}"
    )
}

/// 2070's view mode from below the surface, for `scene::lighting`.
pub fn mode() -> ViewMode {
    ViewMode {
        name: "underwater".into(),
        shade: Some(shade_wgsl()),
        fog: Some(
            "fn underwater_fog(color: vec3<f32>, world: vec3<f32>) -> vec3<f32> { return color; }\n"
                .into(),
        ),
        unlit: Some(UNLIT_WGSL.into()),
    }
}

/// The caustic picture as the mode's host texture (layer 0). `None` when
/// the install lacks it: the view then shows no caustics.
pub fn caustics(install: &Install) -> Option<HostTexture> {
    let read = || -> anyhow::Result<HostTexture> {
        let bytes = install.read(CAUSTICS)?;
        let dds = Dds::parse(&bytes)?;
        let top = dds.decode(&bytes, 0)?;
        Ok(HostTexture {
            width: top.width,
            height: top.height,
            layers: 1,
            texels: Arc::new(top.rgba),
        })
    };
    read().map_err(|e| eprintln!("{CAUSTICS}: {e:#}")).ok()
}

/// The view a Deep Ocean island opens in. Empty ocean chunks draw as the
/// game's DeepWaterChunk quads (terrain.fx:260-279): albedo 0.05 with the
/// normal up (layers.wgsl `DEEP_WATER`), lit and fogged as any ground, at
/// `SEABED` (island.rs), the open sea's `DefaultWaterDepth` (-163840, so
/// -40 m, in properties.xml). TODO: the height the engine hands
/// the quads (their `cSpriteData` y) isn't found in Anno5.exe yet;
/// `DeepwaterNoiseMin`/`Max` (-40 to -30 m, "the generated under-water
/// floor") may vary it per chunk.
pub fn view(install: &Install) -> View {
    View {
        name: "Under water",
        mode: MODE,
        host_texture: caustics(install),
        passes: vec![
            Arc::new(Background::default()),
            Arc::new(Sprites::new(sprite_picture(install))),
        ],
    }
}

/// What empty pixels show from below the surface: the water's colour at
/// full view depth (deferredshading.fx:472 with ViewDepth 1, since the far
/// plane lies past every profile's view depth), drawn where nothing else
/// did, in place of the sky.
#[derive(Default)]
pub struct Background {
    pipeline: std::sync::Mutex<gpu::OnDevice<wgpu::RenderPipeline>>,
}

impl Pass for Background {
    fn stage(&self) -> Stage {
        Stage::Sky
    }

    fn replaces(&self) -> bool {
        true
    }

    fn warm(&self, gpu: &gpu::Warm<'_>) {
        warm(gpu);
    }

    fn prepare(&self, frame: &mut gpu::Prepare<'_>) -> bool {
        (self.pipeline.lock_any()).get_or_insert_with(frame.device, || {
            background(frame.device, frame.frame_layout)
        });
        true
    }

    fn record(&self, frame: &gpu::Record<'_>, pass: &mut wgpu::RenderPass<'_>) {
        let pipeline = self.pipeline.lock_any();
        let Some(pipeline) = pipeline.get(frame.device) else {
            return;
        };
        pass.set_pipeline(pipeline);
        pass.draw(0..3, 0..1);
    }
}

const BACKGROUND: &str = "2070 under-water background";

/// Compiles the view's pipelines before an island needs them.
pub fn warm(gpu: &gpu::Warm<'_>) {
    background(gpu.device, gpu.frame_layout);
    sprite_pipelines(gpu.device, gpu.frame_layout);
}

fn background(device: &wgpu::Device, frame_layout: &wgpu::BindGroupLayout) -> wgpu::RenderPipeline {
    gpu::per_device(device, BACKGROUND, || {
        let source = format!(
            "{}{}{}",
            gpu::FRAME_WGSL,
            common(),
            r#"
struct BackOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) ndc: vec2<f32>,
}

@vertex fn vs_back(@builtin(vertex_index) vertex: u32) -> BackOut {
    let p = vec2<f32>(f32((vertex << 1u) & 2u), f32(vertex & 2u)) * 2.0 - 1.0;
    var out: BackOut;
    out.clip = vec4<f32>(p, 0.0, 1.0);
    out.ndc = p;
    return out;
}

@fragment fn fs_back(in: BackOut) -> @location(0) vec4<f32> {
    var to_eye = globals.eye.xyz;
    if globals.eye.w > 0.5 {
        let near = unproject(in.ndc, 1.0);
        to_eye = -normalize(unproject(in.ndc, 1e-3) - near);
    }
    let eye = underwater_eye();
    return vec4<f32>(underwater_linear(underwater_scattering(to_eye, eye.y)), 1.0);
}
"#
        );
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some(BACKGROUND),
            source: wgpu::ShaderSource::Wgsl(source.into()),
        });
        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some(BACKGROUND),
            bind_group_layouts: &[Some(frame_layout)],
            immediate_size: 0,
        });
        device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some(BACKGROUND),
            layout: Some(&layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_back"),
                compilation_options: Default::default(),
                buffers: &[],
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_back"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: gpu::HDR_FORMAT,
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: Default::default(),
            // At z 0 against reversed depth: only where nothing drew.
            depth_stencil: Some(wgpu::DepthStencilState {
                format: gpu::DEPTH_FORMAT,
                depth_write_enabled: Some(false),
                depth_compare: Some(wgpu::CompareFunction::GreaterEqual),
                stencil: Default::default(),
                bias: Default::default(),
            }),
            multisample: Default::default(),
            multiview_mask: None,
            cache: None,
        })
    })
}

/// The picture underwater.fx's plankton and sunrays both read
/// (`cTexSprite`): the underwater object loads only this one (Anno5.exe
/// 0x14086DED0). TODO: `effects/water/sunray.dds` ships beside
/// it and nothing in the exe names it; a capture says which the rays wear.
pub const SPRITES: &str = "data/graphics/effects/water/plankton.dds";

/// Plankton sprites in one 4 m cell (0x14086DED0), drawn again in every
/// cell of a 5×5×5 block around the eye whose top lies under the surface
/// (0x14086E670).
const PLANKTON: u32 = 40;
const CELLS: u32 = 125;
/// Sunrays alive at once. TODO: the engine sizes its list where
/// the exe hasn't shown yet; 24 fill the view's cone without crowding it.
const RAYS: usize = 24;

/// `SPRITES` decoded (RGBA, linear: `cTexSprite` has no sRGB read), or one
/// blank texel without it.
pub fn sprite_picture(install: &Install) -> (u32, u32, Arc<Vec<u8>>) {
    let read = || -> anyhow::Result<(u32, u32, Arc<Vec<u8>>)> {
        let bytes = install.read(SPRITES)?;
        let dds = Dds::parse(&bytes)?;
        let top = dds.decode(&bytes, 0)?;
        Ok((top.width, top.height, Arc::new(top.rgba)))
    };
    read()
        .map_err(|e| eprintln!("{SPRITES}: {e:#}; no plankton or sunrays"))
        .unwrap_or((1, 1, Arc::new(vec![0; 4])))
}

/// One sunray as underwater.fx's `cSpriteData` holds it: where it starts
/// at the surface and how bright it is now, its width and texture offset.
#[derive(Clone, Copy, Default)]
struct Ray {
    at: [f32; 3],
    width: f32,
    life: f32,
    age: f32,
}

/// The sunrays' run: the game's update (0x14086F0D0, 0x14086F5B0) with a
/// seeded generator in place of `rand`, so a still is the same each time.
struct Rays {
    rays: Vec<Ray>,
    seed: u32,
    time: Option<f32>,
}

impl Rays {
    fn random(&mut self) -> f32 {
        // xorshift32: one stream for the pass.
        let mut x = self.seed;
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        self.seed = x;
        (x >> 8) as f32 / (1u32 << 24) as f32
    }

    /// A new ray 20 to 105 m from `eye` (more of them far), inside the
    /// cone `yaw` looks along, living 15 to 30 s (0x14086F5B0).
    fn spawn(&mut self, eye: [f32; 2], yaw: f32) -> Ray {
        let r = self.random();
        let distance = 85. - 85. * r * r + 20.;
        let angle = yaw + (self.random() * 2. - 1.) * CONE;
        Ray {
            at: [
                eye[0] + angle.cos() * distance,
                0.,
                eye[1] + angle.sin() * distance,
            ],
            width: 0.8 + self.random() * 0.8,
            life: 15. + self.random() * 15.,
            age: 0.,
        }
    }

    /// Steps every ray to `time`, the eye at `eye` (XZ) looking along `yaw`:
    /// one that left the 105 m cone starts over, one that lived out its
    /// life starts again elsewhere (0x14086F0D0).
    fn step(&mut self, time: f32, eye: [f32; 2], yaw: f32) {
        let dt = self.time.map_or(0., |t| (time - t).clamp(0., 1.));
        self.time = Some(time);
        while self.rays.len() < RAYS {
            let mut ray = self.spawn(eye, yaw);
            ray.age = self.random() * ray.life;
            self.rays.push(ray);
        }
        let look = [yaw.cos(), yaw.sin()];
        for i in 0..self.rays.len() {
            let ray = self.rays[i];
            let d = [ray.at[0] - eye[0], ray.at[2] - eye[1]];
            let length = (d[0] * d[0] + d[1] * d[1]).sqrt().max(1e-6);
            let facing = (d[0] * look[0] + d[1] * look[1]) / length;
            if length > 105. || facing < CONE.cos() {
                // Out of view: back in it, at a random point of a life.
                let mut fresh = self.spawn(eye, yaw);
                fresh.age = self.random() * fresh.life;
                self.rays[i] = fresh;
            } else if ray.age + dt >= ray.life {
                self.rays[i] = self.spawn(eye, yaw);
            } else {
                self.rays[i].age += dt;
            }
        }
    }

    /// `cSpriteData` as the rays' vertex stage reads it, two vec4 a ray:
    /// start and alpha, then width and texture offset. The alpha is a
    /// smooth pulse over the ray's life (0x14086F0D0).
    /// Rays past those living stay zero, which draws nothing.
    fn words(&self) -> [[f32; 4]; RAYS * 2] {
        let mut out = [[0.; 4]; RAYS * 2];
        for (ray, words) in self.rays.iter().zip(out.chunks_exact_mut(2)) {
            let p = ray.age / ray.life + 0.5;
            let tri = (2. * (p - p.floor()) - 1.).abs();
            let s = 1. - (3. - 2. * tri) * tri * tri;
            let alpha = 1. - s * s;
            words[0] = [ray.at[0], ray.at[1], ray.at[2], alpha];
            words[1] = [25., ray.width, ray.age * 0.1, 0.];
        }
        out
    }
}

/// Half the cone around the view's heading rays live in. TODO:
/// the engine widens the camera's own angle by 0.3 (0x14086F0D0); the
/// game camera's 0.36 makes 0.66.
const CONE: f32 = 0.66;

/// underwater.fx's plankton (alpha blended, instanced) and sunrays
/// (additive) for the under-water view, at Blend over what the view drew.
pub struct Sprites {
    picture: (u32, u32, Arc<Vec<u8>>),
    rays: std::sync::Mutex<Rays>,
    gpu: std::sync::Mutex<gpu::OnDevice<SpriteGpu>>,
}

struct SpriteGpu {
    pipelines: SpritePipelines,
    params: wgpu::Buffer,
    rays: wgpu::Buffer,
    group: wgpu::BindGroup,
}

#[derive(Clone)]
struct SpritePipelines {
    plankton: wgpu::RenderPipeline,
    rays: wgpu::RenderPipeline,
    layout: wgpu::BindGroupLayout,
}

impl Sprites {
    pub fn new(picture: (u32, u32, Arc<Vec<u8>>)) -> Self {
        Self {
            picture,
            rays: std::sync::Mutex::new(Rays {
                rays: Vec::new(),
                seed: 0x2070_5eed,
                time: None,
            }),
            gpu: std::sync::Mutex::default(),
        }
    }

    fn resources(&self, frame: &gpu::Prepare<'_>) -> SpriteGpu {
        let device = frame.device;
        let pipelines = sprite_pipelines(device, frame.frame_layout);
        let params = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some(SPRITE),
            size: 16,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let rays = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some(SPRITE),
            size: (RAYS * 32) as u64,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let (width, height, texels) = &self.picture;
        let size = wgpu::Extent3d {
            width: *width,
            height: *height,
            depth_or_array_layers: 1,
        };
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some(SPRITE),
            size,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        frame.queue.write_texture(
            texture.as_image_copy(),
            texels,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(width * 4),
                rows_per_image: Some(*height),
            },
            size,
        );
        // LINEAR, WRAP (underwater.fx:14).
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some(SPRITE),
            address_mode_u: wgpu::AddressMode::Repeat,
            address_mode_v: wgpu::AddressMode::Repeat,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        let view = texture.create_view(&Default::default());
        let group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some(SPRITE),
            layout: &pipelines.layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: params.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: rays.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::TextureView(&view),
                },
                wgpu::BindGroupEntry {
                    binding: 3,
                    resource: wgpu::BindingResource::Sampler(&sampler),
                },
            ],
        });
        SpriteGpu {
            pipelines,
            params,
            rays,
            group,
        }
    }
}

impl Pass for Sprites {
    fn stage(&self) -> Stage {
        Stage::Blend
    }

    fn animating(&self) -> bool {
        true
    }

    fn warm(&self, gpu: &gpu::Warm<'_>) {
        sprite_pipelines(gpu.device, gpu.frame_layout);
    }

    fn prepare(&self, frame: &mut gpu::Prepare<'_>) -> bool {
        let Some(eye) = frame.eye else {
            // Orthographic: no eye to be under water with.
            return false;
        };
        // The eye the water is measured from, as `underwater_eye` holds it.
        let back = [eye[0] - frame.focus[0], eye[2] - frame.focus[1]];
        let length = (back[0] * back[0] + back[1] * back[1]).sqrt();
        let scale = if length > REACH { REACH / length } else { 1. };
        let held = [
            frame.focus[0] + back[0] * scale,
            frame.focus[1] + back[1] * scale,
        ];
        let yaw = (-back[1]).atan2(-back[0]);
        let mut rays = self.rays.lock_any();
        rays.step(frame.time, held, yaw);
        let mut gpu = self.gpu.lock_any();
        let gpu = gpu.get_or_insert_with(frame.device, || self.resources(frame));
        frame.queue.write_buffer(
            &gpu.params,
            0,
            bytemuck::cast_slice(&[frame.time, 1., rays.rays.len() as f32, 0.]),
        );
        frame
            .queue
            .write_buffer(&gpu.rays, 0, bytemuck::cast_slice(&rays.words()));
        true
    }

    fn record(&self, frame: &gpu::Record<'_>, pass: &mut wgpu::RenderPass<'_>) {
        let gpu = self.gpu.lock_any();
        let Some(gpu) = gpu.get(frame.device) else {
            return;
        };
        pass.set_bind_group(1, &gpu.group, &[]);
        pass.set_pipeline(&gpu.pipelines.plankton);
        pass.draw(0..6, 0..PLANKTON * CELLS);
        pass.set_pipeline(&gpu.pipelines.rays);
        pass.draw(0..6, 0..RAYS as u32);
    }
}

const SPRITE: &str = "2070 under-water sprites";

fn sprite_pipelines(
    device: &wgpu::Device,
    frame_layout: &wgpu::BindGroupLayout,
) -> SpritePipelines {
    gpu::per_device(device, SPRITE, || {
        let source = [gpu::FRAME_WGSL, &common(), include_str!("underwater.wgsl")].concat();
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some(SPRITE),
            source: wgpu::ShaderSource::Wgsl(source.into()),
        });
        let entry = |binding, visibility, ty| wgpu::BindGroupLayoutEntry {
            binding,
            visibility,
            ty,
            count: None,
        };
        let both = wgpu::ShaderStages::VERTEX_FRAGMENT;
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some(SPRITE),
            entries: &[
                entry(
                    0,
                    both,
                    wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                ),
                entry(
                    1,
                    wgpu::ShaderStages::VERTEX,
                    wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Storage { read_only: true },
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                ),
                entry(
                    2,
                    wgpu::ShaderStages::FRAGMENT,
                    wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                ),
                entry(
                    3,
                    wgpu::ShaderStages::FRAGMENT,
                    wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                ),
            ],
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some(SPRITE),
            bind_group_layouts: &[Some(frame_layout), Some(&layout)],
            immediate_size: 0,
        });
        let pipeline = |vertex: &str, fragment: &str, blend: wgpu::BlendState| {
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some(SPRITE),
                layout: Some(&pipeline_layout),
                vertex: wgpu::VertexState {
                    module: &shader,
                    entry_point: Some(vertex),
                    compilation_options: Default::default(),
                    buffers: &[],
                },
                fragment: Some(wgpu::FragmentState {
                    module: &shader,
                    entry_point: Some(fragment),
                    compilation_options: Default::default(),
                    targets: &[Some(wgpu::ColorTargetState {
                        format: gpu::HDR_FORMAT,
                        blend: Some(blend),
                        write_mask: wgpu::ColorWrites::COLOR,
                    })],
                }),
                primitive: Default::default(),
                // Depth tested, not written (underwater.fxs:8-11, :24-27).
                depth_stencil: Some(wgpu::DepthStencilState {
                    format: gpu::DEPTH_FORMAT,
                    depth_write_enabled: Some(false),
                    depth_compare: Some(wgpu::CompareFunction::GreaterEqual),
                    stencil: Default::default(),
                    bias: Default::default(),
                }),
                multisample: Default::default(),
                multiview_mask: None,
                cache: None,
            })
        };
        // SrcAlpha, InvSrcAlpha (underwater.fxs:13-18); SrcAlpha, One for
        // the rays (:29-34).
        let add = wgpu::BlendState {
            color: wgpu::BlendComponent {
                src_factor: wgpu::BlendFactor::SrcAlpha,
                dst_factor: wgpu::BlendFactor::One,
                operation: wgpu::BlendOperation::Add,
            },
            alpha: wgpu::BlendComponent::OVER,
        };
        SpritePipelines {
            plankton: pipeline(
                "vs_plankton",
                "fs_plankton",
                wgpu::BlendState::ALPHA_BLENDING,
            ),
            rays: pipeline("vs_ray", "fs_ray", add),
            layout,
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scene::tests::{north, probe, profile_glow, reference};
    use crate::scene::{Profile, lighting, to_gamma};
    use gpui_editor_kit::camera::{Camera, Extent};
    use gpui_editor_kit::gpu::{
        Curve, Families, Light, Material, Mesh, MeshData, Offscreen, Piece, Renderer, Scene, Tone,
    };
    use gpui_editor_kit::gpui::{point, px};
    use gpui_editor_kit::math;

    /// The seabed the probes lie on: a Deep Ocean plateau's depth.
    const FLOOR: f32 = -28.;
    /// The caustic picture's every texel: shallow and deep caustics at
    /// `C`, noise full. A flat picture drifts to itself, so the frame
    /// doesn't follow the clock.
    const C: u8 = 40;

    fn flat_caustics() -> HostTexture {
        HostTexture {
            width: 4,
            height: 4,
            layers: 1,
            texels: Arc::new([0, C, C, 255].repeat(16)),
        }
    }

    /// What 2070 shows from below the surface for the probe's surface at
    /// `world` (deferredshading.fx:455-482), gamma-encoded: the surface's
    /// light, caustics under a flat picture, then absorption and the
    /// water's scattering, which glow skips. `caustics` false leaves them
    /// out, `glow_skips` false fogs glow like the rest: the terms the test
    /// checks move pixels.
    fn under(
        profile: &Profile,
        sun: [f32; 3],
        eye: [f32; 3],
        anchor: [f32; 3],
        world: [f32; 3],
        look: [f32; 4],
        caustics: bool,
        glow_skips: bool,
    ) -> [f32; 3] {
        let lit = reference(profile, sun, eye, anchor, world, look, false);
        let w = &profile.water.under;
        let back: [f32; 3] = std::array::from_fn(|a| eye[a] - anchor[a]);
        let len = back.iter().map(|v| v * v).sum::<f32>().sqrt();
        let mut held = eye;
        if len > REACH {
            held = std::array::from_fn(|a| anchor[a] + back[a] * (REACH / len));
        }
        held[1] = held[1].min(EYE_TOP);
        let d: [f32; 3] = std::array::from_fn(|a| held[a] - world[a]);
        let distance = d.iter().map(|v| v * v).sum::<f32>().sqrt();
        // The water's colour along the camera's own ray (PixelToCam).
        let to_eye = toward(eye, world);
        let view_depth = (distance / w.view_depth).clamp(0., 1.);
        let light_depth = (-world[1] / w.light_depth)
            .clamp(0., 1.)
            .max((distance / (w.light_depth * 2.)).clamp(0., 1.));
        let c = f32::from(C) / 255.;
        let gain = [20., 20., 16.].map(|g| g * c * c * light_depth * f32::from(u8::from(caustics)));
        let sun_gamma = profile.sun().map(|v| to_gamma(crate::scene::to_linear(v)));
        let glow = look[0];
        std::array::from_fn(|i| {
            let lit = lit[i] + gain[i];
            let absorption = (1. - light_depth).max(1e-6).powf(w.absorption[i]);
            let up = (1. - (to_eye[1] * 0.5 + 0.5))
                .clamp(0., 1.)
                .powf(w.absorption[i]);
            let scattering =
                sun_gamma[i] * (up + held[1] / (w.light_depth * 2.)).clamp(0., 1.) * 0.6;
            let t = view_depth.powf(w.view_depth_exponent);
            let fogged = lit * absorption + (scattering - lit * absorption) * t;
            match glow_skips {
                true => fogged + (lit - fogged) * glow,
                false => fogged,
            }
        })
    }

    // The under-water mode lights 2070's way on the GPU and the plain mode
    // stays what it was: probes on a Deep Ocean floor from 10 m to 60 m out,
    // glowing and not, drawn by one renderer of the one family set in the
    // plain mode, the under-water mode and the plain mode again. The plain
    // frames are byte for byte the frame of the lighting without modes, and
    // each other's; the under-water frame matches deferredshading.fx's
    // formula worked on the CPU within 3 levels, and its caustics, its fog
    // and glow skipping the fog each move pixels.
    #[test]
    fn the_under_water_mode_lights_as_2070_does_and_switching_back_restores_the_surface() {
        let profile = north();
        let families = Families::with_lighting(vec![probe()], lighting()).unwrap();
        let mut plain_lighting = lighting();
        plain_lighting.modes.clear();
        let plain_families = Families::with_lighting(vec![probe()], plain_lighting).unwrap();
        let (w, h) = (480u32, 320u32);
        let mut target = Offscreen::new(w, h).unwrap();
        let camera = Camera {
            fov: 0.9,
            tilt: 0.7,
            yaw: 0.4,
            ..Camera::default()
        };
        let world = Extent {
            center: [0., FLOOR, 0.],
            size: 30.,
        };
        let bounds = target.bounds();
        let eye = camera.eye(bounds, world).unwrap();
        let anchor = camera.anchor(world);
        let looks: [[f32; 4]; 6] = [
            [0., 0., 1., 1.],
            [0., 1., 1., 0.8],
            [0.5, 0.5, 0.8, 0.05],
            [1., 0., 1., 0.02],
            [0.25, 1., 0.6, 0.3],
            [0., 0.5, 0.4, 1.],
        ];
        let byte = |x: f32| (x * 255.).round() as u32;
        let (mut rows, mut words, mut cases) = (Vec::new(), Vec::new(), Vec::new());
        for (r, y) in [300u32, 240, 180, 130, 90].into_iter().enumerate() {
            for (c, x) in [60u32, 180, 300, 420].into_iter().enumerate() {
                let ray = camera.ray(point(px(x as f32 + 0.5), px(y as f32 + 0.5)), bounds, world);
                assert!(ray.direction[1] < 0., "pixel {x},{y} sees no floor");
                let t = (f64::from(FLOOR) - ray.origin[1]) / ray.direction[1];
                let at: [f32; 3] =
                    std::array::from_fn(|a| (ray.origin[a] + ray.direction[a] * t) as f32);
                let mut m = math::translate(at);
                m[0] = 2.5;
                m[10] = 2.5;
                let look = looks[(r * 4 + c) % looks.len()].map(|v| byte(v) as f32 / 255.);
                rows.push(m);
                words.push(
                    byte(look[0]) | byte(look[1]) << 8 | byte(look[2]) << 16 | byte(look[3]) << 24,
                );
                cases.push(((x, y), at, look));
            }
        }
        let quad = |families: &Families| MeshData {
            positions: vec![
                [-0.5, 0., -0.5],
                [0.5, 0., -0.5],
                [0.5, 0., 0.5],
                [-0.5, 0., 0.5],
            ],
            normals: vec![[0., 1., 0.]; 4],
            indices: vec![0, 1, 2, 0, 2, 3, 0, 2, 1, 0, 3, 2],
            material: Material::Lit,
            family: families.id("probe").unwrap(),
            ..MeshData::default()
        };
        let sun = profile.toward_sun();
        let scene = |mode: u32| Scene {
            light: Light {
                sun,
                sun_color: profile.sun().map(crate::scene::to_linear),
                sky: profile.ambient().map(crate::scene::to_linear),
                ground: profile.ambient().map(crate::scene::to_linear),
                shadows: None,
                fog: None,
            },
            tone: Some(Tone {
                exposure: 1.,
                sun_occlusion: 0.,
                ambient: 1.,
                curve: Curve::Shoulder {
                    gamma: 2.2,
                    max_linear: 1.,
                    white: crate::scene::GLOW_BASE,
                },
                auto: None,
            }),
            glow: profile_glow(&profile),
            view_mode: mode,
            host_texture: Some(flat_caustics()),
            ..Default::default()
        };
        let placed = |families: &Arc<Families>| {
            let renderer = Renderer::with_families(families.clone());
            renderer.set_pieces(vec![Piece {
                levels: vec![(Arc::new(Mesh::new(quad(families)).unwrap()), 0.)],
                impostor: None,
                pose: None,
            }]);
            renderer.set_placements(&vec![0; rows.len()], &rows);
            renderer.set_instance_words(0, &words);
            renderer
        };
        let renderer = placed(&families);
        let mut frame = |renderer: &Renderer, mode: u32| {
            renderer.set_scene(&scene(mode));
            target.render(renderer, camera, world).unwrap()
        };
        let surface = frame(&renderer, 0);
        let below = frame(&renderer, MODE);
        let back = frame(&renderer, 0);
        assert!(Arc::ptr_eq(&renderer.families(), &families));
        let without_modes = frame(&placed(&plain_families), 0);
        assert!(
            surface == without_modes,
            "the plain mode drew apart from the lighting without modes"
        );
        assert!(surface == back, "switching back changed the frame");

        let (mut worst, mut moved) = (0f32, [0u32; 3]);
        for &((x, y), at, look) in &cases {
            let i = ((y * w + x) * 4) as usize;
            let got = [below[i], below[i + 1], below[i + 2]];
            let shown = |c: [f32; 3]| c.map(|v| v.clamp(0., 1.) * 255.);
            let want = shown(under(&profile, sun, eye, anchor, at, look, true, true));
            let off = (0..3)
                .map(|c| (f32::from(got[c]) - want[c]).abs())
                .fold(0., f32::max);
            worst = worst.max(off);
            assert!(
                off <= 3.,
                "{x},{y} look {look:?}: drew {got:?}, 2070 shows {want:?}"
            );
            let apart = |o: [f32; 3]| (0..3).any(|c| (o[c] - want[c]).abs() > 10.);
            let without_caustics = shown(under(&profile, sun, eye, anchor, at, look, false, true));
            let fogged_glow = shown(under(&profile, sun, eye, anchor, at, look, true, false));
            let surface_look = shown(reference(&profile, sun, eye, anchor, at, look, false));
            for (n, o) in moved
                .iter_mut()
                .zip([without_caustics, fogged_glow, surface_look])
            {
                *n += u32::from(apart(o));
            }
        }
        eprintln!(
            "worst {worst} levels; caustics, glow skipping and the water moved {moved:?} of 20"
        );
        assert!(moved.iter().all(|&n| n >= 3), "{moved:?}");
    }

    fn toward(eye: [f32; 3], world: [f32; 3]) -> [f32; 3] {
        let d: [f32; 3] = std::array::from_fn(|a| eye[a] - world[a]);
        let len = d.iter().map(|v| v * v).sum::<f32>().sqrt();
        d.map(|v| v / len)
    }

    /// The eye the water is measured from (`underwater_eye`).
    fn held(eye: [f32; 3], anchor: [f32; 3]) -> [f32; 3] {
        let back: [f32; 3] = std::array::from_fn(|a| eye[a] - anchor[a]);
        let len = back.iter().map(|v| v * v).sum::<f32>().sqrt();
        let mut held = eye;
        if len > REACH {
            held = std::array::from_fn(|a| anchor[a] + back[a] * (REACH / len));
        }
        held[1] = held[1].min(EYE_TOP);
        held
    }

    /// What 2070 shows of an unlit white draw at `world` under water,
    /// gamma-encoded: the effects' absorption over the light's depth
    /// (effects.fxh:47-56) faded by `(d / ViewDepth)²` (:38-42) toward the
    /// water's colour behind it (deferredshading.fx:470).
    fn unlit_under(
        profile: &Profile,
        eye: [f32; 3],
        anchor: [f32; 3],
        world: [f32; 3],
    ) -> [f32; 3] {
        let w = &profile.water.under;
        let held = held(eye, anchor);
        let d: [f32; 3] = std::array::from_fn(|a| held[a] - world[a]);
        let distance = d.iter().map(|v| v * v).sum::<f32>().sqrt();
        let light_depth = (-world[1] / w.light_depth)
            .clamp(0., 1.)
            .max((distance / (w.light_depth * 2.)).clamp(0., 1.));
        let fade = (distance / w.view_depth).clamp(0., 1.).powi(2);
        let to_eye = toward(eye, world);
        let sun_gamma = profile.sun().map(|v| to_gamma(crate::scene::to_linear(v)));
        std::array::from_fn(|i| {
            let absorbed = (1. - light_depth).max(1e-6).powf(w.absorption[i]);
            let up = (1. - (to_eye[1] * 0.5 + 0.5))
                .clamp(0., 1.)
                .powf(w.absorption[i]);
            let behind = sun_gamma[i] * (up + held[1] / (w.light_depth * 2.)).clamp(0., 1.) * 0.6;
            let behind = crate::scene::to_linear(behind);
            to_gamma(absorbed + (behind - absorbed) * fade)
        })
    }

    // Unlit (flat) draws under water take the water as 2070's unlit
    // effects do: white squares on a Deep Ocean floor from near the eye to
    // past the view depth draw what the effects' formula gives within 3
    // levels, and in the plain mode stay white.
    #[test]
    fn flat_draws_under_water_take_the_waters_absorption_and_fade() {
        let profile = north();
        let families = Families::with_lighting(Vec::new(), lighting()).unwrap();
        let (w, h) = (480u32, 320u32);
        let mut target = Offscreen::new(w, h).unwrap();
        let camera = Camera {
            fov: 0.9,
            tilt: 0.7,
            yaw: 0.4,
            ..Camera::default()
        };
        let world = Extent {
            center: [0., FLOOR, 0.],
            size: 30.,
        };
        let bounds = target.bounds();
        let eye = camera.eye(bounds, world).unwrap();
        let anchor = camera.anchor(world);
        let mut cases = Vec::new();
        for (x, y) in [
            (240u32, 300u32),
            (240, 220),
            (240, 150),
            (100, 100),
            (380, 90),
        ] {
            let ray = camera.ray(point(px(x as f32 + 0.5), px(y as f32 + 0.5)), bounds, world);
            let t = (f64::from(FLOOR) - ray.origin[1]) / ray.direction[1];
            let at: [f32; 3] =
                std::array::from_fn(|a| (ray.origin[a] + ray.direction[a] * t) as f32);
            cases.push(((x, y), at));
        }
        let quad = MeshData {
            positions: vec![[-1., 0., -1.], [1., 0., -1.], [1., 0., 1.], [-1., 0., 1.]],
            normals: vec![[0., 1., 0.]; 4],
            indices: vec![0, 1, 2, 0, 2, 3, 0, 2, 1, 0, 3, 2],
            material: Material::Flat,
            ..MeshData::default()
        };
        let renderer = Renderer::with_families(families);
        renderer.set_pieces(vec![Piece {
            levels: vec![(Arc::new(Mesh::new(quad).unwrap()), 0.)],
            impostor: None,
            pose: None,
        }]);
        let rows: Vec<_> = cases.iter().map(|(_, at)| math::translate(*at)).collect();
        renderer.set_placements(&vec![0; rows.len()], &rows);
        let mut frame = |mode: u32| {
            renderer.set_scene(&Scene {
                light: Light {
                    sun: profile.toward_sun(),
                    sun_color: profile.sun().map(crate::scene::to_linear),
                    shadows: None,
                    fog: None,
                    ..Light::default()
                },
                tone: Some(Tone {
                    exposure: 1.,
                    sun_occlusion: 0.,
                    ambient: 1.,
                    curve: Curve::Shoulder {
                        gamma: 2.2,
                        max_linear: 1.,
                        white: crate::scene::GLOW_BASE,
                    },
                    auto: None,
                }),
                glow: profile_glow(&profile),
                view_mode: mode,
                host_texture: Some(flat_caustics()),
                ..Default::default()
            });
            target.render(&renderer, camera, world).unwrap()
        };
        let surface = frame(0);
        let below = frame(MODE);
        let mut shades = Vec::new();
        for &((x, y), at) in &cases {
            let i = ((y * w + x) * 4) as usize;
            assert_eq!(&surface[i..i + 3], &[255; 3], "{x},{y} on the surface");
            let got = [below[i], below[i + 1], below[i + 2]];
            let want = unlit_under(&profile, eye, anchor, at).map(|v| v.clamp(0., 1.) * 255.);
            let off = (0..3)
                .map(|c| (f32::from(got[c]) - want[c]).abs())
                .fold(0., f32::max);
            assert!(off <= 3., "{x},{y}: drew {got:?}, 2070 shows {want:?}");
            shades.push(got);
        }
        // Near and far apart: the fade reaches the far squares.
        assert!(shades.first() != shades.last(), "{shades:?}");
    }
}

#[cfg(test)]
mod sprite_tests {
    use super::*;
    use crate::scene::tests::{north, profile_glow};
    use crate::scene::{families, to_linear};
    use gpui_editor_kit::camera::{Camera, Extent};
    use gpui_editor_kit::gpu::{Light, Offscreen, Renderer, Scene};

    // The sunrays' run keeps every ray within 105 m of the eye inside the
    // cone it looks along, ages them with the clock, and from the same
    // seed runs the same: a still shows the same rays each time.
    #[test]
    fn sunrays_stay_in_the_view_cone_and_run_the_same_from_one_seed() {
        let run = || {
            let mut rays = Rays {
                rays: Vec::new(),
                seed: 7,
                time: None,
            };
            let mut seen = Vec::new();
            for step in 0..200 {
                // The eye walks and turns.
                let t = step as f32 * 0.25;
                let (eye, yaw) = ([t * 0.5, -t * 0.2], t * 0.02);
                rays.step(t, eye, yaw);
                assert_eq!(rays.rays.len(), RAYS);
                for ray in &rays.rays {
                    let d = [ray.at[0] - eye[0], ray.at[2] - eye[1]];
                    let length = (d[0] * d[0] + d[1] * d[1]).sqrt();
                    let facing = (d[0] * yaw.cos() + d[1] * yaw.sin()) / length;
                    // Spawned 20 m out or more; the eye may walk closer since.
                    assert!(length <= 105.01, "{length} m out");
                    assert!(facing >= CONE.cos() - 1e-4, "{facing} off the heading");
                    assert!(ray.age >= 0. && ray.age < ray.life);
                    assert!((15. ..=30.).contains(&ray.life));
                }
                seen.push(rays.words());
            }
            seen
        };
        let (a, b) = (run(), run());
        assert!(a == b, "one seed ran two ways");
        // Alphas pulse between 0 and 1 over a life.
        let alphas: Vec<f32> = a
            .iter()
            .flat_map(|w| w.iter().step_by(2).map(|v| v[3]))
            .collect();
        assert!(alphas.iter().all(|a| (0. ..=1.).contains(a)));
        assert!(alphas.iter().any(|&a| a > 0.9) && alphas.iter().any(|&a| a < 0.1));
    }

    // Plankton and sunrays draw over the view from below: with them listed
    // the frame changes in places, not everywhere, over the water's
    // green-blue background, which the plain view doesn't draw.
    #[test]
    fn plankton_and_sunrays_draw_over_the_view_from_below() {
        let profile = north();
        // A picture that lets every sprite show fully.
        let sprites: Arc<dyn Pass> = Arc::new(Sprites::new((1, 1, Arc::new(vec![255; 4]))));
        let background: Arc<dyn Pass> = Arc::new(Background::default());
        let camera = Camera {
            fov: 0.9,
            tilt: 1.35,
            yaw: 0.4,
            ..Camera::default()
        };
        let world = Extent {
            center: [0., -12., 0.],
            size: 6.,
        };
        let draw = |mode: u32, passes: Vec<Arc<dyn Pass>>| {
            let renderer = Renderer::with_families(families());
            renderer.set_scene(&Scene {
                light: Light {
                    sun: profile.toward_sun(),
                    sun_color: profile.sun().map(to_linear),
                    sky: profile.ambient().map(to_linear),
                    ground: profile.ambient().map(to_linear),
                    shadows: None,
                    fog: None,
                },
                glow: profile_glow(&profile),
                view_mode: mode,
                passes,
                ..Default::default()
            });
            let mut target = Offscreen::new(240, 160).unwrap();
            target.render(&renderer, camera, world).unwrap()
        };
        let bare = draw(MODE, vec![background.clone()]);
        let drawn = draw(MODE, vec![background.clone(), sprites.clone()]);
        let moved = (bare.chunks(4).zip(drawn.chunks(4)))
            .filter(|(p, q)| p != q)
            .count();
        eprintln!("sprites moved {moved} of {} pixels", 240 * 160);
        // No upper bound: the rays light nearly the whole view by design
        // (37,624 of 38,400 pixels alone), and how many follows the
        // renderer's clock, which a loaded machine reaches later.
        assert!(moved > 200, "the sprites moved {moved} pixels");
        // Looking down, the water behind them is green-blue: the profile
        // absorbs red first.
        let bottom = (159 * 240 + 120) * 4;
        let below: [u8; 3] = [bare[bottom], bare[bottom + 1], bare[bottom + 2]];
        assert!(
            below[1] > below[0] && below[2] > below[0],
            "the water's colour: {below:?}"
        );
        // The plain mode draws no water: the view's passes belong to it.
        assert!(draw(0, Vec::new()) != bare);
    }
}
