// A window's renderer: gpui_wgpu on a DX12 swap chain for the HWND, in
// place of the DirectX 11 renderer, so application viewports (GpuCanvas)
// draw with GPUI's own wgpu device on Windows as they do on Linux.

use std::{num::NonZeroIsize, sync::Arc};

use anyhow::{Context as _, Result};
use gpui::{DevicePixels, GpuSpecs, PlatformAtlas, Scene, Size, size};
use gpui_wgpu::{GpuContext, WgpuRenderer, WgpuSurfaceConfig};
use raw_window_handle as rwh;
use windows::Win32::Foundation::HWND;

/// The HWND as wgpu's surface creation wants it (Clone + Send + Sync). Only
/// used on the UI thread, while the window lives.
#[derive(Clone, Copy, Debug)]
struct RawWindow(NonZeroIsize);

impl rwh::HasWindowHandle for RawWindow {
    fn window_handle(&self) -> Result<rwh::WindowHandle<'_>, rwh::HandleError> {
        let handle = rwh::Win32WindowHandle::new(self.0);
        Ok(unsafe { rwh::WindowHandle::borrow_raw(handle.into()) })
    }
}

impl rwh::HasDisplayHandle for RawWindow {
    fn display_handle(&self) -> Result<rwh::DisplayHandle<'_>, rwh::HandleError> {
        Ok(rwh::DisplayHandle::windows())
    }
}

pub(crate) struct WgpuWindowRenderer {
    window: RawWindow,
    renderer: WgpuRenderer,
}

impl WgpuWindowRenderer {
    /// Starts at 1×1 like the DirectX renderer did; the first `WM_SIZE`
    /// sets the real size. `context` is shared by every window.
    pub(crate) fn new(hwnd: HWND, context: GpuContext) -> Result<Self> {
        let window = RawWindow(NonZeroIsize::new(hwnd.0 as isize).context("null HWND")?);
        let config = WgpuSurfaceConfig {
            size: size(DevicePixels(1), DevicePixels(1)),
            transparent: false,
            preferred_present_mode: None,
        };
        let mut renderer = WgpuRenderer::new(context, &window, config, None)
            .context("Creating the wgpu renderer")?;
        // DirectWrite rasterizes ClearType coverage as R, G, B bytes; the wgpu
        // atlas stores color glyphs as BGRA. This swaps them back in the
        // shader, whatever the panel's subpixel order (DirectWrite already
        // applied that).
        renderer.set_subpixel_layout(true);
        Ok(Self { window, renderer })
    }

    /// Draws a frame. Returns true when the next frame must be a forced
    /// render: the device was lost (recovered here, or retried next frame)
    /// or GPU state such as the atlas was cleared.
    pub(crate) fn draw(&mut self, scene: &Scene) -> bool {
        if self.renderer.device_lost() {
            if let Err(err) = self.renderer.recover(&self.window) {
                log::warn!("GPU recovery failed, will retry on next frame: {err:#}");
            }
            return true;
        }
        self.renderer.draw(scene);
        self.renderer.needs_redraw()
    }

    pub(crate) fn resize(&mut self, size: Size<DevicePixels>) {
        self.renderer.update_drawable_size(size);
    }

    pub(crate) fn update_transparency(&mut self, transparent: bool) {
        self.renderer.update_transparency(transparent);
    }

    pub(crate) fn sprite_atlas(&self) -> Arc<dyn PlatformAtlas> {
        self.renderer.sprite_atlas().clone()
    }

    pub(crate) fn gpu_specs(&self) -> Option<GpuSpecs> {
        self.renderer.gpu_specs()
    }
}
