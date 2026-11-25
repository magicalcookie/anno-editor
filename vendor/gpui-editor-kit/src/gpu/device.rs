// The process's GPU setup, one policy for GPUI's windows and the offscreen
// renderer: instance flags, backend order, and the device's features and
// limits. The windows get it through gpui_wgpu's context provider
// (vendor/zed/PATCH.md), so tests draw on the device setup the app does.
// Also what is made per device and dropped when a device is lost
// (`OnDevice`, `lose_window_device`).
use super::Gpu;
use anyhow::{Result, bail};

/// Backends to try, best first, each on an instance of its own: within one
/// instance GPUI ranks adapters by device type before backend, so a
/// discrete DX12 adapter would win over an integrated Vulkan one.
///
/// Windows tries Vulkan before DX12. macOS has Metal only. Linux tries GL
/// only for machines without a Vulkan driver, where GPUI's windows still
/// draw and the viewports say they can't draw 3D (`gpu::supported`).
#[cfg(target_os = "windows")]
const BACKENDS: &[wgpu::Backends] = &[wgpu::Backends::VULKAN, wgpu::Backends::DX12];
#[cfg(target_os = "macos")]
const BACKENDS: &[wgpu::Backends] = &[wgpu::Backends::METAL];
#[cfg(not(any(target_os = "windows", target_os = "macos")))]
const BACKENDS: &[wgpu::Backends] = &[wgpu::Backends::VULKAN, wgpu::Backends::GL];

/// Every optional feature the kit draws with, taken where the adapter
/// offers it. The renderer checks the device's features and picks its
/// paths (`bc_supported`, `ARENA_FEATURES`, `visbuf::supported`).
const WANTED: wgpu::Features = wgpu::Features::TEXTURE_COMPRESSION_BC
    .union(wgpu::Features::TIMESTAMP_QUERY)
    .union(wgpu::Features::TIMESTAMP_QUERY_INSIDE_ENCODERS)
    // Only for gpu::SPLIT_TIMING, which is off unless asked for.
    .union(wgpu::Features::TIMESTAMP_QUERY_INSIDE_PASSES)
    // Screen-space reflections trace a pixel quad's ray together.
    .union(wgpu::Features::SUBGROUP)
    // GPUI's subpixel text.
    .union(wgpu::Features::DUAL_SOURCE_BLENDING)
    .union(super::ARENA_FEATURES)
    .union(super::visbuf::CORNER_FEATURES)
    // Shading math in 16 bits where it fits (pass::half_types).
    .union(wgpu::Features::SHADER_F16);

/// An instance for `backends`. wgpu rechecks every indirect draw at submit
/// unless told not to, once per draw even inside a multi-draw: about 1 ms
/// on a frame that redraws the shadow maps. The draws are all the kit's, and
/// a bad one draws wrong, which is a bug to fix either way (the user's
/// call, 2026-11-20). Off on DX12 too: its pass also supplied each draw's
/// first instance and base vertex there, and the arena's draws start at
/// zero for both (`placed::ARGS`). `WGPU_VALIDATION_INDIRECT_CALL=1` turns
/// it back on, as the other `WGPU_*` flags do theirs.
fn instance_descriptor(backends: wgpu::Backends) -> wgpu::InstanceDescriptor {
    let mut flags = wgpu::InstanceFlags::from_build_config();
    flags.remove(wgpu::InstanceFlags::VALIDATION_INDIRECT_CALL);
    wgpu::InstanceDescriptor {
        backends,
        flags: flags.with_env(),
        ..wgpu::InstanceDescriptor::new_without_display_handle()
    }
}

/// The device on `adapter`: every wanted feature it offers, and all of its limits, since viewports bind more storage buffers and
/// textures per stage than the defaults allow.
fn device_descriptor(adapter: &wgpu::Adapter) -> wgpu::DeviceDescriptor<'static> {
    wgpu::DeviceDescriptor {
        label: Some("kit"),
        required_features: adapter.features() & WANTED,
        required_limits: adapter.limits(),
        // 8-64 MB blocks: the default's 128-256 MB ones reserved 300 MB more
        // than the continental island used.
        memory_hints: wgpu::MemoryHints::MemoryUsage,
        ..Default::default()
    }
}

/// Has GPUI's windows draw on this setup. Call before the first window
/// opens; without it they draw on GPUI's own device, which lacks most of
/// the features above, and viewports fall back to their slower paths.
pub fn install() {
    struct Provider;
    impl gpui_wgpu::WgpuContextProvider for Provider {
        fn instances(&self) -> Vec<wgpu::InstanceDescriptor> {
            BACKENDS.iter().map(|&b| instance_descriptor(b)).collect()
        }

        fn device(&self, adapter: &wgpu::Adapter) -> wgpu::DeviceDescriptor<'static> {
            device_descriptor(adapter)
        }

        /// The pipelines start building before any pane draws, and again
        /// on the device after a loss.
        fn made(&self, device: &wgpu::Device, queue: &wgpu::Queue) {
            let gpu = Gpu {
                device: device.clone(),
                queue: queue.clone(),
            };
            super::offscreen::guard_exit();
            window_frame(&gpu);
            super::warm(gpu);
        }
    }
    static INSTALLED: std::sync::Once = std::sync::Once::new();
    INSTALLED.call_once(|| {
        gpui_wgpu::set_context_provider(Box::new(Provider))
            .expect("the kit owns gpui_wgpu's context provider");
    });
}

