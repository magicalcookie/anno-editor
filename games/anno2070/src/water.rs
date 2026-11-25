// Owns Anno 2070's sea as the island view draws it: the mood profile's
// water (`Sea`), the shipped wave textures, and `Water`, the pass that keeps
// their GPU state: the waves' and the coast waves' prepare passes, and the
// surface and the island's lakes (`lake`) in the kit's Water stage. The
// shaders are water/ocean.wgsl and water/lake.wgsl after the kit's frame
// prelude.
// Engine addresses here are Anno5.exe build 11514956 (Steam).

mod lake;

use anno_core::ambient;
use anno_core::water::{Coast, CoastMap, Inland, half};
use anno_formats::vfs::Install;
use gpui_editor_kit::gpu::LockAny as _;
use gpui_editor_kit::gpu::{self, Stage};
use gpui_editor_kit::heightfield::Heightfield;
use gpui_editor_kit::texture::{Dds, Format};
use std::sync::{Arc, Mutex};

use crate::scene::Profile;
use crate::underwater::{CAUSTIC_DRIFT, CAUSTICS};

/// The textures watergrid.fx and waterocean.fx read, as the water manager
/// loads them (Anno5.exe 0x140835A10): waves01 the great waves' heights
/// (the one the game also keeps on the CPU), waves02 the medium waves'.
const GREAT_WAVES: &str = "data/graphics/effects/water/waves01.dds";
const MEDIUM_WAVES: &str = "data/graphics/effects/water/waves02.dds";
const RIPPLES: &str = "data/graphics/effects/water/ripples.dds";
const COAST_NOISE: &str = "data/graphics/effects/water/coast.dds";
/// The lakes' ripples, which lake.fx's manager loads beside it (Anno5.exe
/// 0x140847C60).
const LAKE_RIPPLES: &str = "data/graphics/effects/river/ripples.dds";

/// The waves' maps a side, as ps_Wave1's texel size has them.
const WAVE_TEXELS: u32 = 384;
/// The coast waves' square a side in texels: ps_ChoppyWaves' 1.1/512.
const DYNAMIC_TEXELS: u32 = 512;
/// World metres a coast wave texel spans. TODO: the engine sets
/// cWaterDynamicWave0 at run time; half a metre keeps the 256 m square
/// over an island's near coast and its waves a few metres long.
const DYNAMIC_TEXEL: f32 = 0.5;
/// Coast wave steps a second. TODO: the game steps once a frame;
/// a fixed rate keeps the waves' speed off the frame rate.
const STEP_RATE: f32 = 30.;
/// Steps run from still water when the waves start, or when a held time
/// moves: 8 s, by which the coast's waves have come in. Bounds the warm-up's
/// GPU work.
const WARM_STEPS: usize = 240;
/// Steps one frame may catch up; a stall beyond starts the waves anew.
const CATCH_UP: usize = 4;
/// Bytes between step parameters: the uniform offset alignment wgpu
/// guarantees.
const STEP_STRIDE: u64 = 256;
/// World metres over which the coast distance (the water info map's
/// green) runs from 0 at the coast to 1. TODO: the engine builds
/// that map from the terrain by a rule not found; 64 m puts the coast waves'
/// band (ShallowCoast, strongest from 5 to 50 m out) where shipped islands'
/// shelves are.
const COAST_REACH: f32 = 24.;
/// The surface's ring grid (vs_ocean), as 117's: about 25 km out from a
/// camera 100 m up.
const SEGMENTS: u32 = 256;
const RINGS: u32 = 180;
const RING_GROWTH: f32 = 1.045;

/// One mood profile's water (`Sea::new`): what watergrid.fx and
/// waterocean.fx take as cWaterAmplitude, cWaterVelocity0/1,
/// cWaterChoppiness, cWaterLightAbsorptionExp, cWaterLightDepth,
/// cWaterViewDepth and cWaterScattering, and the scattering. Its light,
/// cLgtDiffuse and cLgtAmbient, is the frame's (ocean.wgsl `water_sun`).
#[derive(Clone, Debug, PartialEq)]
pub struct Sea {
    pub frequency: f32,
    pub amplitude: [f32; 2],
    pub velocity: [[f32; 2]; 2],
    pub choppiness: f32,
    pub absorption: [f32; 3],
    pub light_depth: f32,
    pub view_depth: f32,
    pub view_exponent: f32,
    pub scattering: [f32; 3],
    /// The profile's scattering (`scene::Scatter::words`), which fogs the
    /// sea and the lakes as it fogs the land; zeros fog nothing.
    pub atmosphere: [[f32; 4]; 5],
}

impl ambient::Sea for Sea {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

impl Sea {
    /// `profile`'s sea: its Water block's above-water values and its
    /// scattering.
    pub fn new(profile: &Profile) -> Sea {
        let water = &profile.water;
        let above = &water.above;
        Sea {
            frequency: water.wave_frequency[0],
            amplitude: water.wave_amplitude,
            velocity: water.wave_velocity,
            choppiness: water.choppiness,
            absorption: above.absorption,
            light_depth: above.light_depth,
            view_depth: above.view_depth,
            view_exponent: above.view_depth_exponent,
            scattering: above.scattering,
            atmosphere: profile.scatter().words(),
        }
    }

    /// The prepare passes' parameters at `time` seconds (ocean.wgsl
    /// `Step`), the coast waves' fields left still.
    fn step(&self, time: f32) -> Step {
        // TODO: the engine turns WaveVelocity into the layers'
        // offsets at run time. Taken here as the profile's velocity in map
        // widths a second, the second layer mirrored across X: both move
        // with the wind along Z and cross each other.
        let [v0, v1] = self.velocity;
        Step {
            velocity0: [v0[0] * time, v0[1] * time, -v0[0] * time, v0[1] * time],
            velocity1: [v1[0] * time, v1[1] * time, -v1[0] * time, v1[1] * time],
            waves: [
                self.amplitude[0],
                self.amplitude[1],
                self.choppiness,
                self.frequency,
            ],
            ..Step::default()
        }
    }
}

/// ocean.wgsl `Step`, 256-byte aligned in its buffer.
#[repr(C)]
#[derive(Clone, Copy, Default, bytemuck::Pod, bytemuck::Zeroable)]
struct Step {
    velocity0: [f32; 4],
    velocity1: [f32; 4],
    waves: [f32; 4],
    dynamic: [f32; 4],
    scroll: [f32; 4],
    info: [f32; 4],
}

/// ocean.wgsl `OceanParams`.
#[repr(C)]
#[derive(Clone, Copy, Default, bytemuck::Pod, bytemuck::Zeroable)]
struct Params {
    grid: [f32; 4],
    absorb: [f32; 4],
    scatter: [f32; 4],
    body: [f32; 4],
    info: [f32; 4],
    dynamic: [f32; 4],
    ripples: [f32; 4],
    caustics: [f32; 4],
    atmosphere: [[f32; 4]; 5],
}

/// One texture as the GPU takes it: its mips, top first.
struct Plain {
    width: u32,
    height: u32,
    format: wgpu::TextureFormat,
    levels: Vec<Vec<u8>>,
}

impl Plain {
    /// `path` from `install`: one-byte maps as their byte (the wave
    /// heights and coast noise ship as A8, which the kit's decoder shows as
    /// grey with full alpha), the rest decoded to RGBA with a box-filtered
    /// mip chain.
    fn read(install: &Install, path: &str) -> Option<Plain> {
        let read = || -> anyhow::Result<Plain> {
            let bytes = install.read(path)?;
            let dds = Dds::parse(&bytes)?;
            if dds.format == Format::R8 {
                let at = dds.mip_offset(0);
                let n = (dds.width * dds.height) as usize;
                let texels = (bytes.get(at..at + n)).ok_or_else(|| anyhow::anyhow!("truncated"))?;
                return Ok(Plain::r8(dds.width, dds.height, texels.to_vec()));
            }
            let top = dds.decode(&bytes, 0)?;
            Ok(Plain::rgba(top.width, top.height, top.rgba))
        };
        read().map_err(|e| eprintln!("{path}: {e:#}")).ok()
    }

    fn r8(width: u32, height: u32, texels: Vec<u8>) -> Plain {
        Plain {
            width,
            height,
            format: wgpu::TextureFormat::R8Unorm,
            levels: vec![texels],
        }
    }

    fn rgba(width: u32, height: u32, rgba: Vec<u8>) -> Plain {
        let mut levels = vec![rgba];
        let (mut w, mut h) = (width as usize, height as usize);
        while w > 1 || h > 1 {
            let (nw, nh) = ((w / 2).max(1), (h / 2).max(1));
            let above = levels.last().expect("top level");
            let mut next = vec![0u8; nw * nh * 4];
            for y in 0..nh {
                for x in 0..nw {
                    for c in 0..4 {
                        let at = |dx: usize, dy: usize| {
                            let (sx, sy) = ((2 * x + dx).min(w - 1), (2 * y + dy).min(h - 1));
                            u32::from(above[(sy * w + sx) * 4 + c])
                        };
                        next[(y * nw + x) * 4 + c] =
                            ((at(0, 0) + at(1, 0) + at(0, 1) + at(1, 1) + 2) / 4) as u8;
                    }
                }
            }
            levels.push(next);
            (w, h) = (nw, nh);
        }
        Plain {
            width,
            height,
            format: wgpu::TextureFormat::Rgba8Unorm,
            levels,
        }
    }

    fn upload(&self, gpu: &Gpu, label: &str) -> wgpu::TextureView {
        use wgpu::util::DeviceExt;
        let data: Vec<u8> = self.levels.concat();
        gpu.device
            .create_texture_with_data(
                &gpu.queue,
                &wgpu::TextureDescriptor {
                    label: Some(label),
                    size: wgpu::Extent3d {
                        width: self.width,
                        height: self.height,
                        depth_or_array_layers: 1,
                    },
                    mip_level_count: self.levels.len() as u32,
                    sample_count: 1,
                    dimension: wgpu::TextureDimension::D2,
                    format: self.format,
                    usage: wgpu::TextureUsages::TEXTURE_BINDING,
                    view_formats: &[],
                },
                wgpu::util::TextureDataOrder::LayerMajor,
                &data,
            )
            .create_view(&Default::default())
    }
}

/// The shipped textures and lake looks the sea draws with, read once with
/// the install (about 2 MB).
struct Shipped {
    great: Plain,
    medium: Plain,
    ripples: Plain,
    caustics: Plain,
    coast_noise: Plain,
    lake_ripples: Plain,
    lake_looks: Arc<Vec<lake::Look>>,
}

impl Shipped {
    fn read(install: &Install) -> Option<Shipped> {
        let looks = install.read(lake::SETTINGS).ok();
        let looks = looks.map_or_else(Vec::new, |x| lake::looks(&String::from_utf8_lossy(&x)));
        Some(Shipped {
            great: Plain::read(install, GREAT_WAVES)?,
            medium: Plain::read(install, MEDIUM_WAVES)?,
            ripples: Plain::read(install, RIPPLES)?,
            caustics: Plain::read(install, CAUSTICS)?,
            coast_noise: Plain::read(install, COAST_NOISE)?,
            lake_ripples: Plain::read(install, LAKE_RIPPLES)?,
            lake_looks: Arc::new(looks),
        })
    }
}

/// 2070's sea for one island view: what `show` last set, the shipped
/// textures, and the GPU state built on the first frame and kept while the
/// view lives.
pub struct Water {
    shown: Mutex<Shown>,
    surface: Mutex<gpu::OnDevice<Surface>>,
    /// The surface while a thread compiles its pipelines.
    building: Mutex<gpu::OnDevice<std::sync::mpsc::Receiver<Surface>>>,
    shipped: Option<Arc<Shipped>>,
}

#[derive(Default)]
struct Shown {
    sea: Option<Sea>,
    field: Option<Arc<Heightfield>>,
    coast: CoastMap,
    lakes: lake::Reader,
    time: Option<f32>,
}

impl Water {
    /// Reads the wave, ripple, caustic and coast noise textures and the
    /// lake settings from `install`. Without them the pass draws nothing.
    pub fn new(install: &Install) -> Self {
        Self::with(Shipped::read(install).map(Arc::new))
    }

    fn with(shipped: Option<Arc<Shipped>>) -> Self {
        Self {
            shown: Mutex::default(),
            surface: Mutex::default(),
            building: Mutex::default(),
            shipped,
        }
    }
}

impl gpu::Pass for Water {
    fn stage(&self) -> Stage {
        Stage::Water
    }

