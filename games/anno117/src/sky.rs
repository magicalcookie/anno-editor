// Owns the island's sky: an ambient preset's physical atmosphere, baked on
// the CPU the way Anno 117's sky.fx does on the GPU, into what the renderer
// draws and lights with: the sky's radiance in every direction, the sun's
// light on the ground and the radiance of its disc.
//
// The model is Bruneton's and Hillaire's (functions_atmosphere.fxh):
// Rayleigh and Mie scattering with exponential density, an ozone tent,
// an optional custom layer, a transmittance table and a multiple
// scattering table. Retail renders the sky from the ground (r = bottom)
// into a cube for lighting, and per pixel from the camera for what is
// seen; this bakes the ground view for lighting, and `pass` draws the
// seen sky and the haze from the camera.
// Engine addresses here are Anno117.exe build 24988830 (Steam).
mod clouds;
mod pass;

pub use clouds::{Clouds, Weather};
pub use pass::{SkyPasses, warm};

use anno_core::ambient::{Baked, Sky, SunMoon};
use anno_formats::xml::{block, tag};
use gpui_editor_kit::gpu::SkyMap;
use rayon::prelude::*;
use std::f64::consts::PI;

type Rgb = [f64; 3];

/// `PhysicalAtmosphere` of an ambient preset, in metres and per metre.
/// Defaults are the engine's (0x14A21FC90): Earth's radii, Bruneton's
/// coefficients, `Scale` 10. Mie's scattering and absorption per unit of
/// `MieStrength` (4 and 0.4) and the ozone tent are not in the files.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Atmosphere {
    bottom: f64,
    top: f64,
    /// `SunIrradiance` times `SunStrength`.
    sun: Rgb,
    /// What the ground reflects into the multiple-scattering table.
    ground_albedo: Rgb,
    mie_scattering: f64,
    mie_absorption: f64,
    mie_height: f64,
    mie_g: f64,
    rayleigh: Rgb,
    rayleigh_height: f64,
    ozone: Rgb,
    ozone_peak: f64,
    ozone_width: f64,
    custom_scattering: Rgb,
    custom_absorption: Rgb,
    custom_height: f64,
    custom_width: f64,
    custom_strength: f64,
    /// Divides every distance and multiplies every density when the engine
    /// fills cbAtmosphere (0x1424FDBA0: radii and scale
    /// heights over it, strengths times it): the sky seen from the ground
    /// stays the same, the haze over the island thickens. The bake ignores
    /// it.
    pub scale: f64,
    /// `NearFadeDist`, world units: where AtmosphereScatteringFactor
    /// brings scattering near the camera back to full (its
    /// cScatterFadeScatterDist; the environment's update copies it there
    /// every frame, Anno117.exe 0x1424821A8).
    pub near_fade: f64,
    /// The moon's irradiance (`MoonLight`, outside the atmosphere block),
    /// and how much of it the sky scatters (`MoonSkyStrength`, Anno's
    /// cAtmosphereMoonScatterFactor: "our moon is quite bright").
    moon: Rgb,
    moon_sky: f64,
}

impl Default for Atmosphere {
    fn default() -> Self {
        Self::from_fields(|_| None)
    }
}

/// globals.xml's `MinimumSkyLight`: the sky's radiance above the horizon
/// never drops below this length (night; a clear day is far above it).
pub(crate) const MINIMUM_SKY_LIGHT: f64 = 0.07;

/// Sun disc radiance per unit of irradiance: environment.fx's "arbitrary
/// factor" (AddSunDiskLight).
const SUN_DISC: f64 = 150.;

const TRANSMITTANCE: [usize; 2] = [256, 64];
/// Entries of `Atmosphere::ambient`.
const AMBIENT: usize = 32;
const MULTISCATTER: usize = 32;
/// Sky map size: 256 around, 128 from zenith to nadir. The sky is smooth;
/// the sun disc is drawn analytically on top.
const MAP: [u32; 2] = [256, 128];

impl Atmosphere {
    /// The preset's atmosphere, or `None` when it lights with fixed colors
    /// (`UseAtmosphere` 0).
    pub fn read(xml: &str) -> Option<Self> {
        if tag(xml, "UseAtmosphere").is_some_and(|v| v.trim() == "0") {
            return None;
        }
        let block = block(xml, "PhysicalAtmosphere").unwrap_or("");
        let mut a = Self::from_fields(|name| field(block, name));
        if let Some(moon) = field(xml, "MoonLight").filter(|v| v.len() == 3) {
            a.moon = [moon[0], moon[1], moon[2]];
        }
        if let Some(v) = tag(xml, "MoonSkyStrength").and_then(|v| v.parse().ok()) {
            a.moon_sky = v;
        }
        Some(a)
    }

