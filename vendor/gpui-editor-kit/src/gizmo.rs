// Viewport transform handles, their screen-space constraints, and box selection.
//
// The host owns the selection and the undo snapshot. A gesture only turns
// cursor movement into a `Delta`; apply it to each target's matrix as it was
// when the drag began, so rounding never accumulates across mouse events.
use crate::camera::{Camera, Extent};
use crate::math::{Mat4, Vec3, add, cross, dot, normalize, scale, sub};
use crate::theme;
use gpui::{Bounds, Pixels, Point, Rgba, Window, point, px};
use std::f32::consts::{PI, TAU};

#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub enum Mode {
    #[default]
    Move,
    Rotate,
    Scale,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Handle {
    Axis(usize),
    Plane(usize, usize),
    /// The center square: free movement in the screen plane, or uniform scale.
    Screen,
}

/// Screen length of an axis handle in logical pixels.
const LENGTH: f32 = 92.;

struct Shape {
    handle: Handle,
    points: Vec<[f32; 2]>,
    fill: bool,
    color: Rgba,
}

/// Handle layout for one pivot in one viewport.
#[derive(Clone, Debug)]
pub struct Frame {
    mode: Mode,
    pivot: Vec3,
    axes: [Vec3; 3],
    screen: [[f32; 2]; 3],
    origin: [f32; 2],
    length: f32,
    right: Vec3,
    down: Vec3,
}

/// A transform relative to the pose at the start of a drag.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Delta {
    Move(Vec3),
    /// Axis and angle in radians.
    Rotate(Vec3, f32),
    /// Optional axis, otherwise uniform.
    Scale(Option<Vec3>, f32),
}

fn xy(p: Point<Pixels>) -> [f32; 2] {
    [p.x.into(), p.y.into()]
}

fn point2(p: [f32; 2]) -> Point<Pixels> {
    point(px(p[0]), px(p[1]))
}

/// Solves `p = u * a + v * b`.
fn solve(p: [f32; 2], a: [f32; 2], b: [f32; 2]) -> Option<[f32; 2]> {
    let determinant = a[0] * b[1] - a[1] * b[0];
    // Near edge-on planes cannot give stable drag distances.
    (determinant.abs() > 180.).then(|| {
        [
            (p[0] * b[1] - p[1] * b[0]) / determinant,
            (a[0] * p[1] - a[1] * p[0]) / determinant,
        ]
    })
}

fn segment_distance(p: [f32; 2], a: [f32; 2], b: [f32; 2]) -> f32 {
    let d = sub(b, a);
    let t = (dot(sub(p, a), d) / dot(d, d).max(1e-6)).clamp(0., 1.);
    let delta = sub(p, add(a, scale(d, t)));
    dot(delta, delta).sqrt()
}

fn snap(value: f32, step: f32, enabled: bool) -> f32 {
    if enabled {
        (value / step).round() * step
    } else {
        value
    }
}

fn axis_label(shapes: &mut Vec<Shape>, axis: usize, position: [f32; 2]) {
    let strokes: &[&[[f32; 2]]] = match axis {
        0 => &[&[[-3., -4.], [3., 4.]], &[[3., -4.], [-3., 4.]]],
        1 => &[&[[-3., -4.], [0., 0.], [3., -4.]], &[[0., 0.], [0., 4.]]],
        2 => &[&[[-3., -4.], [3., -4.], [-3., 4.], [3., 4.]]],
        _ => unreachable!(),
    };
    for stroke in strokes {
        shapes.push(Shape {
            handle: Handle::Axis(axis),
            points: stroke.iter().map(|p| add(position, *p)).collect(),
            fill: false,
            color: theme::get().axes[axis],
        });
    }
}

