// Owns 117's sky as the island view draws it from the camera (pass.wgsl):
// the shown preset's atmosphere, sun and moon, the transmittance and
// multiple scattering tables uploaded for it, its night cube, the moon's
// texture and its clouds (clouds.rs), and the two passes, the backdrop at
// the Sky stage and the haze at Blend, sharing that state.
// Engine addresses here are Anno117.exe build 24988830 (Steam).
use super::clouds::{self, CloudGpu, Clouds, VolumeFiles};
use super::{AMBIENT, Atmosphere, MULTISCATTER, PresetSky, SUN_DISC, TRANSMITTANCE};
use anno_core::ambient::Sky;
use anno_core::cube::Cube;
use anno_core::water::half;
use anno_formats::vfs::Install;
use gpui_editor_kit::gpu::LockAny as _;
use gpui_editor_kit::gpu::{self, Pass, Stage};
use gpui_editor_kit::texture::Dds;
use std::sync::mpsc::{Receiver, TryRecvError};
use std::sync::{Arc, Mutex};

/// The uniform as pass.wgsl's `Air`: sixteen vec4.
type Air = [[f32; 4]; 16];

/// psMoon's texture (cMoonMap). TODO: which of the two shipped moon
/// textures the engine binds is not traced; they are the same size.
const MOON: &str = "data/base/graphics/engine/sky/moon_0.dds";

/// AtmosphereScatteringFactor's cbEnvironment fields, cScatterFadeSunY to
/// cScatterFadeScatterBlendDist. The environment renderer's constructor
/// (Anno117.exe 0x142480E70) sets them at +0x2F8..+0x310 and its constant
/// update (0x14242C1D0) copies them, in order, to the cbuffer's fields at
/// 0x78..0x90. Each frame the scatter distance (+0x30C, 100 here) is
/// replaced by the preset's `NearFadeDist` (`Atmosphere::near_fade`). The
/// distances are world units. So near the camera, with the light above
/// 0.4 and the view below -0.5, only 5% of the air scatters, back to full
/// 20 units either side of NearFadeDist (40 to 80 for amb_celtic_01).
const SUN_Y: f32 = 0.35;
const SUN_Y_BLEND: f32 = 0.1;
const RAY_Y: f32 = -0.4;
const RAY_Y_BLEND: f32 = 0.2;
const MIN_SCATTER: f32 = 0.05;
const SCATTER_BLEND_DIST: f32 = 40.;
/// cAtmosphereStarLight: globals.xml's `StarStrength` (0.512), which the
/// sky renderer reads (0x14AE8B7A0) into the field it copies to cbAtmosphere
/// (0x1424FDBA0); 0.2 when the file has none. The shader's own
/// ATMOSPHERE_STAR_LIGHT (0.0005) is for ambient light, not the stars.
/// TODO: read globals.xml rather than hold its value.
const STAR_LIGHT: f32 = 0.512;
/// camera.xml's highest island preset (20, 175 units): sky and haze are
/// drawn as from no higher than this, as far as the game's camera goes.
/// The editor zooms far higher, through air the game never shows. Earlier
/// this held at the default preset (13, 60 units): from 175 the island sat
/// under a blue veil then, which retail screenshots do not show. With the
/// clouds' and the sky's light and the day path since matched, from 175 the
/// near ground stays clear and the distance hazes to the pale blue the game
/// shows (Colony02 at noon, far grass 133 123 127 in game, 104 85 66 held
/// at 60).
const AIR_CEILING: f32 = 175.;

/// 117's sky passes for one island view (`Module::sky`).
pub struct SkyPasses {
    shared: Arc<Shared>,
    backdrop: Arc<dyn Pass>,
    haze: Arc<dyn Pass>,
}

impl SkyPasses {
    /// Reads the moon's texture and the cloud volumes' files from
    /// `install`; the volumes decode with the pipelines, off this thread.
    pub fn new(install: &Install) -> Self {
        let moon = install
            .read(MOON)
            .ok()
            .and_then(|bytes| Dds::parse(&bytes).ok()?.decode(&bytes, 0).ok())
            .map(|t| (t.width, t.height, t.rgba));
        Self::with(moon, VolumeFiles::read(install))
    }

