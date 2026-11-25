// Owns 2070's sky as the island view draws it from the camera (sky.wgsl):
// the shown profile's gradient, sun and clouds, the noise volume its
// clouds come from, and the one Sky-stage pass that draws it in place of
// the kit's backdrop. The scene's haze is 2070's fog (scene.rs), so there
// is no haze pass.
// Engine addresses here are Anno5.exe build 11514956 (Steam).

use std::sync::{Arc, Mutex};

use anno_formats::vfs::Install;
use gpui_editor_kit::gpu::LockAny as _;
use gpui_editor_kit::gpu::{self, Pass, Stage};

use crate::scene::{self, FAR, MAX_DISTANCE, Profile};

/// What the clouds are cut from: 64³ bytes of noise, alpha only
/// (sky.fx's `cTexture0`, bound by Anno5.exe 0x14084C8B0).
pub const NOISE: &str = "data/graphics/effects/disaster/noisevolume.dds";

/// How fast the two cloud samples turn about the vertical: radians per
/// millisecond of the engine's clock (0x14084D050, which builds
/// `cMtxCloudRotate0` and `cMtxCloudRotate1`).
/// TODO: which way they turn depends on how the engine hands
/// D3DX's row-vector matrices to the shader; a capture tells.
const TURN_PER_MS: [f32; 2] = [-1.0e-5, -5.0e-6];

/// The uniform as sky.wgsl's `Look`: seven vec4.
pub type Look = [[f32; 4]; 7];

/// A cube of noise bytes, `side` along each axis, x fastest.
#[derive(Clone, Debug, PartialEq)]
pub struct Volume {
    pub side: u32,
    pub texels: Vec<u8>,
}

impl Volume {
    /// An uncompressed alpha-only volume DDS, as `NOISE` is.
    pub fn parse(bytes: &[u8]) -> anyhow::Result<Self> {
        use anyhow::ensure;
        let word = |at: usize| -> u32 {
            bytes
                .get(at..at + 4)
                .map_or(0, |b| u32::from_le_bytes(b.try_into().unwrap()))
        };
        ensure!(bytes.starts_with(b"DDS "), "not a DDS");
        let (height, width, depth) = (word(12), word(16), word(24));
        // DDPF_ALPHA, 8 bits, alpha mask 0xff.
        let (flags, bits, alpha) = (word(80), word(88), word(104));
        ensure!(
            flags == 2 && bits == 8 && alpha == 0xff,
            "not 8-bit alpha (flags {flags:#x}, {bits} bits)"
        );
        ensure!(
            width == height && height == depth && width > 0,
            "not a cube: {width}×{height}×{depth}"
        );
        let len = (width * height * depth) as usize;
        let texels = bytes.get(128..128 + len).map(<[u8]>::to_vec);
        let texels = texels.ok_or_else(|| anyhow::anyhow!("short: {} bytes", bytes.len()))?;
        Ok(Self {
            side: width,
            texels,
        })
    }

    /// A clear sky's: one empty texel.
    pub fn empty() -> Self {
        Self {
            side: 1,
            texels: vec![0],
        }
    }
}

/// The volume as the GPU filters it: trilinear, wrapping, texel
/// centres at half steps.
pub fn sample(v: &Volume, p: [f32; 3]) -> f32 {
    let n = v.side as i64;
    let at = p.map(|c| c * v.side as f32 - 0.5);
    let base = at.map(|c| c.floor());
    let frac: [f32; 3] = std::array::from_fn(|a| at[a] - base[a]);
    let mut sum = 0.;
    for corner in 0..8 {
        let mut weight = 1.;
        let mut index = 0i64;
        let mut stride = 1i64;
        for a in 0..3 {
            let up = corner >> a & 1 == 1;
            weight *= if up { frac[a] } else { 1. - frac[a] };
            let i = (base[a] as i64 + i64::from(up)).rem_euclid(n);
            index += i * stride;
            stride *= n;
        }
        sum += weight * f32::from(v.texels[index as usize]) / 255.;
    }
    sum
}

/// Texels along an environment cube face's edge. sky.fx's comment says the
/// game's cube is "very small"; 32 keeps the clouds' shapes.
/// TODO: the cube's size, from a capture or the engine.
pub const CUBE_SIDE: u32 = 32;

