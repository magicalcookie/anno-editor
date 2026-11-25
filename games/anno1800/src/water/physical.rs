// Owns Anno 1800's physical waves (water_physical_waves.fx): the map that
// follows the camera, its compute step and the two textures the ocean
// reads (height, foam; normals, choppy offset). Stepped in the water
// pass's prepare after the FFT waves.
// Engine addresses here are Anno1800.exe build 16367353 (Steam).
use super::{entry, layout_entry, uniform};

/// Texels a side of the map, as the game makes it: 2048 (the render
/// settings' default, Anno1800.exe 0x141c34e23; the textures take it at
/// 0x141ec4587).
pub(super) const SIDE: u32 = 2048;
/// World units a texel: the game sets the map's extent to its width times
/// 0.125 (0x141ec6190), 256 units, and snaps its corner to eighths
/// (0x141ec6316). The wave equation moves half a texel a step, so the
/// texel sets how fast waves run: at a whole unit they ran 16 times as
/// fast as the game's with the 60 steps below.
pub(super) const TEXEL: f32 = 0.125;
/// Steps a second: the game steps when more than 1/30 s of game time has
/// passed since the last (0x141ec5475, 0.0333 over the game speed), not
/// once a frame.
const RATE: f32 = 30.;
/// Steps one frame may catch up on: a slow frame still runs the waves at
/// speed down to 3.75 frames a second; after a stall they skip ahead.
const MAX_STEPS: u32 = 8;

/// The map on the GPU: two state textures stepped into each other, the
/// normals, and where the window stands.
pub(super) struct Physical {
    pipeline: wgpu::ComputePipeline,
    /// The sea's bindings the step reads, as a compute layout.
    sea_layout: wgpu::BindGroupLayout,
    layout: wgpu::BindGroupLayout,
    params: wgpu::Buffer,
    state: [wgpu::Texture; 2],
    pub normals: wgpu::TextureView,
    /// Which state texture holds the latest step.
    latest: usize,
    /// The window's first world texel, and the last step's.
    window: Option<[i32; 2]>,
    /// Seconds the last step stood at.
    stepped: Option<f32>,
}

