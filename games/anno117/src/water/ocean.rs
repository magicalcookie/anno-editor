// Owns the ocean's waves as Anno 117 simulates them (ocean_simulation.fx):
// two wave fields, coastal and offshore, of four bands each. Their spectra
// are built here on the CPU when the settings change, and stepped on the
// GPU every frame (ocean.wgsl) into displacement and slope textures the
// ocean surface samples. The FFT works in metres; the world is in Anno's
// units of 4 m.
use super::{Gpu, entry, half, image_key, inland, key, layout_entry, storage, uniform};
use anno_core::ambient::Detail;
use anno_core::water::{Coast, Inland, IslandMaps, gauss};
use gpui_editor_kit::gpu::{self, Budget, StreamedTexture, bc_supported};
use gpui_editor_kit::texture::Image;
use std::sync::Arc;

/// Texels per side of every band's maps.
pub(crate) const N: u32 = 256;
/// Coastal bands 0-3, offshore 4-7.
pub(crate) const SLICES: u32 = 8;
/// Levels of the displacement and slope maps; the surface reads mip 1 of
/// the fold for foam and caustics.
pub(crate) const MIPS: u32 = 6;
/// Anno's world unit (defines.fxh METERS_PER_WORLD_UNIT).
pub const METRES_PER_UNIT: f32 = 4.;

/// The game's band table (CWaterManager init): patch side in metres and the
/// wavenumbers (rad/m) each keeps, disjoint so the bands sum without
/// counting a wave twice.
pub(crate) const BANDS: [(f32, f32, f32); 4] = [
    (2521., 0., 0.13),
    (1201., 0.13, 0.20),
    (211., 0.20, 1.19),
    (31., 1.19, f32::MAX),
];

/// One wave field: a preset's `CoastalWave*` or `OffshoreWave*`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Waves {
    /// Metres of open water the wind has blown over.
    pub fetch: f32,
    /// Metres per second, 10 m up.
    pub wind_speed: f32,
    /// How far crests lean and sharpen sideways.
    pub choppiness: f32,
    /// 0: waves from every direction; 1: Donelan-Banner spreading around
    /// the wind.
    pub directionality: f32,
    /// Metres; shallow seas damp the longest waves (TMA).
    pub depth: f32,
}

/// Anno 117's open sea: two FFT wave fields blended
/// by distance from the coast, shaded by absorption, scattering, caustics,
/// the sun and the sky's reflection. Fields are an ambient preset's
/// `<Water>` block; depths and distances are world units.
#[derive(Clone, Debug)]
pub struct Ocean {
    pub coastal: Waves,
    pub offshore: Waves,
    /// `WaveSeed`: in Anno the modulus of the spectra's random numbers, not
    /// a seed, so it sets how coarsely they are quantised.
    pub seed: u32,
    /// The wind's direction in world X and Z.
    pub wind: [f32; 2],
    /// Coastal waves out to this distance from the coast, offshore ones
    /// past it plus `offshore_transition`.
    pub offshore_distance: f32,
    pub offshore_transition: f32,
    pub caustics: f32,
    pub deep: [f32; 3],
    pub shallow: [f32; 3],
    pub scatter: [f32; 3],
    pub scatter_strength: f32,
    pub absorption_shallow: f32,
    pub shallow_depth: f32,
    pub absorption_smoothness: f32,
    pub view_depth: f32,
    /// `AmbientLightFactorOcean`: the sky's reflection.
    pub reflection: f32,
    pub coast: Option<Arc<Coast>>,
    /// The island's own WaterInfos and WaterFlow; without them the coast
    /// map stands in.
    pub maps: Option<Arc<IslandMaps>>,
    /// `foam01`: four foam patterns in its channels.
    pub foam: Option<Image>,
    /// `noise01`, whose green channel gates the physical waves' shore rings
    /// and crest foam; value noise without it.
    pub noise: Option<Image>,
    /// Seconds of wave time; `None` follows the clock.
    pub time: Option<f32>,
}

impl Ocean {
    /// The eight slices' wave fields and bands, in slice order.
    fn slices(&self) -> impl Iterator<Item = (Waves, (f32, f32, f32))> + '_ {
        [self.coastal, self.offshore]
            .into_iter()
            .flat_map(|w| BANDS.into_iter().map(move |b| (w, b)))
    }

    /// h0 and ω of every slice, [slice][row][column] (csInitWaveSpectrum
    /// with its quirks: the wind angle's arguments swapped, the phase drawn
    /// from the slice and row, and `seed` as the random numbers' modulus).
    pub(crate) fn spectrum(&self) -> (Vec<[f32; 2]>, Vec<f32>) {
        let n = N as usize;
        let mut h0 = vec![[0.; 2]; n * n * SLICES as usize];
        let mut omega = vec![0.; n * n * SLICES as usize];
        let g = 9.81f32;
        let seed = self.seed.max(1);
        let pi = std::f32::consts::PI;
        // A slice per thread: each cell stands alone, and one after
        // another they held the UI thread 18 ms on the sea's first frame.
        let slices =
            (self.slices().enumerate()).zip(h0.chunks_mut(n * n).zip(omega.chunks_mut(n * n)));
        std::thread::scope(|s| {
            for ((z, (waves, (patch, min_k, max_k))), (h0, omega)) in slices {
                s.spawn(move || {
                    let delta_k = std::f32::consts::TAU / patch;
                    let (f, u) = (waves.fetch.max(1.), waves.wind_speed.max(1e-3));
                    let alpha = 0.076 * (u * u / (f * g)).powf(0.22);
                    let omega_p = 22. * (g * g / (u * f)).powf(0.33);
                    let theta0 = self.wind[0].atan2(self.wind[1]);
                    for y in 0..n {
                        for x in 0..n {
                            let at = y * n + x;
                            let kv = [(x as f32 - 128.) * delta_k, (y as f32 - 128.) * delta_k];
                            let k = (kv[0] * kv[0] + kv[1] * kv[1]).sqrt();
                            let w = (g * k).sqrt();
                            omega[at] = w;
                            if w == 0. || min_k > k || max_k < k {
                                continue;
                            }
                            let d_omega = g / (2. * w);
                            let mut theta = kv[1].atan2(kv[0]) - theta0;
                            if theta < -pi {
                                theta += 2. * pi;
                            }
                            if theta > pi {
                                theta -= 2. * pi;
                            }
                            let phase = 2. * pi * gauss(z as u32, y as u32, seed);
                            let amplitude = gauss(x as u32, y as u32, seed);
                            let spread =
                                directional_spread(theta, omega_p, w, waves.directionality);
                            let tma = tma(waves.depth, alpha, omega_p, w);
                            let scale =
                                (2. * (tma * d_omega / k) * spread * delta_k * delta_k).sqrt();
                            h0[at] = [
                                amplitude * phase.cos() * scale,
                                amplitude * phase.sin() * scale,
                            ];
                        }
                    }
                });
            }
        });
        (h0, omega)
    }

    /// What a change needs the spectra rebuilt for.
    fn spectrum_key(&self) -> SpectrumKey {
        (
            self.coastal,
            self.offshore,
            self.seed,
            self.wind.map(f32::to_bits),
        )
    }
}

type SpectrumKey = (Waves, Waves, u32, [u32; 2]);

