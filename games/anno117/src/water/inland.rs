// Owns rivers and lakes as Anno 117 draws them (water_inland_water.fx
// psRiver/psLake/psRiverLava): the island's water lattice
// (core::water::Inland), its rivers shaded with flowing waves and foam from
// a flow map, its lakes with the ocean's smallest wave band, and rivers
// whose record flags them lava as glowing rock. Drawn after the sea and
// sharing its bindings (water::ocean), so it needs a sea.
// Engine addresses here are Anno117.exe build 26607411 (Steam).
use super::{Gpu, ocean};
use super::{entry, key, layout_entry, storage, uniform};
use anno_core::water::{Inland, Look};
use anyhow::{Result, ensure};
use std::borrow::Cow;

/// `waves_small_y.dds`: 48 frames of a looping ripple height field, as the
/// game ships it. The GPU keeps the blocks and takes the slopes
/// csComputeRiverGradients takes of the two frames a frame samples between
/// (`Pass::prepare`), so sampling them is what the game's per-frame pass
/// computes. Every frame's slopes kept as half floats took 48 MB.
#[derive(Debug)]
pub struct RiverWaves {
    pub size: [u32; 3],
    /// BC6H blocks, frame by frame, row by row.
    blocks: Vec<u8>,
    signed: bool,
}

impl RiverWaves {
    /// Reads the game's file: a BC6H (signed) volume.
    pub fn decode(dds: &[u8]) -> Result<Self> {
        ensure!(dds.len() >= 148 && &dds[..4] == b"DDS ", "not a DDS file");
        let u32_at = |at: usize| u32::from_le_bytes(dds[at..at + 4].try_into().unwrap());
        let (height, width, depth) = (u32_at(12), u32_at(16), u32_at(24).max(1));
        ensure!(&dds[84..88] == b"DX10", "river waves: no DX10 header");
        let format = u32_at(128);
        ensure!(
            format == 95 || format == 96,
            "river waves: DXGI format {format}, expected BC6H"
        );
        let signed = format == 96;
        // Whole blocks, so the GPU takes them as they are.
        ensure!(
            width % 4 == 0 && height % 4 == 0,
            "river waves: {width}x{height} is not whole blocks"
        );
        let bytes = (width / 4 * height / 4 * 16 * depth) as usize;
        let data = &dds[148..];
        ensure!(data.len() >= bytes, "river waves: file too short");
        Ok(Self {
            size: [width, height, depth],
            blocks: data[..bytes].to_vec(),
            signed,
        })
    }

    /// Every frame's heights as half floats, frame by frame, row by row:
    /// what devices without BC take.
    pub fn heights(&self) -> Vec<u16> {
        let [w, h, depth] = self.size.map(|v| v as usize);
        let bw = w / 4;
        let mut heights = vec![0u16; w * h * depth];
        for (b, block) in self.blocks.as_chunks::<16>().0.iter().enumerate() {
            let mut texels = [0f32; 48];
            bcdec_rs::bc6h_float(block, &mut texels, 12, self.signed);
            let (z, b) = (b / (bw * h / 4), b % (bw * h / 4));
            let (bx, by) = (b % bw * 4, b / bw * 4);
            for t in 0..16 {
                let (x, y) = (bx + t % 4, by + t / 4);
                heights[(z * h + y) * w + x] = super::half(texels[t * 3]);
            }
        }
        heights
    }
}

/// csComputeRiverGradients: a frame's slopes, central differences over
/// two texels, over 8, wrapping, into the slopes slice drawn.
const SLOPES_WGSL: &str = "
@group(0) @binding(0) var heights: texture_2d_array<f32>;

struct Slope {
    @builtin(position) clip: vec4<f32>,
    @location(0) @interpolate(flat) frame: u32,
}

@vertex fn vs_slopes(@builtin(vertex_index) i: u32, @builtin(instance_index) frame: u32) -> Slope {
    let uv = vec2<f32>(f32((i << 1u) & 2u), f32(i & 2u));
    return Slope(vec4<f32>(uv * 2.0 - 1.0, 0.0, 1.0), frame);
}

fn height(p: vec2<i32>, frame: u32) -> f32 {
    let size = vec2<i32>(textureDimensions(heights));
    return textureLoad(heights, (p + size) % size, frame, 0).r;
}

@fragment fn fs_slopes(in: Slope) -> @location(0) vec2<f32> {
    let p = vec2<i32>(in.clip.xy);
    let dx = -(height(p + vec2<i32>(1, 0), in.frame) - height(p - vec2<i32>(1, 0), in.frame)) / 8.0;
    let dy = (height(p + vec2<i32>(0, 1), in.frame) - height(p - vec2<i32>(0, 1), in.frame)) / 8.0;
    return vec2<f32>(dx, dy);
}
";

/// The river ripples on a device: the game's height frames, and the
/// slopes of the two a frame samples between, which fs_river's volume
/// filters between as it filtered between all 48.
struct Waves {
    key: usize,
    heights: wgpu::BindGroup,
    /// The slopes, two slices of a volume.
    view: wgpu::TextureView,
    /// The frames `slopes` holds, first slice and second.
    frames: Option<[u32; 2]>,
}

impl Waves {
    fn new(gpu: &Gpu, layout: &wgpu::BindGroupLayout, waves: &RiverWaves, key: usize) -> Self {
        use wgpu::util::DeviceExt;
        let [w, h, depth] = waves.size;
        let bc = gpui_editor_kit::gpu::bc_supported(&gpu.device);
        let heights = if bc {
            let format = if waves.signed {
                wgpu::TextureFormat::Bc6hRgbFloat
            } else {
                wgpu::TextureFormat::Bc6hRgbUfloat
            };
            (format, waves.blocks.clone())
        } else {
            let texels = waves.heights();
            (
                wgpu::TextureFormat::R16Float,
                bytemuck::cast_slice(&texels).to_vec(),
            )
        };
        let frames = gpu.device.create_texture_with_data(
            &gpu.queue,
            &wgpu::TextureDescriptor {
                label: Some("river waves"),
                size: wgpu::Extent3d {
                    width: w,
                    height: h,
                    depth_or_array_layers: depth,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: heights.0,
                usage: wgpu::TextureUsages::TEXTURE_BINDING,
                view_formats: &[],
            },
            wgpu::util::TextureDataOrder::LayerMajor,
            &heights.1,
        );
        let heights = gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("river slopes"),
            layout,
            entries: &[entry(
                0,
                wgpu::BindingResource::TextureView(&frames.create_view(
                    &wgpu::TextureViewDescriptor {
                        dimension: Some(wgpu::TextureViewDimension::D2Array),
                        ..Default::default()
                    },
                )),
            )],
        });
        let slopes = gpu.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("river slopes"),
            size: wgpu::Extent3d {
                width: w,
                height: h,
                depth_or_array_layers: 2,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D3,
            format: wgpu::TextureFormat::Rg16Float,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        });
        Self {
            key,
            heights,
            view: slopes.create_view(&Default::default()),
            frames: None,
        }
    }
}

