// Owns Anno 1800's sky: an ambient preset (`AmbientArea`) with the global
// preset it names (`GlobalAmbientInfo`), whose curves over the day give the
// sun, the sky's light and the scattered sky's colors, and the cube maps it
// draws and lights with. Baked on the CPU as sky.fx psSkyBox draws it
// (functions_scattering.fxh ComputeSkyColor under functions_sky.fxh's cloud
// and star cubes) into the same `Baked` the 117 atmosphere gives.
// Engine addresses here are Anno1800.exe build 16367353 (Steam).
mod pass;

pub use pass::{SkyPasses, warm};

use anno_core::ambient::{Ambient, Baked, Sky, SunMoon};
pub use anno_core::cube::{Cube, Cubes};
use anno_formats::vfs::Install;
use anno_formats::xml::{block, tag};
use gpui_editor_kit::gpu::{self, SkyMap};
use rayon::prelude::*;
use std::sync::Arc;

type Rgb = [f32; 3];

/// Sky map size: clouds read at about a third of a degree per texel.
const MAP: [u32; 2] = [1024, 512];

/// One `<…Graph>`: keys over the day (t 0 midnight, 0.5 noon), each a
/// color and an intensity, linear between keys.
#[derive(Clone, Debug, Default, PartialEq)]
struct Curve(Vec<(f32, [f32; 4])>);

impl Curve {
    fn read(xml: &str, name: &str) -> Self {
        let Some(graph) = block(xml, name) else {
            return Self::default();
        };
        let mut keys = Vec::new();
        for item in graph.split("<i>").skip(1) {
            let get = |n: &str| tag(item, n).and_then(|v| v.parse::<f32>().ok());
            if let (Some(t), Some(r), Some(g), Some(b), Some(a)) =
                (get("t"), get("c.r"), get("c.g"), get("c.b"), get("c.a"))
            {
                keys.push((t, [r, g, b, a]));
            }
        }
        keys.sort_by(|a, b| a.0.total_cmp(&b.0));
        Self(keys)
    }

    /// The key color and intensity at day fraction `t`, held past the ends.
    fn at(&self, t: f32) -> [f32; 4] {
        let keys = &self.0;
        let Some(first) = keys.first() else {
            return [1.; 4];
        };
        if t <= first.0 {
            return first.1;
        }
        for pair in keys.windows(2) {
            let ((t0, a), (t1, b)) = (pair[0], pair[1]);
            if t <= t1 {
                let f = if t1 > t0 { (t - t0) / (t1 - t0) } else { 0. };
                return std::array::from_fn(|i| a[i] + (b[i] - a[i]) * f);
            }
        }
        keys[keys.len() - 1].1
    }

    /// Color times intensity, for color curves.
    fn color(&self, t: f32) -> Rgb {
        let [r, g, b, a] = self.at(t);
        [r * a, g * a, b * a]
    }

    /// The intensity alone, for scalar curves (their color is a constant
    /// grey the editor shows).
    fn value(&self, t: f32) -> f32 {
        self.at(t)[3]
    }
}

/// functions_shadow.fxh CalculateCloudShadowNoise's cCloudTex. TODO: which
/// texture the engine binds is not traced; this is the cloud layer under
/// the environment's effects, whose alpha the shader reads.
const CLOUD_SHADOW_MAP: &str = "data/graphics/effects/environment/cloud_layer_0.dds";

/// The cloud shadow map, read once for every preset.
#[derive(Clone)]
pub struct CloudMap(Arc<gpu::ControlMap>);

impl CloudMap {
    fn read(install: &Install) -> Option<Self> {
        let bytes = install.read(CLOUD_SHADOW_MAP).ok()?;
        let dds = gpui_editor_kit::texture::Dds::parse(&bytes).ok()?;
        let t = dds.decode(&bytes, 0).ok()?;
        let map = gpu::ControlMap::new(t.width as usize, t.height as usize, t.rgba).ok()?;
        Some(Self(Arc::new(map)))
    }
}

impl std::fmt::Debug for CloudMap {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("CloudMap")
    }
}

