// Scene lighting: sun, sky ambient, fog, and fitting the sun's shadow map to
// the part of the scene the camera can see.
use crate::math::{self, Vec3};

/// How a scene is lit. The default reproduces the renderer's original look:
/// constant ambient plus a fixed two-sided sun, no shadows and no fog.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Light {
    /// Direction toward the sun, world space. Normalized on use.
    pub sun: Vec3,
    /// Linear sun intensity per channel, added on surfaces facing the sun.
    pub sun_color: [f32; 3],
    /// Ambient on surfaces facing up; blends to `ground` on surfaces facing down.
    pub sky: [f32; 3],
    pub ground: [f32; 3],
    pub shadows: Option<Shadows>,
    pub fog: Option<Fog>,
}

impl Default for Light {
    fn default() -> Self {
        Self {
            sun: [0.35, 0.8, 0.45],
            // 0.35 times the length of `sun`: the original shader used that
            // vector unnormalized.
            sun_color: [0.3438; 3],
            sky: [0.65; 3],
            ground: [0.65; 3],
            shadows: None,
            fog: None,
        }
    }
}

/// Physically based shading for scenes lit in real units. Albedo decodes
/// from sRGB, [`Light`] and [`Fog`] colors are linear radiance (a clear sun
/// is about 3 to 5 times the sky), surfaces take GGX highlights from their
/// normal maps' gloss and occlusion from their surface maps, and the result
/// is scaled by `exposure` and tone mapped with Narkowicz's ACES fit.
/// Markers, lines and `Flat` meshes stay as given.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Tone {
    pub exposure: f32,
    /// How much occlusion maps darken the sun as well as the sky: 0 none.
    pub sun_occlusion: f32,
}

impl Default for Tone {
    fn default() -> Self {
        Self {
            exposure: 1.,
            sun_occlusion: 0.2,
        }
    }
}

/// What pixels no geometry covers show: a sky shading from `zenith` to
/// `horizon` (a band that narrows toward the horizon), the sun's disc and
/// glow from [`Light`]'s sun, and below the horizon `horizon` or an endless
/// sea. Colors are in the scene's units: linear radiance when it is
/// [`Tone`]d. Set the fog color to `horizon` so geometry fades into it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Sky {
    pub zenith: [f32; 3],
    pub horizon: [f32; 3],
    pub sea: Option<Sea>,
}

/// A flat sea out to the horizon at world height `level`: `color` lit by
/// the sky and sun, mirroring the sky at grazing angles, fogged with
/// distance like geometry.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Sea {
    pub level: f32,
    pub color: [f32; 3],
}

/// Screen-space ambient occlusion: crevices, contact with the ground and
/// the insides of foliage darken. Estimated from the view's depth after
/// opaque geometry draws, blurred, and multiplied over it; blended water,
/// the sky and overlays are untouched.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Occlusion {
    /// World units around a point searched for occluders.
    pub radius: f32,
    /// 0 to 1: how dark full occlusion gets.
    pub strength: f32,
}

impl Default for Occlusion {
    fn default() -> Self {
        Self {
            radius: 1.5,
            strength: 0.8,
        }
    }
}

/// A directional shadow map for the sun.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Shadows {
    /// Map side in texels, clamped to 256..=8192. GPU memory is
    /// `resolution² × 4` bytes per viewport: 16 MB at 2048, 64 MB at 4096.
    pub resolution: u32,
    /// Filter radius in shadow texels. 0 is a hard edge.
    pub softness: f32,
}

impl Default for Shadows {
    fn default() -> Self {
        Self {
            resolution: 2048,
            softness: 1.5,
        }
    }
}

/// Linear distance fog. Distance is from the eye in perspective, and from the
/// anchor plane in orthographic, where nearer points get none.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Fog {
    pub color: [f32; 3],
    /// World units where fog begins, and where it hides the surface entirely.
    pub start: f32,
    pub end: f32,
}

pub(super) const SHADOW_RESOLUTION: std::ops::RangeInclusive<u32> = 256..=8192;

/// World to shadow clip rows `[x, y, z, w]`, each `[cx, cy, cz, constant]`.
/// x and y span -1..1 across the map, depth 0 nearest the sun.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct ShadowFit {
    pub rows: [[f64; 4]; 4],
    /// World units per shadow texel.
    pub texel: f32,
    /// World units the 0..1 shadow depth range covers.
    pub depth: f32,
}

