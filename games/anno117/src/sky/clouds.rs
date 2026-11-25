// Owns what 117's volumetric clouds are made of: the engine's noise
// volumes and cloud type profiles (data/base/graphics/engine/clouds), an
// ambient preset's weather map and cloud settings, and the shade the
// weather map casts on the sun (GetCloudShadowWeathermap). The pass marches
// what the view sees of them with clouds.wgsl.
// Engine addresses here are Anno117.exe build 24988830 (Steam).
use anno_formats::vfs::Install;
use anyhow::{Context, Result, bail, ensure};
use gpui_editor_kit::gpu::LockAny as _;
use gpui_editor_kit::gpu::{CloudLayer, CloudShade, CloudShadows, ControlMap, SkyMap};
use gpui_editor_kit::texture::{Block, Dds, Format, Texture};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// cWeathermapScale and cCloudLayerHeight, world units: the engine's
/// per-frame constant update (Anno117.exe 0x142474F90) copies the first
/// from the environment renderer (+0x164, 1250 from its constructor) and
/// writes the second as 208, between cWetnessOverride and the debug cursor
/// fields of constants.fxh's cbuffer.
pub const WEATHER_SCALE: f32 = 1250.;
const LAYER_HEIGHT: f32 = 208.;
/// cCloudDomeRadius: the cbClouds fill (Anno117.exe 0x142482620) copies it
/// from the environment renderer's +0x178, 200000 from its constructor
/// (0x142480E70). Larger flattens the layer, so clouds reach nearer the
/// horizon.
pub const DOME_RADIUS: f32 = 200_000.;
/// AmbientRadianceClearSH[0] over the sky's mean radiance: sky.fx's
/// convolution scales the first coefficient by 0.282095 and normalises the
/// sum by 4π over the samples, so it holds 4π × 0.282095 times the mean,
/// though functions_pbr_lighting.fxh calls it the average. The clouds'
/// ambient (CloudInscattering) takes the coefficient as is. cSkyStrength,
/// also in the factor, is taken as 1 (not traced).
pub const SH0_OVER_MEAN: f32 = 4. * std::f32::consts::PI * 0.282_095;
/// The weather map's mip the editor keeps: 1024 texels over 1250 units.
/// The clouds' base is 15 units over the game's highest camera, where a
/// coarser texel shows as a block across several degrees.
const WEATHER_MIP: u32 = 1;

const BASE_NOISE: &str = "data/base/graphics/engine/clouds/base_noise.dds";
const EROSION_NOISE: &str = "data/base/graphics/engine/clouds/erosion_noise.dds";
const CURL_NOISE: &str = "data/base/graphics/engine/clouds/curl_noise.dds";
const CLOUD_TYPES: &str = "data/base/graphics/engine/clouds/cloud_types.dds";

/// A preset's `WeatherMap`, RGBA8, rows as stored: coverage in red, the
/// cloud type (of six) in green, rain in blue.
pub struct Weather {
    pub size: u32,
    /// The mip as the file stores it (BC7, a quarter of RGBA's bytes), and
    /// its RGBA once asked for: a region offers a dozen presets, each with
    /// its own map, and only the one shown is decoded (`rgba`).
    stored: (Option<Block>, Vec<u8>),
    rgba: std::sync::OnceLock<Vec<u8>>,
    /// The shade map for an intensity (in 1/64ths), rebuilt when it moves.
    shade: Mutex<Option<(u8, Arc<ControlMap>)>>,
    /// Coverage (red) halved down to a texel, rows as stored: what a
    /// reflected ray sees of a stretch of the layer (`Clouds::reflect`).
    coverage: std::sync::OnceLock<Vec<(usize, Vec<f32>)>>,
}

impl std::fmt::Debug for Weather {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Weather({0}x{0})", self.size)
    }
}

/// An ambient preset's clouds.
#[derive(Clone, Debug)]
pub struct Clouds {
    pub weather: Arc<Weather>,
    /// `CloudAlbedo`.
    pub albedo: [f32; 3],
    /// `CloudShadowIntensity` and `CloudShadowIntensityNgt`.
    pub shadow_day: f32,
    pub shadow_night: f32,
    /// `WindStrength`: how fast the clouds drift (`Clouds::drift`).
    pub wind: f32,
}

/// How far the clouds have drifted with the wind: the weather map
/// (cWeatherScroll, in map units, wrapped), the base and erosion noise
/// (cCloudNoiseScroll) and the curl noise (cCloudCurlScroll), world units.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Drift {
    pub weather: [f32; 2],
    pub noise: [f32; 3],
    pub curl: [f32; 3],
}

/// The noise's own drift on top of the wind, world units a second, along
/// the wind and up: the environment renderer's +0x174 (0.1) and +0x170
/// (0.75), from its constructor (0x142480E70).
const NOISE_DRIFT: f32 = 0.1;
const CURL_DRIFT: f32 = 0.75;
/// The wind's direction in world X and Z. TODO: the engine turns it by the
/// ambient's +0x40C and the renderer's +0x4C (0x1424835C0), neither traced
/// to data; along X as the sea's (sea.rs).
const WIND_DIRECTION: [f32; 2] = [1., 0.];

impl PartialEq for Clouds {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.weather, &other.weather)
            && self.albedo == other.albedo
            && self.shadow_day == other.shadow_day
            && self.shadow_night == other.shadow_night
            && self.wind == other.wind
    }
}

impl Clouds {
    /// The preset's clouds, its weather map read through `cache` (presets
    /// share maps); `None` without one or when it does not read.
    pub fn read(
        xml: &str,
        install: &Install,
        cache: &mut HashMap<String, Option<Arc<Weather>>>,
    ) -> Option<Self> {
        use anno_formats::xml::{block, tag};
        let path = tag(xml, "WeatherMap")?.trim().replace('\\', "/");
        if path.is_empty() {
            return None;
        }
        let weather = cache
            .entry(path.clone())
            .or_insert_with(|| match Weather::read(install, &path) {
                Ok(w) => Some(Arc::new(w)),
                Err(e) => {
                    eprintln!("117 weather map {path}: {e:#}");
                    None
                }
            })
            .clone()?;
        let number = |name: &str, default: f32| {
            tag(xml, name)
                .and_then(|v| v.trim().parse().ok())
                .unwrap_or(default)
        };
        let albedo = block(xml, "CloudAlbedo")
            .and_then(|b| {
                let v: Option<Vec<f32>> = ["x", "y", "z"]
                    .iter()
                    .map(|a| tag(b, a)?.trim().parse().ok())
                    .collect();
                v.map(|v| [v[0], v[1], v[2]])
            })
            .unwrap_or([1.; 3]);
        Some(Self {
            weather,
            albedo,
            shadow_day: number("CloudShadowIntensity", 0.),
            shadow_night: number("CloudShadowIntensityNgt", 0.),
            wind: number("WindStrength", 1.),
        })
    }

    /// The wind, world units a second in X and Z: twice the preset's
    /// strength (held to 4) along the wind's direction (0x1424835C0).
    fn velocity(&self) -> [f32; 2] {
        let speed = 2. * self.wind.clamp(0., 4.);
        WIND_DIRECTION.map(|d| d * speed)
    }

