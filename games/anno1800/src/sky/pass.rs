// Owns 1800's sky as the island view draws it from the camera (pass.wgsl):
// the shown preset's scattering at its hour, its cloud layer baked for
// that hour, the moon's texture, and the two passes, the backdrop at the
// Sky stage and the haze at Blend, sharing that state.
use super::{
    CAMERA_FAR, CAMERA_MAX_HEIGHT, Cube, MAP, Preset, SUN_FALLOFF, SUN_INNER_DEGREES,
    SUN_OUTER_DEGREES,
};
use anno_core::ambient::Sky;
use anno_formats::vfs::Install;
use gpui_editor_kit::gpu::LockAny as _;
use gpui_editor_kit::gpu::{self, Pass, SkyMap, Stage};
use gpui_editor_kit::texture::Dds;
use rayon::prelude::*;
use std::sync::mpsc::{Receiver, TryRecvError};
use std::sync::{Arc, Mutex};

/// The uniform as pass.wgsl's `Look`: eight vec4.
type Look = [[f32; 4]; 8];

/// The cloud layer's size: half the sky bake's, as it blends over a sky
/// drawn per pixel.
const CLOUDS: [u32; 2] = [MAP[0] / 2, MAP[1] / 2];

/// vsMoon's texture (cMoonMap). TODO: which texture the engine binds is not
/// traced; this is the one moon texture under the sky's effects.
const MOON: &str = "data/graphics/effects/sky/moon_0.dds";

/// 1800's sky passes for one island view (`Module::sky`).
pub struct SkyPasses {
    shared: Arc<Shared>,
    backdrop: Arc<dyn Pass>,
    haze: Arc<dyn Pass>,
}

impl SkyPasses {
    /// Reads the moon's texture from `install`.
    pub fn new(install: &Install) -> Self {
        let moon = install
            .read(MOON)
            .ok()
            .and_then(|bytes| Dds::parse(&bytes).ok()?.decode(&bytes, 0).ok())
            .map(|t| (t.width, t.height, t.rgba));
        let shared = Arc::new(Shared {
            moon: moon.unwrap_or((1, 1, vec![0; 4])),
            ..Default::default()
        });
        Self {
            backdrop: Arc::new(Part {
                shared: shared.clone(),
                stage: Stage::Sky,
            }),
            haze: Arc::new(Part {
                shared: shared.clone(),
                stage: Stage::Blend,
            }),
            shared,
        }
    }
}

/// What `show` set: the look, and the sky and hour its cloud layer is for.
struct Shown {
    look: Look,
    sky: Arc<dyn Sky>,
    hours: f32,
}

#[derive(Default)]
struct Shared {
    shown: Mutex<Option<Shown>>,
    moon: (u32, u32, Vec<u8>),
    gpu: Mutex<gpu::OnDevice<Gpu>>,
    building: Mutex<gpu::OnDevice<Receiver<Pipelines>>>,
    /// The cloud layer for a sky and hour while a thread bakes it.
    baking: Mutex<Option<(Key, Receiver<Vec<u16>>)>>,
}

/// A sky (by pointer) at an hour (by bits).
type Key = (usize, u32);

#[derive(Clone)]
struct Pipelines {
    backdrop: wgpu::RenderPipeline,
    haze: wgpu::RenderPipeline,
    layout: wgpu::BindGroupLayout,
}

struct Gpu {
    pipelines: Pipelines,
    buffer: wgpu::Buffer,
    clouds: wgpu::Texture,
    group: wgpu::BindGroup,
    /// What the cloud texture holds.
    baked: Option<Key>,
}

fn key(sky: &Arc<dyn Sky>, hours: f32) -> Key {
    (Arc::as_ptr(sky) as *const () as usize, hours.to_bits())
}

impl anno_core::ambient::SkyPass for SkyPasses {
    fn show(&self, sky: Option<&Arc<dyn Sky>>, hours: f32) {
        let shown = sky.and_then(|sky| {
            let preset = sky.as_any().downcast_ref::<Preset>()?;
            Some(Shown {
                look: look(preset, hours),
                sky: sky.clone(),
                hours,
            })
        });
        *self.shared.shown.lock_any() = shown;
    }