/// Fits an orthographic sun projection around `receivers`, deep enough to
/// include every caster between them and the sun.
pub(super) fn fit(
    sun: Vec3,
    casters: [Vec3; 2],
    receivers: [Vec3; 2],
    resolution: u32,
) -> Option<ShadowFit> {
    let sun = math::normalize(sun)?;
    if (0..3).any(|a| receivers[0][a] > receivers[1][a]) {
        return None;
    }
    let forward = sun.map(|v| -f64::from(v));
    let up = if forward[1].abs() < 0.99 {
        [0., 1., 0.]
    } else {
        [0., 0., 1.]
    };
    let cross = |a: [f64; 3], b: [f64; 3]| {
        [
            a[1] * b[2] - a[2] * b[1],
            a[2] * b[0] - a[0] * b[2],
            a[0] * b[1] - a[1] * b[0],
        ]
    };
    let x = cross(up, forward);
    let length = x.iter().map(|v| v * v).sum::<f64>().sqrt();
    let x = x.map(|v| v / length);
    let y = cross(forward, x);
    let dot = |a: [f64; 3], p: Vec3| (0..3).map(|i| a[i] * f64::from(p[i])).sum::<f64>();
    let range = |axis: [f64; 3], points: &[Vec3]| {
        points
            .iter()
            .fold((f64::INFINITY, f64::NEG_INFINITY), |(lo, hi), p| {
                (lo.min(dot(axis, *p)), hi.max(dot(axis, *p)))
            })
    };
    let corners = math::corners(receivers);
    let (x0, x1) = range(x, &corners);
    let (y0, y1) = range(y, &corners);
    // Square, in quarter-octave steps, centered on whole texels: the map
    // keeps its texel grid while the camera moves, so edges do not crawl.
    let span = (x1 - x0).max(y1 - y0).max(1e-3) * 1.02;
    let size = 2f64.powf((span.log2() * 4.).ceil() / 4.);
    let texel = size / f64::from(resolution);
    let snap = |v: f64| (v / texel).round() * texel;
    let (cx, cy) = (snap((x0 + x1) / 2.), snap((y0 + y1) / 2.));
    let mut depth_points = corners.to_vec();
    if (0..3).all(|a| casters[0][a] <= casters[1][a]) {
        depth_points.extend(math::corners(casters));
    }
    let (z0, z1) = range(forward, &depth_points);
    let (z0, z1) = (z0 - texel, z1 + texel);
    let row = |axis: [f64; 3], k: f64, c: f64| [axis[0] * k, axis[1] * k, axis[2] * k, c];
    Some(ShadowFit {
        rows: [
            row(x, 2. / size, -2. * cx / size),
            row(y, 2. / size, -2. * cy / size),
            row(forward, 1. / (z1 - z0), -z0 / (z1 - z0)),
            [0., 0., 0., 1.],
        ],
        texel: texel as f32,
        depth: (z1 - z0) as f32,
    })
}

/// Column-major f32 matrix from clip rows.
pub(super) fn matrix(rows: [[f64; 4]; 4]) -> [[f32; 4]; 4] {
    std::array::from_fn(|c| std::array::from_fn(|r| rows[r][c] as f32))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn apply(rows: &[[f64; 4]; 4], p: Vec3) -> [f64; 4] {
        rows.map(|r| {
            r[0] * f64::from(p[0]) + r[1] * f64::from(p[1]) + r[2] * f64::from(p[2]) + r[3]
        })
    }

    #[test]
    fn shadow_fit_covers_receivers_and_orders_depth_from_the_sun() {
        let casters = [[-100., 0., -100.], [100., 60., 100.]];
        let receivers = [[-20., 0., -30.], [40., 10., 25.]];
        let fit = fit([0.3, 0.9, 0.2], casters, receivers, 1024).unwrap();
        for p in math::corners(receivers) {
            let c = apply(&fit.rows, p);
            assert!(c[0].abs() <= 1. && c[1].abs() <= 1., "{c:?}");
            assert!((0. ..=1.).contains(&c[2]));
        }
        for p in math::corners(casters) {
            assert!((0. ..=1.).contains(&apply(&fit.rows, p)[2]));
        }
        // Higher along the sun direction is nearer the sun: smaller depth.
        let low = apply(&fit.rows, [0., 0., 0.])[2];
        let high = apply(&fit.rows, [0., 50., 0.])[2];
        assert!(high < low);
        assert!(fit.texel > 0. && fit.texel < 0.2);
        // Moving the receivers by less than a texel keeps the same grid.
        let nudged = receivers.map(|p| [p[0] + 1e-4, p[1], p[2]]);
        assert_eq!(
            fit,
            super::fit([0.3, 0.9, 0.2], casters, nudged, 1024).unwrap()
        );
    }
}