    fn from_fields(get: impl Fn(&str) -> Option<Vec<f64>>) -> Self {
        let one = |name: &str, default: f64| {
            get(name)
                .and_then(|v| v.first().copied())
                .unwrap_or(default)
        };
        let three = |name: &str, default: Rgb| {
            get(name)
                .filter(|v| v.len() == 3)
                .map_or(default, |v| [v[0], v[1], v[2]])
        };
        let scale = one("Scale", 10.).max(1e-3);
        let sun_strength = one("SunStrength", 15.);
        let mie = one("MieStrength", 5e-6);
        let rayleigh = one("RayleighStrength", 1e-4);
        let ozone = one("OzoneStrength", 1e-6);
        Self {
            bottom: 6_360_000.,
            top: 6_420_000.,
            sun: three("SunIrradiance", [1.; 3]).map(|c| c * sun_strength),
            ground_albedo: three("GroundAlbedo", [0.3; 3]),
            mie_scattering: 4. * mie,
            mie_absorption: 0.4 * mie,
            mie_height: one("MieScaleHeight", 1200.),
            mie_g: one("MieG", 0.8),
            rayleigh: three("RayleighScattering", [0.058, 0.135, 0.331]).map(|c| c * rayleigh),
            rayleigh_height: one("RayleighScaleHeight", 8000.),
            ozone: [0.65, 1.88, 0.085].map(|c| c * ozone),
            ozone_peak: 25_000.,
            ozone_width: 15_000.,
            custom_scattering: three("CustomScattering", [0.; 3]),
            custom_absorption: three("CustomAbsorption", [0.; 3]),
            custom_height: one("CustomHeight", 0.),
            custom_width: one("CustomWidth", 0.),
            custom_strength: one("CustomStrength", 0.),
            scale,
            // The engine's default (0x14A21FC90).
            near_fade: one("NearFadeDist", 50.),
            // amb_roman_01's; the engine's own default is not known.
            moon: [0.379, 0.433, 1.097],
            moon_sky: 0.17,
        }
    }

    /// Bakes the sky for the sun and moon in `at`, with the ground below the
    /// horizon reflecting `ground` (`GroundColorAmbient`). The tables do not
    /// depend on the sun; a few milliseconds a bake are theirs.
    pub fn bake(&self, at: SunMoon, ground: [f32; 3]) -> Baked {
        let ground = ground.map(f64::from);
        let t = Transmittance::new(self);
        let ms = Multiscatter::new(self, &t);
        let (day, night) = (at.day(), at.night());
        let to_sun = at.sun.map(f64::from);
        let to_moon = at.moon.map(f64::from);
        // The moon lights the sky only at night, and dimmed: it is the sky
        // seen (ComputeAtmosphereRange), which the scene's light also
        // comes from here.
        let sources = [
            (to_sun, self.sun),
            (to_moon, self.moon.map(|c| c * self.moon_sky * night)),
        ];
        let [w, h] = MAP;
        let texels: Vec<[f32; 3]> = (0..w * h)
            .into_par_iter()
            .map(|i| {
                let d = SkyMap::direction(w, h, i % w, i / w).map(f64::from);
                self.radiance(&t, &ms, d, &sources, ground, MINIMUM_SKY_LIGHT)
                    .map(|c| c as f32)
            })
            .collect();
        // csComputeAtmosphericLight: the light on the ground, divided by π
        // because the engine multiplies it by albedo and N·L alone. The sun
        // by day, its 0.05 floor keeping a low sun from going black; the
        // moon at night, through the atmosphere toward it. The scene has
        // one light: whichever stands above the horizon.
        let (toward, light) = if at.sun[1] > 0. {
            let through = t.to_sun(self, self.bottom, to_sun[1].max(0.05));
            (
                at.sun,
                std::array::from_fn(|c| self.sun[c] * through[c] * day / PI),
            )
        } else {
            let through = t.to_sun(self, self.bottom, to_moon[1].max(0.05));
            (
                at.moon,
                std::array::from_fn(|c| self.moon[c] * through[c] * night / PI),
            )
        };
        let through = t.to_top(self, self.bottom, to_sun[1]);
        let disc = if at.sun[1] > 0. {
            std::array::from_fn(|c| (SUN_DISC * self.sun[c] * through[c]) as f32)
        } else {
            [0.; 3]
        };
        let map = SkyMap {
            width: w,
            height: h,
            texels,
        };
        Baked {
            map: std::sync::Arc::new(map),
            toward,
            light: light.map(|c: f64| c as f32),
            disc,
            ambient: None,
        }
    }

    /// The transmittance and multiple-scattering tables as the sky pass
    /// uploads them: RGBA, `TRANSMITTANCE` and `MULTISCATTER` squared; and
    /// the ambient table over `ground` (`ambient`).
    #[allow(clippy::type_complexity)]
    fn tables(&self, ground: Rgb) -> (Vec<[f32; 4]>, Vec<[f32; 4]>, Vec<[f32; 3]>) {
        let t = Transmittance::new(self);
        let ms = Multiscatter::new(self, &t);
        let ambient = self.ambient_with(&t, &ms, ground);
        let rgba = |texels: &[Rgb]| {
            (texels.iter())
                .map(|c| [c[0] as f32, c[1] as f32, c[2] as f32, 1.])
                .collect()
        };
        (rgba(&t.texels), rgba(&ms.texels), ambient)
    }

