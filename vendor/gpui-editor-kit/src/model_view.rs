// Owns an orbit preview of one model: a renderer of its own, the camera and
// the drag that turns it. For asset previews and pickers; editors with tools
// drive `gpu::Viewport` themselves.
use crate::{
    camera::{Camera, Extent},
    gpu::{Batch, Instances, Light, Line, Material, Mesh, MeshData, Renderer, Scene, Viewport},
    math::{self, Mat4},
};
use gpui::{prelude::*, *};
use std::{cell::Cell, rc::Rc, sync::Arc};

/// Behind the model; the floor fades out to it.
pub const BACKDROP: u32 = 0x2a3038;

/// The floor under the model: a contact shadow under its base (`shadow`
/// across) darkening into a disc a step lighter than the backdrop, which
/// fades out to it at `radius`. A disc alone, barely lighter, read as a
/// vignette and the model still floated.
fn floor(center: [f32; 3], shadow: f32, radius: f32) -> Option<Arc<Mesh>> {
    const SIDES: u32 = 64;
    let mut data = MeshData {
        material: Material::Flat,
        ..Default::default()
    };
    data.positions.push(center);
    data.colors.push(0x1c2127);
    for (r, color) in [(shadow, 0x414955), (radius, BACKDROP)] {
        for k in 0..SIDES {
            let a = k as f32 / SIDES as f32 * std::f32::consts::TAU;
            data.positions
                .push([center[0] + r * a.cos(), center[1], center[2] + r * a.sin()]);
            data.colors.push(color);
        }
    }
    data.normals = vec![[0., 1., 0.]; data.positions.len()];
    let (inner, outer) = (1, 1 + SIDES);
    for k in 0..SIDES {
        let next = (k + 1) % SIDES;
        data.indices.extend([0, inner + k, inner + next]);
        data.indices.extend([inner + k, outer + k, outer + next]);
        data.indices.extend([inner + k, outer + next, inner + next]);
    }
    Mesh::compact(data).ok().map(Arc::new)
}

/// A grid on the floor plane at `step` spacing, inside the floor's disc and
/// fading toward its edge, so a model's size reads against it: the size
/// line said "1.3 m", the picture alone could have been a tree or a shrub.
fn grid(center: [f32; 3], radius: f32, step: f32) -> Vec<Line> {
    let mut lines = Vec::new();
    let n = (radius / step).floor() as i32;
    for axis in 0..2 {
        for k in -n..=n {
            let off = k as f32 * step;
            // The chord of the disc at this offset, kept inside where the
            // floor is still lighter than the backdrop.
            let half = (radius * radius - off * off).max(0.).sqrt() * 0.85;
            if half <= step * 0.5 {
                continue;
            }
            let fade = 1. - off.abs() / radius;
            let alpha = (fade * 64.) as u32;
            let point = |t: f32| {
                let (x, z) = if axis == 0 { (off, t) } else { (t, off) };
                [center[0] + x, center[1], center[2] + z]
            };
            lines.push(Line {
                points: vec![point(-half), point(half)],
                width: 1.,
                color: 0xffffff00 | alpha,
            });
        }
    }
    lines
}

/// The grid's spacing for a floor `radius` across: the round step giving
/// about five lines each side. At a fixed metre a 1.3 m tree got two
/// crossing lines, which read as strays, not a grid.
fn grid_step(radius: f32) -> f32 {
    [0.1, 0.25, 0.5, 1., 2., 5., 10., 25., 50., 100.]
        .into_iter()
        .find(|s| radius / s <= 6.)
        .unwrap_or(100.)
}

pub struct ModelView {
    renderer: Arc<Renderer>,
    viewport: Arc<Viewport>,
    camera: Camera,
    extent: Extent,
    drag: Option<(MouseButton, Point<Pixels>)>,
    /// Where the view was last painted, for zooming at the cursor.
    bounds: Rc<Cell<Bounds<Pixels>>>,
    /// The floor grid's spacing in metres, named on the view.
    step: f32,
}

impl Default for ModelView {
    fn default() -> Self {
        let renderer = Arc::new(Renderer::default());
        Self {
            viewport: renderer.viewport(),
            renderer,
            camera: Camera::default(),
            extent: Extent {
                center: [0.; 3],
                size: 1.,
            },
            drag: None,
            bounds: Rc::default(),
            step: 1.,
        }
    }
}