/// A `River` or `Lake` record's look as 117 shades it. Depths are world
/// units. Fields a record leaves out take the roman pool rivers' values,
/// lakes' own where they differ. TODO: the engine's defaults for missing
/// fields (rivers never store ScatterStrength) are not known.
struct Record {
    lake: bool,
    /// RenderFlags' IWF_LAVA: drawn with LavaRiverDefault, not as water.
    lava: bool,
    /// LavaFlowOffset under -9998: lava that flows while the volcano
    /// sleeps, its crust black (psRiverLava's permanentLava).
    permanent: bool,
    shallow: [f32; 3],
    deep: [f32; 3],
    scatter: [f32; 3],
    scatter_strength: f32,
    absorption_shallow: f32,
    shallow_depth: f32,
    view_depth: f32,
    absorption_smoothness: f32,
    wave_intensity: f32,
    /// Shallower than this along a bank, fast water foams.
    max_foam_depth: f32,
    /// cIWWaveSize and cIWWaveSpeed: the ripples' scale and how fast the
    /// flow carries them.
    wave_size: f32,
    wave_speed: f32,
    /// cIWPhysicalWaveIntensity: how much the shore's physical waves lean
    /// the surface and foam on it.
    physical: f32,
    /// cIWLarge/Medium/SmallWaveWeight; only Egypt's shader reads them.
    weights: [f32; 3],
}

impl Record {
    /// `look` as stored; `global` is the session's look (`Seasons::at`, or
    /// `SeasonLook::DEFAULT` outside Egypt), which a USE_GLOBAL_SHADING
    /// record takes instead of its own.
    fn read(look: &Look, global: &SeasonLook) -> Self {
        let lake = look.lake;
        let number = |name: &str, default: f32| look.number(name).unwrap_or(default);
        // Both are integers the record's floats carry bit for bit.
        let flags = look.number("RenderFlags").map_or(0, f32::to_bits);
        let offset = look
            .number("LavaFlowOffset")
            .map_or(0, |v| v.to_bits() as i32);
        let color = |name: &str, default: [f32; 3]| match look.field(name) {
            Some([r, g, b, ..]) => [*r, *g, *b],
            _ => default,
        };
        let lava = !lake && flags & LAVA != 0;
        let permanent = offset < -9998;
        // Not part of the global look: the game keeps the record's.
        let max_foam_depth = number("MaxFoamDepth", 0.1);
        if flags & USE_GLOBAL_SHADING != 0 {
            let g = global;
            return Self {
                lake,
                lava,
                permanent,
                shallow: g.shallow,
                deep: g.deep,
                scatter: g.scatter,
                scatter_strength: g.scatter_strength,
                absorption_shallow: g.absorption_shallow,
                shallow_depth: g.shallow_depth,
                view_depth: g.view_depth,
                absorption_smoothness: g.absorption_smoothness,
                wave_intensity: g.wave_intensity,
                max_foam_depth,
                wave_size: g.wave_size,
                wave_speed: g.wave_speed,
                physical: g.physical,
                weights: g.weights,
            };
        }
        // What the game's draw sets for every record of its own look
        // (Anno117.exe 0x142515B34).
        Self {
            lake,
            lava,
            permanent,
            shallow: color("ColorShallow", [0.847, 0.839, 0.796]),
            deep: color("ColorDeep", [0.365, 0.522, 0.831]),
            scatter: color("ScatterColor", [0.278, 1., 0.941]),
            scatter_strength: number("ScatterStrength", 0.1),
            absorption_shallow: number("AbsorptionShallow", if lake { 0.3 } else { 0.9 }),
            shallow_depth: number("ShallowDepth", if lake { 0.4 } else { 0.2 }),
            view_depth: number("ViewDepth", if lake { 10. } else { 8. }),
            absorption_smoothness: number("AbsorptionSmoothness", 0.6),
            wave_intensity: number("WaveIntensity", if lake { 0.7 } else { 1.2 }),
            max_foam_depth,
            wave_size: 1.,
            wave_speed: 1.,
            physical: 1.,
            weights: [0., 1., 0.],
        }
    }
}

/// RenderFlags' IWF_LAVA bit (functions_inland_water.fxh).
const LAVA: u32 = 4;
/// RenderFlags' IWF_USE_GLOBAL_SHADING: the water ignores its record's
/// colours, depths, scatter and waves and takes the session's global look
/// instead (Anno117.exe 0x142515B34 swaps them in per draw; MaxFoamDepth
/// and LavaFlowOffset stay the record's). Only two Egypt rivers set it, on
/// egypt_dlc_03_valley_large_05 and egypt_island_quadrant_01_x1_y1.
const USE_GLOBAL_SHADING: u32 = 0x20;

/// One of globals.xml's `DLC03_InlandWater_DrySeason` and `FloodSeason`
/// blocks: the look USE_GLOBAL_SHADING water takes in Egypt.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SeasonLook {
    pub wave_size: f32,
    pub wave_speed: f32,
    pub wave_intensity: f32,
    pub shallow_depth: f32,
    pub absorption_shallow: f32,
    pub shallow: [f32; 3],
    pub deep: [f32; 3],
    pub scatter: [f32; 3],
    pub scatter_strength: f32,
    pub view_depth: f32,
    pub absorption_smoothness: f32,
    pub physical: f32,
    /// Large, medium and small wave weights.
    pub weights: [f32; 3],
}

impl SeasonLook {
    /// The engine's own look (Anno117.exe 0x142516B20): what a block leaves
    /// out, and what USE_GLOBAL_SHADING water takes outside Egypt.
    pub const DEFAULT: Self = Self {
        wave_size: 1.,
        wave_speed: 1.,
        wave_intensity: 0.7,
        shallow_depth: 1.,
        absorption_shallow: 1.,
        shallow: [1.; 3],
        deep: [1.; 3],
        scatter: [1.; 3],
        scatter_strength: 0.1,
        view_depth: 10.,
        absorption_smoothness: 1.,
        physical: 1.,
        weights: [0.3, 0.6, 0.1],
    };