// Read once and shared: the same map is the same texture.
impl PartialEq for CloudMap {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

/// An Anno 1800 ambient preset as its sky needs it.
#[derive(Debug, PartialEq)]
pub struct Preset {
    /// `Time` (day fraction) when `TimeEnabled`: the variances apply in
    /// full there and fade out over the two hours either side. Without it
    /// they apply all day.
    time: Option<f32>,
    /// `LightDirection`, normalised: where the light points at noon.
    light_direction: Rgb,
    ambient_variance: Rgb,
    diffuse_variance: Rgb,
    /// `EnvironmentRotation`, turns about up, of the diffuse cubes.
    environment_rotation: f32,
    horizon_variance: Rgb,
    zenith_variance: Rgb,
    horizon_falloff_variance: f32,
    /// `ScatteringStartVariance`, `ScatteringIntensityVariance` and
    /// `ScatteringFallOffVariance`: the haze's.
    haze_variance: [f32; 3],
    sun_enabled: bool,
    cloud_density: f32,
    pub exposure: f32,
    pub sun_occlusion: f32,
    pub occlusion: f32,
    /// `GlowBrightness` and `NightGlowBrightness`.
    pub glow: f32,
    pub night_glow: f32,
    /// `ToneMappingMaxLinear` and `ToneMappingWhite` when tone mapping is
    /// on.
    pub tone: Option<(f32, f32)>,
    ambient: Curve,
    diffuse: Curve,
    horizon: Curve,
    zenith: Curve,
    horizon_falloff: Curve,
    sun: Curve,
    /// The haze over what stands in the sky (ComputeScattering).
    scattering_start: Curve,
    scattering_intensity: Curve,
    scattering_falloff: Curve,
    /// `CloudShadowParams` (cCloudShadows: scale, speed, density,
    /// intensity), and the map they shade with.
    cloud_shadow: [f32; 4],
    cloud_map: Option<CloudMap>,
    /// `WindDirection`, the way cloud shadows drift.
    wind: [f32; 2],
    clouds: Option<Arc<Cube>>,
    clouds_dense: Option<Arc<Cube>>,
    stars: Option<Arc<Cube>>,
    /// Prefiltered diffuse light at noon, dawn and night.
    light: [Option<Arc<Cube>>; 3],
}

impl Preset {
    /// The preset in `xml`, with the global preset it names from
    /// `ambientsettings/global/`, and its cubes through `cubes`.
    pub fn read(
        install: &Install,
        xml: &str,
        cubes: &mut Cubes,
        cloud_map: Option<&CloudMap>,
    ) -> Option<Self> {
        let global_name = tag(xml, "GlobalName")?;
        let global = install
            .search("config/engine/ambientsettings/global/", 200)
            .into_iter()
            .filter(|&id| install.file_path(id).contains("/ambientsettings/global/"))
            .find_map(|id| {
                let text = String::from_utf8_lossy(&install.read_file(id).ok()?).into_owned();
                tag(&text, "Name")
                    .is_some_and(|n| n.eq_ignore_ascii_case(global_name))
                    .then_some(text)
            })
            .or_else(|| {
                eprintln!("sky: global ambient {global_name} not found");
                None
            })?;
        let number = |name: &str| tag(xml, name).and_then(|v| v.parse::<f32>().ok());
        let rgb = |name: &str| -> Option<Rgb> {
            let inner = block(xml, name)?;
            let get = |n: &str| tag(inner, n).and_then(|v| v.parse::<f32>().ok());
            get("r")
                .zip(get("g"))
                .zip(get("b"))
                .map(|((r, g), b)| [r, g, b])
                .or_else(|| Some([get("x")?, get("y")?, get("z")?]))
        };
        let atmosphere = block(xml, "Atmosphere").unwrap_or("");
        let scalar = |name: &str| tag(atmosphere, name).and_then(|v| v.parse::<f32>().ok());
        let atmosphere_rgb = |name: &str| -> Option<Rgb> {
            let inner = block(atmosphere, name)?;
            let get = |n: &str| tag(inner, n).and_then(|v| v.parse::<f32>().ok());
            Some([get("r")?, get("g")?, get("b")?])
        };
        let global_atmosphere = block(&global, "Atmosphere").unwrap_or("");
        Some(Self {
            time: (number("TimeEnabled") == Some(1.)).then(|| number("Time").unwrap_or(0.5)),
            light_direction: normalize(rgb("LightDirection").unwrap_or([-0.33, -0.63, 0.7])),
            ambient_variance: rgb("AmbientVariance").unwrap_or([1.; 3]),
            diffuse_variance: rgb("DiffuseVariance").unwrap_or([1.; 3]),
            environment_rotation: number("EnvironmentRotation").unwrap_or(0.),
            horizon_variance: atmosphere_rgb("HorizonColorVariance").unwrap_or([1.; 3]),
            zenith_variance: atmosphere_rgb("ZenithColorVariance").unwrap_or([1.; 3]),
            horizon_falloff_variance: scalar("HorizonFallOffVariance").unwrap_or(1.),
            haze_variance: [
                scalar("ScatteringStartVariance").unwrap_or(1.),
                scalar("ScatteringIntensityVariance").unwrap_or(1.),
                scalar("ScatteringFallOffVariance").unwrap_or(1.),
            ],
            sun_enabled: scalar("SunEnabled") != Some(0.),
            cloud_shadow: block(xml, "CloudShadowParams").map_or([0.; 4], |b| {
                let get = |n: &str| tag(b, n).and_then(|v| v.parse::<f32>().ok()).unwrap_or(0.);
                [get("x"), get("y"), get("z"), get("w")]
            }),
            cloud_map: cloud_map.cloned(),
            wind: block(xml, "WindDirection")
                .and_then(|b| {
                    let get = |n: &str| tag(b, n).and_then(|v| v.parse::<f32>().ok());
                    Some([get("x")?, get("y")?])
                })
                .filter(|w| w[0].hypot(w[1]) > 1e-6)
                .map_or([1., 0.], |w| {
                    let l = w[0].hypot(w[1]);
                    [w[0] / l, w[1] / l]
                }),
            cloud_density: scalar("SkyCloudDensity").unwrap_or(0.5),
            exposure: number("Exposure").unwrap_or(0.63),
            // Every shipped preset sets both; the most common values stand
            // in for one that does not.
            glow: number("GlowBrightness").unwrap_or(2.8),
            night_glow: number("NightGlowBrightness").unwrap_or(1.2),
            sun_occlusion: number("SunAO").unwrap_or(0.13),
            occlusion: number("AOIntensity").unwrap_or(1.),
            tone: (number("ToneMappingEnabled") != Some(0.)).then(|| {
                (
                    number("ToneMappingMaxLinear").unwrap_or(0.8),
                    number("ToneMappingWhite").unwrap_or(1.3),
                )
            }),
            ambient: Curve::read(&global, "AmbientGraph"),
            diffuse: Curve::read(&global, "DiffuseGraph"),
            horizon: Curve::read(global_atmosphere, "HorizonColorGraph"),
            zenith: Curve::read(global_atmosphere, "ZenithColorGraph"),
            horizon_falloff: Curve::read(global_atmosphere, "HorizonFallOffGraph"),
            sun: Curve::read(global_atmosphere, "SunColorGraph"),
            scattering_start: Curve::read(global_atmosphere, "ScatteringStartGraph"),
            scattering_intensity: Curve::read(global_atmosphere, "ScatteringIntensityGraph"),
            scattering_falloff: Curve::read(global_atmosphere, "ScatteringFallOffGraph"),
            clouds: tag(atmosphere, "SkyBoxPath").and_then(|p| cubes.get(install, p)),
            clouds_dense: tag(atmosphere, "CloudBoxDensePath").and_then(|p| cubes.get(install, p)),
            stars: tag(atmosphere, "SkyBoxPathNight").and_then(|p| cubes.get(install, p)),
            light: std::array::from_fn(|i| {
                tag(xml, &format!("DiffuseCubePath{i}")).and_then(|p| cubes.get(install, p))
            }),
        })
    }