impl Frame {
    /// Lays out handles at `pivot`. With `local`, axes follow that matrix's
    /// basis, orthonormalized so sheared native transforms still get usable
    /// handles. Returns `None` when the pivot is off screen or degenerate.
    pub fn new(
        mode: Mode,
        pivot: Vec3,
        local: Option<&Mat4>,
        camera: Camera,
        bounds: Bounds<Pixels>,
        extent: Extent,
    ) -> Option<Self> {
        let origin = xy(camera.project(pivot, bounds, extent));
        if !bounds.contains(&point2(origin)) {
            return None;
        }
        let right = camera.right();
        let down = camera.down();
        let pixel_unit = sub(
            xy(camera.project(add(pivot, right), bounds, extent)),
            origin,
        );
        let unit = dot(pixel_unit, pixel_unit).sqrt();
        if !unit.is_finite() || unit < 1e-6 {
            return None;
        }
        let length = LENGTH / unit;
        let mut axes = [[1., 0., 0.], [0., 1., 0.], [0., 0., 1.]];
        if let Some(m) = local {
            axes[0] = normalize([m[0], m[1], m[2]])?;
            let y = [m[4], m[5], m[6]];
            axes[1] = normalize(sub(y, scale(axes[0], dot(y, axes[0]))))?;
            axes[2] = normalize(cross(axes[0], axes[1]))?;
            if dot(axes[2], [m[8], m[9], m[10]]) < 0. {
                axes[2] = scale(axes[2], -1.);
            }
        }
        let screen = axes.map(|axis| {
            let s = sub(
                xy(camera.project(add(pivot, scale(axis, length)), bounds, extent)),
                origin,
            );
            // A perspective axis tip behind the eye has no screen position;
            // treat it like an axis pointing at the viewer.
            if s.iter().all(|v| v.is_finite()) {
                s
            } else {
                [0.; 2]
            }
        });
        Some(Self {
            mode,
            pivot,
            axes,
            screen,
            origin,
            length,
            right,
            down,
        })
    }

    pub fn mode(&self) -> Mode {
        self.mode
    }

    pub fn pivot(&self) -> Vec3 {
        self.pivot
    }

    /// The ground-plane handle when it is usable from this view, else the
    /// screen handle. Use it to drag an object by its body.
    pub fn ground_handle(&self) -> Handle {
        if solve([0.; 2], self.screen[0], self.screen[2]).is_some() {
            Handle::Plane(0, 2)
        } else {
            Handle::Screen
        }
    }

    fn shapes(&self) -> Vec<Shape> {
        let mut shapes = Vec::new();
        if self.mode == Mode::Rotate {
            for (axis, color) in theme::get().axes.into_iter().enumerate() {
                let u = self.screen[(axis + 1) % 3];
                let v = self.screen[(axis + 2) % 3];
                if solve([0.; 2], u, v).is_none() {
                    continue;
                }
                shapes.push(Shape {
                    handle: Handle::Axis(axis),
                    points: (0..=72)
                        .map(|i| {
                            let a = i as f32 * TAU / 72.;
                            add(
                                self.origin,
                                scale(add(scale(u, a.cos()), scale(v, a.sin())), 0.85),
                            )
                        })
                        .collect(),
                    fill: false,
                    color,
                });
                axis_label(&mut shapes, axis, add(self.origin, scale(u, 0.98)));
            }
            return shapes;
        }
        for (axis, color) in theme::get().axes.into_iter().enumerate() {
            let s = self.screen[axis];
            let end = add(self.origin, s);
            // Axes pointing at the viewer collapse to a dot and cannot be dragged.
            if dot(s, s) < 100. {
                continue;
            }
            shapes.push(Shape {
                handle: Handle::Axis(axis),
                points: vec![add(self.origin, scale(s, 0.13)), end],
                fill: false,
                color,
            });
            axis_label(
                &mut shapes,
                axis,
                add(end, scale(s, 14. / dot(s, s).sqrt())),
            );
            let tip = if self.mode == Mode::Move {
                let d = scale(s, 1. / dot(s, s).sqrt());
                let side = [-d[1], d[0]];
                vec![
                    end,
                    add(sub(end, scale(d, 12.)), scale(side, 5.)),
                    sub(sub(end, scale(d, 12.)), scale(side, 5.)),
                    end,
                ]
            } else {
                vec![
                    add(end, [-4.; 2]),
                    add(end, [4., -4.]),
                    add(end, [4.; 2]),
                    add(end, [-4., 4.]),
                    add(end, [-4.; 2]),
                ]
            };
            shapes.push(Shape {
                handle: Handle::Axis(axis),
                points: tip,
                fill: true,
                color,
            });
        }
        if self.mode == Mode::Move {
            for (a, b) in [(0, 1), (1, 2), (2, 0)] {
                if solve([0.; 2], self.screen[a], self.screen[b]).is_none() {
                    continue;
                }
                shapes.push(Shape {
                    handle: Handle::Plane(a, b),
                    // Out from the pivot, where a small model sits: at
                    // 0.21 the squares covered the selected prop itself.
                    points: [
                        [0.28, 0.28],
                        [0.45, 0.28],
                        [0.45, 0.45],
                        [0.28, 0.45],
                        [0.28, 0.28],
                    ]
                    .map(|v| {
                        add(
                            self.origin,
                            add(scale(self.screen[a], v[0]), scale(self.screen[b], v[1])),
                        )
                    })
                    .to_vec(),
                    fill: true,
                    color: theme::get().axes[3 - a - b],
                });
            }
        }
        shapes.push(Shape {
            handle: Handle::Screen,
            points: [[-5., -5.], [5., -5.], [5., 5.], [-5., 5.], [-5., -5.]]
                .map(|p| add(self.origin, p))
                .to_vec(),
            // Hollow: filled, it sat on the pivot and hid a small prop
            // under it. Its hit area is the same square either way.
            fill: false,
            color: theme::get().gizmo_center,
        });
        shapes
    }

