// Owns the sea around an island as Anno 117 draws it: an ambient preset's
// `<Water>` and the textures the water draws with, put together with the
// coast map (anno_core::water::coast) into the ocean the water pass draws
// (crate::water).
use crate::water::{Ocean, RiverWaves, Seasons, Waves};
use anno_core::ambient::Sea;
use anno_core::water::Coast;
use anno_formats::{vfs::Install, xml::tag};
use gpui_editor_kit::texture::{Dds, Image};
use std::sync::Arc;

/// A preset's `<Water>` (and `AmbientLightFactorOcean` beside it). Depths
/// and distances are world units; the waves' fetch and depth metres.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Water {
    coastal: Waves,
    offshore: Waves,
    seed: u32,
    offshore_distance: f32,
    offshore_transition: f32,
    caustics: f32,
    deep: [f32; 3],
    shallow: [f32; 3],
    scatter: [f32; 3],
    scatter_strength: f32,
    absorption_shallow: f32,
    shallow_depth: f32,
    absorption_smoothness: f32,
    view_depth: f32,
    reflection: f32,
}

impl Water {
    /// The `<Water>` block of an ambient preset, when it has the wave
    /// fields 117's ocean needs; 1800's has other ones.
    pub fn read(xml: &str) -> Option<Self> {
        let block = xml.find("<Water>").map(|at| &xml[at..])?;
        tag(block, "CoastalWaveFetch")?;
        let number = |name: &str, default: f32| {
            tag(block, name)
                .and_then(|v| v.parse::<f32>().ok())
                .unwrap_or(default)
        };
        let color = |name: &str, default: [f32; 3]| -> [f32; 3] {
            let Some(at) = block.find(&format!("<{name}>")) else {
                return default;
            };
            let rest = &block[at..];
            let axis = |a: &str| tag(rest, a).and_then(|v| v.parse::<f32>().ok());
            match (axis("x"), axis("y"), axis("z")) {
                (Some(x), Some(y), Some(z)) => [x, y, z],
                _ => default,
            }
        };
        let waves = |prefix: &str| Waves {
            fetch: number(&format!("{prefix}WaveFetch"), 16000.),
            wind_speed: number(&format!("{prefix}WaveWindSpeed"), 1.),
            choppiness: number(&format!("{prefix}WaveChoppiness"), 1.),
            directionality: number(&format!("{prefix}WaveDirectionality"), 0.5),
            depth: number(&format!("{prefix}WaveOceanDepth"), 100.),
        };
        // Defaults are amb_roman_01's: the engine sets none of its own.
        Some(Self {
            coastal: waves("Coastal"),
            offshore: waves("Offshore"),
            seed: number("WaveSeed", 35.) as u32,
            offshore_distance: number("OffshoreDistance", 2.),
            offshore_transition: number("OffshoreTransitionDist", 50.),
            caustics: number("CausticsIntensity", 0.351),
            deep: color("WaterColorDeep", [0.059, 0.188, 0.286]),
            shallow: color("WaterColorShallow", [0.757, 0.949, 1.]),
            scatter: color("ScatterColor", [0.129, 0.337, 0.4]),
            scatter_strength: number("ScatterStrength", 0.18),
            absorption_shallow: number("AbsorptionShallow", 0.6),
            shallow_depth: number("ShallowWaterDepth", 0.31),
            absorption_smoothness: number("AbsorptionSmoothness", 4.),
            view_depth: number("ViewDepth", 9.),
            reflection: tag(xml, "AmbientLightFactorOcean")
                .and_then(|v| v.parse().ok())
                .unwrap_or(1.),
        })
    }
}