    /// How much of the variances applies at day fraction `t`: all of it,
    /// or with `TimeEnabled` all at `Time`, none two hours either side.
    fn variance_weight(&self, t: f32) -> f32 {
        self.time
            .map_or(1., |time| (1. - 12. * (time - t).abs()).max(0.))
    }

    /// A color curve at `t` under `variance` as the engine hands it to the
    /// shaders: graph color times intensity times the faded variance, then
    /// gamma 2.2 to linear.
    fn graded(&self, curve: &Curve, variance: Rgb, t: f32) -> Rgb {
        let w = self.variance_weight(t);
        let c = curve.color(t);
        std::array::from_fn(|i| (c[i] * (1. + (variance[i] - 1.) * w)).max(0.).powf(2.2))
    }

    /// The sun's light at `t` (cLgtDirection, capped so it always comes
    /// from at least 22.5° up: the moon's by night) and where the sky draws
    /// it (cLgtDirectionUncapped), both as directions toward it. At noon it
    /// stands where `LightDirection` puts it; it rises a quarter turn to
    /// one side and sets a quarter turn to the other, 9° below the horizon
    /// (functions at 0x141c719f0, 0x141e44442 in the 1800 executable).
    fn sun_at(&self, t: f32) -> (Rgb, Rgb) {
        let day = Day::at(t);
        let ld = self.light_direction;
        let phi = ld[2].atan2(ld[0]);
        let theta = ld[1].clamp(-1., 1.).asin();
        let (v, s) = if day.is_day > 0.5 {
            (day.progress, 1.)
        } else {
            (day.night_progress, -1.)
        };
        let pi = std::f32::consts::PI;
        let point = |angle: f32, capped: bool| {
            let az = phi + s * pi / 2. * angle.cos();
            let mut e = angle.sin() * (theta - pi / 20.) + pi / 20.;
            if capped {
                e = e.min(-pi / 8.);
            }
            // Toward the light: against where it points.
            [-(az.cos() * e.cos()), -e.sin(), -(az.sin() * e.cos())]
        };
        let minute = pi * (v * 1440. + 0.5).floor() / 1440.;
        (point(minute, true), point(pi * v, false))
    }

    /// Where the island's one light comes from at `hours`.
    pub fn toward(&self, hours: f32) -> Rgb {
        self.sun_at((hours / 24.).rem_euclid(1.)).0
    }

