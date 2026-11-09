// Orbit camera shared by the CPU overlays, GPU renderer and picking.
//
// Orthographic by default; a nonzero `fov` gives a perspective lens. Yaw turns
// around world Y, tilt is the angle from straight down (0 is a top-down map).
// Pan is in screen pixels so dragging stays locked to the cursor at every zoom.
//
// Both modes share one model. Points on the plane through the anchor that
// faces the viewer project identically in either mode: `scale` pixels per
// world unit, shifted by `pan`. Perspective puts the eye `distance` in front
// of that plane and pan slides the eye sideways instead of shifting the
// image, so the optical axis stays at the viewport center and long pans do
// not skew the view.
use crate::math::{Vec3, dot, sub};
use crate::pick::Ray;
use gpui::{Bounds, Pixels, Point, ScaledPixels, point, px};

/// What the camera looks at when it has no explicit center: `size` world
/// units fit the shorter viewport side at zoom 1, centered on `center`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Extent {
    pub center: Vec3,
    pub size: f32,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Camera {
    pub yaw: f32,
    pub tilt: f32,
    pub zoom: f32,
    pub pan: [f32; 2],
    /// Orbit pivot. `None` follows the extent's center.
    pub center: Option<Vec3>,
    /// Field of view across the shorter viewport side, in radians. 0 is
    /// orthographic; perspective values are clamped to 0.05..=2.5.
    pub fov: f32,
    /// Shows world z mirrored, for left-handed data (Direct3D-style games,
    /// where +z is north): a top-down view then has +z up the screen
    /// instead of down. Picking, panning and gizmos follow.
    pub mirror: bool,
}

impl Default for Camera {
    fn default() -> Self {
        Self {
            yaw: 0.,
            tilt: 0.,
            zoom: 1.,
            pan: [0.; 2],
            center: None,
            fov: 0.,
            mirror: false,
        }
    }
}

// Tilt stops short of horizontal: at 90 degrees the ground plane is edge-on
// and orthographic plane picking has no solution.
const TILT: std::ops::RangeInclusive<f32> = 0.05..=1.25;
const ZOOM: std::ops::RangeInclusive<f32> = 0.25..=1_000_000.;
const FOV: std::ops::RangeInclusive<f32> = 0.05..=2.5;
// Perspective near plane as a fraction of the eye distance. Geometry closer
// than this clips; the renderer's reversed f32 depth stays precise anyway.
const NEAR: f32 = 1. / 200.;

impl Camera {
    /// A three-quarter view for model and scene viewports. Despite the name it
    /// is orthographic; set `fov` for a perspective lens.
    pub fn perspective() -> Self {
        Self {
            tilt: 0.85,
            yaw: 0.5,
            ..Default::default()
        }
    }

    pub fn is_perspective(self) -> bool {
        self.fov > 0.
    }

    pub fn anchor(self, extent: impl Into<Extent>) -> Vec3 {
        let extent = extent.into();
        self.center.unwrap_or(extent.center)
    }

    /// Pixels per world unit, at the anchor's depth in perspective.
    pub fn scale(self, bounds: Bounds<Pixels>, extent: impl Into<Extent>) -> f32 {
        let extent = extent.into();
        f32::from(bounds.size.width).min(f32::from(bounds.size.height)) * 0.92
            / extent.size.max(0.001)
            * self.zoom
    }

    /// Eye to anchor plane in world units, `None` when orthographic. Chosen
    /// so the lens covers the shorter viewport side at the anchor's depth
    /// exactly as `scale` does; it does not depend on the viewport size.
    pub fn distance(self, extent: impl Into<Extent>) -> Option<f32> {
        let extent = extent.into();
        let tan = (self.fov.clamp(*FOV.start(), *FOV.end()) * 0.5).tan();
        self.is_perspective()
            .then(|| extent.size.max(0.001) / (1.84 * self.zoom * tan))
    }

    /// The eye position in perspective, `None` when orthographic.
    pub fn eye(self, bounds: Bounds<Pixels>, extent: impl Into<Extent>) -> Option<Vec3> {
        let extent = extent.into();
        let l = self.distance(extent)?;
        let s = self.scale(bounds, extent);
        let (anchor, right, down, toward) = (
            self.anchor(extent),
            self.right(),
            self.down(),
            self.toward(),
        );
        Some(std::array::from_fn(|a| {
            anchor[a] + toward[a] * l - (right[a] * self.pan[0] + down[a] * self.pan[1]) / s
        }))
    }

    /// -1 when z is mirrored.
    fn z_sign(self) -> f32 {
        if self.mirror { -1. } else { 1. }
    }

    pub fn right(self) -> Vec3 {
        [self.yaw.cos(), 0., -self.yaw.sin() * self.z_sign()]
    }

    /// World direction of screen-down at the anchor's depth.
    pub fn down(self) -> Vec3 {
        [
            self.yaw.sin() * self.tilt.cos(),
            -self.tilt.sin(),
            self.yaw.cos() * self.tilt.cos() * self.z_sign(),
        ]
    }

    /// Unit vector from the scene toward the viewer, the reverse view direction.
    pub fn toward(self) -> Vec3 {
        [
            self.yaw.sin() * self.tilt.sin(),
            self.tilt.cos(),
            self.yaw.cos() * self.tilt.sin() * self.z_sign(),
        ]
    }