impl Sea for Water {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

impl Water {
    /// The ocean for this water along `coast`: waves on the clock, or held
    /// at `time` seconds.
    pub(crate) fn ocean(
        &self,
        coast: Option<Arc<Coast>>,
        foam: Option<Image>,
        noise: Option<Image>,
        time: Option<f32>,
    ) -> Ocean {
        Ocean {
            coastal: self.coastal,
            offshore: self.offshore,
            seed: self.seed,
            // TODO: the game takes the wind's direction from an engine
            // global (build 26043928 0x14735b638) not traced to data; a fixed one here.
            wind: [1., 0.],
            offshore_distance: self.offshore_distance,
            offshore_transition: self.offshore_transition,
            caustics: self.caustics,
            deep: self.deep,
            shallow: self.shallow,
            scatter: self.scatter,
            scatter_strength: self.scatter_strength,
            absorption_shallow: self.absorption_shallow,
            shallow_depth: self.shallow_depth,
            absorption_smoothness: self.absorption_smoothness,
            view_depth: self.view_depth,
            reflection: self.reflection,
            coast,
            maps: None,
            foam,
            noise,
            time,
        }
    }
}

/// The game's river ripples (`engine/water/waves_small_y`), 48 frames of
/// 512² heights as BC6H blocks: 12 MB, read only with 117's ocean.
pub fn river_waves(install: &Install) -> Option<Arc<RiverWaves>> {
    let path = "data/base/graphics/engine/water/waves_small_y.dds";
    let bytes = install.read(path).ok()?;
    RiverWaves::decode(&bytes)
        .map(Arc::new)
        .map_err(|e| eprintln!("{path}: {e:#}"))
        .ok()
}

/// The noise the game's physical waves gate their shore rings with
/// (`engine/noise/noise01`).
pub fn noise(install: &Install) -> Option<Image> {
    let path = "data/base/graphics/engine/noise/noise01_0.dds";
    let bytes = install.read(path).ok()?;
    let dds = Dds::parse(&bytes).ok()?;
    dds.compressed_from(0, &bytes[dds.mip_offset(0)..])
        .map(Image::from)
        .map_err(|e| eprintln!("{path}: {e:#}"))
        .ok()
}

/// The game's foam patterns (`engine/water/foam01`), its largest mip at
/// most 1024 texels across, block compressed as shipped.
pub fn foam(install: &Install) -> Option<Image> {
    large(install, "data/base/graphics/engine/water/foam01_0.dds")
}

/// Duckweed on lakes (`engine/water/duckweed`, sRGB colour with its cover
/// in alpha) and its normal map, as [`foam`].
pub fn duckweed(install: &Install) -> Option<[Image; 2]> {
    let dir = "data/base/graphics/engine/water";
    Some([
        large(install, &format!("{dir}/duckweed_0.dds"))?,
        large(install, &format!("{dir}/duckweed_norm_0.dds"))?,
    ])
}

/// Rain on the water (`engine/environment/rainsplash2_bump`, cWaterRippleBumpMap):
/// a looping volume of splash ripples, its height in red, decoded to one
/// byte a texel (1 MB).
pub fn rain_ripples(install: &Install) -> Option<Arc<Ripples>> {
    let path = "data/base/graphics/engine/environment/rainsplash2_bump.dds";
    let bytes = install.read(path).ok()?;
    Ripples::decode(&bytes)
        .map(Arc::new)
        .map_err(|e| eprintln!("{path}: {e:#}"))
        .ok()
}

/// The painted flow map Egypt's rivers flow by (`DLC03_FlowMap_FileName`,
/// a `.psd` the game ships as `_0.dds`): its first mip, block compressed,
/// as the shader reads it texel by texel.
pub fn egypt_flow(install: &Install, path: &str) -> Option<Image> {
    let path = path.replace('\\', "/");
    let path = format!("{}_0.dds", path.strip_suffix(".psd").unwrap_or(&path));
    let bytes = install.read(&path).ok()?;
    let dds = Dds::parse(&bytes).ok()?;
    dds.compressed_from(0, &bytes[dds.mip_offset(0)..])
        .map(Image::from)
        .map_err(|e| eprintln!("{path}: {e:#}"))
        .ok()
}

/// Where Egypt's islands lie in its session: the province templates' size
/// and their fixed islands' corners. The session's flow map spans the map
/// from (0, 0) to `size` (Anno117.exe build 26607411 0x142516BC0 sets it so).
pub struct EgyptMap {
    pub size: [u32; 2],
    corners: Vec<(String, [u32; 2])>,
}

impl EgyptMap {
    pub fn read(install: &Install) -> Option<Self> {
        use anno_formats::a7m::template::{MapTemplate, SlotKind};
        let mut paths: Vec<String> = (install.search(".a7tinfo", 1000).into_iter())
            .map(|id| install.file_path(id).to_owned())
            .filter(|p| p.contains("/provinces/egypt/templates/") && p.ends_with(".a7tinfo"))
            .collect();
        paths.sort();
        let mut out: Option<Self> = None;
        for path in paths {
            let Ok(t) = install
                .read(&path)
                .map_err(anyhow::Error::from)
                .and_then(|b| MapTemplate::parse(&b))
            else {
                continue;
            };
            let map = out.get_or_insert(Self {
                size: t.size,
                corners: Vec::new(),
            });
            for s in &t.slots {
                // TODO: a fixed island turned by quarters; Egypt's aren't.
                if let SlotKind::Fixed { path, .. } = &s.kind
                    && s.turns == 0
                {
                    map.corners
                        .push((path.replace('\\', "/").to_ascii_lowercase(), s.position));
                }
            }
        }
        out
    }