    pub fn hit(&self, cursor: Point<Pixels>) -> Option<Handle> {
        let cursor = xy(cursor);
        let relative = sub(cursor, self.origin);
        if self.mode != Mode::Rotate && relative.iter().all(|v| v.abs() <= 8.) {
            return Some(Handle::Screen);
        }
        if self.mode == Mode::Move {
            for (a, b) in [(0, 1), (1, 2), (2, 0)] {
                if solve(relative, self.screen[a], self.screen[b])
                    .is_some_and(|uv| uv.iter().all(|v| (0.18..=0.43).contains(v)))
                {
                    return Some(Handle::Plane(a, b));
                }
            }
        }
        self.shapes()
            .iter()
            .filter(|shape| matches!(shape.handle, Handle::Axis(_)))
            .filter_map(|shape| {
                let distance = shape
                    .points
                    .windows(2)
                    .map(|p| segment_distance(cursor, p[0], p[1]))
                    .fold(f32::INFINITY, f32::min);
                (distance <= 8.).then_some((distance, shape.handle))
            })
            .min_by(|a, b| a.0.total_cmp(&b.0))
            .map(|(_, handle)| handle)
    }

    /// Converts a drag from `start` to `cursor` into a transform. With
    /// `snap`, moves go in steps of that many world units, rotations in 15
    /// degrees and scales in 0.1 steps.
    pub fn delta(
        &self,
        handle: Handle,
        start: Point<Pixels>,
        cursor: Point<Pixels>,
        snap_step: Option<f32>,
    ) -> Option<Delta> {
        let snapping = snap_step.is_some();
        let step = snap_step.unwrap_or(1.).max(0.001);
        let (start, cursor) = (xy(start), xy(cursor));
        let d = sub(cursor, start);
        match self.mode {
            Mode::Move => {
                let translation = match handle {
                    Handle::Axis(a) => scale(
                        self.axes[a],
                        snap(
                            dot(d, self.screen[a]) / dot(self.screen[a], self.screen[a])
                                * self.length,
                            step,
                            snapping,
                        ),
                    ),
                    Handle::Plane(a, b) => {
                        let uv = solve(d, self.screen[a], self.screen[b])?;
                        add(
                            scale(self.axes[a], snap(uv[0] * self.length, step, snapping)),
                            scale(self.axes[b], snap(uv[1] * self.length, step, snapping)),
                        )
                    }
                    Handle::Screen => add(
                        scale(
                            self.right,
                            snap(d[0] * self.length / LENGTH, step, snapping),
                        ),
                        scale(self.down, snap(d[1] * self.length / LENGTH, step, snapping)),
                    ),
                };
                Some(Delta::Move(translation))
            }
            Mode::Rotate => {
                let Handle::Axis(a) = handle else { return None };
                let u = self.screen[(a + 1) % 3];
                let v = self.screen[(a + 2) % 3];
                let start = solve(sub(start, self.origin), u, v)?;
                let end = solve(sub(cursor, self.origin), u, v)?;
                if dot(end, end) < 0.001 {
                    return None;
                }
                let angle =
                    (end[1].atan2(end[0]) - start[1].atan2(start[0]) + PI).rem_euclid(TAU) - PI;
                let handedness = dot(
                    cross(self.axes[(a + 1) % 3], self.axes[(a + 2) % 3]),
                    self.axes[a],
                )
                .signum();
                Some(Delta::Rotate(
                    self.axes[a],
                    snap(angle, PI / 12., snapping) * handedness,
                ))
            }
            Mode::Scale => {
                let factor = match handle {
                    Handle::Axis(a) => {
                        1. + dot(d, self.screen[a]) / dot(self.screen[a], self.screen[a])
                    }
                    Handle::Screen => ((d[0] - d[1]) * 0.008).exp(),
                    Handle::Plane(_, _) => return None,
                };
                // Scale never crosses zero; a reflection is not a drag result.
                let factor = snap(factor, 0.1, snapping).clamp(0.01, 100.);
                Some(Delta::Scale(
                    match handle {
                        Handle::Axis(a) => Some(self.axes[a]),
                        _ => None,
                    },
                    factor,
                ))
            }
        }
    }