    /// `p` relative to the anchor along right, down and toward.
    fn local(self, p: Vec3, extent: Extent) -> Vec3 {
        let d = sub(p, self.anchor(extent));
        [
            dot(d, self.right()),
            dot(d, self.down()),
            dot(d, self.toward()),
        ]
    }

    /// How much larger a point at anchor-relative depth `z` appears than one
    /// on the anchor plane. 1 in orthographic, `None` at or behind the eye.
    fn magnify(self, z: f32, extent: Extent) -> Option<f32> {
        match self.distance(extent) {
            None => Some(1.),
            Some(l) => (l - z > l * 1e-5).then(|| l / (l - z)),
        }
    }

    /// Screen position of `p`. In perspective, points at or behind the eye
    /// project to NaN, which every `Bounds::contains` check rejects.
    pub fn project(
        self,
        p: Vec3,
        bounds: Bounds<Pixels>,
        extent: impl Into<Extent>,
    ) -> Point<Pixels> {
        let extent = extent.into();
        let [x, y, z] = self.local(p, extent);
        let s = self.scale(bounds, extent);
        let k = self.magnify(z, extent).unwrap_or(f32::NAN);
        let center = bounds.center();
        point(
            center.x + px((x * s + self.pan[0]) * k),
            center.y + px((y * s + self.pan[1]) * k),
        )
    }

    /// Distance toward the viewer from the anchor plane. Larger is closer.
    pub fn depth(self, p: Vec3, extent: impl Into<Extent>) -> f32 {
        self.local(p, extent.into())[2]
    }

    /// World-unit depth range around the anchor that the GPU depth buffer
    /// covers in orthographic. The bounds suit meter-scale worlds from a
    /// single prop to ~30 km maps. Perspective depth reaches to infinity.
    pub fn depth_span(self, extent: impl Into<Extent>) -> f32 {
        let extent = extent.into();
        (extent.size / self.zoom * 8.).clamp(256., 262144.)
    }

    /// How far a `ray` must travel to cover everything the renderer draws:
    /// the depth span in orthographic, unbounded in perspective.
    pub fn reach(self, extent: impl Into<Extent>) -> f32 {
        if self.is_perspective() {
            f32::INFINITY
        } else {
            self.depth_span(extent)
        }
    }

    /// Visible world-unit span, the larger of width and ground depth. In
    /// perspective it measures the footprint on the anchor's horizontal
    /// plane, or the depth span once the horizon is in view.
    pub fn view_extent(self, bounds: Bounds<Pixels>, extent: impl Into<Extent>) -> f32 {
        let extent = extent.into();
        if self.is_perspective() {
            let height = self.anchor(extent)[1];
            let corners = [
                bounds.origin,
                bounds.top_right(),
                bounds.bottom_left(),
                bounds.bottom_right(),
            ]
            .map(|c| self.pick_plane(c, bounds, extent, height));
            let span = self.depth_span(extent);
            let Some(corners) = corners.into_iter().collect::<Option<Vec<_>>>() else {
                return span;
            };
            let widest = corners
                .iter()
                .flat_map(|a| {
                    corners
                        .iter()
                        .map(move |b| crate::math::length(sub(*a, *b)))
                })
                .fold(0., f32::max);
            return widest.min(span);
        }
        let scale = self.scale(bounds, extent);
        (f32::from(bounds.size.width) / scale)
            .max(f32::from(bounds.size.height) / (scale * self.tilt.cos().max(0.1)))
    }

    /// The point at the viewport center, on the anchor's horizontal plane.
    /// A perspective view that misses that plane falls back to the anchor's depth.
    pub fn view_center(self, bounds: Bounds<Pixels>, extent: impl Into<Extent>) -> Vec3 {
        let extent = extent.into();
        let anchor = self.anchor(extent);
        let scale = self.scale(bounds, extent);
        if let Some(l) = self.distance(extent) {
            return self
                .pick_plane(bounds.center(), bounds, extent, anchor[1])
                .unwrap_or_else(|| {
                    let eye = self.eye(bounds, extent).expect("perspective eye");
                    std::array::from_fn(|a| eye[a] - self.toward()[a] * l)
                });
        }
        let x = -self.pan[0] / scale;
        let z = -self.pan[1] / (scale * self.tilt.cos());
        [
            anchor[0] + x * self.yaw.cos() + z * self.yaw.sin(),
            anchor[1],
            anchor[2] + (-x * self.yaw.sin() + z * self.yaw.cos()) * self.z_sign(),
        ]
    }