    /// The clear sky's mean radiance over every direction, ground included,
    /// per unit of light irradiance, for `AMBIENT` heights of the light
    /// from cos(zenith) -0.25 to 1: what the clouds take as the sky's
    /// light (AmbientRadianceClearSH[0], the cube's all-round average).
    fn ambient_with(&self, t: &Transmittance, ms: &Multiscatter, ground: Rgb) -> Vec<[f32; 3]> {
        const DIRECTIONS: usize = 96;
        (0..AMBIENT)
            .into_par_iter()
            .map(|i| {
                let mu = -0.25 + 1.25 * i as f64 / (AMBIENT - 1) as f64;
                let toward = [(1. - mu * mu).max(0.).sqrt(), mu, 0.];
                let sources = [(toward, [1.; 3])];
                let mut sum = [0.; 3];
                // A Fibonacci sphere.
                for k in 0..DIRECTIONS {
                    let y = 1. - (k as f64 + 0.5) * 2. / DIRECTIONS as f64;
                    let side = (1. - y * y).sqrt();
                    let phi = k as f64 * PI * (3. - 5f64.sqrt());
                    let d = [side * phi.cos(), y, side * phi.sin()];
                    // Per unit of irradiance: no MinimumSkyLight floor here,
                    // the pass applies it to the summed light.
                    let l = self.radiance(t, ms, d, &sources, ground, 0.);
                    for c in 0..3 {
                        sum[c] += l[c];
                    }
                }
                sum.map(|c| (c / DIRECTIONS as f64) as f32)
            })
            .collect()
    }

    /// Optical density at `height` metres of Rayleigh, Mie, ozone and the
    /// custom layer (unitless; multiply by the coefficients).
    fn density(&self, height: f64) -> [f64; 4] {
        let custom = if self.custom_width > 0. && self.custom_strength > 0. {
            self.custom_strength * (-(height - self.custom_height).abs() / self.custom_width).exp()
        } else {
            0.
        };
        [
            (-height / self.rayleigh_height).exp(),
            (-height / self.mie_height).exp(),
            (1. - (height - self.ozone_peak).abs() / self.ozone_width).max(0.),
            custom,
        ]
    }

    /// Extinction and scattering per metre at `height`.
    fn medium(&self, height: f64) -> (Rgb, Rgb) {
        let [r, m, o, c] = self.density(height);
        let scattering = std::array::from_fn(|k| {
            self.rayleigh[k] * r + self.mie_scattering * m + self.custom_scattering[k] * c
        });
        let extinction = std::array::from_fn(|k| {
            scattering[k]
                + self.mie_absorption * m
                + self.ozone[k] * o
                + self.custom_absorption[k] * c
        });
        (extinction, scattering)
    }

    fn distance_to_top(&self, r: f64, mu: f64) -> f64 {
        let d = r * r * (mu * mu - 1.) + self.top * self.top;
        (-r * mu + d.max(0.).sqrt()).max(0.)
    }

    fn distance_to_bottom(&self, r: f64, mu: f64) -> f64 {
        let d = r * r * (mu * mu - 1.) + self.bottom * self.bottom;
        (-r * mu - d.max(0.).sqrt()).max(0.)
    }

    fn hits_ground(&self, r: f64, mu: f64) -> bool {
        mu < 0. && r * r * (mu * mu - 1.) + self.bottom * self.bottom >= 0.
    }

    /// csComputeSky for one direction from the ground: single scattering
    /// of each light in `sources` (toward it, its irradiance) by Rayleigh
    /// and Mie plus the multiple-scattering table, marched in steps growing
    /// by a tenth from 10 m, then the lit ground where the ray meets it;
    /// above the horizon raised to at least `floor` (MinimumSkyLight).
    fn radiance(
        &self,
        t: &Transmittance,
        ms: &Multiscatter,
        d: [f64; 3],
        sources: &[([f64; 3], Rgb)],
        ground: Rgb,
        floor: f64,
    ) -> Rgb {
        let r = self.bottom;
        let mu = d[1];
        let hits = self.hits_ground(r, mu);
        let dist = if hits {
            self.distance_to_bottom(r, mu)
        } else {
            self.distance_to_top(r, mu)
        };
        let mut light = [0.; 3];
        let mut through = [1.; 3];
        let (mut at, mut step) = (0., 10.);
        while at < dist {
            let rd = (at * at + 2. * r * mu * at + r * r)
                .sqrt()
                .clamp(self.bottom, self.top);
            let height = rd - self.bottom;
            let [dr, dm, _, dc] = self.density(height);
            let (extinction, _) = self.medium(height);
            let mut inscatter = [0.; 3];
            for (toward, irradiance) in sources {
                if irradiance.iter().all(|&e| e <= 0.) {
                    continue;
                }
                let nu = dot(d, *toward);
                let rayleigh_phase = 3. * (1. + nu * nu) / (16. * PI);
                let mie_phase = hg_phase(self.mie_g, nu);
                let mu_sd = ((r * toward[1] + at * nu) / rd).clamp(-1., 1.);
                let lit = t.to_sun(self, rd, mu_sd);
                let multi = ms.sample(self, rd, mu_sd);
                for k in 0..3 {
                    let rayleigh = self.rayleigh[k] * dr;
                    let mie = self.mie_scattering * dm;
                    let single = mie * mie_phase + rayleigh * rayleigh_phase;
                    // The custom layer scatters the same in every direction.
                    let custom = self.custom_scattering[k] * dc;
                    inscatter[k] +=
                        irradiance[k] * ((single + custom) * lit[k] + multi[k] * (rayleigh + mie));
                }
            }
            integrate(inscatter, extinction, step, &mut light, &mut through);
            at += step;
            step *= 1.1;
        }
        if hits {
            for (toward, irradiance) in sources {
                let nu = dot(d, *toward);
                let mu_si = ((r * toward[1] + dist * nu) / self.bottom).clamp(-1., 1.);
                let lit = t.to_sun(self, self.bottom, mu_si);
                for k in 0..3 {
                    light[k] +=
                        through[k] * ground[k] * lit[k] * irradiance[k] * mu_si.max(0.) / PI;
                }
            }
        } else if d[1] > 0.05 {
            let length = light.iter().map(|c| c * c).sum::<f64>().sqrt();
            if length > 0. && length < floor {
                light = light.map(|c| c * floor / length);
            }
        }
        light
    }
}