    fn passes(&self) -> Vec<Arc<dyn Pass>> {
        vec![self.backdrop.clone(), self.haze.clone()]
    }

    fn hazes(&self) -> bool {
        self.shared.shown.lock_any().is_some()
    }
}

/// The uniform for `preset` at `hours`.
fn look(preset: &Preset, hours: f32) -> Look {
    let t = (hours / 24.).rem_euclid(1.);
    let sky = preset.scattering(t);
    let [start, intensity, falloff] = preset.haze(t);
    let (inner, outer) = (
        SUN_INNER_DEGREES.to_radians().cos(),
        SUN_OUTER_DEGREES.to_radians().cos(),
    );
    // "[0,1] from scatteringStart to far plane": the start as a share of
    // the far plane. TODO: how the engine scales ScatteringStart is not
    // traced; the far plane is the sky bake's estimate.
    let start = start * CAMERA_FAR;
    let on = |b: bool| if b { 1. } else { 0. };
    let rgb = |c: [f32; 3], w: f32| [c[0], c[1], c[2], w];
    [
        rgb(sky.zenith, on(preset.clouds.is_some())),
        rgb(sky.horizon, on(sky.sun_on)),
        rgb(sky.horizon_falloff, 0.),
        rgb(sky.sun, SUN_FALLOFF),
        rgb(sky.toward, 1. / (inner - outer)),
        [
            sky.day.is_day,
            sky.day.day_value,
            sky.day.night_value,
            outer,
        ],
        [start, 1. / (CAMERA_FAR - start).max(1.), intensity, falloff],
        // The moon by night. TODO: when the engine draws the Moon technique
        // is not traced; it would stand on the sun by day.
        [CAMERA_FAR, sky.day.night_value, 0.0008, CAMERA_MAX_HEIGHT],
    ]
}

/// The cloud layer toward every texel of a `CLOUDS` map: twice its color,
/// as psSkyBox lays it over the sky, and its cover; half floats.
fn bake(preset: &Preset, clouds: &Cube, hours: f32) -> Vec<u16> {
    let sky = preset.scattering((hours / 24.).rem_euclid(1.));
    let [w, h] = CLOUDS;
    (0..w * h)
        .into_par_iter()
        .flat_map_iter(|i| {
            let u = SkyMap::direction(w, h, i % w, i / w);
            let c = preset.cloud_layer(clouds, &sky, u);
            [c[0] * 2., c[1] * 2., c[2] * 2., c[3]].map(anno_core::water::half)
        })
        .collect()
}

/// One of the two passes; both prepare the shared state, which is cheap
/// the second time.
struct Part {
    shared: Arc<Shared>,
    stage: Stage,
}

impl Pass for Part {
    fn stage(&self) -> Stage {
        self.stage
    }

    fn prepare(&self, frame: &mut gpu::Prepare<'_>) -> bool {
        self.shared.prepare(frame)
    }

    fn record(&self, frame: &gpu::Record<'_>, pass: &mut wgpu::RenderPass<'_>) {
        // Shown before the GPU state, the order prepare takes them in.
        if self.shared.shown.lock_any().is_none() {
            return;
        }
        let gpu = self.shared.gpu.lock_any();
        let Some(gpu) = gpu.get(frame.device) else {
            return;
        };
        pass.set_pipeline(match self.stage {
            Stage::Sky => &gpu.pipelines.backdrop,
            _ => &gpu.pipelines.haze,
        });
        pass.set_bind_group(1, &gpu.group, &[]);
        pass.draw(0..3, 0..1);
    }
}

