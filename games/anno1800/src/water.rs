// Owns Anno 1800's sea as the island view draws it (water_ocean.fx): the
// pass that keeps its GPU state, steps its waves and draws them in the
// kit's Water stage. Two wave sets as the game has them: small waves from
// the shipped 48-frame volume (waves_small_xz/y.dds) and large ones from a
// Phillips spectrum the preset sets, transformed on the GPU each frame
// (waves.wgsl), with Perlin noise past the patch blend; shaded by
// surface.wgsl. Rivers and lakes are drawn in the same pass after the sea
// (water::inland, water_inland_water.fx).
// Engine addresses here are Anno1800.exe build 16367353 (Steam).
mod inland;
mod physical;

use crate::sea;
use anno_core::ambient::Sea;
use anno_core::water::{Coast, CoastMap, Inland, IslandMaps, IslandMapsReader, gauss, half};
use anno_formats::vfs::Install;
use anyhow::{Context, Result, ensure};
use gpui_editor_kit::gpu::LockAny as _;
use gpui_editor_kit::gpu::{self, Stage, bc_supported};
use gpui_editor_kit::heightfield::Heightfield;
use std::sync::{Arc, Mutex};

/// Texels a side of every wave map (FFT_MAP_SIZE and ACTUAL_DIM, as the
/// game compiles water_ocean.fx).
const N: u32 = 512;
/// The spectrum's rows are four entries wider than the map (IN_WIDTH).
const W: u32 = N + 4;
/// Frames of the small waves' loop (the volume's depth).
const FRAMES: u32 = 48;
/// The gradient maps' mips, down to one texel.
const MIPS: u32 = 10;

/// The ocean simulator's description as CRenderer::InitOcean passes it
/// (Anno1800.exe 0x141ec25ab), in the order of NVIDIA's OceanCS sample it
/// follows: patch length and displacement scale, choppiness (the preset's
/// WaveChoppiness replaces it), Perlin size and speed, then the Perlin
/// octaves' amplitudes, gradients and frequencies.
const PATCH: f32 = 4096.;
const DISPLACEMENT_SCALE: f32 = 1. / 256.;
const PERLIN_SIZE: f32 = 2.6;
const PERLIN_SPEED: f32 = 0.3;
const PERLIN_AMPLITUDE: [f32; 3] = [35., 39., 46.];
const PERLIN_GRADIENT: [f32; 3] = [2., 2.2, 2.6];
const PERLIN_OCTAVE: [f32; 3] = [0.45, 0.23, 1.09];
/// The large waves' shortest wave, as CRenderer::InitOcean sets it; the
/// preset does not override it.
const MIN_WAVE: f32 = 1000.;
/// Patch blend from the FFT waves to Perlin noise, world units from the
/// camera (PATCH_BLEND_BEGIN/END as compiled).
const BLEND: [f32; 2] = [35., 320.];
/// The wave clock: the simulator adds frame milliseconds times 0.0005
/// (0x141d1766d), half a cWaveTime a second, so the small waves' 48
/// frames loop every two seconds. The delta is game time in milliseconds:
/// its other readers divide it by 1000 (0x14550df10), and the water stops
/// while the game is paused.
const WAVE_TIME_RATE: f32 = 0.5;
/// Ring grid around the camera: as 117's ocean.
const SEGMENTS: u32 = 256;
const RINGS: u32 = 180;
const RING_GROWTH: f32 = 1.045;
/// Vectors in the surface's uniforms (SeaParams); the physical waves'
/// last.
const PARAMS: usize = 13;

/// How the sea, rivers and lakes trace reflections of the island, after
/// psScreenSpaceReflect (40 samples, steps 8 pixel footprints long growing
/// 1.03 times a step). Its steps follow the pixel's footprint, so the
/// shader sizes each ray's reach itself (surface.wgsl's ssr_reach) and
/// `reach` only caps it, for views so flat that the footprint runs off.
/// The game fades reflections toward the screen's edges as
/// 1 - |2 uv - 1|^12; the kit fades linearly over `edge`, which at 1 / 13
/// takes the same share of the screen. TODO: the game tests hits on
/// depth to the 32nd power, not in world units, so `thickness` and the
/// cap are tuned by eye against retail screenshots.
const REFLECTIONS: gpu::Ssr = gpu::Ssr {
    steps: 40,
    reach: 1500.,
    thickness: 6.,
    edge: 1. / 13.,
};

/// The small waves as shipped: two BC6H (signed) volumes, sideways
/// displacement in red and green, height in red.
struct Small {
    xz: Vec<u8>,
    y: Vec<u8>,
}

impl Small {
    fn read(install: &Install) -> Option<Self> {
        let read = |name: &str| {
            let path = format!("data/graphics/effects/water/{name}.dds");
            install
                .read(&path)
                .with_context(|| path.clone())
                .and_then(|b| volume(&b).with_context(|| path.clone()))
                .map_err(|e| eprintln!("1800 water: {e:#}"))
                .ok()
        };
        Some(Self {
            xz: read("waves_small_xz")?,
            y: read("waves_small_y")?,
        })
    }
}

/// The block data of a 512² 48-frame signed BC6H volume.
fn volume(dds: &[u8]) -> Result<Vec<u8>> {
    ensure!(dds.len() >= 148 && &dds[..4] == b"DDS ", "not a DDS file");
    let u32_at = |at: usize| u32::from_le_bytes(dds[at..at + 4].try_into().unwrap());
    let (height, width, depth) = (u32_at(12), u32_at(16), u32_at(24));
    ensure!(
        &dds[84..88] == b"DX10" && u32_at(128) == 96,
        "not signed BC6H"
    );
    ensure!(
        width == N && height == N && depth == FRAMES,
        "{width}x{height}x{depth}, expected {N}x{N}x{FRAMES}"
    );
    let bytes = (N as usize / 4).pow(2) * 16 * FRAMES as usize;
    ensure!(dds.len() >= 148 + bytes, "file too short");
    Ok(dds[148..148 + bytes].to_vec())
}

/// Mips of `perlin_noise.dds`, 64² down to 1² (D3DFMT 113: RGBA half
/// floats): the far waves alias into speckled glints without them.
const PERLIN_MIPS: u32 = 7;

/// `perlin_noise.dds`, every mip, half floats in order.
fn perlin(install: &Install) -> Option<Vec<u16>> {
    let path = "data/graphics/effects/noise/perlin_noise.dds";
    let bytes = install.read(path).ok()?;
    let texels: usize = (0..PERLIN_MIPS).map(|m| (64usize >> m).pow(2)).sum();
    let ok = bytes.len() >= 128 + texels * 8
        && &bytes[..4] == b"DDS "
        && u32::from_le_bytes(bytes[84..88].try_into().unwrap()) == 113
        && u32::from_le_bytes(bytes[28..32].try_into().unwrap()) >= PERLIN_MIPS;
    if !ok {
        eprintln!("1800 water: {path}: not 64² RGBA16F with its mips");
        return None;
    }
    Some(
        bytes[128..128 + texels * 8]
            .as_chunks::<2>()
            .0
            .iter()
            .map(|b| u16::from_le_bytes(*b))
            .collect(),
    )
}

/// The large waves' H(0) and ω (csInitWaveHeightMap) for `sea`: rows
/// 0..=N of W entries, K's Y by row and X by column centred on N / 2, in
/// the game's units (patch PATCH, gravity 981). The simulator's own
/// spectrum draws its random numbers from the C runtime seeded with
/// WaveSeed; this follows csInitWaveHeightMap's TeaRand, so the swell is
/// the game's in kind, not wave for wave.
/// The large waves' initial heights and dispersion (`large_spectrum`).
type Spectrum = (Vec<[f32; 2]>, Vec<f32>);

pub(crate) fn large_spectrum(sea: &sea::Water) -> Spectrum {
    let rows = (N + 1) as usize;
    let mut h0 = vec![[0f32; 2]; rows * W as usize];
    let mut omega = vec![0f32; rows * W as usize];
    let step = 2. * std::f32::consts::PI / PATCH;
    let seed = sea.seed.max(1);
    for r in 0..=N {
        for c in 0..=N {
            let k = [
                (c as f32 - N as f32 / 2.) * step,
                (r as f32 - N as f32 / 2.) * step,
            ];
            let i = (r * W + c) as usize;
            omega[i] = (GRAVITY * k[0].hypot(k[1])).sqrt();
            let phil = phillips(k, sea).sqrt();
            h0[i] = [
                phil * gauss(r * r, c, seed) * std::f32::consts::FRAC_1_SQRT_2,
                phil * gauss(r, c * c, seed) * std::f32::consts::FRAC_1_SQRT_2,
            ];
        }
    }
    (h0, omega)
}