    /// The block opened by `<name>` in `xml`, as the game's serialiser
    /// (0x142516350) reads it.
    fn read(xml: &str, name: &str) -> Option<Self> {
        use anno_formats::xml::tag;
        let at = xml.find(&format!("<{name}>"))?;
        // Up to the next season's block, so a field the block leaves out
        // isn't taken from the one after it.
        let block = &xml[at + name.len() + 2..];
        let block = &block[..block.find("<DLC03_").unwrap_or(block.len())];
        let d = Self::DEFAULT;
        let number = |n: &str, default: f32| {
            tag(block, n)
                .and_then(|v| v.parse::<f32>().ok())
                .unwrap_or(default)
        };
        let color = |n: &str, default: [f32; 3]| {
            let Some(at) = block.find(&format!("<{n}>")) else {
                return default;
            };
            let rest = &block[at..];
            let axis = |a: &str| tag(rest, a).and_then(|v| v.parse::<f32>().ok());
            match (axis("x"), axis("y"), axis("z")) {
                (Some(x), Some(y), Some(z)) => [x, y, z],
                _ => default,
            }
        };
        Some(Self {
            wave_size: number("WaveSize", d.wave_size),
            wave_speed: number("WaveSpeed", d.wave_speed),
            wave_intensity: number("WaveIntensity", d.wave_intensity),
            shallow_depth: number("ShallowDepth", d.shallow_depth),
            absorption_shallow: number("AbsorptionShallow", d.absorption_shallow),
            shallow: color("ColorShallow", d.shallow),
            deep: color("ColorDeep", d.deep),
            scatter: color("ScatterColor", d.scatter),
            scatter_strength: number("ScatterStrength", d.scatter_strength),
            view_depth: number("ViewDepth", d.view_depth),
            absorption_smoothness: number("AbsorptionSmoothness", d.absorption_smoothness),
            physical: number("PhysicalWaveIntensity", d.physical),
            weights: [
                number("LargeWaveWeight", d.weights[0]),
                number("MediumWaveWeight", d.weights[1]),
                number("SmallWaveWeight", d.weights[2]),
            ],
        })
    }
}

/// Egypt's inland water settings from globals.xml: the dry and flood
/// season looks, the physical waves' strength halfway between them, and how
/// much wind gusts stir small waves on every river there.
#[derive(Clone, Debug, PartialEq)]
pub struct Seasons {
    pub dry: SeasonLook,
    pub flood: SeasonLook,
    pub transition_physical: f32,
    pub gust_boost: f32,
    /// `DLC03_FlowMap_FileName`: the painted flow map every river in
    /// Egypt's session flows by instead of its computed one.
    pub flow_map: Option<String>,
}

impl Seasons {
    /// globals.xml's DLC03_InlandWater blocks; `None` without the dry one.
    pub fn read(xml: &str) -> Option<Self> {
        use anno_formats::xml::tag;
        let number = |n: &str| tag(xml, n).and_then(|v| v.parse::<f32>().ok());
        Some(Self {
            dry: SeasonLook::read(xml, "DLC03_InlandWater_DrySeason")?,
            flood: SeasonLook::read(xml, "DLC03_InlandWater_FloodSeason")
                .unwrap_or(SeasonLook::DEFAULT),
            transition_physical: number("DLC03_InlandWater_TransitionPhysWaveIntensity")
                .unwrap_or(0.),
            gust_boost: number("DLC03_InlandWater_SmallWaveWindGustBoost").unwrap_or(0.),
            flow_map: tag(xml, "DLC03_FlowMap_FileName")
                .filter(|p| !p.is_empty())
                .map(str::to_owned),
        })
    }

    /// The session as it stands `flood` of the way from the dry season (0)
    /// to the flood (1), whose `RiverHeight`s are `river_heights` (world
    /// units every River object rises, its CurrentRiverLevelOffset): the
    /// look USE_GLOBAL_SHADING water takes and how high the rivers stand.
    /// As CSeasonManager::UpdateSeasonalRiverHeight (Anno117.exe
    /// 0x142CD3540): the rivers' height moves straight from one season's to
    /// the other's, and the look follows it as that height's share of the
    /// highest any season raises them (MaxRiverHeight), so the dry season
    /// shows a fiftieth of the flood.
    pub fn state(&self, flood: f32, river_heights: [f32; 2]) -> (SeasonLook, f32) {
        let [dry, wet] = river_heights;
        let height = dry + (wet - dry) * flood.clamp(0., 1.);
        let max = dry.max(wet);
        let share = if max > 0. {
            (height / max).clamp(0., 1.)
        } else {
            0.
        };
        (self.at(share), height)
    }

    /// The look `flood` of the way from the dry season (0) to the flood (1),
    /// as Anno117.exe 0x1425167A0 blends it: every field straight across
    /// but the ripples' size and speed, which stay the dry season's, and the
    /// physical waves, which peak at `transition_physical` halfway.
    pub fn at(&self, flood: f32) -> SeasonLook {
        let t = flood.clamp(0., 1.);
        let (a, b) = (&self.dry, &self.flood);
        let mix = |x: f32, y: f32| (1. - t) * x + t * y;
        let mix3 = |x: [f32; 3], y: [f32; 3]| [0, 1, 2].map(|i| mix(x[i], y[i]));
        let away = (2. * t - 1.).abs();
        let physical = (1. - away) * self.transition_physical
            + if t < 0.5 { away * a.physical } else { 0. }
            + if t > 0.5 { away * b.physical } else { 0. };
        SeasonLook {
            wave_size: a.wave_size,
            wave_speed: a.wave_speed,
            wave_intensity: mix(a.wave_intensity, b.wave_intensity),
            shallow_depth: mix(a.shallow_depth, b.shallow_depth),
            absorption_shallow: mix(a.absorption_shallow, b.absorption_shallow),
            shallow: mix3(a.shallow, b.shallow),
            deep: mix3(a.deep, b.deep),
            scatter: mix3(a.scatter, b.scatter),
            scatter_strength: mix(a.scatter_strength, b.scatter_strength),
            view_depth: mix(a.view_depth, b.view_depth),
            absorption_smoothness: mix(a.absorption_smoothness, b.absorption_smoothness),
            physical,
            weights: mix3(a.weights, b.weights),
        }
    }
}