    /// Intersects the camera ray through `cursor` with the plane `y = height`.
    /// Works anywhere, including outside the scene's extent. Perspective
    /// rays that point away from the plane miss.
    pub fn pick_plane(
        self,
        cursor: Point<Pixels>,
        bounds: Bounds<Pixels>,
        extent: impl Into<Extent>,
        height: f32,
    ) -> Option<Vec3> {
        let extent = extent.into();
        let scale = self.scale(bounds, extent);
        if !height.is_finite() || !scale.is_finite() || scale <= 0. || self.tilt.cos().abs() < 0.001
        {
            return None;
        }
        if self.is_perspective() {
            let ray = self.ray(cursor, bounds, extent);
            let t = (f64::from(height) - ray.origin[1]) / ray.direction[1];
            if !t.is_finite() || t <= 0. {
                return None;
            }
            let mut hit = ray.point(t);
            hit[1] = height;
            return Some(hit);
        }
        let anchor = self.anchor(extent);
        let x = (f32::from(cursor.x - bounds.center().x) - self.pan[0]) / scale;
        let z = ((f32::from(cursor.y - bounds.center().y) - self.pan[1]) / scale
            + (height - anchor[1]) * self.tilt.sin())
            / self.tilt.cos();
        Some([
            anchor[0] + x * self.yaw.cos() + z * self.yaw.sin(),
            height,
            anchor[2] + (-x * self.yaw.sin() + z * self.yaw.cos()) * self.z_sign(),
        ])
    }

    /// The view ray through `cursor`, with a unit direction. Orthographic
    /// rays start at the near end of the depth range, perspective rays at the eye.
    pub fn ray(
        self,
        cursor: Point<Pixels>,
        bounds: Bounds<Pixels>,
        extent: impl Into<Extent>,
    ) -> Ray {
        let extent = extent.into();
        let center = self.anchor(extent);
        let scale = self.scale(bounds, extent);
        let offset = cursor - bounds.center();
        let (right, down, toward) = (self.right(), self.down(), self.toward());
        if let (Some(l), Some(eye)) = (self.distance(extent), self.eye(bounds, extent)) {
            let focal = f64::from(scale) * f64::from(l);
            let x = f64::from(f32::from(offset.x)) / focal;
            let y = f64::from(f32::from(offset.y)) / focal;
            let length = (1. + x * x + y * y).sqrt();
            return Ray {
                origin: eye.map(f64::from),
                direction: std::array::from_fn(|a| {
                    (f64::from(right[a]) * x + f64::from(down[a]) * y - f64::from(toward[a]))
                        / length
                }),
            };
        }
        let x = f64::from((f32::from(offset.x) - self.pan[0]) / scale);
        let y = f64::from((f32::from(offset.y) - self.pan[1]) / scale);
        // Points on the screen plane through the anchor have depth zero, so
        // the near plane is half the depth span toward the viewer.
        let near = f64::from(self.depth_span(extent) * 0.5);
        Ray {
            origin: std::array::from_fn(|a| {
                f64::from(center[a])
                    + f64::from(right[a]) * x
                    + f64::from(down[a]) * y
                    + f64::from(toward[a]) * near
            }),
            direction: toward.map(|v| -f64::from(v)),
        }
    }

    /// World to clip rows `[x, y, z, w]` for the viewport alone: x and y span
    /// -1..1 across `bounds`, y up. Each row is `[cx, cy, cz, constant]`.
    ///
    /// Standard depth maps near to 0 and far to 1. `reversed` maps near to 1
    /// and far to 0; with f32 depth that keeps perspective precise out to the
    /// horizon, which is why the renderer uses it.
    pub(crate) fn view_rows(
        self,
        bounds: Bounds<Pixels>,
        extent: Extent,
        reversed: bool,
    ) -> [[f64; 4]; 4] {
        let anchor = self.anchor(extent).map(f64::from);
        let s = f64::from(self.scale(bounds, extent));
        let [width, height] =
            [bounds.size.width, bounds.size.height].map(|v| f64::from(f32::from(v)));
        // `k * axis . (p - anchor) + c` as a row.
        let row = |axis: Vec3, k: f64, c: f64| {
            let axis = axis.map(|v| f64::from(v) * k);
            [
                axis[0],
                axis[1],
                axis[2],
                c - axis[0] * anchor[0] - axis[1] * anchor[1] - axis[2] * anchor[2],
            ]
        };
        let pan = self.pan.map(f64::from);
        let (right, down, toward) = (self.right(), self.down(), self.toward());
        match self.distance(extent).map(f64::from) {
            None => {
                let span = f64::from(self.depth_span(extent));
                let sign = if reversed { 1. } else { -1. };
                [
                    row(right, 2. * s / width, 2. * pan[0] / width),
                    row(down, -2. * s / height, -2. * pan[1] / height),
                    row(toward, sign / span, 0.5),
                    [0., 0., 0., 1.],
                ]
            }
            Some(l) => {
                let near = l * f64::from(NEAR);
                let w = row(toward, -1., l);
                [
                    row(right, 2. * s * l / width, 2. * pan[0] * l / width),
                    row(down, -2. * s * l / height, -2. * pan[1] * l / height),
                    if reversed {
                        [0., 0., 0., near]
                    } else {
                        [w[0], w[1], w[2], w[3] - near]
                    },
                    w,
                ]
            }
        }
    }

    /// Column-major clip matrix for the GPU renderer.
    ///
    /// `bounds` is the logical viewport the camera was laid out in; `canvas`
    /// is the same rectangle in device pixels inside a `target` of the given
    /// size. Orthographic depth maps the span around the anchor to 0 (near)
    /// through 1. Perspective depth is `1 - near / distance` with an infinite
    /// far plane; it loses precision far from the eye, so hosts with large
    /// perspective scenes should reverse it.
    pub fn clip(
        self,
        bounds: Bounds<Pixels>,
        extent: impl Into<Extent>,
        canvas: Bounds<ScaledPixels>,
        target: [u32; 2],
    ) -> [[f32; 4]; 4] {
        to_target(self.view_rows(bounds, extent.into(), false), canvas, target)
    }