    fn with(moon: Option<(u32, u32, Vec<u8>)>, volumes: Option<VolumeFiles>) -> Self {
        let shared = Arc::new(Shared {
            moon,
            volumes: volumes.map(Arc::new),
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

/// What `show` set: the atmosphere the tables are for and the ground it
/// sees, the uniform, the night cube and the clouds.
struct Shown {
    atmosphere: Atmosphere,
    ground: [f32; 3],
    air: Air,
    night: Option<Arc<Cube>>,
    clouds: Option<Clouds>,
    /// How dark the clouds' shade is at this hour
    /// (`Clouds::shadow_intensity`).
    shade: f32,
}

#[derive(Default)]
struct Shared {
    shown: Mutex<Option<Shown>>,
    moon: Option<(u32, u32, Vec<u8>)>,
    /// The cloud volumes' files, for the pipeline thread to decode. Kept,
    /// not handed over: a new device after a loss decodes them again, and
    /// reading them again would need the install, which `Module::sky`
    /// only lends.
    volumes: Option<Arc<VolumeFiles>>,
    gpu: Mutex<gpu::OnDevice<Gpu>>,
    /// Pipelines (and the decoded cloud volumes) while a thread builds them.
    building: Mutex<gpu::OnDevice<Receiver<Built>>>,
    /// The tables for an atmosphere and ground while a thread computes them.
    tabling: Mutex<Option<(TableKey, Receiver<Tables>)>>,
    /// What the editor's Graphics pane leaves out (`SkyPass::detail`).
    detail: Mutex<anno_core::ambient::Detail>,
}

#[derive(Clone)]
struct Pipelines {
    backdrop: wgpu::RenderPipeline,
    /// The backdrop with the air from the haze's target (fs_backdrop_up).
    backdrop_up: wgpu::RenderPipeline,
    /// The haze at quarter size into `HazeTarget`, and composited from it.
    haze: wgpu::RenderPipeline,
    /// fs_haze_up, the haze and the clouds below the horizon over what the
    /// scene drew, by what the Graphics pane shows: both, haze, clouds.
    haze_up: [wgpu::RenderPipeline; 3],
    /// fs_haze_fallback over fs_haze's list of blocks, with and without the
    /// clouds.
    haze_fallback: [wgpu::RenderPipeline; 2],
    /// The same two that apply the ambient occlusion too (occluded.wgsl),
    /// where the device blends two sources: drawn when the kit hands it
    /// over, the plain ones otherwise.
    occluded: Option<([wgpu::RenderPipeline; 3], [wgpu::RenderPipeline; 2])>,
    haze_layout: wgpu::BindGroupLayout,
    blocks_layout: wgpu::BindGroupLayout,
    /// clouds.wgsl's march of a quarter of the cloud map, and the pass
    /// that fills the map from it and the last.
    clouds: [wgpu::RenderPipeline; 2],
    /// The clouds' shade on the ground (fs_cloud_shade).
    cloud_shade: wgpu::RenderPipeline,
    layout: wgpu::BindGroupLayout,
    cloud_layout: wgpu::BindGroupLayout,
    cloud_resolve_layout: wgpu::BindGroupLayout,
    empty_layout: wgpu::BindGroupLayout,
}

type TableKey = (Atmosphere, [f32; 3]);

/// What the pipeline thread hands back.
/// The cloud volumes are on the GPU already: made on the thread, filling
/// them on the frame held the UI thread 14 ms.
type Built = (Pipelines, Option<CloudGpu>);

/// Half floats, RGBA: the transmittance and multiple-scattering tables;
/// and `Atmosphere::ambient`.
type Tables = (Vec<u16>, Vec<u16>, Vec<[f32; 3]>);

struct Gpu {
    pipelines: Pipelines,
    buffer: wgpu::Buffer,
    sampler: wgpu::Sampler,
    moon: wgpu::TextureView,
    /// The tables for an atmosphere and ground, and its ambient table.
    tables: Option<(
        TableKey,
        wgpu::TextureView,
        wgpu::TextureView,
        Vec<[f32; 3]>,
    )>,
    /// The night cube, or a black one.
    night: (Option<Arc<Cube>>, wgpu::TextureView),
    /// The group over the current tables, cube and cloud map, and the same
    /// with a blank map, which the march draws into the map with.
    group: Option<(wgpu::BindGroup, wgpu::BindGroup)>,
    /// The clouds, when their volumes decoded.
    clouds: Option<CloudGpu>,
    blank: wgpu::TextureView,
    cloud_sampler: wgpu::Sampler,
    empty: wgpu::BindGroup,
    /// The quarter-size haze of the view being drawn: views draw one after
    /// another, each marching into it before compositing it.
    haze: Option<HazeTarget>,
    /// The cloud shade the view being drawn is lit by.
    shade: Option<gpu::CloudShade>,
}

/// fs_haze's quarter-size target for a view target of `size`, and the group
/// fs_haze_up reads it through.
struct HazeTarget {
    size: [u32; 2],
    view: wgpu::TextureView,
    group: wgpu::BindGroup,
    /// fs_haze's list of blocks that may fall back, the indirect
    /// draw over them, what resets that each view, and the group writing
    /// both.
    blocks: wgpu::Buffer,
    blocks_args: wgpu::Buffer,
    blocks_reset: wgpu::Buffer,
    blocks_group: wgpu::BindGroup,
    /// Marched this frame, for the Blend stage to composite.
    ready: bool,
}

/// The haze is marched at a sixteenth of the pixels: at 5K the full-size
/// march was 1.4 ms of the frame, a quarter of them 0.69, a sixteenth 0.19
/// with the composite no dearer.
const HAZE_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba16Float;
/// Full-size pixels per side of a haze texel (pass.wgsl HAZE_SCALE).
const HAZE_SCALE: u32 = 4;

impl anno_core::ambient::SkyPass for SkyPasses {
    fn show(&self, sky: Option<&Arc<dyn Sky>>, hours: f32) {
        let preset = sky.and_then(|s| s.as_any().downcast_ref::<PresetSky>());
        let moon = self.shared.moon.is_some();
        *self.shared.shown.lock_any() = preset.map(|p| Shown {
            atmosphere: p.atmosphere,
            ground: p.ground,
            air: air(p, hours, moon),
            night: p.night.clone(),
            clouds: p.clouds.clone(),
            shade: (p.clouds.as_ref()).map_or(0., |c| c.shadow_intensity(p.path.progress(hours))),
        });
    }

    fn passes(&self) -> Vec<Arc<dyn Pass>> {
        vec![self.backdrop.clone(), self.haze.clone()]
    }

    fn detail(&self, detail: anno_core::ambient::Detail) {
        *self.shared.detail.lock_any() = detail;
    }

    fn hazes(&self) -> bool {
        self.shared.shown.lock_any().is_some()
    }

    // The pipelines and the shown atmosphere's tables in, the clouds
    // marched out for a still view and their shade drawn.
    fn settled(&self) -> bool {
        let clouds_on = self.shared.detail.lock_any().clouds;
        let key = {
            let shown = self.shared.shown.lock_any();
            let Some(shown) = shown.as_ref() else {
                return true;
            };
            (shown.atmosphere, shown.ground)
        };
        let gpu = self.shared.gpu.lock_any();
        let Some(gpu) = gpu.last() else {
            return false;
        };
        gpu.tables.as_ref().is_some_and(|t| t.0 == key)
            && gpu
                .clouds
                .as_ref()
                .is_none_or(|c| c.shade_drawn() && (!clouds_on || !c.settling()))
    }
}

/// The uniform for `p` at `hours`, in kilometres; `moon` when its texture
/// was read.
fn air(p: &PresetSky, hours: f32, moon: bool) -> Air {
    let a = &p.atmosphere;
    let at = p.sun_moon(hours);
    let km = |m: f64| (m / 1000.) as f32;
    let per_km = |c: f64| (c * 1000.) as f32;
    let night = at.night();
    let moon_light = a.moon.map(|c| (c * a.moon_sky * night) as f32);
    let day = at.sun[1] > 0.;
    // -cLgtDirectionUncapped.y: the sun's height by day, the moon's by night.
    let light_y = if day { at.sun[1] } else { at.moon[1] };
    [
        [km(a.bottom), km(a.top), a.scale as f32, AIR_CEILING],
        [
            per_km(a.rayleigh[0]),
            per_km(a.rayleigh[1]),
            per_km(a.rayleigh[2]),
            km(a.rayleigh_height),
        ],
        [
            per_km(a.mie_scattering),
            per_km(a.mie_absorption),
            km(a.mie_height),
            a.mie_g as f32,
        ],
        [
            per_km(a.ozone[0]),
            per_km(a.ozone[1]),
            per_km(a.ozone[2]),
            km(a.ozone_peak),
        ],
        [
            km(a.ozone_width),
            km(a.custom_height),
            km(a.custom_width),
            a.custom_strength as f32,
        ],
        a.custom_scattering.map(per_km).with_w(0.),
        a.custom_absorption.map(per_km).with_w(0.),
        at.sun.with_w(0.),
        a.sun.map(|c| c as f32).with_w(0.),
        at.moon.with_w(0.),
        moon_light.with_w(if moon { 1. } else { 0. }),
        a.sun
            .map(|c| if day { (SUN_DISC * c) as f32 } else { 0. })
            .with_w(if day { 1. } else { 0. }),
        [
            SUN_Y - 0.5 * SUN_Y_BLEND,
            SUN_Y + 0.5 * SUN_Y_BLEND,
            RAY_Y + 0.5 * RAY_Y_BLEND,
            RAY_Y - 0.5 * RAY_Y_BLEND,
        ],
        [
            MIN_SCATTER,
            a.near_fade as f32 - 0.5 * SCATTER_BLEND_DIST,
            a.near_fade as f32 + 0.5 * SCATTER_BLEND_DIST,
            light_y,
        ],
        // cDayNight.y, the night ratio. TODO: the engine's own is not
        // traced; the night value stands in.
        [
            (hours / 24.).rem_euclid(1.),
            night as f32,
            STAR_LIGHT,
            if p.night.is_some() { 1. } else { 0. },
        ],
        // Whether a cloud map is baked: set as the frame prepares.
        [0.; 4],
    ]
}

trait WithW {
    fn with_w(self, w: f32) -> [f32; 4];
}

impl WithW for [f32; 3] {
    fn with_w(self, w: f32) -> [f32; 4] {
        [self[0], self[1], self[2], w]
    }
}

/// One of the two passes; both prepare the shared state, which is cheap
/// the second time. The backdrop's, first, alone marches the clouds and
/// draws their shade: the haze's ran the march again while a view settled
/// or moved, twice the clouds' time a frame for half a march's grain.
struct Part {
    shared: Arc<Shared>,
    stage: Stage,
}

impl Pass for Part {
    fn stage(&self) -> Stage {
        self.stage
    }

    fn prepare(&self, frame: &mut gpu::Prepare<'_>) -> bool {
        self.shared.prepare(frame, self.stage == Stage::Sky)
    }

    // Both parts draw with the one set.
    fn warm(&self, gpu: &gpu::Warm<'_>) {
        warm(gpu);
    }

    // While the wind blows the clouds the map is marched again each frame,
    // and while it settles.
    fn animating(&self) -> bool {
        // One lock at a time: prepare takes them in another order.
        if !self.shared.detail.lock_any().clouds {
            return false;
        }
        let blowing = (self.shared.shown.lock_any().as_ref())
            .and_then(|s| s.clouds.as_ref())
            .is_some_and(|c| c.wind > 0.);
        let gpu = self.shared.gpu.lock_any();
        let clouds = gpu.last().and_then(|g| g.clouds.as_ref());
        // And while a still view's clouds settle (CloudGpu::settling).
        clouds.is_some_and(|c| blowing || c.settling())
    }

    // The haze composite draws over every pixel: with two-source blending
    // it multiplies the occlusion in as well, while it draws at all (the
    // haze or the low clouds shown).
    fn applies_occlusion(&self) -> bool {
        if self.stage != Stage::Blend {
            return false;
        }
        let detail = *self.shared.detail.lock_any();
        let gpu = self.shared.gpu.lock_any();
        gpu.last().is_some_and(|g| {
            g.pipelines.occluded.is_some() && g.group.is_some() && g.haze.is_some()
        }) && (detail.haze || detail.clouds)
    }

    fn cloud_shade(&self) -> Option<gpu::CloudShade> {
        if self.stage != Stage::Sky {
            return None;
        }
        (self.shared.gpu.lock_any().last()).and_then(|g| g.shade.clone())
    }

    fn record(&self, frame: &gpu::Record<'_>, pass: &mut wgpu::RenderPass<'_>) {
        let mut gpu = self.shared.gpu.lock_any();
        let Some(gpu) = gpu.get_mut(frame.device) else {
            return;
        };
        let Some((group, _)) = &gpu.group else { return };
        if self.stage == Stage::Sky {
            // With this frame's haze marched, its sky texels hold the air.
            match gpu.haze.as_ref().filter(|h| h.ready) {
                Some(haze) => {
                    pass.set_pipeline(&gpu.pipelines.backdrop_up);
                    pass.set_bind_group(2, &haze.group, &[]);
                }
                None => pass.set_pipeline(&gpu.pipelines.backdrop),
            }
            pass.set_bind_group(1, group, &[]);
            pass.draw(0..3, 0..1);
            return;
        }
        // Haze off: the march still ran, for the backdrop's sky texels.
        // The clouds below the horizon draw with the haze, so not without
        // a march this frame either (a view with no pixels).
        let detail = *self.shared.detail.lock_any();
        let clouds = gpu.clouds.is_some() && detail.clouds;
        let Some(haze) = gpu.haze.as_mut().filter(|h| h.ready) else {
            return;
        };
        haze.ready = false;
        let up = match (detail.haze, clouds) {
            (true, true) => 0,
            (true, false) => 1,
            (false, true) => 2,
            (false, false) => return,
        };
        // With the occlusion when the kit handed it over.
        let (ups, fallbacks) = match &gpu.pipelines.occluded {
            Some((ups, fallbacks)) if frame.occlusion => (ups, fallbacks),
            _ => (&gpu.pipelines.haze_up, &gpu.pipelines.haze_fallback),
        };
        pass.set_pipeline(&ups[up]);
        pass.set_bind_group(1, group, &[]);
        pass.set_bind_group(2, &haze.group, &[]);
        pass.draw(0..3, 0..1);
        if detail.haze {
            pass.set_pipeline(&fallbacks[usize::from(!clouds)]);
            pass.set_vertex_buffer(0, haze.blocks.slice(..));
            pass.draw_indirect(&haze.blocks_args, 0);
        }
    }

    // The haze's quarter-size march, over the opaque depth.
    fn after_opaque(&self, frame: &mut gpu::AfterOpaque<'_>) {
        if self.stage != Stage::Blend {
            return;
        }
        let mut gpu = self.shared.gpu.lock_any();
        let Some(gpu) = gpu.get_mut(frame.device) else {
            return;
        };
        let Some((group, _)) = &gpu.group else { return };
        let k = HAZE_SCALE;
        let half = frame.size.map(|v| v.div_ceil(k).max(1));
        if gpu.haze.as_ref().is_none_or(|h| h.size != frame.size) {
            let view = frame
                .device
                .create_texture(&wgpu::TextureDescriptor {
                    label: Some("117 haze"),
                    size: wgpu::Extent3d {
                        width: half[0],
                        height: half[1],
                        depth_or_array_layers: 1,
                    },
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: wgpu::TextureDimension::D2,
                    format: HAZE_FORMAT,
                    usage: wgpu::TextureUsages::RENDER_ATTACHMENT
                        | wgpu::TextureUsages::TEXTURE_BINDING,
                    view_formats: &[],
                })
                .create_view(&Default::default());
            let group = frame.device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("117 haze"),
                layout: &gpu.pipelines.haze_layout,
                entries: &[wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&view),
                }],
            });
            use wgpu::util::DeviceExt as _;
            // One rectangle per texel at most: 9.6 MB at 5K, all of it
            // only if every block fell back.
            let blocks = frame.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("117 haze blocks"),
                size: u64::from(half[0]) * u64::from(half[1]) * 16,
                usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::VERTEX,
                mapped_at_creation: false,
            });
            let blocks_args = frame.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("117 haze blocks args"),
                size: 16,
                usage: wgpu::BufferUsages::STORAGE
                    | wgpu::BufferUsages::INDIRECT
                    | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            let blocks_reset = frame
                .device
                .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                    label: Some("117 haze blocks reset"),
                    contents: bytemuck::cast_slice(&[6u32, 0, 0, 0]),
                    usage: wgpu::BufferUsages::COPY_SRC,
                });
            let blocks_group = frame.device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("117 haze blocks"),
                layout: &gpu.pipelines.blocks_layout,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: blocks_args.as_entire_binding(),
                    },
                    wgpu::BindGroupEntry {
                        binding: 2,
                        resource: blocks.as_entire_binding(),
                    },
                ],
            });
            gpu.haze = Some(HazeTarget {
                size: frame.size,
                view,
                group,
                blocks,
                blocks_args,
                blocks_reset,
                blocks_group,
                ready: false,
            });
        }
        let haze = gpu.haze.as_mut().expect("made above");
        let [x, y, w, h] = frame.viewport;
        let (x0, y0) = ((x / k).min(half[0]), (y / k).min(half[1]));
        let (x1, y1) = (
            (x + w).div_ceil(k).min(half[0]),
            (y + h).div_ceil(k).min(half[1]),
        );
        if x1 <= x0 || y1 <= y0 {
            return;
        }
        let frame_group = frame.frame;
        // The march lists the blocks that may fall back afresh for each
        // view: 6 vertices, no instances yet.
        frame
            .encoder
            .copy_buffer_to_buffer(&haze.blocks_reset, 0, &haze.blocks_args, 0, 16);
        let (encoder, timestamp_writes) = frame.timed("haze");
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("117 haze"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &haze.view,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    // No light, all of the surface: where nothing hazes.
                    load: wgpu::LoadOp::Clear(wgpu::Color {
                        r: 0.,
                        g: 0.,
                        b: 0.,
                        a: 1.,
                    }),
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: None,
            timestamp_writes,
            occlusion_query_set: None,
            multiview_mask: None,
        });
        pass.set_scissor_rect(x0, y0, x1 - x0, y1 - y0);
        pass.set_pipeline(&gpu.pipelines.haze);
        pass.set_bind_group(0, frame_group, &[]);
        pass.set_bind_group(1, group, &[]);
        pass.set_bind_group(2, &haze.blocks_group, &[]);
        pass.draw(0..3, 0..1);
        drop(pass);
        haze.ready = true;
    }
}