/// The environment cube 2070 renders from its sky each frame (sky.fx
/// `SkyCubeMap`), which its models, sea, lakes and rivers reflect: the sky
/// mirrored at the horizon, clouds thickening toward it, and the contrast
/// raised where clouds are (`lerp(c·0.8, c²·3, saturate(4·density))`).
/// Linear, as the kit keeps its environment; 2070 reads it gamma-encoded.
/// Taken from an eye at sea level, with the clouds at `ms`.
/// TODO: the game redraws it as the clouds turn and from the
/// camera's height; the editor's holds one per profile.
pub fn environment(profile: &Profile, volume: &Volume, ms: f32) -> gpu::Environment {
    let look = look(profile, ms);
    gpu::Environment::from_fn(CUBE_SIDE, |d| {
        let (c, density) = sky_color(&look, volume, d, true);
        let t = (4. * density).clamp(0., 1.);
        c.map(|c| crate::scene::to_linear(c * 0.8 + (c * c * 3. - c * 0.8) * t))
    })
}

/// sky.fx's ComputeSkyColor toward unit `p` from sea level, the clouds as
/// turned by `look`: the gamma-encoded colour and the cloud density. As
/// `SkyCubeMap` takes it when `mirrored`: the view and the clouds' fade
/// mirrored at the horizon, the density thinning toward the zenith
/// (`|y|·cCloudColor.w`) rather than ps_Sky's `saturate(2y)`.
pub fn sky_color(look: &Look, v: &Volume, p: [f32; 3], mirrored: bool) -> ([f32; 3], f32) {
    let saturate = |x: f32| x.clamp(0., 1.);
    let y = if mirrored { p[1].abs() } else { p[1] };
    let turned = |a: f32, s: [f32; 3]| {
        let q = [
            a.cos() * p[0] + a.sin() * p[2],
            p[1],
            a.cos() * p[2] - a.sin() * p[0],
        ];
        std::array::from_fn::<f32, 3, _>(|k| q[k] * s[k] * (1. - y))
    };
    let [turn0, turn1, ..] = look[6];
    let clouds = sample(v, turned(turn0, [0.5, 3., 0.5]))
        + sample(v, turned(turn1, [2., 10., 2.]))
        + sample(v, turned(-turn1, [0.3, 0.9, 0.3]));
    let thickness = if mirrored { y } else { saturate(2. * y) };
    let cloud = saturate(clouds - 1.) * thickness * look[5][3];
    let zenith = 1. - saturate(y);
    let toward = look[4];
    let d = [p[0], y, p[2]];
    let sun_angle = d[0] * toward[0] + d[1] * toward[1] + d[2] * toward[2];
    let glow = (saturate(saturate(sun_angle) - toward[3]) * look[3][3])
        .max(0.)
        .powf(look[2][3]);
    let sun_angle = saturate(sun_angle * sun_angle * 0.3 + 0.7);
    let color = std::array::from_fn(|c| {
        let t = zenith.powf(look[2][c]);
        let sky = sun_angle * (look[0][c] + (look[1][c] - look[0][c]) * t) + look[3][c] * glow;
        let shade = look[5][c] * (1. - saturate(cloud));
        sky + (shade - sky) * saturate(8. * cloud)
    });
    (color, cloud)
}

/// The uniform for `profile`, the clouds turned to `ms` on the engine's
/// clock.
pub fn look(profile: &Profile, ms: f32) -> Look {
    let s = profile.scatter();
    let (a, l) = (&profile.atmosphere, &profile.light);
    let sun = a.sun_color.map(|c| c * l.sun_color_power);
    let cloud = a.cloud_color.map(|c| c * l.cloud_color_power);
    let toward = profile.toward_sun();
    let rgb = |c: [f32; 3], w: f32| [c[0], c[1], c[2], w];
    [
        rgb(s.zenith, FAR),
        rgb(s.horizon, MAX_DISTANCE),
        rgb(s.horizon_falloff, s.sun_falloff),
        rgb(sun, s.sun_spread),
        rgb(toward, s.sun_outer),
        rgb(cloud, a.cloud_density),
        [TURN_PER_MS[0] * ms, TURN_PER_MS[1] * ms, 0., 0.],
    ]
}