    /// `clip` with reversed depth: near maps to 1, far (or infinity in
    /// perspective) to 0. Clear depth to 0 and test with `GreaterEqual`.
    pub fn clip_reversed(
        self,
        bounds: Bounds<Pixels>,
        extent: impl Into<Extent>,
        canvas: Bounds<ScaledPixels>,
        target: [u32; 2],
    ) -> [[f32; 4]; 4] {
        to_target(self.view_rows(bounds, extent.into(), true), canvas, target)
    }

    /// The clip volume of the viewport alone, wherever it sits in the
    /// window, for skipping world boxes that cannot reach the screen.
    pub fn frustum(self, bounds: Bounds<Pixels>, extent: impl Into<Extent>) -> Frustum {
        Frustum::from_rows(&self.view_rows(bounds, extent.into(), true))
    }

    /// Rebases the orbit pivot without moving any projected point. Screen-space
    /// pan must be absorbed here, otherwise the old focus orbits off screen.
    /// In perspective the zoom changes too, so the eye stays where it is.
    pub fn begin_orbit(
        &mut self,
        hit: Option<Vec3>,
        bounds: Bounds<Pixels>,
        extent: impl Into<Extent>,
    ) {
        let extent = extent.into();
        let pivot = hit.unwrap_or_else(|| self.view_center(bounds, extent));
        let projected = self.project(pivot, bounds, extent);
        let offset = [
            projected.x - bounds.center().x,
            projected.y - bounds.center().y,
        ]
        .map(f32::from);
        // A pivot behind the eye cannot be orbited without a jump.
        if !offset.iter().all(|v| v.is_finite()) {
            return;
        }
        if let Some(l) = self.distance(extent) {
            let z = self.depth(pivot, extent);
            self.zoom *= l / (l - z);
        }
        self.center = Some(pivot);
        self.pan = offset;
    }

    /// Moves the view so points at the anchor's depth follow the cursor.
    pub fn pan_by(&mut self, delta: Point<Pixels>) {
        self.pan[0] += f32::from(delta.x);
        self.pan[1] += f32::from(delta.y);
    }

    /// Moves the view so `grab` follows the cursor. In perspective, nearer
    /// and farther points slide at their own speed; pass the surface point
    /// under the cursor when the drag began. Orthographic is `pan_by`.
    pub fn pan_at(&mut self, delta: Point<Pixels>, grab: Vec3, extent: impl Into<Extent>) {
        let extent = extent.into();
        let k = self.magnify(self.depth(grab, extent), extent).unwrap_or(1.);
        self.pan[0] += f32::from(delta.x) / k;
        self.pan[1] += f32::from(delta.y) / k;
    }

    pub fn orbit_by(&mut self, delta: Point<Pixels>) {
        // Mirrored, the same yaw step turns the other way on screen.
        self.yaw += f32::from(delta.x) * 0.008 * self.z_sign();
        self.tilt = (self.tilt + f32::from(delta.y) * 0.006).clamp(*TILT.start(), *TILT.end());
    }

    /// Zooms by a scroll delta in pixels, keeping the point under `cursor` on
    /// the anchor plane fixed. Perspective dollies the eye toward that point.
    pub fn zoom_at(&mut self, delta: f32, cursor: Point<Pixels>, bounds: Bounds<Pixels>) {
        let old = self.zoom;
        // Perspective rebasing can leave zoom outside the range; only refuse
        // to go further out of it, never snap back.
        let target = old * (delta * 0.0025).exp();
        self.zoom = if target > old {
            target.min(ZOOM.end().max(old))
        } else {
            target.max(ZOOM.start().min(old))
        };
        let ratio = self.zoom / old;
        let offset = cursor - bounds.center();
        self.pan[0] = f32::from(offset.x) - (f32::from(offset.x) - self.pan[0]) * ratio;
        self.pan[1] = f32::from(offset.y) - (f32::from(offset.y) - self.pan[1]) * ratio;
    }

    /// `zoom_at` toward `target`, usually the surface under the cursor. In
    /// perspective it rebases onto the target first, so the dolly heads for
    /// the surface instead of the anchor plane. Orthographic ignores `target`.
    pub fn zoom_toward(
        &mut self,
        delta: f32,
        cursor: Point<Pixels>,
        target: Option<Vec3>,
        bounds: Bounds<Pixels>,
        extent: impl Into<Extent>,
    ) {
        if let Some(target) = target.filter(|_| self.is_perspective()) {
            self.begin_orbit(Some(target), bounds, extent);
        }
        self.zoom_at(delta, cursor, bounds);
    }

    /// Centers `p` and zooms in to at least 4x.
    pub fn focus(&mut self, p: Vec3) {
        self.center = Some(p);
        self.pan = [0.; 2];
        self.zoom = self.zoom.max(4.);
    }

    /// Fits `size` world units around `center`, keeping the view direction.
    pub fn frame(&mut self, center: Vec3, size: f32, extent: impl Into<Extent>) {
        let extent = extent.into();
        self.center = Some(center);
        self.pan = [0.; 2];
        self.zoom = (extent.size / (size.max(0.01) * 1.6)).clamp(*ZOOM.start(), *ZOOM.end());
    }
}