/// Gravity in the spectrum's units (GRAV_ACCEL, cm/s²).
const GRAVITY: f32 = 981.;

/// The Phillips spectrum at wave vector `k` for `sea`'s wind, as
/// functions_water.fxh Phillips: waves up to v² / g long, those against
/// the wind kept at WindDamping, the shortest (under MIN_WAVE's share)
/// damped out. The amplitude is the preset's times 1e-8, as the
/// simulator's own spectrum takes it (0x141d16b2d).
fn phillips(k: [f32; 2], sea: &sea::Water) -> f32 {
    let sq = k[0] * k[0] + k[1] * k[1];
    if sq == 0. {
        return 0.;
    }
    let l = sea.wind_speed * sea.wind_speed / GRAVITY;
    let w = l / MIN_WAVE;
    let cos = k[0] * sea.wind[0] + k[1] * sea.wind[1];
    let mut p = sea.amplitude * 1e-8 * (-1. / (l * l * sq)).exp() / (sq * sq * sq) * cos * cos;
    if cos < 0. {
        p *= sea.damping;
    }
    p * (-sq * w * w).exp()
}

/// 1800's water for one island view: what `show` last set, the shipped
/// textures (read once with the island), and the GPU state built on the
/// first frame and kept while the view lives.
pub struct Water {
    shown: Mutex<Shown>,
    surface: Mutex<gpu::OnDevice<Surface>>,
    /// The surface while a thread compiles its pipelines.
    building: Mutex<gpu::OnDevice<std::sync::mpsc::Receiver<Surface>>>,
    small: Arc<Small>,
    perlin: Arc<Vec<u16>>,
    /// Foam and noise for rivers and lakes; `None` leaves them the kit's
    /// flat water.
    inland: Option<Arc<inland::Textures>>,
}

#[derive(Default)]
struct Shown {
    sea: Option<sea::Water>,
    /// The island's rivers and lakes.
    inland: Option<Arc<Inland>>,
    /// Waves held at this many seconds; `None` on the clock.
    time: Option<f32>,
    coast: CoastMap,
    /// The island's painted water maps.
    maps: IslandMapsReader,
    detail: anno_core::ambient::Detail,
}

impl Water {
    /// Reads the small waves (25 MB) and the Perlin noise from `install`.
    /// `None` when the install lacks them: the flat sea stays.
    pub fn new(install: &Install) -> Option<Self> {
        Some(Self {
            shown: Mutex::default(),
            surface: Mutex::default(),
            building: Mutex::default(),
            small: Arc::new(Small::read(install)?),
            perlin: Arc::new(perlin(install)?),
            inland: inland::Textures::read(install).map(Arc::new),
        })
    }

    /// Compiles the sea's pipelines before an island needs them: a surface
    /// made and let go, its pipelines kept for the next (`gpu::per_device`).
    pub fn warm(&self, gpu: &gpu::Warm<'_>) {
        drop(Surface::new(
            gpu.device,
            gpu.queue,
            gpu.frame_layout,
            &self.small,
            &self.perlin,
            self.inland.as_deref(),
        ));
    }
}

impl gpu::Pass for Water {
    fn stage(&self) -> Stage {
        Stage::Water
    }

    fn prepare(&self, frame: &mut gpu::Prepare<'_>) -> bool {
        let mut shown = self.shown.lock_any();
        shown.coast.land();
        shown.maps.land();
        let Some(sea) = shown.sea else {
            return false;
        };
        // The island's first coast still measuring: the sea joins with it,
        // rather than breaking against nothing for a few frames.
        if shown.coast.coast().is_none() && shown.coast.measuring() {
            return false;
        }
        let mut surface = self.surface.lock_any();
        if surface.get(frame.device).is_none() {
            // Compiling and uploading the waves took a large share of a
            // second on the first frame: built on a thread, the sea joins
            // a few frames later instead of the window stalling. Again on
            // a new device after a loss.
            let mut building = self.building.lock_any();
            match building.get(frame.device).map(|rx| rx.try_recv()) {
                Some(Ok(built)) => {
                    surface.set(frame.device, built);
                    building.clear();
                }
                Some(Err(std::sync::mpsc::TryRecvError::Empty)) => return false,
                _ => {
                    let (tx, rx) = std::sync::mpsc::sync_channel(1);
                    let (device, queue) = (frame.device.clone(), frame.queue.clone());
                    let layout = frame.frame_layout.clone();
                    let (small, perlin) = (self.small.clone(), self.perlin.clone());
                    let textures = self.inland.clone();
                    let spawned =
                        std::thread::Builder::new()
                            .name("1800 water".into())
                            .spawn(move || {
                                let surface = Surface::new(
                                    &device,
                                    &queue,
                                    &layout,
                                    &small,
                                    &perlin,
                                    textures.as_deref(),
                                );
                                let _ = tx.send(surface);
                            });
                    if spawned.is_ok() {
                        building.set(frame.device, rx);
                    }
                    return false;
                }
            }
        }
        let surface = surface.get_mut(frame.device).expect("built above");
        let seconds = shown.time.unwrap_or(frame.time);
        let (device, queue, focus) = (frame.device, frame.queue, frame.focus);
        frame.timed_work("water prepare", |encoder| {
            surface.prepare(device, queue, encoder, &sea, &shown, seconds, focus)
        })
    }

    fn record(&self, frame: &gpu::Record<'_>, pass: &mut wgpu::RenderPass<'_>) {
        if let Some(surface) = self.surface.lock_any().get(frame.device) {
            surface.draw(frame.device, pass, frame.scene);
        }
    }

    // The ring grid covers the sea to about 235 times the camera's height
    // (14.7 km at least; surface.wgsl vs_sea), and the sky's haze shows the
    // dome over open sea past 1250 units (sky/pass.wgsl fs_haze), so the
    // kit's far sea under it was a full-screen draw no pixel kept. Without
    // the haze (no preset shown) the sliver under the horizon past the
    // rings, a quarter of a degree, takes the backdrop's horizon colour
    // instead of the kit's sea.
    fn replaces(&self) -> bool {
        true
    }

    // Also while a new coast or maps are made, so they show once they land.
    fn animating(&self) -> bool {
        let shown = self.shown.lock_any();
        shown.coast.measuring()
            || shown.maps.reading()
            || (shown.sea.is_some() && shown.time.is_none())
    }
}

impl anno_core::water::Water for Water {
    fn show(
        &self,
        sea: Option<&Arc<dyn Sea>>,
        field: &Arc<Heightfield>,
        hold: bool,
        inland: Option<Arc<Inland>>,
        time: Option<f32>,
    ) {
        let mut shown = self.shown.lock_any();
        let Some(sea) = sea.and_then(|s| s.as_any().downcast_ref::<sea::Water>()) else {
            shown.sea = None;
            return;
        };
        shown.coast.want(field, hold);
        shown.sea = Some(*sea);
        shown.inland = inland;
        shown.time = time;
    }

    // Without the install's foam and noise they stay the kit's flat water.
    fn shades_inland(&self) -> bool {
        self.inland.is_some()
    }

    // Anno1800.exe gives each River record its own strip (0x140DA72A0
    // sends the path points, 0x141E3E3E0 builds "InlandWaterVB"), drawn at
    // the points' heights plus cInlandWaterHeightOffset.
    fn river_strips(&self) -> bool {
        true
    }

    fn island_maps(&self, grids: Option<&Arc<anno_formats::a7m::grids::Grids>>) {
        self.shown.lock_any().maps.want(grids);
    }

    fn detail(&self, detail: anno_core::ambient::Detail) {
        self.shown.lock_any().detail = detail;
    }
}

/// The waves on the GPU: the large ones' spectrum and transform, both
/// sets' gradient maps and mips, and the small ones' volume.
struct Waves {
    vertical: wgpu::ComputePipeline,
    horizontal: wgpu::ComputePipeline,
    large: wgpu::ComputePipeline,
    small: wgpu::ComputePipeline,
    mip: wgpu::ComputePipeline,
    step: wgpu::Buffer,
    h0: wgpu::Buffer,
    omega: wgpu::Buffer,
    /// Read back by the tests; the bind group keeps it alive otherwise.
    #[cfg_attr(not(test), allow(dead_code))]
    disp: wgpu::Buffer,
    /// Dx's, the height's and Dz's group (waves.wgsl `part`).
    groups: [wgpu::BindGroup; 3],
    /// The large gradients' mips 1.., then the small ones'.
    mips: Vec<wgpu::BindGroup>,
    /// The waves last stepped, those the spectrum on the GPU was built
    /// for, and the wave time last stepped to.
    spectrum: Option<sea::Water>,
    built_for: Option<sea::Water>,
    stepped: Option<f32>,
    /// A spectrum building on a thread, and the waves it is for.
    building: Option<(sea::Water, std::sync::mpsc::Receiver<Spectrum>)>,
    large_map: wgpu::TextureView,
    large_gradients: wgpu::TextureView,
    small_gradients: wgpu::TextureView,
    small_xz: wgpu::TextureView,
    small_y: wgpu::TextureView,
    wrap: wgpu::Sampler,
}