/// A device for offscreen drawing, with every wanted feature: the first
/// backend with an adapter, its hardware adapters
/// first (discrete, then integrated).
pub(super) fn offscreen() -> Result<Gpu> {
    let mut failed = Vec::new();
    for &backends in BACKENDS {
        let instance = wgpu::Instance::new(instance_descriptor(backends));
        let mut adapters = pollster::block_on(instance.enumerate_adapters(backends));
        adapters.sort_by_key(|a| match a.get_info().device_type {
            wgpu::DeviceType::DiscreteGpu => 0,
            wgpu::DeviceType::IntegratedGpu => 1,
            wgpu::DeviceType::Other => 2,
            wgpu::DeviceType::VirtualGpu => 3,
            wgpu::DeviceType::Cpu => 4,
        });
        for adapter in adapters {
            match pollster::block_on(adapter.request_device(&device_descriptor(&adapter))) {
                Ok((device, queue)) => {
                    super::offscreen::guard_exit();
                    return Ok(Gpu { device, queue });
                }
                Err(e) => failed.push(format!("{}: {e}", adapter.get_info().name)),
            }
        }
    }
    if failed.is_empty() {
        bail!("No GPU adapter for offscreen rendering on {BACKENDS:?}");
    }
    bail!(
        "No GPU device for offscreen rendering: {}",
        failed.join("; ")
    )
}

/// A value made on one device, kept until it is asked for on another.
/// After a device loss wgpu rejects every object of the lost device ("doesn't
/// match"), and GPUI draws on with a new one: the first frame there drops
/// the old value, and its owner makes it again from what it keeps on the
/// CPU. Passes hold their GPU state in one, as the renderer holds its own
/// (`State::move_to`). Devices compare by identity, and the device held
/// here keeps its identity from going to a new one.
pub struct OnDevice<T>(Option<(wgpu::Device, T)>);

impl<T> Default for OnDevice<T> {
    fn default() -> Self {
        Self(None)
    }
}

impl<T> OnDevice<T> {
    /// The value, if it was made on `device`.
    pub fn get(&self, device: &wgpu::Device) -> Option<&T> {
        self.0.as_ref().filter(|(d, _)| d == device).map(|(_, v)| v)
    }

    pub fn get_mut(&mut self, device: &wgpu::Device) -> Option<&mut T> {
        self.0.as_mut().filter(|(d, _)| d == device).map(|(_, v)| v)
    }

    /// The value, whichever device it was made on: for a pass's questions
    /// that come without a device (`Pass::animating`). The kit asks a pass
    /// to record only once it prepared ready on the frame's device.
    pub fn last(&self) -> Option<&T> {
        self.0.as_ref().map(|(_, v)| v)
    }

    pub fn last_mut(&mut self) -> Option<&mut T> {
        self.0.as_mut().map(|(_, v)| v)
    }

    /// The value made on `device`, made by `make` first if there is none
    /// there; one from another device is dropped.
    pub fn get_or_insert_with(
        &mut self,
        device: &wgpu::Device,
        make: impl FnOnce() -> T,
    ) -> &mut T {
        if self.get(device).is_none() {
            self.0 = Some((device.clone(), make()));
        }
        &mut self.0.as_mut().expect("made above").1
    }

    /// Holds `value`, made on `device`, in place of what was held.
    pub fn set(&mut self, device: &wgpu::Device, value: T) -> &mut T {
        &mut self.0.insert((device.clone(), value)).1
    }

    pub fn clear(&mut self) {
        self.0 = None;
    }
}

/// The device GPUI's windows draw with, and its queue. GPUI's windows
/// share one, so another means GPUI recovered from a loss.
static WINDOW: std::sync::Mutex<Option<Gpu>> = std::sync::Mutex::new(None);

/// Notes that GPUI made `gpu` for its windows, or that a window frame came
/// on it. After a loss the caches let the old device go: what they built
/// there is useless, and would hold the device for as long as the process
/// runs.
pub(super) fn window_frame(gpu: &Gpu) {
    let mut window = WINDOW.lock().unwrap_or_else(|e| e.into_inner());
    if window.as_ref().is_some_and(|w| w.device == gpu.device) {
        return;
    }
    if let Some(lost) = window.replace(gpu.clone()) {
        drop(window);
        eprintln!("kit: window frames moved to a new GPU device; rebuilding on it");
        super::forget(&lost.device);
    }
}

/// The device GPUI's windows draw with, once GPUI made one.
pub(super) fn window_gpu() -> Option<Gpu> {
    WINDOW.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

/// Destroys the device GPUI's windows draw with, as a driver reset loses
/// one: GPUI makes a new device and every view rebuilds on it. For testing
/// recovery in the app (the editor's `ANNO_EDITOR_LOSE_DEVICE`); tests lose
/// an offscreen device with `Offscreen::lose`. False when GPUI has made no
/// device for a window yet, or without [`install`]. Waits for the device's
/// work in flight, as its loss is only reported once that is done (wgpu's
/// `destroy`).
pub fn lose_window_device() -> bool {
    let Some(gpu) = window_gpu() else {
        return false;
    };
    lose(&gpu.device);
    true
}

/// Destroys `device` and has wgpu report the loss now (`poll`).
pub(super) fn lose(device: &wgpu::Device) {
    super::forget(device);
    device.destroy();
    let _ = device.poll(wgpu::PollType::Wait {
        submission_index: None,
        timeout: Some(std::time::Duration::from_secs(5)),
    });
}