impl Physical {
    /// `sea` are the sea's layout entries; the step reads its uniforms,
    /// waves, coast, noise and painted maps.
    pub fn new(
        d: &wgpu::Device,
        shader: &wgpu::ShaderModule,
        sea: &[wgpu::BindGroupLayoutEntry],
    ) -> Self {
        let read = [0, 1, 2, 3, 4, 5, 7, 9, 10, 16, 17, 18];
        let sea_entries: Vec<_> = (sea.iter())
            .filter(|e| read.contains(&e.binding))
            .map(|e| wgpu::BindGroupLayoutEntry {
                visibility: wgpu::ShaderStages::COMPUTE,
                ..*e
            })
            .collect();
        let sea_layout = d.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("1800 physical waves sea"),
            entries: &sea_entries,
        });
        let compute = wgpu::ShaderStages::COMPUTE;
        let storage = wgpu::BindingType::StorageTexture {
            access: wgpu::StorageTextureAccess::WriteOnly,
            format: wgpu::TextureFormat::Rgba16Float,
            view_dimension: wgpu::TextureViewDimension::D2,
        };
        let layout = d.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("1800 physical waves"),
            entries: &[
                layout_entry(0, compute, uniform()),
                layout_entry(
                    1,
                    compute,
                    wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: false },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                ),
                layout_entry(2, compute, storage),
                layout_entry(3, compute, storage),
            ],
        });
        let pipeline_layout = d.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("1800 physical waves"),
            bind_group_layouts: &[None, Some(&sea_layout), Some(&layout)],
            immediate_size: 0,
        });
        // Once per device, as the sea's (`gpu::per_device`).
        let pipeline = gpui_editor_kit::gpu::per_device(d, "1800 physical waves", || {
            d.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
                label: Some("1800 physical waves"),
                layout: Some(&pipeline_layout),
                module: shader,
                entry_point: Some("cs_physical"),
                compilation_options: Default::default(),
                cache: None,
            })
        });
        let texture = |label| {
            d.create_texture(&wgpu::TextureDescriptor {
                label: Some(label),
                size: wgpu::Extent3d {
                    width: SIDE,
                    height: SIDE,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Rgba16Float,
                usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::STORAGE_BINDING,
                view_formats: &[],
            })
        };
        let params = d.create_buffer(&wgpu::BufferDescriptor {
            label: Some("1800 physical waves"),
            size: 2 * 16,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        Self {
            pipeline,
            sea_layout,
            layout,
            params,
            state: [
                texture("1800 physical waves a"),
                texture("1800 physical waves b"),
            ],
            normals: texture("1800 physical wave normals").create_view(&Default::default()),
            latest: 0,
            window: None,
            stepped: None,
        }
    }

    /// The latest state: height, last height, FFT-wave foam, wave foam.
    pub fn state(&self) -> wgpu::TextureView {
        self.state[self.latest].create_view(&Default::default())
    }

    /// The window's world corner and 1 / its extent, for the ocean.
    pub fn uniforms(&self) -> [f32; 4] {
        let w = self.window.unwrap_or_default();
        let extent = SIDE as f32 * TEXEL;
        [w[0] as f32 * TEXEL, w[1] as f32 * TEXEL, 1. / extent, 1.]
    }

    /// Steps the waves up to `seconds` around `focus` (world X and Z),
    /// `sea` the sea's bind group entries the step reads, `depth_scale`
    /// the bound coast map's (0 without one). Held waves (`moving` false)
    /// stay as they are.
    pub fn step(
        &mut self,
        d: &wgpu::Device,
        queue: &wgpu::Queue,
        encoder: &mut wgpu::CommandEncoder,
        sea: &[wgpu::BindGroupEntry<'_>],
        focus: [f32; 2],
        seconds: f32,
        moving: bool,
        depth_scale: f32,
    ) {
        let half = SIDE as i32 / 2;
        let window = focus.map(|v| (v / TEXEL).floor() as i32 - half);
        let was = self.window.unwrap_or(window);
        // Recentre once the view has moved an eighth of the map.
        let moved = (window[0] - was[0]).abs().max((window[1] - was[1]).abs()) > SIDE as i32 / 8;
        let now = if self.window.is_none() || moved {
            window
        } else {
            was
        };
        // Fixed steps up to `seconds`, at most MAX_STEPS: further behind,
        // the waves skip ahead. A clock that went back starts over.
        let (steps, start) = match self.stepped {
            _ if !moving => (0, seconds),
            Some(last) if seconds >= last => {
                let due = ((seconds - last) * RATE) as u32;
                if due > MAX_STEPS {
                    (MAX_STEPS, seconds - MAX_STEPS as f32 / RATE)
                } else {
                    (due, last)
                }
            }
            _ => (1, seconds - 1. / RATE),
        };
        if steps == 0 && !moved && self.window.is_some() {
            return;
        }
        let steps = steps.max(1);
        self.stepped = Some(start + steps as f32 / RATE);
        self.window = Some(now);
        let sea_group = d.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("1800 physical waves sea"),
            layout: &self.sea_layout,
            entries: &(sea.iter())
                .filter(|e| [0, 1, 2, 3, 4, 5, 7, 9, 10, 16, 17, 18].contains(&e.binding))
                .cloned()
                .collect::<Vec<_>>(),
        });
        let normals = &self.normals;
        for i in 0..steps {
            // The first step of a move reads the old window; later ones the
            // new.
            let previous = if i == 0 { was } else { now };
            let params = [
                [
                    now[0] as f32,
                    now[1] as f32,
                    previous[0] as f32,
                    previous[1] as f32,
                ],
                [
                    TEXEL,
                    start + (i + 1) as f32 / RATE,
                    SIDE as f32,
                    depth_scale,
                ],
            ];
            // Each step's uniforms go in a buffer of their own: queue
            // writes all land before the encoder runs.
            let buffer = if i == 0 {
                queue.write_buffer(&self.params, 0, bytemuck::cast_slice(&params));
                self.params.clone()
            } else {
                use wgpu::util::DeviceExt;
                d.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                    label: Some("1800 physical waves step"),
                    contents: bytemuck::cast_slice(&params),
                    usage: wgpu::BufferUsages::UNIFORM,
                })
            };
            let (from, to) = (self.latest, 1 - self.latest);
            let view = |t: &wgpu::Texture| t.create_view(&Default::default());
            let group = d.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("1800 physical waves"),
                layout: &self.layout,
                entries: &[
                    entry(0, buffer.as_entire_binding()),
                    entry(
                        1,
                        wgpu::BindingResource::TextureView(&view(&self.state[from])),
                    ),
                    entry(
                        2,
                        wgpu::BindingResource::TextureView(&view(&self.state[to])),
                    ),
                    entry(3, wgpu::BindingResource::TextureView(normals)),
                ],
            });
            let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
                label: Some("1800 physical waves"),
                timestamp_writes: None,
            });
            pass.set_pipeline(&self.pipeline);
            pass.set_bind_group(1, &sea_group, &[]);
            pass.set_bind_group(2, &group, &[]);
            pass.dispatch_workgroups(SIDE / 8, SIDE / 8, 1);
            drop(pass);
            self.latest = to;
        }
    }
}