    /// Paints the handles. `active` is the handle being dragged, which stays
    /// highlighted even when the cursor leaves it.
    pub fn paint(&self, active: Option<Handle>, window: &mut Window) {
        let hover = active.or_else(|| self.hit(window.mouse_position()));
        let shapes = self.shapes();
        // A dark stroke under every line first, so red and green axes hold
        // against straw and soil; drawn in its own pass so no underlay
        // crosses another handle's colour.
        for shape in &shapes {
            let points = shape.points.iter().map(|p| point2(*p)).collect::<Vec<_>>();
            crate::paint::line(
                &points,
                if shape.fill { 3. } else { 4.5 },
                theme::get().gizmo_underlay,
                window,
            );
        }
        for shape in shapes {
            // Near white, not the amber highlight: hovered, an axis read as
            // part of the selection's orange outline and box.
            let color = if hover == Some(shape.handle) {
                theme::get().gizmo_hover
            } else {
                shape.color
            };
            let points = shape.points.iter().map(|p| point2(*p)).collect::<Vec<_>>();
            crate::paint::line(&points, if shape.fill { 1.5 } else { 2.5 }, color, window);
            // Plane squares barely filled until hovered, so the model under
            // them shows through; arrow tips stay solid.
            if shape.fill {
                let alpha = match shape.handle {
                    Handle::Plane(..) if hover != Some(shape.handle) => 0.125,
                    _ => 0.44,
                };
                crate::paint::polygon(&points, color.alpha(alpha), window);
            }
        }
    }
}

impl Delta {
    fn vector(&self, p: Vec3) -> Vec3 {
        match *self {
            Self::Move(_) => p,
            Self::Rotate(axis, angle) => add(
                add(scale(p, angle.cos()), scale(cross(axis, p), angle.sin())),
                scale(axis, dot(axis, p) * (1. - angle.cos())),
            ),
            Self::Scale(Some(axis), factor) => add(p, scale(axis, dot(axis, p) * (factor - 1.))),
            Self::Scale(None, factor) => scale(p, factor),
        }
    }

    /// Applies the delta around `pivot`. Basis vectors are transformed
    /// directly, so shear and reflection in native matrices survive rotation.
    pub fn apply(&self, matrix: &Mat4, pivot: Vec3) -> Mat4 {
        let mut result = *matrix;
        for col in 0..3 {
            let v = self.vector([matrix[col * 4], matrix[col * 4 + 1], matrix[col * 4 + 2]]);
            result[col * 4..col * 4 + 3].copy_from_slice(&v);
        }
        let p = add(
            self.vector(sub([matrix[12], matrix[13], matrix[14]], pivot)),
            pivot,
        );
        result[12..15].copy_from_slice(&if let Self::Move(d) = self {
            add(p, *d)
        } else {
            p
        });
        result
    }
}

/// One drag on a gizmo handle.
#[derive(Clone, Debug)]
pub struct Gesture {
    pub frame: Frame,
    pub handle: Handle,
    start: Point<Pixels>,
    moved: bool,
}

impl Gesture {
    /// Starts a drag if `cursor` is on a handle.
    pub fn begin(frame: Frame, cursor: Point<Pixels>) -> Option<Self> {
        let handle = frame.hit(cursor)?;
        Some(Self::with_handle(frame, handle, cursor))
    }

