// Editing the island tint: the working map, painting, terrain bake, and
// image import/export. Encoding, tiles and the save bake live in
// `anno_formats::tint`.
//
// Images exchange the shader's decoded color c (0..1, 0.5 neutral), the
// value before `multiplier = (2c)^2.2`, so a mid-grey image is no tint and
// painting in an image editor behaves like the game.
use anno_formats::{
    tint::{self, TintMap},
    vfs::Install,
};
use anyhow::{Context as _, Result};
use gpui_editor_kit::heightfield::{Brush, Heightfield};
use std::path::Path;

/// Level edited and previewed: 4 texels per metre. Finer shipped levels are
/// regenerated from it on save, so an edited island loses detail below
/// 25 cm; unedited islands keep their shipped tiles.
pub const WORK_LEVEL: u32 = 3;
/// Terrain vertex colors carry `multiplier / GAIN`; the renderer multiplies
/// back, so tint can brighten. Shipped multipliers stay under ~2.
pub const GAIN: f32 = 4.;

/// Paint presets as multipliers.
pub const PRESETS: [(&str, [f32; 3]); 7] = [
    ("Neutral", [1., 1., 1.]),
    ("Darken", [0.6, 0.6, 0.6]),
    ("Brighten", [1.5, 1.5, 1.5]),
    ("Warm", [1.25, 1.05, 0.8]),
    ("Cool", [0.85, 0.95, 1.2]),
    ("Dry", [1.3, 1.15, 0.7]),
    ("Lush", [0.8, 1.15, 0.75]),
];

/// The island's working tint level; missing tiles read as neutral, a
/// missing folder (no tint shipped) as an all-neutral map.
pub fn load(install: &Install, data_dir: &str, world: [u32; 2]) -> TintMap {
    TintMap::read(install, data_dir, WORK_LEVEL, world).unwrap_or_else(|e| {
        eprintln!("{data_dir}: tint unavailable, starting neutral: {e:#}");
        TintMap::neutral(WORK_LEVEL, world)
    })
}

/// Multiplier at island metres `(x, z)`, nearest texel.
pub fn sample(map: &TintMap, x: f32, z: f32) -> [f32; 3] {
    let tx = ((x.max(0.) * map.per_metre) as usize).min(map.width - 1);
    let ty = ((z.max(0.) * map.per_metre) as usize).min(map.height - 1);
    tint::decode(map.get(tx, ty))
}

/// Blends texels under the brush toward `target` (or neutral when `erase`).
pub fn paint(
    map: &mut TintMap,
    center: [f32; 2],
    brush: &Brush,
    target: [f32; 3],
    erase: bool,
) -> bool {
    let target = if erase { [1.; 3] } else { target };
    let pm = map.per_metre;
    let span = |c: f32, n: usize| {
        let lo = ((c - brush.radius) * pm).floor().max(0.) as usize;
        let hi = (((c + brush.radius) * pm).ceil().max(0.) as usize).min(n);
        lo..hi
    };
    let mut changed = false;
    for y in span(center[1], map.height) {
        for x in span(center[0], map.width) {
            let (wx, wz) = ((x as f32 + 0.5) / pm, (y as f32 + 0.5) / pm);
            let t = brush.weight((wx - center[0]).hypot(wz - center[1])) * brush.strength;
            // Decode/encode is lossy at u8; leave untouched texels alone.
            if t <= 0.01 {
                continue;
            }
            let i = y * map.width + x;
            let m = tint::decode(map.texels[i]);
            let next = tint::encode(std::array::from_fn(|c| {
                m[c] + (target[c] - m[c]) * t.min(1.)
            }));
            if next != map.texels[i] {
                map.texels[i] = next;
                changed = true;
            }
        }
    }
    changed
}