impl Waves {
    fn new(d: &wgpu::Device, queue: &wgpu::Queue, small: &Small) -> Self {
        use wgpu::BufferUsages as B;
        use wgpu::ShaderStages as S;
        let d2 = wgpu::TextureViewDimension::D2;
        let array = wgpu::TextureViewDimension::D2Array;
        let float = |filterable| wgpu::TextureSampleType::Float { filterable };
        let texture = |dim, sample_type| wgpu::BindingType::Texture {
            sample_type,
            view_dimension: dim,
            multisampled: false,
        };
        let written = wgpu::BindingType::StorageTexture {
            access: wgpu::StorageTextureAccess::WriteOnly,
            format: wgpu::TextureFormat::Rgba16Float,
            view_dimension: d2,
        };
        let buffer = |label, size: u64, usage| {
            d.create_buffer(&wgpu::BufferDescriptor {
                label: Some(label),
                size,
                usage,
                mapped_at_creation: false,
            })
        };
        let map = |label, mips| {
            d.create_texture(&wgpu::TextureDescriptor {
                label: Some(label),
                size: wgpu::Extent3d {
                    width: N,
                    height: N,
                    depth_or_array_layers: 1,
                },
                mip_level_count: mips,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Rgba16Float,
                usage: wgpu::TextureUsages::STORAGE_BINDING | wgpu::TextureUsages::TEXTURE_BINDING,
                view_formats: &[],
            })
        };
        let level = |t: &wgpu::Texture, mip: u32| {
            t.create_view(&wgpu::TextureViewDescriptor {
                base_mip_level: mip,
                mip_level_count: Some(1),
                ..Default::default()
            })
        };
        let large_map = map("1800 large waves", 1);
        let large_gradients = map("1800 large gradients", MIPS);
        let small_gradients = map("1800 small gradients", MIPS);
        let (small_xz, small_y) = upload_small(d, queue, small);
        let wrap = d.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("1800 waves"),
            address_mode_u: wgpu::AddressMode::Repeat,
            address_mode_v: wgpu::AddressMode::Repeat,
            address_mode_w: wgpu::AddressMode::Repeat,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            mipmap_filter: wgpu::MipmapFilterMode::Linear,
            ..Default::default()
        });
        let layout = d.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("1800 waves"),
            entries: &[
                layout_entry(0, S::COMPUTE, uniform()),
                layout_entry(1, S::COMPUTE, storage(true)),
                layout_entry(2, S::COMPUTE, storage(true)),
                layout_entry(3, S::COMPUTE, storage(false)),
                layout_entry(4, S::COMPUTE, storage(false)),
                layout_entry(5, S::COMPUTE, written),
                layout_entry(6, S::COMPUTE, written),
                layout_entry(7, S::COMPUTE, texture(array, float(true))),
                layout_entry(8, S::COMPUTE, texture(array, float(true))),
                layout_entry(
                    9,
                    S::COMPUTE,
                    wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                ),
                layout_entry(10, S::COMPUTE, written),
                layout_entry(11, S::COMPUTE, uniform()),
            ],
        });
        let mip_layout = d.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("1800 wave mips"),
            entries: &[
                layout_entry(0, S::COMPUTE, texture(d2, float(false))),
                layout_entry(1, S::COMPUTE, written),
            ],
        });
        let module = d.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("1800 waves"),
            source: wgpu::ShaderSource::Wgsl(
                [include_str!("water/waves.wgsl"), anno_core::water::FFT_WGSL]
                    .concat()
                    .into(),
            ),
        });
        let main = d.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("1800 waves"),
            bind_group_layouts: &[Some(&layout)],
            immediate_size: 0,
        });
        let mips_pipeline = d.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("1800 wave mips"),
            bind_group_layouts: &[None, Some(&mip_layout)],
            immediate_size: 0,
        });
        // Compiled once per device (`gpu::per_device`), as are the sea's.
        let compute = |layout: &wgpu::PipelineLayout, entry_point: &str, key| {
            gpu::per_device(d, key, || {
                d.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
                    label: Some(entry_point),
                    layout: Some(layout),
                    module: &module,
                    entry_point: Some(entry_point),
                    compilation_options: Default::default(),
                    cache: None,
                })
            })
        };
        let entries = ((N + 1) * W) as u64;
        let texels = u64::from(N * N);
        let step = buffer("1800 wave step", 16, B::UNIFORM | B::COPY_DST);
        let h0 = buffer("1800 h0", entries * 8, B::STORAGE | B::COPY_DST);
        let omega = buffer("1800 omega", entries * 4, B::STORAGE | B::COPY_DST);
        // One component at a time (waves.wgsl `temp`).
        let temp = buffer("1800 spectra", texels * 8, B::STORAGE);
        let disp = buffer(
            "1800 displacement",
            texels * 4 * 3,
            B::STORAGE | B::COPY_SRC,
        );
        let (lm, lg, sg) = (
            level(&large_map, 0),
            level(&large_gradients, 0),
            level(&small_gradients, 0),
        );
        let view = wgpu::BindingResource::TextureView;
        // One group per component the transform takes in turn (waves.wgsl
        // `part`).
        let groups = [0u32, 1, 2].map(|part| {
            use wgpu::util::DeviceExt as _;
            let which = d.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("1800 wave part"),
                contents: bytemuck::cast_slice(&[part, 0, 0, 0]),
                usage: B::UNIFORM,
            });
            d.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("1800 waves"),
                layout: &layout,
                entries: &[
                    entry(0, step.as_entire_binding()),
                    entry(1, h0.as_entire_binding()),
                    entry(2, omega.as_entire_binding()),
                    entry(3, temp.as_entire_binding()),
                    entry(4, disp.as_entire_binding()),
                    entry(5, view(&lm)),
                    entry(6, view(&lg)),
                    entry(7, view(&small_xz)),
                    entry(8, view(&small_y)),
                    entry(9, wgpu::BindingResource::Sampler(&wrap)),
                    entry(10, view(&sg)),
                    entry(11, which.as_entire_binding()),
                ],
            })
        });
        let mips = [&large_gradients, &small_gradients]
            .into_iter()
            .flat_map(|t| {
                (1..MIPS)
                    .map(|m| {
                        let (from, to) = (level(t, m - 1), level(t, m));
                        d.create_bind_group(&wgpu::BindGroupDescriptor {
                            label: Some("1800 wave mip"),
                            layout: &mip_layout,
                            entries: &[
                                entry(0, wgpu::BindingResource::TextureView(&from)),
                                entry(1, wgpu::BindingResource::TextureView(&to)),
                            ],
                        })
                    })
                    .collect::<Vec<_>>()
            })
            .collect();
        Self {
            vertical: compute(&main, "cs_vertical", "1800 waves vertical"),
            horizontal: compute(&main, "cs_horizontal", "1800 waves horizontal"),
            large: compute(&main, "cs_large", "1800 waves large"),
            small: compute(&main, "cs_small", "1800 waves small"),
            mip: compute(&mips_pipeline, "cs_mip", "1800 waves mip"),
            step,
            h0,
            omega,
            disp,
            groups,
            mips,
            spectrum: None,
            built_for: None,
            stepped: None,
            building: None,
            large_map: large_map.create_view(&Default::default()),
            large_gradients: large_gradients.create_view(&Default::default()),
            small_gradients: small_gradients.create_view(&Default::default()),
            small_xz,
            small_y,
            wrap,
        }
    }

    /// Steps both sets to `wave_time` (cWaveTime), rebuilding the large
    /// spectrum first when `sea`'s waves changed. A second call for the
    /// same time and sea (another view this frame) does nothing.
    /// Builds `sea`'s spectrum here and uploads it.
    fn set_spectrum(&mut self, queue: &wgpu::Queue, sea: &sea::Water) {
        self.upload_spectrum(queue, *sea, large_spectrum(sea));
    }

    fn upload_spectrum(&mut self, queue: &wgpu::Queue, sea: sea::Water, (h0, omega): Spectrum) {
        // Columns first (waves.wgsl `h0`): W entries a column as a row
        // had, past row N none.
        let w = W as usize;
        let at = |i: usize| (i % w) * w + i / w;
        let h0: Vec<[f32; 2]> = (0..h0.len())
            .map(|i| h0.get(at(i)).copied().unwrap_or_default())
            .collect();
        let omega: Vec<f32> = (0..omega.len())
            .map(|i| omega.get(at(i)).copied().unwrap_or_default())
            .collect();
        queue.write_buffer(&self.h0, 0, bytemuck::cast_slice(&h0));
        queue.write_buffer(&self.omega, 0, bytemuck::cast_slice(&omega));
        self.built_for = Some(sea);
        self.stepped = None;
    }

    /// Steps the waves to `wave_time`; false until a spectrum is in. A
    /// new spectrum builds on a thread, the old waves moving meanwhile:
    /// 512² random draws held an 1800 island's first frame 20-33 ms.
    fn step(
        &mut self,
        queue: &wgpu::Queue,
        encoder: &mut wgpu::CommandEncoder,
        sea: &sea::Water,
        wave_time: f32,
    ) -> bool {
        let waves = |s: &sea::Water| (s.amplitude, s.wind_speed, s.damping, s.seed, s.wind);
        if let Some((built, rx)) = &self.building
            && let Ok(spectrum) = rx.try_recv()
        {
            let built = *built;
            self.building = None;
            self.upload_spectrum(queue, built, spectrum);
        }
        let wanted = Some(waves(sea));
        if self.built_for.as_ref().map(waves) != wanted
            && self.building.as_ref().map(|(s, _)| waves(s)) != wanted
        {
            let (tx, rx) = std::sync::mpsc::sync_channel(1);
            let for_sea = *sea;
            let spawned = std::thread::Builder::new()
                .name("1800 waves".into())
                .spawn(move || {
                    let _ = tx.send(large_spectrum(&for_sea));
                });
            match spawned {
                Ok(_) => self.building = Some((*sea, rx)),
                Err(_) => self.set_spectrum(queue, sea),
            }
        }
        if self.built_for.is_none() {
            return false;
        }
        if self.spectrum.map(|s| s.choppiness) != Some(sea.choppiness) {
            self.stepped = None;
        }
        self.spectrum = Some(*sea);
        if self.stepped == Some(wave_time) {
            return true;
        }
        self.stepped = Some(wave_time);
        let grid_len = N as f32 / PATCH;
        queue.write_buffer(
            &self.step,
            0,
            bytemuck::cast_slice(&[wave_time, sea.choppiness, grid_len, 0.]),
        );
        let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
            label: Some("1800 waves"),
            timestamp_writes: None,
        });
        for group in &self.groups {
            pass.set_bind_group(0, group, &[]);
            pass.set_pipeline(&self.vertical);
            pass.dispatch_workgroups(N, 1, 1);
            pass.set_pipeline(&self.horizontal);
            pass.dispatch_workgroups(N, 1, 1);
        }
        pass.set_pipeline(&self.large);
        pass.dispatch_workgroups(N / 16, N / 16, 1);
        pass.set_pipeline(&self.small);
        pass.dispatch_workgroups(N / 16, N / 16, 1);
        pass.set_pipeline(&self.mip);
        for (i, group) in self.mips.iter().enumerate() {
            let side = N >> (i as u32 % (MIPS - 1) + 1);
            pass.set_bind_group(1, group, &[]);
            pass.dispatch_workgroups(side.div_ceil(8), side.div_ceil(8), 1);
        }
        true
    }
}