fn texture(
    frame: &gpu::Prepare<'_>,
    label: &str,
    size: [u32; 3],
    format: wgpu::TextureFormat,
    texel_bytes: u32,
    data: &[u8],
) -> wgpu::Texture {
    let texture = frame.device.create_texture(&wgpu::TextureDescriptor {
        label: Some(label),
        size: wgpu::Extent3d {
            width: size[0],
            height: size[1],
            depth_or_array_layers: size[2],
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    frame.queue.write_texture(
        texture.as_image_copy(),
        data,
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(size[0] * texel_bytes),
            rows_per_image: Some(size[1]),
        },
        wgpu::Extent3d {
            width: size[0],
            height: size[1],
            depth_or_array_layers: size[2],
        },
    );
    texture
}

/// `cube`'s faces as a half-float cube texture, or a black one.
fn cube_view(frame: &gpu::Prepare<'_>, cube: Option<&Cube>) -> wgpu::TextureView {
    // A face a thread: through iterators on the frame, the conversion held
    // the UI thread 16 ms on the sky's first frame.
    let (side, texels) = match cube {
        Some(c) => {
            let face = (c.side() * c.side()) as usize * 4;
            let mut texels = vec![0u16; face * 6];
            std::thread::scope(|s| {
                for (out, f) in texels.chunks_mut(face).zip(c.faces()) {
                    s.spawn(move || {
                        for (o, t) in out.as_chunks_mut::<4>().0.iter_mut().zip(f) {
                            *o = t.map(half);
                        }
                    });
                }
            });
            (c.side(), texels)
        }
        None => (1, vec![0; 24]),
    };
    let t = texture(
        frame,
        "117 night sky",
        [side, side, 6],
        wgpu::TextureFormat::Rgba16Float,
        8,
        bytemuck::cast_slice(&texels),
    );
    t.create_view(&wgpu::TextureViewDescriptor {
        dimension: Some(wgpu::TextureViewDimension::Cube),
        ..Default::default()
    })
}

impl Shared {
    /// Brings the shared resources up for this frame; `marches` marches the
    /// cloud map and draws the shade too.
    fn prepare(&self, frame: &mut gpu::Prepare<'_>, marches: bool) -> bool {
        let shown = self.shown.lock_any();
        // Nothing shown (`show(None)`: an island naming a preset the game
        // doesn't ship): ready and drawing nothing, so the kit's backdrop
        // shows and the view settles. Still loading, it kept frames coming
        // for as long as the island was open. The last preset's groups go,
        // so nothing draws them; the next preset builds them again.
        let Some(shown) = shown.as_ref() else {
            if let Some(gpu) = self.gpu.lock_any().last_mut() {
                gpu.group = None;
                gpu.shade = None;
            }
            return true;
        };
        let mut gpu = self.gpu.lock_any();
        if gpu.get(frame.device).is_none() {
            // Compiling on a thread, as the water does, and decoding the
            // cloud volumes there: the kit's backdrop shows until both are
            // in. Again on a new device after a loss.
            let mut building = self.building.lock_any();
            match building.get(frame.device).map(Receiver::try_recv) {
                Some(Ok((pipelines, clouds))) => {
                    building.clear();
                    let made = self.resources(frame, pipelines, clouds);
                    gpu.set(frame.device, made);
                }
                Some(Err(TryRecvError::Empty)) => return false,
                _ => {
                    let (tx, rx) = std::sync::mpsc::sync_channel(1);
                    let (device, queue) = (frame.device.clone(), frame.queue.clone());
                    let layout = frame.frame_layout.clone();
                    let files = self.volumes.clone();
                    let spawned =
                        std::thread::Builder::new()
                            .name("117 sky".into())
                            .spawn(move || {
                                let volumes = files.and_then(|f| match f.decode() {
                                    Ok(v) => Some(v),
                                    Err(e) => {
                                        eprintln!("117 cloud volumes: {e:#}");
                                        None
                                    }
                                });
                                let clouds = volumes.map(|v| CloudGpu::new(&device, &queue, &v));
                                let _ = tx.send((shared_pipelines(&device, &layout), clouds));
                            });
                    if spawned.is_ok() {
                        building.set(frame.device, rx);
                    }
                    return false;
                }
            }
        }
        let gpu = gpu.get_mut(frame.device).expect("built above");
        let key = (shown.atmosphere, shown.ground);
        let mut changed = false;
        if gpu.tables.as_ref().is_none_or(|(k, ..)| *k != key) {
            // The tables take a few milliseconds of every core: computed
            // on a thread, the last atmosphere's stand in meanwhile.
            let mut tabling = self.tabling.lock_any();
            let done = match tabling.as_ref() {
                Some((k, rx)) if *k == key => match rx.try_recv() {
                    Ok(tables) => Some(tables),
                    Err(TryRecvError::Empty) => None,
                    Err(TryRecvError::Disconnected) => {
                        *tabling = None;
                        None
                    }
                },
                _ => {
                    let (tx, rx) = std::sync::mpsc::sync_channel(1);
                    let spawned = std::thread::Builder::new()
                        .name("117 sky tables".into())
                        .spawn(move || {
                            let (atmosphere, ground) = key;
                            let (t, m, ambient) = atmosphere.tables(ground.map(f64::from));
                            let halves = |v: Vec<[f32; 4]>| -> Vec<u16> {
                                v.iter().flat_map(|c| c.map(half)).collect()
                            };
                            let _ = tx.send((halves(t), halves(m), ambient));
                        });
                    *tabling = spawned.ok().map(|_| (key, rx));
                    None
                }
            };
            if let Some((transmittance, multiscatter, ambient)) = done {
                *tabling = None;
                let [tw, th] = TRANSMITTANCE.map(|v| v as u32);
                let m = MULTISCATTER as u32;
                let f = wgpu::TextureFormat::Rgba16Float;
                let t = texture(
                    frame,
                    "117 sky transmittance",
                    [tw, th, 1],
                    f,
                    8,
                    bytemuck::cast_slice(&transmittance),
                );
                let s = texture(
                    frame,
                    "117 sky multiscatter",
                    [m, m, 1],
                    f,
                    8,
                    bytemuck::cast_slice(&multiscatter),
                );
                gpu.tables = Some((
                    key,
                    t.create_view(&Default::default()),
                    s.create_view(&Default::default()),
                    ambient,
                ));
                changed = true;
            }
        }
        // Held, not just its address: a cube dropped and another made in its
        // place would compare equal.
        let same = |a: &Option<Arc<Cube>>| match (a, &shown.night) {
            (Some(a), Some(b)) => Arc::ptr_eq(a, b),
            (None, None) => true,
            _ => false,
        };
        if !same(&gpu.night.0) {
            gpu.night = (
                shown.night.clone(),
                cube_view(frame, shown.night.as_deref()),
            );
            changed = true;
        }
        // The cloud map follows the view's size. TODO: two perspective
        // views of one island share it and march it in turn each frame;
        // the editor shows one.
        if let Some(c) = gpu.clouds.as_mut() {
            changed |= c.fit(frame.device, frame.size);
        }
        if (changed || gpu.group.is_none())
            && let Some(tables) = &gpu.tables
        {
            let map = gpu.clouds.as_ref().map_or(&gpu.blank, |c| c.map());
            let groups = (
                gpu.group_with(frame.device, tables, map),
                gpu.group_with(frame.device, tables, &gpu.blank),
            );
            gpu.group = Some(groups);
        }
        let Some((_, march_group)) = &gpu.group else {
            return false;
        };
        let mut air = shown.air;
        let detail = *self.detail.lock_any();
        // The clouds, marched into their map for this view when it, the
        // sun or the preset moved.
        if let (Some(c), Some(clouds), Some(eye), Some(tables)) =
            (gpu.clouds.as_mut(), &shown.clouds, frame.eye, &gpu.tables)
        {
            // Clouds off: the backdrop shows clear sky (no map baked).
            air[15][0] = if detail.clouds { 1. } else { 0. };
            // For clouds_over (pass.wgsl): the layer's base, to tell what lies under it.
            air[15][1] = clouds::DOME_RADIUS;
            frame
                .queue
                .write_buffer(&gpu.buffer, 0, bytemuck::cast_slice(&air));
            // The clouds are marched from the eye itself, through the layer
            // and above it: the editor zooms past camera.xml's highest
            // island preset (175), and the volumes are there to fly through.
            let from = [eye[0], eye[1].max(0.), eye[2]];
            let unclip = frame.unclip;
            let uniform = |time| cloud_uniform(&air, clouds, from, unclip, &tables.3, time);
            if !marches {
                return true;
            }
            // Whether or not the clouds show: their shade lights the ground.
            c.bind(
                frame.device,
                frame.queue,
                [
                    &gpu.pipelines.cloud_layout,
                    &gpu.pipelines.cloud_resolve_layout,
                ],
                clouds,
            );
            if detail.clouds {
                c.march(
                    frame,
                    [&gpu.pipelines.clouds[0], &gpu.pipelines.clouds[1]],
                    [&gpu.empty, march_group],
                    clouds,
                    uniform(frame.time),
                    &air,
                );
            }
            // The shade at the kit's clock for cloud shadows: in steps, it
            // is drawn again only as a step moves it half a texel. None
            // when cloud shadows are off.
            // TODO: two perspective views share the shade as they share
            // the map, and redraw it in turn when they look apart.
            gpu.shade = match frame.cloud_time {
                Some(time) => c.shade(
                    frame,
                    &gpu.pipelines.cloud_shade,
                    [&gpu.empty, march_group],
                    uniform(time),
                    shown.shade,
                ),
                None => None,
            };
        } else {
            gpu.shade = None;
            frame
                .queue
                .write_buffer(&gpu.buffer, 0, bytemuck::cast_slice(&air));
        }
        true
    }

    /// The uniform buffer, samplers, moon texture, a black night cube and
    /// blank cloud map, and the clouds' textures when `volumes` decoded.
    fn resources(
        &self,
        frame: &gpu::Prepare<'_>,
        pipelines: Pipelines,
        clouds: Option<CloudGpu>,
    ) -> Gpu {
        let buffer = frame.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("117 sky air"),
            size: std::mem::size_of::<Air>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let sampler = frame.device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("117 sky"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        let cloud_sampler = frame.device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("117 cloud map"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        let (w, h, rgba) = self.moon.clone().unwrap_or((1, 1, vec![0; 4]));
        let moon = texture(
            frame,
            "117 moon",
            [w, h, 1],
            wgpu::TextureFormat::Rgba8Unorm,
            4,
            &rgba,
        )
        .create_view(&Default::default());
        let blank = texture(
            frame,
            "117 no clouds",
            [1, 1, 1],
            wgpu::TextureFormat::Rgba16Float,
            8,
            bytemuck::cast_slice(&[0u16, 0, 0, half(1.)]),
        )
        .create_view(&Default::default());
        let empty = frame.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("117 empty"),
            layout: &pipelines.empty_layout,
            entries: &[],
        });
        Gpu {
            pipelines,
            buffer,
            sampler,
            moon,
            tables: None,
            night: (None, cube_view(frame, None)),
            group: None,
            clouds,
            blank,
            cloud_sampler,
            empty,
            haze: None,
            shade: None,
        }
    }
}

impl Gpu {
    /// The sky's group over `tables`, the night cube, the moon and `map`.
    fn group_with(
        &self,
        device: &wgpu::Device,
        tables: &(
            TableKey,
            wgpu::TextureView,
            wgpu::TextureView,
            Vec<[f32; 3]>,
        ),
        map: &wgpu::TextureView,
    ) -> wgpu::BindGroup {
        let view = wgpu::BindingResource::TextureView;
        device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("117 sky"),
            layout: &self.pipelines.layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: self.buffer.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: view(&tables.1),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: view(&tables.2),
                },
                wgpu::BindGroupEntry {
                    binding: 3,
                    resource: wgpu::BindingResource::Sampler(&self.sampler),
                },
                wgpu::BindGroupEntry {
                    binding: 4,
                    resource: view(&self.night.1),
                },
                wgpu::BindGroupEntry {
                    binding: 5,
                    resource: view(&self.moon),
                },
                wgpu::BindGroupEntry {
                    binding: 6,
                    resource: view(map),
                },
                wgpu::BindGroupEntry {
                    binding: 7,
                    resource: wgpu::BindingResource::Sampler(&self.cloud_sampler),
                },
            ],
        })
    }
}

