# Local changes to Zed's GPUI

`gpui`, `gpui_wgpu`, `gpui_windows` and `gpui_macos` are copies of Zed's crates at the
commit named in their manifests, made by `tools/vendor-zed.py`, and replace
the git versions through `[patch."https://github.com/zed-industries/zed"]`
in the root manifest. Every other GPUI crate comes from git at the same
commit. The first commit touching each directory holds it unmodified; the
changes below are the diff on top. To move to a newer Zed commit, rerun the
tool with the new revision, bump the revisions in `Cargo.toml`, and replay
that diff.

- **Build configuration.** Declare `rust_analyzer` as a known cfg, matching
  GPUI's own styled helpers, so consuming editors build without cfg warnings.

- **Texture handoff (Linux, Windows and macOS).** `TextureHandoff` in
  `gpui/src/texture_handoff.rs`, `Window::paint_gpu_texture` in
  `gpui/src/window.rs`, and `draw_texture` in `gpui_wgpu/src/wgpu_renderer.rs`.
  `PaintSurface::texture` is the handoff a surface draws. `gpui` gains a
  `wgpu` dependency at Zed's revision for the types on the platforms whose
  windows draw through `gpui_wgpu`. Another thread renders into a texture
  on GPUI's device and GPUI draws it in the same frame, so the UI thread
  does not record it. The kit's 3D viewports and the editor's UI canvas
  draw this way; GPUI's own passes never break for an application's:
  - Each frame that draws or presents asks every texture the last frame
    drew for a new frame (`Window::ask_textures`), after the next-frame
    callbacks and before layout; `paint_gpu_texture` asks one painted for
    the first time. The producer records while the window lays out.
  - Just before drawing it, the renderer waits for the producer's submit of
    the frame asked for (`TextureHandoff::begin_read`), at most until the
    handoff's deadline from the ask, and not at all when the producer has
    yet to see this device or has not answered the previous frame's ask.
    It then draws the texture with the polychrome sprite pipeline, inside
    the pass: one texel a device pixel from the surface's origin, clipped
    to the surface and its content mask.
  - One device and one queue: submission order is the GPU sync. The
    producer's next submit waits until GPUI's frame reading the texture is
    submitted (`end_read`, on a guard that also runs if the frame fails),
    so a single texture needs no second buffer and adds no frame.
  - A frame that drew an older producer frame than it asked for presents
    once more (`Window::follow_late_textures`) to show the late one. A
    closed handoff never counts as late: it keeps drawing its last frame
    (the kit's `Viewport::still`) and none will follow.
  - The producer learns GPUI's device from the first frame that draws the
    texture, and a new one after a device loss (`TextureHandoff::wait`);
    a texture made on another device is not drawn. The texture's format
    is the producer's: the sprite shader samples it as float.
- **Glyph rasterizer seam.** For text an application draws into a texture
  of its own (the editor's UI canvas) that must match GPUI's labels.
  `Window::glyph_raster` in `gpui/src/window.rs` places a glyph as
  `paint_glyph` does in grayscale (the origin snapped to a quarter pixel
  across, the dilation for its colour) and returns its `RenderGlyphParams`
  and the device pixel it hangs from; `paint_glyph` shares that code
  (`place_glyph`). `TextSystem::raster_bounds` and
  `TextSystem::rasterize_glyph` in `gpui/src/text_system.rs` are public, so
  the platform's own rasterizer makes the coverage on any thread.
  `grayscale_glyph_correction` there gives the gamma ratios and enhanced
  contrast GPUI's monochrome sprite shader corrects coverage with, from the
  same `ZED_FONTS_*` variables; `gpui_wgpu`'s `RenderingParameters` reads
  them through it. Shaping stays GPUI's public `WindowTextSystem`.
- **Device seam.** The one place an application shapes GPUI's GPU
  context. `WgpuContextProvider` and `set_context_provider` in
  `gpui_wgpu/src/wgpu_context.rs`: the provider names the instances to try,
  in order (backends and flags; GPUI sets the display handle), and the
  device to request on an adapter (features and limits; GPUI adds the
  dual-source blending its text uses). GPUI still chooses the adapter as
  upstream does, by testing the window's surface, rejects software
  adapters during device-loss recovery, and asks the provider again after
  a loss. `WgpuContextProvider::made` hears of each device GPUI makes for
  its windows, the first and each after a loss, before anything draws on
  it (the kit starts building its pipelines there). `WgpuContext::for_window`
  is the path windows and recovery take in `wgpu_renderer.rs`; the headless
  context takes the provider's instances too, and calls no `made`. Without
  a provider GPUI is as upstream. The editor kit
  installs one (`gpu::install`, its `src/gpu/device.rs`), so GPUI's own
  device setup carries no kit features or limits.
- **Present without drawing.** `Window::request_present` in
  `gpui/src/window.rs` sets the window's existing `needs_present`, so the
  next frame request presents the last scene without laying out or
  painting: a camera move or an animating viewport gets a new 3D frame
  (textures are asked at the frame's start either way) without the
  window's per-frame element work.
- **Draw phase timer.** `set_draw_timer` in `gpui/src/window.rs` takes a
  function called with each draw phase's name and duration: request
  layout, layout and prepaint, paint, a11y, finish, the whole draw, and
  present. The kit's `profile::enable` sets it, so a window rebuild's time
  shows beside the app's own spans without the `profiler` feature.
- **Nested cached views.** `prepaint_view` in `gpui/src/view.rs` no longer
  sets `window.refreshing` while a cached view renders, so cached views
  inside it reuse their last frame unless they are dirty themselves. Zed
  forced them all to render: the editor's dock panes sit in a cached shell,
  and notifying one pane (the UI canvas on hover) redrew all of them. A
  nested cached view must observe whatever it reads, as GPUI never
  invalidated cached views for the entities they read anyway. A view's
  recorded ranges index the frame that drew it, so `ViewElementState`
  stamps them with `Window::frame_number` and replays them only in the next
  frame. A view its parent replayed holds older ranges and renders instead
  (replaying them panicked in `LineLayoutCache::reuse_layouts`).
  `gpui/tests/cached_views.rs` covers it.
- **Path targets sized to the paths.** Paths rasterize into slots of a
  small intermediate and its MSAA twin, each path in its own slot, moved
  there by whole pixels (`pack_paths`, `draw_paths_to_intermediate` in
  `gpui_wgpu/src/wgpu_renderer.rs`), and composite from their slots
  (`PathSprite::offset`, `vs_path`); a batch that outgrows it draws in
  turns. Zed made both window-sized, 281 MiB of a 5K window for the dozen
  icons the editor draws as paths. The intermediate is as wide as the
  widest path, at least 1024 pixels, and as tall as the frame's paths
  pack within the frame, sides rounded up to 256; a frame without paths
  drops it. `GlobalParams::path_size` carries its size. Zed composited a
  batch of different orders as one sprite over their union; one per path,
  in order, blends the same. The kit's `tests/gpui_paths.rs` covers it.
- **Present mode.** Without a platform preference (X11, Windows), the
  surface uses mailbox where the driver offers it instead of FIFO, which
  capped the frame rate at the display refresh.
- **A destroyed device is lost.** `DeviceErrorState::install` in
  `gpui_wgpu/src/wgpu_context.rs` marks the device lost whatever the
  reason. Zed ignored `DeviceLostReason::Destroyed`, but GPUI never
  destroys its own device, so a window whose device something else
  destroyed stayed black instead of recovering. The editor kit's
  `gpu::lose_window_device` destroys it to test recovery.
- **Error reporting.** Log each uncaptured device error before storing the
  latest error for recovery. Otherwise a dependent invalid-command error
  can overwrite the original pipeline or binding failure before observation.
- **Windows on wgpu.** `gpui_windows` draws each window with
  `gpui_wgpu::WgpuRenderer` on a swap chain for its HWND
  (`gpui_windows/src/wgpu_window.rs`) instead of its DirectX 11 renderer,
  so the texture handoff works there. GPUI's own instance in
  `gpui_wgpu/src/wgpu_context.rs` asks for DX12 only on Windows (a context
  provider may name others; the kit's tries Vulkan first). Windows share
  one wgpu device through `WindowsPlatformState::gpu_context`.
  - DirectWrite still rasterizes glyphs on the CPU, and color glyphs on its
    own D3D11 device, which the platform keeps for that and for vsync.
    `directx_renderer` stays compiled (`allow(dead_code)`) for the font
    gamma parameters DirectWrite reads from it.
  - ClearType coverage arrives as R, G, B bytes while the wgpu atlas holds
    BGRA, so the renderer's `is_bgr` swap is always on.
  - Windows lose `WS_EX_NOREDIRECTIONBITMAP`: wgpu presents through an HWND
    swap chain, not DirectComposition. `GPUI_DISABLE_DIRECT_COMPOSITION`
    no longer does anything.
  - HWND swap chains are opaque, so transparent and blurred window
    backgrounds draw opaque (Mica and the other DWM backdrops still apply
    to the frame).
  - wgpu detects and recovers from its own device loss in `draw`; the
    platform's D3D11 device-lost broadcast only forces the next render.
    `render_to_image` (test-support) is not implemented.
  - The DX12 shader compiler is wgpu's `Auto`: `dxcompiler.dll` next to the
    executable or on PATH if present, else the system's FXC, which is slow
    to build pipelines.
- **Lockfile.** `gpu-allocator` resolves to `windows` 0.62 (as in Zed's
  lock) to match wgpu-hal's DX12 backend; `zed-scap` keeps 0.61.

- **macOS on wgpu.** `gpui_macos` is vendored and its windows draw with
  `gpui_wgpu::WgpuRenderer` on a Metal swap chain for the view
  (`gpui_macos/src/wgpu_window.rs`) instead of `gpui_apple`'s Metal
  renderer, so the texture handoff works there. GPUI's own instance in
  `gpui_wgpu/src/wgpu_context.rs` asks for Metal only on macOS. Every
  window shares one wgpu device through `MacPlatformState::gpu_context`. The
  view's backing layer is a `CAMetalLayer` the platform owns; wgpu adopts
  it. Frames are paced with `CVDisplayLink`. A `CAMetalDisplayLink` would
  take the drawable before wgpu could.
  - Core Text still rasterizes glyphs, which needs `gpui_platform`'s
    `font-kit` feature. Without it the window draws no text.
  - Video surfaces (`CVPixelBuffer`) are not drawn. `render_to_image`
    (test-support) is not implemented.
  - Present mode falls through to FIFO. Metal does not offer mailbox.
  - iOS keeps Zed's Metal renderer (`gpui_apple`).