    pub fn with_handle(frame: Frame, handle: Handle, cursor: Point<Pixels>) -> Self {
        Self {
            frame,
            handle,
            start: cursor,
            moved: false,
        }
    }

    /// `None` until the cursor leaves a 3 px dead zone, so clicks never edit.
    /// `snap_step`, when snapping, is the move step in world units.
    pub fn delta(&mut self, cursor: Point<Pixels>, snap_step: Option<f32>) -> Option<Delta> {
        let d = sub(xy(cursor), xy(self.start));
        self.moved |= dot(d, d) > 9.;
        if !self.moved {
            return None;
        }
        self.frame.delta(self.handle, self.start, cursor, snap_step)
    }
}

/// Rubber-band selection in screen space.
#[derive(Clone, Copy, Debug)]
pub struct Marquee {
    pub start: Point<Pixels>,
    pub cursor: Point<Pixels>,
    /// Shift-drag adds to the selection instead of replacing it.
    pub additive: bool,
}

impl Marquee {
    pub fn new(start: Point<Pixels>, additive: bool) -> Self {
        Self {
            start,
            cursor: start,
            additive,
        }
    }

    pub fn rect(&self) -> Bounds<Pixels> {
        Bounds::from_corners(
            point(
                self.start.x.min(self.cursor.x),
                self.start.y.min(self.cursor.y),
            ),
            point(
                self.start.x.max(self.cursor.x),
                self.start.y.max(self.cursor.y),
            ),
        )
    }

    /// Shorter than 4 px is a click, not a box.
    pub fn is_click(&self) -> bool {
        let d = sub(xy(self.cursor), xy(self.start));
        dot(d, d) < 16.
    }

    /// Ids whose projected pivot lies inside the box and the viewport.
    pub fn select<K>(
        &self,
        pivots: impl IntoIterator<Item = (K, Vec3)>,
        camera: Camera,
        bounds: Bounds<Pixels>,
        extent: Extent,
    ) -> Vec<K> {
        let rect = self.rect();
        pivots
            .into_iter()
            .filter_map(|(id, p)| {
                let p = camera.project(p, bounds, extent);
                (bounds.contains(&p)
                    && p.x >= rect.left()
                    && p.y >= rect.top()
                    && p.x <= rect.right()
                    && p.y <= rect.bottom())
                .then_some(id)
            })
            .collect()
    }