/// Each look's uniforms (fs_river's InlandLook): absorption worked out as
/// the ocean's is.
fn look_uniforms(inland: &Inland, global: &SeasonLook) -> Vec<[[f32; 4]; 7]> {
    inland
        .looks
        .iter()
        .map(|l| {
            let l = Record::read(l, global);
            let (shallow, deep, scatter) = ocean::absorption_terms(
                l.shallow,
                l.deep,
                l.scatter,
                l.absorption_shallow,
                l.view_depth,
                l.scatter_strength,
            );
            [
                [shallow[0], shallow[1], shallow[2], l.shallow_depth],
                [deep[0], deep[1], deep[2], l.absorption_smoothness],
                [scatter[0], scatter[1], scatter[2], l.view_depth],
                [
                    l.shallow[0],
                    l.shallow[1],
                    l.shallow[2],
                    if l.lake { 1. } else { 0. },
                ],
                [
                    l.wave_intensity,
                    l.max_foam_depth,
                    if l.permanent { 1. } else { 0. },
                    0.,
                ],
                [l.wave_size, l.wave_speed, l.physical, 0.],
                [l.weights[0], l.weights[1], l.weights[2], 0.],
            ]
        })
        .collect()
}

/// The inland water pass's GPU state: its pipelines (lakes, rivers, lava),
/// the lattice uploaded, and the river waves.
pub(crate) struct Pass {
    layout: wgpu::BindGroupLayout,
    lakes: wgpu::RenderPipeline,
    rivers: wgpu::RenderPipeline,
    lava: wgpu::RenderPipeline,
    params: wgpu::Buffer,
    wrap3: wgpu::Sampler,
    flat: wgpu::TextureView,
    lattice: Option<Lattice>,
    waves: Option<Waves>,
    /// SLOPES_WGSL's layout and pipeline.
    slopes: (wgpu::BindGroupLayout, wgpu::RenderPipeline),
    rain: Option<(usize, wgpu::TextureView)>,
    /// Egypt's gust textures (cWindTex, cPerlinNoiseTex), by `sea::Gusts`.
    gusts: Option<(usize, [wgpu::TextureView; 2])>,
    /// Egypt's painted flow map (cInlandWaterClusterFlowTex from a file).
    flow_file: Option<(usize, std::sync::Arc<gpui_editor_kit::gpu::StreamedTexture>)>,
    /// 1x1 stand-in for missing duckweed.
    blank: wgpu::TextureView,
}

/// Indices run lakes, then water rivers from `split`; lava cells are left
/// out, the lava drawn from its own strips.
struct Lattice {
    key: usize,
    corners: wgpu::Buffer,
    looks: wgpu::Buffer,
    indices: wgpu::Buffer,
    split: u32,
    count: u32,
    /// The session look the looks were written with: a new season rewrites
    /// them in place.
    global: SeasonLook,
    /// Lava strips' vertices, indices and index count.
    lava: Option<(wgpu::Buffer, wgpu::Buffer, u32)>,
}

