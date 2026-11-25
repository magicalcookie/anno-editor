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
/// is exposed and tone mapped along `curve`.
/// Markers, lines and `Flat` meshes stay as given.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Tone {
    pub exposure: f32,
    /// How much occlusion maps and [`Occlusion`] darken the sun as well as
    /// the sky: 0 none.
    pub sun_occlusion: f32,
    /// Scales the sky's diffuse light, not its reflections.
    pub ambient: f32,
    pub curve: Curve,
    /// Measure each frame and expose it toward a target, on top of
    /// `exposure`.
    pub auto: Option<AutoExposure>,
}

impl Default for Tone {
    fn default() -> Self {
        Self {
            exposure: 1.,
            sun_occlusion: 0.2,
            ambient: 1.,
            curve: Curve::default(),
            auto: None,
        }
    }
}

/// Automatic exposure from a histogram of each frame's log2 luminance
/// (0.299, 0.587, 0.114) between `log_min` and `log_max`, 64 bins: the
/// darkest `low` share of the samples is dropped and the next `count`
/// share averaged, and the frame is exposed by `target` over that mean,
/// clamped to `min..max`. A new exposure within `tolerance` (a ratio) of
/// the last is not followed; past it the exposure moves at `speed_up`
/// (brightening the frame) or `speed_down`, in a time constant that
/// shortens the further off it is. Views drawn after a pause take the
/// measured exposure at once.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AutoExposure {
    pub log_min: f32,
    pub log_max: f32,
    pub low: f32,
    pub count: f32,
    pub target: f32,
    pub min: f32,
    pub max: f32,
    pub tolerance: f32,
    pub speed_up: f32,
    pub speed_down: f32,
}

/// How exposed radiance maps to the display. A game picks the curve and
/// its values; the default is a filmic curve with a mild shadow lift.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Curve {
    /// Filmic: exposed values under `lift_edge` bend up along a
    /// `lift_power` power, so shadows keep detail instead of crushing to
    /// black (an edge of 0 leaves them), then Narkowicz's ACES fit of the
    /// value times `scale`, then the sRGB encoding.
    Aces {
        lift_edge: f32,
        lift_power: f32,
        scale: f32,
    },
    /// Gamma first: each channel raised to 1/`gamma`, then exposed, then
    /// above `max_linear` bent toward `white` along a shoulder; below it
    /// the value stays. The gamma is the encoding: no sRGB step.
    Shoulder {
        gamma: f32,
        max_linear: f32,
        white: f32,
    },
}

impl Default for Curve {
    fn default() -> Self {
        Self::Aces {
            lift_edge: 0.1,
            lift_power: 0.7,
            scale: 0.6,
        }
    }
}

/// What pixels no geometry covers show: a sky shading from `zenith` to
/// `horizon` (a band that narrows toward the horizon), the sun's disc and
/// glow from [`Light`]'s sun, and below the horizon `horizon` or an endless
/// sea. Colors are in the scene's units: linear radiance when it is
/// [`Tone`]d. Set the fog color to `horizon` so geometry fades into it.
///
/// With `radiance` the sky is that map instead of the gradient, plus a sun
/// disc of radiance `sun_disc`; the map also lights the scene in place of
/// [`Light`]'s `sky` and `ground`, and fog takes its color from the map's
/// horizon in the direction looked along. `ambient` replaces the light the
/// map would give ([`SkyMap::irradiance`]) where a game lights from
/// something else than the sky it shows: Anno 1800's prefiltered diffuse
/// cubes ([`SkyMap::light`]).
#[derive(Clone, Debug, PartialEq)]
pub struct Sky {
    pub zenith: [f32; 3],
    pub horizon: [f32; 3],
    pub sea: Option<Sea>,
    pub radiance: Option<std::sync::Arc<SkyMap>>,
    pub sun_disc: [f32; 3],
    pub ambient: Option<[[[f32; 4]; 4]; 3]>,
}

/// A cube of radiance around the scene for surfaces to reflect: frame
/// WGSL's `environment(d)` reads it, and the kit draws nothing with it. A
/// game fills it from its own sky (2070's `SkyCubeMap`). Texels are
/// linear, as the kit's sky map is, so `environment` falls back to the sky
/// where a scene has no cube.
#[derive(Clone, Debug, PartialEq)]
pub struct Environment {
    /// Texels along a face's edge.
    pub side: u32,
    /// The six faces, +X, −X, +Y, −Y, +Z, −Z, each `side`² rows from the
    /// top; [`Environment::direction`] says which way a texel looks. The
    /// same `Arc` uploads once.
    pub texels: std::sync::Arc<Vec<[f32; 3]>>,
}