/// Henyey-Greenstein phase (functions_volumetric.fxh ComputeHGPhase).
fn hg_phase(g: f64, nu: f64) -> f64 {
    let gg = g * g;
    (1. - gg) / (4. * PI * (1. + gg - 2. * g * nu).powf(1.5))
}

/// IntegrateStep with BETTER_SCATTER_INTEGRATION: the light scattered over
/// a step of constant medium, integrated exactly (Hillaire, Frostbite).
fn integrate(inscatter: Rgb, extinction: Rgb, step: f64, light: &mut Rgb, through: &mut Rgb) {
    if extinction.iter().map(|e| e * e).sum::<f64>() < 1e-10 * 1e-10 {
        return;
    }
    for k in 0..3 {
        let t = (-extinction[k] * step).exp();
        light[k] += through[k] * inscatter[k] * (1. - t) / extinction[k];
        through[k] *= t;
    }
}

/// Transmittance from a point at radius r looking at cos(zenith) mu to the
/// atmosphere's top, tabulated over Bruneton's mapping (sky.fx
/// csComputeTransmittanceLUT; TransmittanceUVFromRay).
struct Transmittance {
    texels: Vec<Rgb>,
}

impl Transmittance {
    fn new(a: &Atmosphere) -> Self {
        let [w, h] = TRANSMITTANCE;
        let texels = (0..w * h)
            .into_par_iter()
            .map(|i| {
                let (x, y) = (i % w, i / w);
                let (r, mu) = ray_from_uv(a, x as f64 / (w - 1) as f64, y as f64 / (h - 1) as f64);
                let samples = 256;
                let dx = a.distance_to_top(r, mu) / samples as f64;
                let mut depth = [0.; 3];
                for j in 0..=samples {
                    let di = j as f64 * dx;
                    let ri = (di * di + 2. * r * mu * di + r * r).sqrt() - a.bottom;
                    let weight = if j == 0 || j == samples { 0.5 } else { 1. };
                    let (extinction, _) = a.medium(ri);
                    for k in 0..3 {
                        depth[k] += dx * weight * extinction[k];
                    }
                }
                depth.map(|d| (-d).exp())
            })
            .collect();
        Self { texels }
    }

    fn to_top(&self, a: &Atmosphere, r: f64, mu: f64) -> Rgb {
        let [w, h] = TRANSMITTANCE;
        let (xmu, xr) = uv_from_ray(a, r, mu);
        bilinear(&self.texels, w, h, xmu, xr)
    }

    /// LookupTransmittanceToSun: to the top, faded over the sun's angular
    /// radius as it sets behind the planet.
    fn to_sun(&self, a: &Atmosphere, r: f64, mu_s: f64) -> Rgb {
        const SUN_RADIUS: f64 = 0.004675;
        let sin_h = a.bottom / r;
        let cos_h = -(1. - sin_h * sin_h).max(0.).sqrt();
        let visible = smoothstep(-sin_h * SUN_RADIUS, sin_h * SUN_RADIUS, mu_s - cos_h);
        self.to_top(a, r, mu_s).map(|c| c * visible)
    }
}

/// Bruneton's (r, mu) for unit table coordinates.
fn ray_from_uv(a: &Atmosphere, xmu: f64, xr: f64) -> (f64, f64) {
    let h = (a.top * a.top - a.bottom * a.bottom).sqrt();
    let rho = h * xr;
    let r = (rho * rho + a.bottom * a.bottom).sqrt();
    let dmin = a.top - r;
    let dmax = rho + h;
    let d = dmin + xmu * (dmax - dmin);
    let mu = if d == 0. {
        1.
    } else {
        (h * h - rho * rho - d * d) / (2. * r * d)
    };
    (r, mu.clamp(-1., 1.))
}

fn uv_from_ray(a: &Atmosphere, r: f64, mu: f64) -> (f64, f64) {
    let h = (a.top * a.top - a.bottom * a.bottom).sqrt();
    let rho = (r * r - a.bottom * a.bottom).max(0.).sqrt();
    let d = a.distance_to_top(r, mu);
    let dmin = a.top - r;
    let dmax = rho + h;
    ((d - dmin) / (dmax - dmin).max(1e-9), rho / h)
}

/// Light scattered twice or more, per unit of sun irradiance and of
/// scattering coefficient, over radius and cos(sun zenith) (sky.fx
/// csComputeMultiscatLUT, Hillaire 2020's Ψms).
struct Multiscatter {
    texels: Vec<Rgb>,
}

/// csComputeMultiscatLUT's lowest cos(sun zenith).
const MULTISCATTER_MIN_MU_S: f64 = -0.25;