    fn prepare(&self, frame: &mut gpu::Prepare<'_>) -> bool {
        let Some(shipped) = &self.shipped else {
            return false;
        };
        let mut shown = self.shown.lock_any();
        shown.coast.land();
        shown.lakes.land();
        let Some(sea) = shown.sea.clone() else {
            return false;
        };
        // The island's first coast still measuring: the sea waits for it,
        // rather than showing waves on the beach for a few frames.
        if shown.coast.coast().is_none() && shown.coast.measuring() {
            return false;
        }
        let gpu = Gpu {
            device: frame.device.clone(),
            queue: frame.queue.clone(),
        };
        let mut surface = self.surface.lock_any();
        if surface.get(frame.device).is_none() {
            // Built on a thread, as 117's sea: the textures' upload would
            // otherwise stall the frame that first shows the sea, and the
            // pipelines too on a device nothing warmed (a new one after a
            // loss).
            let mut building = self.building.lock_any();
            match building.get(frame.device).map(|rx| rx.try_recv()) {
                Some(Ok(built)) => {
                    surface.set(frame.device, built);
                    building.clear();
                }
                Some(Err(std::sync::mpsc::TryRecvError::Empty)) => return false,
                // Not started, or the build panicked: start (again).
                _ => {
                    let (tx, rx) = std::sync::mpsc::sync_channel(1);
                    let (device, queue) = (gpu.device.clone(), gpu.queue.clone());
                    let layout = frame.frame_layout.clone();
                    let shipped = shipped.clone();
                    let spawned =
                        std::thread::Builder::new()
                            .name("2070 water".into())
                            .spawn(move || {
                                let gpu = Gpu { device, queue };
                                let _ = tx.send(Surface::new(&gpu, &layout, &shipped));
                            });
                    if spawned.is_ok() {
                        building.set(frame.device, rx);
                    }
                    return false;
                }
            }
        }
        let surface = surface.get_mut(frame.device).expect("built above");
        let coast = shown.coast.coast().cloned();
        let field = shown.field.clone();
        let lakes = shown.lakes.triangles().cloned();
        let (time, held) = match shown.time {
            Some(t) => (t, true),
            None => (frame.time, false),
        };
        let focus = frame.focus;
        frame.timed_work("2070 water prepare", |encoder| {
            let around = Around {
                coast: coast.as_ref(),
                field: field.as_deref(),
                lakes: lakes.as_ref(),
                focus,
                time,
                held,
            };
            surface.prepare(&gpu, encoder, &sea, around);
        });
        true
    }

    fn record(&self, frame: &gpu::Record<'_>, pass: &mut wgpu::RenderPass<'_>) {
        if let Some(surface) = self.surface.lock_any().get_mut(frame.device) {
            surface.draw(frame.device, pass, frame.scene);
        }
    }

    fn warm(&self, gpu: &gpu::Warm<'_>) {
        warm(gpu);
    }

    // The sea covers all the kit's flat sea would, to the horizon.
    fn replaces(&self) -> bool {
        true
    }

    fn animating(&self) -> bool {
        let shown = self.shown.lock_any();
        shown.coast.measuring()
            || shown.lakes.reading()
            || (shown.sea.is_some() && shown.time.is_none())
    }
}

impl anno_core::water::Water for Water {
    // 2070's sea reflects a mirror view of the scene at sea level where its
    // graphics option turns reflections on (WATER_REFLECTION_ENABLED,
    // waterocean.fx:287); its lakes take the cube alone (lake.wgsl).
    fn reflection(&self, detail: ambient::Detail) -> Option<gpu::Reflection> {
        detail.reflections.then_some(gpu::Reflection { level: 0. })
    }

    fn show(
        &self,
        sea: Option<&Arc<dyn ambient::Sea>>,
        field: &Arc<Heightfield>,
        hold: bool,
        _inland: Option<Arc<Inland>>,
        time: Option<f32>,
    ) {
        let mut shown = self.shown.lock_any();
        shown.sea = (sea.and_then(|s| s.as_any().downcast_ref::<Sea>())).cloned();
        if shown.sea.is_none() {
            return;
        }
        shown.coast.want(field, hold);
        shown.field = Some(field.clone());
        shown.time = time;
    }

    // 2070's lakes come from the island file (`island_file`), not the
    // editor's river and lake curves, which 2070 islands have none of.
    fn shades_inland(&self) -> bool {
        false
    }

    fn island_file(&self, file: Option<&Arc<[u8]>>) {
        let Some(shipped) = &self.shipped else {
            return;
        };
        (self.shown.lock_any().lakes).want(file, &shipped.lake_looks);
    }
}

/// Builds the sea's shader module and every pipeline over it, once per
/// device, before an island needs them.
pub fn warm(warm: &gpu::Warm<'_>) {
    compiled(warm.device, warm.frame_layout);
}

/// The sea's pipelines on `device`: built once per device, module and
/// all, as 117's sea.
fn compiled(device: &wgpu::Device, frame: &wgpu::BindGroupLayout) -> Pipelines {
    gpu::per_device(device, "2070 ocean", || Pipelines::new(device, frame))
}

#[derive(Clone)]
struct Pipelines {
    surface_layout: wgpu::BindGroupLayout,
    surface: wgpu::RenderPipeline,
    /// The lakes, over the surface's groups.
    lake: wgpu::RenderPipeline,
    /// Group 2 of each prepare pass, and the pass.
    wave_position: (wgpu::BindGroupLayout, wgpu::ComputePipeline),
    wave_normal: (wgpu::BindGroupLayout, wgpu::ComputePipeline),
    dynamic_step: (wgpu::BindGroupLayout, wgpu::ComputePipeline),
    choppy: (wgpu::BindGroupLayout, wgpu::ComputePipeline),
}

impl Pipelines {
    fn new(d: &wgpu::Device, frame: &wgpu::BindGroupLayout) -> Pipelines {
        use wgpu::ShaderStages as S;
        let module = d.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("2070 ocean"),
            source: wgpu::ShaderSource::Wgsl(module_source().into()),
        });
        let both = S::VERTEX | S::FRAGMENT;
        let float = wgpu::BindingType::Texture {
            sample_type: wgpu::TextureSampleType::Float { filterable: true },
            view_dimension: wgpu::TextureViewDimension::D2,
            multisampled: false,
        };
        let sampler = wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering);
        let storage = wgpu::BindingType::StorageTexture {
            access: wgpu::StorageTextureAccess::WriteOnly,
            format: wgpu::TextureFormat::Rgba16Float,
            view_dimension: wgpu::TextureViewDimension::D2,
        };
        let scene = wgpu::BindingType::Texture {
            sample_type: wgpu::TextureSampleType::Uint,
            view_dimension: wgpu::TextureViewDimension::D2,
            multisampled: false,
        };
        let surface_layout = layout(
            d,
            "2070 ocean surface",
            &[
                (0, both, uniform(false)),
                (1, both, float),
                (2, S::FRAGMENT, float),
                (3, S::FRAGMENT, scene),
                (4, both, float),
                (5, S::FRAGMENT, float),
                (6, S::FRAGMENT, float),
                (7, both, float),
                (8, S::FRAGMENT, float),
                (9, both, sampler),
                (10, both, sampler),
                (11, S::FRAGMENT, float),
                (
                    12,
                    S::FRAGMENT,
                    wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Storage { read_only: true },
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                ),
            ],
        );
        let pipeline_layout = d.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("2070 ocean surface"),
            bind_group_layouts: &[Some(frame), Some(&surface_layout)],
            immediate_size: 0,
        });
        let surface = d.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("2070 ocean surface"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &module,
                entry_point: Some("vs_ocean"),
                buffers: &[],
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &module,
                entry_point: Some("fs_ocean"),
                targets: &[Some(wgpu::ColorTargetState {
                    format: gpu::HDR_FORMAT,
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: Default::default(),
            }),
            // One strip per band between rings (vs_ocean); seen from either
            // side, as water_ocean.fxs culls none.
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleStrip,
                ..Default::default()
            },
            // Opaque and depth-written, so the sky and blended surfaces
            // after it meet the water where it lies.
            depth_stencil: Some(wgpu::DepthStencilState {
                format: gpu::DEPTH_FORMAT,
                depth_write_enabled: Some(true),
                depth_compare: Some(wgpu::CompareFunction::GreaterEqual),
                stencil: Default::default(),
                bias: Default::default(),
            }),
            multisample: Default::default(),
            multiview_mask: None,
            cache: None,
        });
        let lake = d.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("2070 lakes"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &module,
                entry_point: Some("vs_lake"),
                buffers: &[Some(wgpu::VertexBufferLayout {
                    array_stride: 16,
                    step_mode: wgpu::VertexStepMode::Vertex,
                    attributes: &wgpu::vertex_attr_array![0 => Float32x4],
                })],
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &module,
                entry_point: Some("fs_lake"),
                targets: &[Some(wgpu::ColorTargetState {
                    format: gpu::HDR_FORMAT,
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: Default::default(),
            }),
            // Opaque (water_lake.fxs) and culling none: outlines run either
            // way round.
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: Some(wgpu::DepthStencilState {
                format: gpu::DEPTH_FORMAT,
                depth_write_enabled: Some(true),
                depth_compare: Some(wgpu::CompareFunction::GreaterEqual),
                stencil: Default::default(),
                bias: Default::default(),
            }),
            multisample: Default::default(),
            multiview_mask: None,
            cache: None,
        });
        let compute = |entry: &str, entries: &[(u32, S, wgpu::BindingType)]| {
            let group = layout(d, entry, entries);
            let pipeline_layout = d.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some(entry),
                bind_group_layouts: &[None, None, Some(&group)],
                immediate_size: 0,
            });
            let pipeline = d.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
                label: Some(entry),
                layout: Some(&pipeline_layout),
                module: &module,
                entry_point: Some(entry),
                compilation_options: Default::default(),
                cache: None,
            });
            (group, pipeline)
        };
        let c = S::COMPUTE;
        Pipelines {
            surface_layout,
            surface,
            lake,
            wave_position: compute(
                "cs_wave_position",
                &[
                    (0, c, uniform(true)),
                    (1, c, float),
                    (2, c, float),
                    (3, c, sampler),
                    (4, c, storage),
                ],
            ),
            wave_normal: compute(
                "cs_wave_normal",
                &[(0, c, uniform(true)), (5, c, float), (6, c, storage)],
            ),
            dynamic_step: compute(
                "cs_dynamic_step",
                &[
                    (0, c, uniform(true)),
                    (3, c, sampler),
                    (5, c, float),
                    (7, c, float),
                    (8, c, storage),
                    (9, c, storage),
                    (10, c, float),
                    (11, c, float),
                    (15, c, sampler),
                ],
            ),
            choppy: compute(
                "cs_choppy",
                &[
                    (0, c, uniform(true)),
                    (7, c, float),
                    (10, c, float),
                    (12, c, float),
                    (13, c, storage),
                    (14, c, storage),
                    (15, c, sampler),
                ],
            ),
        }
    }
}

/// The module's WGSL: the frame prelude with the scene's fog (2070's
/// family set's `pass_prelude`, which brings the scattering), then the
/// sea's and the lakes'. They don't call that `fog`: 2070 scatters its
/// forward passes per vertex and composes the sea in gamma space before
/// its GammaToLinear (waterocean.fx:193, lake.fx:90), while `fog` scatters
/// a finished linear colour per pixel. They take its `scene_view_scatter`
/// at their vertices instead, from the sea's own copy of the profile's
/// words (`Sea::atmosphere`, equal to the frame's in the editor).
fn module_source() -> String {
    [
        crate::scene::families().pass_prelude().as_str(),
        include_str!("water/ocean.wgsl"),
        include_str!("water/lake.wgsl"),
    ]
    .concat()
}

/// What a frame's sea is prepared around.
struct Around<'a> {
    coast: Option<&'a Arc<Coast>>,
    field: Option<&'a Heightfield>,
    /// The island's lakes' triangles (`lake::triangles`).
    lakes: Option<&'a Arc<Vec<[f32; 4]>>>,
    /// Where the camera looks, world X and Z: the coast waves' square's
    /// centre.
    focus: [f32; 2],
    time: f32,
    /// Whether `time` is held rather than the clock's.
    held: bool,
}

/// Where the coast waves' square lies and what it last stepped to.
#[derive(Default)]
struct Sim {
    /// The state texture holding the latest step.
    current: usize,
    /// Its corner in texels of the world (`DYNAMIC_TEXEL`), and the time
    /// and info map it stepped to; `None` before the first step.
    stepped: Option<([i32; 2], f32, usize)>,
}