impl Environment {
    /// The unit direction through the centre of texel (x, y) of `face`, as
    /// a cube texture lays its faces out (WebGPU's, Vulkan's and D3D's).
    pub fn direction(side: u32, face: u32, x: u32, y: u32) -> Vec3 {
        let u = 2. * (x as f32 + 0.5) / side as f32 - 1.;
        let v = 2. * (y as f32 + 0.5) / side as f32 - 1.;
        let d = match face {
            0 => [1., -v, -u],
            1 => [-1., -v, u],
            2 => [u, 1., v],
            3 => [u, -1., -v],
            4 => [u, -v, 1.],
            _ => [-u, -v, -1.],
        };
        let l = (d[0] * d[0] + d[1] * d[1] + d[2] * d[2]).sqrt();
        d.map(|c| c / l)
    }

    /// A cube of `side` filled with what `radiance` gives toward each
    /// texel's direction.
    pub fn from_fn(side: u32, radiance: impl Fn(Vec3) -> [f32; 3]) -> Self {
        let texels = (0..6)
            .flat_map(|f| (0..side * side).map(move |i| (f, i % side, i / side)))
            .map(|(f, x, y)| radiance(Self::direction(side, f, x, y)))
            .collect();
        Self {
            side,
            texels: std::sync::Arc::new(texels),
        }
    }
}

/// Radiance arriving from every direction, linear, in the scene's units.
/// Columns run once around from +X through +Z; rows from straight up to
/// straight down with the latitude squared, so half the rows lie within
/// about 22° of the horizon, where the sky changes fastest (Hillaire's
/// sky-view mapping).
#[derive(Clone, Debug, PartialEq)]
pub struct SkyMap {
    pub width: u32,
    pub height: u32,
    /// Row-major, top row first.
    pub texels: Vec<[f32; 3]>,
}

impl SkyMap {
    /// The unit direction through the centre of texel (x, y).
    pub fn direction(width: u32, height: u32, x: u32, y: u32) -> Vec3 {
        let u = (x as f32 + 0.5) / width as f32;
        let s = 1. - 2. * (y as f32 + 0.5) / height as f32;
        let lat = s * s.abs() * std::f32::consts::FRAC_PI_2;
        let phi = (u - 0.5) * std::f32::consts::TAU;
        [lat.cos() * phi.cos(), lat.sin(), lat.cos() * phi.sin()]
    }

    /// Bilinear radiance toward unit direction `d`, wrapping around and
    /// clamped at the poles, as the shader samples it (`sky_uv`).
    pub fn sample(&self, d: Vec3) -> [f32; 3] {
        let lat = d[1].clamp(-1., 1.).asin();
        let s = lat.signum() * (lat.abs() / std::f32::consts::FRAC_PI_2).sqrt();
        let u = d[2].atan2(d[0]) / std::f32::consts::TAU + 0.5;
        let (w, h) = (self.width as usize, self.height as usize);
        let x = u * w as f32 - 0.5;
        let y = ((1. - s) * 0.5 * h as f32 - 0.5).clamp(0., h as f32 - 1.);
        let (x0, y0) = (x.floor(), y.floor());
        let (fx, fy) = (x - x0, y - y0);
        let col = |i: f32| (i as isize).rem_euclid(w as isize) as usize;
        let row = |j: f32| (j as usize).min(h - 1);
        let at = |i: f32, j: f32| self.texels[row(j) * w + col(i)];
        let mix =
            |a: [f32; 3], b: [f32; 3], t: f32| std::array::from_fn(|c| a[c] + (b[c] - a[c]) * t);
        mix(
            mix(at(x0, y0), at(x0 + 1., y0), fx),
            mix(at(x0, y0 + 1.), at(x0 + 1., y0 + 1.), fx),
            fy,
        )
    }

    /// Diffuse light from the map for every surface direction, as three
    /// symmetric matrices M (red, green, blue): a surface facing n gets
    /// (n, 1)ᵀ M (n, 1). Nine spherical harmonics of the radiance,
    /// convolved with the cosine lobe and divided by π (Ramamoorthi and
    /// Hanrahan's irradiance matrix), as Anno's GetAmbientLightDiffuse
    /// evaluates its sky's SH9.
    pub fn irradiance(&self) -> [[[f32; 4]; 4]; 3] {
        self.matrices([1. / std::f32::consts::PI; 3])
    }