    /// Where the clouds have drifted `seconds` in, as the environment
    /// renderer's update (0x1424820E0) adds it up frame by frame: the
    /// weather map with the wind, the noise with the wind plus its own
    /// drift along it and up, the erosion noise twice as far in the shader.
    /// Summed here in closed form, so the shadows' fixed velocity keeps up;
    /// the game's gust (±10% of the speed, a period of about 8 s) moves the
    /// clouds back and forth by under a unit and is left out.
    pub fn drift(&self, seconds: f32) -> Drift {
        let t = f64::from(seconds);
        let [vx, vz] = self.velocity().map(f64::from);
        let [dx, dz] = WIND_DIRECTION.map(f64::from);
        let scale = f64::from(WEATHER_SCALE);
        let along = |own: f32| {
            let own = f64::from(own);
            [
                ((own * dx + vx) * t) as f32,
                (own * t) as f32,
                ((own * dz + vz) * t) as f32,
            ]
        };
        Drift {
            weather: [
                (vx * t / scale).rem_euclid(1.) as f32,
                (vz * t / scale).rem_euclid(1.) as f32,
            ],
            noise: along(NOISE_DRIFT),
            curl: along(CURL_DRIFT),
        }
    }

    /// How dark the cloud shadow gets: the night's `CloudShadowIntensityNgt`
    /// eased to the day's over the first and last fifth of the day
    /// (GetShadow). `progress` is the sun's way from sunrise (0) to sunset
    /// (1), cTimeOfDay.z (`DayPath`).
    pub fn shadow_intensity(&self, progress: f32) -> f32 {
        let ease = |x: f32| {
            let x = x.clamp(0., 1.);
            x * x * (3. - 2. * x)
        };
        let day = if (0. ..=1.).contains(&progress) {
            ease(progress / 0.2) * ease((1. - progress) / 0.2)
        } else {
            0.
        };
        (self.shadow_night + (self.shadow_day - self.shadow_night) * day).clamp(0., 1.)
    }

    /// The cloud shadow beyond the cloud shade's square (`CloudGpu::shade`),
    /// as the game takes the weather map past its shadow distance
    /// (GetCloudShadow). functions_shadow.fxh GetShadow's artist version:
    /// the light left is lerp(1 - intensity, 1, 1 - LinearStep(0, 0.5,
    /// coverage)), coverage read where the ray toward the light crosses
    /// cCloudLayerHeight, the intensity `shadow_intensity`'s at `progress`.
    /// `toward` is the light, the sun by day and the moon by night. Ground
    /// heights are taken as sea level for where the ray crosses; islands
    /// are low next to 208 units.
    pub fn shadows(&self, progress: f32, toward: [f32; 3]) -> Option<CloudShadows> {
        let intensity = self.shadow_intensity(progress);
        if intensity <= 0. || toward[1] < 1e-3 {
            return None;
        }
        let map = self.weather.shade(intensity)?;
        let along = LAYER_HEIGHT / toward[1];
        let layer = CloudLayer {
            scale: 1. / WEATHER_SCALE,
            offset: [
                -(0.25 + along * toward[0] / WEATHER_SCALE),
                -(0.25 + along * toward[2] / WEATHER_SCALE),
            ],
            // Moving with the weather map's drift: the game adds it to u
            // and to the flipped v, the map here is stored unflipped.
            velocity: {
                let [vx, vz] = self.velocity();
                [-vx / WEATHER_SCALE, vz / WEATHER_SCALE]
            },
        };
        // Both layers alike and contrast 1: the kit's light left is then
        // 1 - alpha, the shade baked into the map.
        Some(CloudShadows {
            map,
            layers: [layer; 2],
            contrast: 1.,
            power: 1.,
        })
    }
}

/// clouds.wgsl's layer: its base and thickness.
const LAYER_LOWER: f32 = 190.;
const LAYER_THICKNESS: f32 = 180.;

impl Clouds {
    /// `map` with the clouds over it as csAddReflectedClouds puts them in
    /// the cube the game lights and reflects with: seen from the ground,
    /// the weather map drifted by `scroll` (`Drift::weather`), the sky
    /// behind dimmed by what they cover and their light added. `toward` is
    /// the light (sun or moon) and `light` its light on the ground over π
    /// (`Baked::light`).
    ///
    /// The game marches each texel through the noise volumes; this takes
    /// each texel's stretch of the weather map at once: its coverage gives
    /// the share the clouds cover by the rule the cloud shadow uses
    /// (LinearStep(0, 0.5, coverage)), lit by CloudInscattering's phase
    /// octaves at a representative 0.6 of the light getting through to
    /// them, plus a quarter of the sky's mean radiance. Clouds fade out
    /// over the lowest 0.1 of the sky, where the air in front of them
    /// (which this leaves out) swallows them. Estimates throughout; the
    /// view's own clouds are marched in full (clouds.wgsl).
    pub fn reflect(
        &self,
        map: &SkyMap,
        toward: [f32; 3],
        light: [f32; 3],
        scroll: [f32; 2],
    ) -> SkyMap {
        let (w, h) = (map.width, map.height);
        let n = (w * h) as usize;
        // The sky's mean radiance over every direction (AmbientRadiance
        // ClearSH[0]): rows weighted by the solid angle they cover.
        let mut ambient = [0f32; 3];
        let mut weight = 0f32;
        for y in 0..h {
            let d = SkyMap::direction(w, h, 0, y);
            let cos = (1. - d[1] * d[1]).max(0.).sqrt().max(1e-3);
            for x in 0..w {
                let c = map.texels[(y * w + x) as usize];
                for k in 0..3 {
                    ambient[k] += c[k] * cos;
                }
                weight += cos;
            }
        }
        let ambient = ambient.map(|c| SH0_OVER_MEAN * c / weight.max(1e-6));
        let direct = light.map(|c| c * std::f32::consts::PI);
        let hg = |g: f32, nu: f32| {
            (1. - g * g) / (4. * std::f32::consts::PI * (1. + g * g - 2. * g * nu).powf(1.5))
        };
        let pyramid = self.weather.coverage();
        let texel_angle = std::f32::consts::TAU / w as f32;
        let mut texels = map.texels.clone();
        for (i, texel) in texels.iter_mut().enumerate().take(n) {
            let (x, y) = (i as u32 % w, i as u32 / w);
            let d = SkyMap::direction(w, h, x, y);
            if d[1] <= 0.01 {
                continue;
            }
            // Through the middle of the layer from (100, 1, 100).
            let along = (LAYER_LOWER + 0.5 * LAYER_THICKNESS - 1.) / d[1];
            let p = [100. + d[0] * along, 100. + d[2] * along];
            let footprint = along * texel_angle / d[1].max(0.05);
            let coverage = self.weather.coverage_at(pyramid, p, footprint, scroll);
            let cover = (coverage / 0.5).clamp(0., 1.) * (d[1] / 0.1).clamp(0., 1.);
            if cover <= 0. {
                continue;
            }
            let nu = d[0] * toward[0] + d[1] * toward[1] + d[2] * toward[2];
            let through = 0.6f32;
            let sum = 0.5 + 0.5 + 0.06;
            let phase = 2.
                * ((0.5 / sum) * through * hg(0.975, nu)
                    + (0.5 / sum) * through.powf(0.75) * hg(0.9, nu)
                    + (0.06 / sum) * through.powf(0.4));
            let phase = phase.max(0.11 / std::f32::consts::PI);
            for k in 0..3 {
                let cloud = self.albedo[k] * (direct[k] * phase + 0.25 * ambient[k]);
                texel[k] = texel[k] * (1. - cover) + cloud * cover;
            }
        }
        SkyMap {
            width: w,
            height: h,
            texels,
        }
    }
}