impl Pass {
    /// `shared` are the ocean surface's layout entries, bindings 0 to 7.
    pub fn new(
        gpu: &Gpu,
        shader: &wgpu::ShaderModule,
        frame: &wgpu::BindGroupLayout,
        shared: &[wgpu::BindGroupLayoutEntry],
        format: wgpu::TextureFormat,
    ) -> Self {
        use wgpu::ShaderStages as S;
        let d = &gpu.device;
        let both = S::VERTEX | S::FRAGMENT;
        let texture = |dim| wgpu::BindingType::Texture {
            sample_type: wgpu::TextureSampleType::Float { filterable: true },
            view_dimension: dim,
            multisampled: false,
        };
        let mut entries = shared.to_vec();
        entries.extend([
            layout_entry(8, both, uniform(false)),
            layout_entry(9, both, storage(true)),
            layout_entry(10, both, storage(true)),
            layout_entry(
                11,
                S::FRAGMENT,
                wgpu::BindingType::Texture {
                    sample_type: wgpu::TextureSampleType::Float { filterable: true },
                    view_dimension: wgpu::TextureViewDimension::D3,
                    multisampled: false,
                },
            ),
            layout_entry(
                12,
                S::FRAGMENT,
                wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
            ),
            layout_entry(19, S::FRAGMENT, texture(wgpu::TextureViewDimension::D2)),
            layout_entry(20, S::FRAGMENT, texture(wgpu::TextureViewDimension::D2)),
            layout_entry(21, S::FRAGMENT, texture(wgpu::TextureViewDimension::D3)),
            layout_entry(22, S::FRAGMENT, texture(wgpu::TextureViewDimension::D2)),
            layout_entry(23, S::FRAGMENT, texture(wgpu::TextureViewDimension::D2)),
            layout_entry(24, S::FRAGMENT, texture(wgpu::TextureViewDimension::D2)),
        ]);
        let layout = d.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("inland water"),
            entries: &entries,
        });
        let pipeline_layout = d.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("inland water"),
            bind_group_layouts: &[Some(frame), Some(&layout)],
            immediate_size: 0,
        });
        let pipeline = |vertex: &str,
                        buffers: &[Option<wgpu::VertexBufferLayout>],
                        fragment: &str,
                        blend: Option<wgpu::BlendState>| {
            d.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some(fragment),
                layout: Some(&pipeline_layout),
                vertex: wgpu::VertexState {
                    module: shader,
                    entry_point: Some(vertex),
                    buffers,
                    compilation_options: Default::default(),
                },
                fragment: Some(wgpu::FragmentState {
                    module: shader,
                    entry_point: Some(fragment),
                    targets: &[Some(wgpu::ColorTargetState {
                        format,
                        blend,
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                    compilation_options: Default::default(),
                }),
                primitive: wgpu::PrimitiveState::default(),
                // As the game's sbInlandWater: tested and written, so what
                // is blended later meets the water.
                depth_stencil: Some(wgpu::DepthStencilState {
                    format: wgpu::TextureFormat::Depth32Float,
                    depth_write_enabled: Some(true),
                    depth_compare: Some(wgpu::CompareFunction::GreaterEqual),
                    stencil: Default::default(),
                    bias: Default::default(),
                }),
                multisample: Default::default(),
                multiview_mask: None,
                cache: None,
            })
        };
        let lakes = pipeline("vs_inland", &[], "fs_lake", None);
        // psRiver's alpha fades a river's ends over what lies below (the
        // sea at a mouth), keeping the target's alpha. Lava blends the same
        // way (sbInlandWater), opaque but for its faded ends.
        let ends = wgpu::BlendState {
            color: wgpu::BlendComponent {
                src_factor: wgpu::BlendFactor::SrcAlpha,
                dst_factor: wgpu::BlendFactor::OneMinusSrcAlpha,
                operation: wgpu::BlendOperation::Add,
            },
            alpha: wgpu::BlendComponent {
                src_factor: wgpu::BlendFactor::Zero,
                dst_factor: wgpu::BlendFactor::One,
                operation: wgpu::BlendOperation::Add,
            },
        };
        let rivers = pipeline("vs_inland", &[], "fs_river", Some(ends));
        // core::water::StripVertex as vs_lava's LavaVertex reads it.
        let strip = wgpu::VertexBufferLayout {
            array_stride: std::mem::size_of::<anno_core::water::StripVertex>() as u64,
            step_mode: wgpu::VertexStepMode::Vertex,
            attributes: &wgpu::vertex_attr_array![
                0 => Float32x3,
                1 => Uint32,
                2 => Float32x3,
                3 => Float32,
                4 => Float32,
            ],
        };
        let lava = pipeline("vs_lava", &[Some(strip)], "fs_lava", Some(ends));
        let params = d.create_buffer(&wgpu::BufferDescriptor {
            label: Some("inland water"),
            size: 6 * 16,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let wrap3 = d.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("river waves"),
            address_mode_u: wgpu::AddressMode::Repeat,
            address_mode_v: wgpu::AddressMode::Repeat,
            address_mode_w: wgpu::AddressMode::Repeat,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        let flat = volume(gpu, [1, 1, 1], &[[0, 0]]);
        let blank = d
            .create_texture(&wgpu::TextureDescriptor {
                label: Some("inland blank"),
                size: wgpu::Extent3d {
                    width: 1,
                    height: 1,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Rgba8Unorm,
                usage: wgpu::TextureUsages::TEXTURE_BINDING,
                view_formats: &[],
            })
            .create_view(&Default::default());
        let slopes = {
            let module = d.create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some("river slopes"),
                source: wgpu::ShaderSource::Wgsl(SLOPES_WGSL.into()),
            });
            let layout = d.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("river slopes"),
                entries: &[layout_entry(
                    0,
                    S::FRAGMENT,
                    wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: false },
                        view_dimension: wgpu::TextureViewDimension::D2Array,
                        multisampled: false,
                    },
                )],
            });
            let pipeline = d.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some("river slopes"),
                layout: Some(&d.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                    label: Some("river slopes"),
                    bind_group_layouts: &[Some(&layout)],
                    immediate_size: 0,
                })),
                vertex: wgpu::VertexState {
                    module: &module,
                    entry_point: Some("vs_slopes"),
                    buffers: &[],
                    compilation_options: Default::default(),
                },
                fragment: Some(wgpu::FragmentState {
                    module: &module,
                    entry_point: Some("fs_slopes"),
                    targets: &[Some(wgpu::TextureFormat::Rg16Float.into())],
                    compilation_options: Default::default(),
                }),
                primitive: wgpu::PrimitiveState::default(),
                depth_stencil: None,
                multisample: Default::default(),
                multiview_mask: None,
                cache: None,
            });
            (layout, pipeline)
        };
        Self {
            layout,
            lakes,
            rivers,
            lava,
            params,
            wrap3,
            flat,
            lattice: None,
            waves: None,
            slopes,
            rain: None,
            gusts: None,
            flow_file: None,
            blank,
        }
    }

    /// Uploads what changed since the last frame and the time, and takes
    /// the slopes of the ripple frames it falls between.
    pub fn prepare(
        &mut self,
        gpu: &Gpu,
        encoder: &mut wgpu::CommandEncoder,
        shown: &ocean::InlandShown<'_>,
        time: f32,
    ) {
        let (inland, waves) = (shown.inland, shown.waves);
        let rain_key = shown.rain.map_or(0, key);
        if self.rain.as_ref().map(|r| r.0) != Some(rain_key) {
            self.rain = shown.rain.map(|r| (rain_key, ripples(gpu, r)));
        }
        let k = key(inland);
        if self.lattice.as_ref().map(|l| l.key) != Some(k) {
            self.lattice = upload(gpu, inland, k, &shown.global);
        } else if let Some(l) = &mut self.lattice
            && l.global != shown.global
        {
            let looks = look_uniforms(inland, &shown.global);
            gpu.queue
                .write_buffer(&l.looks, 0, bytemuck::cast_slice(&looks));
            l.global = shown.global;
        }
        ocean::streamed(
            gpu,
            &mut self.flow_file,
            shown.flow_file.map(|f| f.0),
            gpui_editor_kit::gpu::bc_supported(&gpu.device),
            "egypt flow map",
        );
        let gusts_key = shown.gusts.map_or(0, key);
        if self.gusts.as_ref().map(|g| g.0) != Some(gusts_key) {
            self.gusts = shown.gusts.map(|g| (gusts_key, gust_textures(gpu, g)));
        }
        let waves_key = waves.map_or(0, key);
        if self.waves.as_ref().map(|w| w.key) != Some(waves_key) {
            self.waves = waves.map(|w| Waves::new(gpu, &self.slopes.0, w, waves_key));
        }
        // fs_river's place between the two frames: the frame the game
        // loops through at 0.05 a second (computed as the shader did, in
        // f32), between the slices' centres of a two-slice volume.
        let mut between = 0.;
        if let (Some(w), Some(source)) = (&mut self.waves, waves) {
            let count = source.size[2] as f32;
            let at = (0.05 * time).fract() * count - 0.5;
            let first = at.floor();
            let frames = [first, first + 1.].map(|f| f.rem_euclid(count) as u32);
            if w.frames != Some(frames) {
                for (slice, frame) in frames.into_iter().enumerate() {
                    let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                        label: Some("river slopes"),
                        color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                            view: &w.view,
                            resolve_target: None,
                            ops: wgpu::Operations {
                                load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                                store: wgpu::StoreOp::Store,
                            },
                            depth_slice: Some(slice as u32),
                        })],
                        depth_stencil_attachment: None,
                        timestamp_writes: None,
                        occlusion_query_set: None,
                        multiview_mask: None,
                    });
                    pass.set_pipeline(&self.slopes.1);
                    pass.set_bind_group(0, &w.heights, &[]);
                    pass.draw(0..3, frame..frame + 1);
                }
                w.frames = Some(frames);
            }
            between = 0.25 + 0.5 * (at - first);
        }
        let params = [
            [
                inland.origin[0],
                inland.origin[1],
                inland.step,
                1. / inland.step.max(1e-6),
            ],
            [
                inland.size[0] as f32,
                inland.size[1] as f32,
                time,
                if waves.is_some() { 1. } else { 0. },
            ],
            [
                if self.rain.is_some() {
                    shown.raining
                } else {
                    0.
                },
                if shown.duckweed.is_some() { 1. } else { 0. },
                between,
                0.,
            ],
            // Egypt's shader (EGYPT_INLAND_WATER), its gust boost and the
            // wind the gusts drift with.
            [
                if shown.egypt.is_some() { 1. } else { 0. },
                shown.egypt.unwrap_or(0.),
                shown.wind[0],
                shown.wind[1],
            ],
            [shown.river_lift, 0., 0., 0.],
            // Egypt's flow map: on, the island's corner on it, the
            // session's side it spans.
            match shown.flow_file {
                Some((_, corner, size)) if self.flow_file.is_some() => {
                    [1., corner[0], corner[1], size[0]]
                }
                _ => [0.; 4],
            },
        ];
        gpu.queue
            .write_buffer(&self.params, 0, bytemuck::cast_slice(&params));
    }

    /// Draws lakes, then rivers, then lava; `shared` are the ocean's bind
    /// group entries 0 to 7.
    pub fn draw(
        &self,
        gpu: &Gpu,
        pass: &mut wgpu::RenderPass<'_>,
        shared: Vec<wgpu::BindGroupEntry<'_>>,
        duckweed: Option<[&wgpu::TextureView; 2]>,
    ) {
        let Some(lattice) = &self.lattice else {
            return;
        };
        let waves = self.waves.as_ref().map_or(&self.flat, |w| &w.view);
        let flow_file = self.flow_file.as_ref().map(|f| f.1.view());
        let mut entries = shared;
        entries.extend([
            entry(8, self.params.as_entire_binding()),
            entry(9, lattice.corners.as_entire_binding()),
            entry(10, lattice.looks.as_entire_binding()),
            entry(11, wgpu::BindingResource::TextureView(waves)),
            entry(12, wgpu::BindingResource::Sampler(&self.wrap3)),
            entry(19, view(duckweed.map_or(&self.blank, |d| d[0]))),
            entry(20, view(duckweed.map_or(&self.blank, |d| d[1]))),
            entry(21, view(self.rain.as_ref().map_or(&self.flat, |r| &r.1))),
            entry(
                22,
                view(self.gusts.as_ref().map_or(&self.blank, |g| &g.1[0])),
            ),
            entry(
                23,
                view(self.gusts.as_ref().map_or(&self.blank, |g| &g.1[1])),
            ),
            entry(24, view(flow_file.as_ref().unwrap_or(&self.blank))),
        ]);
        let group = gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("inland water"),
            layout: &self.layout,
            entries: &entries,
        });
        pass.set_bind_group(1, &group, &[]);
        pass.set_index_buffer(lattice.indices.slice(..), wgpu::IndexFormat::Uint32);
        if lattice.split > 0 {
            pass.set_pipeline(&self.lakes);
            pass.draw_indexed(0..lattice.split, 0, 0..1);
        }
        if lattice.count > lattice.split {
            pass.set_pipeline(&self.rivers);
            pass.draw_indexed(lattice.split..lattice.count, 0, 0..1);
        }
        if let Some((vertices, indices, count)) = &lattice.lava {
            pass.set_pipeline(&self.lava);
            pass.set_vertex_buffer(0, vertices.slice(..));
            pass.set_index_buffer(indices.slice(..), wgpu::IndexFormat::Uint32);
            pass.draw_indexed(0..*count, 0, 0..1);
        }
    }
}