    /// [`irradiance`](Self::irradiance)'s matrices for a map that already
    /// holds what a surface facing each direction gets (a prefiltered
    /// diffuse cube): the map itself to second order, not convolved again.
    pub fn light(&self) -> [[[f32; 4]; 4]; 3] {
        // The matrix constants carry the cosine lobe's band factors π,
        // 2π/3 and π/4; dividing them back out leaves the projection.
        let pi = std::f32::consts::PI;
        self.matrices([1. / pi, 1.5 / pi, 4. / pi])
    }

    // `band` scales the projection of bands 0, 1 and 2.
    fn matrices(&self, band: [f32; 3]) -> [[[f32; 4]; 4]; 3] {
        // A Fibonacci sphere: even coverage, so every sample weighs 4π/N.
        const N: usize = 4096;
        let mut sh = [[0f32; 3]; 9];
        for i in 0..N {
            let y = 1. - 2. * (i as f32 + 0.5) / N as f32;
            let r = (1. - y * y).sqrt();
            let phi = i as f32 * 2.399_963;
            let d = [r * phi.cos(), y, r * phi.sin()];
            let l = self.sample(d);
            let [x, y, z] = d;
            let basis = [
                0.282_095,
                0.488_603 * y,
                0.488_603 * z,
                0.488_603 * x,
                1.092_548 * x * y,
                1.092_548 * y * z,
                0.315_392 * (3. * z * z - 1.),
                1.092_548 * x * z,
                0.546_274 * (x * x - y * y),
            ];
            for (k, b) in basis.iter().enumerate() {
                for c in 0..3 {
                    sh[k][c] += l[c] * b;
                }
            }
        }
        let w = 4. * std::f32::consts::PI / N as f32;
        let (c1, c2, c3, c4, c5) = (0.429_043, 0.511_664, 0.743_125, 0.886_227, 0.247_708);
        std::array::from_fn(|c| {
            let l = |k: usize| {
                sh[k][c]
                    * w
                    * band[if k == 0 {
                        0
                    } else if k < 4 {
                        1
                    } else {
                        2
                    }]
            };
            // Order: L00, L1-1 (y), L10 (z), L11 (x), L2-2 (xy), L2-1
            // (yz), L20 (3z² - 1), L21 (xz), L22 (x² - y²).
            [
                [c1 * l(8), c1 * l(4), c1 * l(7), c2 * l(3)],
                [c1 * l(4), -c1 * l(8), c1 * l(5), c2 * l(1)],
                [c1 * l(7), c1 * l(5), c3 * l(6), c2 * l(2)],
                [c2 * l(3), c2 * l(1), c2 * l(2), c4 * l(0) - c5 * l(6)],
            ]
        })
    }
}

/// [`SkyMap::irradiance`]'s matrices for a hemisphere light: `sky` on
/// surfaces facing up, `ground` facing down, linear in between.
pub fn hemisphere(sky: [f32; 3], ground: [f32; 3]) -> [[[f32; 4]; 4]; 3] {
    std::array::from_fn(|c| {
        let (mean, half) = ((sky[c] + ground[c]) * 0.5, (sky[c] - ground[c]) * 0.5);
        let mut m = [[0.; 4]; 4];
        m[3][3] = mean;
        m[1][3] = half * 0.5;
        m[3][1] = half * 0.5;
        m
    })
}

/// A flat sea out to the horizon at world height `level`: `color` lit by
/// the sky and sun, mirroring the sky at grazing angles, fogged with
/// distance like geometry. The kit draws it at the Water stage where
/// nothing opaque drew; a game's Water pass may replace it on its own
/// (`pass::Stage::Water`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Sea {
    pub level: f32,
    pub color: [f32; 3],
}