    pub fn paint(&self, window: &mut Window) {
        window.paint_quad(gpui::quad(
            self.rect(),
            0.,
            theme::get().accent.alpha(0.16),
            px(1.),
            theme::get().accent,
            gpui::BorderStyle::Solid,
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::math::{IDENTITY, transform, translation};
    use gpui::size;

    const EXTENT: Extent = Extent {
        center: [0.; 3],
        size: 100.,
    };

    fn bounds() -> Bounds<Pixels> {
        Bounds::new(point(px(0.), px(0.)), size(px(800.), px(600.)))
    }

    fn frame(mode: Mode, local: Option<&Mat4>) -> Frame {
        Frame::new(
            mode,
            [1., 2., 3.],
            local,
            Camera::perspective(),
            bounds(),
            EXTENT,
        )
        .unwrap()
    }

    fn close(a: &[f32], b: &[f32]) {
        assert!(
            a.iter().zip(b).all(|(a, b)| (a - b).abs() < 1e-3),
            "{a:?} != {b:?}"
        );
    }

    fn tip(frame: &Frame, axis: usize, t: f32) -> Point<Pixels> {
        point2(add(frame.origin, scale(frame.screen[axis], t)))
    }

    #[test]
    fn axis_drags_move_in_world_units_and_snap() {
        let frame = frame(Mode::Move, None);
        assert_eq!(frame.hit(tip(&frame, 0, 0.7)), Some(Handle::Axis(0)));
        assert_eq!(frame.hit(point2(frame.origin)), Some(Handle::Screen));
        let start = tip(&frame, 0, 0.7);
        let end = tip(&frame, 0, 0.7 + 0.23 / frame.length * 10.);
        let Some(Delta::Move(d)) = frame.delta(Handle::Axis(0), start, end, None) else {
            panic!()
        };
        close(&d, &[2.3, 0., 0.]);
        let Some(Delta::Move(d)) = frame.delta(Handle::Axis(0), start, end, Some(1.)) else {
            panic!()
        };
        close(&d, &[2., 0., 0.]);
        // A finer step for small props: 2.3 m lands on 2.5 at half a metre.
        let Some(Delta::Move(d)) = frame.delta(Handle::Axis(0), start, end, Some(0.5)) else {
            panic!()
        };
        close(&d, &[2.5, 0., 0.]);
    }

    #[test]
    fn rotation_preserves_shear_and_reflection_around_the_pivot() {
        let mut m = IDENTITY;
        m[0] = -2.;
        m[4] = 0.5;
        m[12..15].copy_from_slice(&[4., 2., 3.]);
        let delta = Delta::Rotate([0., 1., 0.], PI / 2.);
        let rotated = delta.apply(&m, [1., 2., 3.]);
        close(&translation(&rotated), &[1., 2., 0.]);
        let back = Delta::Rotate([0., 1., 0.], -PI / 2.).apply(&rotated, [1., 2., 3.]);
        close(&back, &m);
    }

    #[test]
    fn local_handles_are_orthonormal_for_sheared_matrices() {
        let mut m = IDENTITY;
        m[4] = 0.7;
        m[8] = -0.3;
        let frame = frame(Mode::Move, Some(&m));
        for a in 0..3 {
            for b in 0..3 {
                let expected = if a == b { 1. } else { 0. };
                assert!((dot(frame.axes[a], frame.axes[b]) - expected).abs() < 1e-5);
            }
        }
        assert!(dot(frame.axes[2], [m[8], m[9], m[10]]) > 0.);
    }

    #[test]
    fn scaling_never_crosses_zero_and_gestures_ignore_clicks() {
        let frame = frame(Mode::Scale, None);
        let start = tip(&frame, 1, 1.);
        let far = tip(&frame, 1, -5.);
        let Some(Delta::Scale(Some(_), factor)) = frame.delta(Handle::Axis(1), start, far, None)
        else {
            panic!()
        };
        assert_eq!(factor, 0.01);
        let mut gesture = Gesture::with_handle(frame, Handle::Screen, start);
        assert!(gesture.delta(start + point(px(2.), px(0.)), None).is_none());
        assert!(gesture.delta(start + point(px(9.), px(0.)), None).is_some());
        // Returning to the start after moving still reports an identity delta.
        let Some(Delta::Scale(None, f)) = gesture.delta(start, None) else {
            panic!()
        };
        assert_eq!(f, 1.);
        let m = Delta::Scale(None, 2.).apply(&IDENTITY, [1., 0., 0.]);
        close(&transform(&m, [1., 0., 0.]), &[1., 0., 0.]);
    }

    #[test]
    fn box_selection_uses_projected_pivots_in_both_drag_directions() {
        let camera = Camera::default();
        let pivots = [(1, [0., 0., 0.]), (2, [20., 0., 0.]), (3, [400., 0., 0.])];
        let center = bounds().center();
        let forward = Marquee {
            start: center - point(px(10.), px(10.)),
            cursor: center + point(px(10.), px(10.)),
            additive: false,
        };
        assert_eq!(forward.select(pivots, camera, bounds(), EXTENT), vec![1]);
        let backward = Marquee {
            start: forward.cursor + point(px(200.), px(0.)),
            cursor: forward.start,
            additive: false,
        };
        assert_eq!(
            backward.select(pivots, camera, bounds(), EXTENT),
            vec![1, 2]
        );
        assert!(Marquee::new(center, false).is_click());
    }

    #[test]
    fn perspective_handles_and_marquee_follow_the_lens() {
        let camera = Camera {
            fov: 1.,
            ..Camera::perspective()
        };
        let pivot = [1., 2., 3.];
        let frame = Frame::new(Mode::Move, pivot, None, camera, bounds(), EXTENT).unwrap();
        assert_eq!(frame.hit(tip(&frame, 0, 0.7)), Some(Handle::Axis(0)));
        // A short drag along the projected axis moves along that axis and
        // lands the pivot close to the cursor.
        let start = point2(frame.origin);
        let end = tip(&frame, 0, 0.1);
        let Some(Delta::Move(d)) = frame.delta(Handle::Axis(0), start, end, None) else {
            panic!()
        };
        assert!(d[0] > 0. && d[1] == 0. && d[2] == 0.);
        let moved = camera.project(add(pivot, d), bounds(), EXTENT);
        assert!(
            f32::from((moved - end).x).abs() < 1.5 && f32::from((moved - end).y).abs() < 1.5,
            "{moved:?} {end:?}"
        );

        let eye = camera.eye(bounds(), EXTENT).unwrap();
        let behind = add(eye, scale(camera.toward(), 5.));
        let marquee = Marquee {
            start: bounds().origin,
            cursor: point(bounds().right(), bounds().bottom()),
            additive: false,
        };
        assert_eq!(
            marquee.select([(1, pivot), (2, behind)], camera, bounds(), EXTENT),
            vec![1]
        );
    }
}

/// Side of [`orientation`] in px.
pub const ORIENTATION: f32 = 64.;

/// The world's axes as `camera` sees them, for a viewport corner: X, Y and
/// Z from a centre dot in the axis colours, each labelled at its tip, the
/// far end of an axis pointing away drawn dimmer. `north` names +Z (a map's
/// north) when set. Orbiting without it lost which way the map faced.
pub fn orientation(camera: Camera, north: bool) -> gpui::Div {
    use gpui::{IntoElement, ParentElement, Styled, canvas, div};
    let (right, down) = (camera.right(), camera.down());
    let toward = cross(right, down);
    // Room inside the rim for the letters past each tip.
    let r = ORIENTATION / 2. - 15.;
    // Each axis: its screen offset from the centre, and whether it points
    // at the viewer (drawn on top) or away.
    let axes: Vec<([f32; 2], bool)> = (0..3)
        .map(|a| {
            let mut v = [0.; 3];
            v[a] = 1.;
            ([dot(v, right) * r, dot(v, down) * r], dot(v, toward) >= 0.)
        })
        .collect();
    // The gizmo's and the Inspector's letters; north is a mark on the rim,
    // where +Z meets it, not a fourth name for the axis.
    let label = |a: usize| ["X", "Y", "Z"][a];
    let north_at = {
        let [x, z] = [dot([0., 0., 1.], right), dot([0., 0., 1.], down)];
        let len = (x * x + z * z).sqrt();
        (north && len > 0.2).then(|| [x / len, z / len])
    };
    let painted = axes.clone();
    let mut widget = div()
        .relative()
        .size(px(ORIENTATION))
        .rounded_full()
        .bg(theme::get().floating)
        .border_1()
        .border_color(theme::get().floating_border)
        .child(
            canvas(
                |_, _, _| (),
                move |bounds, _, window, _| {
                    let c = bounds.center();
                    // Away-pointing axes first, so the near ones draw over.
                    let mut order: Vec<usize> = (0..3).collect();
                    order.sort_by_key(|&a| painted[a].1);
                    for a in order {
                        let ([x, y], near) = painted[a];
                        let tip = c + point(px(x), px(y));
                        let color = if near {
                            theme::get().axes[a]
                        } else {
                            theme::get().axes[a].alpha(0.44)
                        };
                        crate::paint::line(&[c, tip], 2., color, window);
                        crate::paint::filled_circle(tip, 2.5, color, window);
                    }
                    crate::paint::filled_circle(c, 2., theme::get().text, window);
                    // A notch on the rim at north, as a compass bezel marks it.
                    if let Some([x, y]) = north_at {
                        let edge = ORIENTATION / 2. - 1.;
                        let a = c + point(px(x * edge), px(y * edge));
                        let b = c + point(px(x * (edge - 6.)), px(y * (edge - 6.)));
                        crate::paint::line(&[a, b], 2., theme::get().text, window);
                    }
                },
            )
            .size_full(),
        );
    for (a, ([x, y], near)) in axes.into_iter().enumerate() {
        // Past the tip, so the letter does not sit on its line.
        let len = (x * x + y * y).sqrt().max(1e-3);
        let (lx, ly) = (x + x / len * 8., y + y / len * 8.);
        widget = widget.child(
            div()
                .absolute()
                .left(px(ORIENTATION / 2. + lx - 4.))
                .top(px(ORIENTATION / 2. + ly - 7.))
                .text_size(px(11.))
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .text_color(if near {
                    theme::get().axes[a]
                } else {
                    theme::get().axes[a].alpha(0.56)
                })
                .child(label(a))
                .into_any_element(),
        );
    }
    widget
}