impl Ocean {
    /// The surface's uniforms (fs_ocean's OceanParams) at wave time `time`.
    /// Absorption and scattering are fixed per preset, so they are worked
    /// out here (MeanPathAbsorption's coefficients, ComputeWaterDiffuse's
    /// σs) instead of per pixel.
    fn uniforms(&self, time: f32) -> [[f32; 4]; PARAMS] {
        let (shallow, deep, scatter) = absorption_terms(
            self.shallow,
            self.deep,
            self.scatter,
            self.absorption_shallow,
            self.view_depth,
            self.scatter_strength,
        );
        let coast = self.coast.as_ref().map_or([0.; 4], |c| {
            [
                c.origin[0],
                c.origin[1],
                1. / c.size[0].max(1e-3),
                1. / c.size[1].max(1e-3),
            ]
        });
        let patch = |b: usize| BANDS[b].0 / METRES_PER_UNIT;
        [
            [shallow[0], shallow[1], shallow[2], self.shallow_depth],
            [deep[0], deep[1], deep[2], self.absorption_smoothness],
            [scatter[0], scatter[1], scatter[2], self.view_depth],
            [
                self.shallow[0],
                self.shallow[1],
                self.shallow[2],
                self.caustics,
            ],
            [
                self.offshore_distance,
                self.offshore_transition,
                time,
                self.reflection,
            ],
            coast,
            [patch(0), patch(1), patch(2), patch(3)],
            [
                SEGMENTS as f32,
                RINGS as f32,
                RING_GROWTH,
                if self.foam.is_some() { 1. } else { 0. },
            ],
            // The physical waves' square, filled in once stepped.
            [0.; 4],
            [
                self.coast.as_ref().map_or(4., |c| c.depth_scale),
                0.,
                0.,
                0.,
            ],
            (self.maps.as_ref()).map_or([0.; 4], |m| {
                let [x, z] = m.scale();
                [x, z, 1., 0.]
            }),
            REFLECTIONS.params(),
        ]
    }
}

/// OceanParams' vec4s.
const PARAMS: usize = 12;

/// How the sea, rivers and lakes trace reflections of the island (the
/// game's SSR pass, FidelityFX SSSR, traced in its own resolution and
/// denoised; the kit's is a march per pixel). TODO: the game's reach and
/// thickness are not traced to data; a ray reaching across a large island
/// at a tenth of the cost of the rest of the water.
const REFLECTIONS: gpu::Ssr = gpu::Ssr {
    steps: 40,
    reach: 250.,
    thickness: 3.,
    edge: 0.08,
};

/// MeanPathAbsorption's shallow and deep coefficients and
/// ComputeWaterDiffuse's σs, from a water's colors and depths. Fixed per
/// water, so worked out here instead of per pixel.
pub(crate) fn absorption_terms(
    shallow: [f32; 3],
    deep: [f32; 3],
    scatter: [f32; 3],
    absorption_shallow: f32,
    view_depth: f32,
    scatter_strength: f32,
) -> ([f32; 3], [f32; 3], [f32; 3]) {
    let hue = hsb(scatter);
    (
        absorption(shallow).map(|c| c * absorption_shallow),
        absorption(deep).map(|c| c * (0.01f32.ln() / -view_depth.max(1e-3))),
        rgb([hue[0], hue[1], 1.]).map(|c| c * scatter_strength),
    )
}

/// ConvertWaterColorToAbsorption: a water color's complement at full
/// brightness, what the water takes out of light.
fn absorption(color: [f32; 3]) -> [f32; 3] {
    let h = hsb(color.map(|c| 1. - c));
    rgb([h[0], h[1], 1.])
}

/// RGBtoHSB (functions.fxh).
fn hsb(c: [f32; 3]) -> [f32; 3] {
    let top = c[0].max(c[1]).max(c[2]);
    let low = c[0].min(c[1]).min(c[2]);
    let chroma = top - low;
    if chroma == 0. {
        return [0., 0., top];
    }
    let d = c.map(|v| (top - v) / chroma);
    let delta = [d[0] - d[2] + 2., d[1] - d[0] + 4., d[2] - d[1]];
    let h = if c[0] >= top {
        delta[2]
    } else if c[1] >= top {
        delta[0]
    } else {
        delta[1]
    };
    [(h / 6.).rem_euclid(1.), chroma / top, top]
}

/// HSBtoRGB (functions.fxh).
fn rgb(h: [f32; 3]) -> [f32; 3] {
    [5., 3., 1.].map(|f: f32| {
        let k = (f + 360. * h[0] / 60.).rem_euclid(6.);
        h[2] * (1. - h[1] * k.min(4. - k).clamp(0., 1.))
    })
}

/// The surface grid around the camera: segments around, rings out, and
/// how much wider each ring is than the last (vs_ocean). The outermost
/// ring lies about 61 000 times the first one's radius out: some 25 km
/// from a camera 100 units up.
const SEGMENTS: u32 = 256;
const RINGS: u32 = 180;
const RING_GROWTH: f32 = 1.045;

/// Vertices of the grid's strips (vs_ocean): per band between two rings
/// both rings' corners around to the seam, and two more joining it to the
/// next; the last band needs no join.
fn strip_vertices() -> u32 {
    (RINGS - 1) * (2 * SEGMENTS + 4) - 2
}

/// The ocean surface pass and the presented ocean's GPU state: the wave
/// simulation, the coast map and the foam texture. Built with the device;
/// the state appears with the first ocean and stays for the next.
pub(crate) struct Surface {
    layout: wgpu::BindGroupLayout,
    pipeline: wgpu::RenderPipeline,
    clamp: wgpu::Sampler,
    wrap: wgpu::Sampler,
    params: wgpu::Buffer,
    /// 1x1 stand-ins for a missing coast or foam map.
    blank: wgpu::TextureView,
    simulation: Option<Simulation>,
    coast: Option<(usize, wgpu::TextureView)>,
    /// The island's WaterInfos and WaterFlow.
    maps: Option<(usize, [wgpu::TextureView; 2])>,
    foam: Option<(usize, Arc<StreamedTexture>)>,
    noise: Option<(usize, Arc<StreamedTexture>)>,
    physical: Physical,
    /// Rivers and lakes, drawn after the sea with its bindings.
    inland: inland::Pass,
    duckweed: Option<[(usize, Arc<StreamedTexture>); 2]>,
    inland_ready: bool,
}