/// clouds.wgsl's uniform: where the rays start (the eye), the view's
/// clip-to-world `unclip`, the light the clouds take, the clear sky's
/// light from the ambient table for the sun's and the moon's heights (as
/// AmbientRadianceClearSH[0]), and how far the clouds have drifted at `time`.
fn cloud_uniform(
    air: &Air,
    clouds: &Clouds,
    eye: [f32; 3],
    unclip: [[f32; 4]; 4],
    ambient: &[[f32; 3]],
    time: f32,
) -> clouds::Uniform {
    let drift = clouds.drift(time);
    let (sun, sun_irradiance) = (air[7], air[8]);
    let (moon, moon_irradiance) = (air[9], air[10]);
    let day = sun[1] > 0.;
    let toward = if day { sun } else { moon };
    let at = |mu: f32| -> [f32; 3] {
        let x = ((mu + 0.25) / 1.25).clamp(0., 1.) * (AMBIENT - 1) as f32;
        let (i, f) = (x.floor() as usize, x.fract());
        let j = (i + 1).min(AMBIENT - 1);
        std::array::from_fn(|c| ambient[i][c] * (1. - f) + ambient[j][c] * f)
    };
    let (a_sun, a_moon) = (at(sun[1]), at(moon[1]));
    let mut mean: [f32; 3] =
        std::array::from_fn(|c| sun_irradiance[c] * a_sun[c] + moon_irradiance[c] * a_moon[c]);
    // The cube's sky is held to MinimumSkyLight above the horizon: on the
    // all-round mean, half of that (the ground half stays dark).
    let floor = 0.5 * super::MINIMUM_SKY_LIGHT as f32;
    let length = mean.iter().map(|c| c * c).sum::<f32>().sqrt();
    if length > 0. && length < floor {
        mean = mean.map(|c| c * floor / length);
    }
    // TODO: csComputeAtmosphericLight fades the sun by cDayNight.z, not
    // traced; here over its last few degrees above the horizon.
    let strength = ((sun[1] + 0.02) / 0.07).clamp(0., 1.);
    let mean = mean.map(|c| c * clouds::SH0_OVER_MEAN);
    [
        [eye[0], eye[1], eye[2], clouds::DOME_RADIUS],
        [toward[0], toward[1], toward[2], if day { 1. } else { 0. }],
        [mean[0], mean[1], mean[2], strength],
        [
            clouds.albedo[0],
            clouds.albedo[1],
            clouds.albedo[2],
            clouds::WEATHER_SCALE,
        ],
        unclip[0],
        unclip[1],
        unclip[2],
        unclip[3],
        // The map's size, the curl's scale and the march count, filled in
        // by the march.
        [drift.weather[0], drift.weather[1], 0., 0.],
        [0.; 4],
        [drift.noise[0], drift.noise[1], drift.noise[2], 0.],
        [drift.curl[0], drift.curl[1], drift.curl[2], 0.],
        // The last march's world to clip and eye, filled in by the march.
        [0.; 4],
        [0.; 4],
        [0.; 4],
        [0.; 4],
        [0.; 4],
        // The shade's square, filled in by CloudGpu::shade.
        [0.; 4],
        // How far the clouds moved since the last march, filled in by it.
        [0.; 4],
    ]
}