impl Multiscatter {
    fn new(a: &Atmosphere, t: &Transmittance) -> Self {
        const SPHERE: u32 = 64;
        let n = MULTISCATTER;
        let iso = 1. / (4. * PI);
        let texels = (0..n * n)
            .into_par_iter()
            .map(|i| {
                let (x, y) = (
                    (i % n) as f64 / (n - 1) as f64,
                    (i / n) as f64 / (n - 1) as f64,
                );
                let mu_s = MULTISCATTER_MIN_MU_S + x * (1. - MULTISCATTER_MIN_MU_S);
                let r = a.bottom + y * (a.top - a.bottom);
                let (mut l2, mut fms) = ([0.; 3], [0.; 3]);
                for s in 0..SPHERE {
                    // Hammersley on the sphere: x around, y up.
                    let phi = s as f64 / SPHERE as f64 * 2. * PI;
                    let up = -1. + 2. * (s.reverse_bits() as f64 / 4_294_967_296.);
                    let sx = (1. - up * up).sqrt() * phi.cos();
                    let mu = up;
                    let nu = up * mu_s + sx * (1. - mu_s * mu_s).sqrt();
                    let hits = a.hits_ground(r, mu);
                    let dist = if hits {
                        a.distance_to_bottom(r, mu)
                    } else {
                        a.distance_to_top(r, mu)
                    };
                    let steps = 8. + (32. - 8.) * (1e-4 * dist).clamp(0., 1.);
                    let steps = steps as u32;
                    let dx = dist / steps as f64;
                    let (mut through, mut through_f) = ([1.; 3], [1.; 3]);
                    let (mut lf, mut lp) = ([0.; 3], [0.; 3]);
                    for j in 0..steps {
                        let di = j as f64 * dx;
                        let ri = (di * di + 2. * r * mu * di + r * r)
                            .sqrt()
                            .clamp(a.bottom, a.top);
                        let mu_si = ((r * mu_s + di * nu) / ri).clamp(-1., 1.);
                        let (extinction, scattering) = a.medium(ri - a.bottom);
                        let sun = t.to_sun(a, ri, mu_si);
                        let inscat: Rgb = std::array::from_fn(|k| scattering[k] * iso * sun[k]);
                        integrate(scattering, extinction, dx, &mut lf, &mut through_f);
                        integrate(inscat, extinction, dx, &mut lp, &mut through);
                    }
                    if hits {
                        let mu_si = ((r * mu_s + dist * nu) / a.bottom).clamp(-1., 1.);
                        let sun = t.to_sun(a, a.bottom, mu_si);
                        for k in 0..3 {
                            lp[k] += through[k] * a.ground_albedo[k] * sun[k] * mu_si.max(0.) / PI;
                        }
                    }
                    for k in 0..3 {
                        l2[k] += lp[k] * iso;
                        fms[k] += lf[k] * iso;
                    }
                }
                let norm = 4. * PI / SPHERE as f64;
                std::array::from_fn(|k| l2[k] * norm / (1. - fms[k] * norm))
            })
            .collect();
        Self { texels }
    }

    fn sample(&self, a: &Atmosphere, r: f64, mu_s: f64) -> Rgb {
        let n = MULTISCATTER;
        let xmu = ((mu_s - MULTISCATTER_MIN_MU_S) / (1. - MULTISCATTER_MIN_MU_S)).clamp(0., 1.);
        let xr = ((r - a.bottom) / (a.top - a.bottom)).clamp(0., 1.);
        bilinear(&self.texels, n, n, xmu, xr)
    }
}

/// Bilinear lookup in a table whose first and last texel centres sit at
/// unit coordinates 0 and 1 (how both tables are generated).
fn bilinear(texels: &[Rgb], w: usize, h: usize, u: f64, v: f64) -> Rgb {
    let x = u.clamp(0., 1.) * (w - 1) as f64;
    let y = v.clamp(0., 1.) * (h - 1) as f64;
    let (x0, y0) = (x.floor() as usize, y.floor() as usize);
    let (x1, y1) = ((x0 + 1).min(w - 1), (y0 + 1).min(h - 1));
    let (fx, fy) = (x - x0 as f64, y - y0 as f64);
    let at = |x: usize, y: usize| texels[y * w + x];
    std::array::from_fn(|k| {
        let top = at(x0, y0)[k] * (1. - fx) + at(x1, y0)[k] * fx;
        let bottom = at(x0, y1)[k] * (1. - fx) + at(x1, y1)[k] * fx;
        top * (1. - fy) + bottom * fy
    })
}

fn dot(a: [f64; 3], b: [f64; 3]) -> f64 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

fn smoothstep(e0: f64, e1: f64, x: f64) -> f64 {
    let t = ((x - e0) / (e1 - e0)).clamp(0., 1.);
    t * t * (3. - 2. * t)
}

/// A field's number, or its `<x><y><z>` triple, directly inside `block`.
fn field(block_text: &str, name: &str) -> Option<Vec<f64>> {
    let inner = block(block_text, name)?;
    if inner.contains('<') {
        ["x", "y", "z"]
            .iter()
            .map(|axis| tag(inner, axis)?.parse().ok())
            .collect()
    } else {
        inner.trim().parse().ok().map(|v| vec![v])
    }
}