/// Screen-space ambient occlusion: crevices, contact with the ground and
/// the insides of foliage darken. Horizon-based, as NVIDIA's HBAO+ which
/// both Annos run (ambient_ssao.fx): from each pixel, steps along eight
/// screen directions find the surfaces rising over its tangent plane.
/// Estimated from the view's depth after opaque geometry draws, blurred,
/// and multiplied over it; blended water, the sky and overlays are
/// untouched. As in Anno it darkens the sky's light, and the sun's only by
/// [`Tone::sun_occlusion`]: sunlit ground at a wall's foot stays lit.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Occlusion {
    /// World units around a point searched for occluders.
    pub radius: f32,
    /// 0 to 1: how dark full occlusion gets.
    pub strength: f32,
    /// Ignores occluders this little over the tangent plane (the cosine of
    /// the angle above it): shallow creases of a tessellated surface stay
    /// open.
    pub bias: f32,
    /// The blurred estimate is raised to this: higher darkens contact more.
    pub power: f32,
    /// Weights of the first step out (contact) and the steps beyond.
    pub small_scale: f32,
    pub large_scale: f32,
    /// The search's screen radius stays between these many pixels, the
    /// world radius growing or shrinking with it: far ground keeps its
    /// occlusion, and a surface right before the camera does not search
    /// half the screen.
    pub min_pixels: f32,
    pub max_pixels: f32,
}

impl Default for Occlusion {
    /// HBAO+'s own defaults (GFSDK_SSAO_Parameters).
    fn default() -> Self {
        Self {
            radius: 1.,
            strength: 1.,
            bias: 0.1,
            power: 2.,
            small_scale: 1.,
            large_scale: 1.,
            min_pixels: 0.,
            max_pixels: 128.,
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
    /// Maps fitted to slices of each view's depth, nearest first, up to
    /// [`MAX_CASCADES`]: sharp shadows near the camera, where the scene's
    /// map alone spreads a texel over the whole scene. Beyond the last, and
    /// with none, the scene's map shades. Each view draws them again when
    /// its camera or a caster moved: `cascade_resolution²` × 4 bytes each
    /// per viewport.
    pub cascades: u32,
    pub cascade_resolution: u32,
    /// World units along the view from its near plane that cascades cover.
    pub reach: f32,
}

impl Default for Shadows {
    fn default() -> Self {
        Self {
            resolution: 2048,
            softness: 1.5,
            cascades: 0,
            cascade_resolution: 2048,
            reach: 500.,
        }
    }
}

/// The most view cascades a [`Shadows`] can ask for.
pub const MAX_CASCADES: usize = 3;

/// The most [`LocalLight`]s a scene lights with; past it the rest are
/// dropped. The most any shipped Anno cfg carries is 93 (117's
/// hippodrome); each takes three words of the frame's uniform.
pub const MAX_LOCAL_LIGHTS: usize = 128;

/// A light of the scene's own beside the sun: a lamp or a fire, unshadowed.
/// Surfaces take `color` times its falloff times N·L, diffuse only, as
/// Anno's `GetPointLighting` and `GetSpotLighting` hand their light to
/// `GetAnalyticalLighting`; blended sheets take it wrapped round them, as
/// 117's particles take the sun.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LocalLight {
    pub position: Vec3,
    /// World units past which it lights nothing.
    pub reach: f32,
    /// In the units of [`Light::sun_color`]: at a unit's distance with
    /// [`Falloff::InverseSquare`], at the light with [`Falloff::Power`].
    pub color: [f32; 3],
    pub falloff: Falloff,
    pub spot: Option<Spot>,
}

/// How a [`LocalLight`] dims with distance `d` toward its reach `R`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Falloff {
    /// 117's `GetPointLightAttenuation`: Yuksel's `2 / (r² + d² + d·√(d² +
    /// r²))` for a light of radius `r = max(0.1, 0.001·R)`, faded to 0 at
    /// `R` by `1 - (d/R)²`.
    InverseSquare,
    /// 1800's `GetLightRangeIntensity`: `((R - d) / R)` to this power.
    Power(f32),
}

/// A spot's cone: it shines along `aim` (a unit vector), fully inside
/// the inner half angle and out to nothing at the outer, by the square of
/// `(cos θ - cos outer) / (cos inner - cos outer)` as Anno's spots fade.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Spot {
    pub aim: Vec3,
    pub cos_inner: f32,
    pub cos_outer: f32,
}