fn upload(gpu: &Gpu, inland: &Inland, key: usize, global: &SeasonLook) -> Option<Lattice> {
    use wgpu::util::DeviceExt;
    let (indices, split) = inland.triangles();
    let strips = &inland.strips;
    if (indices.is_empty() && strips.indices.is_empty()) || inland.looks.is_empty() {
        return None;
    }
    let indices = without_lava(inland, indices, split);
    let buffer = |label, contents: &[u8], usage| {
        gpu.device
            .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some(label),
                contents,
                usage,
            })
    };
    let storage = wgpu::BufferUsages::STORAGE;
    Some(Lattice {
        key,
        corners: buffer(
            "inland corners",
            bytemuck::cast_slice(&inland.gpu_corners()),
            storage,
        ),
        looks: buffer(
            "inland looks",
            bytemuck::cast_slice(&look_uniforms(inland, global)),
            storage | wgpu::BufferUsages::COPY_DST,
        ),
        // A buffer can't be empty: one unused triangle where all is lava.
        indices: buffer(
            "inland indices",
            bytemuck::cast_slice(if indices.is_empty() {
                &[0u32; 3][..]
            } else {
                &indices
            }),
            wgpu::BufferUsages::INDEX,
        ),
        split,
        count: indices.len() as u32,
        global: *global,
        lava: (!strips.indices.is_empty()).then(|| {
            (
                buffer(
                    "lava strips",
                    bytemuck::cast_slice(&strips.vertices),
                    wgpu::BufferUsages::VERTEX,
                ),
                buffer(
                    "lava strip indices",
                    bytemuck::cast_slice(&strips.indices),
                    wgpu::BufferUsages::INDEX,
                ),
                strips.indices.len() as u32,
            )
        }),
    })
}

/// `indices` without the river triangles whose look is lava: lava flows on
/// the lattice's flow map but draws from its strips (`Inland::strips`). A
/// triangle takes its first corner's look. Islands without lava (all but
/// one shipped) keep their indices as they are.
fn without_lava<'a>(inland: &Inland, indices: &'a [u32], split: u32) -> Cow<'a, [u32]> {
    let mut lava = vec![0u64; inland.looks.len().div_ceil(64)];
    for (k, look) in inland.looks.iter().enumerate() {
        if Record::read(look, &SeasonLook::DEFAULT).lava {
            lava[k / 64] |= 1 << (k % 64);
        }
    }
    if lava.iter().all(|&w| w == 0) {
        return Cow::Borrowed(indices);
    }
    let is_lava = |t: &[u32]| {
        let look = inland.corners[t[0] as usize].look as usize;
        lava.get(look / 64)
            .is_some_and(|w| w >> (look % 64) & 1 != 0)
    };
    let (lakes, rivers) = indices.split_at(split as usize);
    let mut kept = Vec::with_capacity(indices.len());
    kept.extend_from_slice(lakes);
    kept.extend(rivers.chunks_exact(3).filter(|t| !is_lava(t)).flatten());
    Cow::Owned(kept)
}

fn view(v: &wgpu::TextureView) -> wgpu::BindingResource<'_> {
    wgpu::BindingResource::TextureView(v)
}