/// The sky's module: the prelude, pass.wgsl and clouds.wgsl, and with
/// `occluded` the haze composite that applies the occlusion (occluded.wgsl),
/// which needs two-source blending enabled before everything.
fn source(occluded: bool) -> String {
    let mut parts = vec![
        gpu::FRAME_WGSL,
        include_str!("pass.wgsl"),
        include_str!("clouds.wgsl"),
    ];
    if occluded {
        parts.insert(0, "enable dual_source_blending;\n");
        parts.push(include_str!("occluded.wgsl"));
    }
    parts.concat()
}

/// The sky's pipelines on `device`, compiled once for every island (0.7 s
/// cold) and early by `warm`.
fn shared_pipelines(device: &wgpu::Device, frame_layout: &wgpu::BindGroupLayout) -> Pipelines {
    gpu::per_device(device, "117 sky", || pipelines(device, frame_layout))
}

/// Compiles the sky's pipelines before an island needs them.
pub fn warm(gpu: &gpu::Warm<'_>) {
    shared_pipelines(gpu.device, gpu.frame_layout);
}

fn pipelines(device: &wgpu::Device, frame_layout: &wgpu::BindGroupLayout) -> Pipelines {
    let occluded = device
        .features()
        .contains(wgpu::Features::DUAL_SOURCE_BLENDING);
    let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("117 sky"),
        source: wgpu::ShaderSource::Wgsl(source(occluded).into()),
    });
    let texture = |binding, view_dimension| wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::FRAGMENT,
        ty: wgpu::BindingType::Texture {
            sample_type: wgpu::TextureSampleType::Float { filterable: true },
            view_dimension,
            multisampled: false,
        },
        count: None,
    };
    let d2 = wgpu::TextureViewDimension::D2;
    let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("117 sky"),
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
            texture(1, d2),
            texture(2, d2),
            wgpu::BindGroupLayoutEntry {
                binding: 3,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                count: None,
            },
            texture(4, wgpu::TextureViewDimension::Cube),
            texture(5, d2),
            texture(6, d2),
            wgpu::BindGroupLayoutEntry {
                binding: 7,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                count: None,
            },
        ],
    });
    let cloud_layout = clouds::layout(device);
    // The march takes the view from its own uniform: group 0 stands empty.
    let empty_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("117 empty"),
        entries: &[],
    });
    let bake_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some("117 cloud map"),
        bind_group_layouts: &[Some(&empty_layout), Some(&layout), Some(&cloud_layout)],
        immediate_size: 0,
    });
    let cloud_resolve_layout = clouds::resolve_layout(device);
    let resolve_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some("117 cloud resolve"),
        bind_group_layouts: &[
            Some(&empty_layout),
            Some(&layout),
            Some(&cloud_resolve_layout),
        ],
        immediate_size: 0,
    });
    let target = |format| {
        Some(wgpu::ColorTargetState {
            format,
            blend: None,
            write_mask: wgpu::ColorWrites::ALL,
        })
    };
    // The map, and where its clouds lie (clouds.rs Map).
    let map = clouds::MAP_FORMAT;
    let full = |label, layout, entry_point, targets: &[Option<wgpu::ColorTargetState>]| {
        device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some(label),
            layout: Some(layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_full"),
                compilation_options: Default::default(),
                buffers: &[],
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some(entry_point),
                compilation_options: Default::default(),
                targets,
            }),
            primitive: Default::default(),
            depth_stencil: None,
            multisample: Default::default(),
            multiview_mask: None,
            cache: None,
        })
    };
    let clouds = [
        full(
            "fs_clouds_march",
            &bake_layout,
            "fs_clouds_march",
            &[target(map), target(clouds::FRESH_INFO_FORMAT)],
        ),
        full(
            "fs_clouds",
            &resolve_layout,
            "fs_clouds",
            &[target(map), target(map)],
        ),
    ];
    let cloud_shade = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("fs_cloud_shade"),
        layout: Some(&bake_layout),
        vertex: wgpu::VertexState {
            module: &shader,
            entry_point: Some("vs_full"),
            compilation_options: Default::default(),
            buffers: &[],
        },
        fragment: Some(wgpu::FragmentState {
            module: &shader,
            entry_point: Some("fs_cloud_shade"),
            compilation_options: Default::default(),
            targets: &[Some(wgpu::ColorTargetState {
                format: clouds::SHADE_FORMAT,
                blend: None,
                write_mask: wgpu::ColorWrites::ALL,
            })],
        }),
        primitive: Default::default(),
        depth_stencil: None,
        multisample: Default::default(),
        multiview_mask: None,
        cache: None,
    });
    let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some("117 sky"),
        bind_group_layouts: &[Some(frame_layout), Some(&layout)],
        immediate_size: 0,
    });
    let haze_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("117 haze"),
        entries: &[wgpu::BindGroupLayoutEntry {
            binding: 0,
            visibility: wgpu::ShaderStages::FRAGMENT,
            ty: wgpu::BindingType::Texture {
                sample_type: wgpu::TextureSampleType::Float { filterable: false },
                view_dimension: d2,
                multisampled: false,
            },
            count: None,
        }],
    });
    let storage = |binding| wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::FRAGMENT,
        ty: wgpu::BindingType::Buffer {
            ty: wgpu::BufferBindingType::Storage { read_only: false },
            has_dynamic_offset: false,
            min_binding_size: None,
        },
        count: None,
    };
    let blocks_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("117 haze blocks"),
        entries: &[storage(1), storage(2)],
    });
    let up_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some("117 haze up"),
        bind_group_layouts: &[Some(frame_layout), Some(&layout), Some(&haze_layout)],
        immediate_size: 0,
    });
    // Into the half-size target as is: light and transmittance.
    let haze = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("fs_haze"),
        layout: Some(
            &device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("117 haze"),
                bind_group_layouts: &[Some(frame_layout), Some(&layout), Some(&blocks_layout)],
                immediate_size: 0,
            }),
        ),
        vertex: wgpu::VertexState {
            module: &shader,
            entry_point: Some("vs_full"),
            compilation_options: Default::default(),
            buffers: &[],
        },
        fragment: Some(wgpu::FragmentState {
            module: &shader,
            entry_point: Some("fs_haze"),
            compilation_options: Default::default(),
            targets: &[Some(wgpu::ColorTargetState {
                format: HAZE_FORMAT,
                blend: None,
                write_mask: wgpu::ColorWrites::ALL,
            })],
        }),
        primitive: Default::default(),
        depth_stencil: None,
        multisample: Default::default(),
        multiview_mask: None,
        cache: None,
    });
    // `constants`: pass.wgsl's HAZE and CLOUDS, for the entries that read
    // them. fs_haze_fallback draws over the listed blocks (vs_block), one
    // instance each; the rest over the whole target.
    let block = [Some(wgpu::VertexBufferLayout {
        array_stride: 16,
        step_mode: wgpu::VertexStepMode::Instance,
        attributes: &wgpu::vertex_attr_array![0 => Float32x4],
    })];
    let pipeline = |entry: &str,
                    layout,
                    blend: Option<wgpu::BlendState>,
                    compare,
                    constants: &[(&str, f64)]| {
        let blocks = entry.starts_with("fs_haze_fallback");
        device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some(entry),
            layout: Some(layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some(if blocks { "vs_block" } else { "vs_full" }),
                compilation_options: Default::default(),
                buffers: if blocks { &block } else { &[] },
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some(entry),
                compilation_options: wgpu::PipelineCompilationOptions {
                    constants,
                    ..Default::default()
                },
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
    let premultiplied = wgpu::BlendState {
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
    };
    // With the occlusion: the haze plus what the target held times the
    // factor and the transmittance (occluded.wgsl's second source).
    let occluding = wgpu::BlendState {
        color: wgpu::BlendComponent {
            src_factor: wgpu::BlendFactor::One,
            dst_factor: wgpu::BlendFactor::Src1,
            operation: wgpu::BlendOperation::Add,
        },
        alpha: premultiplied.alpha,
    };
    let composites = |up: &str, fallback: &str, blend| {
        (
            [(1., 1.), (1., 0.), (0., 1.)].map(|(haze, clouds)| {
                let constants = [("HAZE", haze), ("CLOUDS", clouds)];
                pipeline(
                    up,
                    &up_layout,
                    Some(blend),
                    wgpu::CompareFunction::Always,
                    &constants,
                )
            }),
            [1., 0.].map(|clouds| {
                pipeline(
                    fallback,
                    &up_layout,
                    Some(blend),
                    wgpu::CompareFunction::Always,
                    &[("CLOUDS", clouds)],
                )
            }),
        )
    };
    let (haze_up, haze_fallback) = composites("fs_haze_up", "fs_haze_fallback", premultiplied);
    let occluded = occluded.then(|| {
        composites(
            "fs_haze_up_occluded",
            "fs_haze_fallback_occluded",
            occluding,
        )
    });
    Pipelines {
        // At z 0 against reversed depth: only where nothing drew.
        backdrop: pipeline(
            "fs_backdrop",
            &pipeline_layout,
            None,
            wgpu::CompareFunction::GreaterEqual,
            &[],
        ),
        backdrop_up: pipeline(
            "fs_backdrop_up",
            &up_layout,
            None,
            wgpu::CompareFunction::GreaterEqual,
            &[],
        ),
        haze,
        // In-scattered light plus the surface times the air's
        // transmittance (alpha), under the clouds below the horizon (seen
        // from in or over the layer): their light plus what lies behind
        // times their transmittance. The target's alpha stays.
        haze_up,
        haze_fallback,
        occluded,
        haze_layout,
        blocks_layout,
        clouds,
        cloud_shade,
        layout,
        cloud_layout,
        cloud_resolve_layout,
        empty_layout,
    }
}

#[cfg(test)]
mod tests {
    use super::super::{Atmosphere, PresetSky};
    use super::*;
    use anno_core::ambient::SkyPass as _;
    use gpui_editor_kit::camera::{Camera, Extent};
    use gpui_editor_kit::gpu::{
        Batch, Instances, Light, Material, Mesh, MeshData, Offscreen, Renderer, Scene, Tone,
    };
    use gpui_editor_kit::math;

    const PRESET: &str = "<AmbientArea><Info><UseAtmosphere>1</>\
        <PhysicalAtmosphere><MieStrength>3e-06</><Scale>20</>\
        <RayleighStrength>1e-04</><SunStrength>15</></></></>";

    fn sky() -> Arc<dyn Sky> {
        Arc::new(PresetSky {
            atmosphere: Atmosphere::read(PRESET).unwrap(),
            path: super::super::DayPath {
                min: -0.2,
                max: 0.65,
                turn: -2.443461,
            },
            ground: [0.2; 3],
            night: None,
            clouds: None,
        })
    }

    /// Flat dark ground from the camera out to 20 km, darker than the sky
    /// the haze lightens it toward.
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

    fn render(passes: Vec<Arc<dyn Pass>>, batches: Vec<Batch>, height: f32) -> (Vec<u8>, usize) {
        let (w, h) = (160, 120);
        let mut target = Offscreen::new(w, h).unwrap();
        let renderer = Renderer::default();
        renderer.set_scene(&Scene {
            batches,
            light: Light {
                sun: [0.3, 1., 0.2],
                sun_color: [3.; 3],
                sky: [0.8, 0.9, 1.1],
                ground: [0.3; 3],
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
        (target.render(&renderer, camera, world).unwrap(), w as usize)
    }

    fn luma(pixels: &[u8], w: usize, x: usize, y: usize) -> [u8; 3] {
        let i = (y * w + x) * 4;
        [pixels[i], pixels[i + 1], pixels[i + 2]]
    }

    // Drawn from the camera: a noon sky that reads blue, deeper and darker
    // from 6 km up (300 units at Scale 20) than from the ground, where the
    // baked map sees it the same from both.
    #[test]
    fn the_sky_is_seen_from_the_cameras_height() {
        let passes = SkyPasses::with(None, None);
        passes.show(Some(&sky()), 12.);
        let (low, w) = render(passes.passes(), Vec::new(), 1.);
        let (high, _) = render(passes.passes(), Vec::new(), 300.);
        let (a, b) = (luma(&low, w, 80, 10), luma(&high, w, 80, 10));
        assert!(a[2] > a[0], "the sky is blue {a:?}");
        let sum = |c: [u8; 3]| c.iter().map(|&v| u32::from(v)).sum::<u32>();
        assert!(sum(b) + 20 < sum(a), "higher is darker: {a:?} then {b:?}");
    }

    // Nothing shown (an island naming a preset the game doesn't ship):
    // the passes are ready and draw nothing, so the view settles on the
    // kit's backdrop, also after a preset was shown. Loading, the view
    // never settled (`Offscreen::settle` gave up after 20 s).
    #[test]
    fn nothing_shown_draws_the_kits_backdrop_and_settles() {
        let passes = SkyPasses::with(None, None);
        let (bare, _) = render(Vec::new(), Vec::new(), 1.);
        passes.show(None, 12.);
        assert!(render(passes.passes(), Vec::new(), 1.).0 == bare);
        passes.show(Some(&sky()), 12.);
        assert!(render(passes.passes(), Vec::new(), 1.).0 != bare);
        passes.show(None, 12.);
        assert!(render(passes.passes(), Vec::new(), 1.).0 == bare);
    }

    // The haze lightens far ground toward the sky's color and leaves the
    // ground under the camera, where the game fades it out, as it is.
    #[test]
    fn haze_grows_with_distance() {
        let passes = SkyPasses::with(None, None);
        passes.show(Some(&sky()), 12.);
        let all = passes.passes();
        // 40 units up: the rows under the horizon reach kilometres out,
        // past where the game fades scattering near the camera.
        let (clear, w) = render(vec![all[0].clone()], vec![ground()], 40.);
        let (hazed, _) = render(all, vec![ground()], 40.);
        let change = |y: usize| {
            let (a, b) = (luma(&clear, w, 80, y), luma(&hazed, w, 80, y));
            a.iter()
                .zip(b)
                .map(|(&x, y)| x.abs_diff(y) as u32)
                .sum::<u32>()
        };
        // The bottom row is ground a few units off; the first rows under
        // the horizon (where the backdrop gives way to ground) are km out.
        // Above it the sky differs by a level here and there: with the haze
        // drawn, the backdrop takes its air from the haze's quarter-size
        // march.
        let horizon = (0..120)
            .find(|&y| change(y) > 15)
            .expect("the haze shows somewhere");
        let (near, far) = (change(118), change(horizon + 1));
        assert!(far > near + 15, "near {near}, far {far}");
    }
}

// The shader parses and validates after the kit's prelude it is joined to,
// which a game pass shares names with.
#[cfg(test)]
#[test]
fn the_shader_validates_after_the_prelude() {
    for occluded in [false, true] {
        let source = source(occluded);
        let module = match wgpu::naga::front::wgsl::parse_str(&source) {
            Ok(m) => m,
            Err(e) => panic!("{}", e.emit_to_string(&source)),
        };
        use wgpu::naga::valid::{Capabilities, ValidationFlags, Validator};
        let mut validator = Validator::new(ValidationFlags::all(), Capabilities::all());
        if let Err(e) = validator.validate(&module) {
            panic!("{}", e.emit_to_string(&source));
        }
    }
}