impl LocalLight {
    /// The three words frame WGSL's `local_light` reads: position and
    /// reach; colour and falloff (negative for the inverse square); the aim
    /// scaled by `1 / (cos inner - cos outer)` and the outer cosine, zero
    /// for a point light.
    pub(super) fn words(&self) -> [[f32; 4]; 3] {
        let [x, y, z] = self.position;
        let [r, g, b] = self.color;
        let falloff = match self.falloff {
            Falloff::InverseSquare => -1.,
            Falloff::Power(p) => p.max(0.),
        };
        let cone = self.spot.and_then(|s| {
            let aim = math::normalize(s.aim)?;
            // An inner cone as wide as the outer is a hard edge.
            let spread = 1. / (s.cos_inner - s.cos_outer).max(1e-4);
            Some([
                aim[0] * spread,
                aim[1] * spread,
                aim[2] * spread,
                s.cos_outer,
            ])
        });
        [
            [x, y, z, self.reach.max(0.)],
            [r, g, b, falloff],
            cone.unwrap_or([0.; 4]),
        ]
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
    /// What the map holds in the sun's frame: the square's centre x and y
    /// and half side, and the depth range; and the sun it was fitted to.
    area: [f64; 5],
    sun: Vec3,
}

impl ShadowFit {
    /// Whether a map drawn with this fit also serves `want`: the same sun,
    /// `want`'s square and depth range inside this one, and texels at most
    /// `coarser` times `want`'s.
    pub fn covers(&self, want: &ShadowFit, coarser: f32) -> bool {
        let ([x, y, h, z0, z1], [wx, wy, wh, wz0, wz1]) = (self.area, want.area);
        // The two fits' depth ranges are the same up to rounding (a few
        // millionths of a unit): exact, a still view drew a cascade of
        // 1024² again every frame, while 2048² happened to round the
        // right way.
        let slack = (z1 - z0).abs() * 1e-5;
        self.sun == want.sun
            && (x - wx).abs() + wh <= h
            && (y - wy).abs() + wh <= h
            && z0 <= wz0 + slack
            && wz1 <= z1 + slack
            && self.texel <= want.texel * coarser
    }

    /// `want`'s square in this map's clip: centre x, y and half side.
    pub fn square_of(&self, want: &ShadowFit) -> [f32; 4] {
        let ([x, y, h, ..], [wx, wy, wh, ..]) = (self.area, want.area);
        [
            ((wx - x) / h) as f32,
            ((wy - y) / h) as f32,
            (wh / h) as f32,
            0.,
        ]
    }
}

/// Fits an orthographic sun projection around the `receivers` points,
/// deep enough to include every caster between them and the sun, and the
/// `deep` box besides. `margin` widens the square past the receivers, for a
/// map that still serves after the view moves a little.
pub(super) fn fit(
    sun: Vec3,
    casters: [Vec3; 2],
    receivers: &[Vec3],
    deep: [Vec3; 2],
    resolution: u32,
    margin: f64,
) -> Option<ShadowFit> {
    let given = sun;
    let sun = math::normalize(sun)?;
    if receivers.is_empty() {
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
    let (x0, x1) = range(x, receivers);
    let (y0, y1) = range(y, receivers);
    // Square, in quarter-octave steps, centered on whole texels: the map
    // keeps its texel grid while the camera moves, so edges do not crawl.
    let span = (x1 - x0).max(y1 - y0).max(1e-3) * 1.02 * margin;
    let size = 2f64.powf((span.log2() * 4.).ceil() / 4.);
    let texel = size / f64::from(resolution);
    let snap = |v: f64| (v / texel).round() * texel;
    let (cx, cy) = (snap((x0 + x1) / 2.), snap((y0 + y1) / 2.));
    let mut depth_points = receivers.to_vec();
    for b in [casters, deep] {
        if (0..3).all(|a| b[0][a] <= b[1][a]) {
            depth_points.extend(math::corners(b));
        }
    }
    let (z0, z1) = range(forward, &depth_points);
    // Room past the farthest caster for the slope bias. A share of the
    // range, not a texel: a view's cascade and the wider map drawn for it
    // then span the same depth and one covers the other.
    let pad = (z1 - z0) * 1e-3 + 1e-3;
    let (z0, z1) = (z0 - pad, z1 + pad);
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
        area: [cx, cy, size / 2., z0, z1],
        sun: given,
    })
}