/// The sea's GPU state for one view: the textures, the waves' and coast
/// waves' maps, the water info map, and what the surface draws with.
struct Surface {
    pipelines: Pipelines,
    params: wgpu::Buffer,
    steps: wgpu::Buffer,
    wrap: wgpu::Sampler,
    clamp: wgpu::Sampler,
    great: wgpu::TextureView,
    medium: wgpu::TextureView,
    ripples: wgpu::TextureView,
    caustics: wgpu::TextureView,
    coast_noise: wgpu::TextureView,
    displacement: wgpu::TextureView,
    normal: wgpu::TextureView,
    state: [wgpu::TextureView; 2],
    foam: wgpu::TextureView,
    dynamic_vertex: wgpu::TextureView,
    dynamic_pixel: wgpu::TextureView,
    /// The water info map, the coast map it was made from (held, so its
    /// address can't name another), and its corner and 1 / size; a 1x1
    /// stand-in without a coast.
    info: (wgpu::TextureView, Option<Arc<Coast>>, [f32; 4]),
    /// The prepare passes' bind groups, for the info map they bind.
    groups: Option<Groups>,
    /// The surface's bind group and the scene view it binds, for the info
    /// map it binds; rebuilt when either changes.
    drawn: Option<(usize, wgpu::TextureView, wgpu::BindGroup)>,
    /// The frame's steps as the buffer takes them, kept between frames.
    scratch: Vec<u8>,
    sim: Sim,
    lake_ripples: wgpu::TextureView,
    /// Every lake look, two vec4 each (lake.wgsl `LakeLook`).
    lake_looks: wgpu::Buffer,
    /// The lakes' vertices, the triangles they were made from (held) and
    /// their count.
    lakes: Option<(Arc<Vec<[f32; 4]>>, wgpu::Buffer, u32)>,
}

impl Surface {
    fn new(gpu: &Gpu, frame: &wgpu::BindGroupLayout, shipped: &Shipped) -> Surface {
        use wgpu::util::DeviceExt;
        let d = &gpu.device;
        let pipelines = compiled(d, frame);
        let target = |label: &str, side: u32| {
            d.create_texture(&wgpu::TextureDescriptor {
                label: Some(label),
                size: wgpu::Extent3d {
                    width: side,
                    height: side,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Rgba16Float,
                usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::STORAGE_BINDING,
                view_formats: &[],
            })
            .create_view(&Default::default())
        };
        let sampler = |label: &str, mode: wgpu::AddressMode| {
            d.create_sampler(&wgpu::SamplerDescriptor {
                label: Some(label),
                address_mode_u: mode,
                address_mode_v: mode,
                mag_filter: wgpu::FilterMode::Linear,
                min_filter: wgpu::FilterMode::Linear,
                mipmap_filter: wgpu::MipmapFilterMode::Linear,
                ..Default::default()
            })
        };
        Surface {
            params: d.create_buffer(&wgpu::BufferDescriptor {
                label: Some("2070 ocean"),
                size: size_of::<Params>() as u64,
                usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            }),
            // The waves' step, every coast wave step a frame may run, and
            // the choppy pass's.
            steps: d.create_buffer(&wgpu::BufferDescriptor {
                label: Some("2070 ocean steps"),
                size: (WARM_STEPS as u64 + 2) * STEP_STRIDE,
                usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            }),
            wrap: sampler("2070 ocean wrap", wgpu::AddressMode::Repeat),
            clamp: sampler("2070 ocean clamp", wgpu::AddressMode::ClampToEdge),
            great: shipped.great.upload(gpu, "2070 great waves"),
            medium: shipped.medium.upload(gpu, "2070 medium waves"),
            ripples: shipped.ripples.upload(gpu, "2070 ripples"),
            caustics: shipped.caustics.upload(gpu, "2070 caustics"),
            coast_noise: shipped.coast_noise.upload(gpu, "2070 coast noise"),
            displacement: target("2070 wave displacement", WAVE_TEXELS),
            normal: target("2070 wave normal", WAVE_TEXELS),
            state: [
                target("2070 coast waves", DYNAMIC_TEXELS),
                target("2070 coast waves", DYNAMIC_TEXELS),
            ],
            foam: target("2070 coast foam", DYNAMIC_TEXELS),
            dynamic_vertex: target("2070 coast waves vertex", DYNAMIC_TEXELS),
            dynamic_pixel: target("2070 coast waves pixel", DYNAMIC_TEXELS),
            info: (
                upload_info(gpu, 1, 1, &[[half(0.), half(1.), 0, half(1.)]]),
                None,
                [0.; 4],
            ),
            groups: None,
            drawn: None,
            scratch: Vec::new(),
            pipelines,
            sim: Sim::default(),
            lake_ripples: shipped.lake_ripples.upload(gpu, "2070 lake ripples"),
            // One look past the settings, so the buffer binds without them.
            lake_looks: d.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("2070 lake looks"),
                contents: bytemuck::cast_slice(
                    &(shipped.lake_looks.iter())
                        .map(|l| [l.color, [l.max_depth, 0., 0., 0.]])
                        .chain(std::iter::once([[0.; 4], [1., 0., 0., 0.]]))
                        .collect::<Vec<[[f32; 4]; 2]>>(),
                ),
                usage: wgpu::BufferUsages::STORAGE,
            }),
            lakes: None,
        }
    }

    /// Readies the sea: the info map from a new coast, the waves' maps,
    /// the coast waves stepped (from still water when they start or a held
    /// time moves, so a held frame is the same every time), and the
    /// surface's parameters.
    fn prepare(
        &mut self,
        gpu: &Gpu,
        encoder: &mut wgpu::CommandEncoder,
        sea: &Sea,
        around: Around<'_>,
    ) {
        let Around {
            coast,
            field,
            lakes,
            focus,
            time,
            held,
        } = around;
        let lakes = lakes.filter(|l| !l.is_empty());
        let same = match (&self.lakes, lakes) {
            (Some(had), Some(now)) => Arc::ptr_eq(&had.0, now),
            (had, now) => had.is_none() && now.is_none(),
        };
        if !same {
            use wgpu::util::DeviceExt;
            self.lakes = lakes.map(|l| {
                let buffer = gpu
                    .device
                    .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                        label: Some("2070 lakes"),
                        contents: bytemuck::cast_slice(l),
                        usage: wgpu::BufferUsages::VERTEX,
                    });
                (l.clone(), buffer, l.len() as u32)
            });
        }
        if let (Some(coast), Some(field)) = (coast, field) {
            if self
                .info
                .1
                .as_ref()
                .is_none_or(|held| !Arc::ptr_eq(held, coast))
            {
                let texels = info_map(coast, field);
                let view = upload_info(gpu, coast.width, coast.height, &texels);
                let map = [
                    coast.origin[0],
                    coast.origin[1],
                    1. / coast.size[0],
                    1. / coast.size[1],
                ];
                self.info = (view, Some(coast.clone()), map);
            }
        }
        // The coast map by address: `info` holds it, so it names no other.
        let info_key = self.info.1.as_ref().map_or(0, |c| Arc::as_ptr(c) as usize);
        if self.groups.as_ref().is_none_or(|g| g.info != info_key) {
            self.groups = Some(Groups::new(gpu, self, info_key));
        }
        let corner = focus.map(|v| (v / DYNAMIC_TEXEL).floor() as i32 - DYNAMIC_TEXELS as i32 / 2);
        // The steps this frame runs: `count` clocks `interval` apart from
        // `first`, and whether the first starts from still water.
        let interval = 1. / STEP_RATE;
        let (first, count, fresh) = match self.sim.stepped {
            Some((at, t, key)) if held && at == corner && t == time && key == info_key => {
                (time, 0, false)
            }
            Some((_, t, key)) if !held && key == info_key && time >= t && time - t < 2. => {
                let n = (((time - t) * STEP_RATE) as usize).min(CATCH_UP);
                (t + interval, n, false)
            }
            _ => (time - (WARM_STEPS - 1) as f32 * interval, WARM_STEPS, true),
        };
        let clock = |i: usize| first + i as f32 * interval;
        let square = |clock: f32| {
            [
                corner[0] as f32 * DYNAMIC_TEXEL,
                corner[1] as f32 * DYNAMIC_TEXEL,
                DYNAMIC_TEXELS as f32 * DYNAMIC_TEXEL,
                clock,
            ]
        };
        // The waves' step first, then the coast waves' (the first one
        // reading the last state where the square has moved since), then
        // the choppy pass's, each at its stride in the reused scratch.
        let steps = count + 2;
        self.scratch.clear();
        self.scratch.resize(steps * STEP_STRIDE as usize, 0);
        let mut put = |i: usize, step: Step| {
            let at = i * STEP_STRIDE as usize;
            self.scratch[at..at + size_of::<Step>()].copy_from_slice(bytemuck::bytes_of(&step));
        };
        put(0, sea.step(time));
        let scroll = match self.sim.stepped {
            Some((at, ..)) if !fresh => [corner[0] - at[0], corner[1] - at[1]],
            _ => [0; 2],
        };
        for i in 0..count {
            let reset = if fresh && i == 0 { 1. } else { 0. };
            let scroll = if i == 0 { scroll } else { [0; 2] };
            put(
                i + 1,
                Step {
                    dynamic: square(clock(i)),
                    scroll: [scroll[0] as f32, scroll[1] as f32, reset, 0.],
                    info: self.info.2,
                    ..sea.step(time)
                },
            );
        }
        put(
            steps - 1,
            Step {
                dynamic: square(time),
                info: self.info.2,
                ..Step::default()
            },
        );
        gpu.queue.write_buffer(&self.steps, 0, &self.scratch);
        let offset = |i: usize| (i as u64 * STEP_STRIDE) as u32;

        let p = &self.pipelines;
        let groups = self.groups.as_ref().expect("built above");
        let wave_groups = WAVE_TEXELS.div_ceil(8);
        let dynamic_groups = DYNAMIC_TEXELS.div_ceil(8);
        let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
            label: Some("2070 ocean prepare"),
            timestamp_writes: None,
        });
        pass.set_pipeline(&p.wave_position.1);
        pass.set_bind_group(2, &groups.wave_position, &[offset(0)]);
        pass.dispatch_workgroups(wave_groups, wave_groups, 1);
        pass.set_pipeline(&p.wave_normal.1);
        pass.set_bind_group(2, &groups.wave_normal, &[offset(0)]);
        pass.dispatch_workgroups(wave_groups, wave_groups, 1);
        if count > 0 {
            let mut current = self.sim.current;
            pass.set_pipeline(&p.dynamic_step.1);
            for i in 0..count {
                pass.set_bind_group(2, &groups.dynamic_step[current], &[offset(i + 1)]);
                pass.dispatch_workgroups(dynamic_groups, dynamic_groups, 1);
                current = 1 - current;
            }
            pass.set_pipeline(&p.choppy.1);
            pass.set_bind_group(2, &groups.choppy[current], &[offset(steps - 1)]);
            pass.dispatch_workgroups(dynamic_groups, dynamic_groups, 1);
            drop(pass);
            self.sim.current = current;
            let last = if held { time } else { clock(count - 1) };
            self.sim.stepped = Some((corner, last, info_key));
        } else {
            drop(pass);
        }

        let stepped = self.sim.stepped.map(|(at, ..)| at);
        // TODO: the engine moves cWaterRipplesOffset and
        // cWaterCausticsOffset by the clock at speeds set at run time (the
        // profiles' Current block may feed them); these drift a few
        // centimetres a second, the layers against each other. The
        // caustics drift as the under-water view's do.
        let params = Params {
            grid: [SEGMENTS as f32, RINGS as f32, RING_GROWTH, time],
            absorb: [
                sea.absorption[0],
                sea.absorption[1],
                sea.absorption[2],
                sea.light_depth,
            ],
            scatter: [
                sea.scattering[0],
                sea.scattering[1],
                sea.scattering[2],
                sea.view_depth,
            ],
            // TODO: cUserConstant1.x, the double refracted
            // light's gain, is set at run time; taken as 1.
            body: [sea.view_exponent, 1., 0., 0.],
            info: self.info.2,
            dynamic: match stepped {
                Some(at) => [
                    at[0] as f32 * DYNAMIC_TEXEL,
                    at[1] as f32 * DYNAMIC_TEXEL,
                    1. / (DYNAMIC_TEXELS as f32 * DYNAMIC_TEXEL),
                    1.,
                ],
                None => [0.; 4],
            },
            ripples: [time * 0.031, time * 0.023, -time * 0.019, time * 0.027],
            caustics: CAUSTIC_DRIFT.map(|v| v * time),
            atmosphere: sea.atmosphere,
        };
        gpu.queue
            .write_buffer(&self.params, 0, bytemuck::bytes_of(&params));
    }

    fn draw(
        &mut self,
        device: &wgpu::Device,
        pass: &mut wgpu::RenderPass<'_>,
        scene: &wgpu::TextureView,
    ) {
        let info = self.info.1.as_ref().map_or(0, |c| Arc::as_ptr(c) as usize);
        if (self.drawn.as_ref()).is_none_or(|(i, view, _)| *i != info || view != scene) {
            let group = self.surface_group(device, scene);
            self.drawn = Some((info, scene.clone(), group));
        }
        let (_, _, group) = self.drawn.as_ref().expect("built above");
        pass.set_pipeline(&self.pipelines.surface);
        pass.set_bind_group(1, group, &[]);
        pass.draw(0..strip_vertices(), 0..1);
        if let Some((_, vertices, count)) = &self.lakes {
            pass.set_pipeline(&self.pipelines.lake);
            pass.set_vertex_buffer(0, vertices.slice(..));
            pass.draw(0..*count, 0..1);
        }
    }

    /// The surface's bind group over `scene`, the view's colour so far.
    fn surface_group(&self, device: &wgpu::Device, scene: &wgpu::TextureView) -> wgpu::BindGroup {
        use wgpu::BindingResource::{Sampler, TextureView as View};
        let entries = [
            (0, self.params.as_entire_binding()),
            (1, View(&self.displacement)),
            (2, View(&self.normal)),
            (3, View(scene)),
            (4, View(&self.info.0)),
            (5, View(&self.ripples)),
            (6, View(&self.caustics)),
            (7, View(&self.dynamic_vertex)),
            (8, View(&self.dynamic_pixel)),
            (9, Sampler(&self.wrap)),
            (10, Sampler(&self.clamp)),
            (11, View(&self.lake_ripples)),
            (12, self.lake_looks.as_entire_binding()),
        ]
        .map(|(binding, resource)| wgpu::BindGroupEntry { binding, resource });
        device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("2070 ocean surface"),
            layout: &self.pipelines.surface_layout,
            entries: &entries,
        })
    }
}