/// The sun's and moon's way over the day as the engine moves them
/// (Anno117.exe 0x1422AF8F0, sunrise and sunset 0x1422B0290), from a
/// preset's `MinSunElevation`, `MaxSunElevation` (sines of the elevation)
/// and `CamYRotOffset2`. With t the time of day over 24 hours, the sun's
/// elevation runs from asin(min) at midnight to asin(max) at noon along
/// 0.5 - 0.5 cos(2πt), and its azimuth turns once a day, 2πt; the moon's
/// runs from asin(-0.8) at noon to asin(0.6) at midnight, azimuth π - 2πt.
/// Both are then turned about up by CamYRotOffset2 + 3π/4
/// (0x1422AFFA0). t is the renderer's hour, not the game clock's: the
/// clock's hour goes through `renderer_hour` first. So amb_roman_01 (-0.2,
/// 0.644) is at 27° at the clock's 19:00, highest (40°) at 13:43, and sets
/// past 22:00.
/// TODO: with a flag of the environment (+0x28 bit 2) the turn is taken
/// later from another angle (0x140517FD0); not traced, the sun does not
/// follow the camera in game.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DayPath {
    pub min: f32,
    pub max: f32,
    pub turn: f32,
}

/// assets.xml's DayNightFeature `GameDaytimeToRendererDaytimeMapInfo`:
/// the game clock's sunrise start and end and sunset start and end, and
/// the renderer's hours for the sunrise's and the sunset's start.
/// TODO: read from assets.xml rather than hold its values.
const GAME_SUNRISE: (f32, f32) = (6., 8.);
const GAME_SUNSET: (f32, f32) = (18., 21.);
const RENDER_SUNRISE: f32 = 6.;
const RENDER_SUNSET: f32 = 15.;

/// The renderer's hour for the game clock's `hours` (Anno117.exe
/// 0x14F98B100): sunrise and sunset keep their length, shifted to the
/// renderer's starts, the day between is squeezed to fit (8:00 to 18:00
/// becomes 8:00 to 15:00 as shipped) and the night stretched.
pub fn renderer_hour(hours: f32) -> f32 {
    let g = hours.rem_euclid(24.);
    let (rise, rise_end) = GAME_SUNRISE;
    let (set, set_end) = GAME_SUNSET;
    let rise_end_r = rise_end + RENDER_SUNRISE - rise;
    let set_end_r = set_end + RENDER_SUNSET - set;
    // (game from, game to, renderer from, renderer to)
    let (g0, g1, r0, r1) = if g < rise {
        (0., rise, 0., RENDER_SUNRISE)
    } else if g <= rise_end {
        (rise, rise_end, RENDER_SUNRISE, rise_end_r)
    } else if g <= set {
        (rise_end, set, rise_end_r, RENDER_SUNSET)
    } else if g <= set_end {
        (set, set_end, RENDER_SUNSET, set_end_r)
    } else {
        (set_end, 24., set_end_r, 24.)
    };
    let k = ((g - g0) / (g1 - g0)).clamp(0., 1.);
    (r0 + (r1 - r0) * k).rem_euclid(24.)
}

impl DayPath {
    /// At the game clock's `hours`.
    pub fn at(&self, hours: f32) -> SunMoon {
        use std::f32::consts::{FRAC_PI_4, PI, TAU};
        let t = renderer_hour(hours) / 24.;
        let lift = 0.5 - 0.5 * (TAU * t).cos();
        let between = |low: f32, high: f32, s: f32| {
            let (low, high) = (low.clamp(-1., 1.).asin(), high.clamp(-1., 1.).asin());
            low + (high - low) * s
        };
        let turn = self.turn + 3. * FRAC_PI_4;
        let toward = |elevation: f32, azimuth: f32| {
            let a = azimuth + turn;
            [
                elevation.cos() * a.cos(),
                elevation.sin(),
                elevation.cos() * a.sin(),
            ]
        };
        SunMoon {
            sun: toward(between(self.min, self.max, lift), TAU * t),
            moon: toward(between(-0.8, 0.6, 1. - lift), PI - TAU * t),
        }
    }

    /// The day's share of the sun's way, 0 at sunrise and 1 at sunset, as
    /// 0x1422B0290 finds them (cTimeOfDay.z); below 0 or over 1 by night.
    /// A sun that never sets or never rises gives the whole day or none.
    pub fn progress(&self, hours: f32) -> f32 {
        let t = renderer_hour(hours) / 24.;
        match self.rise() {
            Some(rise) => (t - rise) / (1. - 2. * rise),
            None if self.min >= 0. => t,
            None => -1.,
        }
    }

    /// cDayNight.x at the game clock's `hours`, as 0x1422B0290 sets it: 0
    /// by night to 1 by day over a fifth of a day centred on sunrise, and
    /// back down over one centred on sunset. A sun that never sets or
    /// never rises gives 1 or 0.
    pub fn day_ratio(&self, hours: f32) -> f32 {
        let Some(rise) = self.rise() else {
            return if self.min >= 0. { 1. } else { 0. };
        };
        let t = renderer_hour(hours) / 24.;
        let ratio = if t <= 0.5 {
            (t - rise + 0.1) / 0.2
        } else {
            (1. - rise + 0.1 - t) / 0.2
        };
        ratio.clamp(0., 1.)
    }