/// 2070's sky pass for one island view (`SceneParts::sky`).
pub struct SkyPasses {
    shared: Arc<Shared>,
}

struct Shared {
    volume: Volume,
    /// The shown profile.
    shown: Mutex<Option<Profile>>,
    /// The engine-clock milliseconds the clouds hold at; `None` follows
    /// the renderer's clock.
    held: Mutex<Option<f32>>,
    /// The shown profile's environment cube, by the profile's name.
    cube: Mutex<Option<(String, gpu::Environment)>>,
    gpu: Mutex<gpu::OnDevice<Gpu>>,
}

struct Gpu {
    pipelines: Pipelines,
    buffer: wgpu::Buffer,
    group: wgpu::BindGroup,
}

#[derive(Clone)]
struct Pipelines {
    sky: wgpu::RenderPipeline,
    layout: wgpu::BindGroupLayout,
}

impl SkyPasses {
    /// Reads the cloud noise from `install`; without it the sky shows clear.
    pub fn new(install: &Install) -> Self {
        let volume = (install.read(NOISE))
            .and_then(|bytes| Volume::parse(&bytes))
            .map_err(|e| eprintln!("{NOISE}: {e:#}; 2070's sky shows no clouds"))
            .unwrap_or_else(|_| Volume::empty());
        Self::with_volume(volume)
    }

    pub fn with_volume(volume: Volume) -> Self {
        Self {
            shared: Arc::new(Shared {
                volume,
                shown: Mutex::new(None),
                held: Mutex::new(None),
                cube: Mutex::new(None),
                gpu: Mutex::default(),
            }),
        }
    }

    /// Holds the clouds at `ms` on the engine's clock, for stills; `None`
    /// lets them drift with the renderer's clock again.
    pub fn hold(&self, ms: Option<f32>) {
        *self.shared.held.lock_any() = ms;
    }

    /// The pass that draws it.
    pub fn pass(&self) -> Arc<dyn Pass> {
        self.shared.clone()
    }
}

impl anno_core::ambient::SkyPass for SkyPasses {
    // 2070's profile is the time: the hour changes nothing.
    fn show(&self, sky: Option<&Arc<dyn anno_core::ambient::Sky>>, _hours: f32) {
        let profile = sky
            .and_then(|s| s.as_any().downcast_ref::<scene::Sky>())
            .map(|s| s.profile.clone());
        *self.shared.shown.lock_any() = profile;
    }

    fn passes(&self) -> Vec<Arc<dyn Pass>> {
        vec![self.pass()]
    }

    fn hazes(&self) -> bool {
        self.shared.shown.lock_any().is_some()
    }

    fn environment(&self) -> Option<gpu::Environment> {
        let shown = self.shared.shown.lock_any();
        let profile = shown.as_ref()?;
        let mut cube = self.shared.cube.lock_any();
        if cube.as_ref().is_none_or(|c| c.0 != profile.name) {
            let built = environment(profile, &self.shared.volume, 0.);
            *cube = Some((profile.name.clone(), built));
        }
        cube.as_ref().map(|c| c.1.clone())
    }
}

impl Pass for Shared {
    fn stage(&self) -> Stage {
        Stage::Sky
    }

    // Not while nothing is shown: the kit's backdrop stays.
    fn replaces(&self) -> bool {
        self.shown.lock_any().is_some()
    }

    fn warm(&self, gpu: &gpu::Warm<'_>) {
        warm(gpu);
    }

    fn prepare(&self, frame: &mut gpu::Prepare<'_>) -> bool {
        let shown = self.shown.lock_any();
        // Nothing shown: ready and drawing nothing (`record`), so the kit's
        // backdrop shows and the view settles instead of loading forever.
        let Some(profile) = shown.as_ref() else {
            return true;
        };
        let ms = (*self.held.lock_any()).unwrap_or(frame.time * 1000.);
        let mut gpu = self.gpu.lock_any();
        let gpu = gpu.get_or_insert_with(frame.device, || self.resources(frame));
        let look = look(profile, ms);
        frame
            .queue
            .write_buffer(&gpu.buffer, 0, bytemuck::cast_slice(&look));
        true
    }