/// Fits up to `count` cascades to slices of a view's depth (its clip
/// `rows`, reversed depth as the kit's cameras give it), each slice clamped
/// into `receivers` so a view over the sea spends no texels there. Their
/// depth spans the casters and the whole of `receivers`, the same for any
/// view: from a slice's corners it moved by centimetres as the view
/// turned, and every map no longer covered its slice. Slices
/// cover `reach` world units from the near plane, split between even and
/// logarithmic spacing, and overlap the next by a tenth as Anno's cascades
/// do (SHADOW_CASCADE_OVERLAP_FACTOR 1.1), for the shader to blend across.
pub(super) fn cascades(
    sun: Vec3,
    casters: [Vec3; 2],
    receivers: [Vec3; 2],
    rows: &[[f64; 4]; 4],
    count: usize,
    reach: f32,
    resolution: u32,
    margin: f64,
) -> Vec<ShadowFit> {
    if count == 0 || (0..3).any(|a| receivers[0][a] > receivers[1][a]) {
        return Vec::new();
    }
    let clip: math::Mat4 = std::array::from_fn(|i| rows[i % 4][i / 4] as f32);
    let Some(inverse) = math::inverse(&clip) else {
        return Vec::new();
    };
    let unproject = |x: f32, y: f32, z: f32| {
        let m = &inverse;
        let w = m[3] * x + m[7] * y + m[11] * z + m[15];
        math::scale(math::transform(m, [x, y, z]), 1. / w)
    };
    // Each corner's point on the near plane (reversed depth 1) and its
    // step per unit of depth along the view; z 0.5 lies twice the near
    // distance out in perspective and halfway through an orthographic span.
    let near = unproject(0., 0., 1.);
    let Some(forward) = math::normalize(math::sub(unproject(0., 0., 0.5), near)) else {
        return Vec::new();
    };
    let corners = [[-1., -1.], [1., -1.], [1., 1.], [-1., 1.]].map(|[x, y]| {
        let a = unproject(x, y, 1.);
        let along = math::sub(unproject(x, y, 0.5), a);
        (
            a,
            math::scale(along, 1. / math::dot(along, forward).max(1e-6)),
        )
    });
    let depth = |p: Vec3| math::dot(math::sub(p, near), forward);
    let box_corners = math::corners(receivers);
    let lo = box_corners
        .iter()
        .map(|&p| depth(p))
        .fold(f32::INFINITY, f32::min)
        .max(0.);
    let hi = box_corners
        .iter()
        .map(|&p| depth(p))
        .fold(0f32, f32::max)
        .min(reach);
    if hi <= lo {
        return Vec::new();
    }
    let first = lo.max(1.);
    let split = |i: usize| {
        let f = i as f32 / count as f32;
        let even = lo + (hi - lo) * f;
        let log = first * (hi.max(first) / first).powf(f);
        0.75 * log + 0.25 * even
    };
    let clamp =
        |p: Vec3| -> Vec3 { std::array::from_fn(|a| p[a].clamp(receivers[0][a], receivers[1][a])) };
    (0..count)
        .filter_map(|i| {
            let (a, b) = (if i == 0 { lo } else { split(i) }, split(i + 1) * 1.1);
            let points: Vec<Vec3> = (corners.iter())
                .flat_map(|&(o, step)| [a, b].map(|t| clamp(math::add(o, math::scale(step, t)))))
                .collect();
            fit(sun, casters, &points, receivers, resolution, margin)
        })
        .collect()
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
        let fit = fit(
            [0.3, 0.9, 0.2],
            casters,
            &math::corners(receivers),
            receivers,
            1024,
            1.,
        )
        .unwrap();
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
            super::fit(
                [0.3, 0.9, 0.2],
                casters,
                &math::corners(nudged),
                nudged,
                1024,
                1.
            )
            .unwrap()
        );
    }

    // A cascade's depth is the scene's, whichever way the view turns: from
    // the slice's corners it moved, and no drawn map covered the next
    // frame's fit.
    #[test]
    fn cascade_depth_does_not_follow_the_view() {
        let casters = [[-100., 0., -100.], [100., 30., 100.]];
        let receivers = [[-120., -5., -120.], [120., 40., 120.]];
        let view = |yaw: f32| {
            let camera = crate::camera::Camera {
                fov: 0.9,
                tilt: 1.0,
                yaw,
                ..Default::default()
            };
            let extent = crate::camera::Extent {
                center: [0.; 3],
                size: 200.,
            };
            let bounds = gpui::Bounds::new(
                gpui::point(gpui::px(0.), gpui::px(0.)),
                gpui::size(gpui::px(320.), gpui::px(240.)),
            );
            camera.view_rows(bounds, extent, true)
        };
        let fits = |yaw| {
            cascades(
                [0.3, 0.9, 0.2],
                casters,
                receivers,
                &view(yaw),
                3,
                150.,
                1024,
                1.,
            )
        };
        let (a, b) = (fits(0.), fits(0.03));
        assert_eq!(a.len(), 3);
        for (a, b) in a.iter().zip(&b) {
            assert_eq!((a.rows[2], a.depth), (b.rows[2], b.depth));
        }
    }
}