/// The sea's prepare passes' bind groups. They bind the surface's own
/// textures and the info map, so they are built once per coast, not per
/// frame; the steps ride a dynamic offset.
struct Groups {
    /// The coast map they were built for, by address (`Surface::info`).
    info: usize,
    wave_position: wgpu::BindGroup,
    wave_normal: wgpu::BindGroup,
    /// Stepping from state 0 into 1, and from 1 into 0.
    dynamic_step: [wgpu::BindGroup; 2],
    /// The choppy pass reading state 0, or 1.
    choppy: [wgpu::BindGroup; 2],
}

impl Groups {
    fn new(gpu: &Gpu, s: &Surface, info: usize) -> Groups {
        use wgpu::BindingResource::{Sampler, TextureView as View};
        let p = &s.pipelines;
        let group = |layout: &wgpu::BindGroupLayout, rest: &[(u32, wgpu::BindingResource<'_>)]| {
            let step = wgpu::BindGroupEntry {
                binding: 0,
                resource: wgpu::BindingResource::Buffer(wgpu::BufferBinding {
                    buffer: &s.steps,
                    offset: 0,
                    size: wgpu::BufferSize::new(size_of::<Step>() as u64),
                }),
            };
            let entries: Vec<_> = std::iter::once(step)
                .chain(rest.iter().map(|(binding, resource)| wgpu::BindGroupEntry {
                    binding: *binding,
                    resource: resource.clone(),
                }))
                .collect();
            gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("2070 ocean prepare"),
                layout,
                entries: &entries,
            })
        };
        let map = &s.info.0;
        let dynamic_step = |from: usize| {
            group(
                &p.dynamic_step.0,
                &[
                    (3, Sampler(&s.wrap)),
                    (5, View(&s.displacement)),
                    (7, View(&s.state[from])),
                    (8, View(&s.state[1 - from])),
                    (9, View(&s.foam)),
                    (10, View(map)),
                    (11, View(&s.coast_noise)),
                    (15, Sampler(&s.clamp)),
                ],
            )
        };
        let choppy = |state: usize| {
            group(
                &p.choppy.0,
                &[
                    (7, View(&s.state[state])),
                    (10, View(map)),
                    (12, View(&s.foam)),
                    (13, View(&s.dynamic_vertex)),
                    (14, View(&s.dynamic_pixel)),
                    (15, Sampler(&s.clamp)),
                ],
            )
        };
        Groups {
            info,
            wave_position: group(
                &p.wave_position.0,
                &[
                    (1, View(&s.great)),
                    (2, View(&s.medium)),
                    (3, Sampler(&s.wrap)),
                    (4, View(&s.displacement)),
                ],
            ),
            wave_normal: group(
                &p.wave_normal.0,
                &[(5, View(&s.displacement)), (6, View(&s.normal))],
            ),
            dynamic_step: [dynamic_step(0), dynamic_step(1)],
            choppy: [choppy(0), choppy(1)],
        }
    }
}

/// Vertices of the ring grid's strips (vs_ocean): per band both rings'
/// corners around to the seam and two joining it to the next.
fn strip_vertices() -> u32 {
    (RINGS - 1) * (2 * SEGMENTS + 4) - 2
}

/// 2070's water info map (waterwaves.fx:85-91) from the coast map measured
/// off `field`, texel for texel, as half floats: r the land's height (0 to
/// 1 m: ps_DynamicWaves reads it over 0.7 m, the building plateau), g the
/// coast distance over [`COAST_REACH`], b how steep the coast is, a the
/// water's depth as the coast map normalises it (0 at the waterline, 1 from
/// 4 m down). TODO: the engine makes this map from the terrain
/// by rules not found; a's scale also sets how far the waves and the view
/// depth fade toward the shore.
fn info_map(coast: &Coast, field: &Heightfield) -> Vec<[u16; 4]> {
    let (lo, size) = (field.grid.origin, field.grid.size());
    let cell = [
        coast.size[0] / coast.width as f32,
        coast.size[1] / coast.height as f32,
    ];
    (coast.texels.iter().enumerate())
        .map(|(i, &[signed, depth, _, beach])| {
            let (x, z) = (i % coast.width as usize, i / coast.width as usize);
            let p = [
                coast.origin[0] + (x as f32 + 0.5) * cell[0],
                coast.origin[1] + (z as f32 + 0.5) * cell[1],
            ];
            let inside = (0..2).all(|k| p[k] >= lo[k] && p[k] <= lo[k] + size[k]);
            let land = if inside {
                field.height(p).clamp(0., 1.)
            } else {
                0.
            };
            let distance = (signed / COAST_REACH).clamp(0., 1.);
            [land, distance, 1. - beach, depth].map(half)
        })
        .collect()
}

fn upload_info(gpu: &Gpu, width: u32, height: u32, texels: &[[u16; 4]]) -> wgpu::TextureView {
    use wgpu::util::DeviceExt;
    gpu.device
        .create_texture_with_data(
            &gpu.queue,
            &wgpu::TextureDescriptor {
                label: Some("2070 water info"),
                size: wgpu::Extent3d {
                    width: width.max(1),
                    height: height.max(1),
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Rgba16Float,
                usage: wgpu::TextureUsages::TEXTURE_BINDING,
                view_formats: &[],
            },
            wgpu::util::TextureDataOrder::LayerMajor,
            bytemuck::cast_slice(texels),
        )
        .create_view(&Default::default())
}

/// The device and queue the sea's GPU code takes, cloned from the frame
/// (both are handles).
struct Gpu {
    device: wgpu::Device,
    queue: wgpu::Queue,
}

fn layout(
    d: &wgpu::Device,
    label: &str,
    entries: &[(u32, wgpu::ShaderStages, wgpu::BindingType)],
) -> wgpu::BindGroupLayout {
    let entries: Vec<_> = (entries.iter())
        .map(|&(binding, visibility, ty)| wgpu::BindGroupLayoutEntry {
            binding,
            visibility,
            ty,
            count: None,
        })
        .collect();
    d.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some(label),
        entries: &entries,
    })
}

