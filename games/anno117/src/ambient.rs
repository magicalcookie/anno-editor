// Reads Anno 117's ambient presets (`config/engine/ambientsettings/`, one
// file per preset): their light, physical atmosphere, night sky and ocean.
use crate::{
    sea::Water,
    sky::{Atmosphere, Clouds, PresetSky},
};
use anno_core::ambient::Ambient;
use anno_core::cube::Cubes;
use anno_formats::{
    vfs::{FileId, Install},
    xml::tag,
};
use std::sync::Arc;

/// The presets an island of `region` can pick: every preset in
/// `ambientsettings/` but character portraits, UI scenes, benchmarks and
/// tests, so any region's light, the DLC's interior and event variants and
/// the generic ones are all on offer. Shipped islands use `amb_roman_01`,
/// `amb_celtic_01`/`_03` and a few one-offs.
pub fn names(install: &Install, region: &str) -> Vec<String> {
    let mut out: Vec<String> = (install.search("config/engine/ambientsettings/", 2000))
        .into_iter()
        .filter_map(|id| {
            let name = install.file_name(id).strip_suffix(".xml")?;
            let skip = ["benchmark", "test"].iter().any(|s| name.contains(s))
                || name.starts_with("portrait_")
                || name.starts_with("ui_");
            (!skip).then(|| name.to_owned())
        })
        .collect();
    anno_core::ambient::order_presets(&mut out, &format!("amb_{}_", region.to_ascii_lowercase()));
    out
}

/// The presets among `names` that have a file, with defaults for what each
/// leaves out.
pub fn ambients(install: &Install, names: &[&str]) -> Vec<(String, Ambient)> {
    let files = install.search("config/engine/ambientsettings/", 2000);
    // Presets share their night sky: one read of each cube.
    let mut cubes = Cubes::default();
    let mut weather = std::collections::HashMap::new();
    (names.iter())
        .filter_map(|name| {
            let file = format!("{name}.xml");
            let id =
                (files.iter()).find(|&&id| install.file_name(id).eq_ignore_ascii_case(&file))?;
            Some((
                (*name).to_owned(),
                read(install, *id, &mut cubes, &mut weather)?,
            ))
        })
        .collect()
}

fn read(
    install: &Install,
    id: FileId,
    cubes: &mut Cubes,
    weather: &mut std::collections::HashMap<String, Option<Arc<crate::sky::Weather>>>,
) -> Option<Ambient> {
    let xml = String::from_utf8_lossy(&install.read_file(id).ok()?).into_owned();
    let number = |name: &str| tag(&xml, name)?.parse::<f32>().ok();
    // `<LightColor><x>4.197</><y>…</><z>…</></>`
    let color = |name: &str| -> Option<[f32; 3]> {
        let open = format!("<{name}>");
        let rest = &xml[xml.find(&open)? + open.len()..];
        let mut out = [0.; 3];
        for (v, axis) in out.iter_mut().zip(["x", "y", "z"]) {
            *v = tag(rest, axis)?.parse().ok()?;
        }
        Some(out)
    };
    let d = Ambient::default();
    let ground = color("GroundColorAmbient").unwrap_or(d.ground);
    let elevation = number("MaxSunElevation").unwrap_or(d.elevation);
    let min_elevation = number("MinSunElevation").unwrap_or(d.min_elevation);
    let sky = Atmosphere::read(&xml).map(|atmosphere| {
        Arc::new(PresetSky {
            atmosphere,
            path: crate::sky::DayPath {
                min: min_elevation,
                max: elevation,
                // The engine's default when a preset has none is not
                // traced; every shipped preset sets it.
                turn: number("CamYRotOffset2").unwrap_or(0.),
            },
            ground,
            night: tag(&xml, "SkyBoxPathNight").and_then(|p| cubes.get(install, p)),
            clouds: Clouds::read(&xml, install, weather),
        }) as _
    });
    Some(Ambient {
        sun: color("LightColor").unwrap_or(d.sun),
        ground,
        exposure: number("Exposure").unwrap_or(d.exposure),
        sun_occlusion: number("SunAO").unwrap_or(d.sun_occlusion),
        occlusion: number("AOIntensity").unwrap_or(d.occlusion),
        ambient: number("AmbientLightFactor").unwrap_or(d.ambient),
        sky,
        sea: Water::read(&xml).map(|w| Arc::new(w) as _),
        min_elevation,
        elevation,
        glow: number("GlowBrightness").unwrap_or(d.glow),
        night_glow: number("NightGlowBrightness").unwrap_or(d.night_glow),
        ..d
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // Needs the game: the presets differ in exposure and sun occlusion.
    #[test]
    fn ambient_presets_read_their_light() {
        let Some(dir) = anno_formats::vfs::install_dir("ANNO117_DIR") else {
            eprintln!("skipping: ANNO117_DIR unset");
            return;
        };
        let install = Install::open(dir, crate::ARCHIVE_FAMILIES).unwrap();
        let read = ambients(
            &install,
            &["amb_celtic_03", "amb_roman_01", "no_such_preset"],
        );
        let get = |name: &str| read.iter().find(|(n, _)| n == name).map(|(_, a)| a.clone());
        let celtic = get("amb_celtic_03").unwrap();
        assert_eq!((celtic.exposure, celtic.sun_occlusion), (0.65, 0.198));
        assert!((celtic.sun[0] - 4.197).abs() < 1e-3 && celtic.ground == [0.2; 3]);
        assert_eq!((celtic.occlusion, celtic.ambient), (0.999, 1.));
        assert!((get("amb_roman_01").unwrap().elevation - 0.644444).abs() < 1e-5);
        assert!(get("no_such_preset").is_none());
    }
}
