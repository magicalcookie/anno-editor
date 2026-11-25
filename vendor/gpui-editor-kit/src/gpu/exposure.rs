// Owns automatic exposure (exposure.wgsl): the two compute pipelines, and
// per view the histogram and the exposure the post pass multiplies by.
use super::{Gpu, light::AutoExposure};

/// Every second pixel each way: a quarter of the target is plenty for a
/// 64-bin histogram and keeps the pass near 0.05 ms at 1080p.
const STRIDE: u32 = 2;
/// Samples each histogram thread counts per axis (exposure.wgsl CELL).
const CELL: [u32; 2] = [4, 2];
/// Frames further apart than this take the measured exposure at once: the
/// editor draws on demand, and a view shown again after a pause should not
/// fade in from whatever it last saw.
const JUMP_AFTER: f32 = 0.1;

#[derive(Clone)]
pub(super) struct Pipelines {
    layout: wgpu::BindGroupLayout,
    histogram: wgpu::ComputePipeline,
    settle: wgpu::ComputePipeline,
}

impl Pipelines {
    pub(super) fn new(device: &wgpu::Device) -> Self {
        let storage = |binding| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::COMPUTE,
            ty: wgpu::BindingType::Buffer {
                ty: wgpu::BufferBindingType::Storage { read_only: false },
                has_dynamic_offset: false,
                min_binding_size: None,
            },
            count: None,
        };
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("editor exposure"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::COMPUTE,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::COMPUTE,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: false },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                storage(2),
                storage(3),
            ],
        });
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("editor exposure"),
            source: wgpu::ShaderSource::Wgsl(include_str!("exposure.wgsl").into()),
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("editor exposure"),
            bind_group_layouts: &[Some(&layout)],
            immediate_size: 0,
        });
        let pipeline = |entry_point| {
            device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
                label: Some("editor exposure"),
                layout: Some(&pipeline_layout),
                module: &shader,
                entry_point: Some(entry_point),
                compilation_options: Default::default(),
                cache: None,
            })
        };
        Self {
            histogram: pipeline("cs_histogram"),
            settle: pipeline("cs_exposure"),
            layout,
        }
    }
}

/// One view's histogram and exposure.
pub(super) struct Exposure {
    params: wgpu::Buffer,
    bins: wgpu::Buffer,
    /// [exposure, unused ×3]; 1 without automatic exposure.
    pub(super) value: wgpu::Buffer,
    last: Option<std::time::Instant>,
}

impl Exposure {
    pub(super) fn new(gpu: &Gpu) -> Self {
        let buffer = |label, size, usage| {
            gpu.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some(label),
                size,
                usage,
                mapped_at_creation: false,
            })
        };
        let storage = wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST;
        let exposure = Self {
            params: buffer(
                "editor exposure params",
                80,
                wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            ),
            bins: buffer("editor exposure histogram", 64 * 4, storage),
            value: buffer(
                "editor exposure",
                16,
                storage | wgpu::BufferUsages::COPY_SRC,
            ),
            last: None,
        };
        gpu.queue
            .write_buffer(&exposure.bins, 0, bytemuck::cast_slice(&[0u32; 64]));
        gpu.queue.write_buffer(
            &exposure.value,
            0,
            bytemuck::cast_slice(&[1f32, 0., 0., 0.]),
        );
        exposure
    }

    /// Measures `rect` (x, y, width, height) of `hdr` and settles this
    /// view's exposure, or puts it back to 1 without `auto`. Only the view's
    /// own rect counts: the target is window sized and other panes or stale
    /// pixels may sit outside it.
    pub(super) fn record(
        &mut self,
        gpu: &Gpu,
        pipelines: &Pipelines,
        encoder: &mut wgpu::CommandEncoder,
        hdr: &wgpu::TextureView,
        rect: [u32; 4],
        auto: Option<&AutoExposure>,
        timestamp_writes: Option<wgpu::ComputePassTimestampWrites<'_>>,
    ) {
        let Some(a) = auto else {
            if self.last.take().is_some() {
                gpu.queue
                    .write_buffer(&self.value, 0, bytemuck::cast_slice(&[1f32, 0., 0., 0.]));
            }
            return;
        };
        let now = std::time::Instant::now();
        let dt = self.last.map_or(f32::MAX, |t| (now - t).as_secs_f32());
        self.last = Some(now);
        let jump = u32::from(dt > JUMP_AFTER);
        let floats = [
            a.log_min,
            (a.log_max - a.log_min).max(1e-3),
            a.low,
            a.count,
            a.target,
            a.min,
            a.max,
            a.tolerance,
            a.speed_up,
            a.speed_down,
            dt.min(1.),
            0.,
        ];
        let mut bytes: Vec<u8> = Vec::with_capacity(80);
        bytes.extend(bytemuck::cast_slice(&[rect[2], rect[3], STRIDE, jump]));
        bytes.extend(bytemuck::cast_slice(&floats));
        bytes.extend(bytemuck::cast_slice(&[rect[0], rect[1], 0u32, 0u32]));
        gpu.queue.write_buffer(&self.params, 0, &bytes);
        let group = gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("editor exposure"),
            layout: &pipelines.layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: self.params.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(hdr),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: self.bins.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 3,
                    resource: self.value.as_entire_binding(),
                },
            ],
        });
        let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
            label: Some("editor exposure"),
            timestamp_writes,
        });
        pass.set_bind_group(0, &group, &[]);
        pass.set_pipeline(&pipelines.histogram);
        let cells = [0, 1].map(|i| rect[2 + i].div_ceil(STRIDE).div_ceil(16 * CELL[i]));
        pass.dispatch_workgroups(cells[0], cells[1], 1);
        pass.set_pipeline(&pipelines.settle);
        pass.dispatch_workgroups(1, 1, 1);
    }
}