impl Weather {
    /// Coverage halved level by level, built once.
    fn coverage(&self) -> &[(usize, Vec<f32>)] {
        self.coverage.get_or_init(|| {
            let n = self.size as usize;
            let rgba = self.rgba();
            let mut levels = vec![(
                n,
                (0..n * n)
                    .map(|i| f32::from(rgba[i * 4]) / 255.)
                    .collect::<Vec<_>>(),
            )];
            while levels.last().unwrap().0 > 1 {
                let (m, prev) = levels.last().unwrap();
                let half = m / 2;
                let next = (0..half * half)
                    .map(|i| {
                        let (x, y) = (i % half * 2, i / half * 2);
                        (prev[y * m + x]
                            + prev[y * m + x + 1]
                            + prev[(y + 1) * m + x]
                            + prev[(y + 1) * m + x + 1])
                            / 4.
                    })
                    .collect();
                levels.push((half, next));
            }
            levels
        })
    }

    /// The weather map's coverage around world X/Z `p`, averaged over about
    /// `footprint` units (ComputeWeatherSamplePosition's mapping: a quarter
    /// map offset, rows flipped, repeating every WEATHER_SCALE units),
    /// with cWeatherScroll's `scroll` added after the flip.
    fn coverage_at(
        &self,
        pyramid: &[(usize, Vec<f32>)],
        p: [f32; 2],
        footprint: f32,
        scroll: [f32; 2],
    ) -> f32 {
        let texel = WEATHER_SCALE / self.size as f32;
        let level = ((footprint / texel).max(1.).log2().round() as usize).min(pyramid.len() - 1);
        let (m, values) = &pyramid[level];
        let m = *m;
        let u = (p[0] / WEATHER_SCALE + 0.25 + scroll[0]).rem_euclid(1.) * m as f32 - 0.5;
        let v = (1. - (p[1] / WEATHER_SCALE + 0.25) + scroll[1]).rem_euclid(1.) * m as f32 - 0.5;
        let (x0, y0) = (u.floor(), v.floor());
        let (fx, fy) = (u - x0, v - y0);
        let at = |x: f32, y: f32| {
            let (x, y) = (
                (x as i64).rem_euclid(m as i64) as usize,
                (y as i64).rem_euclid(m as i64) as usize,
            );
            values[y * m + x]
        };
        let top = at(x0, y0) * (1. - fx) + at(x0 + 1., y0) * fx;
        let bottom = at(x0, y0 + 1.) * (1. - fx) + at(x0 + 1., y0 + 1.) * fx;
        top * (1. - fy) + bottom * fy
    }

    fn read(install: &Install, path: &str) -> Result<Self> {
        // The file names the source image; the game ships its texture as
        // `<name>_0.dds`.
        let stem = path.rsplit_once('.').map_or(path, |(s, _)| s);
        let dds_path = format!("{stem}_0.dds");
        let bytes = install.read(&dds_path).with_context(|| dds_path.clone())?;
        let dds = Dds::parse(&bytes)?;
        let mip = WEATHER_MIP.min(dds.mips - 1);
        let (width, height) = dds.mip_size(mip);
        ensure!(width == height, "weather map is {width}x{height}");
        let (stored, rgba) = match dds.format {
            Format::Block(block) => {
                let at = dds.mip_offset(mip);
                let c = dds.compressed_from(mip, bytes.get(at..).unwrap_or_default())?;
                let top = c.mips.into_iter().next().context("weather map: no mip")?;
                ((Some(block), top), std::sync::OnceLock::new())
            }
            _ => {
                let t = dds.decode(&bytes, mip)?;
                ((None, Vec::new()), std::sync::OnceLock::from(t.rgba))
            }
        };
        Ok(Self {
            size: width,
            stored,
            rgba,
            shade: Mutex::new(None),
            coverage: Default::default(),
        })
    }

    /// A map already decoded, for tests.
    #[cfg(test)]
    fn decoded(size: u32, rgba: Vec<u8>) -> Self {
        Self {
            size,
            stored: (None, Vec::new()),
            rgba: std::sync::OnceLock::from(rgba),
            shade: Mutex::new(None),
            coverage: Default::default(),
        }
    }

    /// The map's RGBA texels, decoded on first use.
    pub fn rgba(&self) -> &[u8] {
        self.rgba.get_or_init(|| {
            let n = self.size;
            let (Some(block), data) = &self.stored else {
                unreachable!("decoded when read")
            };
            Texture::decode(*block, n, n, data).map_or_else(
                |e| {
                    eprintln!("117 weather map: {e:#}");
                    vec![0; (n * n * 4) as usize]
                },
                |t| t.rgba,
            )
        })
    }

    /// The shade as the kit's map takes it: alpha = intensity ×
    /// LinearStep(0, 0.5, coverage), rows flipped (the game samples at
    /// 1 - v) so world Z maps on with a positive scale.
    fn shade(&self, intensity: f32) -> Option<Arc<ControlMap>> {
        let key = (intensity.clamp(0., 1.) * 64.).round() as u8;
        let mut shade = self.shade.lock_any();
        if let Some((k, map)) = shade.as_ref()
            && *k == key
        {
            return Some(map.clone());
        }
        let n = self.size as usize;
        let strength = f32::from(key) / 64.;
        let mut texels = vec![0u8; n * n * 4];
        let rgba = self.rgba();
        for y in 0..n {
            let row = &rgba[(n - 1 - y) * n * 4..][..n * 4];
            for x in 0..n {
                let coverage = f32::from(row[x * 4]) / 255.;
                let a = strength * (coverage / 0.5).clamp(0., 1.);
                texels[(y * n + x) * 4 + 3] = (a * 255.).round() as u8;
            }
        }
        let map = Arc::new(ControlMap::new(n, n, texels).ok()?);
        *shade = Some((key, map.clone()));
        Some(map)
    }
}

/// One channel of a cube of texels per mip, largest first.
pub struct Volume {
    pub side: u32,
    pub levels: Vec<Vec<u8>>,
}

/// curl_noise.dds as the GPU takes it: RGBA8 snorm per texel per mip,
/// each channel over `scale`. The file is RGBA32F, which wgpu cannot
/// filter without a feature; 8 bits of a 7-unit offset are ample.
pub struct Curl {
    pub side: u32,
    pub scale: f32,
    pub levels: Vec<Vec<u8>>,
}

/// The engine's cloud textures, decoded for the GPU.
pub struct Volumes {
    pub base: Volume,
    pub erosion: Volume,
    pub curl: Curl,
    /// cloud_types.dds: 256 heights by six types, the density profile of
    /// each type up the layer.
    pub types: Vec<u8>,
}

/// The files the volumes decode from, read where an install is at hand
/// and decoded later off the UI thread.
pub struct VolumeFiles {
    base: Vec<u8>,
    erosion: Vec<u8>,
    curl: Vec<u8>,
    types: Vec<u8>,
}

impl VolumeFiles {
    pub fn read(install: &Install) -> Option<Self> {
        Some(Self {
            base: install.read(BASE_NOISE).ok()?,
            erosion: install.read(EROSION_NOISE).ok()?,
            curl: install.read(CURL_NOISE).ok()?,
            types: install.read(CLOUD_TYPES).ok()?,
        })
    }