    /// The renderer's day fraction where the sun crosses the horizon
    /// rising; `None` when it never does.
    fn rise(&self) -> Option<f32> {
        let (low, high) = (
            self.min.clamp(-1., 1.).asin(),
            self.max.clamp(-1., 1.).asin(),
        );
        if low >= 0. || high <= 0. {
            return None;
        }
        // Where 0.5 - 0.5 cos(2πt) lifts the sun past the horizon.
        let s = -low / (high - low);
        Some((2. * s - 1.).asin() / std::f32::consts::TAU + 0.25)
    }
}

/// A preset's atmosphere with the sun's path and the ground it lights.
#[derive(Debug)]
pub struct PresetSky {
    pub atmosphere: Atmosphere,
    pub path: DayPath,
    pub ground: [f32; 3],
    /// `SkyBoxPathNight`: the stars psSkyBox draws by night.
    pub night: Option<std::sync::Arc<anno_core::cube::Cube>>,
    /// `WeatherMap` and the clouds' settings, when the preset has clouds.
    pub clouds: Option<Clouds>,
}

impl Sky for PresetSky {
    fn sun_moon(&self, hours: f32) -> SunMoon {
        self.path.at(hours)
    }

    fn bake(&self, hours: f32) -> Baked {
        let mut baked = self.atmosphere.bake(self.sun_moon(hours), self.ground);
        if let Some(clouds) = &self.clouds {
            let drift = clouds.drift(gpui_editor_kit::gpu::clock());
            let map = clouds.reflect(&baked.map, baked.toward, baked.light, drift.weather);
            baked.map = std::sync::Arc::new(map);
        }
        baked
    }

    fn drifts(&self) -> bool {
        self.clouds.as_ref().is_some_and(|c| c.wind > 0.)
    }

    fn day_ratio(&self, hours: f32) -> f32 {
        self.path.day_ratio(hours)
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn cloud_shadows(&self, hours: f32) -> Option<gpui_editor_kit::gpu::CloudShadows> {
        let at = self.sun_moon(hours);
        let toward = if at.sun[1] > 0. { at.sun } else { at.moon };
        self.clouds
            .as_ref()?
            .shadows(self.path.progress(hours), toward)
    }
}

#[cfg(test)]
mod tests {
    use super::{Atmosphere, SunMoon};

    const PRESET: &str = "<AmbientArea><Info><UseAtmosphere>1</>\
        <PhysicalAtmosphere><GroundAlbedo><x>0.44</><y>0.76</><z>0.87</></>\
        <MieStrength>3e-06</><NearFadeDist>30</><Scale>20</><RayleighStrength>1e-04</>\
        <SunStrength>15</></><SnowDrift><Scale>0.1</></></></>";

    #[test]
    fn preset_fields_inside_the_atmosphere_block_only() {
        let a = Atmosphere::read(PRESET).unwrap();
        // SnowDrift's Scale comes later and must not leak in.
        assert_eq!(a.scale, 20.);
        assert_eq!(a.ground_albedo, [0.44, 0.76, 0.87]);
        assert!((a.mie_scattering - 1.2e-5).abs() < 1e-12);
        assert_eq!(a.near_fade, 30.);
        assert!(Atmosphere::read("<UseAtmosphere>0</>").is_none());
    }

    // Earth's sky from the ground with the sun high: blue overhead, paler
    // toward the horizon, and the sun's light on the ground a warm white
    // near the preset's own LightColor (4.2, 3.6, 3.2 in amb_roman_01).
    #[test]
    fn a_clear_day_is_blue_above_and_lit_warm_white() {
        let a = Atmosphere::read(PRESET).unwrap();
        let baked = a.bake(SunMoon::at(12., 0.644, -0.2), [0.2; 3]);
        let zenith = baked.zenith();
        assert!(
            zenith[2] > zenith[1] && zenith[1] > zenith[0],
            "zenith {zenith:?}"
        );
        let horizon = baked.horizon();
        let ratio = |c: [f32; 3]| c[2] / c[0];
        assert!(
            ratio(horizon) < ratio(zenith),
            "horizon {horizon:?}, zenith {zenith:?}"
        );
        let sun = baked.light;
        assert!(sun[0] > sun[1] && sun[1] > sun[2], "sun {sun:?}");
        assert!((3.0..5.0).contains(&sun[0]) && sun[2] > 2.5, "sun {sun:?}");
        assert!(baked.disc[0] > 1000., "disc {:?}", baked.disc);
    }

    // Noon is where the editor always lit from; the sun is up at 6:00 and
    // gone at 18:00, and at midnight the moon stands where the sun did.
    #[test]
    fn the_sun_crosses_the_sky_from_six_to_six() {
        let noon = SunMoon::at(12., 0.644, -0.2);
        let e = 0.644f32;
        let old = [-0.67 * e.cos(), e.sin(), 0.74 * e.cos()];
        assert!(
            (0..3).all(|i| (noon.sun[i] - old[i]).abs() < 0.01),
            "{noon:?}"
        );
        assert!(SunMoon::at(6., 0.644, -0.2).sun[1].abs() < 1e-3);
        assert!(SunMoon::at(9., 0.644, -0.2).sun[1] > 0.3);
        let midnight = SunMoon::at(0., 0.644, -0.2);
        assert!((midnight.sun[1] - (-0.2f32).sin()).abs() < 1e-4);
        assert!((midnight.moon[1] - noon.sun[1]).abs() < 1e-4);
    }