impl Shared {
    fn prepare(&self, frame: &mut gpu::Prepare<'_>) -> bool {
        let shown = self.shown.lock_any();
        // Nothing shown: ready and drawing nothing (`record`), so the kit's
        // backdrop shows and the view settles instead of loading forever.
        let Some(shown) = shown.as_ref() else {
            return true;
        };
        let mut gpu = self.gpu.lock_any();
        if gpu.get(frame.device).is_none() {
            // Compiling on a thread, as the water does: the kit's backdrop
            // shows until the pipelines are in. Again on a new device
            // after a loss, whose clouds bake again.
            let mut building = self.building.lock_any();
            match building.get(frame.device).map(Receiver::try_recv) {
                Some(Ok(pipelines)) => {
                    building.clear();
                    let made = self.resources(frame, pipelines);
                    gpu.set(frame.device, made);
                }
                Some(Err(TryRecvError::Empty)) => return false,
                _ => {
                    let (tx, rx) = std::sync::mpsc::sync_channel(1);
                    let device = frame.device.clone();
                    let layout = frame.frame_layout.clone();
                    let spawned =
                        std::thread::Builder::new()
                            .name("1800 sky".into())
                            .spawn(move || {
                                let _ = tx.send(shared_pipelines(&device, &layout));
                            });
                    if spawned.is_ok() {
                        building.set(frame.device, rx);
                    }
                    return false;
                }
            }
        }
        let gpu = gpu.get_mut(frame.device).expect("built above");
        frame
            .queue
            .write_buffer(&gpu.buffer, 0, bytemuck::cast_slice(&shown.look));
        let wanted = key(&shown.sky, shown.hours);
        let has_clouds = shown.look[0][3] > 0.5;
        if has_clouds && gpu.baked != Some(wanted) {
            // A few milliseconds of every core per hour shown: baked on a
            // thread, the last hour's clouds stand in meanwhile.
            let mut baking = self.baking.lock_any();
            match baking.as_ref() {
                Some((k, rx)) if *k == wanted => match rx.try_recv() {
                    Ok(texels) => {
                        let [w, h] = CLOUDS;
                        frame.queue.write_texture(
                            gpu.clouds.as_image_copy(),
                            bytemuck::cast_slice(&texels),
                            wgpu::TexelCopyBufferLayout {
                                offset: 0,
                                bytes_per_row: Some(w * 8),
                                rows_per_image: None,
                            },
                            wgpu::Extent3d {
                                width: w,
                                height: h,
                                depth_or_array_layers: 1,
                            },
                        );
                        gpu.baked = Some(wanted);
                        *baking = None;
                    }
                    Err(TryRecvError::Empty) => {}
                    Err(TryRecvError::Disconnected) => *baking = None,
                },
                _ => {
                    let (tx, rx) = std::sync::mpsc::sync_channel(1);
                    let (sky, hours) = (shown.sky.clone(), shown.hours);
                    let spawned = std::thread::Builder::new()
                        .name("1800 sky clouds".into())
                        .spawn(move || {
                            let preset = sky.as_any().downcast_ref::<Preset>();
                            if let Some(preset) = preset
                                && let Some(clouds) = &preset.clouds
                            {
                                let _ = tx.send(bake(preset, clouds, hours));
                            }
                        });
                    *baking = spawned.ok().map(|_| (wanted, rx));
                }
            }
            // The first hour waits for its clouds; later ones show the
            // previous hour's until theirs land.
            return gpu.baked.is_some();
        }
        true
    }

    /// The uniform buffer, cloud and moon textures and their group.
    fn resources(&self, frame: &gpu::Prepare<'_>, pipelines: Pipelines) -> Gpu {
        let device = frame.device;
        let buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("1800 sky look"),
            size: std::mem::size_of::<Look>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let texture = |label, width, height, format| {
            device.create_texture(&wgpu::TextureDescriptor {
                label: Some(label),
                size: wgpu::Extent3d {
                    width,
                    height,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format,
                usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                view_formats: &[],
            })
        };
        let [w, h] = CLOUDS;
        let clouds = texture("1800 sky clouds", w, h, wgpu::TextureFormat::Rgba16Float);
        let (mw, mh, rgba) = &self.moon;
        let moon = texture("1800 moon", *mw, *mh, wgpu::TextureFormat::Rgba8Unorm);
        frame.queue.write_texture(
            moon.as_image_copy(),
            rgba,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(mw * 4),
                rows_per_image: None,
            },
            wgpu::Extent3d {
                width: *mw,
                height: *mh,
                depth_or_array_layers: 1,
            },
        );
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("1800 sky"),
            address_mode_u: wgpu::AddressMode::Repeat,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        let (clouds_view, moon_view) = (
            clouds.create_view(&Default::default()),
            moon.create_view(&Default::default()),
        );
        let group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("1800 sky"),
            layout: &pipelines.layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: buffer.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(&clouds_view),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::TextureView(&moon_view),
                },
                wgpu::BindGroupEntry {
                    binding: 3,
                    resource: wgpu::BindingResource::Sampler(&sampler),
                },
            ],
        });
        Gpu {
            pipelines,
            buffer,
            clouds,
            group,
            baked: None,
        }
    }
}