    pub fn decode(&self) -> Result<Volumes> {
        // cloud_types.dds is 8-bit luminance without a DX10 header.
        let types = self
            .types
            .get(128..128 + 256 * 6)
            .context("cloud_types.dds is short")?
            .to_vec();
        Ok(Volumes {
            base: volume(&self.base)?,
            erosion: volume(&self.erosion)?,
            curl: curl(&self.curl).context("curl_noise.dds")?,
            types,
        })
    }
}

/// An RGBA32F volume DDS (DX10 header, DXGI format 2) to snorm bytes.
fn curl(bytes: &[u8]) -> Result<Curl> {
    let u32_at = |at: usize| u32::from_le_bytes(bytes[at..at + 4].try_into().expect("four"));
    ensure!(bytes.len() > 148 && bytes.starts_with(b"DDS "), "not a DDS");
    ensure!(&bytes[84..88] == b"DX10", "no DX10 header");
    let side = u32_at(16);
    let mips = u32_at(28).max(1);
    if u32_at(128) != 2 || side != u32_at(24) || side != u32_at(12) {
        bail!("not a cubic RGBA32F volume");
    }
    let floats: Vec<f32> = bytes[148..]
        .as_chunks::<4>()
        .0
        .iter()
        .map(|b| f32::from_le_bytes(*b))
        .collect();
    let mut at = 0;
    let mut levels = Vec::new();
    for mip in 0..mips {
        let n = (side >> mip) as usize;
        if n == 0 {
            break;
        }
        let len = n * n * n * 4;
        levels.push(floats.get(at..at + len).context("volume is short")?);
        at += len;
    }
    let scale = (levels.iter().flat_map(|l| l.iter()))
        .fold(0f32, |m, v| m.max(v.abs()))
        .max(1e-6);
    let levels = levels
        .iter()
        .map(|l| {
            l.iter()
                .map(|v| ((v / scale).clamp(-1., 1.) * 127.).round() as i8 as u8)
                .collect()
        })
        .collect();
    Ok(Curl {
        side,
        scale,
        levels,
    })
}

/// A BC4 volume DDS (DX10 header): every slice of each mip in turn, down to
/// a texel; far clouds read from the small mips.
fn volume(bytes: &[u8]) -> Result<Volume> {
    let u32_at = |at: usize| u32::from_le_bytes(bytes[at..at + 4].try_into().expect("four"));
    ensure!(bytes.len() > 148 && bytes.starts_with(b"DDS "), "not a DDS");
    ensure!(&bytes[84..88] == b"DX10", "no DX10 header");
    let side = u32_at(16);
    let depth = u32_at(24);
    let mips = u32_at(28).max(1);
    if !(79..=81).contains(&u32_at(128)) || side != depth || side != u32_at(12) {
        bail!("not a cubic BC4 volume");
    }
    let mut at = 148;
    let mut levels = Vec::new();
    for mip in 0..mips {
        let n = side >> mip;
        if n == 0 {
            break;
        }
        let slice = Block::Bc4.size(n, n);
        let mut texels = Vec::with_capacity((n * n * n) as usize);
        for _ in 0..n {
            let data = bytes.get(at..at + slice).context("volume is short")?;
            let t = Texture::decode(Block::Bc4, n, n, data)?;
            texels.extend(t.rgba.as_chunks::<4>().0.iter().map(|p| p[0]));
            at += slice;
        }
        levels.push(texels);
    }
    Ok(Volume { side, levels })
}

/// The cloud map is the view's size over this, each way: the game marches
/// its clouds at a quarter of the screen too. Marched again when the view,
/// sun or preset moves, a quarter of its texels at a time (clouds.wgsl
/// fs_clouds).
pub const MAP_DIVISOR: u32 = 4;

/// The uniform as clouds.wgsl's `Cloud`.
pub type Uniform = [[f32; 4]; 19];

/// The cloud shade (fs_cloud_shade): texels a side, and the least and most
/// world units its square spans. It follows where the view looks, sized to
/// some three times the eye's distance from there, in powers of two (4
/// units a texel far out, down to half a unit up close; the clouds'
/// erosion noise repeats every 106 units over 32 texels). The game marches the same per shadow
/// cascade texel, out to its shadow distance, and takes the weather map
/// beyond: here `Clouds::shadows` beyond the square. 0.67 ms a draw at
/// 1024² on an RTX 4080 SUPER (gpu_bench), drawn again only as the clouds
/// drift half a texel or the view leaves its square; a drift's redraw is
/// spread over SHADE_SLICES frames.
const SHADE_TEXELS: u32 = 1024;
/// Frames a drift's redraw takes, a band of rows each, into the target not
/// shown, instead of a 0.67 ms spike: at four a band over continental_overview's
/// cloud bank still took 0.30 ms. The shade shows the drift a few frames
/// late, while it moves under a fifth of a texel.
const SHADE_SLICES: u32 = 8;
const SHADE_MIN: f32 = 512.;
const SHADE_MAX: f32 = 4096.;

/// The cloud shade's two targets, the one shown and the one drawn into,
/// and what the last draw was for.
struct Shade {
    views: [wgpu::TextureView; 2],
    /// The target `shown` reads.
    front: usize,
    buffer: wgpu::Buffer,
    drawn: Option<Uniform>,
    shown: Option<CloudShade>,
    /// A draw into the back target under way: its square and the slices
    /// done.
    pending: Option<([f32; 2], f32, u32)>,
    /// Draws so far, for CloudShade::version.
    draws: u64,
}

/// The clouds' GPU side: the volumes, the shown weather map, the uniform,
/// and the map of what the view sees that fs_backdrop reads.
pub struct CloudGpu {
    base: wgpu::TextureView,
    erosion: wgpu::TextureView,
    curl: wgpu::TextureView,
    /// What the curl's snorm texels are over (`Curl::scale`).
    curl_scale: f32,
    types: wgpu::TextureView,
    wrap: wgpu::Sampler,
    clamp: wgpu::Sampler,
    buffer: wgpu::Buffer,
    /// The shown weather map, uploaded, and the groups over it.
    weather: Option<WeatherGpu>,
    shade: Shade,
    /// The map fs_backdrop and fs_haze_up read, and the one before,
    /// which the next march reprojects and blends into.
    now: Map,
    before: Map,
    /// What a march marched (fs_clouds_march): a quarter of the map at its
    /// top left, all of it when there is no history.
    fresh: Map,
    /// fs_clouds' group over `fresh` and `before` (`resolve_layout`).
    resolve: Option<wgpu::BindGroup>,
    map_size: [u32; 2],
    /// What the map was marched for: the uniform, the sky's uniform and
    /// the weather map. A march runs only when one moves, or while it
    /// settles.
    marched: Option<(Uniform, Vec<[f32; 4]>, usize)>,
    /// Marches since the view (not the drift) last moved, and in all.
    still: u32,
    marches: u32,
    /// The last march's view (clip to world), eye and weather map scroll,
    /// for reprojecting, and the light, preset and weather it was lit by:
    /// a history lit otherwise is dropped, not blended in.
    last_view: Option<LastView>,
    last_light: Option<Light>,
}

/// A weather map on the GPU and the groups over it, the march's and the
/// shade's (with its own uniform). `source` is held, not just its address:
/// a map dropped and another made in its place would compare equal.
struct WeatherGpu {
    source: Arc<Weather>,
    view: wgpu::TextureView,
    /// Over the maps as they were when made: `None` once `fit` remade them.
    groups: Option<[wgpu::BindGroup; 2]>,
}

