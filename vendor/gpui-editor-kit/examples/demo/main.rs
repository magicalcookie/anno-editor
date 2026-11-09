// The demo's viewports use `gpu`, which exists where GPUI's windows draw
// with gpui_wgpu: Linux and Windows. Elsewhere the example only says so,
// so `cargo check --all-targets` stays green on macOS.
#[cfg(any(target_os = "linux", target_os = "windows"))]
mod app;

#[cfg(any(target_os = "linux", target_os = "windows"))]
fn main() -> anyhow::Result<()> {
    app::main()
}

#[cfg(not(any(target_os = "linux", target_os = "windows")))]
fn main() {
    eprintln!("The demo needs gpu_editor_kit::gpu, which is Linux and Windows only.");
}