impl Surface {
    pub fn new(
        gpu: &Gpu,
        shader: &wgpu::ShaderModule,
        frame: &wgpu::BindGroupLayout,
        format: wgpu::TextureFormat,
    ) -> Self {
        use wgpu::ShaderStages as S;
        let d = &gpu.device;
        let both = S::VERTEX | S::FRAGMENT;
        let texture = |dim, sample_type| wgpu::BindingType::Texture {
            sample_type,
            view_dimension: dim,
            multisampled: false,
        };
        let float = wgpu::TextureSampleType::Float { filterable: true };
        let array = wgpu::TextureViewDimension::D2Array;
        let d2 = wgpu::TextureViewDimension::D2;
        let entries = [
            layout_entry(0, both, uniform(false)),
            layout_entry(1, both, texture(array, float)),
            layout_entry(2, S::FRAGMENT, texture(array, float)),
            layout_entry(3, S::FRAGMENT, texture(d2, wgpu::TextureSampleType::Uint)),
            layout_entry(4, both, texture(d2, float)),
            layout_entry(5, S::FRAGMENT, texture(d2, float)),
            layout_entry(
                6,
                both,
                wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
            ),
            layout_entry(
                7,
                both,
                wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
            ),
            layout_entry(13, both, texture(d2, float)),
            layout_entry(17, both, texture(d2, float)),
            layout_entry(18, both, texture(d2, float)),
        ];
        let layout = d.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("ocean surface"),
            entries: &entries,
        });
        let pipeline_layout = d.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("ocean surface"),
            bind_group_layouts: &[Some(frame), Some(&layout)],
            immediate_size: 0,
        });
        // Compiled once per device (0.9 s cold), and before any island by
        // `water::warm`; as the waves' and the physical waves'.
        let pipeline = gpu::per_device(d, "117 ocean surface", || {
            d.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some("ocean surface"),
                layout: Some(&pipeline_layout),
                vertex: wgpu::VertexState {
                    module: shader,
                    entry_point: Some("vs_ocean"),
                    buffers: &[],
                    compilation_options: Default::default(),
                },
                fragment: Some(wgpu::FragmentState {
                    module: shader,
                    entry_point: Some("fs_ocean"),
                    targets: &[Some(wgpu::ColorTargetState {
                        format,
                        blend: None,
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                    compilation_options: Default::default(),
                }),
                // One strip per band between rings (vs_ocean).
                primitive: wgpu::PrimitiveState {
                    topology: wgpu::PrimitiveTopology::TriangleStrip,
                    ..Default::default()
                },
                // Opaque and depth-written, so the sky, decals and blended
                // surfaces after it meet the water where it lies.
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
        });
        let clamp = d.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("ocean coast"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        let wrap = d.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("ocean waves"),
            address_mode_u: wgpu::AddressMode::Repeat,
            address_mode_v: wgpu::AddressMode::Repeat,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            mipmap_filter: wgpu::MipmapFilterMode::Linear,
            ..Default::default()
        });
        let params = d.create_buffer(&wgpu::BufferDescriptor {
            label: Some("ocean surface"),
            size: PARAMS as u64 * 16,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let blank = d
            .create_texture(&wgpu::TextureDescriptor {
                label: Some("ocean blank"),
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
        let inland = inland::Pass::new(gpu, shader, frame, &entries, format);
        let physical = Physical::new(gpu, shader);
        Self {
            layout,
            pipeline,
            clamp,
            wrap,
            params,
            blank,
            simulation: None,
            coast: None,
            maps: None,
            foam: None,
            noise: None,
            physical,
            inland,
            duckweed: None,
            inland_ready: false,
        }
    }

    /// Readies `ocean` for this frame: its maps uploaded (the foam a slice
    /// per frame), the waves stepped to `time`, the uniforms written.
    /// False while the foam is still uploading, so the sea does not flash
    /// in without it.
    pub fn prepare(
        &mut self,
        gpu: &Gpu,
        encoder: &mut wgpu::CommandEncoder,
        ocean: &Ocean,
        // Rivers and lakes, with what they draw with.
        inland: Option<InlandShown<'_>>,
        focus: [f32; 2],
        time: f32,
        // A window's frame, whose texture uploads keep to the time budget.
        timed: bool,
        detail: Detail,
    ) -> bool {
        let bc = bc_supported(&gpu.device);
        let coast_key = ocean.coast.as_ref().map_or(0, key);
        if self.coast.as_ref().map(|c| c.0) != Some(coast_key) {
            self.coast = ocean
                .coast
                .as_ref()
                .map(|c| (coast_key, upload_coast(gpu, c)));
        }
        let maps_key = ocean.maps.as_ref().map_or(0, key);
        if self.maps.as_ref().map(|m| m.0) != Some(maps_key) {
            self.maps = (ocean.maps.as_ref()).map(|m| (maps_key, upload_maps(gpu, m)));
        }
        streamed(gpu, &mut self.foam, ocean.foam.as_ref(), bc, "ocean foam");
        streamed(
            gpu,
            &mut self.noise,
            ocean.noise.as_ref(),
            bc,
            "ocean noise",
        );
        if let Some(images) = inland.as_ref().and_then(|i| i.duckweed) {
            let k = image_key(&images[0]);
            if self.duckweed.as_ref().map(|d| d[0].0) != Some(k) {
                let made = images.each_ref().map(|image| {
                    StreamedTexture::new(&gpu.device, vec![image.clone()], false, bc)
                        .map_err(|e| eprintln!("duckweed: {e:#}"))
                        .ok()
                        .map(|t| (image_key(image), Arc::new(t)))
                });
                self.duckweed = match made {
                    [Some(a), Some(b)] => Some([a, b]),
                    _ => None,
                };
            }
        }
        for (_, texture) in
            (self.foam.iter().chain(&self.noise)).chain(self.duckweed.iter().flatten())
        {
            if !texture.stream(&gpu.queue, &mut Budget::frame(timed)) {
                return false;
            }
        }
        let simulation = self.simulation.get_or_insert_with(|| Simulation::new(gpu));
        simulation.step(gpu, encoder, ocean, time);
        let mut uniforms = ocean.uniforms(time);
        uniforms[7][3] = if self.foam.is_some() { 1. } else { 0. };
        if !detail.reflections {
            uniforms[11][0] = 0.;
        }
        // band_factors leaves the finest band out (the shore's y).
        uniforms[9][1] = if detail.fine_waves { 0. } else { 1. };
        let coast = self.coast.as_ref().map_or(&self.blank, |c| &c.1);
        let noise = self.noise.as_ref().map(|n| n.1.view());
        // Queued writes land before the frame's commands, so the step reads
        // the params written below.
        let [info, flow] = self
            .maps
            .as_ref()
            .map_or([&self.blank; 2], |m| [&m.1[0], &m.1[1]]);
        let shared = Shared {
            params: &self.params,
            waves: &simulation.displacement,
            slopes: &simulation.gradients,
            coast,
            info,
            flow,
            noise: noise.as_ref().unwrap_or(&self.blank),
            clamp: &self.clamp,
            wrap: &self.wrap,
        };
        self.physical
            .step(gpu, encoder, &shared, noise.is_some(), focus, time);
        uniforms[8] = self.physical.area();
        gpu.queue
            .write_buffer(&self.params, 0, bytemuck::cast_slice(&uniforms));
        self.inland_ready = inland.is_some();
        if let Some(shown) = inland {
            self.inland.prepare(gpu, encoder, &shown, time);
        }
        true
    }

    /// Draws the sea, then rivers and lakes, into `pass`, whose group 0 is
    /// the frame with the opaque depth copy; `scene` is the opaque pass's
    /// linear color.
    pub fn draw(&self, gpu: &Gpu, pass: &mut wgpu::RenderPass<'_>, scene: &wgpu::TextureView) {
        let Some(simulation) = &self.simulation else {
            return;
        };
        let coast = self.coast.as_ref().map_or(&self.blank, |c| &c.1);
        let foam = self.foam.as_ref().map(|f| f.1.view());
        let foam = foam.as_ref().unwrap_or(&self.blank);
        let physical = self.physical.current().unwrap_or(&self.blank);
        let [info, flow] = self
            .maps
            .as_ref()
            .map_or([&self.blank; 2], |m| [&m.1[0], &m.1[1]]);
        let entries = || {
            vec![
                entry(0, self.params.as_entire_binding()),
                entry(1, view(&simulation.displacement)),
                entry(2, view(&simulation.gradients)),
                entry(3, view(scene)),
                entry(4, view(coast)),
                entry(5, view(foam)),
                entry(6, wgpu::BindingResource::Sampler(&self.clamp)),
                entry(7, wgpu::BindingResource::Sampler(&self.wrap)),
                entry(13, view(physical)),
                entry(17, view(info)),
                entry(18, view(flow)),
            ]
        };
        let group = gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("ocean surface"),
            layout: &self.layout,
            entries: &entries(),
        });
        pass.set_pipeline(&self.pipeline);
        pass.set_bind_group(1, &group, &[]);
        pass.draw(0..strip_vertices(), 0..1);
        if self.inland_ready {
            let duckweed = self
                .duckweed
                .as_ref()
                .map(|d| [d[0].1.view(), d[1].1.view()]);
            let duckweed = duckweed.as_ref().map(|[a, b]| [a, b]);
            self.inland.draw(gpu, pass, entries(), duckweed);
        }
    }
}

/// Rivers and lakes to draw, and what they draw with: the river ripples
/// (flat without), duckweed, rain's ripples with how hard it rains, and the
/// session's look and shader.
pub(crate) struct InlandShown<'a> {
    pub inland: &'a Arc<Inland>,
    pub waves: Option<&'a Arc<inland::RiverWaves>>,
    pub duckweed: Option<&'a [Image; 2]>,
    pub rain: Option<&'a Arc<crate::sea::Ripples>>,
    pub raining: f32,
    /// The look USE_GLOBAL_SHADING water takes this session.
    pub global: inland::SeasonLook,
    /// In Egypt, which draws with EGYPT_INLAND_WATER: its small waves'
    /// wind gust boost.
    pub egypt: Option<f32>,
    /// cAccumulatedWind: world X and Z the wind has blown the gusts.
    pub wind: [f32; 2],
    /// How far the season lifts every river (CurrentRiverLevelOffset).
    pub river_lift: f32,
    /// Egypt's painted flow map, the island's corner on it and the
    /// session's size it spans (ISF_USES_FLOW_TEX_FROM_FILE).
    pub flow_file: Option<(&'a Image, [f32; 2], [f32; 2])>,
    pub gusts: Option<&'a Arc<crate::sea::Gusts>>,
}

/// Keeps `slot` holding `image` as a streamed texture, rebuilt when the
/// image changes.
pub(super) fn streamed(
    gpu: &Gpu,
    slot: &mut Option<(usize, Arc<StreamedTexture>)>,
    image: Option<&Image>,
    bc: bool,
    what: &str,
) {
    let k = image.map_or(0, image_key);
    if slot.as_ref().map(|s| s.0) == Some(k) {
        return;
    }
    *slot = image.and_then(|image| {
        let texture = StreamedTexture::new(&gpu.device, vec![image.clone()], false, bc)
            .map_err(|e| eprintln!("{what}: {e:#}"))
            .ok()?;
        Some((k, Arc::new(texture)))
    });
}

/// What the physical waves read of the ocean.
struct Shared<'a> {
    params: &'a wgpu::Buffer,
    waves: &'a wgpu::TextureView,
    slopes: &'a wgpu::TextureView,
    coast: &'a wgpu::TextureView,
    info: &'a wgpu::TextureView,
    flow: &'a wgpu::TextureView,
    noise: &'a wgpu::TextureView,
    clamp: &'a wgpu::Sampler,
    wrap: &'a wgpu::Sampler,
}

/// Texels a side of the physical waves' square: the game's at medium and
/// high WaterQuality (1440 at ultra), a texel an eighth of a unit, so 160
/// units (640 m) around the camera's focus.
const PHYSICAL_N: u32 = 1280;
const PHYSICAL_TEXEL: f32 = 0.125;
/// Steps a second. The game steps once a rendered frame, so its waves run
/// faster at higher frame rates; 60 is what it is tuned to look right at.
const PHYSICAL_RATE: f32 = 60.;
/// At most this many steps a frame, so a stalled frame does not queue up
/// seconds of steps; the sim falls behind the clock instead.
const PHYSICAL_STEPS: u32 = 4;
/// A step's uniforms sit this far apart (wgpu's uniform offset alignment).
const STEP_STRIDE: u64 = 256;

/// The physical waves (cs_physical): two squares of (h, h last, crest foam,
/// wave foam), each step reading one and writing the other, centred on the
/// camera's focus and scrolled as it moves.
struct Physical {
    layout: wgpu::BindGroupLayout,
    pipeline: wgpu::ComputePipeline,
    steps: wgpu::Buffer,
    /// Allocated with the first step: 26 MB.
    states: Option<[wgpu::TextureView; 2]>,
    /// Which state the last step wrote.
    current: usize,
    origin: Option<[f32; 2]>,
    /// Wave time stepped to.
    time: Option<f32>,
}

impl Physical {
    fn new(gpu: &Gpu, shader: &wgpu::ShaderModule) -> Self {
        use wgpu::ShaderStages as S;
        let d = &gpu.device;
        let c = S::COMPUTE;
        let texture = |dim| wgpu::BindingType::Texture {
            sample_type: wgpu::TextureSampleType::Float { filterable: true },
            view_dimension: dim,
            multisampled: false,
        };
        let d2 = wgpu::TextureViewDimension::D2;
        let array = wgpu::TextureViewDimension::D2Array;
        let sampler = wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering);
        let layout = d.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("physical waves"),
            entries: &[
                layout_entry(0, c, uniform(false)),
                layout_entry(1, c, texture(array)),
                layout_entry(2, c, texture(array)),
                layout_entry(4, c, texture(d2)),
                layout_entry(6, c, sampler),
                layout_entry(7, c, sampler),
                layout_entry(13, c, texture(d2)),
                layout_entry(14, c, texture(d2)),
                layout_entry(15, c, uniform(true)),
                layout_entry(17, c, texture(d2)),
                layout_entry(18, c, texture(d2)),
                layout_entry(
                    16,
                    c,
                    wgpu::BindingType::StorageTexture {
                        access: wgpu::StorageTextureAccess::WriteOnly,
                        format: wgpu::TextureFormat::Rgba16Float,
                        view_dimension: d2,
                    },
                ),
            ],
        });
        let pipeline_layout = d.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("physical waves"),
            bind_group_layouts: &[None, Some(&layout)],
            immediate_size: 0,
        });
        let pipeline = gpu::per_device(d, "117 physical waves", || {
            d.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
                label: Some("physical waves"),
                layout: Some(&pipeline_layout),
                module: shader,
                entry_point: Some("cs_physical"),
                compilation_options: Default::default(),
                cache: None,
            })
        });
        let steps = d.create_buffer(&wgpu::BufferDescriptor {
            label: Some("physical waves steps"),
            size: STEP_STRIDE * u64::from(PHYSICAL_STEPS),
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        Self {
            layout,
            pipeline,
            steps,
            states: None,
            current: 0,
            origin: None,
            time: None,
        }
    }

    /// The state the ocean draws: the last one written.
    fn current(&self) -> Option<&wgpu::TextureView> {
        self.states.as_ref().map(|s| &s[self.current])
    }

    /// OceanParams' `physical`: the square's corner, 1 / its side, and
    /// 1 / its texels a side; zero before the first step.
    fn area(&self) -> [f32; 4] {
        let side = PHYSICAL_N as f32 * PHYSICAL_TEXEL;
        self.origin
            .map_or([0.; 4], |o| [o[0], o[1], 1. / side, 1. / PHYSICAL_N as f32])
    }

    /// Steps the waves at [`PHYSICAL_RATE`] up to `time`, the square moved
    /// to `focus` (snapped to whole texels, plus the game's half unit).
    fn step(
        &mut self,
        gpu: &Gpu,
        encoder: &mut wgpu::CommandEncoder,
        shared: &Shared<'_>,
        noise: bool,
        focus: [f32; 2],
        time: f32,
    ) {
        let side = PHYSICAL_N as f32 * PHYSICAL_TEXEL;
        let origin =
            focus.map(|c| ((c - side * 0.5) / PHYSICAL_TEXEL).floor() * PHYSICAL_TEXEL + 0.5);
        // A clock that went back (a reset preview) starts the sea afresh.
        if self.time.is_some_and(|t| time < t) {
            self.time = None;
            self.origin = None;
        }
        let due = self.time.map_or(1, |t| ((time - t) * PHYSICAL_RATE) as u32);
        let moved = self.origin != Some(origin);
        let count = due.max(u32::from(moved)).min(PHYSICAL_STEPS);
        if count == 0 {
            return;
        }
        let states = self
            .states
            .get_or_insert_with(|| [0, 1].map(|_| state(gpu)));
        let mut slots = Vec::with_capacity((STEP_STRIDE as usize / 4) * count as usize);
        for i in 0..count {
            // Only the first step scrolls; a new square drops the old.
            let offset = match (i, self.origin) {
                (0, Some(prev)) => [0, 1].map(|a| ((origin[a] - prev[a]) / PHYSICAL_TEXEL).round()),
                (0, None) => [-9999.; 2],
                _ => [0.; 2],
            };
            let at = self
                .time
                .map_or(time, |t| t + (i + 1) as f32 / PHYSICAL_RATE)
                .min(time);
            let mut slot = [0f32; STEP_STRIDE as usize / 4];
            slot[..8].copy_from_slice(&[
                origin[0],
                origin[1],
                side,
                1. / PHYSICAL_N as f32,
                offset[0],
                offset[1],
                at,
                if noise { 1. } else { 0. },
            ]);
            slots.extend_from_slice(&slot);
        }
        gpu.queue
            .write_buffer(&self.steps, 0, bytemuck::cast_slice(&slots));
        let group = |read: usize| {
            gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("physical waves"),
                layout: &self.layout,
                entries: &[
                    entry(0, shared.params.as_entire_binding()),
                    entry(1, view(shared.waves)),
                    entry(2, view(shared.slopes)),
                    entry(4, view(shared.coast)),
                    entry(6, wgpu::BindingResource::Sampler(shared.clamp)),
                    entry(7, wgpu::BindingResource::Sampler(shared.wrap)),
                    entry(13, view(&states[read])),
                    entry(14, view(shared.noise)),
                    entry(
                        15,
                        wgpu::BindingResource::Buffer(wgpu::BufferBinding {
                            buffer: &self.steps,
                            offset: 0,
                            size: wgpu::BufferSize::new(32),
                        }),
                    ),
                    entry(16, view(&states[1 - read])),
                    entry(17, view(shared.info)),
                    entry(18, view(shared.flow)),
                ],
            })
        };
        let groups = [group(0), group(1)];
        let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
            label: Some("physical waves"),
            timestamp_writes: None,
        });
        pass.set_pipeline(&self.pipeline);
        let groups_a_side = PHYSICAL_N.div_ceil(8);
        for i in 0..count {
            pass.set_bind_group(
                1,
                &groups[self.current],
                &[(u64::from(i) * STEP_STRIDE) as u32],
            );
            pass.dispatch_workgroups(groups_a_side, groups_a_side, 1);
            self.current = 1 - self.current;
        }
        // Behind by more than a frame's steps: catch up to the clock.
        self.time = Some(match self.time {
            Some(t) if due <= PHYSICAL_STEPS => t + due as f32 / PHYSICAL_RATE,
            _ => time,
        });
        self.origin = Some(origin);
    }
}

