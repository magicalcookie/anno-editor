// Windowless rendering: a wgpu device of its own, an RGBA8 target and a
// readback buffer, for thumbnails and renderer tests.
use super::{Gpu, Renderer};
use crate::camera::{Camera, Extent};
use anyhow::{Context as _, Result, bail};
use gpui::{Bounds, Pixels, point, px, size};

const FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;

/// Draws a `Renderer`'s scene into memory instead of a window. Owns its own
/// device, so it works from any thread; renderers drawn here must not also
/// be painted in a window. Uncovered pixels stay transparent black.
pub struct Offscreen {
    pub(super) gpu: Gpu,
    target: wgpu::Texture,
    view: wgpu::TextureView,
    readback: wgpu::Buffer,
    /// Readback rows are padded to wgpu's 256-byte copy alignment.
    row_bytes: u32,
    width: u32,
    height: u32,
    /// Physical pixels per logical one, as a window's scale factor.
    scale: f32,
}

impl Offscreen {
    /// Needs a Vulkan device (any GPU or lavapipe), or DX12 on Windows,
    /// the backend GPUI's windows use there.
    pub fn new(width: u32, height: u32) -> Result<Self> {
        #[cfg(target_os = "windows")]
        let backends = wgpu::Backends::DX12;
        #[cfg(not(target_os = "windows"))]
        let backends = wgpu::Backends::VULKAN;
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends,
            ..wgpu::InstanceDescriptor::new_without_display_handle()
        });
        let adapter = pollster::block_on(instance.request_adapter(&Default::default()))
            .context("No GPU adapter for offscreen rendering")?;
        // The same optional features GPUI asks for (vendor/zed/PATCH.md), so
        // tests exercise the window's paths.
        let features = adapter.features()
            & (wgpu::Features::TEXTURE_COMPRESSION_BC
                | wgpu::Features::TIMESTAMP_QUERY
                | wgpu::Features::TIMESTAMP_QUERY_INSIDE_ENCODERS
                | super::ARENA_FEATURES);
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("offscreen"),
            required_features: features,
            required_limits: adapter.limits(),
            ..Default::default()
        }))
        .context("No GPU device for offscreen rendering")?;
        let target = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("offscreen"),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: FORMAT,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let view = target.create_view(&Default::default());
        let row_bytes = (width * 4).next_multiple_of(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT);
        let readback = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("offscreen readback"),
            size: u64::from(row_bytes * height),
            usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        Ok(Self {
            gpu: Gpu { device, queue },
            target,
            view,
            readback,
            row_bytes,
            width,
            height,
            scale: 1.,
        })
    }

    /// The scene of `renderer` through `camera`, once fully uploaded: RGBA
    /// rows, `width × height × 4` bytes.
    pub fn render(
        &mut self,
        renderer: &Renderer,
        camera: Camera,
        extent: Extent,
    ) -> Result<Vec<u8>> {
        self.aim(renderer, camera, extent);
        self.settle(renderer)
    }

    /// Draws as a window with this scale factor would: the target keeps
    /// its pixels, views and screen-size choices (detail levels, line
    /// widths) go by logical pixels. A 5K screen at scale 2 is 2560x1440.
    pub fn with_scale(mut self, scale: f32) -> Self {
        self.scale = scale.max(1e-3);
        self
    }

    /// The target in logical pixels.
    pub fn bounds(&self) -> Bounds<Pixels> {
        Bounds::new(
            point(px(0.), px(0.)),
            size(
                px(self.width as f32 / self.scale),
                px(self.height as f32 / self.scale),
            ),
        )
    }

    /// Points `renderer` at this target with `camera`.
    pub(crate) fn aim(&self, renderer: &Renderer, camera: Camera, extent: Extent) {
        self.aim_in(renderer, 0, camera, extent);
    }

    /// `aim` for viewport `slot`.
    pub(crate) fn aim_in(&self, renderer: &Renderer, slot: usize, camera: Camera, extent: Extent) {
        renderer.aim(
            slot,
            super::View {
                camera,
                extent,
                bounds: self.bounds(),
            },
        );
    }

    /// Clears, draws one frame and reads it back, whether or not the scene
    /// has finished uploading: for checking how fast a change reaches the
    /// screen. Draw with `render` first, which aims the renderer.
    pub fn frame(&mut self, renderer: &Renderer) -> Result<Vec<u8>> {
        self.frame_in(renderer, 0)
    }

    /// `frame` for viewport `slot` of `renderer`, aimed at this target.
    pub(crate) fn frame_in(&mut self, renderer: &Renderer, slot: usize) -> Result<Vec<u8>> {
        let mut encoder = self.draw(renderer, slot);
        encoder.copy_texture_to_buffer(
            self.target.as_image_copy(),
            wgpu::TexelCopyBufferInfo {
                buffer: &self.readback,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(self.row_bytes),
                    rows_per_image: None,
                },
            },
            self.target.size(),
        );
        self.submit(renderer, slot, encoder)?;
        let slice = self.readback.slice(..);
        slice.map_async(wgpu::MapMode::Read, |_| {});
        self.wait()?;
        let mut pixels = Vec::with_capacity((self.width * self.height * 4) as usize);
        {
            let data = slice.get_mapped_range().context("Offscreen readback")?;
            for row in data.chunks(self.row_bytes as usize) {
                pixels.extend_from_slice(&row[..self.width as usize * 4]);
            }
        }
        self.readback.unmap();
        Ok(pixels)
    }

    /// Draws one frame and waits for the GPU, without reading it back: for
    /// benchmarks. A window never pays for the readback in `frame`.
    pub fn time_frame(&mut self, renderer: &Renderer) -> Result<std::time::Duration> {
        let started = std::time::Instant::now();
        let encoder = self.draw(renderer, 0);
        self.submit(renderer, 0, encoder)?;
        Ok(started.elapsed())
    }

    /// Clears the target and records `slot`'s viewport into a new encoder.
    fn draw(&self, renderer: &Renderer, slot: usize) -> wgpu::CommandEncoder {
        let mut encoder = self.gpu.device.create_command_encoder(&Default::default());
        drop(encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("offscreen clear"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &self.view,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                    store: wgpu::StoreOp::Store,
                },
                depth_slice: None,
            })],
            ..Default::default()
        }));
        let scaled = self.bounds().scale(self.scale);
        renderer.draw(
            slot,
            &self.gpu,
            &mut encoder,
            &self.view,
            FORMAT,
            [self.width, self.height],
            scaled,
            scaled,
        );
        encoder
    }

    /// Submits and waits, then takes the frame's readbacks so picks and
    /// counts answer on the next call, as a window's would a frame later.
    fn submit(
        &self,
        renderer: &Renderer,
        slot: usize,
        encoder: wgpu::CommandEncoder,
    ) -> Result<()> {
        self.gpu.queue.submit([encoder.finish()]);
        self.wait()?;
        renderer.flush_readbacks(&self.gpu, slot);
        Ok(())
    }

    fn wait(&self) -> Result<()> {
        if let Err(e) = self.gpu.device.poll(wgpu::PollType::Wait {
            submission_index: None,
            timeout: Some(std::time::Duration::from_secs(10)),
        }) {
            bail!("Offscreen frame did not finish: {e}");
        }
        Ok(())
    }

    /// What is drawn under `cursor`: asks, then draws the frame that reads
    /// the answer back (see `Renderer::pick`). Aim first.
    pub fn pick(
        &mut self,
        renderer: &Renderer,
        cursor: gpui::Point<Pixels>,
    ) -> Result<Option<super::Hit>> {
        renderer.pick(0, cursor);
        self.frame(renderer)?;
        Ok(renderer.pick(0, cursor))
    }

    /// Draws until the scene is fully uploaded, then once more.
    pub(crate) fn settle(&mut self, renderer: &Renderer) -> Result<Vec<u8>> {
        // Uploads are budgeted per frame for interactive viewports (3 ms, at
        // least one item); under CPU load that can mean one mesh or texture
        // per frame. Nobody waits on these frames, so allow plenty of them.
        let started = std::time::Instant::now();
        while renderer.pending() {
            if started.elapsed().as_secs() >= 20 {
                bail!("Scene still uploading after 20 s of offscreen frames");
            }
            self.frame(renderer)?;
        }
        self.frame(renderer)
    }

    #[cfg(test)]
    pub(crate) fn pixel(&self, pixels: &[u8], p: gpui::Point<Pixels>) -> [u8; 3] {
        let (x, y) = (f32::from(p.x) as usize, f32::from(p.y) as usize);
        let i = (y * self.width as usize + x) * 4;
        [pixels[i], pixels[i + 1], pixels[i + 2]]
    }
}