/// The surface: the waves and what draws them over the ring grid.
struct Surface {
    waves: Waves,
    layout: wgpu::BindGroupLayout,
    pipeline: wgpu::RenderPipeline,
    params: wgpu::Buffer,
    clamp: wgpu::Sampler,
    perlin: wgpu::TextureView,
    blank: wgpu::TextureView,
    coast: Option<(usize, wgpu::TextureView)>,
    /// cWaterFoamMap, cNoiseTex, cNoiseTex2 and cNoiseTex3D (blank
    /// without the install's).
    foam: wgpu::TextureView,
    noise: wgpu::TextureView,
    noise2: wgpu::TextureView,
    noise3d: wgpu::TextureView,
    /// cWaterInfoMap and cWaterFlowMap, by the maps they were made from.
    painted: Option<PaintedMaps>,
    physical: physical::Physical,
    inland: Option<inland::Pass>,
    /// What `prepare` was last given to draw: rivers and lakes or none.
    shows_inland: bool,
}

impl Surface {
    fn new(
        d: &wgpu::Device,
        queue: &wgpu::Queue,
        frame: &wgpu::BindGroupLayout,
        small: &Small,
        perlin: &[u16],
        textures: Option<&inland::Textures>,
    ) -> Self {
        use wgpu::ShaderStages as S;
        let d2 = wgpu::TextureViewDimension::D2;
        let array = wgpu::TextureViewDimension::D2Array;
        let float = wgpu::TextureSampleType::Float { filterable: true };
        let texture = |dim, sample_type| wgpu::BindingType::Texture {
            sample_type,
            view_dimension: dim,
            multisampled: false,
        };
        let filtering = wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering);
        let waves = Waves::new(d, queue, small);
        let perlin = upload_perlin(d, queue, perlin);
        let clamp = d.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("1800 coast"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        let both = S::VERTEX | S::FRAGMENT;
        let entries = [
            layout_entry(0, both, uniform()),
            layout_entry(1, both, texture(d2, float)),
            layout_entry(2, S::FRAGMENT, texture(d2, float)),
            layout_entry(3, S::FRAGMENT, texture(d2, float)),
            layout_entry(4, both, texture(array, float)),
            layout_entry(5, both, texture(array, float)),
            layout_entry(6, both, texture(d2, float)),
            layout_entry(7, both, texture(d2, float)),
            layout_entry(8, S::FRAGMENT, texture(d2, wgpu::TextureSampleType::Uint)),
            layout_entry(9, both, filtering),
            layout_entry(10, both, filtering),
            layout_entry(14, S::FRAGMENT, texture(d2, float)),
            layout_entry(15, S::FRAGMENT, texture(d2, float)),
            layout_entry(16, S::FRAGMENT, texture(d2, float)),
            layout_entry(17, both, texture(d2, float)),
            layout_entry(18, both, texture(d2, float)),
            layout_entry(
                20,
                S::FRAGMENT,
                texture(wgpu::TextureViewDimension::D3, float),
            ),
            layout_entry(23, both, texture(d2, float)),
            layout_entry(24, both, texture(d2, float)),
        ];
        let layout = d.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("1800 sea"),
            entries: &entries,
        });
        let shader = d.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("1800 sea"),
            source: wgpu::ShaderSource::Wgsl(
                gpu::ssr_module(
                    d,
                    &[
                        include_str!("water/surface.wgsl"),
                        include_str!("water/inland.wgsl"),
                    ],
                )
                .into(),
            ),
        });
        let inland = textures.map(|t| inland::Pass::new(d, queue, &shader, frame, &entries, t));
        let physical_shader = d.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("1800 physical waves"),
            source: wgpu::ShaderSource::Wgsl(
                [
                    gpu::FRAME_WGSL,
                    gpu::SSR_WGSL,
                    include_str!("water/surface.wgsl"),
                    include_str!("water/physical.wgsl"),
                ]
                .concat()
                .into(),
            ),
        });
        let physical = physical::Physical::new(d, &physical_shader, &entries);
        let pipeline_layout = d.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("1800 sea"),
            bind_group_layouts: &[Some(frame), Some(&layout)],
            immediate_size: 0,
        });
        // The driver compiled this anew at every island opened, 0.7 s with
        // the waves': once per device instead.
        let pipeline = gpu::per_device(d, "1800 sea", || {
            d.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some("1800 sea"),
                layout: Some(&pipeline_layout),
                vertex: wgpu::VertexState {
                    module: &shader,
                    entry_point: Some("vs_sea"),
                    buffers: &[],
                    compilation_options: Default::default(),
                },
                fragment: Some(wgpu::FragmentState {
                    module: &shader,
                    entry_point: Some("fs_sea"),
                    targets: &[Some(wgpu::ColorTargetState {
                        format: gpu::HDR_FORMAT,
                        blend: None,
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                    compilation_options: Default::default(),
                }),
                // One strip per band between rings (vs_sea).
                primitive: wgpu::PrimitiveState {
                    topology: wgpu::PrimitiveTopology::TriangleStrip,
                    ..Default::default()
                },
                // Opaque and depth-written, as 117's sea: what draws after it
                // meets the water where it lies.
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
            })
        });
        let params = d.create_buffer(&wgpu::BufferDescriptor {
            label: Some("1800 sea"),
            size: PARAMS as u64 * 16,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let blank = d
            .create_texture(&wgpu::TextureDescriptor {
                label: Some("1800 blank"),
                size: wgpu::Extent3d {
                    width: 1,
                    height: 1,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Rgba16Float,
                usage: wgpu::TextureUsages::TEXTURE_BINDING,
                view_formats: &[],
            })
            .create_view(&Default::default());
        let image = |label, mips: Option<&Vec<gpui_editor_kit::texture::Texture>>| {
            mips.map_or_else(|| blank.clone(), |m| inland::upload(d, queue, label, m))
        };
        let foam = image("1800 water foam", textures.map(|t| &t.foam));
        let noise = image("1800 noise", textures.map(|t| &t.noise));
        let noise2 = image("1800 noise 2", textures.map(|t| &t.noise2));
        let noise3d = upload_noise3d(d, queue, textures.map(|t| t.noise3d.as_slice()));
        Self {
            waves,
            layout,
            pipeline,
            params,
            clamp,
            perlin,
            blank,
            coast: None,
            foam,
            noise,
            noise2,
            noise3d,
            painted: None,
            physical,
            inland,
            shows_inland: false,
        }
    }

    /// Steps the waves of `sea` to `seconds` and writes the surface's
    /// uniforms, with what else `shown` holds: the coast and the rivers and
    /// lakes.
    fn prepare(
        &mut self,
        d: &wgpu::Device,
        queue: &wgpu::Queue,
        encoder: &mut wgpu::CommandEncoder,
        sea: &sea::Water,
        shown: &Shown,
        seconds: f32,
        focus: [f32; 2],
    ) -> bool {
        let coast = shown.coast.coast();
        self.shows_inland = false;
        if let (Some(pass), Some(inland)) = (&mut self.inland, &shown.inland) {
            pass.prepare(d, queue, inland);
            self.shows_inland = true;
        }
        let maps = shown.maps.maps();
        let painted_key = maps.map(|m| Arc::as_ptr(m) as usize);
        if self.painted.as_ref().map(|p| p.key) != painted_key {
            self.painted = maps.map(|m| PaintedMaps::upload(d, queue, m));
        }
        let coast_key = coast.map(|c| Arc::as_ptr(c) as usize);
        if self.coast.as_ref().map(|c| c.0) != coast_key {
            self.coast = coast.map(|c| (Arc::as_ptr(c) as usize, upload_coast(d, queue, c)));
        }
        let wave_time = seconds * WAVE_TIME_RATE;
        // The sea joins once its first spectrum is in.
        if !self.waves.step(queue, encoder, sea, wave_time) {
            return false;
        }
        let depth_scale = coast.map_or(0., |c| c.depth_scale);
        let coast4 = coast.map_or([0.; 4], |c| {
            [c.origin[0], c.origin[1], 1. / c.size[0], 1. / c.size[1]]
        });
        // cPerlinMovement as NVIDIA's sample moves it: against the wind,
        // PerlinSpeed per unit of wave time.
        let moved = wave_time * PERLIN_SPEED;
        let uniforms: [[f32; 4]; PARAMS] = [
            [sea.deep[0], sea.deep[1], sea.deep[2], sea.caustics],
            [
                sea.shallow[0],
                sea.shallow[1],
                sea.shallow[2],
                sea.absorption,
            ],
            [sea.view_depth, sea.light_depth, wave_time, seconds],
            coast4,
            // One FFT patch, PATCH units scaled to the world, spans 16
            // world units (the ships' wave lookup divides by both); half a
            // texel is the sample's offset, two texels the normal's run.
            [
                1. / (PATCH * DISPLACEMENT_SCALE),
                0.5 / N as f32,
                DISPLACEMENT_SCALE,
                PATCH / N as f32 * 2.,
            ],
            [
                PERLIN_AMPLITUDE[0],
                PERLIN_AMPLITUDE[1],
                PERLIN_AMPLITUDE[2],
                PERLIN_SIZE,
            ],
            [PERLIN_OCTAVE[0], PERLIN_OCTAVE[1], PERLIN_OCTAVE[2], 0.],
            [
                PERLIN_GRADIENT[0],
                PERLIN_GRADIENT[1],
                PERLIN_GRADIENT[2],
                0.,
            ],
            [
                -sea.wind[0] * moved,
                -sea.wind[1] * moved,
                BLEND[0],
                BLEND[1],
            ],
            // w: 1 leaves out the near detail (reduced wave detail).
            [
                SEGMENTS as f32,
                RINGS as f32,
                RING_GROWTH,
                if shown.detail.fine_waves { 0. } else { 1. },
            ],
            self.painted
                .as_ref()
                .map_or([0.; 4], |p| [p.scale[0], p.scale[1], 1., 0.]),
            if shown.detail.reflections {
                REFLECTIONS.params()
            } else {
                [0.; 4]
            },
            self.physical.uniforms(),
        ];
        queue.write_buffer(&self.params, 0, bytemuck::cast_slice(&uniforms));
        // The physical waves read the uniforms written above: queue writes
        // land before the encoder runs.
        let view = wgpu::BindingResource::TextureView;
        let waves = &self.waves;
        let coast = self.coast.as_ref().map_or(&self.blank, |c| &c.1);
        let infos = self.painted.as_ref().map_or(&self.blank, |p| &p.infos);
        let flow = self.painted.as_ref().map_or(&self.blank, |p| &p.flow);
        let sea_entries = [
            entry(0, self.params.as_entire_binding()),
            entry(1, view(&waves.large_map)),
            entry(2, view(&waves.large_gradients)),
            entry(3, view(&waves.small_gradients)),
            entry(4, view(&waves.small_xz)),
            entry(5, view(&waves.small_y)),
            entry(7, view(coast)),
            entry(9, wgpu::BindingResource::Sampler(&self.clamp)),
            entry(10, wgpu::BindingResource::Sampler(&waves.wrap)),
            entry(16, view(&self.noise2)),
            entry(17, view(infos)),
            entry(18, view(flow)),
        ];
        self.physical.step(
            d,
            queue,
            encoder,
            &sea_entries,
            focus,
            seconds,
            shown.time.is_none(),
            depth_scale,
        );
        // The window may have moved with that step.
        queue.write_buffer(
            &self.params,
            (PARAMS as u64 - 1) * 16,
            bytemuck::cast_slice(&self.physical.uniforms()),
        );
        true
    }

    fn draw(&self, d: &wgpu::Device, pass: &mut wgpu::RenderPass<'_>, scene: &wgpu::TextureView) {
        let view = wgpu::BindingResource::TextureView;
        let waves = &self.waves;
        let coast = self.coast.as_ref().map_or(&self.blank, |c| &c.1);
        let physical = self.physical.state();
        let entries = vec![
            entry(0, self.params.as_entire_binding()),
            entry(1, view(&waves.large_map)),
            entry(2, view(&waves.large_gradients)),
            entry(3, view(&waves.small_gradients)),
            entry(4, view(&waves.small_xz)),
            entry(5, view(&waves.small_y)),
            entry(6, view(&self.perlin)),
            entry(7, view(coast)),
            entry(8, view(scene)),
            entry(9, wgpu::BindingResource::Sampler(&self.clamp)),
            entry(10, wgpu::BindingResource::Sampler(&waves.wrap)),
            entry(14, view(&self.foam)),
            entry(15, view(&self.noise)),
            entry(16, view(&self.noise2)),
            entry(
                17,
                view(self.painted.as_ref().map_or(&self.blank, |p| &p.infos)),
            ),
            entry(
                18,
                view(self.painted.as_ref().map_or(&self.blank, |p| &p.flow)),
            ),
            entry(20, view(&self.noise3d)),
            entry(23, view(&physical)),
            entry(24, view(&self.physical.normals)),
        ];
        let group = d.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("1800 sea"),
            layout: &self.layout,
            entries: &entries,
        });
        pass.set_pipeline(&self.pipeline);
        pass.set_bind_group(1, &group, &[]);
        // Strips per band between rings (vs_sea), as 117's ocean draws them.
        pass.draw(0..(RINGS - 1) * (2 * SEGMENTS + 4) - 2, 0..1);
        if let (Some(inland), true) = (&self.inland, self.shows_inland) {
            inland.draw(d, pass, entries);
        }
    }
}