/// cWindTex (one byte a texel) and cPerlinNoiseTex's red channel (half
/// floats), as Egypt's river gusts sample them.
fn gust_textures(gpu: &Gpu, g: &crate::sea::Gusts) -> [wgpu::TextureView; 2] {
    use wgpu::util::DeviceExt;
    let plane = |label, size: [u32; 2], format, bytes: &[u8]| {
        gpu.device
            .create_texture_with_data(
                &gpu.queue,
                &wgpu::TextureDescriptor {
                    label: Some(label),
                    size: wgpu::Extent3d {
                        width: size[0],
                        height: size[1],
                        depth_or_array_layers: 1,
                    },
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: wgpu::TextureDimension::D2,
                    format,
                    usage: wgpu::TextureUsages::TEXTURE_BINDING,
                    view_formats: &[],
                },
                wgpu::util::TextureDataOrder::LayerMajor,
                bytes,
            )
            .create_view(&Default::default())
    };
    [
        plane(
            "wind gusts",
            g.wind_size,
            wgpu::TextureFormat::R8Unorm,
            &g.wind,
        ),
        plane(
            "perlin noise",
            g.perlin_size,
            wgpu::TextureFormat::R16Float,
            bytemuck::cast_slice(&g.perlin),
        ),
    ]
}