    /// The sky and the light at `hours`.
    pub fn bake(&self, hours: f32) -> Baked {
        let t = (hours / 24.).rem_euclid(1.);
        let (toward, _) = self.sun_at(t);
        let light = self.graded(&self.diffuse, self.diffuse_variance, t);
        let sky = self.scattering(t);
        let [w, h] = MAP;
        let texels: Vec<Rgb> = (0..w * h)
            .into_par_iter()
            .map(|i| self.seen(&sky, SkyMap::direction(w, h, i % w, i / w)))
            .collect();
        let map = Arc::new(SkyMap {
            width: w,
            height: h,
            texels,
        });
        let ambient = self.graded(&self.ambient, self.ambient_variance, t);
        Baked {
            map,
            toward,
            light,
            disc: [0.; 3],
            ambient: self.light(t, toward, ambient).map(|m| m.light()),
        }
    }

    /// ComputeSkyColorHelper's inputs at day fraction `t`.
    fn scattering(&self, t: f32) -> Scattering {
        let (_, sky_toward) = self.sun_at(t);
        let w = self.variance_weight(t);
        let falloff =
            self.horizon_falloff.value(t) * (1. + (self.horizon_falloff_variance - 1.) * w);
        Scattering {
            zenith: self.graded(&self.zenith, self.zenith_variance, t),
            horizon: self.graded(&self.horizon, self.horizon_variance, t),
            horizon_falloff: [falloff, falloff * 0.9, falloff * 0.75],
            sun: self.graded(&self.sun, [1.; 3], t),
            sun_on: self.sun_enabled,
            toward: sky_toward,
            day: Day::at(t),
        }
    }

    /// ComputeScattering's start, intensity and distance falloff at day
    /// fraction `t`: the global preset's graphs under the local preset's
    /// variances, as the horizon falloff takes its own.
    fn haze(&self, t: f32) -> [f32; 3] {
        let w = self.variance_weight(t);
        let graded = |curve: &Curve, variance: f32| curve.value(t) * (1. + (variance - 1.) * w);
        [
            graded(&self.scattering_start, self.haze_variance[0]),
            graded(&self.scattering_intensity, self.haze_variance[1]),
            graded(&self.scattering_falloff, self.haze_variance[2]),
        ]
    }

    /// The diffuse light a surface facing each direction gets
    /// (`EnvIrradiance` of the normal turned by cMtxEnvironmentRotation,
    /// times cLgtAmbient), as a map for [`SkyMap::light`]: the noon or
    /// night cube toward the dawn one by cPrefilteredCubeMapLerpFac.
    fn light(&self, t: f32, toward: Rgb, ambient: Rgb) -> Option<SkyMap> {
        let day = Day::at(t);
        let first = if day.day_value > 0.5 { 0 } else { 2 };
        let a = self.light[first].as_ref()?;
        let b = self.light[1].as_ref().unwrap_or(a);
        let f = 1.
            - (((day.day_value - 0.5) * 2.).clamp(0., 1.)
                + ((day.night_ratio - 0.25) * 4.).clamp(0., 1.));
        // θ from cLgtDirection (pointing away from `toward`), so the
        // environment turns with the light (0x141e79818).
        let theta =
            (-toward[0]).atan2(-toward[2]) + self.environment_rotation * std::f32::consts::TAU;
        let (sin, cos) = theta.sin_cos();
        let [w, h] = [64, 32];
        let texels = (0..w * h)
            .map(|i| {
                let n = SkyMap::direction(w, h, i % w, i / w);
                // MUL(N, M): the normal as a row vector.
                let d = [n[0] * cos - n[2] * sin, n[1], n[0] * sin + n[2] * cos];
                let (x, y) = (a.sample(d), b.sample(d));
                std::array::from_fn(|c| (x[c] + (y[c] - x[c]) * f) * ambient[c])
            })
            .collect();
        Some(SkyMap {
            width: w,
            height: h,
            texels,
        })
    }

    /// psSkyBox: the scattered sky, with the cloud cube laid over it and
    /// the stars behind the clouds at night.
    fn seen(&self, sky: &Scattering, d: Rgb) -> Rgb {
        let scattered = sky.color(d);
        let Some(clouds) = &self.clouds else {
            return scattered;
        };
        // PositionOS: the direction lifted for the camera's height, not
        // normalised again.
        let at = [d[0], d[1] + CAMERA_LIFT, d[2]];
        let cloud = self.cloud_layer(clouds, sky, at);
        // Faded out toward the horizon, then over the scattered sky at
        // twice its brightness.
        let fade = 1. - (1. - at[1]).abs().powi(64);
        let a = (cloud[3] * fade).clamp(0., 1.);
        std::array::from_fn(|i| scattered[i] + (cloud[i] * 2. - scattered[i]) * a)
    }