    fn record(&self, frame: &gpu::Record<'_>, pass: &mut wgpu::RenderPass<'_>) {
        // Shown before the GPU state, the order prepare takes them in.
        if self.shown.lock_any().is_none() {
            return;
        }
        let gpu = self.gpu.lock_any();
        let Some(gpu) = gpu.get(frame.device) else {
            return;
        };
        pass.set_pipeline(&gpu.pipelines.sky);
        pass.set_bind_group(1, &gpu.group, &[]);
        pass.draw(0..3, 0..1);
    }
}

impl Shared {
    /// The uniform buffer and the noise volume, and their group.
    fn resources(&self, frame: &gpu::Prepare<'_>) -> Gpu {
        let device = frame.device;
        let pipelines = shared_pipelines(device, frame.frame_layout);
        let buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("2070 sky look"),
            size: std::mem::size_of::<Look>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let side = self.volume.side;
        let size = wgpu::Extent3d {
            width: side,
            height: side,
            depth_or_array_layers: side,
        };
        let noise = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("2070 sky noise"),
            size,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D3,
            format: wgpu::TextureFormat::R8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        frame.queue.write_texture(
            noise.as_image_copy(),
            &self.volume.texels,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(side),
                rows_per_image: Some(side),
            },
            size,
        );
        // LINEAR, WRAP (sky.fx:17).
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("2070 sky noise"),
            address_mode_u: wgpu::AddressMode::Repeat,
            address_mode_v: wgpu::AddressMode::Repeat,
            address_mode_w: wgpu::AddressMode::Repeat,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        let view = noise.create_view(&Default::default());
        let group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("2070 sky"),
            layout: &pipelines.layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: buffer.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(&view),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::Sampler(&sampler),
                },
            ],
        });
        Gpu {
            pipelines,
            buffer,
            group,
        }
    }
}

/// The sky's pipeline on `device`, compiled once for every island and
/// early by `warm`.
fn shared_pipelines(device: &wgpu::Device, frame_layout: &wgpu::BindGroupLayout) -> Pipelines {
    gpu::per_device(device, "2070 sky", || pipelines(device, frame_layout))
}

/// Compiles the sky's pipeline before an island needs it.
pub fn warm(gpu: &gpu::Warm<'_>) {
    shared_pipelines(gpu.device, gpu.frame_layout);
}