impl ModelView {
    /// Shows `parts` (a mesh and where it sits in model space), framed.
    /// `mirror` as [`Camera::mirror`]: left-handed game data.
    pub fn show(&mut self, parts: Vec<(Arc<Mesh>, Mat4)>, mirror: bool) {
        let [lo, hi] = if parts.is_empty() {
            [[0.; 3]; 2]
        } else {
            math::bounds(parts.iter().flat_map(|(mesh, local)| {
                math::corners(mesh.bounds()).map(|c| math::transform(local, c))
            }))
        };
        let size = (0..3).map(|i| hi[i] - lo[i]).fold(0.01, f32::max);
        let center: [f32; 3] = std::array::from_fn(|i| (lo[i] + hi[i]) / 2.);
        // Just under the model's lowest point, so its base does not flicker
        // against the floor.
        let across = (hi[0] - lo[0]).max(hi[2] - lo[2]);
        let base = [center[0], lo[1] - size * 0.002, center[2]];
        let ground = floor(base, across * 0.35, size * 0.9);
        self.step = grid_step(size * 0.9);
        // A hair above the floor, so the lines do not fight its depth.
        let lines = grid(
            [base[0], base[1] + size * 0.001, base[2]],
            size * 0.9,
            self.step,
        );
        let parts = parts.into_iter().chain(ground.map(|m| (m, math::IDENTITY)));
        self.renderer.set_scene(&Scene {
            batches: parts
                .map(|(mesh, local)| {
                    Batch::new(
                        mesh,
                        Arc::new(Instances {
                            matrices: vec![local],
                            ids: vec![None],
                        }),
                    )
                })
                .collect(),
            // Less ambient and more sun than the scene default, so leaves
            // and branches separate by their facing: under a bright even
            // sky pale foliage read as one flat mass. Still above the
            // default overall, so dark models keep their outline.
            light: Light {
                sun_color: [0.62; 3],
                sky: [0.55; 3],
                ground: [0.32; 3],
                ..Default::default()
            },
            lines: Arc::new(lines),
            ..Default::default()
        });
        // The largest side about fills the shorter viewport side: seen
        // three-quarter, a model's box is smaller than its largest side, and
        // at 80% an olive tree took under half the pane's height.
        self.extent = Extent {
            center,
            size: size * 0.95,
        };
        self.camera.mirror = mirror;
        self.frame();
    }

    /// Back to the three-quarter view of the whole model.
    pub fn frame(&mut self) {
        self.camera = Camera {
            yaw: 0.6,
            tilt: 0.95,
            // A mild lens: orthographic flattened buildings, a wide one
            // bent their walls.
            fov: 0.5,
            mirror: self.camera.mirror,
            ..Default::default()
        };
    }

    /// The view, filling its parent. Left- or right-drag turns, middle-drag
    /// (or Shift-drag) pans, the wheel zooms at the cursor and a
    /// double-click frames the model again: the editors' viewports orbit on
    /// right-drag and pan on middle-drag, and a preview that panned on
    /// right-drag broke that habit. Left-drag turns too, there being
    /// nothing to select. `lens` finds this view in
    /// the host; `changed` redraws whatever caches the host's pane.
    pub fn render<V: 'static>(
        &self,
        id: impl Into<ElementId>,
        lens: fn(&mut V) -> &mut ModelView,
        changed: fn(&mut V, &mut Context<V>),
        window: &mut Window,
        cx: &mut Context<V>,
    ) -> Stateful<Div> {
        let (viewport, camera, extent) = (self.viewport.clone(), self.camera, self.extent);
        let (bounds, renderer) = (self.bounds.clone(), self.renderer.clone());
        let host = window.current_view();
        let mut view = div()
            .id(id)
            .relative()
            .size_full()
            .overflow_hidden()
            .bg(rgb(BACKDROP))
            .child(
                canvas(
                    |_, _, _| (),
                    move |b, _, window, cx| {
                        bounds.set(b);
                        viewport.paint(camera, extent, b, window);
                        // Uploads spread over frames, and a cached pane does
                        // not paint again by itself: redraw it until the model
                        // is all there.
                        if renderer.pending() {
                            cx.notify(host);
                        }
                    },
                )
                .absolute()
                .inset_0(),
            )
            // What a grid square is, top left: hosts put their key hints
            // along the bottom.
            .child(
                crate::widgets::overlay_pill(format!("Grid {} m", self.step))
                    .absolute()
                    .left_2()
                    .top_2(),
            );
        for button in [MouseButton::Left, MouseButton::Right, MouseButton::Middle] {
            view = view
                .on_mouse_down(
                    button,
                    cx.listener(move |v: &mut V, e: &MouseDownEvent, _, cx| {
                        let m = lens(v);
                        if button == MouseButton::Left && e.click_count == 2 {
                            m.drag = None;
                            m.frame();
                            changed(v, cx);
                            return;
                        }
                        m.drag = Some((button, e.position));
                    }),
                )
                .on_mouse_up(
                    button,
                    cx.listener(move |v: &mut V, _, _, _| lens(v).drag = None),
                );
        }
        view.on_mouse_move(cx.listener(move |v: &mut V, e: &MouseMoveEvent, _, cx| {
            let m = lens(v);
            let Some((button, last)) = m.drag else {
                return;
            };
            // Released outside the view: its mouse-up went elsewhere.
            if e.pressed_button != Some(button) {
                m.drag = None;
                return;
            }
            m.drag = Some((button, e.position));
            let delta = e.position - last;
            if button != MouseButton::Middle && !e.modifiers.shift {
                m.camera.orbit_by(delta);
            } else {
                m.camera.pan_by(delta);
            }
            changed(v, cx);
        }))
        .on_scroll(cx.listener(move |v: &mut V, e: &ScrollEvent, _, cx| {
            let m = lens(v);
            let delta = match e.delta {
                ScrollDelta::Pixels(p) => f32::from(p.y),
                ScrollDelta::Lines(p) => p.y * 32.,
            };
            m.camera.zoom_at(delta, e.position, m.bounds.get());
            changed(v, cx);
        }))
    }
}
