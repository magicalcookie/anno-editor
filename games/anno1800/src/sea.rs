// Owns Anno 1800's sea as its ambient presets set it: the `<Water>` block
// (CAmbientArea reads it into its water settings, Anno1800.exe 0x141c713ac)
// and the preset's `<WindDirection>`. The water pass (crate::water) draws
// it.
// Engine addresses here are Anno1800.exe build 16367353 (Steam).
use anno_core::ambient::Sea;
use anno_formats::xml::tag;

/// A preset's `<Water>`. The ocean simulator copies the wave fields over
/// its large-wave settings every frame (0x141d17726): `WaveAmplitude` is
/// the Phillips spectrum's amplitude, `WaveSize` its wind speed, despite
/// the name, `WindDamping` how much waves running against the wind keep,
/// `WaveSeed` the modulus of its random numbers, and `WaveChoppiness` how
/// far crests lean sideways.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Water {
    pub amplitude: f32,
    pub choppiness: f32,
    pub wind_speed: f32,
    pub damping: f32,
    pub seed: u32,
    /// World units of water the view and the sun see through.
    pub view_depth: f32,
    pub light_depth: f32,
    pub absorption: f32,
    pub caustics: f32,
    pub deep: [f32; 3],
    pub shallow: [f32; 3],
    /// The wind's direction in world X and Z, unit length.
    pub wind: [f32; 2],
}

impl Water {
    /// The engine's own large-wave settings (CRenderer::InitOcean,
    /// 0x141ec25f8): what a preset without a field keeps.
    const ENGINE: Water = Water {
        amplitude: 0.35,
        choppiness: 1.4,
        wind_speed: 900.,
        damping: 0.3,
        seed: 13,
        // The engine sets no depths or colors; moderate_01's.
        view_depth: 12.,
        light_depth: 10.,
        absorption: 3.5,
        caustics: 1.,
        deep: [0.054902, 0.160784, 0.270588],
        shallow: [0., 0.77, 0.82],
        // Five of the seven presets that set a wind blow it along +X.
        wind: [1., 0.],
    };

    /// The `<Water>` block of an 1800 preset; `None` for one without, or
    /// for 117's, which has other fields.
    pub fn read(xml: &str) -> Option<Self> {
        let block = xml.find("<Water>").map(|at| &xml[at..])?;
        tag(block, "WaveAmplitude")?;
        let d = Self::ENGINE;
        let number = |name: &str, default: f32| {
            tag(block, name)
                .and_then(|v| v.parse::<f32>().ok())
                .unwrap_or(default)
        };
        let vector = |text: &str, name: &str| -> Option<[f32; 3]> {
            let rest = &text[text.find(&format!("<{name}>"))?..];
            let axis = |a: &str| tag(rest, a).and_then(|v| v.parse::<f32>().ok());
            Some([axis("x")?, axis("y")?, axis("z").unwrap_or(0.)])
        };
        let wind = vector(xml, "WindDirection")
            .map(|[x, y, _]| [x, y])
            .filter(|w| w[0].hypot(w[1]) > 1e-6)
            .map_or(d.wind, |w| {
                let l = w[0].hypot(w[1]);
                [w[0] / l, w[1] / l]
            });
        Some(Self {
            amplitude: number("WaveAmplitude", d.amplitude),
            choppiness: number("WaveChoppiness", d.choppiness),
            wind_speed: number("WaveSize", d.wind_speed),
            damping: number("WindDamping", d.damping),
            seed: (number("WaveSeed", d.seed as f32) as u32).max(1),
            view_depth: number("ViewDepth", d.view_depth),
            light_depth: number("LightDepth", d.light_depth),
            absorption: number("Absorption", d.absorption),
            caustics: number("CausticsIntensity", d.caustics),
            deep: vector(block, "WaterColorDeep").unwrap_or(d.deep),
            shallow: vector(block, "WaterColorShallow").unwrap_or(d.shallow),
            wind,
        })
    }
}

impl Sea for Water {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // moderate_01's block as shipped.
    const MODERATE: &str = "<Water><WaveAmplitude>0.250000</>\n<WaveChoppiness>1.600000</>\n\
        <WaveSize>800.000000</>\n<WindDamping>0.500000</>\n<WaveSeed>13</>\n<ViewDepth>12.000000</>\n\
        <LightDepth>10.000000</>\n<Absorption>3.500000</>\n<CausticsIntensity>1.000000</>\n\
        <WaterColorDeep><x>0.054902</>\n<y>0.160784</>\n<z>0.270588</>\n</>\n\
        <WaterColorShallow><x>0.000000</>\n<y>0.770000</>\n<z>0.820000</>\n</>\n</>";

    #[test]
    fn a_preset_block_reads_every_field() {
        let w = Water::read(MODERATE).unwrap();
        assert_eq!(w.amplitude, 0.25);
        assert_eq!(w.choppiness, 1.6);
        assert_eq!(w.wind_speed, 800.);
        assert_eq!(w.damping, 0.5);
        assert_eq!(w.seed, 13);
        assert_eq!((w.view_depth, w.light_depth), (12., 10.));
        assert_eq!(w.deep, [0.054902, 0.160784, 0.270588]);
        assert_eq!(w.shallow, [0., 0.77, 0.82]);
        assert_eq!(w.wind, [1., 0.]);
        // A preset's wind direction turns the waves, kept unit length.
        let windy = format!("{MODERATE}<WindDirection><x>3</>\n<y>-4</>\n</>");
        assert_eq!(Water::read(&windy).unwrap().wind, [0.6, -0.8]);
        // 117's block has other fields and is not 1800's water.
        assert!(Water::read("<Water><CoastalWaveFetch>1</></>").is_none());
        assert!(Water::read("<Name>x</>").is_none());
    }

    // Every shipped local preset carries a block, and its values are ones
    // the waves can take.
    #[test]
    fn shipped_presets_read() {
        let Some(dir) = anno_formats::vfs::install_dir("ANNO1800_DIR") else {
            return;
        };
        let install = anno_formats::vfs::Install::open(dir, crate::ARCHIVE_FAMILIES).unwrap();
        let files = install.search("config/engine/ambientsettings/local/", 2000);
        let mut read = 0;
        for &id in &files {
            let xml = String::from_utf8_lossy(&install.read_file(id).unwrap()).into_owned();
            let Some(w) = Water::read(&xml) else { continue };
            assert!(
                w.amplitude >= 0. && w.wind_speed > 0. && w.seed > 0,
                "{w:?}"
            );
            read += 1;
        }
        assert!(read > 40, "{read} presets with water");
    }
}
