// Reusable GPUI pieces for game editors: chrome widgets, docking, an
// orthographic viewport camera with picking and gizmos, and a wgpu mesh
// renderer that draws inside GPUI's frame. Game formats stay out of here.

pub mod camera;
pub mod code;
pub mod design;
pub mod dock;
pub mod field;
pub mod gizmo;
// Needs GPUI's windows on gpui_wgpu: Linux, and Windows via vendor/zed.
#[cfg(any(target_os = "linux", target_os = "windows"))]
pub mod gpu;
pub mod heightfield;
pub mod history;
pub mod icon;
pub mod math;
#[cfg(any(target_os = "linux", target_os = "windows"))]
pub mod model_view;
pub mod paint;
pub mod pick;
pub mod platform;
pub mod profile;
pub mod scatter;
pub mod task;
pub mod texture;
pub mod theme;
pub mod tree;
pub mod widgets;

/// The patched GPUI this crate is built against. Use it instead of a
/// separate `gpui` dependency so element and context types always match.
pub use gpui;
pub use gpui_platform;