/// A tint derived from the terrain shape: hollows and steep faces darken,
/// ridges lighten a little, so fresh terrain does not read flat.
pub fn bake(field: &Heightfield, world: [u32; 2]) -> TintMap {
    // Cavity is measured against the mean 4 m away: large enough to see
    // gullies, small enough to keep cliff edges crisp.
    const REACH: f32 = 4.;
    TintMap::from_fn(WORK_LEVEL, world, |x, z| {
        let h = |dx: f32, dz: f32| field.height([x + dx, z + dz]);
        let c = h(0., 0.);
        let mean = (h(REACH, 0.) + h(-REACH, 0.) + h(0., REACH) + h(0., -REACH)) / 4.;
        let cavity = (c - mean).clamp(-3., 3.);
        let slope = ((h(1., 0.) - h(-1., 0.)).hypot(h(0., 1.) - h(0., -1.)) / 2.).min(2.);
        let light = 1. + cavity * 0.06 - slope * 0.1;
        // Hollows also cool slightly, as damp ground does.
        let cool = (-cavity * 0.02).max(0.);
        [light - cool, light, light + cool]
    })
}

/// Reads an image as decoded tint colors and resamples it over the island.
pub fn import(path: &Path, world: [u32; 2]) -> Result<TintMap> {
    let image = image::open(path)
        .with_context(|| format!("reading {}", path.display()))?
        .to_rgb32f();
    let (iw, ih) = image.dimensions();
    let size = [world[0] as f32, world[1] as f32];
    Ok(TintMap::from_fn(WORK_LEVEL, world, |x, z| {
        // Image row 0 is north, at z = size, as the game's map shows it.
        let px = ((x / size[0] * iw as f32) as u32).min(iw - 1);
        let py = (((1. - z / size[1]) * ih as f32) as u32).min(ih - 1);
        let c = image.get_pixel(px, py).0;
        c.map(|c| (2. * c.clamp(0., 1.)).powf(2.2))
    }))
}

/// Writes the map as an 8-bit RGB PNG of decoded tint colors.
pub fn export(map: &TintMap, path: &Path) -> Result<()> {
    let mut rgb = Vec::with_capacity(map.texels.len() * 3);
    // North (the last texel row) first, as image rows run.
    for &t in map.texels.chunks(map.width).rev().flatten() {
        for m in tint::decode(t) {
            rgb.push(
                (m.max(0.).powf(1. / 2.2) / 2. * 255.)
                    .round()
                    .clamp(0., 255.) as u8,
            );
        }
    }
    image::save_buffer(
        path,
        &rgb,
        map.width as u32,
        map.height as u32,
        image::ColorType::Rgb8,
    )
    .with_context(|| format!("writing {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::{PRESETS, WORK_LEVEL, export, import, paint, sample};
    use anno_formats::tint::TintMap;
    use gpui_editor_kit::heightfield::Brush;

    fn brush(radius: f32) -> Brush {
        Brush {
            radius,
            strength: 1.,
            ..Brush::default()
        }
    }

    #[test]
    fn painting_moves_toward_the_target_and_erase_returns_to_neutral() {
        let mut map = TintMap::neutral(WORK_LEVEL, [64, 64]);
        let dark = PRESETS[1].1;
        assert!(paint(&mut map, [32., 32.], &brush(4.), dark, false));
        let m = sample(&map, 32., 32.);
        assert!((m[0] - dark[0]).abs() < 0.05, "{m:?}");
        // Outside the brush stays shipped-neutral (1.04 luma).
        assert!((sample(&map, 5., 5.)[1] - 1.04).abs() < 0.03);
        paint(&mut map, [32., 32.], &brush(4.), dark, true);
        assert!((sample(&map, 32., 32.)[1] - 1.).abs() < 0.06);
    }

    #[test]
    fn exported_image_imports_back() {
        let mut map = TintMap::neutral(WORK_LEVEL, [32, 32]);
        paint(&mut map, [10., 20.], &brush(6.), PRESETS[3].1, false);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tint.png");
        export(&map, &path).unwrap();
        let back = import(&path, [32, 32]).unwrap();
        for (x, z) in [(10., 20.), (28., 4.)] {
            let (a, b) = (sample(&map, x, z), sample(&back, x, z));
            for c in 0..3 {
                assert!((a[c] - b[c]).abs() < 0.06, "{a:?} vs {b:?} at {x},{z}");
            }
        }
    }
}
