// Windowless rendering: the process's offscreen wgpu device, an RGBA8
// target and a readback buffer, for thumbnails and renderer tests.
use super::{Gpu, Renderer};
use crate::camera::{Camera, Extent};
use anyhow::{Context as _, Result, bail};
use gpui::{Bounds, Pixels, point, px, size};

use super::VIEW_FORMAT as FORMAT;

/// Draws a `Renderer`'s scene into memory instead of a window. Draws on the
/// process's offscreen device, not a window's, so it works from any
/// thread; renderers drawn here must not also be painted in a window.
/// Uncovered pixels stay transparent black.
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
    /// A pane of the target, in logical pixels, when drawing as one
    /// viewport of a bigger window; `None` draws the whole target.
    viewport: Option<Bounds<Pixels>>,
    /// Uploads under a window frame's time allowance too (`Budget::frame`),
    /// not by bytes alone: for timing loads as the app sees them.
    timed: bool,
}

impl Offscreen {
    /// On the device setup GPUI's windows use (gpu/device.rs).
    pub fn new(width: u32, height: u32) -> Result<Self> {
        Self::on(width, height, shared()?)
    }

    /// On a device of its own, made once per process, that no other target
    /// draws on: its allocator report counts only what this target's
    /// renderers hold, for tests of how much video memory they keep. The
    /// shared device's report counts every test drawing at the same time.
    /// One such test at a time.
    pub fn alone(width: u32, height: u32) -> Result<Self> {
        static ALONE: std::sync::OnceLock<Result<Gpu, String>> = std::sync::OnceLock::new();
        let made = ALONE.get_or_init(|| super::device::offscreen().map_err(|e| format!("{e:#}")));
        Self::on(width, height, made.clone().map_err(anyhow::Error::msg)?)
    }

    /// On a new device made for this target alone, for tests that lose it
    /// ([`Offscreen::lose`]): losing the shared one would break every test
    /// drawing on it. A handful per process: the NVIDIA driver leaked a
    /// device per target (`shared`).
    pub fn on_new_device(width: u32, height: u32) -> Result<Self> {
        Self::on(width, height, super::device::offscreen()?)
    }

    /// Destroys this target's device, as a driver reset loses one. A
    /// renderer drawn here then draws on another target's device as if
    /// nothing happened, making its GPU copies there again. Only for
    /// targets [`Offscreen::on_new_device`] made.
    pub fn lose(&self) {
        super::device::lose(&self.gpu.device);
    }