/// A march's view (clip to world), eye and weather map scroll.
type LastView = ([[f32; 4]; 4], [f32; 4], [f32; 2]);

/// What lights a march: clouds.wgsl's light, ambient and albedo rows, the
/// sky's uniform and the weather map.
type Light = ([[f32; 4]; 3], Vec<[f32; 4]>, usize);

/// One cloud map: per texel the clouds' light and transmittance (`color`),
/// and how far along the ray they lie and how many marches it blends
/// (`info`, red and green), for the next march to reproject it.
struct Map {
    color: wgpu::Texture,
    color_view: wgpu::TextureView,
    info: wgpu::Texture,
    info_view: wgpu::TextureView,
}

impl Map {
    fn new(device: &wgpu::Device, size: [u32; 2], info: wgpu::TextureFormat) -> Self {
        let (color, color_view) = map_texture(device, size, MAP_FORMAT);
        let (info, info_view) = map_texture(device, size, info);
        Self {
            color,
            color_view,
            info,
            info_view,
        }
    }

    fn copy_to(&self, encoder: &mut wgpu::CommandEncoder, to: &Map, size: [u32; 2]) {
        let extent = wgpu::Extent3d {
            width: size[0],
            height: size[1],
            depth_or_array_layers: 1,
        };
        encoder.copy_texture_to_texture(
            self.color.as_image_copy(),
            to.color.as_image_copy(),
            extent,
        );
        encoder.copy_texture_to_texture(self.info.as_image_copy(), to.info.as_image_copy(), extent);
    }
}

/// Marches a still view keeps marching for after it stops: each jitters
/// its samples anew and blends into the ones before (clouds.wgsl
/// fs_clouds), as the game accumulates its clouds over frames, reprojected
/// as the view moves. Sixteen of each texel, four of the map's at a time.
const SETTLE: u32 = 64;

/// Group 2 of the march: clouds.wgsl's bindings.
pub fn layout(device: &wgpu::Device) -> wgpu::BindGroupLayout {
    let texture = |binding, view_dimension| wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::FRAGMENT,
        ty: wgpu::BindingType::Texture {
            sample_type: wgpu::TextureSampleType::Float { filterable: true },
            view_dimension,
            multisampled: false,
        },
        count: None,
    };
    let sampler = |binding| wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::FRAGMENT,
        ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
        count: None,
    };
    let (d2, d3) = (
        wgpu::TextureViewDimension::D2,
        wgpu::TextureViewDimension::D3,
    );
    device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("117 clouds"),
        entries: &[
            wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            },
            texture(1, d3),
            texture(2, d3),
            texture(3, d2),
            texture(4, d2),
            sampler(5),
            sampler(6),
            texture(7, d3),
        ],
    })
}

/// Group 2 of fs_clouds, which fills the map from the march: its uniform,
/// the last map (color and info) and the march's (color and info).
pub fn resolve_layout(device: &wgpu::Device) -> wgpu::BindGroupLayout {
    let texture = |binding| wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::FRAGMENT,
        ty: wgpu::BindingType::Texture {
            sample_type: wgpu::TextureSampleType::Float { filterable: true },
            view_dimension: wgpu::TextureViewDimension::D2,
            multisampled: false,
        },
        count: None,
    };
    device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("117 cloud resolve"),
        entries: &[
            wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            },
            texture(8),
            texture(9),
            texture(10),
            texture(11),
        ],
    })
}

#[allow(clippy::too_many_arguments)]
fn upload(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    label: &str,
    size: [u32; 3],
    dimension: wgpu::TextureDimension,
    format: wgpu::TextureFormat,
    texel: u32,
    levels: &[&[u8]],
) -> wgpu::TextureView {
    let texture = device.create_texture(&wgpu::TextureDescriptor {
        label: Some(label),
        size: wgpu::Extent3d {
            width: size[0],
            height: size[1],
            depth_or_array_layers: size[2],
        },
        mip_level_count: levels.len() as u32,
        sample_count: 1,
        dimension,
        format,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    for (mip, data) in levels.iter().enumerate() {
        let at = |v: u32| (v >> mip).max(1);
        let depth = if dimension == wgpu::TextureDimension::D3 {
            at(size[2])
        } else {
            size[2]
        };
        let extent = wgpu::Extent3d {
            width: at(size[0]),
            height: at(size[1]),
            depth_or_array_layers: depth,
        };
        queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &texture,
                mip_level: mip as u32,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            data,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(extent.width * texel),
                rows_per_image: Some(extent.height),
            },
            extent,
        );
    }
    texture.create_view(&Default::default())
}

