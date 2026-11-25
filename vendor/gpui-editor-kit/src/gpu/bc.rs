// Block compression of images the GPU bakes (bc.wgsl): ground pages and
// impostor views drawn as RGBA8 or RG8 and kept as BC3 and BC5, a quarter
// and a half of those sizes. Jobs encode into rows of a buffer, which each
// job then copies into its place in a BC texture.
use super::{entry, layout_entry, sampled, storage};
use wgpu::util::DeviceExt;

/// What a job encodes (bc.wgsl `Job::kind`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Kind {
    /// RGBA as BC3.
    Rgba = 0,
    /// Red and green as BC5.
    Rg = 1,
}

/// A rectangle of one layer's mip to encode: texels from `origin`, whole
/// blocks across and down.
#[derive(Clone, Copy, Debug)]
pub(super) struct Job {
    pub origin: [u32; 2],
    pub layer: u32,
    pub mip: u32,
    pub blocks: [u32; 2],
    pub kind: Kind,
}

impl Job {
    /// Bytes between rows of blocks: copies take rows 256-byte aligned.
    pub fn pitch(&self) -> u32 {
        (self.blocks[0] * 16).next_multiple_of(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT)
    }

    pub fn bytes(&self) -> u64 {
        u64::from(self.pitch()) * u64::from(self.blocks[1])
    }

    /// Copies the job's blocks, encoded at `offset` of `buffer`, to
    /// `origin` of `texture`'s `mip`, layer `layer`.
    pub fn copy(
        &self,
        encoder: &mut wgpu::CommandEncoder,
        buffer: &wgpu::Buffer,
        offset: u64,
        texture: &wgpu::Texture,
        mip: u32,
        origin: [u32; 2],
        layer: u32,
    ) {
        encoder.copy_buffer_to_texture(
            wgpu::TexelCopyBufferInfo {
                buffer,
                layout: wgpu::TexelCopyBufferLayout {
                    offset,
                    bytes_per_row: Some(self.pitch()),
                    rows_per_image: None,
                },
            },
            wgpu::TexelCopyTextureInfo {
                texture,
                mip_level: mip,
                origin: wgpu::Origin3d {
                    x: origin[0],
                    y: origin[1],
                    z: layer,
                },
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::Extent3d {
                width: self.blocks[0] * 4,
                height: self.blocks[1] * 4,
                depth_or_array_layers: 1,
            },
        );
    }
}

#[derive(Clone)]
pub(super) struct Encoder {
    layout: wgpu::BindGroupLayout,
    pipeline: wgpu::ComputePipeline,
}

impl Encoder {
    pub fn new(device: &wgpu::Device) -> Self {
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("editor block compression"),
            source: wgpu::ShaderSource::Wgsl(include_str!("bc.wgsl").into()),
        });
        let compute = wgpu::ShaderStages::COMPUTE;
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("editor block compression"),
            entries: &[
                layout_entry(
                    0,
                    compute,
                    sampled(false, wgpu::TextureViewDimension::D2Array),
                ),
                layout_entry(1, compute, storage(true)),
                layout_entry(2, compute, storage(false)),
            ],
        });
        let pipeline = device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
            label: Some("editor block compression"),
            layout: Some(
                &device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                    label: Some("editor block compression"),
                    bind_group_layouts: &[Some(&layout)],
                    immediate_size: 0,
                }),
            ),
            module: &module,
            entry_point: Some("cs_encode"),
            compilation_options: Default::default(),
            cache: None,
        });
        Self { layout, pipeline }
    }

    /// Encodes `jobs` from `source`, a `D2Array` view, into `out` one
    /// after another from `at`. Returns each job's offset, for `Job::copy`.
    #[allow(clippy::too_many_arguments)]
    pub fn encode(
        &self,
        device: &wgpu::Device,
        encoder: &mut wgpu::CommandEncoder,
        source: &wgpu::TextureView,
        jobs: &[Job],
        out: &wgpu::Buffer,
        at: u64,
        timestamp_writes: Option<wgpu::ComputePassTimestampWrites<'_>>,
    ) -> Vec<u64> {
        let mut offsets = Vec::with_capacity(jobs.len());
        let mut words: Vec<[u32; 12]> = Vec::with_capacity(jobs.len());
        let mut offset = at;
        let mut most = [0u32; 2];
        for job in jobs {
            offsets.push(offset);
            words.push([
                job.origin[0],
                job.origin[1],
                job.layer,
                job.mip,
                job.blocks[0],
                job.blocks[1],
                (offset / 4) as u32,
                job.pitch() / 4,
                job.kind as u32,
                0,
                0,
                0,
            ]);
            most = [most[0].max(job.blocks[0]), most[1].max(job.blocks[1])];
            offset += job.bytes();
        }
        assert!(offset <= out.size(), "block compression output overflows");
        if jobs.is_empty() {
            return offsets;
        }
        let buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("editor block compression jobs"),
            contents: bytemuck::cast_slice(&words),
            usage: wgpu::BufferUsages::STORAGE,
        });
        let group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("editor block compression"),
            layout: &self.layout,
            entries: &[
                entry(0, wgpu::BindingResource::TextureView(source)),
                entry(1, buffer.as_entire_binding()),
                entry(2, out.as_entire_binding()),
            ],
        });
        let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
            label: Some("editor block compression"),
            timestamp_writes,
        });
        pass.set_pipeline(&self.pipeline);
        pass.set_bind_group(0, &group, &[]);
        pass.dispatch_workgroups(most[0].div_ceil(8), most[1].div_ceil(8), jobs.len() as u32);
        offsets
    }
}