fn uniform(dynamic: bool) -> wgpu::BindingType {
    wgpu::BindingType::Buffer {
        ty: wgpu::BufferBindingType::Uniform,
        has_dynamic_offset: dynamic,
        min_binding_size: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anno_core::water::Water as _;
    use gpui_editor_kit::camera::{Camera, Extent};
    use gpui_editor_kit::gpu::{
        Batch, Instances, Light, Material, Mesh, MeshData, Offscreen, Renderer, Scene, Sky, Tone,
    };
    use gpui_editor_kit::heightfield::Grid;
    use gpui_editor_kit::math::{self, Vec3};

    // The CPU reference for ocean.wgsl's water body.
    impl Sea {
        /// The water body's terms at one pixel (ocean.wgsl `shore_terms`):
        /// how much of the view's way to what lies behind runs through water
        /// (ViewDepth) and of the light's way down to it (LightDepth), for a
        /// view `path` metres long under water to a point at height
        /// `seabed_y`, the sun `sun_y` up, and the info map's `depth` there.
        pub fn shore(&self, path: f32, seabed_y: f32, sun_y: f32, depth: f32) -> [f32; 2] {
            let view = (path / (self.view_depth * depth).max(1e-4)).clamp(0., 1.);
            let light = ((-seabed_y / sun_y.max(0.05)) / (self.light_depth * depth).max(1e-4))
                .clamp(0., 1.);
            [view, light]
        }

        /// What shows through the water in gamma space (ocean.wgsl
        /// `water_body`) without caustics: `refraction` absorbed by the
        /// light's way, lerped toward the scattering colour by the view's.
        pub fn body(&self, refraction: [f32; 3], terms: [f32; 2]) -> [f32; 3] {
            let toward = terms[0].powf(self.view_exponent);
            std::array::from_fn(|k| {
                let absorbed = refraction[k] * (1. - terms[1]).powf(self.absorption[k]);
                absorbed + (self.scattering[k] - absorbed) * toward
            })
        }
    }

    /// North's first setting as ambientmap.xml ships it.
    fn north() -> Sea {
        Sea {
            frequency: 1.,
            amplitude: [0.34, 0.35],
            velocity: [[0.0211, 0.01735], [0.02005, 0.01665]],
            choppiness: 3.5,
            absorption: [3.88, 0.75, 0.1],
            light_depth: 30.,
            view_depth: 10.,
            view_exponent: 0.35,
            scattering: [0.1490196, 0.1843137, 0.1843137],
            atmosphere: [[0.; 4]; 5],
        }
    }

    /// Flat stand-ins for the shipped textures: still waves, level ripples
    /// with full foam detail, no caustics, still coast noise.
    fn flat() -> Shipped {
        let r8 = |n: u32| Plain::r8(n, n, vec![128; (n * n) as usize]);
        Shipped {
            great: r8(4),
            medium: r8(4),
            ripples: Plain::rgba(4, 4, [128, 128, 255, 255].repeat(16)),
            caustics: Plain::rgba(4, 4, [0, 0, 0, 255].repeat(16)),
            coast_noise: r8(4),
            lake_ripples: Plain::rgba(4, 4, [128, 255, 128, 0].repeat(16)),
            lake_looks: Arc::new(Vec::new()),
        }
    }

    fn install() -> Option<Install> {
        let dir = anno_formats::vfs::install_dir("ANNO2070_DIR")?;
        Some(Install::open(&dir, crate::ARCHIVE_FAMILIES).unwrap())
    }

    fn fnv(bytes: &[u8]) -> u64 {
        (bytes.iter()).fold(0xcbf29ce484222325, |h, &b| {
            (h ^ u64::from(b)).wrapping_mul(0x100000001b3)
        })
    }

    // The sea's module parses and validates as one, with naga's message
    // and the line when it does not (wgpu only says it failed).
    #[test]
    fn the_sea_module_validates() {
        let source = module_source();
        let module = naga::front::wgsl::parse_str(&source)
            .unwrap_or_else(|e| panic!("{}", e.emit_to_string(&source)));
        naga::valid::Validator::new(
            naga::valid::ValidationFlags::all(),
            naga::valid::Capabilities::all(),
        )
        .validate(&module)
        .unwrap_or_else(|e| panic!("{}", e.emit_to_string(&source)));
    }

    // A profile's sea is its Water block's above-water values, its light
    // with the powers applied, and its scattering.
    #[test]
    fn a_profile_gives_its_own_sea() {
        let profile = crate::scene::tests::north();
        let want = Sea {
            atmosphere: profile.scatter().words(),
            ..north()
        };
        assert_eq!(Sea::new(&profile), want);
    }

    /// A seabed sloping from 0.2 m under water at -z to 6 m at +z over
    /// x -40..0, and nothing opaque over x 0..40.
    fn half_seabed() -> (Batch, Arc<Heightfield>) {
        let floor = MeshData {
            positions: vec![
                [-40., -0.2, -40.],
                [0., -0.2, -40.],
                [0., -6., 40.],
                [-40., -6., 40.],
            ],
            normals: vec![[0., 1., 0.]; 4],
            colors: vec![0xd8d0b8; 4],
            indices: vec![0, 2, 1, 0, 3, 2],
            material: Material::Lit,
            ..Default::default()
        };
        let batch = Batch::new(
            Arc::new(Mesh::new(floor).unwrap()),
            Arc::new(Instances {
                matrices: vec![math::IDENTITY],
                ids: vec![Some(1)],
            }),
        );
        let grid = Grid {
            width: 9,
            depth: 9,
            spacing: 10.,
            origin: [-40.; 2],
        };
        let heights = (0..81).map(|i| -0.2 - 5.8 * (i / 9) as f32 / 8.).collect();
        (batch, Arc::new(Heightfield::new(grid, heights)))
    }

    // Plugged into the Water stage, the sea's pipelines build once per
    // device before the frame settles (the offscreen settle warms its
    // passes), and the sea draws where the kit's flat sea would: where
    // nothing opaque drew, the kit's red sea gives way to water. Over the
    // sloping floor, the shallows show more of it than the deep.
    #[test]
    fn the_ocean_warms_and_replaces_the_default_sea() {
        let mut target = Offscreen::new(320, 240).unwrap();
        let (floor, field) = half_seabed();
        let world = Extent {
            center: [0.; 3],
            size: 60.,
        };
        let camera = Camera::default();
        let render = |target: &mut Offscreen, water: bool| {
            let renderer = Renderer::default();
            let passes: Vec<Arc<dyn gpu::Pass>> = if water {
                let pass = Water::with(Some(Arc::new(flat())));
                let sea: Arc<dyn ambient::Sea> = Arc::new(north());
                pass.show(Some(&sea), &field, false, None, Some(0.));
                vec![Arc::new(pass)]
            } else {
                Vec::new()
            };
            renderer.set_scene(&Scene {
                batches: vec![floor.clone()],
                light: Light {
                    sun: [0.59, 0.74, 0.32],
                    sun_color: [3.; 3],
                    sky: [0.8, 0.9, 1.1],
                    ground: [0.3; 3],
                    ..Default::default()
                },
                tone: Some(Tone::default()),
                sky: Some(Sky {
                    zenith: [0.3, 0.5, 1.],
                    horizon: [0.8, 0.9, 1.],
                    sea: Some(gpu::Sea {
                        level: 0.,
                        color: [4., 0., 0.],
                    }),
                    radiance: None,
                    sun_disc: [0.; 3],
                    ambient: None,
                }),
                passes,
                ..Default::default()
            });
            target.render(&renderer, camera, world).unwrap()
        };
        let at = |target: &Offscreen, pixels: &[u8], p: Vec3| {
            let s = camera.project(p, target.bounds(), world);
            let i = (f32::from(s.y) as usize * 320 + f32::from(s.x) as usize) * 4;
            [pixels[i], pixels[i + 1], pixels[i + 2]]
        };
        let kit = render(&mut target, false);
        let open = at(&target, &kit, [20., 0., 0.]);
        assert!(open[0] > 150 && open[1] < 40, "the kit's sea {open:?}");
        let sea = render(&mut target, true);
        // Built and kept for the device: asking again builds nothing.
        gpu::per_device::<Pipelines>(target.device(), "2070 ocean", || {
            panic!("the ocean's pipelines were not built for this device")
        });
        let open = at(&target, &sea, [20., 0., 0.]);
        assert!(open[2] > open[0], "water where the kit's sea was {open:?}");
        let luma = |c: [u8; 3]| u32::from(c[0]) + u32::from(c[1]) + u32::from(c[2]);
        let shallow = at(&target, &sea, [-20., 0., -30.]);
        let deep = at(&target, &sea, [-20., 0., 30.]);
        let bare = at(&target, &kit, [-20., 0., -30.]);
        assert!(
            luma(shallow) > luma(deep) + 30,
            "shallow {shallow:?}, deep {deep:?}"
        );
        assert!(bare != shallow, "the shallows are under water {bare:?}");
    }

    // The water body's terms on the GPU (ocean.wgsl shore_terms and
    // water_body, run from a compute probe in the sea's own module) against
    // `Sea::shore` and `Sea::body`, looking straight down at a flat floor
    // at known depths, the info map's depth as the coast map has it (the
    // depth over 4 m).
    #[test]
    fn the_shore_and_absorption_match_the_cpu_reference_at_known_depths() {
        use wgpu::util::DeviceExt;
        let target = Offscreen::new(4, 4).unwrap();
        let (device, queue) = (target.device(), target.queue());
        let sea = north();
        let sun_y = 0.7416229;
        let sand = [0.8, 0.7, 0.5];
        let depths = [0.1f32, 0.5, 1., 2., 4., 6., 8., 12., 16., 32.];
        let cases: Vec<[f32; 12]> = (depths.iter())
            .map(|&d| {
                let a = (d / 4.).clamp(0., 1.);
                [
                    d,
                    -d,
                    sun_y,
                    a,
                    sea.view_depth,
                    sea.light_depth,
                    sea.view_exponent,
                    0.,
                    sand[0],
                    sand[1],
                    sand[2],
                    0.,
                ]
            })
            .collect();
        let probe = "
            struct ProbeCase { at: vec4<f32>, sea: vec4<f32>, refraction: vec4<f32> }
            struct ProbeSea { absorb: vec4<f32>, scatter: vec4<f32> }
            @group(3) @binding(0) var<storage, read> probe_in: array<ProbeCase>;
            @group(3) @binding(1) var<storage, read_write> probe_out: array<vec4<f32>>;
            @group(3) @binding(2) var<uniform> probe_sea: ProbeSea;
            @compute @workgroup_size(1) fn cs_probe(@builtin(global_invocation_id) id: vec3<u32>) {
                let c = probe_in[id.x];
                let terms = shore_terms(c.at.x, c.at.y, c.at.z, c.at.w, c.sea.x, c.sea.y);
                let body = water_body(c.refraction.rgb, vec3<f32>(0.0), terms, probe_sea.absorb.rgb,
                    probe_sea.scatter.rgb, c.sea.z);
                probe_out[id.x * 2u] = vec4<f32>(terms, 0.0, 0.0);
                probe_out[id.x * 2u + 1u] = vec4<f32>(body, 0.0);
            }";
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("2070 shore probe"),
            source: wgpu::ShaderSource::Wgsl((module_source() + probe).into()),
        });
        let pipeline = device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
            label: Some("2070 shore probe"),
            layout: None,
            module: &module,
            entry_point: Some("cs_probe"),
            compilation_options: Default::default(),
            cache: None,
        });
        let input = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: None,
            contents: bytemuck::cast_slice(&cases),
            usage: wgpu::BufferUsages::STORAGE,
        });
        let uniforms: [f32; 8] = [
            sea.absorption[0],
            sea.absorption[1],
            sea.absorption[2],
            0.,
            sea.scattering[0],
            sea.scattering[1],
            sea.scattering[2],
            0.,
        ];
        let uniforms = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: None,
            contents: bytemuck::cast_slice(&uniforms),
            usage: wgpu::BufferUsages::UNIFORM,
        });
        let size = (cases.len() * 32) as u64;
        let output = device.create_buffer(&wgpu::BufferDescriptor {
            label: None,
            size,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC,
            mapped_at_creation: false,
        });
        let readback = device.create_buffer(&wgpu::BufferDescriptor {
            label: None,
            size,
            usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: None,
            layout: &pipeline.get_bind_group_layout(3),
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: input.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: output.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: uniforms.as_entire_binding(),
                },
            ],
        });
        let mut encoder = device.create_command_encoder(&Default::default());
        {
            let mut pass = encoder.begin_compute_pass(&Default::default());
            pass.set_pipeline(&pipeline);
            pass.set_bind_group(3, &group, &[]);
            pass.dispatch_workgroups(cases.len() as u32, 1, 1);
        }
        encoder.copy_buffer_to_buffer(&output, 0, &readback, 0, size);
        queue.submit([encoder.finish()]);
        readback.slice(..).map_async(wgpu::MapMode::Read, |_| {});
        device.poll(wgpu::PollType::wait_indefinitely()).unwrap();
        let mapped = readback.slice(..).get_mapped_range().expect("mapped");
        let got: &[[f32; 4]] = bytemuck::cast_slice(&mapped);
        let mut last = [0f32; 2];
        for (i, &d) in depths.iter().enumerate() {
            let terms = sea.shore(d, -d, sun_y, (d / 4.).clamp(0., 1.));
            let body = sea.body(sand, terms);
            let gpu_terms = [got[2 * i][0], got[2 * i][1]];
            let gpu_body = [got[2 * i + 1][0], got[2 * i + 1][1], got[2 * i + 1][2]];
            for k in 0..2 {
                assert!(
                    (gpu_terms[k] - terms[k]).abs() < 1e-5,
                    "{d} m: GPU {gpu_terms:?}, CPU {terms:?}"
                );
            }
            for k in 0..3 {
                assert!(
                    (gpu_body[k] - body[k]).abs() < 1e-4,
                    "{d} m: GPU {gpu_body:?}, CPU {body:?}"
                );
            }
            // Deeper never shows more of the floor: the view and the light
            // both travel at least as far through water.
            assert!(
                terms[0] >= last[0] && terms[1] >= last[1],
                "{d} m: {terms:?} after {last:?}"
            );
            last = terms;
        }
        // Over the shelf (under 4 m) the info map's depth scales with the
        // water's, so the view's share holds at 4 / ViewDepth; deep water
        // turns to the scattering colour.
        let shelf = sea.shore(1., -1., sun_y, 0.25)[0];
        assert!((shelf - 0.4).abs() < 1e-6, "{shelf}");
        let deep = sea.body(sand, sea.shore(32., -32., sun_y, 1.));
        assert!(
            (0..3).all(|k| (deep[k] - sea.scattering[k]).abs() < 1e-6),
            "{deep:?}"
        );
    }

    /// How a test draws an island and its water.
    struct Shot {
        camera: Camera,
        size: [u32; 2],
        /// Where the camera turns about: the island's middle, the first
        /// shore met walking in from its west edge along its middle row,
        /// or a point.
        pivot: Pivot,
        /// Whether the pass gets the island's file, and so its lakes.
        lakes: bool,
        zenith: [f32; 3],
        /// Whether the sea scatters by the profile's atmosphere; without,
        /// it draws unhazed beside hazed land, as it did before the sea took
        /// the profile's scattering.
        fogged: bool,
        /// The profile lighting the island and giving the sea.
        profile: &'static str,
    }

    enum Pivot {
        Middle,
        WestShore,
        At([f32; 3]),
    }

    impl Shot {
        fn new(camera: Camera, pivot: Pivot) -> Shot {
            Shot {
                camera,
                size: [640, 400],
                pivot,
                lakes: true,
                zenith: [0.27, 0.5, 0.64],
                fogged: true,
                profile: "North",
            }
        }
    }

    /// What `draw` drew.
    struct Drawn {
        /// RGBA rows.
        pixels: Vec<u8>,
        /// Where `probe` lands in them.
        at: usize,
        world: Extent,
        bounds: gpui_editor_kit::gpui::Bounds<gpui_editor_kit::gpui::Pixels>,
        field: Arc<Heightfield>,
    }

    /// `path`'s island drawn with its own ground and this sea at a fixed
    /// time, lit, fogged and given its sea by the shot's profile as the
    /// editor shows it (scene.rs), as `shot` says.
    fn draw(install: &Install, path: &str, shot: &Shot, probe: [f32; 3]) -> Drawn {
        use crate::scene::{self, to_linear};
        use anno_render::layers::{LayerPaint, LayerTextures, island};
        let registry = crate::island::registry(install).unwrap();
        let xml = install.read(crate::island::REGISTRY).unwrap();
        let xml = String::from_utf8_lossy(&xml);
        let textures = anno_formats::isd::terrain::GroundTextures::parse(&xml).unwrap();
        let file: Arc<[u8]> = install.read(path).unwrap().into();
        let doc = anno_formats::isd::Doc::parse(&file).unwrap();
        let terrain = anno_formats::isd::terrain::Terrain::read(&doc, path).unwrap();
        let layers = Arc::new(LayerTextures::of(install, &registry, 256).unwrap());
        let paint = Arc::new(LayerPaint::read(&terrain, &textures, &layers));
        let (ground, ground_draw) = island(&terrain, paint, layers).unwrap();
        // Heights only: no objects, so no asset database.
        let field = crate::island::from_bytes(file.clone(), path, registry.clone(), None)
            .unwrap()
            .doc
            .field
            .clone();
        let [w, d] = field.grid.size();
        let mut camera = shot.camera;
        match shot.pivot {
            Pivot::Middle => {}
            Pivot::WestShore => {
                let z = d / 2.;
                let x = (0..w as usize)
                    .map(|x| x as f32)
                    .find(|&x| field.height([x, z]) > 0.)
                    .expect("the middle row reaches land");
                camera.center = Some([x, 0., z]);
            }
            Pivot::At(p) => camera.center = Some(p),
        }
        let map = scene::AmbientMap::read(install).unwrap();
        let profile = map.get(shot.profile).unwrap();
        let water = Water::new(install);
        let mut sea = Sea::new(profile);
        if !shot.fogged {
            sea.atmosphere = [[0.; 4]; 5];
        }
        let sea: Arc<dyn ambient::Sea> = Arc::new(sea);
        water.show(Some(&sea), &field, false, None, Some(12.));
        water.island_file(shot.lakes.then_some(&file));
        let renderer = Renderer::with_families(scene::families());
        let ambient = scene::ambient(profile.clone());
        // The land fogs by the profile whether or not the sea does.
        let mut glow = gpu::Glow::default();
        scene::host_words(profile, &mut glow.host);
        let curve = ambient.sky.as_ref().and_then(|s| s.curve()).unwrap();
        renderer.set_scene(&Scene {
            terrain: Some(Arc::new(ground)),
            ground: ground_draw,
            light: Light {
                sun: profile.toward_sun(),
                sun_color: profile.sun().map(to_linear),
                sky: profile.ambient().map(to_linear),
                ground: profile.ambient().map(to_linear),
                ..Default::default()
            },
            tone: Some(Tone {
                exposure: 1.,
                sun_occlusion: 0.,
                ambient: 1.,
                curve,
                auto: None,
            }),
            sky: Some(Sky {
                zenith: shot.zenith,
                horizon: [0.65, 0.8, 0.9],
                sea: None,
                radiance: None,
                sun_disc: [0.; 3],
                ambient: None,
            }),
            glow,
            passes: vec![Arc::new(water)],
            ..Default::default()
        });
        let mut target = Offscreen::new(shot.size[0], shot.size[1]).unwrap();
        let world = Extent {
            center: [w / 2., 0., d / 2.],
            size: w.max(d),
        };
        let pixels = target.render(&renderer, camera, world).unwrap();
        let bounds = target.bounds();
        let s = camera.project(probe, bounds, world);
        let at = (f32::from(s.y) as usize * shot.size[0] as usize + f32::from(s.x) as usize) * 4;
        Drawn {
            pixels,
            at,
            world,
            bounds,
            field,
        }
    }

    fn save(pixels: &[u8], size: [u32; 2], name: &str) {
        let Some(dir) = std::env::var_os("ANNO2070_WATER_SHOTS") else {
            return;
        };
        let path = std::path::Path::new(&dir).join(name);
        image::save_buffer(
            &path,
            pixels,
            size[0],
            size[1],
            image::ExtendedColorType::Rgba8,
        )
        .unwrap();
        eprintln!("wrote {}", path.display());
    }

    fn digest(pixels: &[u8]) -> String {
        format!("len {} fnv {:016x}", pixels.len(), fnv(pixels))
    }

    const M08: &str = "data/levels/islands/normal/n_m08.isd";
    const L03: &str = "data/levels/islands/normal/n_l03.isd";

    // One island's coast at a fixed wave time, close enough for the coast
    // waves and the shore, drawn with its own ground, against the digest
    // recorded when the sea landed. A changed digest is a changed look:
    // check the shots (ANNO2070_WATER_SHOTS=<dir>) and record the new one.
    // The other shots, a second coast and both islands whole, are for
    // looking at only.
    #[test]
    fn a_coast_draws_at_its_recorded_digest() {
        let Some(install) = install() else {
            return;
        };
        let close = Camera {
            yaw: -1.2,
            tilt: 1.0,
            zoom: 7.,
            fov: 0.7,
            ..Default::default()
        };
        let shot = Shot::new(close, Pivot::WestShore);
        let pixels = draw(&install, M08, &shot, [0.; 3]).pixels;
        save(&pixels, shot.size, "coast_n_m08_close.png");
        if std::env::var_os("ANNO2070_WATER_SHOTS").is_some() {
            let other = draw(&install, L03, &shot, [0.; 3]).pixels;
            save(&other, shot.size, "coast_n_l03_close.png");
            let wide = Camera {
                yaw: 0.6,
                tilt: 1.05,
                zoom: 2.2,
                fov: 0.7,
                ..Default::default()
            };
            let whole = draw(&install, M08, &Shot::new(wide, Pivot::Middle), [0.; 3]).pixels;
            save(&whole, shot.size, "coast_n_m08.png");
            let wide = Camera {
                yaw: 2.4,
                tilt: 0.95,
                zoom: 1.8,
                ..wide
            };
            let whole = draw(&install, L03, &Shot::new(wide, Pivot::Middle), [0.; 3]).pixels;
            save(&whole, shot.size, "coast_n_l03.png");
        }
        assert_eq!(digest(&pixels), "len 1024000 fnv 23a64e76e9e2fa7e");
    }

    // Each profile draws its own sea. North and North_RivalIsland (the
    // sinking island's) hold different Water blocks; through the module,
    // as the editor asks, each one's ambient carries a sea of its own
    // block's values, and the same coast draws differently under each, at
    // a digest recorded for each when the profiles' seas landed.
    #[test]
    fn each_profile_draws_its_own_sea() {
        let Some(install) = install() else {
            return;
        };
        let module: &dyn anno_core::game::Module = &crate::MODULE;
        let names = ["North", "North_RivalIsland"];
        let lights = module.scene().unwrap().ambients(&install, &names);
        assert_eq!(lights.len(), 2);
        let map = crate::scene::AmbientMap::read(&install).unwrap();
        let seas: Vec<Sea> = (lights.iter())
            .map(|(name, ambient)| {
                let sea = ambient.sea.as_ref().expect("every profile has a sea");
                let sea = sea.as_any().downcast_ref::<Sea>().unwrap().clone();
                assert_eq!(sea, Sea::new(map.get(name).unwrap()), "{name}");
                sea
            })
            .collect();
        assert_eq!(
            seas[0],
            Sea {
                atmosphere: seas[0].atmosphere,
                ..north()
            }
        );
        let rival = &seas[1];
        assert_eq!(rival.amplitude, [0.35, 0.38]);
        assert_eq!(rival.absorption, [3.95, 2.18, 0.1]);
        assert_eq!(rival.scattering, [0.1372549, 0.1529412, 0.1490196]);
        assert_ne!(seas[0].absorption, rival.absorption);
        assert_ne!(seas[0].scattering, rival.scattering);

        let close = Camera {
            yaw: -1.2,
            tilt: 1.0,
            zoom: 7.,
            fov: 0.7,
            ..Default::default()
        };
        let digests: Vec<String> = (names.iter())
            .map(|&profile| {
                let shot = Shot {
                    profile,
                    ..Shot::new(close, Pivot::WestShore)
                };
                let pixels = draw(&install, M08, &shot, [0.; 3]).pixels;
                save(&pixels, shot.size, &format!("sea_n_m08_{profile}.png"));
                digest(&pixels)
            })
            .collect();
        eprintln!("{digests:?}");
        assert_ne!(digests[0], digests[1]);
        assert_eq!(
            digests,
            [
                "len 1024000 fnv 23a64e76e9e2fa7e",
                "len 1024000 fnv c5daadaa72d1eeba"
            ]
        );
    }

    // n_l03's lake (N_L17, 3.9 m up) draws from the island file over its
    // ground: without the file the same pixel shows what lies under it.
    // It mirrors the sky cube (another zenith changes it), and at a fixed
    // time matches the digest recorded when the lakes landed.
    #[test]
    fn an_island_lake_draws_with_the_sky_cube_at_its_recorded_digest() {
        let Some(install) = install() else {
            return;
        };
        let file = install.read(L03).unwrap();
        let looks = install.read(lake::SETTINGS).unwrap();
        let looks = lake::looks(&String::from_utf8_lossy(&looks));
        let lakes = lake::read(&file, &looks).unwrap();
        assert_eq!(lakes.len(), 1);
        assert_eq!(looks[lakes[0].look as usize].name, "N_L17");
        let outline = &lakes[0].outline;
        let n = outline.len() as f32;
        let middle = [
            outline.iter().map(|p| p[0]).sum::<f32>() / n,
            lakes[0].height,
            outline.iter().map(|p| p[1]).sum::<f32>() / n,
        ];
        let camera = Camera {
            yaw: 0.8,
            tilt: 0.9,
            zoom: 9.,
            fov: 0.7,
            ..Default::default()
        };
        let shot = Shot::new(camera, Pivot::At(middle));
        let Drawn { pixels, at, .. } = draw(&install, L03, &shot, middle);
        save(&pixels, shot.size, "lake_n_l03.png");
        let lake = &pixels[at..at + 3];
        let dry = Shot {
            lakes: false,
            ..Shot::new(camera, Pivot::At(middle))
        };
        let without = draw(&install, L03, &dry, middle).pixels;
        save(&without, shot.size, "lake_n_l03_without.png");
        assert_ne!(lake, &without[at..at + 3], "the lake is drawn");
        let red = Shot {
            zenith: [1.5, 0.1, 0.1],
            ..Shot::new(camera, Pivot::At(middle))
        };
        let reddened = draw(&install, L03, &red, middle).pixels;
        let mirrored = &reddened[at..at + 3];
        assert!(
            mirrored[0] > lake[0],
            "a red sky reddens the lake: {mirrored:?} after {lake:?}"
        );
        assert_eq!(digest(&pixels), "len 1024000 fnv f0711905701c906c");
    }

    /// Runs `wgsl`'s `cs_probe`, appended to the sea's module, once per
    /// case: the cases (`stride` bytes each) at group 3 binding 0, its
    /// output, `out` vec4 a case, at 1, and `uniform` at 2.
    fn run_probe(
        target: &Offscreen,
        wgsl: &str,
        cases: &[u8],
        stride: usize,
        uniform: &[u8],
        out: usize,
    ) -> Vec<[f32; 4]> {
        use wgpu::util::DeviceExt;
        let (device, queue) = (target.device(), target.queue());
        let count = cases.len() / stride;
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("2070 sea probe"),
            source: wgpu::ShaderSource::Wgsl((module_source() + wgsl).into()),
        });
        let pipeline = device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
            label: Some("2070 sea probe"),
            layout: None,
            module: &module,
            entry_point: Some("cs_probe"),
            compilation_options: Default::default(),
            cache: None,
        });
        let buffer = |contents: &[u8], usage| {
            device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: None,
                contents,
                usage,
            })
        };
        let input = buffer(cases, wgpu::BufferUsages::STORAGE);
        let uniform = buffer(uniform, wgpu::BufferUsages::UNIFORM);
        let size = (count * out * 16) as u64;
        let output = device.create_buffer(&wgpu::BufferDescriptor {
            label: None,
            size,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC,
            mapped_at_creation: false,
        });
        let readback = device.create_buffer(&wgpu::BufferDescriptor {
            label: None,
            size,
            usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let entries = [
            input.as_entire_binding(),
            output.as_entire_binding(),
            uniform.as_entire_binding(),
        ];
        let entries: Vec<_> = (entries.into_iter().enumerate())
            .map(|(i, resource)| wgpu::BindGroupEntry {
                binding: i as u32,
                resource,
            })
            .collect();
        let group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: None,
            layout: &pipeline.get_bind_group_layout(3),
            entries: &entries,
        });
        let mut encoder = device.create_command_encoder(&Default::default());
        {
            let mut pass = encoder.begin_compute_pass(&Default::default());
            pass.set_pipeline(&pipeline);
            pass.set_bind_group(3, &group, &[]);
            pass.dispatch_workgroups(count as u32, 1, 1);
        }
        encoder.copy_buffer_to_buffer(&output, 0, &readback, 0, size);
        queue.submit([encoder.finish()]);
        readback.slice(..).map_async(wgpu::MapMode::Read, |_| {});
        device.poll(wgpu::PollType::wait_indefinitely()).unwrap();
        let mapped = readback.slice(..).get_mapped_range().expect("mapped");
        bytemuck::cast_slice(&mapped).to_vec()
    }

    /// The North profile's lighting as the scene sets it (scene.rs
    /// `ambient`), with 2070's display curve.
    fn north_light(profile: &crate::scene::Profile) -> (Light, Tone) {
        use crate::scene::to_linear;
        let light = Light {
            sun: profile.toward_sun(),
            sun_color: profile.sun().map(to_linear),
            sky: profile.ambient().map(to_linear),
            ground: profile.ambient().map(to_linear),
            shadows: None,
            fog: None,
        };
        let tone = Tone {
            exposure: 1.,
            sun_occlusion: 0.,
            ambient: 1.,
            curve: gpu::Curve::Shoulder {
                gamma: 2.2,
                max_linear: 1.,
                white: crate::scene::GLOW_BASE,
            },
            auto: None,
        };
        (light, tone)
    }

    /// Deep water everywhere around the origin, for a sea with no coast.
    fn deep_field() -> Arc<Heightfield> {
        let grid = Grid {
            width: 9,
            depth: 9,
            spacing: 10.,
            origin: [-40.; 2],
        };
        Arc::new(Heightfield::new(grid, vec![-50.; 81]))
    }

    // The sea scatters as the land does, the constants and the function
    // one (scene.rs `ATMOSPHERE_WGSL`, the sea's copy of the constants
    // from `Scatter::words`). Down the middle of the view, where the
    // scattering is partway (80 to 200 m out), the sea's module fogs the
    // land's unfogged colour to what the land draws there through the
    // scene's fog, and both to the CPU's `Scatter::at`. Farther out than
    // the far plane, where the scattering is whole and the colour beneath
    // gone, the sea pass itself, scattered per vertex, draws what the land
    // draws at the same pixels.
    #[test]
    fn the_sea_and_the_land_scatter_alike() {
        use crate::scene::{self, held_eye, tests as lit};
        use gpui_editor_kit::gpu::{Families, Glow, Piece};
        use gpui_editor_kit::gpui::{point, px};

        let profile = lit::north();
        let families = Families::with_lighting(vec![lit::probe()], scene::lighting()).unwrap();
        let (w, h) = (480u32, 320u32);
        let mut target = Offscreen::new(w, h).unwrap();
        let camera = Camera {
            fov: 0.9,
            tilt: 1.2,
            yaw: 0.4,
            ..Default::default()
        };
        let world = Extent {
            center: [0.; 3],
            size: 40.,
        };
        let bounds = target.bounds();
        let eye = camera.eye(bounds, world).unwrap();
        let anchor = camera.anchor(world);
        let held = held_eye(eye, anchor);
        let sun = profile.toward_sun();
        // Each pixel of a few columns that sees the ground: where, and how
        // far from the eye the scattering measures from.
        let mut seen = Vec::new();
        for x in [120u32, 240, 360] {
            for y in 0..h {
                let ray = camera.ray(point(px(x as f32 + 0.5), px(y as f32 + 0.5)), bounds, world);
                if ray.direction[1] >= -1e-3 {
                    continue;
                }
                let t = -ray.origin[1] / ray.direction[1];
                let at: [f32; 3] =
                    std::array::from_fn(|a| (ray.origin[a] + ray.direction[a] * t) as f32);
                let d = (0..3)
                    .map(|a| (at[a] - held[a]).powi(2))
                    .sum::<f32>()
                    .sqrt();
                seen.push((x, y, at, d));
            }
        }
        let nearest = |want: f32| {
            *(seen.iter().filter(|c| c.0 == 240))
                .min_by(|a, b| (a.3 - want).abs().total_cmp(&(b.3 - want).abs()))
                .unwrap()
        };
        let partway: Vec<_> = [80., 140., 200.].map(nearest).into();
        // Past the far plane (350 m), and short of where the ring grid's
        // cells grow too long for one vertex's scattering to stand for a
        // pixel's.
        let far: Vec<_> = (seen.iter())
            .filter(|c| c.3 > 420. && c.3 < 2000. && c.1 % 6 == 0)
            .copied()
            .collect();
        assert!(far.len() >= 6, "{} far pixels", far.len());
        for c in &partway {
            let (fex, _) = profile.scatter().at(held, c.2, sun);
            assert!(
                fex[0] > 0.05 && fex[0] < 0.95,
                "{:.0} m: extinction {fex:?}",
                c.3
            );
        }
        for c in &far {
            let (fex, _) = profile.scatter().at(held, c.2, sun);
            assert!(
                fex.iter().all(|&e| e < 1e-6),
                "{:.0} m: extinction {fex:?}",
                c.3
            );
        }

        // The land: the probe family's plain look at each point, through
        // the scene's shade and fog.
        let (light, tone) = north_light(&profile);
        let look = [0., 0., 1., 1.];
        let word = 255u32 << 16 | 255 << 24;
        let cases: Vec<_> = partway.iter().chain(&far).copied().collect();
        let quad = MeshData {
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
        let rows: Vec<_> = (cases.iter())
            .map(|c| {
                let mut m = math::translate(c.2);
                m[0] = (0.05 * c.3).max(2.);
                m[10] = m[0];
                m
            })
            .collect();
        let mut glow = Glow {
            strength: 1.,
            ..Glow::default()
        };
        scene::host_words(&profile, &mut glow.host);
        let scene = |passes: Vec<Arc<dyn gpu::Pass>>| Scene {
            light,
            tone: Some(tone),
            glow,
            passes,
            ..Default::default()
        };
        let renderer = Renderer::with_families(families.clone());
        renderer.set_scene(&scene(Vec::new()));
        renderer.set_pieces(vec![Piece {
            levels: vec![(Arc::new(Mesh::new(quad).unwrap()), 0.)],
            impostor: None,
            pose: None,
        }]);
        renderer.set_placements(&vec![0; rows.len()], &rows);
        renderer.set_instance_words(0, &vec![word; rows.len()]);
        let land = target.render(&renderer, camera, world).unwrap();

        // The sea alone, scattered by its own copy of the constants.
        let sea = Sea {
            atmosphere: profile.scatter().words(),
            ..north()
        };
        let pass = Water::with(Some(Arc::new(flat())));
        let shown: Arc<dyn ambient::Sea> = Arc::new(sea.clone());
        pass.show(Some(&shown), &deep_field(), false, None, Some(0.));
        let renderer = Renderer::with_families(families.clone());
        renderer.set_scene(&scene(vec![Arc::new(pass)]));
        let water = target.render(&renderer, camera, world).unwrap();

        let pixel = |pixels: &[u8], c: &(u32, u32, [f32; 3], f32)| {
            let i = ((c.1 * w + c.0) * 4) as usize;
            [0, 1, 2].map(|k| f32::from(pixels[i + k]))
        };
        let shown = |c: [f32; 3]| c.map(|c| c.clamp(0., 1.) * 255.);
        let apart =
            |a: [f32; 3], b: [f32; 3]| (0..3).map(|k| (a[k] - b[k]).abs()).fold(0., f32::max);
        // The sea module's scattering of the land's own unfogged colour.
        let unfogged: Vec<[f32; 3]> = (partway.iter())
            .map(|c| lit::reference(&profile, sun, eye, anchor, c.2, look, false))
            .collect();
        let probe_cases: Vec<[f32; 16]> = (partway.iter().zip(&unfogged))
            .map(|(c, base)| {
                let mut v = [0.; 16];
                v[0..3].copy_from_slice(&held);
                v[4..7].copy_from_slice(&c.2);
                v[8..11].copy_from_slice(&sun);
                v[12..15].copy_from_slice(base);
                v
            })
            .collect();
        let probe = "
            struct ProbeCase { eye: vec4<f32>, world: vec4<f32>, sun: vec4<f32>, base: vec4<f32> }
            @group(3) @binding(0) var<storage, read> probe_in: array<ProbeCase>;
            @group(3) @binding(1) var<storage, read_write> probe_out: array<vec4<f32>>;
            @group(3) @binding(2) var<uniform> probe_air: scene_atmosphere;
            @compute @workgroup_size(1) fn cs_probe(@builtin(global_invocation_id) id: vec3<u32>) {
                let c = probe_in[id.x];
                let s = scene_scatter(probe_air, c.eye.xyz, c.world.xyz, c.sun.xyz);
                probe_out[id.x] = vec4<f32>(scene_scattered(c.base.rgb, s), 0.0);
            }";
        let fogged = run_probe(
            &target,
            probe,
            bytemuck::cast_slice(&probe_cases),
            64,
            bytemuck::cast_slice(&sea.atmosphere),
            1,
        );
        let mut worst = [0f32; 4];
        for (i, c) in partway.iter().enumerate() {
            let cpu = lit::reference(&profile, sun, eye, anchor, c.2, look, true);
            let sea_gpu = [fogged[i][0], fogged[i][1], fogged[i][2]];
            let off = apart(sea_gpu, cpu);
            assert!(
                off < 2e-3,
                "{:.0} m: the sea's scattering {sea_gpu:?}, the CPU's {cpu:?}",
                c.3
            );
            let drawn = pixel(&land, c);
            let land_off = apart(drawn, shown(cpu));
            assert!(
                land_off <= 3.,
                "{:.0} m: the land drew {drawn:?}, the CPU {:?}",
                c.3,
                shown(cpu)
            );
            worst[0] = worst[0].max(off * 255.);
            worst[1] = worst[1].max(apart(drawn, shown(sea_gpu)));
            eprintln!(
                "{:.0} m: unfogged {:?}, land drew {drawn:?}, sea fogs it to {:?}, CPU {:?}",
                c.3,
                shown(unfogged[i]),
                shown(sea_gpu),
                shown(cpu)
            );
        }
        for c in &far {
            let cpu = lit::reference(&profile, sun, eye, anchor, c.2, look, true);
            let (drawn, sea_drawn) = (pixel(&land, c), pixel(&water, c));
            worst[2] = worst[2].max(apart(drawn, shown(cpu)));
            worst[3] = worst[3].max(apart(sea_drawn, drawn));
            assert!(
                apart(drawn, shown(cpu)) <= 3. && apart(sea_drawn, drawn) <= 4.,
                "{:.0} m at {},{}: land {drawn:?}, sea {sea_drawn:?}, CPU {:?}",
                c.3,
                c.0,
                c.1,
                shown(cpu)
            );
        }
        eprintln!(
            "levels of 255 apart: sea probe to CPU {:.2}, land to sea probe {:.1}, \
             far land to CPU {:.1}, far sea to land {:.1} ({} far pixels)",
            worst[0],
            worst[1],
            worst[2],
            worst[3],
            far.len()
        );
    }

    // Sky and sea meet at the horizon without a seam. Away from the sun,
    // 2070's sky (sky.fx ComputeSkyColor, sky.rs) and the scattering the
    // sea wears far out (whole, so nothing of the water shows) are one
    // gradient: on the CPU they agree as the view closes on the horizon
    // from above and below. Drawn, the step from the last sky row to the
    // first sea row is no bigger than the steps along either side, and
    // each side matches its CPU formula.
    #[test]
    fn sky_and_sea_meet_at_the_horizon() {
        use crate::scene::{held_eye, tests as lit, to_gamma, to_linear};
        use crate::sky::{SkyPasses, Volume};
        use anno_core::ambient::SkyPass as _;
        use gpui_editor_kit::gpui::{point, px};

        let profile = lit::north();
        let toward = profile.toward_sun();
        let scatter = profile.scatter();
        let look = crate::sky::look(&profile, 0.);
        // sky.fx below the clouds' reach (p.y <= 0 thins them to none).
        let sky_at = |p: [f32; 3], height: f32| -> [f32; 3] {
            let saturate = |x: f32| x.clamp(0., 1.);
            let offset = height / look[0][3];
            let zenith = 1. - saturate((p[1] + offset) / (offset + 1.));
            let t = look[4];
            let angle = p[0] * t[0] + p[1] * t[1] + p[2] * t[2];
            let glow = (saturate(saturate(angle) - t[3]) * look[3][3])
                .max(0.)
                .powf(look[2][3]);
            let angle = saturate(angle * angle * 0.3 + 0.7);
            std::array::from_fn(|c| {
                angle * (look[0][c] + (look[1][c] - look[0][c]) * zenith.powf(look[2][c]))
                    + look[3][c] * glow
            })
        };
        let away = {
            let l = (toward[0] * toward[0] + toward[2] * toward[2]).sqrt();
            [-toward[0] / l, 0., -toward[2] / l]
        };
        for height in [20f32, 60., 140.] {
            let dir = |y: f32| {
                let l = (1. + y * y).sqrt();
                [away[0] / l, y / l, away[2] / l]
            };
            let sky = sky_at(dir(1e-4), height);
            // Seen from `height`, sea level lies `height / 1e-4` out along
            // this ray: far past the far plane.
            let (fex, sea) = scatter.along(height, dir(-1e-4), height * 1e4, toward);
            assert!(fex.iter().all(|&e| e < 1e-6));
            for c in 0..3 {
                assert!(
                    (sky[c] - sea[c]).abs() < 1e-3,
                    "{height} m up: sky {sky:?}, sea {sea:?}"
                );
            }
        }

        let camera = Camera {
            fov: 0.9,
            tilt: 1.25,
            yaw: toward[0].atan2(toward[2]),
            ..Default::default()
        };
        let world = Extent {
            center: [0.; 3],
            size: 40.,
        };
        let (w, h) = (320u32, 240u32);
        let mut target = Offscreen::new(w, h).unwrap();
        let bounds = target.bounds();
        let eye = camera.eye(bounds, world).unwrap();
        let height = held_eye(eye, camera.anchor(world))[1];
        let skies = SkyPasses::with_volume(Volume::empty());
        skies.hold(Some(0.));
        let ambient = crate::scene::ambient(profile.clone());
        skies.show(ambient.sky.as_ref(), 12.);
        let pass = Water::with(Some(Arc::new(flat())));
        let sea = Sea {
            atmosphere: scatter.words(),
            ..north()
        };
        let shown: Arc<dyn ambient::Sea> = Arc::new(sea);
        pass.show(Some(&shown), &deep_field(), false, None, Some(0.));
        let (light, tone) = north_light(&profile);
        let mut passes = skies.passes();
        passes.push(Arc::new(pass));
        let renderer = Renderer::default();
        renderer.set_scene(&Scene {
            light,
            tone: Some(tone),
            passes,
            ..Default::default()
        });
        let pixels = target.render(&renderer, camera, world).unwrap();
        let x = w / 2;
        let ray = |y: u32| {
            let r = camera.ray(point(px(x as f32 + 0.5), px(y as f32 + 0.5)), bounds, world);
            r.direction.map(|c| c as f32)
        };
        let horizon = (0..h)
            .find(|&y| ray(y)[1] < 0.)
            .expect("the horizon is in view");
        assert!(horizon > 8 && horizon < h - 8, "horizon at row {horizon}");
        let at = |y: u32| {
            let i = ((y * w + x) * 4) as usize;
            [0, 1, 2].map(|k| f32::from(pixels[i + k]))
        };
        let step = |a: u32, b: u32| {
            (0..3)
                .map(|k| (at(a)[k] - at(b)[k]).abs())
                .fold(0., f32::max)
        };
        let shown = |c: [f32; 3]| c.map(|c| to_gamma(to_linear(c)).clamp(0., 1.) * 255.);
        let apart =
            |a: [f32; 3], b: [f32; 3]| (0..3).map(|k| (a[k] - b[k]).abs()).fold(0., f32::max);
        let (sky_row, sea_row) = (horizon - 1, horizon);
        let sky_want = shown(sky_at(ray(sky_row), height));
        let p = ray(sea_row);
        let (_, sea_want) = scatter.along(height, p, height / -p[1], toward);
        let sea_want = shown(sea_want);
        let across = step(sky_row, sea_row);
        let along = step(sky_row - 1, sky_row).max(step(sea_row, sea_row + 1));
        eprintln!(
            "row {horizon}: sky {:?} (CPU {sky_want:?}), sea {:?} (CPU {sea_want:?}); \
             step across {across}, along {along}",
            at(sky_row),
            at(sea_row)
        );
        assert!(apart(at(sky_row), sky_want) <= 4., "the sky");
        assert!(apart(at(sea_row), sea_want) <= 4., "the sea");
        assert!(across <= 2. * along + 3., "a seam at the horizon");
    }

    // The sea lights by the scene's light at the scene's scale. The frame's
    // sun and ambient are the profile's cLgtDiffuse and cLgtAmbient made
    // linear (scene.rs `ambient`): shade() takes them back to gamma, and so
    // does the sea, getting the profile's own values. Drawn from above over
    // open deep water with flat waves, where waterocean.fx's light is
    // colour · dot(lerp(ambient, sun, N·L), 0.33) plus a fixed term, the
    // pixel moves with the frame's light, in step with it: North's light
    // scaled by k in gamma gives pixels on one line in k.
    #[test]
    fn the_sea_lights_by_the_scenes_light() {
        use crate::scene::{tests as lit, to_gamma, to_linear};
        let profile = lit::north();
        let (light, tone) = north_light(&profile);
        // The scale shade() and the sea both read the light at.
        for (frame, own) in [
            (light.sun_color, profile.sun()),
            (light.sky, profile.ambient()),
        ] {
            for c in 0..3 {
                assert!(
                    (to_gamma(frame[c]) - own[c]).abs() < 1e-4,
                    "{frame:?} {own:?}"
                );
            }
        }
        let mut target = Offscreen::new(64, 64).unwrap();
        let world = Extent {
            center: [0.; 3],
            size: 40.,
        };
        let camera = Camera::default();
        let sea = Sea::new(&profile);
        let scales = [0.15f32, 0.3, 0.5];
        let drawn: Vec<[f32; 3]> = (scales.iter())
            .map(|&k| {
                let pass = Water::with(Some(Arc::new(flat())));
                let shown: Arc<dyn ambient::Sea> = Arc::new(sea.clone());
                pass.show(Some(&shown), &deep_field(), false, None, Some(0.));
                let scaled = |c: [f32; 3]| c.map(|c| to_linear(k * c));
                let renderer = Renderer::default();
                renderer.set_scene(&Scene {
                    light: Light {
                        sun_color: scaled(profile.sun()),
                        sky: scaled(profile.ambient()),
                        ground: scaled(profile.ambient()),
                        ..light
                    },
                    tone: Some(tone),
                    passes: vec![Arc::new(pass)],
                    ..Default::default()
                });
                let pixels = target.render(&renderer, camera, world).unwrap();
                let i = (32 * 64 + 32) * 4;
                [0, 1, 2].map(|c| f32::from(pixels[i + c]))
            })
            .collect();
        // Unclamped at these scales: North's light term is about 1.1 at
        // k = 1 (and clamps there), so k = 0.5 keeps it under 1.
        let up = profile.toward_sun()[1];
        let term = |k: f32| {
            let (a, s) = (profile.ambient(), profile.sun());
            (0..3)
                .map(|c| k * (a[c] + (s[c] - a[c]) * up) * 0.33)
                .sum::<f32>()
        };
        assert!(
            term(0.5) < 1. && term(1.) > 1.,
            "{} {}",
            term(0.5),
            term(1.)
        );
        eprintln!("the sea at k {scales:?}: {drawn:?}");
        let t = (scales[2] - scales[0]) / (scales[1] - scales[0]);
        for c in 0..3 {
            let moved = drawn[2][c] - drawn[0][c];
            assert!(
                moved > 8.,
                "channel {c}: the sea keeps its own light {drawn:?}"
            );
            let want = drawn[0][c] + t * (drawn[1][c] - drawn[0][c]);
            assert!(
                (drawn[2][c] - want).abs() <= 2.5,
                "channel {c}: {drawn:?} is not linear in the light"
            );
        }
    }

    const M02: &str = "data/levels/islands/normal/n_m02.isd";

    // The water_shots overview of n_m02 (the editor's 3D camera, tilt 0.9,
    // yaw 0.6) shows land and sea in one haze. Pixels are sorted by how far
    // what they show lies from the eye the scattering measures from; in
    // the 20 m band holding the most of both, the land's and the sea's
    // mean colours stand apart by their own colours alone once the sea
    // scatters too, much less than when only the land did (the sea
    // unscattered, `fogged` off). Both are recorded.
    #[test]
    fn n_m02_land_and_sea_wear_one_haze() {
        let Some(install) = install() else {
            return;
        };
        let camera = Camera {
            fov: 0.9,
            zoom: 1.3,
            mirror: true,
            tilt: 0.9,
            yaw: 0.6,
            ..Camera::perspective()
        };
        let shot = Shot {
            size: [800, 450],
            ..Shot::new(camera, Pivot::Middle)
        };
        let after = draw(&install, M02, &shot, [0.; 3]);
        let before = draw(
            &install,
            M02,
            &Shot {
                fogged: false,
                size: shot.size,
                ..Shot::new(camera, Pivot::Middle)
            },
            [0.; 3],
        );
        save(&after.pixels, shot.size, "haze_n_m02_after.png");
        save(&before.pixels, shot.size, "haze_n_m02_before.png");
        let field = &after.field;
        let top = field.heights.iter().copied().fold(0., f32::max);
        let eye = camera.eye(after.bounds, after.world).unwrap();
        let held = crate::scene::held_eye(eye, camera.anchor(after.world));
        // Per pixel (every other one): land or sea, and the band of 20 m
        // what it shows lies in. The ray walks down from the land's top to
        // sea level in half metres.
        let [w, h] = shot.size;
        let bands = 40usize;
        let mut sorted: Vec<(bool, usize, usize)> = Vec::new();
        for y in (0..h).step_by(2) {
            for x in (0..w).step_by(2) {
                use gpui_editor_kit::gpui::{point, px};
                let ray = camera.ray(
                    point(px(x as f32 + 0.5), px(y as f32 + 0.5)),
                    after.bounds,
                    after.world,
                );
                let (o, d) = (
                    ray.origin.map(|v| v as f32),
                    ray.direction.map(|v| v as f32),
                );
                if d[1] >= -1e-3 {
                    continue;
                }
                let along = |t: f32| [o[0] + d[0] * t, o[1] + d[1] * t, o[2] + d[2] * t];
                let (from, to) = ((top - o[1]) / d[1], -o[1] / d[1]);
                let mut t = from.max(0.);
                let mut land = false;
                while t < to {
                    let p = along(t);
                    if p[1] <= field.height([p[0], p[2]]) {
                        land = true;
                        break;
                    }
                    t += 0.5;
                }
                let p = along(t.min(to));
                let far = (0..3).map(|a| (p[a] - held[a]).powi(2)).sum::<f32>().sqrt();
                let band = (far / 20.) as usize;
                if band < bands {
                    sorted.push((land, band, ((y * w + x) * 4) as usize));
                }
            }
        }
        let mut counts = vec![[0u32; 2]; bands];
        for &(land, band, _) in &sorted {
            counts[band][usize::from(land)] += 1;
        }
        let band = (0..bands)
            .max_by_key(|&b| counts[b][0].min(counts[b][1]))
            .unwrap();
        assert!(
            counts[band].iter().all(|&n| n > 500),
            "band {band}: {:?} sea and land pixels",
            counts[band]
        );
        let mean = |pixels: &[u8], land: bool| {
            let mut sum = [0f64; 3];
            let mut n = 0f64;
            for &(l, b, i) in &sorted {
                if l == land && b == band {
                    (0..3).for_each(|k| sum[k] += f64::from(pixels[i + k]));
                    n += 1.;
                }
            }
            sum.map(|v| (v / n) as f32)
        };
        let gap =
            |a: [f32; 3], b: [f32; 3]| (0..3).map(|k| (a[k] - b[k]).powi(2)).sum::<f32>().sqrt();
        let (land_before, sea_before) = (mean(&before.pixels, true), mean(&before.pixels, false));
        let (land_after, sea_after) = (mean(&after.pixels, true), mean(&after.pixels, false));
        eprintln!(
            "n_m02 overview, {}-{} m from the eye ({} sea, {} land pixels):\n  \
             before: land {land_before:.1?}, sea {sea_before:.1?}, apart {:.1}\n  \
             after:  land {land_after:.1?}, sea {sea_after:.1?}, apart {:.1}",
            band * 20,
            band * 20 + 20,
            counts[band][0],
            counts[band][1],
            gap(land_before, sea_before),
            gap(land_after, sea_after),
        );
        // The land itself draws as it did; only the sea takes the haze.
        assert!(
            gap(land_before, land_after) < 1.,
            "{land_before:?} {land_after:?}"
        );
        assert!(
            gap(land_after, sea_after) < 0.6 * gap(land_before, sea_before),
            "the haze still jumps at the coast"
        );
    }
}
