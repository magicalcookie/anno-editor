// Vector drawing for canvas overlays: strokes, fills, circles, box outlines,
// and the cubic links node graphs use, with hit tests in screen space.
use gpui::{Background, Pixels, Point, Window, point, px};
use std::f32::consts::PI;

// Screen coordinates past this are off any display; projections of points
// at or behind a perspective eye come out NaN or enormous, and the path
// tessellator asserts on non-finite input (and overflows on huge input).
const MAX_COORD: f32 = 1e6;

/// Whether a point can be drawn: finite and within `MAX_COORD`.
fn drawable(p: Point<Pixels>) -> bool {
    let (x, y) = (f32::from(p.x), f32::from(p.y));
    x.is_finite() && y.is_finite() && x.abs() < MAX_COORD && y.abs() < MAX_COORD
}

/// The runs of drawable points, split where a point is not.
fn runs(points: &[Point<Pixels>]) -> impl Iterator<Item = &[Point<Pixels>]> {
    points
        .split(|p| !drawable(*p))
        .filter(|run| !run.is_empty())
}

/// Strokes the polyline through `points`. Points that cannot be drawn
/// (behind a perspective camera) break it into separate pieces.
pub fn line(
    points: &[Point<Pixels>],
    width: f32,
    color: impl Into<Background>,
    window: &mut Window,
) {
    let mut builder = gpui::PathBuilder::stroke(px(width));
    let mut any = false;
    for run in runs(points) {
        builder.move_to(run[0]);
        for p in &run[1..] {
            builder.line_to(*p);
        }
        any = true;
    }
    if !any {
        return;
    }
    if let Ok(path) = builder.build() {
        window.paint_path(path, color);
    }
}

/// Fills the polygon; nothing if any corner cannot be drawn.
pub fn polygon(points: &[Point<Pixels>], color: impl Into<Background>, window: &mut Window) {
    if points.len() < 3 || !points.iter().all(|p| drawable(*p)) {
        return;
    }
    let mut builder = gpui::PathBuilder::fill();
    builder.move_to(points[0]);
    for p in &points[1..] {
        builder.line_to(*p);
    }
    builder.close();
    if let Ok(path) = builder.build() {
        window.paint_path(path, color);
    }
}

fn ring(center: Point<Pixels>, radius: f32, closed: bool) -> Vec<Point<Pixels>> {
    (0..32 + usize::from(closed))
        .map(|i| {
            let angle = i as f32 * PI / 16.;
            center + point(px(angle.cos() * radius), px(angle.sin() * radius))
        })
        .collect()
}

pub fn circle(
    center: Point<Pixels>,
    radius: f32,
    width: f32,
    color: impl Into<Background>,
    window: &mut Window,
) {
    line(&ring(center, radius, true), width, color, window);
}

pub fn filled_circle(
    center: Point<Pixels>,
    radius: f32,
    color: impl Into<Background>,
    window: &mut Window,
) {
    polygon(&ring(center, radius, false), color, window);
}

/// Strokes the 12 edges of a projected box. Corners follow [`crate::math::corners`].
pub fn box_outline(
    corners: &[Point<Pixels>; 8],
    width: f32,
    color: impl Into<Background>,
    window: &mut Window,
) {
    let mut path = gpui::PathBuilder::stroke(px(width));
    let mut any = false;
    for i in 0..8 {
        for a in 0..3 {
            if i & (1 << a) == 0 && drawable(corners[i]) && drawable(corners[i | (1 << a)]) {
                any = true;
                path.move_to(corners[i]);
                path.line_to(corners[i | (1 << a)]);
            }
        }
    }
    if !any {
        return;
    }
    if let Ok(path) = path.build() {
        window.paint_path(path, color);
    }
}

/// A horizontal S-curve from an output port to an input port, as cubic
/// Bézier control points. The bend grows with distance but stays readable
/// for ports that are very close or far apart.
pub fn link(from: [f32; 2], to: [f32; 2]) -> [[f32; 2]; 4] {
    let bend = ((to[0] - from[0]).abs() * 0.5).clamp(50., 220.);
    [from, [from[0] + bend, from[1]], [to[0] - bend, to[1]], to]
}

pub fn curve_point(curve: [[f32; 2]; 4], t: f32) -> [f32; 2] {
    let s = 1. - t;
    std::array::from_fn(|k| {
        s * s * s * curve[0][k]
            + 3. * s * s * t * curve[1][k]
            + 3. * s * t * t * curve[2][k]
            + t * t * t * curve[3][k]
    })
}

// Links flatten to this many segments for both drawing and hit testing.
const CURVE_SEGMENTS: usize = 32;

/// Whether `p` lies within `radius` pixels of the curve.
pub fn curve_hit(curve: [[f32; 2]; 4], p: [f32; 2], radius: f32) -> bool {
    (0..CURVE_SEGMENTS).any(|i| {
        let a = curve_point(curve, i as f32 / CURVE_SEGMENTS as f32);
        let b = curve_point(curve, (i + 1) as f32 / CURVE_SEGMENTS as f32);
        let d = [b[0] - a[0], b[1] - a[1]];
        let length = d[0] * d[0] + d[1] * d[1];
        let t = if length > 0. {
            ((p[0] - a[0]) * d[0] + (p[1] - a[1]) * d[1]) / length
        } else {
            0.
        }
        .clamp(0., 1.);
        (p[0] - a[0] - t * d[0]).powi(2) + (p[1] - a[1] - t * d[1]).powi(2) <= radius * radius
    })
}

pub fn curve(curve: [[f32; 2]; 4], width: f32, color: impl Into<Background>, window: &mut Window) {
    let p = curve.map(|p| point(px(p[0]), px(p[1])));
    if !p.iter().all(|p| drawable(*p)) {
        return;
    }
    let mut path = gpui::PathBuilder::stroke(px(width));
    path.move_to(p[0]);
    path.cubic_bezier_to(p[3], p[1], p[2]);
    if let Ok(path) = path.build() {
        window.paint_path(path, color);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn undrawable_points_split_lines_and_drop_shapes() {
        let pts = [
            point(px(0.), px(0.)),
            point(px(10.), px(0.)),
            point(px(f32::NAN), px(5.)),
            point(px(20.), px(0.)),
            point(px(1e9), px(0.)),
            point(px(30.), px(0.)),
            point(px(40.), px(0.)),
        ];
        let pieces: Vec<usize> = runs(&pts).map(<[_]>::len).collect();
        assert_eq!(pieces, [2, 1, 2]);
        assert!(runs(&[point(px(f32::INFINITY), px(0.))]).next().is_none());
        assert!(drawable(point(px(-999.), px(5000.))));
    }

    #[test]
    fn links_start_and_end_at_ports_and_hit_near_the_curve_only() {
        let c = link([0., 0.], [300., 120.]);
        assert_eq!(curve_point(c, 0.), [0., 0.]);
        assert_eq!(curve_point(c, 1.), [300., 120.]);
        let mid = curve_point(c, 0.5);
        assert!(curve_hit(c, [mid[0] + 3., mid[1]], 7.));
        assert!(!curve_hit(c, [mid[0], mid[1] + 40.], 7.));
    }
}