    /// The cloud cube toward `at` as psSkyBox lays it over the sky, before
    /// its fade toward the horizon: ComputeCloudBoxColor through the day's
    /// turn, darkened by night, lit through the front and at rims around
    /// the sun, cut by the cloud density, with the stars behind thin cloud
    /// by night. Only `at`'s direction matters.
    fn cloud_layer(&self, clouds: &Cube, sky: &Scattering, at: Rgb) -> [f32; 4] {
        let day = sky.day;
        // ComputeCloudBoxColor, through the day sky's turn.
        let turned = sky_turn_day(at, day.t);
        let sparse = clouds.sample(turned);
        let dense = self
            .clouds_dense
            .as_ref()
            .map_or(sparse, |c| c.sample(turned));
        let mix = ((self.cloud_density - 0.5) * 2.).clamp(0., 1.);
        let mut cloud: [f32; 4] = std::array::from_fn(|i| sparse[i] + (dense[i] - sparse[i]) * mix);
        // DarkenCloudsAtNight.
        let dark = (1. - day.night_ratio * 0.8).powf(2.5);
        for c in &mut cloud[..3] {
            *c *= dark;
        }
        // ApplyCloudIllumination: lit through the front by day, glowing at
        // thin rims around the sun.
        let v_dot_l = dot(normalize(at), sky.toward);
        let front = ((-v_dot_l + 1.) * 0.5).powf(0.3) * cloud[3] * 0.5 * day.day_ratio.powi(4);
        let back = v_dot_l.max(0.).powi(16) * (0.86 - cloud[3]).clamp(0., 1.).powf(1.6) * 10.;
        let lit = (front + back).clamp(0., 1.);
        let glow = normalize(sky.sun).map(|c| c * 10.);
        for (c, g) in cloud[..3].iter_mut().zip(glow) {
            *c += (*c * g - *c) * lit;
        }
        // Cloud density as a cut through the cloud alpha (BlendLayer).
        let keep = ((cloud[3].clamp(0., 1.) - 1.) + 2. * (self.cloud_density * 2.).clamp(0., 1.))
            .clamp(0., 1.);
        for c in &mut cloud {
            *c *= keep;
        }
        // The stars show through where clouds are thin, by night.
        if let Some(stars) = &self.stars {
            let s = stars.sample(sky_turn_night(at, day.t));
            let f = day.night_ratio * (1. - cloud[3]).powi(3);
            for i in 0..4 {
                cloud[i] += (s[i] - cloud[i]) * f;
            }
        }
        cloud
    }
}

/// cMtxSkyRotDay times `v`: the clouds turn once a day about up.
fn sky_turn_day(v: Rgb, t: f32) -> Rgb {
    let (s, c) = (std::f32::consts::TAU * t).sin_cos();
    [c * v[0] + s * v[2], v[1], -s * v[0] + c * v[2]]
}

/// cMtxSkyRotNight times `v`: the stars turn the other way and tip.
fn sky_turn_night(v: Rgb, t: f32) -> Rgb {
    let b = std::f32::consts::TAU * (1. - t);
    let c = std::f32::consts::PI * (t + 0.5).fract();
    let ((sb, cb), (sc, cc)) = (b.sin_cos(), c.sin_cos());
    [
        cb * v[0] + sb * v[2],
        sc * sb * v[0] + cc * v[1] - sc * cb * v[2],
        -cc * sb * v[0] + sc * v[1] + cc * cb * v[2],
    ]
}

/// How the sky is lifted for a camera above the sea
/// (SKY_SCATTERING_OPTIMIZATION_FACTOR: camera height times 0.0008), and
/// cScatteringRelativeCameraHeight and CameraHeightFactor from the
/// camera's height over its far plane.
/// The baked map, which lights the island, takes a camera 100 m up with a
/// 2 km far plane; `pass` draws the seen sky from the camera's own height
/// against the same far plane. TODO: the far plane the haze runs to is not
/// traced. Cameras default to a FarClip of 1000 (Anno1800.exe 0x140b50f84,
/// 0x141c6d1f0), but with the presets' haze start and intensity that
/// hazes Moderate 01 warm at dusk two thirds over at the top zoom's look
/// point, which the game does not show; what the game camera sets at
/// runtime, or how the engine scales ScatteringStart, is still to find.
const CAMERA_HEIGHT: f32 = 100.;
const CAMERA_FAR: f32 = 2000.;

/// The highest the game's camera goes (config/game/camera.xml, Normal
/// preset 20, the MaxZoomPreset). The editor's camera climbs far past it,
/// and the lift is linear in height: at 1 km it is 0.8, which lowers the
/// sky's horizon 39° under the real one and slides the cloud layer as the
/// camera zooms. The seen sky takes the height capped here.
const CAMERA_MAX_HEIGHT: f32 = 275.;
const CAMERA_LIFT: f32 = CAMERA_HEIGHT * 0.0008;

/// cTimeOfDay and cDayNight at day fraction `t` (0 midnight, 0.5 noon), as
/// the engine computes them (0x14f9a79e0 with 0x141e77c20's constants):
/// day from 0.2 to 0.8, twilight 0.1 to 0.3 and 0.7 to 0.9.
#[derive(Clone, Copy, Debug)]
struct Day {
    t: f32,
    /// cTimeOfDay.y, .z, .w: 1 by day, how far through the day (0 to 1)
    /// and the night (0.5 at midnight).
    is_day: f32,
    progress: f32,
    night_progress: f32,
    /// cDayNight: day and night ratio (summing to 1), day and night value.
    day_ratio: f32,
    night_ratio: f32,
    day_value: f32,
    night_value: f32,
}

impl Day {
    fn at(t: f32) -> Self {
        let sat = |v: f32| v.clamp(0., 1.);
        let (day_ratio, day_value, night_value) = if (0.3..=0.7).contains(&t) {
            (1., 1., 0.)
        } else if t <= 0.1 || t >= 0.9 {
            (0., 0., 1.)
        } else if t < 0.3 {
            (
                (t - 0.1) / 0.2,
                ((t - 0.2) / 0.1).max(0.),
                1. - ((t - 0.1) / 0.1).min(1.),
            )
        } else {
            (
                1. - (t - 0.7) / 0.2,
                1. - ((t - 0.7) / 0.1).min(1.),
                ((t - 0.8) / 0.1).max(0.),
            )
        };
        Self {
            t,
            is_day: if (0.2..=0.8).contains(&t) { 1. } else { 0. },
            progress: sat((t - 0.2) / 0.6),
            night_progress: if t > 0.2 {
                ((t - 0.8) / 0.4).clamp(0., 0.5)
            } else {
                (t / 0.4 + 0.5).clamp(0.5, 1.)
            },
            day_ratio,
            night_ratio: 1. - day_ratio,
            day_value,
            night_value,
        }
    }
}

/// ComputeSkyColorHelper's inputs for one time of day.
struct Scattering {
    zenith: Rgb,
    horizon: Rgb,
    horizon_falloff: Rgb,
    sun: Rgb,
    sun_on: bool,
    toward: Rgb,
    day: Day,
}

// The sun's glow fades in from an outer to an inner angle and is raised to
// a falloff. TODO: the engine keeps both angles and the falloff in its
// atmosphere state (offsets 128, 132, 136), set by code not found; these
// are estimates.
const SUN_INNER_DEGREES: f32 = 0.5;
const SUN_OUTER_DEGREES: f32 = 20.;
const SUN_FALLOFF: f32 = 2. * 3.;

impl Scattering {
    /// ComputeSkyColor toward `d` at full scattering depth (the sky dome).
    fn color(&self, d: Rgb) -> Rgb {
        let d = normalize([d[0], d[1] + CAMERA_LIFT, d[2]]);
        let relative = CAMERA_HEIGHT / CAMERA_FAR;
        let horizon_amount = 1. - ((d[1] + relative) / (relative + 1.)).clamp(0., 1.);
        let v_dot_l = dot(d, self.toward).max(0.);
        let illumination =
            0.5 + v_dot_l.powi(3) * (self.day.day_value + self.day.night_value * 1.2);
        let (inner, outer) = (
            SUN_INNER_DEGREES.to_radians().cos(),
            SUN_OUTER_DEGREES.to_radians().cos(),
        );
        let factor = if self.sun_on {
            1. / (inner - outer)
        } else {
            0.
        };
        let sun_amount = (v_dot_l - outer).clamp(0., 1.)
            * factor
            * (self.day.is_day + self.day.night_value)
                .clamp(0., 1.)
                .powf(0.2);
        let sun = sun_amount.max(0.).powf(SUN_FALLOFF);
        std::array::from_fn(|i| {
            let f = horizon_amount.powf(self.horizon_falloff[i]);
            (self.zenith[i] + (self.horizon[i] - self.zenith[i]) * f) * illumination
                + self.sun[i] * sun
        })
    }
}

fn dot(a: Rgb, b: Rgb) -> f32 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

fn normalize(v: Rgb) -> Rgb {
    let l = dot(v, v).sqrt().max(1e-12);
    v.map(|c| c / l)
}

impl Sky for Preset {
    // The light follows LightDirection by day and the moon curve at night
    // from the same side, so sun and moon share the direction.
    fn sun_moon(&self, hours: f32) -> SunMoon {
        let toward = self.toward(hours);
        SunMoon {
            sun: toward,
            moon: toward,
        }
    }