/// Maps viewport clip rows into a `canvas` inside a larger `target`, as a
/// column-major matrix.
pub(crate) fn to_target(
    rows: [[f64; 4]; 4],
    canvas: Bounds<ScaledPixels>,
    target: [u32; 2],
) -> [[f32; 4]; 4] {
    let [width, height] = target.map(f64::from);
    let origin = [canvas.origin.x, canvas.origin.y].map(f64::from);
    let size = [canvas.size.width, canvas.size.height].map(f64::from);
    let [x, y, z, w] = rows;
    let x: [f64; 4] = std::array::from_fn(|c| {
        x[c] * size[0] / width + w[c] * ((2. * origin[0] + size[0]) / width - 1.)
    });
    let y: [f64; 4] = std::array::from_fn(|c| {
        y[c] * size[1] / height + w[c] * (1. - (2. * origin[1] + size[1]) / height)
    });
    let rows = [x, y, z, w];
    std::array::from_fn(|c| std::array::from_fn(|r| rows[r][c] as f32))
}

/// Six world-space planes around everything a view draws: the viewport
/// edges, the near plane and, in orthographic, the far end of the depth span.
#[derive(Clone, Copy, Debug)]
pub struct Frustum {
    /// `[a, b, c, d]` with `a x + b y + c z + d >= 0` inside.
    planes: [[f64; 4]; 6],
}

impl Frustum {
    /// From reversed-depth clip rows `[x, y, z, w]`: visible points have
    /// `-w <= x, y <= w` and `0 <= z <= w`. Each bound is linear in world
    /// space, so it holds behind the eye too, where `w` turns negative.
    pub(crate) fn from_rows(rows: &[[f64; 4]; 4]) -> Self {
        let [x, y, z, w] = *rows;
        let plane = |a: [f64; 4], k: f64| -> [f64; 4] { std::array::from_fn(|i| w[i] + k * a[i]) };
        Self {
            planes: [
                plane(x, 1.),
                plane(x, -1.),
                plane(y, 1.),
                plane(y, -1.),
                plane(z, -1.),
                z,
            ],
        }
    }

    /// The six planes, `[a, b, c, d]` with `a x + b y + c z + d >= 0` inside.
    pub fn planes(&self) -> [[f64; 4]; 6] {
        self.planes
    }