/// The small waves as two 48-layer arrays: block compressed as shipped
/// where the device takes BC (12.5 MB each), else decoded to half floats.
fn upload_small(
    d: &wgpu::Device,
    queue: &wgpu::Queue,
    small: &Small,
) -> (wgpu::TextureView, wgpu::TextureView) {
    let bc = bc_supported(d);
    let blocks = N / 4;
    let slice = (blocks * blocks * 16) as usize;
    let upload = |label, data: &[u8], channels: usize| {
        let format = if bc {
            wgpu::TextureFormat::Bc6hRgbFloat
        } else {
            wgpu::TextureFormat::Rgba16Float
        };
        let texture = d.create_texture(&wgpu::TextureDescriptor {
            label: Some(label),
            size: wgpu::Extent3d {
                width: N,
                height: N,
                depth_or_array_layers: FRAMES,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        for z in 0..FRAMES {
            let frame = &data[z as usize * slice..(z as usize + 1) * slice];
            let origin = wgpu::TexelCopyTextureInfo {
                texture: &texture,
                mip_level: 0,
                origin: wgpu::Origin3d { x: 0, y: 0, z },
                aspect: wgpu::TextureAspect::All,
            };
            let size = wgpu::Extent3d {
                width: N,
                height: N,
                depth_or_array_layers: 1,
            };
            if bc {
                let layout = wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(blocks * 16),
                    rows_per_image: Some(blocks),
                };
                queue.write_texture(origin, frame, layout, size);
            } else {
                let texels = decode_bc6h(frame, channels);
                let layout = wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(N * 8),
                    rows_per_image: Some(N),
                };
                queue.write_texture(origin, bytemuck::cast_slice(&texels), layout, size);
            }
        }
        texture.create_view(&wgpu::TextureViewDescriptor {
            dimension: Some(wgpu::TextureViewDimension::D2Array),
            ..Default::default()
        })
    };
    (
        upload("1800 small waves xz", &small.xz, 2),
        upload("1800 small waves y", &small.y, 1),
    )
}

/// One signed BC6H frame as half-float RGBA, its first `channels` kept.
fn decode_bc6h(frame: &[u8], channels: usize) -> Vec<[u16; 4]> {
    let blocks = (N / 4) as usize;
    let mut out = vec![[0u16; 4]; (N * N) as usize];
    for (b, block) in frame.as_chunks::<16>().0.iter().enumerate() {
        let mut texels = [0f32; 48];
        bcdec_rs::bc6h_float(block, &mut texels, 12, true);
        let (bx, by) = (b % blocks * 4, b / blocks * 4);
        for t in 0..16 {
            let mut texel = [0u16; 4];
            for c in 0..channels {
                texel[c] = half(texels[t * 3 + c]);
            }
            out[(by + t / 4) * N as usize + bx + t % 4] = texel;
        }
    }
    out
}

fn upload_perlin(d: &wgpu::Device, queue: &wgpu::Queue, texels: &[u16]) -> wgpu::TextureView {
    let texture = d.create_texture(&wgpu::TextureDescriptor {
        label: Some("1800 perlin"),
        size: wgpu::Extent3d {
            width: 64,
            height: 64,
            depth_or_array_layers: 1,
        },
        mip_level_count: PERLIN_MIPS,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba16Float,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    let mut at = 0;
    for mip in 0..PERLIN_MIPS {
        let side = 64 >> mip;
        let count = (side * side * 4) as usize;
        queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &texture,
                mip_level: mip,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            bytemuck::cast_slice(&texels[at..at + count]),
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(side * 8),
                rows_per_image: None,
            },
            wgpu::Extent3d {
                width: side,
                height: side,
                depth_or_array_layers: 1,
            },
        );
        at += count;
    }
    texture.create_view(&Default::default())
}

fn upload_coast(d: &wgpu::Device, queue: &wgpu::Queue, coast: &Coast) -> wgpu::TextureView {
    let size = wgpu::Extent3d {
        width: coast.width.max(1),
        height: coast.height.max(1),
        depth_or_array_layers: 1,
    };
    let texture = d.create_texture(&wgpu::TextureDescriptor {
        label: Some("1800 coast"),
        size,
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba16Float,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    let texels: Vec<u16> = coast.texels.iter().flat_map(|t| t.map(half)).collect();
    queue.write_texture(
        texture.as_image_copy(),
        bytemuck::cast_slice(&texels),
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(coast.width * 8),
            rows_per_image: None,
        },
        size,
    );
    texture.create_view(&Default::default())
}

/// The island's painted water maps on the GPU, as RGBA8.
struct PaintedMaps {
    key: usize,
    infos: wgpu::TextureView,
    flow: wgpu::TextureView,
    /// 1 / the maps' extent in metres (cWaterInfoCoord).
    scale: [f32; 2],
}

impl PaintedMaps {
    fn upload(d: &wgpu::Device, queue: &wgpu::Queue, m: &Arc<IslandMaps>) -> Self {
        use wgpu::util::DeviceExt;
        let map = |label, texels: &[[u8; 4]]| {
            d.create_texture_with_data(
                queue,
                &wgpu::TextureDescriptor {
                    label: Some(label),
                    size: wgpu::Extent3d {
                        width: m.width,
                        height: m.height,
                        depth_or_array_layers: 1,
                    },
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: wgpu::TextureDimension::D2,
                    format: wgpu::TextureFormat::Rgba8Unorm,
                    usage: wgpu::TextureUsages::TEXTURE_BINDING,
                    view_formats: &[],
                },
                wgpu::util::TextureDataOrder::LayerMajor,
                bytemuck::cast_slice(texels),
            )
            .create_view(&Default::default())
        };
        Self {
            key: Arc::as_ptr(m) as usize,
            infos: map("1800 water infos", &m.info),
            flow: map("1800 water flow", &m.flow),
            scale: m.scale(),
        }
    }
}

/// cNoiseTex3D: the 64³ volume, mid grey without the install's.
fn upload_noise3d(
    d: &wgpu::Device,
    queue: &wgpu::Queue,
    texels: Option<&[u8]>,
) -> wgpu::TextureView {
    let side = inland::NOISE_3D;
    let size = wgpu::Extent3d {
        width: side,
        height: side,
        depth_or_array_layers: side,
    };
    let texture = d.create_texture(&wgpu::TextureDescriptor {
        label: Some("1800 noise 3d"),
        size,
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D3,
        format: wgpu::TextureFormat::R8Unorm,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    let grey = vec![128u8; side.pow(3) as usize];
    queue.write_texture(
        texture.as_image_copy(),
        texels.unwrap_or(&grey),
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(side),
            rows_per_image: Some(side),
        },
        size,
    );
    texture.create_view(&Default::default())
}

fn entry(binding: u32, resource: wgpu::BindingResource<'_>) -> wgpu::BindGroupEntry<'_> {
    wgpu::BindGroupEntry { binding, resource }
}

fn layout_entry(
    binding: u32,
    visibility: wgpu::ShaderStages,
    ty: wgpu::BindingType,
) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility,
        ty,
        count: None,
    }
}

fn uniform() -> wgpu::BindingType {
    wgpu::BindingType::Buffer {
        ty: wgpu::BufferBindingType::Uniform,
        has_dynamic_offset: false,
        min_binding_size: None,
    }
}

fn storage(read_only: bool) -> wgpu::BindingType {
    wgpu::BindingType::Buffer {
        ty: wgpu::BufferBindingType::Storage { read_only },
        has_dynamic_offset: false,
        min_binding_size: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui_editor_kit::camera::{Camera, Extent};
    use gpui_editor_kit::gpu::{
        Batch, Instances, Light, Material, Mesh, MeshData, Offscreen, Renderer, Scene, Tone,
    };
    use gpui_editor_kit::math::{self, Vec3};

    fn moderate() -> sea::Water {
        sea::Water {
            amplitude: 0.25,
            choppiness: 1.6,
            wind_speed: 800.,
            damping: 0.5,
            seed: 13,
            view_depth: 12.,
            light_depth: 10.,
            absorption: 3.5,
            caustics: 1.,
            deep: [0.054902, 0.160784, 0.270588],
            shallow: [0., 0.77, 0.82],
            wind: [1., 0.],
        }
    }

    /// Flat small waves and Perlin noise: only the large waves move.
    fn flat() -> (Arc<Small>, Arc<Vec<u16>>) {
        let bytes = (N as usize / 4).pow(2) * 16 * FRAMES as usize;
        let small = Small {
            xz: vec![0; bytes],
            y: vec![0; bytes],
        };
        let texels: usize = (0..PERLIN_MIPS).map(|m| (64usize >> m).pow(2)).sum();
        (Arc::new(small), Arc::new(vec![0; texels * 4]))
    }

    impl Water {
        /// Shows `sea` at `time` seconds, with flat small waves and no
        /// coast, without an install.
        fn bare(sea: sea::Water, time: f32) -> Self {
            let (small, perlin) = flat();
            let water = Self {
                shown: Mutex::default(),
                surface: Mutex::default(),
                building: Mutex::default(),
                small,
                perlin,
                inland: None,
            };
            let mut shown = water.shown.lock_any();
            shown.sea = Some(sea);
            shown.time = Some(time);
            drop(shown);
            water
        }
    }

    impl Waves {
        fn read_displacement(&self, d: &wgpu::Device, queue: &wgpu::Queue) -> Vec<f32> {
            let size = self.disp.size();
            let readback = d.create_buffer(&wgpu::BufferDescriptor {
                label: Some("1800 readback"),
                size,
                usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            let mut encoder = d.create_command_encoder(&Default::default());
            encoder.copy_buffer_to_buffer(&self.disp, 0, &readback, 0, size);
            queue.submit([encoder.finish()]);
            readback.slice(..).map_async(wgpu::MapMode::Read, |_| {});
            d.poll(wgpu::PollType::wait_indefinitely()).unwrap();
            let mapped = readback.slice(..).get_mapped_range().expect("mapped");
            bytemuck::cast_slice::<u8, f32>(&mapped).to_vec()
        }
    }

    // The spectrum has no constant term, waves against the wind keep the
    // preset's damping of those along it, short waves fall away, and
    // twice the amplitude is √2 times every wave.
    #[test]
    fn the_large_spectrum_follows_the_wind() {
        let sea = moderate();
        let k = 2. * std::f32::consts::PI / PATCH * 20.;
        let (down, up) = (phillips([k, 0.], &sea), phillips([-k, 0.], &sea));
        assert!(
            down > 0. && (up / down - sea.damping).abs() < 1e-5,
            "{down} {up}"
        );
        assert_eq!(phillips([0., k], &sea), 0., "across the wind");
        assert_eq!(phillips([0., 0.], &sea), 0.);
        // Twelve times shorter: the spectrum's 1/k⁴ leaves under 1/10000.
        let short = 2. * std::f32::consts::PI / PATCH * 250.;
        assert!(phillips([short, 0.], &sea) < down * 1e-4);
        let (h0, omega) = large_spectrum(&sea);
        let at = |c: u32, r: u32| (r * W + c) as usize;
        let mid = N / 2;
        assert_eq!(h0[at(mid, mid)], [0., 0.]);
        assert_eq!(omega[at(mid, mid)], 0.);
        let twice = large_spectrum(&sea::Water {
            amplitude: 0.5,
            ..sea
        })
        .0;
        let (a, b) = (h0[at(mid + 3, mid + 1)], twice[at(mid + 3, mid + 1)]);
        assert!((b[0] / a[0] - 2f32.sqrt()).abs() < 1e-4, "{a:?} {b:?}");
    }

    // The GPU's transform is the spectrum's 2D DFT at time t, the
    // centred spectrum's sign corrected, as csOceanFFTWrapUp keeps it.
    #[test]
    fn the_gpu_transform_is_the_games_dft() {
        let target = Offscreen::new(4, 4).unwrap();
        let (d, queue) = (target.device(), target.queue());
        let (small, _) = flat();
        let mut waves = Waves::new(d, queue, &small);
        let sea = moderate();
        let seconds = 7.;
        let mut encoder = d.create_command_encoder(&Default::default());
        waves.set_spectrum(queue, &sea);
        assert!(waves.step(queue, &mut encoder, &sea, seconds * WAVE_TIME_RATE));
        queue.submit([encoder.finish()]);
        let got = waves.read_displacement(d, queue);
        let (h0, omega) = large_spectrum(&sea);
        let n = N as usize;
        let t = f64::from(seconds * WAVE_TIME_RATE) / 0.13;
        let ht = |x: usize, y: usize| -> [f64; 2] {
            let k = h0[y * W as usize + x].map(f64::from);
            let mk = h0[(n - y) * W as usize + (n - x)].map(f64::from);
            let (s, c) = (f64::from(omega[y * W as usize + x]) * t).sin_cos();
            [
                (k[0] + mk[0]) * c - (k[1] + mk[1]) * s,
                (k[0] - mk[0]) * s + (k[1] - mk[1]) * c,
            ]
        };
        let spectrum: Vec<[f64; 2]> = (0..n * n).map(|p| ht(p % n, p / n)).collect();
        let peak = got[n * n..2 * n * n]
            .iter()
            .fold(0f32, |m, v| m.max(v.abs()));
        assert!(peak > 1., "the large waves stand, {peak}");
        for (x, y) in [(0usize, 0usize), (5, 9), (300, 77), (511, 511), (31, 400)] {
            let mut re = 0f64;
            for (p, h) in spectrum.iter().enumerate() {
                let (i, j) = (p % n, p / n);
                let angle = -2. * std::f64::consts::PI * ((i * x + j * y) % n) as f64 / n as f64;
                re += h[0] * angle.cos() - h[1] * angle.sin();
            }
            let sign = if (x + y) % 2 == 1 { -1. } else { 1. };
            let want = re * sign;
            let have = f64::from(got[n * n + y * n + x]);
            assert!(
                (have - want).abs() <= 1e-3 * peak as f64 + 1e-3,
                "({x}, {y}): {have} vs {want}"
            );
        }
    }

    // Over a sloping seabed the sea reads its deep color where the floor
    // lies deep and lighter where it comes up; the large waves move with
    // time, and a preset without amplitude leaves the sea calmer.
    #[test]
    fn the_sea_tints_by_depth_moves_and_calms() {
        let mut target = Offscreen::new(320, 240).unwrap();
        // Sloping from 0.2 units under water at -x to 8 under at +x.
        let floor = MeshData {
            positions: vec![
                [-40., -0.2, -40.],
                [40., -8., -40.],
                [40., -8., 40.],
                [-40., -0.2, 40.],
            ],
            normals: vec![[0., 1., 0.]; 4],
            colors: vec![0xd8d0b8; 4],
            indices: vec![0, 1, 2, 0, 2, 3],
            material: Material::Lit,
            ..Default::default()
        };
        let floor = Batch::new(
            Arc::new(Mesh::new(floor).unwrap()),
            Arc::new(Instances {
                matrices: vec![math::IDENTITY],
                ids: vec![Some(1)],
            }),
        );
        let renderer = Renderer::default();
        let light = Light {
            sun: [0.3, 1., 0.2],
            sun_color: [3.; 3],
            sky: [0.8, 0.9, 1.1],
            ground: [0.3; 3],
            ..Default::default()
        };
        let scene = |sea: sea::Water, time: f32| Scene {
            batches: vec![floor.clone()],
            light,
            tone: Some(Tone::default()),
            passes: vec![Arc::new(Water::bare(sea, time))],
            ..Default::default()
        };
        let world = Extent {
            center: [0.; 3],
            size: 60.,
        };
        let camera = Camera::default();
        let bounds = target.bounds();
        let at = |pixels: &[u8], p: Vec3| {
            let s = camera.project(p, bounds, world);
            let (x, y) = (f32::from(s.x) as usize, f32::from(s.y) as usize);
            let i = (y * 320 + x) * 4;
            [pixels[i], pixels[i + 1], pixels[i + 2]]
        };
        renderer.set_scene(&scene(moderate(), 0.));
        let first = target.render(&renderer, camera, world).unwrap();
        let (shallow, deep) = (at(&first, [-30., 0., 0.]), at(&first, [30., 0., 0.]));
        assert!(deep[2] > deep[0], "deep water is blue {deep:?}");
        let luma = |c: [u8; 3]| u32::from(c[0]) + u32::from(c[1]) + u32::from(c[2]);
        assert!(
            luma(shallow) > luma(deep) + 30,
            "shallow {shallow:?}, deep {deep:?}"
        );
        renderer.set_scene(&scene(moderate(), 3.));
        let later = target.render(&renderer, camera, world).unwrap();
        let changed = |a: &[u8], b: &[u8]| {
            a.chunks(4)
                .zip(b.chunks(4))
                .filter(|(a, b)| a.iter().zip(b.iter()).any(|(x, y)| x.abs_diff(*y) > 3))
                .count()
        };
        let moved = changed(&first, &later);
        assert!(moved > 500, "{moved} pixels moved");
        let calm = sea::Water {
            amplitude: 0.,
            ..moderate()
        };
        renderer.set_scene(&scene(calm, 0.));
        let still = target.render(&renderer, camera, world).unwrap();
        renderer.set_scene(&scene(calm, 3.));
        let still_later = target.render(&renderer, camera, world).unwrap();
        let calm_moved = changed(&still, &still_later);
        assert!(calm_moved * 4 < moved, "calm {calm_moved}, waves {moved}");
    }

    // Looking just under the horizon through a narrow lens, past where the
    // rings end: the kit's far sea (green) doesn't draw there once the sea
    // is ready, so the backdrop fills it with its horizon colour (red),
    // and nearer in the sea itself draws.
    #[test]
    fn the_sea_draws_instead_of_the_kits_far_sea() {
        use gpui_editor_kit::gpu::{Sea, Sky};
        let (w, h) = (240usize, 240usize);
        let mut target = Offscreen::new(w as u32, h as u32).unwrap();
        let renderer = Renderer::default();
        renderer.set_scene(&Scene {
            light: Light {
                sun: [0.3, 1., 0.2],
                sun_color: [1.; 3],
                sky: [0.5; 3],
                ground: [0.2; 3],
                ..Default::default()
            },
            sky: Some(Sky {
                zenith: [0., 0., 1.],
                horizon: [1., 0., 0.],
                // Grazing, the kit's sea is 98% the sky it reflects; a body
                // this bright still shows through as green.
                sea: Some(Sea {
                    level: 0.,
                    color: [0., 40., 0.],
                }),
                radiance: None,
                sun_disc: [0.; 3],
                ambient: None,
            }),
            passes: vec![Arc::new(Water::bare(moderate(), 0.))],
            ..Default::default()
        });
        // The eye ~130 units up and the view 0.006 under level: the last
        // ring, 235 eye heights out, sits 0.0043 under the horizon. At
        // 0.05 rad over 240 rows the horizon is row ~91 and the rings end
        // at row ~112.
        let camera = Camera {
            tilt: std::f32::consts::FRAC_PI_2 - 0.006,
            fov: 0.05,
            ..Camera::default()
        };
        let world = Extent {
            center: [0.; 3],
            size: 1000.,
        };
        let pixels = target.render(&renderer, camera, world).unwrap();
        let at = |row: usize| {
            let i = (row * w + w / 2) * 4;
            [pixels[i], pixels[i + 1], pixels[i + 2]]
        };
        for row in [96, 101, 106] {
            assert_eq!(at(row), [255, 0, 0], "row {row}, past the rings");
        }
        let near = at(h - 2);
        assert_ne!(near, [255, 0, 0], "the sea under the camera");
    }

    // A river with a red record beside a lake with a blue one, over a
    // sunken floor: each takes its record's colour, the river's foam moves
    // along its flow, and the lake, whose small waves are flat here, holds
    // still.
    #[test]
    fn rivers_and_lakes_take_their_records_and_the_river_flows() {
        use anno_core::water::{Corner, Look};
        let mut target = Offscreen::new(320, 240).unwrap();
        let floor = MeshData {
            positions: vec![
                [-40., -1., -40.],
                [40., -1., -40.],
                [40., -1., 40.],
                [-40., -1., 40.],
            ],
            normals: vec![[0., 1., 0.]; 4],
            colors: vec![0xd8d0b8; 4],
            indices: vec![0, 1, 2, 0, 2, 3],
            material: Material::Lit,
            ..Default::default()
        };
        let floor = Batch::new(
            Arc::new(Mesh::new(floor).unwrap()),
            Arc::new(Instances {
                matrices: vec![math::IDENTITY],
                ids: vec![Some(1)],
            }),
        );
        let (w, d) = (61u32, 41u32);
        let corners = (0..w * d)
            .map(|i| {
                let x = -30. + (i % w) as f32;
                if x < 0. {
                    Corner {
                        y: 0.5,
                        look: 0,
                        flow: [1., 0.],
                        speed: 8.,
                        fade: 1.,
                        flow_fade: 1.,
                    }
                } else {
                    Corner {
                        y: 0.5,
                        look: 1,
                        flow: [0., 0.],
                        speed: 0.,
                        fade: 1.,
                        flow_fade: 1.,
                    }
                }
            })
            .collect();
        let record = |lake: bool, argb: u32| Look {
            lake,
            fields: vec![
                ("WaterColor".into(), vec![f32::from_bits(argb)]),
                ("Turbidity".into(), vec![3.]),
            ],
        };
        let inland = Arc::new(Inland::new(
            [-30., -20.],
            1.,
            [w, d],
            corners,
            vec![record(false, 0xffc0_2020), record(true, 0xff20_40c0)],
            None,
        ));
        let renderer = Renderer::default();
        let light = Light {
            sun: [0.3, 1., 0.2],
            sun_color: [3.; 3],
            sky: [0.8, 0.9, 1.1],
            ground: [0.3; 3],
            ..Default::default()
        };
        let calm = sea::Water {
            amplitude: 0.,
            ..moderate()
        };
        let scene = |time: f32| {
            let water = Water::bare(calm, time);
            let water = Water {
                inland: Some(Arc::new(inland::Textures::striped())),
                ..water
            };
            water.shown.lock_any().inland = Some(inland.clone());
            Scene {
                batches: vec![floor.clone()],
                light,
                tone: Some(Tone::default()),
                passes: vec![Arc::new(water)],
                ..Default::default()
            }
        };
        let world = Extent {
            center: [0.; 3],
            size: 60.,
        };
        let camera = Camera::default();
        let bounds = target.bounds();
        let px = |p: Vec3| {
            let s = camera.project(p, bounds, world);
            (f32::from(s.x) as usize, f32::from(s.y) as usize)
        };
        let at = |pixels: &[u8], p: Vec3| {
            let (x, y) = px(p);
            let i = (y * 320 + x) * 4;
            [pixels[i], pixels[i + 1], pixels[i + 2]]
        };
        renderer.set_scene(&scene(0.));
        let first = target.render(&renderer, camera, world).unwrap();
        let (river, lake) = (at(&first, [-15., 0.5, 0.]), at(&first, [15., 0.5, 0.]));
        assert!(river[0] > river[2], "the river is red {river:?}");
        assert!(lake[2] > lake[0], "the lake is blue {lake:?}");
        renderer.set_scene(&scene(0.7));
        let later = target.render(&renderer, camera, world).unwrap();
        // Pixels that changed over the river's half and the lake's.
        let changed = |lo: f32, hi: f32| {
            let (x0, _) = px([lo, 0.5, 0.]);
            let (x1, _) = px([hi, 0.5, 0.]);
            let (_, y0) = px([0., 0.5, -10.]);
            let (_, y1) = px([0., 0.5, 10.]);
            let (x0, x1) = (x0.min(x1), x0.max(x1));
            let (y0, y1) = (y0.min(y1), y0.max(y1));
            let mut n = 0;
            for y in y0..y1 {
                for x in x0..x1 {
                    let i = (y * 320 + x) * 4;
                    if (0..3).any(|k| first[i + k].abs_diff(later[i + k]) > 3) {
                        n += 1;
                    }
                }
            }
            n
        };
        let (river_moved, lake_moved) = (changed(-25., -5.), changed(5., 25.));
        assert!(river_moved > 200, "{river_moved} river pixels moved");
        assert!(
            lake_moved * 10 < river_moved,
            "lake {lake_moved}, river {river_moved}"
        );
    }

    // The shipped textures read: both small-wave volumes and the noise.
    #[test]
    fn the_installs_wave_textures_read() {
        let Some(dir) = anno_formats::vfs::install_dir("ANNO1800_DIR") else {
            return;
        };
        let install = Install::open(dir, crate::ARCHIVE_FAMILIES).unwrap();
        let small = Small::read(&install).expect("small waves");
        assert_eq!(
            small.xz.len(),
            (N as usize / 4).pow(2) * 16 * FRAMES as usize
        );
        let noise = perlin(&install).expect("perlin noise");
        assert!(noise.iter().any(|&h| h != 0));
        // The volume's first frame decodes to finite heights, not all
        // zero.
        let frame = decode_bc6h(&small.y[..(N as usize / 4).pow(2) * 16], 1);
        let finite = |h: u16| (h >> 10) & 0x1f != 0x1f;
        assert!(frame.iter().all(|t| finite(t[0])));
        assert!(frame.iter().any(|t| t[0] & 0x7fff != 0));
    }
}