    fn on(width: u32, height: u32, gpu: Gpu) -> Result<Self> {
        let _busy = busy().context("Exiting: no new offscreen targets")?;
        let Gpu { device, queue } = gpu;
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
            viewport: None,
            timed: false,
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

    /// Draws into `viewport` (logical pixels) of the target only, as a
    /// viewport pane in a window the target's size: the renderer's own
    /// targets are then window-sized, as they are in the app. Readbacks
    /// stay whole; for benchmarks.
    pub fn with_viewport(mut self, viewport: Bounds<Pixels>) -> Self {
        self.viewport = Some(viewport);
        self
    }

    /// Uploads as a window's frames do, within a time allowance as well as
    /// a byte one, so how many frames a load takes matches the app's.
    pub fn with_window_budget(mut self) -> Self {
        self.timed = true;
        self
    }

    /// What a window device builds (`gpu::warm_passes`), on this thread:
    /// the renderer's pipelines for this target, then `passes`.
    pub fn warm(&self, passes: impl FnOnce(&super::Warm<'_>)) {
        let built = super::Pipelines::shared(&self.gpu, FORMAT);
        passes(&super::Warm {
            device: &self.gpu.device,
            queue: &self.gpu.queue,
            frame_layout: &built.kit.frame_layout,
            kit: &built.kit,
            format: FORMAT,
        });
    }

    /// The headless device, for a plugged pass's own GPU work in tests
    /// and tools (a simulation step it checks on the CPU).
    pub fn device(&self) -> &wgpu::Device {
        &self.gpu.device
    }

    pub fn queue(&self) -> &wgpu::Queue {
        &self.gpu.queue
    }

    /// What the renderer draws into, in logical pixels: the viewport pane,
    /// or the whole target.
    pub fn bounds(&self) -> Bounds<Pixels> {
        if let Some(viewport) = self.viewport {
            return viewport;
        }
        Bounds::new(
            point(px(0.), px(0.)),
            size(
                px(self.width as f32 / self.scale),
                px(self.height as f32 / self.scale),
            ),
        )
    }

    /// Points `renderer` at this target with `camera`, for `frame` and
    /// `time_frame` to draw.
    pub fn aim(&self, renderer: &Renderer, camera: Camera, extent: Extent) {
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
        let _busy = busy().context("Exiting: no more offscreen frames")?;
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
        let _busy = busy().context("Exiting: no more offscreen frames")?;
        let started = std::time::Instant::now();
        let encoder = self.draw(renderer, 0);
        self.submit(renderer, 0, encoder)?;
        Ok(started.elapsed())
    }

    /// `time_frame` split into recording on the CPU and the wait after
    /// submitting, which is the GPU's time when nothing else is queued.
    pub fn time_frame_parts(&mut self, renderer: &Renderer) -> Result<[std::time::Duration; 2]> {
        let _busy = busy().context("Exiting: no more offscreen frames")?;
        let started = std::time::Instant::now();
        let encoder = self.draw(renderer, 0);
        let recorded = started.elapsed();
        self.submit(renderer, 0, encoder)?;
        Ok([recorded, started.elapsed() - recorded])
    }

    /// `time_frame` in four: recording on the CPU, finishing and
    /// submitting the encoder, the wait for the GPU, and taking the
    /// frame's readbacks.
    pub fn time_frame_stages(&mut self, renderer: &Renderer) -> Result<[std::time::Duration; 4]> {
        let _busy = busy().context("Exiting: no more offscreen frames")?;
        let started = std::time::Instant::now();
        let encoder = self.draw(renderer, 0);
        let recorded = started.elapsed();
        self.gpu.queue.submit([encoder.finish()]);
        let submitted = started.elapsed();
        self.wait()?;
        let waited = started.elapsed();
        renderer.flush_readbacks(&self.gpu, 0);
        let done = started.elapsed();
        Ok([
            recorded,
            submitted - recorded,
            waited - submitted,
            done - waited,
        ])
    }

    /// Clears the target and records `slot`'s viewport into a new encoder.
    fn draw(&self, renderer: &Renderer, slot: usize) -> wgpu::CommandEncoder {
        let mut encoder = self.gpu.device.create_command_encoder(&Default::default());
        let scaled = self.bounds().scale(self.scale);
        renderer.note_vram(&self.gpu.device);
        let drew = renderer.draw(
            slot,
            &self.gpu,
            &mut encoder,
            &self.view,
            FORMAT,
            [self.width, self.height],
            scaled,
            scaled,
            self.timed,
            true,
        );
        // The post pass cleared it when something drew.
        if !drew {
            super::clear_target(&mut encoder, "offscreen clear", &self.view);
        }
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
    pub fn settle(&mut self, renderer: &Renderer) -> Result<Vec<u8>> {
        // Pipelines first, outside the clock: a process's first settle of a
        // game's scene compiled its sky and water on the passes' threads,
        // 3-4 s each cold at load 100, and at load 80-200 the 20 s below
        // ran out while they still compiled. Once per device, as the app's
        // warm-up does; then free.
        let passes = renderer.passes();
        self.warm(|gpu| {
            for pass in &passes {
                pass.warm(gpu);
            }
        });
        // Uploads are budgeted per frame for interactive viewports (3 ms, at
        // least one item); under CPU load that can mean one mesh or texture
        // per frame. Nobody waits on these frames, so allow plenty of them.
        let started = std::time::Instant::now();
        let mut frames = 0;
        while renderer.pending() {
            if started.elapsed().as_secs() >= 20 {
                bail!(
                    "Scene still uploading after 20 s of offscreen frames ({frames}), \
                     waiting on {:?}",
                    renderer.waiting_on()
                );
            }
            self.frame(renderer)?;
            frames += 1;
        }
        self.frame(renderer)
    }

    /// The colour at `p` (as `Camera::project` gives it for this target's
    /// bounds) in `pixels`, a frame this target drew.
    pub fn pixel(&self, pixels: &[u8], p: gpui::Point<Pixels>) -> [u8; 3] {
        let (x, y) = (f32::from(p.x) as usize, f32::from(p.y) as usize);
        let i = (y * self.width as usize + x) * 4;
        [pixels[i], pixels[i + 1], pixels[i + 2]]
    }
}

/// The device every offscreen target in the process draws on, made by the
/// first. One instance and device per target leaked in the NVIDIA driver:
/// the kit's tests could no longer find an adapter after about 65 targets,
/// and run in parallel one sometimes spun in the driver for good.
fn shared() -> Result<Gpu> {
    static SHARED: std::sync::OnceLock<Result<Gpu, String>> = std::sync::OnceLock::new();
    let made = SHARED.get_or_init(|| super::device::offscreen().map_err(|e| format!("{e:#}")));
    made.clone().map_err(anyhow::Error::msg)
}

/// Holds off process exit's GPU driver teardown while alive: offscreen
/// work in flight on another thread (a thumbnail worker's frame, or its
/// freeing what it made) crashed the process at exit, when atexit handlers
/// tore the driver down under it. Offscreen frames and targets take one
/// themselves; hold one around other GPU work off the main thread.
pub struct Busy(());

/// Offscreen work in flight, and whether the process is exiting.
static WORK: std::sync::Mutex<(usize, bool)> = std::sync::Mutex::new((0, false));
static IDLE: std::sync::Condvar = std::sync::Condvar::new();

/// A [`Busy`] for GPU work about to start; `None` once the process is
/// exiting, when the GPU must not be touched again (leak, don't free).
pub fn busy() -> Option<Busy> {
    let mut work = WORK.lock().unwrap_or_else(|e| e.into_inner());
    if work.1 {
        return None;
    }
    work.0 += 1;
    Some(Busy(()))
}

impl Drop for Busy {
    fn drop(&mut self) {
        let mut work = WORK.lock().unwrap_or_else(|e| e.into_inner());
        work.0 -= 1;
        if work.0 == 0 {
            IDLE.notify_all();
        }
    }
}

/// Threads that drew offscreen and were let go without a join, joined at
/// exit instead: a thread still ending, its thread-local state freed
/// while exit tore the driver down, crashed the process.
static RETIRED: std::sync::Mutex<Vec<std::thread::JoinHandle<()>>> =
    std::sync::Mutex::new(Vec::new());

/// Lets `thread`, which draws offscreen and is ending, go without joining
/// it now; exit waits for it. For a worker its owner drops on the UI
/// thread, where a join would wait out its render.
pub fn join_at_exit(thread: std::thread::JoinHandle<()>) {
    let mut retired = RETIRED.lock().unwrap_or_else(|e| e.into_inner());
    retired.retain(|t| !t.is_finished());
    retired.push(thread);
}

/// Runs at exit before the driver's own handlers, which the driver
/// registered when it loaded (exit runs them in reverse): refuses new
/// work, waits for what is in flight, and joins retired threads, then ends
/// the process without those handlers. Bounded, so a frame stuck in the
/// driver cannot hang exit for good.
///
/// Waiting alone did not keep exit from crashing (NVIDIA 620.70, cores of
/// 2026-11-19): the driver's own teardown (libEGL_nvidia's handler
/// freeing eglcore) ran while its shader compiler threads were still
/// compiling, which then crashed or corrupted the heap (glibc's
/// "corrupted size vs. prev_size"), and a frame compiling pipelines under
/// load outlasted the bound. Nothing after this handler needs to run in a
/// process with a GPU device: Rust flushed its output before calling exit,
/// C stdio is flushed here, the kernel frees the device, and the driver's
/// shader disk cache is written as it compiles. Handlers registered before
/// this one are skipped with the driver's, so a coverage profiler's would
/// be too.
#[cfg(all(target_os = "linux", target_env = "gnu"))]
extern "C" fn exiting(status: std::ffi::c_int, _: *mut std::ffi::c_void) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    {
        let mut work = WORK.lock().unwrap_or_else(|e| e.into_inner());
        work.1 = true;
        while work.0 > 0 {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            if left.is_zero() {
                break;
            }
            work = IDLE
                .wait_timeout(work, left)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
    }
    let retired = std::mem::take(&mut *RETIRED.lock().unwrap_or_else(|e| e.into_inner()));
    for thread in retired {
        while !thread.is_finished() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        if thread.is_finished() {
            let _ = thread.join();
        }
    }
    // SAFETY: fflush(NULL) flushes every C stream; _exit ends the process
    // with the status exit was given.
    unsafe {
        libc::fflush(std::ptr::null_mut());
        libc::_exit(status);
    }
}

/// Has exit wait for GPU work in flight ([`Busy`], [`join_at_exit`])
/// before the driver tears itself down. Call once a device exists, window
/// or offscreen, so after the driver loaded and registered its own
/// teardown: this then runs first (traced 2026-11-19: NVIDIA's and LLVM's
/// handlers all register at device creation). Window devices too: an app
/// that never drew offscreen exited with a renderer still freeing on its
/// thread, and crashed in the driver. Once, however many devices.
pub(super) fn guard_exit() {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    {
        // glibc's on_exit: an atexit handler that is told the status.
        unsafe extern "C" {
            fn on_exit(
                f: extern "C" fn(std::ffi::c_int, *mut std::ffi::c_void),
                arg: *mut std::ffi::c_void,
            ) -> std::ffi::c_int;
        }
        static REGISTERED: std::sync::Once = std::sync::Once::new();
        // SAFETY: registers a handler with no argument; it lives for the
        // whole process.
        REGISTERED.call_once(|| unsafe {
            on_exit(exiting, std::ptr::null_mut());
        });
    }
}