/// One physical waves square, cleared.
fn state(gpu: &Gpu) -> wgpu::TextureView {
    gpu.device
        .create_texture(&wgpu::TextureDescriptor {
            label: Some("physical waves"),
            size: wgpu::Extent3d {
                width: PHYSICAL_N,
                height: PHYSICAL_N,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba16Float,
            // Copied out only by the tests.
            usage: wgpu::TextureUsages::TEXTURE_BINDING
                | wgpu::TextureUsages::STORAGE_BINDING
                | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        })
        .create_view(&Default::default())
}

fn view(v: &wgpu::TextureView) -> wgpu::BindingResource<'_> {
    wgpu::BindingResource::TextureView(v)
}

/// The island's maps as two RGBA8 textures, filtered as the game's BC
/// copies are.
fn upload_maps(gpu: &Gpu, maps: &IslandMaps) -> [wgpu::TextureView; 2] {
    use wgpu::util::DeviceExt;
    [&maps.info, &maps.flow].map(|texels| {
        gpu.device
            .create_texture_with_data(
                &gpu.queue,
                &wgpu::TextureDescriptor {
                    label: Some("island water maps"),
                    size: wgpu::Extent3d {
                        width: maps.width.max(1),
                        height: maps.height.max(1),
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
    })
}

/// The coast map as half floats, which filter on every device.
fn upload_coast(gpu: &Gpu, coast: &Coast) -> wgpu::TextureView {
    let texture = gpu.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("ocean coast"),
        size: wgpu::Extent3d {
            width: coast.width.max(1),
            height: coast.height.max(1),
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba16Float,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    let texels: Vec<u16> = coast.texels.iter().flat_map(|t| t.map(half)).collect();
    gpu.queue.write_texture(
        texture.as_image_copy(),
        bytemuck::cast_slice(&texels),
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(coast.width * 8),
            rows_per_image: None,
        },
        wgpu::Extent3d {
            width: coast.width,
            height: coast.height,
            depth_or_array_layers: 1,
        },
    );
    texture.create_view(&Default::default())
}

fn jonswap(alpha: f32, omega_p: f32, omega: f32) -> f32 {
    if omega_p == 0. || omega == 0. {
        return 0.;
    }
    let g = 9.81f32;
    let frac = omega_p / omega;
    let sigma = if omega <= omega_p { 0.07 } else { 0.09 };
    let r = (-(omega - omega_p).powi(2) / (2. * sigma * sigma * omega_p * omega_p)).exp();
    alpha * g * g / omega.powi(5) * (-1.25 * frac.powi(4)).exp() * 3.3f32.powf(r)
}

fn tma(depth: f32, alpha: f32, omega_p: f32, omega: f32) -> f32 {
    let omega_h = omega * (depth / 9.81).sqrt();
    let phi = if omega_h <= 1. {
        omega_h * omega_h / 2.
    } else if omega_h < 2. {
        1. - 0.5 * (2. - omega_h).powi(2)
    } else {
        1.
    };
    phi * jonswap(alpha, omega_p, omega)
}

/// Donelan-Banner spreading around the wind, blended from even by
/// `strength`.
fn directional_spread(theta: f32, omega_p: f32, omega: f32, strength: f32) -> f32 {
    let frac = omega / omega_p;
    let beta = if frac < 0.95 {
        2.61 * frac.powf(1.3)
    } else if frac < 1.6 {
        2.28 * frac.powf(-1.3)
    } else {
        10f32.powf(-0.4 + 0.8393 * (-0.567 * (frac * frac).ln()).exp())
    };
    let sech = 1. / (beta * theta).cosh();
    let donelan_banner = 0.5 * beta * sech * sech / (beta * std::f32::consts::PI).tanh();
    let flat = 1. / std::f32::consts::TAU;
    flat + (donelan_banner - flat) * strength
}

/// The GPU side: spectra, scratch and the textures the surface samples.
/// One per renderer; stepped once per frame however many views draw it.
pub(crate) struct Simulation {
    vertical: wgpu::ComputePipeline,
    horizontal: wgpu::ComputePipeline,
    finish: wgpu::ComputePipeline,
    mip: wgpu::ComputePipeline,
    step: wgpu::Buffer,
    h0: wgpu::Buffer,
    omega: wgpu::Buffer,
    /// Read back by the tests; the view keeps it alive otherwise.
    #[cfg_attr(not(test), allow(dead_code))]
    waves: wgpu::Texture,
    /// Bands 0 and 1's group, then 2 and 3's (ocean.wgsl `part`).
    parts: [wgpu::BindGroup; 2],
    mips: Vec<wgpu::BindGroup>,
    /// Displacement in metres (Dx, Dy, Dz) with the fold, every slice, and
    /// (-dh/dx, -dh/dz) of a band's coastal and of its offshore slice, every
    /// band; every mip.
    pub displacement: wgpu::TextureView,
    pub gradients: wgpu::TextureView,
    key: Option<SpectrumKey>,
    /// The wave time last stepped to.
    time: Option<f32>,
}

impl Simulation {
    pub fn new(gpu: &Gpu) -> Self {
        use wgpu::BufferUsages as B;
        use wgpu::ShaderStages as S;
        let d = &gpu.device;
        let array = wgpu::TextureViewDimension::D2Array;
        let written = wgpu::BindingType::StorageTexture {
            access: wgpu::StorageTextureAccess::WriteOnly,
            format: wgpu::TextureFormat::Rgba16Float,
            view_dimension: array,
        };
        let read = wgpu::BindingType::Texture {
            sample_type: wgpu::TextureSampleType::Float { filterable: false },
            view_dimension: array,
            multisampled: false,
        };
        let layout = d.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("ocean"),
            entries: &[
                layout_entry(0, S::COMPUTE, uniform(false)),
                layout_entry(1, S::COMPUTE, storage(true)),
                layout_entry(2, S::COMPUTE, storage(true)),
                layout_entry(3, S::COMPUTE, storage(false)),
                layout_entry(4, S::COMPUTE, storage(false)),
                layout_entry(5, S::COMPUTE, written),
                layout_entry(6, S::COMPUTE, written),
                layout_entry(7, S::COMPUTE, uniform(false)),
            ],
        });
        let mip_layout = d.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("ocean mips"),
            entries: &[
                layout_entry(0, S::COMPUTE, read),
                layout_entry(1, S::COMPUTE, written),
                layout_entry(2, S::COMPUTE, read),
                layout_entry(3, S::COMPUTE, written),
            ],
        });
        let module = d.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("ocean"),
            source: wgpu::ShaderSource::Wgsl(
                [include_str!("ocean.wgsl"), anno_core::water::FFT_WGSL]
                    .concat()
                    .into(),
            ),
        });
        let main = d.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("ocean"),
            bind_group_layouts: &[Some(&layout)],
            immediate_size: 0,
        });
        let mips_layout = d.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("ocean mips"),
            bind_group_layouts: &[None, Some(&mip_layout)],
            immediate_size: 0,
        });
        let pipeline = |layout: &wgpu::PipelineLayout, entry_point: &str, key| {
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
        let texels = u64::from(N * N * SLICES);
        let buffer = |label, size: u64, usage| {
            d.create_buffer(&wgpu::BufferDescriptor {
                label: Some(label),
                size,
                usage,
                mapped_at_creation: false,
            })
        };
        let step = buffer("ocean step", 64, B::UNIFORM | B::COPY_DST);
        let h0 = buffer("ocean h0", texels * 8, B::STORAGE | B::COPY_DST);
        let omega = buffer("ocean omega", texels * 4, B::STORAGE | B::COPY_DST);
        // Two bands at a time (ocean.wgsl `temp`).
        let temp = buffer("ocean spectra", texels / 2 * 8 * 3, B::STORAGE);
        let disp = buffer("ocean displacement", texels / 2 * 4 * 3, B::STORAGE);
        let texture = |label, layers| {
            d.create_texture(&wgpu::TextureDescriptor {
                label: Some(label),
                size: wgpu::Extent3d {
                    width: N,
                    height: N,
                    depth_or_array_layers: layers,
                },
                mip_level_count: MIPS,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Rgba16Float,
                // Copied out by the tests (read_displacement).
                usage: wgpu::TextureUsages::STORAGE_BINDING
                    | wgpu::TextureUsages::TEXTURE_BINDING
                    | wgpu::TextureUsages::COPY_SRC,
                view_formats: &[],
            })
        };
        // A band's coastal and offshore slopes share a texel (ocean.wgsl).
        let (displacement, gradients) = (
            texture("ocean waves", SLICES),
            texture("ocean slopes", SLICES / 2),
        );
        let level = |t: &wgpu::Texture, mip: u32| {
            t.create_view(&wgpu::TextureViewDescriptor {
                dimension: Some(array),
                base_mip_level: mip,
                mip_level_count: Some(1),
                ..Default::default()
            })
        };
        let (d0, g0) = (level(&displacement, 0), level(&gradients, 0));
        // One group per pair of bands stepped (ocean.wgsl `part`).
        let parts = [0u32, 1].map(|part| {
            use wgpu::util::DeviceExt as _;
            let which = d.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("ocean part"),
                contents: bytemuck::cast_slice(&[part, 0, 0, 0]),
                usage: B::UNIFORM,
            });
            d.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("ocean"),
                layout: &layout,
                entries: &[
                    entry(0, step.as_entire_binding()),
                    entry(1, h0.as_entire_binding()),
                    entry(2, omega.as_entire_binding()),
                    entry(3, temp.as_entire_binding()),
                    entry(4, disp.as_entire_binding()),
                    entry(5, wgpu::BindingResource::TextureView(&d0)),
                    entry(6, wgpu::BindingResource::TextureView(&g0)),
                    entry(7, which.as_entire_binding()),
                ],
            })
        });
        let mips = (1..MIPS)
            .map(|m| {
                let views = [
                    level(&displacement, m - 1),
                    level(&displacement, m),
                    level(&gradients, m - 1),
                    level(&gradients, m),
                ];
                d.create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some("ocean mip"),
                    layout: &mip_layout,
                    entries: &[
                        entry(0, wgpu::BindingResource::TextureView(&views[0])),
                        entry(1, wgpu::BindingResource::TextureView(&views[1])),
                        entry(2, wgpu::BindingResource::TextureView(&views[2])),
                        entry(3, wgpu::BindingResource::TextureView(&views[3])),
                    ],
                })
            })
            .collect();
        let whole = |t: &wgpu::Texture| {
            t.create_view(&wgpu::TextureViewDescriptor {
                dimension: Some(array),
                ..Default::default()
            })
        };
        Self {
            vertical: pipeline(&main, "cs_vertical", "117 waves vertical"),
            horizontal: pipeline(&main, "cs_horizontal", "117 waves horizontal"),
            finish: pipeline(&main, "cs_finish", "117 waves finish"),
            mip: pipeline(&mips_layout, "cs_mip", "117 waves mip"),
            step,
            h0,
            omega,
            parts,
            mips,
            waves: displacement.clone(),
            displacement: whole(&displacement),
            gradients: whole(&gradients),
            key: None,
            time: None,
        }
    }

    /// Steps the waves to `time` seconds, rebuilding the spectra first when
    /// `ocean`'s waves changed. A second call for the same time (another
    /// view in the same frame) does nothing.
    pub fn step(
        &mut self,
        gpu: &Gpu,
        encoder: &mut wgpu::CommandEncoder,
        ocean: &Ocean,
        time: f32,
    ) {
        let key = ocean.spectrum_key();
        if self.key != Some(key) {
            let (h0, omega) = ocean.spectrum();
            gpu.queue
                .write_buffer(&self.h0, 0, bytemuck::cast_slice(&h0));
            // Columns first (ocean.wgsl `omega`): the transform reads a
            // column's down its rows, which side by side in memory spares
            // it a cache line a value.
            let n = N as usize;
            let columns: Vec<f32> = (0..omega.len())
                .map(|i| {
                    let (slice, at) = (i / (n * n), i % (n * n));
                    omega[slice * n * n + (at % n) * n + at / n]
                })
                .collect();
            gpu.queue
                .write_buffer(&self.omega, 0, bytemuck::cast_slice(&columns));
            self.key = Some(key);
            self.time = None;
        }
        if self.time == Some(time) {
            return;
        }
        self.time = Some(time);
        let mut uniforms = [0f32; 16];
        uniforms[0] = time;
        for (i, (waves, _)) in ocean.slices().enumerate() {
            uniforms[4 + i] = waves.choppiness;
        }
        for (i, band) in BANDS.iter().enumerate() {
            uniforms[12 + i] = band.0;
        }
        gpu.queue
            .write_buffer(&self.step, 0, bytemuck::cast_slice(&uniforms));
        let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
            label: Some("ocean"),
            timestamp_writes: None,
        });
        // A pair of bands at a time through the scratch: their four
        // slices' three components.
        for group in &self.parts {
            pass.set_bind_group(0, group, &[]);
            pass.set_pipeline(&self.vertical);
            pass.dispatch_workgroups(N, SLICES / 2, 3);
            pass.set_pipeline(&self.horizontal);
            pass.dispatch_workgroups(N, SLICES / 2, 3);
            pass.set_pipeline(&self.finish);
            pass.dispatch_workgroups(N / 16, N / 16, SLICES / 2);
        }
        pass.set_pipeline(&self.mip);
        for (m, group) in self.mips.iter().enumerate() {
            let side = N >> (m + 1);
            pass.set_bind_group(1, group, &[]);
            pass.dispatch_workgroups(side.div_ceil(8), side.div_ceil(8), SLICES);
        }
    }

    /// The displacement as last stepped, from its texture's top level:
    /// [component][slice][row][column], metres, at half-float precision.
    #[cfg(test)]
    pub fn read_displacement(&self, gpu: &Gpu) -> Vec<f32> {
        let n = N as usize;
        let row = u64::from(N) * 8;
        let readback = gpu.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("ocean readback"),
            size: row * u64::from(N * SLICES),
            usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let mut encoder = gpu.device.create_command_encoder(&Default::default());
        encoder.copy_texture_to_buffer(
            self.waves.as_image_copy(),
            wgpu::TexelCopyBufferInfo {
                buffer: &readback,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(row as u32),
                    rows_per_image: Some(N),
                },
            },
            wgpu::Extent3d {
                width: N,
                height: N,
                depth_or_array_layers: SLICES,
            },
        );
        gpu.queue.submit([encoder.finish()]);
        readback.slice(..).map_async(wgpu::MapMode::Read, |_| {});
        gpu.device
            .poll(wgpu::PollType::wait_indefinitely())
            .unwrap();
        let mapped = readback.slice(..).get_mapped_range().expect("mapped");
        let halves: &[u16] = bytemuck::cast_slice(&mapped);
        let float = |h: u16| {
            let (sign, exp, frac) = (h >> 15, (h >> 10) & 0x1f, f32::from(h & 0x3ff));
            let v = match exp {
                0 => frac * 2f32.powi(-24),
                _ => (1. + frac / 1024.) * 2f32.powi(i32::from(exp) - 15),
            };
            if sign == 1 { -v } else { v }
        };
        let texels = n * n * SLICES as usize;
        (0..3 * texels)
            .map(|i| float(halves[(i % texels) * 4 + i / texels]))
            .collect()
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// amb_roman_01's `<Water>`.
    pub(crate) fn roman() -> Ocean {
        Ocean {
            coastal: Waves {
                fetch: 16000.,
                wind_speed: 1.,
                choppiness: 1.,
                directionality: 0.441,
                depth: 100.,
            },
            offshore: Waves {
                fetch: 120_000.,
                wind_speed: 3.,
                choppiness: 1.,
                directionality: 0.657,
                depth: 100.,
            },
            seed: 35,
            wind: [1., 0.],
            offshore_distance: 2.,
            offshore_transition: 50.,
            caustics: 0.351,
            deep: [0.058_824, 0.188_235, 0.286_274],
            shallow: [0.756_863, 0.949_02, 1.],
            scatter: [0.129_412, 0.337_255, 0.4],
            scatter_strength: 0.18,
            absorption_shallow: 0.6,
            shallow_depth: 0.31,
            absorption_smoothness: 4.,
            view_depth: 9.,
            reflection: 1.,
            coast: None,
            maps: None,
            foam: None,
            noise: None,
            time: Some(0.),
        }
    }

    // Each band keeps only its own wavenumbers. A 3 m/s offshore wind over
    // 120 km peaks near 29 m waves (ωp 1.46 rad/s), in band 2.
    #[test]
    fn bands_split_the_spectrum() {
        let (h0, _) = roman().spectrum();
        let n = N as usize;
        let energy = |z: usize| -> f32 {
            h0[z * n * n..(z + 1) * n * n]
                .iter()
                .map(|c| c[0] * c[0] + c[1] * c[1])
                .sum()
        };
        let bands: Vec<f32> = (4..8).map(energy).collect();
        assert!(bands.iter().all(|&e| e <= bands[2]), "{bands:?}");
        let delta = std::f32::consts::TAU / BANDS[2].0;
        let z = 6;
        for y in 0..n {
            for x in 0..n {
                let k = ((x as f32 - 128.).powi(2) + (y as f32 - 128.).powi(2)).sqrt() * delta;
                if k < BANDS[2].1 {
                    assert_eq!(h0[(z * n + y) * n + x], [0.; 2]);
                }
            }
        }
    }

    // The GPU's transform against the definition, D(x, y) = (-1)^(x+y) ·
    // Re Σ Ht(i, j) e^(-2πi(ix + jy)/N), at a few texels of offshore band 0.
    #[test]
    fn the_gpu_transform_is_the_games_dft() {
        let target = gpui_editor_kit::gpu::Offscreen::new(4, 4).unwrap();
        let gpu = &Gpu {
            device: target.device().clone(),
            queue: target.queue().clone(),
        };
        let ocean = roman();
        let mut sim = Simulation::new(gpu);
        let mut encoder = gpu.device.create_command_encoder(&Default::default());
        sim.step(gpu, &mut encoder, &ocean, 3.5);
        gpu.queue.submit([encoder.finish()]);
        let got = sim.read_displacement(gpu);
        let (h0, omega) = ocean.spectrum();
        let n = N as usize;
        let z = 4;
        let at = |row: usize, col: usize| (z * n + row) * n + col;
        // csUpdateWaveSpectrum's Dy at column i, row j.
        let ht = |i: usize, j: usize| -> [f64; 2] {
            let k = h0[at(i, j)].map(f64::from);
            let mk = h0[at(n - 1 - i, n - 1 - j)].map(f64::from);
            let (s, c) = (f64::from(omega[at(j, i)]) * 3.5).sin_cos();
            [
                (k[0] + mk[0]) * c - (k[1] - mk[1]) * s,
                (k[0] - mk[0]) * s + (k[1] + mk[1]) * c,
            ]
        };
        let spectrum: Vec<[f64; 2]> = (0..n * n).map(|p| ht(p % n, p / n)).collect();
        let peak = (got[n * n * 8 + z * n * n..n * n * 8 + (z + 1) * n * n].iter())
            .fold(0f32, |m, v| m.max(v.abs()));
        assert!(peak > 0.05, "offshore swell moves the sea, {peak} m");
        for (x, y) in [(0usize, 0usize), (5, 9), (128, 77), (255, 255), (31, 200)] {
            let mut sum = 0f64;
            for j in 0..n {
                for i in 0..n {
                    let h = spectrum[j * n + i];
                    let angle = -std::f64::consts::TAU * ((i * x + j * y) % n) as f64 / n as f64;
                    sum += h[0] * angle.cos() - h[1] * angle.sin();
                }
            }
            let want = if (x + y) % 2 == 1 { -sum } else { sum };
            let have = f64::from(got[n * n * 8 + (z * n + y) * n + x]);
            assert!(
                (have - want).abs() < 1e-4 + f64::from(peak) * 1e-3,
                "texel ({x}, {y}): GPU {have}, DFT {want}"
            );
        }
    }

    // A beach along x = -40 sends rings of waves ashore: after twelve
    // seconds the water off the coast moves and foams, while open sea
    // farther out than a wave can travel in that time stays flat.
    #[test]
    fn the_coast_sends_physical_waves_ashore() {
        let target = gpui_editor_kit::gpu::Offscreen::new(4, 4).unwrap();
        let gpu = &Gpu {
            device: target.device().clone(),
            queue: target.queue().clone(),
        };
        let d = &gpu.device;
        let shader = d.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("editor"),
            source: wgpu::ShaderSource::Wgsl(
                gpui_editor_kit::gpu::ssr_module(d, &[include_str!("surface.wgsl")]).into(),
            ),
        });
        let side = 128;
        let texels = (0..side * side)
            .map(|i| {
                let x = -128. + ((i % side) as f32 + 0.5) * 2.;
                let sdf = x + 40.;
                [sdf, (sdf / 16.).clamp(0., 1.), 1., 1.]
            })
            .collect();
        let coast = Coast {
            width: side as u32,
            height: side as u32,
            origin: [-128.; 2],
            size: [256.; 2],
            texels,
            depth_scale: 4.,
        };
        let coast_view = upload_coast(gpu, &coast);
        let ocean = Ocean {
            coast: Some(Arc::new(coast)),
            ..roman()
        };
        let params = d.create_buffer(&wgpu::BufferDescriptor {
            label: None,
            size: PARAMS as u64 * 16,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        gpu.queue
            .write_buffer(&params, 0, bytemuck::cast_slice(&ocean.uniforms(0.)));
        let sampler = |mode| {
            d.create_sampler(&wgpu::SamplerDescriptor {
                address_mode_u: mode,
                address_mode_v: mode,
                mag_filter: wgpu::FilterMode::Linear,
                min_filter: wgpu::FilterMode::Linear,
                ..Default::default()
            })
        };
        let (clamp, wrap) = (
            sampler(wgpu::AddressMode::ClampToEdge),
            sampler(wgpu::AddressMode::Repeat),
        );
        let blank = state(gpu);
        let mut sim = Simulation::new(gpu);
        let mut physical = Physical::new(gpu, &shader);
        for k in 0..=180 {
            let time = k as f32 / 15.;
            let mut encoder = d.create_command_encoder(&Default::default());
            sim.step(gpu, &mut encoder, &ocean, time);
            let shared = Shared {
                params: &params,
                waves: &sim.displacement,
                slopes: &sim.gradients,
                coast: &coast_view,
                info: &blank,
                flow: &blank,
                noise: &blank,
                clamp: &clamp,
                wrap: &wrap,
            };
            physical.step(gpu, &mut encoder, &shared, false, [0.; 2], time);
            gpu.queue.submit([encoder.finish()]);
        }
        let state = read_state(gpu, physical.current().unwrap());
        let origin = physical.origin.unwrap();
        let n = PHYSICAL_N as usize;
        let band = |from: f32, to: f32| {
            let column = |x: f32| ((x - origin[0]) / PHYSICAL_TEXEL) as usize;
            (0..n).flat_map(move |row| (column(from)..column(to)).map(move |c| row * n + c))
        };
        let most = |from, to, k: usize| {
            band(from, to)
                .map(|i| state[i][k].abs())
                .fold(0f32, f32::max)
        };
        let near = most(-38., -22., 0);
        let far = most(40., 60., 0);
        assert!(near > 1e-3, "waves off the coast: {near}");
        assert!(far < 1e-4, "open sea beyond reach: {far}");
        assert!(most(-40., -20., 3) > 0., "the waves leave foam");
    }

    /// A physical waves square as (h, h last, crest foam, wave foam).
    fn read_state(gpu: &Gpu, view: &wgpu::TextureView) -> Vec<[f32; 4]> {
        let texture = view.texture();
        let row = PHYSICAL_N as u64 * 8;
        let readback = gpu.device.create_buffer(&wgpu::BufferDescriptor {
            label: None,
            size: row * u64::from(PHYSICAL_N),
            usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let mut encoder = gpu.device.create_command_encoder(&Default::default());
        encoder.copy_texture_to_buffer(
            texture.as_image_copy(),
            wgpu::TexelCopyBufferInfo {
                buffer: &readback,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(row as u32),
                    rows_per_image: None,
                },
            },
            texture.size(),
        );
        gpu.queue.submit([encoder.finish()]);
        readback.slice(..).map_async(wgpu::MapMode::Read, |_| {});
        gpu.device
            .poll(wgpu::PollType::wait_indefinitely())
            .unwrap();
        let mapped = readback.slice(..).get_mapped_range().expect("mapped");
        let half = |h: u16| {
            let m = f32::from(h & 0x3ff) / 1024.;
            let e = i32::from((h >> 10) & 31);
            let v = if e == 0 {
                m * 2f32.powi(-14)
            } else {
                (1. + m) * 2f32.powi(e - 15)
            };
            if h & 0x8000 != 0 { -v } else { v }
        };
        bytemuck::cast_slice::<u8, [u16; 4]>(&mapped)
            .iter()
            .map(|t| t.map(half))
            .collect()
    }
}