impl CloudGpu {
    pub fn new(device: &wgpu::Device, queue: &wgpu::Queue, v: &Volumes) -> Self {
        let d3 = wgpu::TextureDimension::D3;
        let r8 = wgpu::TextureFormat::R8Unorm;
        let volume = |label, v: &Volume| {
            let levels: Vec<&[u8]> = v.levels.iter().map(Vec::as_slice).collect();
            upload(device, queue, label, [v.side; 3], d3, r8, 1, &levels)
        };
        let base = volume("117 cloud base noise", &v.base);
        let erosion = volume("117 cloud erosion noise", &v.erosion);
        let curl_levels: Vec<&[u8]> = v.curl.levels.iter().map(Vec::as_slice).collect();
        let curl = upload(
            device,
            queue,
            "117 cloud curl noise",
            [v.curl.side; 3],
            d3,
            wgpu::TextureFormat::Rgba8Snorm,
            4,
            &curl_levels,
        );
        let types = upload(
            device,
            queue,
            "117 cloud types",
            [256, 6, 1],
            wgpu::TextureDimension::D2,
            r8,
            1,
            &[&v.types],
        );
        let sampler = |label, mode| {
            device.create_sampler(&wgpu::SamplerDescriptor {
                label: Some(label),
                address_mode_u: mode,
                address_mode_v: mode,
                address_mode_w: mode,
                mag_filter: wgpu::FilterMode::Linear,
                min_filter: wgpu::FilterMode::Linear,
                mipmap_filter: wgpu::MipmapFilterMode::Linear,
                ..Default::default()
            })
        };
        let map_size = [1, 1];
        let [now, before, fresh] = maps(device, map_size);
        let shade = || {
            device
                .create_texture(&wgpu::TextureDescriptor {
                    label: Some("117 cloud shade"),
                    size: wgpu::Extent3d {
                        width: SHADE_TEXELS,
                        height: SHADE_TEXELS,
                        depth_or_array_layers: 1,
                    },
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: wgpu::TextureDimension::D2,
                    format: SHADE_FORMAT,
                    usage: wgpu::TextureUsages::TEXTURE_BINDING
                        | wgpu::TextureUsages::RENDER_ATTACHMENT,
                    view_formats: &[],
                })
                .create_view(&Default::default())
        };
        Self {
            base,
            erosion,
            curl,
            curl_scale: v.curl.scale,
            types,
            wrap: sampler("117 clouds wrap", wgpu::AddressMode::Repeat),
            clamp: sampler("117 clouds clamp", wgpu::AddressMode::ClampToEdge),
            buffer: device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("117 clouds"),
                size: std::mem::size_of::<Uniform>() as u64,
                usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            }),
            weather: None,
            shade: Shade {
                views: [shade(), shade()],
                front: 0,
                buffer: device.create_buffer(&wgpu::BufferDescriptor {
                    label: Some("117 cloud shade"),
                    size: std::mem::size_of::<Uniform>() as u64,
                    usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                    mapped_at_creation: false,
                }),
                drawn: None,
                shown: None,
                pending: None,
                draws: 0,
            },
            now,
            before,
            fresh,
            resolve: None,
            map_size,
            marched: None,
            still: 0,
            marches: 0,
            last_view: None,
            last_light: None,
        }
    }

    /// The map of what the view sees: light and transmittance.
    pub fn map(&self) -> &wgpu::TextureView {
        &self.now.color_view
    }

    /// Whether a still view's map is still settling, so frames should keep
    /// coming.
    pub fn settling(&self) -> bool {
        self.still < SETTLE
    }

    /// Whether no draw of the shade is under way.
    pub fn shade_drawn(&self) -> bool {
        self.shade.pending.is_none()
    }

    /// Sizes the map for a view `size` pixels across; true when it was
    /// made anew, so groups over it must be too.
    pub fn fit(&mut self, device: &wgpu::Device, size: [u32; 2]) -> bool {
        let size = size.map(|v| v.div_ceil(MAP_DIVISOR).max(1));
        if size == self.map_size {
            return false;
        }
        [self.now, self.before, self.fresh] = maps(device, size);
        self.map_size = size;
        self.marched = None;
        self.last_view = None;
        self.resolve = None;
        true
    }

    /// Uploads `clouds`' weather map when it is not the one shown, and
    /// makes the groups the march and the shade draw with (`layout`, group
    /// 2) and the one fs_clouds fills the map with (`resolve_layout`) when
    /// they are missing. Before either: the shade draws with them whether
    /// or not the clouds are marched.
    pub fn bind(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        [layout, resolve_layout]: [&wgpu::BindGroupLayout; 2],
        clouds: &Clouds,
    ) {
        if self.resolve.is_none() {
            let entry = |binding, view| wgpu::BindGroupEntry {
                binding,
                resource: wgpu::BindingResource::TextureView(view),
            };
            self.resolve = Some(device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("117 cloud resolve"),
                layout: resolve_layout,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: self.buffer.as_entire_binding(),
                    },
                    entry(8, &self.before.color_view),
                    entry(9, &self.before.info_view),
                    entry(10, &self.fresh.color_view),
                    entry(11, &self.fresh.info_view),
                ],
            }));
        }
        if self
            .weather
            .as_ref()
            .is_none_or(|w| !Arc::ptr_eq(&w.source, &clouds.weather))
        {
            let w = &clouds.weather;
            let view = upload(
                device,
                queue,
                "117 weather map",
                [w.size, w.size, 1],
                wgpu::TextureDimension::D2,
                wgpu::TextureFormat::Rgba8Unorm,
                4,
                &[w.rgba()],
            );
            self.weather = Some(WeatherGpu {
                source: w.clone(),
                view,
                groups: None,
            });
            self.marched = None;
            self.shade.drawn = None;
        }
        let weather = self.weather.as_mut().expect("uploaded above");
        if weather.groups.is_some() {
            return;
        }
        let view = wgpu::BindingResource::TextureView;
        let group = |buffer: &wgpu::Buffer| {
            device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("117 clouds"),
                layout,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: buffer.as_entire_binding(),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: view(&self.base),
                    },
                    wgpu::BindGroupEntry {
                        binding: 2,
                        resource: view(&self.erosion),
                    },
                    wgpu::BindGroupEntry {
                        binding: 3,
                        resource: view(&self.types),
                    },
                    wgpu::BindGroupEntry {
                        binding: 4,
                        resource: view(&weather.view),
                    },
                    wgpu::BindGroupEntry {
                        binding: 5,
                        resource: wgpu::BindingResource::Sampler(&self.wrap),
                    },
                    wgpu::BindGroupEntry {
                        binding: 6,
                        resource: wgpu::BindingResource::Sampler(&self.clamp),
                    },
                    wgpu::BindGroupEntry {
                        binding: 7,
                        resource: view(&self.curl),
                    },
                ],
            })
        };
        weather.groups = Some([group(&self.buffer), group(&self.shade.buffer)]);
    }

    /// Marches the map for `clouds` (`bind` first) seen with `uniform`
    /// under the sky's `air` when any of them moved since the last march:
    /// a draw of `march` (fs_clouds_march) over a quarter of the map's
    /// texels, or all of them without a history, and one of `resolve`
    /// (fs_clouds) over the map, with `groups[1]` (the sky's group without
    /// the map) bound.
    pub fn march(
        &mut self,
        frame: &mut gpui_editor_kit::gpu::Prepare<'_>,
        [march, resolve]: [&wgpu::RenderPipeline; 2],
        groups: [&wgpu::BindGroup; 2],
        clouds: &Clouds,
        uniform: Uniform,
        air: &[[f32; 4]],
    ) {
        let queue = frame.queue;
        // The weather map's address: `bind` holds it, so no other map can
        // take it while the march compares against it.
        let key = Arc::as_ptr(&clouds.weather) as usize;
        let mut uniform = uniform;
        uniform[8][2] = self.map_size[0] as f32;
        uniform[8][3] = self.map_size[1] as f32;
        uniform[9][0] = self.curl_scale;
        let wanted = (uniform, air.to_vec(), key);
        // The view: everything but the drift (row 8's xy, rows 10 and 11).
        let view = |(u, air, key): &(Uniform, Vec<[f32; 4]>, usize)| {
            (u[..8].to_vec(), u[8][2..].to_vec(), air.clone(), *key)
        };
        let held = self
            .marched
            .as_ref()
            .is_some_and(|m| view(m) == view(&wanted));
        if held && self.marched.as_ref() == Some(&wanted) && !self.settling() {
            return;
        }
        self.still = if held { self.still + 1 } else { 0 };
        // The jitter turns with each march; wrapped so f32 keeps it exact.
        self.marches = (self.marches + 1) % 4096;
        uniform[9][1] = self.marches as f32;
        let eye = uniform[0];
        let unclip = [uniform[4], uniform[5], uniform[6], uniform[7]];
        let light: Light = ([uniform[1], uniform[2], uniform[3]], air.to_vec(), key);
        if self.last_light.as_ref() != Some(&light) {
            self.last_view = None;
            self.last_light = Some(light);
        }
        // A history begun afresh (a new light, size or preset) takes the
        // jitter from its start: else the map a still view settles on
        // turned with how many marches came before, which varied with how
        // soon the tables and volumes came up.
        let whole = self.last_view.is_none();
        if whole {
            self.marches = 0;
            uniform[9][1] = 0.;
            uniform[9][3] = 1.;
        }
        let scroll = [uniform[8][0], uniform[8][1]];
        if let Some((last, last_eye, last_scroll)) = self.last_view
            && let Some(clip) = gpui_editor_kit::math::inverse(&bytemuck::cast(last))
        {
            let clip: [[f32; 4]; 4] = bytemuck::cast(clip);
            uniform[12..16].copy_from_slice(&clip);
            uniform[16] = [last_eye[0], last_eye[1], last_eye[2], 1.];
            // The weather map's scroll in world units (clouds.wgsl's
            // mapping: u adds it, v adds it after the flip), wrapped.
            let [du, dv] = [0, 1].map(|k| {
                let d = scroll[k] - last_scroll[k];
                (d - d.round()) * WEATHER_SCALE
            });
            uniform[18] = [-du, 0., dv, 0.];
        }
        self.last_view = Some((unclip, eye, scroll));
        queue.write_buffer(&self.buffer, 0, bytemuck::cast_slice(&uniform));
        let (Some([group, _]), Some(resolve_group)) = (
            self.weather.as_ref().and_then(|w| w.groups.as_ref()),
            self.resolve.as_ref(),
        ) else {
            return;
        };
        let [w, h] = if whole {
            self.map_size
        } else {
            self.map_size.map(|v| v.div_ceil(2))
        };
        let draws = [
            (
                "clouds",
                "117 cloud map",
                &self.fresh,
                march,
                group,
                Some([w, h]),
            ),
            (
                "cloud resolve",
                "117 cloud resolve",
                &self.now,
                resolve,
                resolve_group,
                None,
            ),
        ];
        for (timer, label, target, pipeline, group, viewport) in draws {
            let (encoder, timestamp_writes) = frame.timed(timer);
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some(label),
                color_attachments: &[&target.color_view, &target.info_view].map(|view| {
                    Some(wgpu::RenderPassColorAttachment {
                        view,
                        depth_slice: None,
                        resolve_target: None,
                        ops: wgpu::Operations {
                            // Every texel the passes read is drawn.
                            load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                            store: wgpu::StoreOp::Store,
                        },
                    })
                }),
                depth_stencil_attachment: None,
                timestamp_writes,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            if let Some([w, h]) = viewport {
                pass.set_viewport(0., 0., w as f32, h as f32, 0., 1.);
            }
            pass.set_pipeline(pipeline);
            pass.set_bind_group(0, groups[0], &[]);
            pass.set_bind_group(1, groups[1], &[]);
            pass.set_bind_group(2, group, &[]);
            pass.draw(0..3, 0..1);
        }
        self.now.copy_to(frame.encoder, &self.before, self.map_size);
        self.marched = Some(wanted);
    }
}

