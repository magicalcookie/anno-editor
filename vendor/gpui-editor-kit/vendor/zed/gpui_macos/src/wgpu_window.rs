// A window's renderer: gpui_wgpu on a Metal swap chain for the view, in
// place of gpui_apple's Metal renderer, so application textures
// (TextureHandoff) draw on GPUI's own wgpu device on macOS as they do on
// Linux and Windows.
//
// The view's backing layer is a CAMetalLayer this type owns.
// `makeBackingLayer` hands that layer to AppKit, and wgpu adopts it.
// Frames are not paced by CAMetalDisplayLink: that link takes the
// drawable, and wgpu asks the layer for one itself.

use std::{ptr::NonNull, sync::Arc};

use anyhow::{Context as _, Result};
use gpui::{DevicePixels, GpuSpecs, PlatformAtlas, Scene, Size, size};
use gpui_wgpu::{GpuContext, WgpuRenderer, WgpuSurfaceConfig};
use objc2::{
    msg_send,
    rc::Retained,
    runtime::{AnyObject, Bool, ProtocolObject},
};
use objc2_foundation::NSRect;
use objc2_quartz_core::{CAAutoresizingMask, CAMetalDrawable, CAMetalLayer};
use raw_window_handle as rwh;

/// The NSView as wgpu's surface creation wants it. Only used on the main
/// thread, while the window lives.
#[derive(Clone, Copy, Debug)]
struct RawView(NonNull<AnyObject>);

unsafe impl Send for RawView {}
unsafe impl Sync for RawView {}

impl rwh::HasWindowHandle for RawView {
    fn window_handle(&self) -> Result<rwh::WindowHandle<'_>, rwh::HandleError> {
        let handle = rwh::AppKitWindowHandle::new(self.0.cast());
        Ok(unsafe { rwh::WindowHandle::borrow_raw(handle.into()) })
    }
}

impl rwh::HasDisplayHandle for RawView {
    fn display_handle(&self) -> Result<rwh::DisplayHandle<'_>, rwh::HandleError> {
        Ok(rwh::DisplayHandle::appkit())
    }
}

pub(crate) struct WgpuWindowRenderer {
    view: RawView,
    layer: Retained<CAMetalLayer>,
    context: GpuContext,
    renderer: Option<WgpuRenderer>,
    size: Size<DevicePixels>,
    transparent: bool,
}

impl WgpuWindowRenderer {
    /// The wgpu device is created on the first draw or resize, once AppKit
    /// has installed `layer` as the view's backing layer.
    pub(crate) fn new(view: NonNull<AnyObject>, context: GpuContext) -> Self {
        let layer = CAMetalLayer::new();
        layer.setAutoresizingMask(
            CAAutoresizingMask::LayerWidthSizable | CAAutoresizingMask::LayerHeightSizable,
        );
        layer.setNeedsDisplayOnBoundsChange(true);
        layer.setOpaque(true);
        Self {
            view: RawView(view),
            layer,
            context,
            renderer: None,
            size: size(DevicePixels(1), DevicePixels(1)),
            transparent: false,
        }
    }

    /// No `CAMetalDisplayLink` is attached. Returning `None` keeps the
    /// window on `CVDisplayLink`, which only wakes frames.
    pub(crate) fn layer(&self) -> Option<&CAMetalLayer> {
        None
    }

    pub(crate) fn layer_ptr(&self) -> *mut CAMetalLayer {
        Retained::as_ptr(&self.layer).cast_mut()
    }

    pub(crate) fn set_display_link_attached(&mut self, _attached: bool) {}

    /// The Metal display link is not used. The argument is dropped so the
    /// window's frame callback still type-checks.
    pub(crate) fn set_frame_drawable(
        &mut self,
        _drawable: Option<Retained<ProtocolObject<dyn CAMetalDrawable>>>,
    ) -> Option<Retained<ProtocolObject<dyn CAMetalDrawable>>> {
        None
    }

    /// Live resize draws inside AppKit's transaction. wgpu reads this flag
    /// off the layer when it acquires the drawable.
    pub(crate) fn set_presents_with_transaction(&mut self, presents: bool) {
        self.layer.setPresentsWithTransaction(presents);
    }

    pub(crate) fn update_drawable_size(&mut self, size: Size<DevicePixels>) {
        self.size = Size {
            width: DevicePixels(size.width.0.max(1)),
            height: DevicePixels(size.height.0.max(1)),
        };
        let size = self.size;
        if let Err(err) = self.ensure().map(|renderer| renderer.update_drawable_size(size)) {
            log::error!("wgpu resize failed: {err:#}");
        }
    }

    pub(crate) fn update_transparency(&mut self, transparent: bool) {
        self.transparent = transparent;
        self.layer.setOpaque(!transparent);
        if let Ok(renderer) = self.ensure() {
            renderer.update_transparency(transparent);
        }
    }

    pub(crate) fn destroy(&mut self) {
        if let Some(renderer) = self.renderer.as_mut() {
            renderer.destroy();
        }
    }

    /// Draws a frame. Returns true when the next frame must be a forced
    /// render: the device was lost (recovered here, or retried next frame)
    /// or GPU state such as the atlas was cleared.
    pub(crate) fn draw(&mut self, scene: &Scene) -> bool {
        if let Err(err) = self.ensure().map(|_| ()) {
            log::error!("wgpu renderer unavailable: {err:#}");
            return false;
        }
        let view = self.view;
        let Some(renderer) = self.renderer.as_mut() else {
            return false;
        };
        if renderer.device_lost() {
            if let Err(err) = renderer.recover(&view) {
                log::warn!("GPU recovery failed, will retry on next frame: {err:#}");
            }
            return true;
        }
        renderer.draw(scene);
        renderer.needs_redraw()
    }

    pub(crate) fn sprite_atlas(&mut self) -> Arc<dyn PlatformAtlas> {
        self.ensure()
            .expect("Creating the wgpu renderer")
            .sprite_atlas()
            .clone()
    }

    pub(crate) fn gpu_specs(&mut self) -> Option<GpuSpecs> {
        self.ensure().ok()?.gpu_specs()
    }

    fn ensure(&mut self) -> Result<&mut WgpuRenderer> {
        if self.renderer.is_none() {
            self.fit_layer_to_view();
            let config = WgpuSurfaceConfig {
                size: self.size,
                transparent: self.transparent,
                // Metal offers FIFO and immediate, not mailbox. FIFO is
                // the tear-free one; the display link already paces frames.
                preferred_present_mode: None,
            };
            let renderer = WgpuRenderer::new(self.context.clone(), &self.view, config, None)
                .context("Creating the wgpu renderer")?;
            self.renderer = Some(renderer);
        }
        Ok(self.renderer.as_mut().unwrap())
    }

    fn fit_layer_to_view(&self) {
        unsafe {
            let view = self.view.0.as_ptr();
            let _: () = msg_send![view, setWantsLayer: Bool::YES];
            let _: () = msg_send![view, setLayer: &*self.layer];
            let bounds: NSRect = msg_send![view, bounds];
            // NSRect and CGRect are the same layout on macOS. `setFrame:`
            // takes the layer's rectangle.
            let _: () = msg_send![&*self.layer, setFrame: bounds];
        }
    }
}