/// The sky's pipelines on `device`, compiled once for every island and
/// early by `warm`.
fn shared_pipelines(device: &wgpu::Device, frame_layout: &wgpu::BindGroupLayout) -> Pipelines {
    gpu::per_device(device, "1800 sky", || pipelines(device, frame_layout))
}

/// Compiles the sky's pipelines before an island needs them.
pub fn warm(gpu: &gpu::Warm<'_>) {
    shared_pipelines(gpu.device, gpu.frame_layout);
}

fn pipelines(device: &wgpu::Device, frame_layout: &wgpu::BindGroupLayout) -> Pipelines {
    let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("1800 sky"),
        source: wgpu::ShaderSource::Wgsl(
            [gpu::FRAME_WGSL, include_str!("pass.wgsl")].concat().into(),
        ),
    });
    let texture = |binding| wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::FRAGMENT,
        ty: wgpu::BindingType::Texture {
            sample_type: wgpu::TextureSampleType::Float { filterable: true },
            view_dimension: wgpu::TextureViewDimension::D2,
            multisampled: false,
        },
        count: None,
    };
    let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("1800 sky"),
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
            texture(1),
            texture(2),
            wgpu::BindGroupLayoutEntry {
                binding: 3,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                count: None,
            },
        ],
    });
    let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some("1800 sky"),
        bind_group_layouts: &[Some(frame_layout), Some(&layout)],
        immediate_size: 0,
    });
    let pipeline = |entry: &str, blend: Option<wgpu::BlendState>, compare| {
        device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some(entry),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_full"),
                compilation_options: Default::default(),
                buffers: &[],
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some(entry),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: gpu::HDR_FORMAT,
                    blend,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: Default::default(),
            depth_stencil: Some(wgpu::DepthStencilState {
                format: gpu::DEPTH_FORMAT,
                depth_write_enabled: Some(false),
                depth_compare: Some(compare),
                stencil: Default::default(),
                bias: Default::default(),
            }),
            multisample: Default::default(),
            multiview_mask: None,
            cache: None,
        })
    };
    Pipelines {
        // At z 0 against reversed depth: only where nothing drew.
        backdrop: pipeline("fs_backdrop", None, wgpu::CompareFunction::GreaterEqual),
        // The sky's color for the distance plus the surface times what
        // the haze leaves of it (alpha); the target's alpha stays.
        haze: pipeline(
            "fs_haze",
            Some(wgpu::BlendState {
                color: wgpu::BlendComponent {
                    src_factor: wgpu::BlendFactor::One,
                    dst_factor: wgpu::BlendFactor::SrcAlpha,
                    operation: wgpu::BlendOperation::Add,
                },
                alpha: wgpu::BlendComponent {
                    src_factor: wgpu::BlendFactor::Zero,
                    dst_factor: wgpu::BlendFactor::One,
                    operation: wgpu::BlendOperation::Add,
                },
            }),
            wgpu::CompareFunction::Always,
        ),
        layout,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anno_core::ambient::SkyPass as _;
    use gpui_editor_kit::camera::{Camera, Extent};
    use gpui_editor_kit::gpu::{
        Batch, Instances, Light, Material, Mesh, MeshData, Offscreen, Renderer, Scene, Tone,
    };
    use gpui_editor_kit::math;

    fn install() -> Option<Install> {
        Some(
            Install::open(
                anno_formats::vfs::install_dir("ANNO1800_DIR")?,
                crate::ARCHIVE_FAMILIES,
            )
            .unwrap(),
        )
    }

    /// Flat dark ground out to 20 km.
    fn ground() -> Batch {
        let r = 20_000.;
        let plane = MeshData {
            positions: vec![[-r, 0., -r], [r, 0., -r], [r, 0., r], [-r, 0., r]],
            normals: vec![[0., 1., 0.]; 4],
            colors: vec![0x181818; 4],
            indices: vec![0, 2, 1, 0, 3, 2],
            material: Material::Lit,
            ..Default::default()
        };
        Batch::new(
            Arc::new(Mesh::new(plane).unwrap()),
            Arc::new(Instances {
                matrices: vec![math::IDENTITY],
                ids: vec![Some(1)],
            }),
        )
    }

    fn render(passes: Vec<Arc<dyn Pass>>, batches: Vec<Batch>, height: f32) -> Vec<u8> {
        let mut target = Offscreen::new(160, 120).unwrap();
        let renderer = Renderer::default();
        renderer.set_scene(&Scene {
            batches,
            light: Light {
                sun: [0.3, 1., 0.2],
                sun_color: [1.; 3],
                sky: [0.3; 3],
                ground: [0.1; 3],
                ..Default::default()
            },
            tone: Some(Tone::default()),
            passes,
            ..Default::default()
        });
        let camera = Camera {
            tilt: 1.45,
            fov: 0.9,
            ..Camera::perspective()
        };
        let world = Extent {
            center: [0., height, 0.],
            size: 6.,
        };
        target.render(&renderer, camera, world).unwrap()
    }

    fn pixel(pixels: &[u8], y: usize) -> [u8; 3] {
        let i = (y * 160 + 80) * 4;
        [pixels[i], pixels[i + 1], pixels[i + 2]]
    }

    fn change(a: &[u8], b: &[u8], y: usize) -> u32 {
        (pixel(a, y).iter().zip(pixel(b, y)))
            .map(|(&x, y)| x.abs_diff(y) as u32)
            .sum()
    }

    // Needs the game. Nothing shown (an island naming a preset the game
    // doesn't ship): the passes are ready and draw nothing, so the view
    // settles on the kit's backdrop, also after a preset was shown.
    #[test]
    fn nothing_shown_draws_the_kits_backdrop_and_settles() {
        let Some(install) = install() else { return };
        let preset = super::super::presets(&install, &["Moderate_01"])
            .remove(0)
            .1;
        let sky: Arc<dyn Sky> = Arc::new(preset);
        let passes = SkyPasses::new(&install);
        let bare = render(Vec::new(), Vec::new(), 2.);
        passes.show(None, 12.);
        assert!(render(passes.passes(), Vec::new(), 2.) == bare);
        passes.show(Some(&sky), 12.);
        assert!(render(passes.passes(), Vec::new(), 2.) != bare);
        passes.show(None, 12.);
        assert!(render(passes.passes(), Vec::new(), 2.) == bare);
    }

    // Needs the game: Moderate_01 at noon from the camera, its sky pulled
    // down as the camera rises (as the game's sky follows the camera's
    // height), and its haze lightening far ground far more than near.
    #[test]
    fn moderate_01_draws_from_the_camera_and_hazes_the_distance() {
        let Some(install) = install() else { return };
        let preset = super::super::presets(&install, &["Moderate_01"])
            .remove(0)
            .1;
        let sky: Arc<dyn Sky> = Arc::new(preset);
        let passes = SkyPasses::new(&install);
        passes.show(Some(&sky), 12.);
        let all = passes.passes();

        let low = render(vec![all[0].clone()], Vec::new(), 2.);
        let high = render(vec![all[0].clone()], Vec::new(), 400.);
        let moved = (0..40).filter(|&y| change(&low, &high, y) > 6).count();
        assert!(
            moved > 10,
            "{moved} sky rows changed with the camera's height"
        );

        // 40 units up: the rows under the horizon reach kilometres out.
        let clear = render(vec![all[0].clone()], vec![ground()], 40.);
        let hazed = render(all, vec![ground()], 40.);
        // The haze starts some way out and thickens fast (Moderate_01's
        // falloff is 3.6): strongest just under the horizon, nil underfoot.
        let far = (0..120).map(|y| change(&clear, &hazed, y)).max().unwrap();
        let near = change(&clear, &hazed, 118);
        assert!(far > near + 15, "near {near}, far {far}");
    }
}

// The shader parses after the kit's prelude it is joined to, which a game
// pass shares names with.
#[cfg(test)]
#[test]
fn the_shader_parses_after_the_prelude() {
    let source = [gpu::FRAME_WGSL, include_str!("pass.wgsl")].concat();
    if let Err(e) = wgpu::naga::front::wgsl::parse_str(&source) {
        panic!("{}", e.emit_to_string(&source));
    }
}