/// Rain's ripple volume, one byte a texel.
fn ripples(gpu: &Gpu, r: &crate::sea::Ripples) -> wgpu::TextureView {
    use wgpu::util::DeviceExt;
    gpu.device
        .create_texture_with_data(
            &gpu.queue,
            &wgpu::TextureDescriptor {
                label: Some("rain ripples"),
                size: wgpu::Extent3d {
                    width: r.size[0],
                    height: r.size[1],
                    depth_or_array_layers: r.size[2],
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D3,
                format: wgpu::TextureFormat::R8Unorm,
                usage: wgpu::TextureUsages::TEXTURE_BINDING,
                view_formats: &[],
            },
            wgpu::util::TextureDataOrder::LayerMajor,
            &r.heights,
        )
        .create_view(&Default::default())
}

/// A volume of half-float pairs.
fn volume(gpu: &Gpu, size: [u32; 3], texels: &[[u16; 2]]) -> wgpu::TextureView {
    use wgpu::util::DeviceExt;
    gpu.device
        .create_texture_with_data(
            &gpu.queue,
            &wgpu::TextureDescriptor {
                label: Some("river waves"),
                size: wgpu::Extent3d {
                    width: size[0],
                    height: size[1],
                    depth_or_array_layers: size[2],
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D3,
                format: wgpu::TextureFormat::Rg16Float,
                usage: wgpu::TextureUsages::TEXTURE_BINDING,
                view_formats: &[],
            },
            wgpu::util::TextureDataOrder::LayerMajor,
            bytemuck::cast_slice(texels),
        )
        .create_view(&Default::default())
}

#[cfg(test)]
mod tests {
    use super::*;

    // A record's own fields win; what it leaves out takes the roman pool
    // rivers' values, lakes' own where they differ.
    #[test]
    fn records_fill_what_they_leave_out() {
        let river = Record::read(
            &Look {
                lake: false,
                fields: vec![
                    ("ViewDepth".into(), vec![3.]),
                    ("ColorDeep".into(), vec![0.1, 0.2, 0.3]),
                ],
            },
            &SeasonLook::DEFAULT,
        );
        assert_eq!(river.view_depth, 3.);
        assert_eq!(river.deep, [0.1, 0.2, 0.3]);
        assert_eq!(river.wave_intensity, 1.2);
        let lake = Record::read(
            &Look {
                lake: true,
                fields: Vec::new(),
            },
            &SeasonLook::DEFAULT,
        );
        assert!(lake.lake);
        assert_eq!(
            (lake.view_depth, lake.wave_intensity, lake.shallow_depth),
            (10., 0.7, 0.4)
        );
    }

    // RenderFlags' bit 4 makes a river lava, and a LavaFlowOffset under
    // -9998 keeps it flowing; both are integers stored as float bits, as
    // the continental island's streams store them (flags 15, offset -9999).
    #[test]
    fn records_flag_lava() {
        let river = |fields: Vec<(String, Vec<f32>)>| {
            Record::read(
                &Look {
                    lake: false,
                    fields,
                },
                &SeasonLook::DEFAULT,
            )
        };
        let bits = |v: u32| vec![f32::from_bits(v)];
        let stream = river(vec![
            ("RenderFlags".into(), bits(15)),
            ("LavaFlowOffset".into(), bits(78)),
        ]);
        assert!(stream.lava && !stream.permanent);
        let kept = river(vec![
            ("RenderFlags".into(), bits(7)),
            ("LavaFlowOffset".into(), bits(-9999i32 as u32)),
        ]);
        assert!(kept.lava && kept.permanent);
        assert!(!river(vec![("RenderFlags".into(), bits(3))]).lava);
        assert!(!river(Vec::new()).lava);
    }

    // globals.xml's Egypt blocks as the game ships them (2026-11-18), cut
    // down to what the reader takes.
    const GLOBALS: &str = "<LSAOMaximum>0.457</>
<DLC03_InlandWater_DrySeason><WaveSize>5.02</>
<WaveSpeed>1.5</>
<WaveIntensity>1</>
<ShallowDepth>0.7</>
<AbsorptionShallow>0.27</>
<ColorShallow><x>0.6117647</>
<y>0.85490197</>
<z>0.7058824</>
</>
<ColorDeep><x>0.13333334</>
<y>0.3372549</>
<z>0.49411765</>
</>
<ScatterColor><x>0.19312572</>
<y>0.2130055</>
<z>0.27058825</>
</>
<ScatterStrength>0.11</>
<ViewDepth>7.73</>
<AbsorptionSmoothness>1</>
<PhysicalWaveIntensity>0.6</>
<LargeWaveWeight>0.5</>
<MediumWaveWeight>0.8</>
<SmallWaveWeight>0.5</>
</>
<DLC03_InlandWater_FloodSeason><WaveSize>5.02</>
<WaveSpeed>1.5</>
<WaveIntensity>1.3</>
<ShallowDepth>0.12</>
<AbsorptionShallow>0.52</>
<ColorShallow><x>0.9232737</>
<y>0.87933874</>
<z>0.74617517</>
</>
<ColorDeep><x>0.72378516</>
<y>0.5452072</>
<z>0.1166201</>
</>
<ScatterColor><x>0.8388747</>
<y>0.7208744</>
<z>0.29607347</>
</>
<ScatterStrength>0.57</>
<ViewDepth>11.4</>
<AbsorptionSmoothness>0.94</>
<PhysicalWaveIntensity>0.55</>
<LargeWaveWeight>0.5</>
<MediumWaveWeight>0.8</>
<SmallWaveWeight>0.5</>
</>
<DLC03_InlandWater_TransitionPhysWaveIntensity>1.71</>
<DLC03_InlandWater_SmallWaveWindGustBoost>0.7</>
<DLC03_FlowMap_FileName>data/dlc03/graphics/library/effects/water/maps/river_flowmap_01_diff.psd</>
<StarStrength>0.512</>";

    // Both seasons and the two loose values read from globals.xml.
    #[test]
    fn seasons_read_from_globals() {
        let s = Seasons::read(GLOBALS).expect("seasons");
        assert_eq!(s.dry.deep, [0.13333334, 0.3372549, 0.49411765]);
        assert_eq!((s.dry.wave_size, s.dry.wave_speed), (5.02, 1.5));
        assert_eq!(s.dry.weights, [0.5, 0.8, 0.5]);
        assert_eq!(s.flood.view_depth, 11.4);
        assert_eq!(s.flood.scatter_strength, 0.57);
        assert_eq!((s.transition_physical, s.gust_boost), (1.71, 0.7));
        assert_eq!(
            s.flow_map.as_deref(),
            Some("data/dlc03/graphics/library/effects/water/maps/river_flowmap_01_diff.psd")
        );
        assert!(Seasons::read("<StarStrength>0.5</>").is_none());
        // A block missing a field takes the engine's, not the next block's.
        let thin = "<DLC03_InlandWater_DrySeason><WaveSize>2</>\n</>\n<DLC03_InlandWater_FloodSeason><ViewDepth>3</>\n</>";
        let s = Seasons::read(thin).unwrap();
        assert_eq!(s.dry.wave_size, 2.);
        assert_eq!(s.dry.view_depth, SeasonLook::DEFAULT.view_depth);
    }

    // Between the seasons everything blends straight across but the
    // ripples' size and speed (the dry season's) and the physical waves,
    // which peak at the transition value halfway.
    #[test]
    fn seasons_blend_as_the_game_does() {
        let s = Seasons::read(GLOBALS).unwrap();
        assert_eq!(
            s.at(0.),
            SeasonLook {
                physical: 0.6,
                ..s.dry
            }
        );
        assert_eq!(
            s.at(1.),
            SeasonLook {
                wave_size: 5.02,
                wave_speed: 1.5,
                physical: 0.55,
                ..s.flood
            }
        );
        let half = s.at(0.5);
        assert_eq!(half.physical, 1.71);
        assert!((half.view_depth - (7.73 + 11.4) / 2.).abs() < 1e-5);
        let quarter = s.at(0.25);
        assert!((quarter.physical - (0.5 * 0.6 + 0.5 * 1.71)).abs() < 1e-5);
    }

    // The dry season raises the rivers 0.01 and the flood 0.5, the look
    // following the height's share of the flood's: a fiftieth when dry.
    #[test]
    fn seasons_lift_the_rivers() {
        let s = Seasons::read(GLOBALS).unwrap();
        let (dry, lift) = s.state(0., [0.01, 0.5]);
        assert_eq!(lift, 0.01);
        assert_eq!(dry, s.at(0.02));
        let (flood, lift) = s.state(1., [0.01, 0.5]);
        assert_eq!((flood, lift), (s.at(1.), 0.5));
        let (_, lift) = s.state(0.5, [0.01, 0.5]);
        assert!((lift - 0.255).abs() < 1e-6);
    }

    // RenderFlags 0x20 swaps the record's look for the session's: Egypt's
    // season look, or the engine's elsewhere. MaxFoamDepth stays the
    // record's, and records of their own look keep the engine's fixed wave
    // terms.
    #[test]
    fn records_take_the_global_look() {
        let look = |flags: u32| Look {
            lake: false,
            fields: vec![
                ("RenderFlags".into(), vec![f32::from_bits(flags)]),
                ("ColorDeep".into(), vec![0.988, 0.949, 0.835]),
                ("WaveIntensity".into(), vec![1.3]),
                ("MaxFoamDepth".into(), vec![0.25]),
            ],
        };
        let flood = Seasons::read(GLOBALS).unwrap().at(1.);
        let egypt = Record::read(&look(35), &flood);
        assert_eq!(egypt.deep, flood.deep);
        assert_eq!((egypt.wave_size, egypt.physical), (5.02, 0.55));
        assert_eq!(egypt.weights, [0.5, 0.8, 0.5]);
        assert_eq!(egypt.max_foam_depth, 0.25);
        let elsewhere = Record::read(&look(35), &SeasonLook::DEFAULT);
        assert_eq!(elsewhere.deep, [1.; 3]);
        let own = Record::read(&look(3), &flood);
        assert_eq!(own.deep, [0.988, 0.949, 0.835]);
        assert_eq!((own.wave_size, own.wave_speed, own.physical), (1., 1., 1.));
        assert_eq!(own.weights, [0., 1., 0.]);
    }

    // Lava cells leave the lattice's triangles (lava draws from its strips),
    // lakes staying first; an island without lava keeps its indices.
    #[test]
    fn lava_cells_leave_the_lattice() {
        use anno_core::water::Corner;
        let look = |lake: bool, flags: u32| Look {
            lake,
            fields: vec![("RenderFlags".into(), vec![f32::from_bits(flags)])],
        };
        // One row of cells, 5 wide: lake, lava, water, lava, lake.
        let cells = [0u32, 1, 2, 1, 0];
        let corners: Vec<Corner> = (0..2)
            .flat_map(|_| (0..6).map(|x| cells[x.min(4)]))
            .map(|look| Corner {
                look,
                ..Corner::dry()
            })
            .collect();
        let looks = vec![look(true, 0), look(false, 15), look(false, 3)];
        let inland = Inland::new([0.; 2], 1., [6, 2], corners, looks.clone(), None);
        let (indices, split) = inland.triangles();
        let kept = without_lava(&inland, indices, split);
        let looks_of = |r: std::ops::Range<usize>| -> Vec<u32> {
            kept[r]
                .chunks(3)
                .map(|t| inland.corners[t[0] as usize].look)
                .collect()
        };
        assert_eq!(looks_of(0..split as usize), [0; 4]);
        assert_eq!(looks_of(split as usize..kept.len()), [2; 2]);

        let calm: Vec<Look> = vec![look(true, 0), look(false, 3), look(false, 3)];
        let corners = inland.corners.clone();
        let inland = Inland::new([0.; 2], 1., [6, 2], corners, calm, None);
        let (indices, split) = inland.triangles();
        let kept = without_lava(&inland, indices, split);
        assert!(matches!(kept, Cow::Borrowed(_)));
    }
}