    fn bake(&self, hours: f32) -> Baked {
        Preset::bake(self, hours)
    }

    fn day_ratio(&self, hours: f32) -> f32 {
        Day::at((hours / 24.).rem_euclid(1.)).day_ratio
    }

    // "Tone mapping lite" (posteffect_aa_bloom.fx): gamma 2.2 first, then
    // exposure, then each channel above the preset's ToneMappingMaxLinear
    // bends toward its ToneMappingWhite. Tone mapping off leaves every
    // value linear.
    fn curve(&self) -> Option<gpu::Curve> {
        let (max_linear, white) = self.tone.unwrap_or((f32::MAX / 4., f32::MAX / 2.));
        Some(gpu::Curve::Shoulder {
            gamma: 2.2,
            max_linear,
            white,
        })
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    // functions_shadow.fxh: two samples of the map's alpha at world xz
    // × 0.01 and × 0.015 × scale less cCloudShadowsDeltas, averaged, then
    // CalculateCloudShadow with normal (not inverted) shadows: 1 - (1 +
    // (noise - 1) × 2.75 density), raised to 3 × intensity. TODO: the
    // engine's deltas are not traced; the layers drift with the wind at
    // `speed` world units a second, the second at half that for depth.
    fn cloud_shadows(&self, _hours: f32) -> Option<gpu::CloudShadows> {
        let [scale, speed, density, intensity] = self.cloud_shadow;
        let map = self.cloud_map.as_ref()?;
        if intensity <= 0. || scale <= 0. {
            return None;
        }
        let layer = |s: f32, drift: f32| gpu::CloudLayer {
            scale: s,
            offset: [0.; 2],
            velocity: self.wind.map(|w| w * speed * s * drift),
        };
        Some(gpu::CloudShadows {
            map: map.0.clone(),
            layers: [layer(0.01 * scale, 1.), layer(0.015 * scale, 0.5)],
            contrast: 2.75 * density,
            power: 3. * intensity,
        })
    }
}

/// The presets an island of `region` can pick: the region's named presets
/// from `ambientsettings/local/` and the generic `amb_default_*` ones, by
/// their `<Name>`, without interiors, old, movie and colour-graded ones.
pub fn ambient_names(install: &Install, region: &str) -> Vec<String> {
    let prefix = match region {
        "Colony01" => "south_america_caribic_",
        "Arctic" => "dlc03_",
        "Africa" => "colony_02",
        _ => "moderate_",
    };
    let mut out: Vec<String> = (install.search("config/engine/ambientsettings/", 2000))
        .into_iter()
        .filter_map(|id| {
            let name = install.file_name(id).strip_suffix(".xml")?;
            let skip = [
                "benchmark",
                "test",
                "_int_",
                "ign_",
                "senate",
                "_bw",
                "sepia",
            ];
            if !(name.starts_with(prefix) || name.starts_with("amb_default_"))
                || skip.iter().any(|s| name.contains(s))
                || !install.file_path(id).contains("/ambientsettings/local/")
                || ["_old", "_movie", "_gc_", "_ls_"]
                    .iter()
                    .any(|s| name.contains(s))
            {
                return None;
            }
            let bytes = install.read_file(id).ok()?;
            tag(&String::from_utf8_lossy(&bytes), "Name").map(str::to_owned)
        })
        .collect();
    anno_core::ambient::order_presets(&mut out, prefix);
    out
}

/// The presets among `names`, by their `<Name>` in `ambientsettings/local/`
/// (1800 names presets, not files), each lit and drawn by its sky.
pub fn ambients(install: &Install, names: &[&str]) -> Vec<(String, Ambient)> {
    (presets(install, names).into_iter())
        .map(|(name, preset, water)| {
            let ambient = Ambient {
                exposure: preset.exposure,
                glow: preset.glow,
                night_glow: preset.night_glow,
                sun_occlusion: preset.sun_occlusion,
                occlusion: preset.occlusion,
                sky: Some(Arc::new(preset)),
                sea: water.map(|w| Arc::new(w) as Arc<dyn anno_core::ambient::Sea>),
                ..Ambient::default()
            };
            (name, ambient)
        })
        .collect()
}

/// The presets among `names` that have a file, sharing the cube maps they
/// name, each with its sea.
fn presets(install: &Install, names: &[&str]) -> Vec<(String, Preset, Option<crate::sea::Water>)> {
    let files = install.search("config/engine/ambientsettings/", 2000);
    let mut cubes = Cubes::default();
    let cloud_map = CloudMap::read(install);
    let mut out: Vec<(String, Preset, Option<crate::sea::Water>)> = Vec::new();
    for &id in &files {
        if !install.file_path(id).contains("/ambientsettings/local/") {
            continue;
        }
        let Ok(bytes) = install.read_file(id) else {
            continue;
        };
        let xml = String::from_utf8_lossy(&bytes);
        let Some(name) = tag(&xml, "Name") else {
            continue;
        };
        let Some(&wanted) = names.iter().find(|w| w.eq_ignore_ascii_case(name)) else {
            continue;
        };
        if out.iter().any(|(n, ..)| n == wanted) {
            continue;
        }
        if let Some(preset) = Preset::read(install, &xml, &mut cubes, cloud_map.as_ref()) {
            out.push((wanted.to_owned(), preset, crate::sea::Water::read(&xml)));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn curves_interpolate_and_hold_past_the_ends() {
        let xml = "<G><size>2</><array><i><t>0.25</><c.r>0</><c.g>0</><c.b>0</><c.a>1</></>\
                   <i><t>0.75</><c.r>1</><c.g>0.5</><c.b>0</><c.a>2</></></></>";
        let c = Curve::read(xml, "G");
        assert_eq!(c.at(0.), [0., 0., 0., 1.]);
        assert_eq!(c.at(0.5), [0.5, 0.25, 0., 1.5]);
        assert_eq!(c.color(0.75), [2., 1., 0.]);
        assert_eq!(c.value(1.), 2.);
    }

    // Needs the game: Moderate_01, the Old World's usual preset, with its
    // global curves and every cube, lights noon warm from high up and
    // night by the blue moon curve, its sky bluer overhead than at the
    // horizon.
    #[test]
    fn moderate_01_reads_bakes_and_lights() {
        let Some(dir) = anno_formats::vfs::install_dir("ANNO1800_DIR") else {
            return;
        };
        let install = Install::open(dir, crate::ARCHIVE_FAMILIES).unwrap();
        assert!(ambients(&install, &["Moderate_01"])[0].1.sky.is_some());
        let preset = presets(&install, &["Moderate_01"]).remove(0).1;
        assert!(preset.clouds.is_some() && preset.stars.is_some());
        assert!(
            preset.light.iter().all(Option::is_some),
            "noon, dawn and night cubes"
        );
        assert_eq!(preset.tone, Some((0.8, 1.3)));

        let noon = preset.bake(12.);
        // At noon the light comes from where LightDirection points away from.
        let ld = preset.light_direction;
        assert!(
            dot(noon.toward, ld) < -0.999,
            "noon sun {:?} against {ld:?}",
            noon.toward
        );
        // DiffuseGraph at noon times DiffuseVariance, gamma 2.2 to linear.
        let red = (0.922f32 * 2.396 * 0.7725).powf(2.2);
        assert!(
            (noon.light[0] - red).abs() < 0.03,
            "{:?} vs {red}",
            noon.light
        );
        assert!(noon.light[0] > noon.light[2], "warm sun {:?}", noon.light);
        let (up, side) = (
            noon.map.sample([0., 1., 0.]),
            noon.map.sample([1., 0.05, 0.]),
        );
        assert!(
            up[2] / up[0] > side[2] / side[0],
            "zenith {up:?} bluer than horizon {side:?}"
        );

        let night = preset.bake(0.);
        assert!(
            night.light[2] > night.light[0],
            "blue night {:?}",
            night.light
        );
        assert!(noon.ambient.is_some(), "lit by the diffuse cubes");
    }

    // cDayNight and cTimeOfDay at midnight, sunrise, mid-morning, noon and
    // dusk, as the engine's day/night function gives them.
    #[test]
    fn day_follows_the_engine() {
        let v = |t: f32| {
            let d = Day::at(t);
            [
                d.is_day,
                d.progress,
                d.night_progress,
                d.day_ratio,
                d.night_ratio,
                d.day_value,
                d.night_value,
            ]
        };
        assert_eq!(v(0.), [0., 0., 0.5, 0., 1., 0., 1.]);
        assert_eq!(v(0.2), [1., 0., 1., 0.5, 0.5, 0., 0.]);
        assert_eq!(v(0.5), [1., 0.5, 0., 1., 0., 1., 0.]);
        let dusk = v(0.85);
        assert_eq!(dusk[0], 0.);
        assert!(
            (dusk[3] - 0.25).abs() < 1e-5 && (dusk[6] - 0.5).abs() < 1e-5,
            "{dusk:?}"
        );
    }

    // Needs the game: Moderate_01 casts cloud shadows from its
    // CloudShadowParams (scale 2, density 1.025, intensity 0.2) over the
    // shipped cloud layer, drifting with the wind.
    #[test]
    fn moderate_01_casts_cloud_shadows() {
        let Some(dir) = anno_formats::vfs::install_dir("ANNO1800_DIR") else {
            return;
        };
        let install = Install::open(dir, crate::ARCHIVE_FAMILIES).unwrap();
        let preset = presets(&install, &["Moderate_01"]).remove(0).1;
        let clouds = preset.cloud_shadows(12.).expect("Moderate_01 casts them");
        assert!((clouds.contrast - 2.75 * 1.025).abs() < 1e-4);
        assert!((clouds.power - 0.6).abs() < 1e-4);
        assert_eq!(clouds.layers.map(|l| l.scale), [0.02, 0.03]);
        assert!(clouds.layers.iter().all(|l| l.velocity != [0.; 2]));
    }
}
