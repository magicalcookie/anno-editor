# Local changes to Zed's GPUI

`gpui`, `gpui_wgpu` and `gpui_windows` are copies of Zed's crates at the
commit named in their manifests, made by `tools/vendor-zed.py`, and replace
the git versions through `[patch."https://github.com/zed-industries/zed"]`
in the root manifest. Every other GPUI crate comes from git at the same
commit. The first commit touching each directory holds it unmodified; the
changes below are the diff on top. To move to a newer Zed commit, rerun the
tool with the new revision, bump the revisions in `Cargo.toml`, and replay
that diff.

- **Build configuration.** Declare `rust_analyzer` as a known cfg, matching
  GPUI's own styled helpers, so consuming editors build without cfg warnings.

- **Viewport hook (Linux and Windows).** `GpuCanvas` and `GpuCanvasFrame`
  in `gpui/src/scene.rs`, `Window::paint_gpu_canvas` in
  `gpui/src/window.rs`. `PaintSurface` carries the canvas; the wgpu
  renderer ends its pass at a surface batch, calls each canvas with GPUI's
  device, queue, encoder and target, then resumes with a loading pass. wgpu
  tracks resource lifetimes, so canvases need no frame begin or release
  callbacks. `gpui` gains a `wgpu` dependency at Zed's revision for the
  trait's types on the platforms whose windows draw through `gpui_wgpu`.
- **Device.** Native builds request the adapter's limits instead of the
  downlevel defaults (viewports bind more storage buffers and textures per
  stage), and where offered BC texture compression, timestamp queries, and
  what the placement arena needs: indirect first instances and
  non-uniformly indexed texture arrays.
- **Present without drawing.** `Window::request_present` in
  `gpui/src/window.rs` sets the window's existing `needs_present`, so the
  next frame request presents the last scene without laying out or
  painting: viewport canvases redraw a camera move without the window's
  per-frame element work.
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
- **Present mode.** Without a platform preference (X11, Windows), the
  surface uses mailbox where the driver offers it instead of FIFO, which
  capped the frame rate at the display refresh.
- **Error reporting.** Log each uncaptured device error before storing the
  latest error for recovery. Otherwise a dependent invalid-command error
  can overwrite the original pipeline or binding failure before observation.
- **Windows on wgpu.** `gpui_windows` draws each window with
  `gpui_wgpu::WgpuRenderer` on a DX12 swap chain for its HWND
  (`gpui_windows/src/wgpu_window.rs`) instead of its DirectX 11 renderer,
  so the viewport hook works there. The instance in
  `gpui_wgpu/src/wgpu_context.rs` asks for DX12 only on Windows. Windows
  share one wgpu device through `WindowsPlatformState::gpu_context`.
  - DirectWrite still rasterizes glyphs on the CPU, and color glyphs on its
    own D3D11 device, which the platform keeps for that and for vsync.
    `directx_renderer` stays compiled (`allow(dead_code)`) for the font
    gamma parameters DirectWrite reads from it.
  - ClearType coverage arrives as R, G, B bytes while the wgpu atlas holds
    BGRA, so the renderer's `is_bgr` swap is always on.
  - Windows lose `WS_EX_NOREDIRECTIONBITMAP`: wgpu presents through an HWND
    swap chain, not DirectComposition. `GPUI_DISABLE_DIRECT_COMPOSITION`
    no longer does anything.
  - DX12 HWND swap chains are opaque, so transparent and blurred window
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

macOS and iOS keep Zed's Metal renderer (`gpui_macos`, `gpui_apple`); see
the README for what moving them onto `gpui_wgpu` would take.
