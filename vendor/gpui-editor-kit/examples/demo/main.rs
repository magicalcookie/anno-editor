// The demo's viewports use `gpu`, which exists where GPUI's windows draw
// with gpui_wgpu: Linux, Windows and macOS.
#[cfg(any(target_os = "linux", target_os = "windows", target_os = "macos"))]
mod app;

#[cfg(any(target_os = "linux", target_os = "windows", target_os = "macos"))]
fn main() -> anyhow::Result<()> {
    app::main()
}

#[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
fn main() {
    eprintln!("The demo needs gpui_editor_kit::gpu, which is Linux, Windows and macOS only.");
}