fn pipelines(device: &wgpu::Device, frame_layout: &wgpu::BindGroupLayout) -> Pipelines {
    let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("2070 sky"),
        source: wgpu::ShaderSource::Wgsl(
            [gpu::FRAME_WGSL, include_str!("sky.wgsl")].concat().into(),
        ),
    });
    let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("2070 sky"),
        entries: &[
            wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            },
            wgpu::BindGroupLayoutEntry {
                binding: 1,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Texture {
                    sample_type: wgpu::TextureSampleType::Float { filterable: true },
                    view_dimension: wgpu::TextureViewDimension::D3,
                    multisampled: false,
                },
                count: None,
            },
            wgpu::BindGroupLayoutEntry {
                binding: 2,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                count: None,
            },
        ],
    });
    let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some("2070 sky"),
        bind_group_layouts: &[Some(frame_layout), Some(&layout)],
        immediate_size: 0,
    });
    let sky = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("2070 sky"),
        layout: Some(&pipeline_layout),
        vertex: wgpu::VertexState {
            module: &shader,
            entry_point: Some("vs_full"),
            compilation_options: Default::default(),
            buffers: &[],
        },
        fragment: Some(wgpu::FragmentState {
            module: &shader,
            entry_point: Some("fs_sky"),
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
    });
    Pipelines { sky, layout }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scene::{AmbientMap, to_gamma, to_linear};
    use anno_core::ambient::SkyPass as _;
    use gpui_editor_kit::camera::{Camera, Extent};
    use gpui_editor_kit::gpu::{Curve, Light, Offscreen, Renderer, Scene, Sky, Tone};
    use gpui_editor_kit::gpui::{point, px};

    fn install() -> Option<Install> {
        let Some(dir) = anno_formats::vfs::install_dir(crate::PROFILE.env_var) else {
            eprintln!("skip: ANNO2070_DIR unset");
            return None;
        };
        Some(Install::open(&dir, crate::ARCHIVE_FAMILIES).unwrap())
    }

    /// sky.fx's ComputeSkyColor toward unit `p` from an eye `height` up,
    /// the clouds as turned by `look`, gamma-encoded.
    fn reference(look: &Look, v: &Volume, p: [f32; 3], height: f32) -> [f32; 3] {
        let saturate = |x: f32| x.clamp(0., 1.);
        let turned = |a: f32, s: [f32; 3]| {
            let p = [
                a.cos() * p[0] + a.sin() * p[2],
                p[1],
                a.cos() * p[2] - a.sin() * p[0],
            ];
            std::array::from_fn::<f32, 3, _>(|k| p[k] * s[k] * (1. - p[1]))
        };
        let [turn0, turn1, ..] = look[6];
        let clouds = sample(v, turned(turn0, [0.5, 3., 0.5]))
            + sample(v, turned(turn1, [2., 10., 2.]))
            + sample(v, turned(-turn1, [0.3, 0.9, 0.3]));
        let cloud = saturate(clouds - 1.) * saturate(2. * p[1]) * look[5][3];
        let offset = height / look[0][3];
        let zenith = 1. - saturate((p[1] + offset) / (offset + 1.));
        let toward = look[4];
        let sun_angle = p[0] * toward[0] + p[1] * toward[1] + p[2] * toward[2];
        let glow = (saturate(saturate(sun_angle) - toward[3]) * look[3][3])
            .max(0.)
            .powf(look[2][3]);
        let sun_angle = saturate(sun_angle * sun_angle * 0.3 + 0.7);
        std::array::from_fn(|c| {
            let t = zenith.powf(look[2][c]);
            let sky = sun_angle * (look[0][c] + (look[1][c] - look[0][c]) * t) + look[3][c] * glow;
            let shade = look[5][c] * (1. - saturate(cloud));
            sky + (shade - sky) * saturate(8. * cloud)
        })
    }

    fn digest(pixels: &[u8]) -> u64 {
        pixels.iter().fold(0xcbf29ce484222325, |h, &b| {
            (h ^ u64::from(b)).wrapping_mul(0x100000001b3)
        })
    }

    // Nothing shown (an island naming a profile the game doesn't ship):
    // the pass is ready, draws nothing and leaves the kit's backdrop, so
    // the view settles, also after a profile was shown.
    #[test]
    fn nothing_shown_draws_the_kits_backdrop_and_settles() {
        let Some(install) = install() else { return };
        let map = AmbientMap::read(&install).unwrap();
        let ambient = crate::scene::ambient(map.get("North").unwrap().clone());
        let passes = SkyPasses::new(&install);
        let mut target = Offscreen::new(160, 120).unwrap();
        let mut render = |passes: Vec<Arc<dyn Pass>>| {
            let renderer = Renderer::default();
            renderer.set_scene(&Scene {
                passes,
                ..Default::default()
            });
            let camera = Camera {
                tilt: 1.45,
                ..Camera::default()
            };
            let world = Extent {
                center: [0., 2., 0.],
                size: 6.,
            };
            target.render(&renderer, camera, world).unwrap()
        };
        let bare = render(Vec::new());
        passes.show(None, 12.);
        assert!(render(passes.passes()) == bare);
        passes.show(ambient.sky.as_ref(), 12.);
        assert!(render(passes.passes()) != bare);
        passes.show(None, 12.);
        assert!(render(passes.passes()) == bare);
    }

    // The pass compiles, draws in place of the kit's backdrop (a magenta
    // one leaves no trace), and draws sky.fx's sky: North's first setting
    // with its clouds held at 0 ms, looking toward the sun across the
    // horizon, matches the CPU's formula pixel by pixel and a digest
    // recorded from this frame.
    #[test]
    fn the_sky_pass_replaces_the_backdrop_with_2070s_sky() {
        let Some(install) = install() else { return };
        let map = AmbientMap::read(&install).unwrap();
        let profile = map.get("North").unwrap().clone();
        let passes = SkyPasses::new(&install);
        assert_eq!(passes.shared.volume.side, 64, "the noise volume read");
        passes.hold(Some(0.));
        let ambient = crate::scene::ambient(profile.clone());
        passes.show(ambient.sky.as_ref(), 12.);
        assert!(passes.hazes());

        let toward = profile.toward_sun();
        let camera = Camera {
            fov: 1.0,
            tilt: 1.5,
            yaw: (-toward[0]).atan2(-toward[2]),
            ..Camera::default()
        };
        let world = Extent {
            center: [0., 30., 0.],
            size: 20.,
        };
        let (w, h) = (320u32, 200u32);
        let mut target = Offscreen::new(w, h).unwrap();
        let magenta = [1., 0., 1.];
        let scene = |passes: Vec<Arc<dyn Pass>>| Scene {
            light: Light {
                sun: toward,
                sun_color: profile.sun().map(to_linear),
                sky: profile.ambient().map(to_linear),
                ground: profile.ambient().map(to_linear),
                ..Default::default()
            },
            tone: Some(Tone {
                exposure: 1.,
                sun_occlusion: 0.,
                ambient: 1.,
                curve: Curve::Shoulder {
                    gamma: 2.2,
                    max_linear: 1.,
                    white: 8.,
                },
                auto: None,
            }),
            sky: Some(Sky {
                zenith: magenta,
                horizon: magenta,
                sea: None,
                radiance: None,
                sun_disc: [0.; 3],
                ambient: None,
            }),
            passes,
            ..Default::default()
        };
        let is_magenta = |p: &[u8]| p[0] > 200 && p[1] < 60 && p[2] > 200;
        let kit = Renderer::default();
        kit.set_scene(&scene(Vec::new()));
        let backdrop = target.render(&kit, camera, world).unwrap();
        let shown = backdrop.chunks(4).filter(|p| is_magenta(p)).count();
        assert!(
            shown > (w * h / 2) as usize,
            "the kit's backdrop: {shown} magenta"
        );

        let renderer = Renderer::default();
        renderer.set_scene(&scene(passes.passes()));
        let pixels = target.render(&renderer, camera, world).unwrap();
        let left = pixels.chunks(4).filter(|p| is_magenta(p)).count();
        assert_eq!(left, 0, "magenta left under 2070's sky");

        let bounds = target.bounds();
        let eye = camera.eye(bounds, world).unwrap();
        let height = crate::scene::held_eye(eye, camera.anchor(world))[1];
        let look = look(&profile, 0.);
        let mut worst = 0f32;
        let (mut above, mut below) = (0, 0);
        // The clouds must show: pixels they move by more than 10 levels.
        let mut clear = look;
        clear[5][3] = 0.;
        let mut clouded = 0;
        for y in (4..h).step_by(16) {
            for x in (4..w).step_by(16) {
                let ray = camera.ray(point(px(x as f32 + 0.5), px(y as f32 + 0.5)), bounds, world);
                let p = ray.direction.map(|c| c as f32);
                if p[1] > 0. {
                    above += 1
                } else {
                    below += 1
                }
                let want = reference(&look, &passes.shared.volume, p, height);
                let open = reference(&clear, &passes.shared.volume, p, height);
                clouded += usize::from((0..3).any(|c| (want[c] - open[c]).abs() * 255. > 10.));
                let i = ((y * w + x) * 4) as usize;
                for c in 0..3 {
                    // Linear out, gamma back through the curve: the gamma
                    // value itself, as sky.fx's target shows it.
                    let want = to_gamma(to_linear(want[c])).clamp(0., 1.) * 255.;
                    let off = (f32::from(pixels[i + c]) - want).abs();
                    worst = worst.max(off);
                    assert!(
                        off <= 4.,
                        "pixel {x},{y} toward {p:?}: {:?} vs {want}",
                        &pixels[i..i + 3]
                    );
                }
            }
        }
        assert!(above > 20 && below > 20, "{above} above, {below} below");
        assert!(clouded >= 10, "{clouded} pixels under cloud");
        eprintln!(
            "worst {worst} levels, {clouded} clouded, digest {:#x}",
            digest(&pixels)
        );
        assert_eq!(digest(&pixels), 0x16dc7d40632645f3, "recorded digest");
    }
}