    /// The island at `path`'s corner in the session: its own slot, else,
    /// for an island holding slotted ones in its folder (the quadrants each
    /// hold a quarter of the valley), the nearest corner of those.
    pub fn corner(&self, path: &str) -> Option<[f32; 2]> {
        let path = path.replace('\\', "/").to_ascii_lowercase();
        if let Some((_, at)) = self.corners.iter().find(|(p, _)| *p == path) {
            return Some(at.map(|v| v as f32));
        }
        let folder = &path[..path.rfind('/')? + 1];
        (self.corners.iter())
            .filter(|(p, _)| p.starts_with(folder))
            .map(|(_, at)| *at)
            .reduce(|a, b| [a[0].min(b[0]), a[1].min(b[1])])
            .map(|at| at.map(|v| v as f32))
    }
}

/// Egypt's river seasons from `config/engine/globals.xml`.
pub fn seasons(install: &Install) -> Option<Seasons> {
    let path = "data/base/config/engine/globals.xml";
    let bytes = install.read(path).ok()?;
    Seasons::read(&String::from_utf8_lossy(&bytes))
}

/// What Egypt's rivers draw wind gusts from (ComputeGustIntensity):
/// `engine/noise/wind_texture` (cWindTex, 256² bytes) and the red channel of
/// `engine/noise/perlin_noise_and_gradients` (cPerlinNoiseTex, 64² half
/// floats), first mips.
pub struct Gusts {
    pub wind_size: [u32; 2],
    pub wind: Vec<u8>,
    pub perlin_size: [u32; 2],
    pub perlin: Vec<u16>,
}

pub fn gusts(install: &Install) -> Option<Arc<Gusts>> {
    let read = |path: &str| {
        install
            .read(path)
            .map_err(|e| eprintln!("{path}: {e:#}"))
            .ok()
    };
    let wind = read("data/base/graphics/engine/noise/wind_texture.dds")?;
    let perlin = read("data/base/graphics/engine/noise/perlin_noise_and_gradients.dds")?;
    Gusts::decode(&wind, &perlin)
        .map(Arc::new)
        .map_err(|e| eprintln!("gust textures: {e:#}"))
        .ok()
}

impl Gusts {
    /// `wind` an 8-bit luminance DDS, `perlin` a D3DFMT_A16B16G16R16F one.
    fn decode(wind: &[u8], perlin: &[u8]) -> anyhow::Result<Self> {
        use anyhow::ensure;
        let size = |d: &[u8]| -> anyhow::Result<[u32; 2]> {
            ensure!(d.len() >= 128 && &d[..4] == b"DDS ", "not a DDS file");
            let u32_at = |at: usize| u32::from_le_bytes(d[at..at + 4].try_into().unwrap());
            Ok([u32_at(16), u32_at(12)])
        };
        let wind_size = size(wind)?;
        ensure!(
            &wind[84..88] == [0; 4] && wind[88] == 8,
            "wind texture: expected 8-bit luminance"
        );
        let texels = (wind_size[0] * wind_size[1]) as usize;
        ensure!(wind.len() >= 128 + texels, "wind texture: too short");
        let perlin_size = size(perlin)?;
        ensure!(perlin[84] == 113, "perlin noise: expected RGBA half floats");
        let count = (perlin_size[0] * perlin_size[1]) as usize;
        ensure!(perlin.len() >= 128 + count * 8, "perlin noise: too short");
        let perlin = (perlin[128..128 + count * 8].as_chunks::<8>().0.iter())
            .map(|t| u16::from_le_bytes([t[0], t[1]]))
            .collect();
        Ok(Self {
            wind_size,
            wind: wind[128..128 + texels].to_vec(),
            perlin_size,
            perlin,
        })
    }
}

/// A volume of one-byte heights, frame by frame, row by row.
#[derive(Debug)]
pub struct Ripples {
    pub size: [u32; 3],
    pub heights: Vec<u8>,
}

impl Ripples {
    /// Decodes a DXT5 (BC3) volume's red channel, its first mip only.
    fn decode(dds: &[u8]) -> anyhow::Result<Self> {
        use anyhow::ensure;
        ensure!(dds.len() >= 128 && &dds[..4] == b"DDS ", "not a DDS file");
        let u32_at = |at: usize| u32::from_le_bytes(dds[at..at + 4].try_into().unwrap());
        let (height, width, depth) = (u32_at(12), u32_at(16), u32_at(24).max(1));
        ensure!(&dds[84..88] == b"DXT5", "rain ripples: expected DXT5");
        let (bw, bh) = (width.div_ceil(4) as usize, height.div_ceil(4) as usize);
        let slice = bw * bh * 16;
        let data = &dds[128..];
        ensure!(
            data.len() >= slice * depth as usize,
            "rain ripples: too short"
        );
        let (w, h) = (width as usize, height as usize);
        let mut heights = vec![0u8; w * h * depth as usize];
        for z in 0..depth as usize {
            for (b, block) in data[z * slice..(z + 1) * slice]
                .as_chunks::<16>()
                .0
                .iter()
                .enumerate()
            {
                let mut texels = [0u8; 64];
                bcdec_rs::bc3(block, &mut texels, 16);
                let (bx, by) = (b % bw * 4, b / bw * 4);
                for t in 0..16 {
                    let (x, y) = (bx + t % 4, by + t / 4);
                    if x < w && y < h {
                        heights[(z * h + y) * w + x] = texels[t * 4];
                    }
                }
            }
        }
        Ok(Self {
            size: [width, height, depth],
            heights,
        })
    }
}

/// A large texture's largest mip at most 1024 texels across.
fn large(install: &Install, path: &str) -> Option<Image> {
    let header = install.read_range(path, 0, 148).ok()?;
    let dds = Dds::parse(&header).ok()?;
    let first = dds.mip_for(1024);
    let at = dds.mip_offset(first) as u64;
    let tail = install.read_range(path, at, u64::MAX).ok()?;
    dds.compressed_from(first, &tail)
        .map(Image::from)
        .map_err(|e| eprintln!("{path}: {e:#}"))
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    // The ripple volume decodes to 48 frames of finite heights.
    #[test]
    fn river_waves_decode_from_the_game() {
        let Some(dir) = anno_formats::vfs::install_dir("ANNO117_DIR") else {
            eprintln!("skipping: ANNO117_DIR unset");
            return;
        };
        let waves = river_waves(&Install::open(dir, crate::ARCHIVE_FAMILIES).unwrap()).unwrap();
        assert_eq!(waves.size, [512, 512, 48]);
        let heights = waves.heights();
        assert_eq!(heights.len(), 512 * 512 * 48);
        assert!(heights.iter().any(|h| h & 0x7fff != 0));
        assert!(heights.iter().all(|h| h & 0x7c00 != 0x7c00));
    }

    // Needs the game: Egypt's islands find their corner in the session from
    // the province template, a quadrant from the islands in its folder, and
    // the painted flow map covers the whole 2560 m session.
    #[test]
    fn egypt_islands_lie_on_the_session_flow_map() {
        let Some(dir) = anno_formats::vfs::install_dir("ANNO117_DIR") else {
            return;
        };
        let install = Install::open(dir, crate::ARCHIVE_FAMILIES).unwrap();
        let map = EgyptMap::read(&install).expect("egypt templates");
        assert_eq!(map.size, [2560, 2560]);
        let pool = "data/dlc03/provinces/egypt/islands/pool";
        let quadrant =
            format!("{pool}/egypt_island_quadrant_01_x1_y1/egypt_island_quadrant_01_x1_y1.a7m");
        assert_eq!(map.corner(&quadrant), Some([960., 960.]));
        let valley = format!(
            "{pool}/egypt_island_quadrant_01_x1_y1/egypt_dlc_03_valley_large_05/egypt_dlc_03_valley_large_05.a7m"
        );
        assert_eq!(map.corner(&valley), Some([960., 1216.]));
        assert_eq!(
            map.corner("data/base/provinces/roman/islands/x/x.a7m"),
            None
        );
        let seasons = seasons(&install).expect("globals.xml");
        let flow = egypt_flow(&install, seasons.flow_map.as_deref().unwrap()).expect("flow map");
        let Image::Compressed(c) = flow else {
            panic!("block compressed")
        };
        assert_eq!((c.width, c.height), (2560, 2560));
    }

    // Needs the game: amb_roman_01's water.
    #[test]
    fn roman_preset_has_two_wave_fields() {
        let Some(dir) = anno_formats::vfs::install_dir("ANNO117_DIR") else {
            return;
        };
        let install = Install::open(dir, crate::ARCHIVE_FAMILIES).unwrap();
        let id = install
            .search("amb_roman_01.xml", 10)
            .into_iter()
            .find(|&id| install.file_name(id) == "amb_roman_01.xml")
            .expect("preset");
        let xml = String::from_utf8_lossy(&install.read_file(id).unwrap()).into_owned();
        let water = Water::read(&xml).expect("117 water");
        assert_eq!(water.coastal.fetch, 16000.);
        assert_eq!(water.offshore.wind_speed, 3.);
        assert_eq!(water.seed, 35);
        assert_eq!(water.view_depth, 9.);
        assert!(foam(&install).is_some(), "foam01 ships");
    }
}