    /// Whether the box `[min, max]` lies wholly outside one plane, so
    /// nothing in it can be drawn. Boxes that touch a plane, straddle the
    /// eye or are empty (min above max) are kept.
    pub fn excludes(&self, bounds: [Vec3; 2]) -> bool {
        self.planes.iter().any(|p| {
            // The corner furthest inside this plane, and the size of the terms.
            let (mut reach, mut size) = (p[3], p[3].abs());
            for a in 0..3 {
                let [low, high] = [bounds[0][a], bounds[1][a]].map(f64::from);
                reach += p[a] * if p[a] >= 0. { high } else { low };
                size += (p[a] * low).abs().max((p[a] * high).abs());
            }
            // The GPU transforms in f32, so keep boxes a rounding error outside.
            reach < -1e-5 * size
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::size;

    const EXTENT: Extent = Extent {
        center: [500., 20., -300.],
        size: 1000.,
    };

    fn bounds() -> Bounds<Pixels> {
        Bounds::new(point(px(40.), px(30.)), size(px(800.), px(600.)))
    }

    fn camera() -> Camera {
        Camera {
            yaw: 0.7,
            tilt: 0.9,
            zoom: 3.,
            pan: [37., -12.],
            center: None,
            fov: 0.,
            mirror: false,
        }
    }

    fn lens() -> Camera {
        Camera {
            fov: 1.1,
            ..camera()
        }
    }

    fn near(a: Vec3, b: Vec3, tolerance: f32) {
        assert!(
            a.iter().zip(b).all(|(a, b)| (a - b).abs() < tolerance),
            "{a:?} {b:?}"
        );
    }

    // Points around the anchor, all in front of the perspective eye.
    const POINTS: [Vec3; 4] = [
        [500., 20., -300.],
        [612., 85., -250.],
        [400., -60., -420.],
        [380., -40., -500.],
    ];

    fn close(a: Point<Pixels>, b: Point<Pixels>) {
        assert!(
            f32::from((a - b).x).abs() < 0.05 && f32::from((a - b).y).abs() < 0.05,
            "{a:?} {b:?}"
        );
    }

    #[test]
    fn mirrored_views_put_plus_z_up_and_stay_consistent() {
        // Top-down, unrotated: +z runs down the screen, mirrored up.
        let top = |mirror| Camera {
            mirror,
            ..Camera::default()
        };
        let (north, south) = ([500., 20., -200.], [500., 20., -400.]);
        let y = |c: Camera, p| f32::from(c.project(p, bounds(), EXTENT).y);
        assert!(y(top(false), north) > y(top(false), south));
        assert!(y(top(true), north) < y(top(true), south));
        // x stays to the right either way: a mirror, not a rotation.
        let x = |c: Camera, p| f32::from(c.project(p, bounds(), EXTENT).x);
        let east = [600., 20., -300.];
        assert!(x(top(true), east) > x(top(true), EXTENT.center));
        // Every screen <-> world mapping agrees under the mirror, in both
        // lenses: plane picks, rays and the GPU clip matrix.
        for camera in [camera(), lens()].map(|c| Camera { mirror: true, ..c }) {
            for p in POINTS {
                let cursor = camera.project(p, bounds(), EXTENT);
                let picked = camera.pick_plane(cursor, bounds(), EXTENT, p[1]).unwrap();
                near(picked, p, 0.05);
                let ray = camera.ray(cursor, bounds(), EXTENT);
                let t = (f64::from(p[1]) - ray.origin[1]) / ray.direction[1];
                near(ray.point(t), p, 0.05);
            }
            let canvas = bounds().scale(1.);
            let m = camera.clip(bounds(), EXTENT, canvas, [840, 630]);
            for p in POINTS {
                let clip: [f32; 4] =
                    std::array::from_fn(|r| (0..3).map(|c| m[c][r] * p[c]).sum::<f32>() + m[3][r]);
                let screen = camera.project(p, bounds(), EXTENT);
                let sx = (clip[0] / clip[3] + 1.) / 2. * 840.;
                let sy = (1. - clip[1] / clip[3]) / 2. * 630.;
                close(point(px(sx), px(sy)), screen);
            }
        }
    }

    #[test]
    fn plane_picks_invert_projection_after_pan_zoom_and_rotation() {
        let p = [612., 85., -250.];
        let cursor = camera().project(p, bounds(), EXTENT);
        let picked = camera().pick_plane(cursor, bounds(), EXTENT, 85.).unwrap();
        assert!(
            picked.iter().zip(p).all(|(a, b)| (a - b).abs() < 0.05),
            "{picked:?}"
        );
    }

    #[test]
    fn rays_pass_through_the_projected_point_and_start_at_the_near_plane() {
        let camera = camera();
        let p = [400., -60., -420.];
        let ray = camera.ray(camera.project(p, bounds(), EXTENT), bounds(), EXTENT);
        let t = (f64::from(p[1]) - ray.origin[1]) / ray.direction[1];
        let hit = ray.point(t);
        assert!(
            hit.iter().zip(p).all(|(a, b)| (a - b).abs() < 0.05),
            "{hit:?}"
        );
        let near = camera.depth(ray.point(0.), EXTENT);
        assert!((near - camera.depth_span(EXTENT) * 0.5).abs() < 0.05);
    }

    #[test]
    fn clip_matrix_agrees_with_cpu_projection_and_depth() {
        let camera = camera();
        let bounds = bounds();
        let scale = 1.5;
        let canvas = bounds.scale(scale);
        let target = [1500, 1100];
        let m = camera.clip(bounds, EXTENT, canvas, target);
        for p in [[500., 20., -300.], [700., 140., -150.], [380., -40., -500.]] {
            let clip: [f32; 4] =
                std::array::from_fn(|r| (0..3).map(|c| m[c][r] * p[c]).sum::<f32>() + m[3][r]);
            let screen = camera.project(p, bounds, EXTENT);
            let x = (clip[0] + 1.) / 2. * target[0] as f32 / scale;
            let y = (1. - clip[1]) / 2. * target[1] as f32 / scale;
            close(point(px(x), px(y)), screen);
            let depth = 0.5 - camera.depth(p, EXTENT) / camera.depth_span(EXTENT);
            assert!((clip[2] - depth).abs() < 1e-5);
        }
    }

    #[test]
    fn orbit_pivot_rebase_keeps_the_pivot_on_screen() {
        let mut camera = camera();
        let pivot = [560., 10., -340.];
        let before = camera.project(pivot, bounds(), EXTENT);
        camera.begin_orbit(Some(pivot), bounds(), EXTENT);
        close(camera.project(pivot, bounds(), EXTENT), before);
        camera.orbit_by(point(px(80.), px(-30.)));
        close(camera.project(pivot, bounds(), EXTENT), before);
    }

    #[test]
    fn scroll_zoom_keeps_the_point_under_the_cursor() {
        let mut camera = camera();
        let cursor = point(px(220.), px(400.));
        let under = camera.pick_plane(cursor, bounds(), EXTENT, 20.).unwrap();
        camera.zoom_at(-160., cursor, bounds());
        close(camera.project(under, bounds(), EXTENT), cursor);
    }

    #[test]
    fn perspective_rays_start_at_the_eye_and_pass_through_projected_points() {
        let camera = lens();
        let eye = camera.eye(bounds(), EXTENT).unwrap();
        for p in POINTS {
            let cursor = camera.project(p, bounds(), EXTENT);
            let ray = camera.ray(cursor, bounds(), EXTENT);
            near(ray.point(0.), eye, 0.01);
            let length = ray.direction.iter().map(|v| v * v).sum::<f64>().sqrt();
            assert!((length - 1.).abs() < 1e-6);
            let t = (f64::from(p[1]) - ray.origin[1]) / ray.direction[1];
            near(ray.point(t), p, 0.05);
            let picked = camera.pick_plane(cursor, bounds(), EXTENT, p[1]).unwrap();
            near(picked, p, 0.05);
        }
        // Points on the anchor plane project as they would orthographically.
        let flat = Camera { fov: 0., ..camera };
        let on_plane = camera.anchor(EXTENT);
        close(
            camera.project(on_plane, bounds(), EXTENT),
            flat.project(on_plane, bounds(), EXTENT),
        );
        // Behind the eye there is no screen position, and nothing to pick.
        let behind = std::array::from_fn(|a| eye[a] + camera.toward()[a] * 10.);
        assert!(!bounds().contains(&camera.project(behind, bounds(), EXTENT)));
        let up = point(px(440.), px(40.));
        let looking_up = Camera {
            tilt: 1.25,
            ..camera
        };
        assert!(
            looking_up
                .pick_plane(up, bounds(), EXTENT, -1000.)
                .is_none()
        );
    }

    #[test]
    fn perspective_clip_matrix_agrees_with_cpu_projection_in_both_depth_orders() {
        let camera = lens();
        let bounds = bounds();
        let scale = 1.5;
        let canvas = bounds.scale(scale);
        let target = [1500, 1100];
        let m = camera.clip(bounds, EXTENT, canvas, target);
        let reversed = camera.clip_reversed(bounds, EXTENT, canvas, target);
        let apply = |m: &[[f32; 4]; 4], p: Vec3| -> [f32; 4] {
            std::array::from_fn(|r| (0..3).map(|c| m[c][r] * p[c]).sum::<f32>() + m[3][r])
        };
        let mut last = None;
        let mut sorted = POINTS;
        sorted.sort_by(|a, b| {
            camera
                .depth(*b, EXTENT)
                .total_cmp(&camera.depth(*a, EXTENT))
        });
        for p in sorted {
            let clip = apply(&m, p);
            let x = (clip[0] / clip[3] + 1.) / 2. * target[0] as f32 / scale;
            let y = (1. - clip[1] / clip[3]) / 2. * target[1] as f32 / scale;
            close(point(px(x), px(y)), camera.project(p, bounds, EXTENT));
            let standard = clip[2] / clip[3];
            let flipped = apply(&reversed, p);
            let flipped = flipped[2] / flipped[3];
            assert!((0. ..1.).contains(&standard) && (0. ..1.).contains(&flipped));
            assert!((standard + flipped - 1.).abs() < 1e-5);
            // Sorted nearest first: standard depth grows, reversed shrinks.
            if let Some(previous) = last {
                assert!(standard > previous);
            }
            last = Some(standard);
        }
    }

    #[test]
    fn perspective_orbit_rebase_moves_nothing_and_keeps_the_pivot_put() {
        let mut camera = lens();
        let pivot = [560., 10., -340.];
        let before = POINTS.map(|p| camera.project(p, bounds(), EXTENT));
        let eye = camera.eye(bounds(), EXTENT).unwrap();
        let on_screen = camera.project(pivot, bounds(), EXTENT);
        camera.begin_orbit(Some(pivot), bounds(), EXTENT);
        near(camera.eye(bounds(), EXTENT).unwrap(), eye, 0.01);
        for (p, before) in POINTS.into_iter().zip(before) {
            close(camera.project(p, bounds(), EXTENT), before);
        }
        camera.orbit_by(point(px(80.), px(-30.)));
        close(camera.project(pivot, bounds(), EXTENT), on_screen);
        // The eye kept its distance to the pivot: a true orbit, no dolly.
        let distance = |e: Vec3| crate::math::length(sub(e, pivot));
        assert!((distance(camera.eye(bounds(), EXTENT).unwrap()) - distance(eye)).abs() < 0.01);
    }

    #[test]
    fn perspective_zoom_and_pan_keep_the_grabbed_point_under_the_cursor() {
        let mut camera = lens();
        let cursor = point(px(220.), px(400.));
        // Off the anchor plane: plain `zoom_at` would let it drift.
        let surface = camera.pick_plane(cursor, bounds(), EXTENT, 85.).unwrap();
        let eye = camera.eye(bounds(), EXTENT).unwrap();
        camera.zoom_toward(160., cursor, Some(surface), bounds(), EXTENT);
        close(camera.project(surface, bounds(), EXTENT), cursor);
        let closer = camera.eye(bounds(), EXTENT).unwrap();
        let distance = |e: Vec3| crate::math::length(sub(e, surface));
        assert!(distance(closer) < distance(eye) * 0.9);

        let grab = camera.pick_plane(cursor, bounds(), EXTENT, -60.).unwrap();
        let delta = point(px(-45.), px(30.));
        camera.pan_at(delta, grab, EXTENT);
        close(camera.project(grab, bounds(), EXTENT), cursor + delta);

        let mut flat = super::tests::camera();
        let under = flat.pick_plane(cursor, bounds(), EXTENT, 20.).unwrap();
        flat.zoom_toward(-160., cursor, Some([0.; 3]), bounds(), EXTENT);
        close(flat.project(under, bounds(), EXTENT), cursor);
    }

    /// What the CPU projection shows: inside the viewport, past the near
    /// plane, and within the orthographic depth span.
    fn shown(camera: Camera, p: Vec3) -> bool {
        let depth = camera.depth(p, EXTENT);
        let in_depth = match camera.distance(EXTENT) {
            Some(l) => l - depth >= l * NEAR,
            None => depth.abs() <= camera.depth_span(EXTENT) * 0.5,
        };
        in_depth && bounds().contains(&camera.project(p, bounds(), EXTENT))
    }

    #[test]
    fn frustum_excludes_only_boxes_with_nothing_on_screen() {
        let horizon = Camera {
            tilt: 1.25,
            ..lens()
        };
        let mirrored = Camera {
            mirror: true,
            ..lens()
        };
        let mut seed = 7u64;
        let mut random = move || {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (seed >> 11) as f32 / (1u64 << 53) as f32
        };
        for camera in [camera(), lens(), mirrored, horizon] {
            let frustum = camera.frustum(bounds(), EXTENT);
            let rows = camera.view_rows(bounds(), EXTENT, true);
            let (mut excluded, mut kept) = (0, 0);
            for _ in 0..2000 {
                // Centers to 800 m around the anchor, past the perspective
                // eye (~300 m out) too; sizes 1 m to 1 km.
                let center: Vec3 =
                    std::array::from_fn(|a| EXTENT.center[a] + (random() - 0.5) * 1600.);
                let half: Vec3 = std::array::from_fn(|_| 0.5 * 1000f32.powf(random()));
                let b = [0, 1].map(|s| {
                    std::array::from_fn(|a| center[a] + if s == 0 { -half[a] } else { half[a] })
                });
                // The same rule over all eight corners in clip space.
                let corners = crate::math::corners(b).map(|p| {
                    rows.map(|r| (0..3).map(|a| r[a] * f64::from(p[a])).sum::<f64>() + r[3])
                });
                let all = |f: &dyn Fn(&[f64; 4]) -> bool| corners.iter().all(f);
                let reference = all(&|c| c[0] < -c[3])
                    || all(&|c| c[0] > c[3])
                    || all(&|c| c[1] < -c[3])
                    || all(&|c| c[1] > c[3])
                    || all(&|c| c[2] > c[3])
                    || all(&|c| c[2] < 0.);
                assert_eq!(frustum.excludes(b), reference, "{camera:?} {b:?}");
                if !reference {
                    kept += 1;
                    continue;
                }
                excluded += 1;
                for i in 0..216 {
                    let t = [i % 6, i / 6 % 6, i / 36].map(|v| v as f32 / 5.);
                    let p = std::array::from_fn(|a| b[0][a] + (b[1][a] - b[0][a]) * t[a]);
                    assert!(!shown(camera, p), "{camera:?} {b:?} shows {p:?}");
                }
            }
            assert!(excluded > 100 && kept > 100, "{excluded} {kept}");
        }
    }

    #[test]
    fn frustum_keeps_edges_and_eye_crossings_and_drops_what_lies_beyond() {
        let cube = |p: Vec3, r: f32| [p.map(|v| v - r), p.map(|v| v + r)];
        let along = |p: Vec3, d: Vec3, t: f32| -> Vec3 { std::array::from_fn(|a| p[a] + d[a] * t) };
        let lens = lens();
        let frustum = lens.frustum(bounds(), EXTENT);
        let eye = lens.eye(bounds(), EXTENT).unwrap();
        let toward = lens.toward();
        assert!(!frustum.excludes(cube(lens.anchor(EXTENT), 0.1)));
        // Behind the eye, and in front of its near plane.
        assert!(frustum.excludes(cube(along(eye, toward, 50.), 10.)));
        assert!(frustum.excludes(cube(eye, 0.1)));
        // A box through the eye plane that reaches the view is drawn.
        let [a, b] = [along(eye, toward, 20.), along(eye, toward, -100.)];
        let through = [0, 1].map(|s| {
            std::array::from_fn(|i| {
                if s == 0 {
                    a[i].min(b[i])
                } else {
                    a[i].max(b[i])
                }
            })
        });
        assert!(!frustum.excludes(through));
        // Five pixels either side of the left edge.
        let y = bounds().center().y;
        for (dx, out) in [(-5., true), (5., false)] {
            let cursor = point(bounds().left() + px(dx), y);
            let p = lens.pick_plane(cursor, bounds(), EXTENT, 20.).unwrap();
            assert_eq!(frustum.excludes(cube(p, 0.01)), out);
        }
        // Orthographic: past either end of the depth span.
        let flat = camera();
        let frustum = flat.frustum(bounds(), EXTENT);
        let span = flat.depth_span(EXTENT);
        let anchor = flat.anchor(EXTENT);
        for (t, out) in [(-0.6, true), (-0.4, false), (0.4, false), (0.6, true)] {
            let p = along(anchor, flat.toward(), span * t);
            assert_eq!(frustum.excludes(cube(p, 1.)), out, "{t}");
        }
        let empty = [[f32::INFINITY; 3], [f32::NEG_INFINITY; 3]];
        assert!(!frustum.excludes(empty));
    }

    #[test]
    fn perspective_view_queries_see_the_horizon() {
        let camera = lens();
        let center = camera.view_center(bounds(), EXTENT);
        assert!((center[1] - camera.anchor(EXTENT)[1]).abs() < 1e-3);
        close(camera.project(center, bounds(), EXTENT), bounds().center());
        assert!(camera.view_extent(bounds(), EXTENT) > 0.);
        let horizon = Camera {
            tilt: 1.25,
            ..camera
        };
        assert_eq!(
            horizon.view_extent(bounds(), EXTENT),
            horizon.depth_span(EXTENT)
        );
        assert!(camera.reach(EXTENT).is_infinite());
    }
}