impl CloudGpu {
    /// Draws the cloud shade under the clouds `uniform` (as `march` took
    /// it, `bind` first) describes, `intensity` dark (`Clouds::shadow_intensity`), over
    /// a square around where the view looks, sized for the uniform's eye:
    /// one draw of `pipeline` with
    /// `groups` (as the march's) bound, when the light, weather or square
    /// moved, or (over SHADE_SLICES frames) the clouds drifted half a
    /// texel. `None` when there is no
    /// shade to draw: no weather map yet, or no shadow at this hour.
    pub fn shade(
        &mut self,
        frame: &mut gpui_editor_kit::gpu::Prepare<'_>,
        pipeline: &wgpu::RenderPipeline,
        groups: [&wgpu::BindGroup; 2],
        uniform: Uniform,
        intensity: f32,
    ) -> Option<CloudShade> {
        let (focus, eye) = (frame.focus, uniform[0]);
        let Some([_, group]) = self.weather.as_ref().and_then(|w| w.groups.as_ref()) else {
            return None;
        };
        if intensity <= 0. || uniform[1][1] < 1e-3 {
            self.shade.shown = None;
            self.shade.pending = None;
            return None;
        }
        let reach =
            ((eye[0] - focus[0]).powi(2) + eye[1].powi(2) + (eye[2] - focus[1]).powi(2)).sqrt();
        let size = (3. * reach)
            .clamp(SHADE_MIN, SHADE_MAX)
            .log2()
            .ceil()
            .exp2();
        // Its corner on a grid an eighth of it apart: whole texels, so the
        // shade does not swim as the view pans, and redrawn only as the
        // view leaves that eighth.
        let snap = size / 8.;
        let origin = focus.map(|v| (v / snap).round() * snap - 0.5 * size);
        let mut uniform = uniform;
        // The curl pushes the shade's clouds as the march's (row 9's x):
        // without it the shadows lay where the clouds would without curl.
        uniform[9][0] = self.curl_scale;
        uniform[9][2] = SHADE_TEXELS as f32;
        uniform[17] = [origin[0], origin[1], size, intensity];
        // Redrawn when anything but the eye, the view and the drift moved,
        // or the drift by half a texel; the dome under the eye bends the
        // layer by under a unit across the square.
        let texel = size / SHADE_TEXELS as f32;
        let held = |u: &Uniform| (u[1], u[3], u[9][2], u[17]);
        let drift = |u: &Uniform| {
            let w = WEATHER_SCALE;
            [
                u[8][0] * w,
                u[8][1] * w,
                u[10][0],
                u[10][1],
                u[10][2],
                u[11][0],
                u[11][1],
                u[11][2],
            ]
        };
        // Drawn again when anything but the eye, the view and the drift
        // moved, at once; or when the drift moved half a texel, a slice a
        // frame. The dome under the eye bends the layer by under a unit
        // across the square. A draw under way is finished before the next
        // starts: it is compared against, and the drift moves a tenth of a
        // texel over its frames.
        let (at_once, drifted) = match &self.shade.drawn {
            None => (true, false),
            Some(was) => {
                // The weather map's drift wraps at a map.
                let far = drift(was).iter().zip(drift(&uniform)).any(|(a, b)| {
                    let d = (a - b).abs();
                    d.min((d - WEATHER_SCALE).abs()) > 0.5 * texel
                });
                (held(was) != held(&uniform), far)
            }
        };
        let shown = self.shade.shown.is_some();
        if at_once || (drifted && self.shade.pending.is_none()) {
            frame
                .queue
                .write_buffer(&self.shade.buffer, 0, bytemuck::cast_slice(&uniform));
            self.shade.drawn = Some(uniform);
            // Nothing shown yet, or what is shown is for another light or
            // square: drawn whole this frame.
            let slices = if at_once || !shown { SHADE_SLICES } else { 0 };
            self.shade.pending = Some((origin, size, slices));
        }
        if let Some((origin, size, done)) = self.shade.pending {
            let back = 1 - self.shade.front;
            // SHADE_SLICES done asks for every slice at once.
            let (from, to) = if done == SHADE_SLICES {
                (0, SHADE_SLICES)
            } else {
                (done, done + 1)
            };
            let rows = SHADE_TEXELS / SHADE_SLICES;
            let (encoder, timestamp_writes) = frame.timed("cloud shade");
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("117 cloud shade"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &self.shade.views[back],
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        // Every texel is drawn by some slice.
                        load: wgpu::LoadOp::Load,
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            pass.set_pipeline(pipeline);
            pass.set_bind_group(0, groups[0], &[]);
            pass.set_bind_group(1, groups[1], &[]);
            pass.set_bind_group(2, group, &[]);
            pass.set_scissor_rect(0, from * rows, SHADE_TEXELS, (to - from) * rows);
            pass.draw(0..3, 0..1);
            drop(pass);
            if to < SHADE_SLICES {
                self.shade.pending = Some((origin, size, to));
            } else {
                self.shade.pending = None;
                self.shade.front = back;
                self.shade.draws += 1;
                self.shade.shown = Some(CloudShade {
                    view: self.shade.views[back].clone(),
                    origin,
                    size,
                    version: self.shade.draws,
                });
            }
        }
        self.shade.shown.clone()
    }
}

/// The cloud shade's texels: the light left, which bilinear filtering
/// blurs as the game blurs its transmittance map (csBlurTransmittanceMap).
pub const SHADE_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::R8Unorm;

/// The map's light and transmittance, and its info (where the clouds lie,
/// marches blended).
pub const MAP_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba16Float;
/// The march's info: only how far its clouds lie.
pub const FRESH_INFO_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::R16Float;

/// The map shown, the one before and the march's, for a map `size` texels.
fn maps(device: &wgpu::Device, size: [u32; 2]) -> [Map; 3] {
    [
        Map::new(device, size, MAP_FORMAT),
        Map::new(device, size, MAP_FORMAT),
        Map::new(device, size, FRESH_INFO_FORMAT),
    ]
}

fn map_texture(
    device: &wgpu::Device,
    size: [u32; 2],
    format: wgpu::TextureFormat,
) -> (wgpu::Texture, wgpu::TextureView) {
    let texture = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("117 cloud map"),
        size: wgpu::Extent3d {
            width: size[0],
            height: size[1],
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::TEXTURE_BINDING
            | wgpu::TextureUsages::RENDER_ATTACHMENT
            | wgpu::TextureUsages::COPY_SRC
            | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    let view = texture.create_view(&Default::default());
    (texture, view)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn install() -> Option<Install> {
        let dir = anno_formats::vfs::install_dir("ANNO117_DIR")?;
        Install::open(dir, crate::ARCHIVE_FAMILIES).ok()
    }

    // Needs the game: the noise volumes decode to their full mip chains
    // and carry noise, not a flat fill.
    #[test]
    fn the_engine_cloud_volumes_decode() {
        let Some(install) = install() else {
            eprintln!("skipping: ANNO117_DIR unset");
            return;
        };
        let v = VolumeFiles::read(&install).unwrap().decode().unwrap();
        assert_eq!((v.base.side, v.base.levels.len()), (128, 8));
        assert_eq!((v.erosion.side, v.erosion.levels.len()), (32, 6));
        assert_eq!(v.base.levels[0].len(), 128 * 128 * 128);
        let (lo, hi) = (v.base.levels[0].iter()).fold((255, 0), |(l, h), &x| (l.min(x), h.max(x)));
        assert!(hi - lo > 60, "base noise spans {lo}..{hi}");
        assert_eq!(v.types.len(), 256 * 6);
        assert_eq!((v.curl.side, v.curl.levels.len()), (32, 5));
        assert_eq!(v.curl.levels[0].len(), 32 * 32 * 32 * 4);
        // Signed: the curl pushes both ways.
        let signed = v.curl.levels[0].iter().map(|&b| b as i8);
        let (lo, hi) = signed.fold((0i8, 0i8), |(l, h), x| (l.min(x), h.max(x)));
        assert!(lo < -60 && hi > 60, "curl spans {lo}..{hi}");
    }

    // The shade darkens where the map has cover and leaves clear sky
    // alone, as strong as the preset says.
    #[test]
    fn the_shade_follows_coverage() {
        let mut rgba = vec![0u8; 4 * 4 * 4];
        rgba[0] = 255; // full cover at the first texel of the first row
        let weather = Arc::new(Weather::decoded(4, rgba));
        let clouds = Clouds {
            weather,
            albedo: [1.; 3],
            shadow_day: 0.8,
            shadow_night: 0.,
            wind: 1.111,
        };
        let shadows = clouds.shadows(0.5, [0., 1., 0.]).expect("by day");
        let map = shadows.map.clone();
        // Rows flipped: the first stored row is the last.
        let texels = map.to_vec();
        assert!(texels[(3 * 4) * 4 + 3].abs_diff(204) <= 1);
        assert_eq!(texels[3], 0);
        assert!(
            clouds.shadows(-0.5, [0., 1., 0.]).is_none(),
            "none by night"
        );
    }

    // The wind carries the weather map and the shadows it casts alike, and
    // the noise further and upward, the curl most.
    #[test]
    fn the_wind_drifts_clouds_and_their_shadows_together() {
        let clouds = Clouds {
            weather: Arc::new(Weather::decoded(4, vec![255; 4 * 4 * 4])),
            albedo: [1.; 3],
            shadow_day: 0.8,
            shadow_night: 0.,
            wind: 1.111,
        };
        let t = 100.;
        let drift = clouds.drift(t);
        // 2 × 1.111 units a second along X over a 1250-unit map.
        assert!((drift.weather[0] - 2. * 1.111 * t / WEATHER_SCALE).abs() < 1e-5);
        assert_eq!(drift.weather[1], 0.);
        let layer = clouds.shadows(0.5, [0., 1., 0.]).expect("by day").layers[0];
        // The shadow map is sampled at world × scale − velocity × t: the
        // same shift as the shader's + cWeatherScroll on u.
        assert!((-layer.velocity[0] * t - drift.weather[0]).abs() < 1e-5);
        assert_eq!(layer.velocity[1], 0.);
        assert!(drift.curl[1] > drift.noise[1] && drift.noise[1] > 0.);
        assert!(drift.noise[0] > 2. * 1.111 * t && drift.curl[0] > drift.noise[0]);
        assert_eq!(clouds.drift(0.), Drift::default());
        let calm = Clouds {
            wind: 0.,
            ..clouds.clone()
        };
        assert_eq!(calm.drift(t).weather, [0.; 2]);
    }

    // Reflected clouds whiten an overcast sky overhead and leave a clear
    // one, and the ground below the horizon, as it was.
    #[test]
    fn reflected_clouds_whiten_a_covered_sky() {
        let clouds = |cover: u8| Clouds {
            weather: Arc::new(Weather::decoded(4, [cover, 0, 0, 255].repeat(16))),
            albedo: [1.; 3],
            shadow_day: 0.8,
            shadow_night: 0.,
            wind: 1.111,
        };
        let (w, h) = (32, 16);
        let blue = SkyMap {
            width: w,
            height: h,
            texels: vec![[0.2, 0.4, 1.]; (w * h) as usize],
        };
        let sun = [0.3, 0.9, 0.3];
        let light = [1.; 3];
        let zenith = (w / 2) as usize;
        let clear = clouds(0).reflect(&blue, sun, light, [0.; 2]);
        assert_eq!(clear.texels, blue.texels);
        let overcast = clouds(255).reflect(&blue, sun, light, [0.; 2]);
        let [r, _, b] = overcast.texels[zenith];
        assert!(r / b > 0.6, "overhead {:?}", overcast.texels[zenith]);
        let ground = (w * (h - 1)) as usize;
        assert_eq!(overcast.texels[ground], blue.texels[ground]);
    }

    // The reflected clouds drift with the weather map: cover on the map's
    // left half stands overhead at first, and half a map later the clear
    // half does.
    #[test]
    fn reflected_clouds_drift_with_the_weather_map() {
        let rgba = (0..16)
            .flat_map(|i| [if i % 4 < 2 { 255 } else { 0 }, 0, 0, 255])
            .collect();
        let clouds = Clouds {
            weather: Arc::new(Weather::decoded(4, rgba)),
            albedo: [1.; 3],
            shadow_day: 0.8,
            shadow_night: 0.,
            wind: 1.111,
        };
        let (w, h) = (32, 16);
        let blue = SkyMap {
            width: w,
            height: h,
            texels: vec![[0.2, 0.4, 1.]; (w * h) as usize],
        };
        let (sun, light) = ([0.3, 0.9, 0.3], [1.; 3]);
        let zenith = (w / 2) as usize;
        let now = clouds.reflect(&blue, sun, light, [0.; 2]);
        let later = clouds.reflect(&blue, sun, light, [0.5, 0.]);
        assert_ne!(now.texels[zenith], blue.texels[zenith], "covered overhead");
        assert_eq!(later.texels[zenith], blue.texels[zenith], "clear overhead");
    }
}