    // The clock's day squeezed into the renderer's: 6:00 to 8:00 as is,
    // 8:00 to 18:00 into 8:00 to 15:00, 18:00 to 21:00 three hours earlier,
    // the rest of the night stretched to midnight.
    #[test]
    fn the_clock_maps_onto_the_renderers_day() {
        let r = super::renderer_hour;
        for (game, render) in [
            (3., 3.),
            (7., 7.),
            (13., 11.5),
            (18., 15.),
            (19., 16.),
            (21., 18.),
            (22.5, 21.),
        ] {
            assert!((r(game) - render).abs() < 1e-4, "{game} -> {}", r(game));
        }
    }

    // amb_roman_01's day as the engine runs it, in renderer hours: up at
    // 3:46, highest at noon at asin(0.644), down at 20:14 (the clock's
    // 22:07), and the moon
    // high at midnight; the day's share runs 0 to 1 between rise and set.
    // At the clock's 19:00 the sun still stands 27° up.
    #[test]
    fn the_games_day_runs_from_before_four_to_past_eight() {
        let roman = super::DayPath {
            min: -0.2,
            max: 0.644444,
            turn: -2.443461,
        };
        // The clock's hours for the renderer's 3:46, 12:00 and 20:14.
        let rise = 3. + 46. / 60.;
        let set = 21. + (20. + 14. / 60. - 18.) / 2.;
        let noon = 8. + 4. * 10. / 7.;
        assert!((roman.at(19.).sun[1].asin().to_degrees() - 27.2).abs() < 0.3);
        assert!(roman.at(rise).sun[1].abs() < 0.01, "{:?}", roman.at(rise));
        assert!(roman.at(set).sun[1].abs() < 0.01, "{:?}", roman.at(set));
        assert!((roman.at(noon).sun[1] - 0.644444).abs() < 1e-4);
        assert!((roman.at(0.).sun[1] - -0.2).abs() < 1e-4);
        assert!((roman.at(0.).moon[1] - 0.6).abs() < 1e-4);
        assert!(roman.progress(rise).abs() < 0.01 && (roman.progress(set) - 1.).abs() < 0.01);
        assert!((roman.progress(noon) - 0.5).abs() < 1e-4);
        assert!(roman.progress(1.) < 0. && roman.progress(23.5) > 1.);
        // At noon toward π + CamYRotOffset2 + 3π/4 about up.
        let a = std::f32::consts::PI - 2.443461 + 3. * std::f32::consts::FRAC_PI_4;
        let noon = roman.at(noon).sun;
        assert!((noon[2].atan2(noon[0]) - a).abs() < 1e-4, "{noon:?}");
    }

    // The day ratio night masks light by (0x1422B0290's cDayNight.x): half
    // as the sun crosses the horizon, over a fifth of the renderer's day
    // each way, so amb_roman_01's windows start to light at about 20:50 on
    // the game's clock, all by 23:20, and are dark again by 6:10.
    #[test]
    fn the_day_ratio_turns_over_a_fifth_of_a_day_at_sunrise_and_sunset() {
        let roman = super::DayPath {
            min: -0.2,
            max: 0.644444,
            turn: -2.443461,
        };
        let rise = 3. + 46. / 60.;
        let set = 21. + (20. + 14. / 60. - 18.) / 2.;
        assert_eq!(roman.day_ratio(0.), 0.);
        assert_eq!(roman.day_ratio(12.), 1.);
        assert!((roman.day_ratio(rise) - 0.5).abs() < 0.02);
        assert!((roman.day_ratio(set) - 0.5).abs() < 0.02);
        assert_eq!(roman.day_ratio(6.5), 1.);
        assert!(roman.day_ratio(20.7) == 1. && roman.day_ratio(21.) < 1.);
        assert_eq!(roman.day_ratio(23.5), 0.);
        let never = super::DayPath { min: 0.1, ..roman };
        assert_eq!(never.day_ratio(0.), 1.);
        let dark = super::DayPath { max: -0.1, ..roman };
        assert_eq!(dark.day_ratio(12.), 0.);
    }

    // At night the moon lights the island, far dimmer than the sun and
    // blue, the sun's disc is gone, and the sky is dark but not black.
    #[test]
    fn night_is_moonlit_blue_and_dark() {
        let a = Atmosphere::read(PRESET).unwrap();
        let at = SunMoon::at(0., 0.644, -0.2);
        let night = a.bake(at, [0.2; 3]);
        assert_eq!(night.toward, at.moon);
        let l = night.light;
        assert!(l[2] > l[0] && l[2] < 0.5 && l[2] > 0.05, "moonlight {l:?}");
        assert_eq!(night.disc, [0.; 3]);
        let day = a.bake(SunMoon::at(12., 0.644, -0.2), [0.2; 3]);
        let zenith = night.zenith();
        assert!(zenith[2] < day.zenith()[2] / 3., "night zenith {zenith:?}");
        assert!(zenith.iter().map(|c| c * c).sum::<f32>().sqrt() > 0.06);
    }
}
