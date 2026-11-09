// Terrain in and out as files other tools use: heightmaps (16-bit grey PNG,
// raw .r16 and .r32, 8-bit images) and meshes (OBJ, glTF/GLB from Blender or
// World Machine) resampled onto a height grid. Needs the `import` feature.
//
// Rows run along +Z (grid row 0 first). Games store heights in their own
// fixed-point units, so 16-bit files go through a `Units16` the editor picks;
// export then import is lossless in those units.
use super::Heightfield;
use crate::math::{self, Vec3};
use crate::pick::{PickTree, Ray};
use anyhow::{Context, Result, bail, ensure};
use std::path::Path;

/// How 16-bit samples (PNG, .r16) map to metres:
/// `sample = height × per_metre + zero`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Units16 {
    pub per_metre: f32,
    /// The sample value of height 0, usually sea level.
    pub zero: f32,
}

impl Units16 {
    fn encode(self, h: f32) -> u16 {
        (h * self.per_metre + self.zero).round().clamp(0., 65535.) as u16
    }

    fn decode(self, v: u16) -> f32 {
        (f32::from(v) - self.zero) / self.per_metre
    }
}

/// Writes the heights as a 16-bit grey PNG, or raw little-endian `.r16` by extension.
pub fn export_heightmap(field: &Heightfield, path: &Path, units: Units16) -> Result<()> {
    let g = &field.grid;
    let values: Vec<u16> = field.heights.iter().map(|&h| units.encode(h)).collect();
    if extension(path) == "r16" {
        let bytes: Vec<u8> = values.iter().flat_map(|v| v.to_le_bytes()).collect();
        return std::fs::write(path, bytes).with_context(|| format!("writing {}", path.display()));
    }
    let image =
        image::ImageBuffer::<image::Luma<u16>, _>::from_raw(g.width as u32, g.depth as u32, values)
            .context("heightmap buffer")?;
    image
        .save(path)
        .with_context(|| format!("writing {}", path.display()))
}

/// A decoded heightmap file in metres, rows along +Z, before it is fitted to
/// any grid. Hosts that import into part of a terrain sample it themselves.
#[derive(Clone, Debug, PartialEq)]
pub struct HeightImage {
    pub width: usize,
    pub depth: usize,
    pub metres: Vec<f32>,
}

impl HeightImage {
    /// Bilinear height at `u`, `v` in 0..=1 across the image; out-of-range
    /// coordinates clamp to the edge. The corners hit the corner samples exactly.
    pub fn sample(&self, u: f32, v: f32) -> f32 {
        let fx = u.clamp(0., 1.) * (self.width - 1) as f32;
        let fz = v.clamp(0., 1.) * (self.depth - 1) as f32;
        let (x0, z0) = (fx.floor() as usize, fz.floor() as usize);
        let (x1, z1) = ((x0 + 1).min(self.width - 1), (z0 + 1).min(self.depth - 1));
        let (u, v) = (fx - x0 as f32, fz - z0 as f32);
        let at = |x: usize, z: usize| self.metres[z * self.width + x];
        let top = at(x0, z0) * (1. - u) + at(x1, z0) * u;
        let bottom = at(x0, z1) * (1. - u) + at(x1, z1) * u;
        top * (1. - v) + bottom * v
    }
}

/// Largest heightmap `read_heightmap` decodes: 8193², a common World Machine
/// export size. Its metres alone take 256 MiB, so bigger files are refused
/// before any pixels are read.
pub const MAX_HEIGHTMAP_SAMPLES: usize = 8193 * 8193;

/// Reads an image or raw heightmap into metres. 16-bit PNG and `.r16`/`.raw`
/// use `units`; 8-bit images map 0..255 onto `eight_bit` metres; `.r32` is
/// little-endian f32 metres. Raw files must be square with no trailing bytes.
/// Rejects non-finite samples and files over `MAX_HEIGHTMAP_SAMPLES`, so
/// callers can validate before editing.
pub fn read_heightmap(path: &Path, units: Units16, eight_bit: [f32; 2]) -> Result<HeightImage> {
    ensure!(
        units.per_metre.is_finite() && units.per_metre != 0. && units.zero.is_finite(),
        "16-bit height units must be finite with a nonzero scale"
    );
    ensure!(
        eight_bit.iter().all(|v| v.is_finite()),
        "8-bit height range must be finite"
    );
    let raw = |bytes: &[u8], size: usize| -> Result<usize> {
        ensure!(
            bytes.len().is_multiple_of(size),
            "{} is {} bytes, not a whole number of {}-bit samples",
            path.display(),
            bytes.len(),
            size * 8
        );
        let n = bytes.len() / size;
        let side = (n as f64).sqrt().round() as usize;
        ensure!(
            side * side == n,
            "{} holds {n} samples, not a square {}-bit raw heightmap",
            path.display(),
            size * 8
        );
        Ok(side)
    };
    let too_big = |samples: usize| {
        ensure!(
            samples <= MAX_HEIGHTMAP_SAMPLES,
            "{} has {samples} samples; heightmaps are limited to 8193 × 8193",
            path.display()
        );
        Ok(())
    };
    let read = |size: usize| -> Result<Vec<u8>> {
        let length = std::fs::metadata(path)
            .with_context(|| format!("reading {}", path.display()))?
            .len();
        too_big(usize::try_from(length / size as u64).unwrap_or(usize::MAX))?;
        std::fs::read(path).with_context(|| format!("reading {}", path.display()))
    };
    let (width, depth, metres): (usize, usize, Vec<f32>) = match extension(path).as_str() {
        "r16" | "raw" => {
            let bytes = read(2)?;
            let side = raw(&bytes, 2)?;
            let m = bytes
                .as_chunks::<2>()
                .0
                .iter()
                .map(|b| units.decode(u16::from_le_bytes(*b)))
                .collect();
            (side, side, m)
        }
        "r32" => {
            let bytes = read(4)?;
            let side = raw(&bytes, 4)?;
            let m = bytes
                .as_chunks::<4>()
                .0
                .iter()
                .map(|b| f32::from_le_bytes(*b))
                .collect();
            (side, side, m)
        }
        _ => {
            // Compressed images can decode far larger than the file.
            let (w, h) = image::image_dimensions(path)
                .with_context(|| format!("reading {}", path.display()))?;
            too_big(w as usize * h as usize)?;
            let image = image::open(path).with_context(|| format!("reading {}", path.display()))?;
            let (w, h) = (image.width() as usize, image.height() as usize);
            let metres = match image {
                image::DynamicImage::ImageLuma16(_)
                | image::DynamicImage::ImageLumaA16(_)
                | image::DynamicImage::ImageRgb16(_)
                | image::DynamicImage::ImageRgba16(_) => image
                    .into_luma16()
                    .into_raw()
                    .into_iter()
                    .map(|v| units.decode(v))
                    .collect(),
                _ => {
                    let [lo, hi] = eight_bit;
                    image
                        .into_luma8()
                        .into_raw()
                        .into_iter()
                        .map(|v| lo + f32::from(v) / 255. * (hi - lo))
                        .collect()
                }
            };
            (w, h, metres)
        }
    };
    ensure!(width > 1 && depth > 1, "heightmap is too small");
    if let Some(i) = metres.iter().position(|h| !h.is_finite()) {
        bail!(
            "{} has a non-finite height at column {}, row {}",
            path.display(),
            i % width,
            i / width
        );
    }
    Ok(HeightImage {
        width,
        depth,
        metres,
    })
}

/// Replaces the heights from an image or raw file (see `read_heightmap`),
/// resampled bilinearly when its size differs from the grid and clamped to the
/// field's limits. The field is untouched when the file is rejected.
pub fn import_heightmap(
    field: &mut Heightfield,
    path: &Path,
    units: Units16,
    eight_bit: [f32; 2],
) -> Result<()> {
    let image = read_heightmap(path, units, eight_bit)?;
    let g = field.grid;
    let [lo, hi] = field.limits;
    for z in 0..g.depth {
        for x in 0..g.width {
            let u = x as f32 / (g.width - 1) as f32;
            let v = z as f32 / (g.depth - 1) as f32;
            field.heights[g.index(x, z)] = image.sample(u, v).clamp(lo, hi);
        }
    }
    Ok(())
}

/// How an imported mesh lands on the grid.
#[derive(Clone, Copy, Debug)]
pub struct MeshFit {
    /// Scale the mesh uniformly so its X/Z footprint fills the grid, and move
    /// its lowest point to `floor`; otherwise use its coordinates as metres
    /// unchanged.
    pub fit: bool,
    /// Blender's OBJ default is Y up; some tools write Z up.
    pub z_up: bool,
    /// Height for grid samples the mesh does not cover (open sea).
    pub floor: f32,
}

impl Default for MeshFit {
    fn default() -> Self {
        Self {
            fit: true,
            z_up: false,
            floor: 0.,
        }
    }
}

/// What a mesh import did.
pub struct MeshReport {
    pub triangles: usize,
    pub covered: usize,
    pub samples: usize,
}

/// Loads every triangle of an OBJ or glTF/GLB file in its own units, Y up.
pub fn load_mesh(path: &Path, z_up: bool) -> Result<(Vec<Vec3>, Vec<u32>)> {
    let (mut positions, mut indices) = (Vec::new(), Vec::new());
    match extension(path).as_str() {
        "obj" => {
            let (models, _) = tobj::load_obj(
                path,
                &tobj::LoadOptions {
                    triangulate: true,
                    single_index: true,
                    ..Default::default()
                },
            )
            .with_context(|| format!("reading {}", path.display()))?;
            for m in models {
                let base = positions.len() as u32;
                positions.extend(m.mesh.positions.as_chunks::<3>().0.iter().copied());
                indices.extend(m.mesh.indices.iter().map(|i| base + i));
            }
        }
        "gltf" | "glb" => {
            let (doc, buffers, _) =
                gltf::import(path).with_context(|| format!("reading {}", path.display()))?;
            let scene = doc
                .default_scene()
                .or_else(|| doc.scenes().next())
                .context("glTF has no scene")?;
            let mut stack: Vec<(gltf::Node, [f32; 16])> =
                scene.nodes().map(|n| (n, math::IDENTITY)).collect();
            while let Some((node, parent)) = stack.pop() {
                let local: [f32; 16] = node
                    .transform()
                    .matrix()
                    .as_flattened()
                    .try_into()
                    .expect("4x4");
                let world = math::multiply(&parent, &local);
                if let Some(mesh) = node.mesh() {
                    for prim in mesh.primitives() {
                        if prim.mode() != gltf::mesh::Mode::Triangles {
                            continue;
                        }
                        let reader =
                            prim.reader(|b| buffers.get(b.index()).map(|d| d.0.as_slice()));
                        let Some(points) = reader.read_positions() else {
                            continue;
                        };
                        let base = positions.len() as u32;
                        positions.extend(points.map(|p| math::transform(&world, p)));
                        match reader.read_indices() {
                            Some(i) => indices.extend(i.into_u32().map(|i| base + i)),
                            None => indices.extend(base..positions.len() as u32),
                        }
                    }
                }
                stack.extend(node.children().map(|c| (c, world)));
            }
        }
        other => bail!(
            "Unsupported mesh format .{other}; export OBJ or glTF (FBX: convert with Blender)"
        ),
    }
    ensure!(!indices.is_empty(), "{} has no triangles", path.display());
    ensure!(
        positions.iter().flatten().all(|v| v.is_finite()),
        "{} has non-finite vertex positions",
        path.display()
    );
    ensure!(
        indices.iter().all(|&i| (i as usize) < positions.len()),
        "{} has triangle indices past its vertices",
        path.display()
    );
    if z_up {
        for p in &mut positions {
            *p = [p[0], p[2], -p[1]];
        }
    }
    Ok((positions, indices))
}

/// Replaces the heights by casting straight down onto an imported mesh.
pub fn import_mesh(field: &mut Heightfield, path: &Path, fit: MeshFit) -> Result<MeshReport> {
    let (mut positions, indices) = load_mesh(path, fit.z_up)?;
    let [lo, hi] = math::bounds(positions.iter().copied());
    if fit.fit {
        let [w, d] = field.grid.size();
        let span = (hi[0] - lo[0]).max(hi[2] - lo[2]).max(1e-6);
        let k = w.min(d) / span;
        for p in &mut positions {
            *p = [
                (p[0] - lo[0]) * k + field.grid.origin[0],
                (p[1] - lo[1]) * k + fit.floor,
                (p[2] - lo[2]) * k + field.grid.origin[1],
            ];
        }
    }
    let tree = PickTree::build(&positions, &indices);
    let top = math::bounds(positions.iter().copied())[1][1] as f64 + 10.;
    let g = field.grid;
    let [lo_h, hi_h] = field.limits;
    let mut covered = 0;
    for z in 0..g.depth {
        for x in 0..g.width {
            let [wx, wz] = g.world(x, z);
            let ray = Ray {
                origin: [f64::from(wx), top, f64::from(wz)],
                direction: [0., -1., 0.],
            };
            // Nearest hit from above = the visible top surface; overhangs
            // and caves below it are left for cliff meshes.
            let h = match tree.hit(&positions, &indices, ray, f64::MAX, |_| true) {
                Some(hit) => {
                    covered += 1;
                    hit.point[1]
                }
                None => fit.floor,
            };
            field.heights[g.index(x, z)] = h.clamp(lo_h, hi_h);
        }
    }
    Ok(MeshReport {
        triangles: indices.len() / 3,
        covered,
        samples: g.len(),
    })
}

fn extension(path: &Path) -> String {
    path.extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase()
}

#[cfg(test)]
mod tests {
    use super::{
        MeshFit, Units16, export_heightmap, import_heightmap, import_mesh, read_heightmap,
    };
    use crate::heightfield::{Grid, Heightfield};

    const UNITS: Units16 = Units16 {
        per_metre: 256.,
        zero: 32768.,
    };

    fn field(side: usize) -> Heightfield {
        let grid = Grid {
            width: side,
            depth: side,
            spacing: 0.5,
            origin: [0., 0.],
        };
        let mut f = Heightfield::new(
            grid,
            (0..side * side)
                .map(|i| (i % side) as f32 * 0.25 - 3.)
                .collect(),
        );
        f.limits = [-128., 128.];
        f
    }

    #[test]
    fn png_and_raw_heightmaps_roundtrip_in_their_units() {
        let dir = tempfile::tempdir().unwrap();
        for name in ["h.png", "h.r16"] {
            let original = field(33);
            let path = dir.path().join(name);
            export_heightmap(&original, &path, UNITS).unwrap();
            let mut back = field(33);
            back.heights.iter_mut().for_each(|h| *h = 0.);
            import_heightmap(&mut back, &path, UNITS, [0., 1.]).unwrap();
            for (a, b) in original.heights.iter().zip(&back.heights) {
                assert!((a - b).abs() <= 1. / 256., "{name}: {a} vs {b}");
            }
            // A smaller target resamples.
            let mut small = field(17);
            import_heightmap(&mut small, &path, UNITS, [0., 1.]).unwrap();
            assert!((small.at(16, 0) - original.at(32, 0)).abs() < 0.01);
        }
        std::fs::write(dir.path().join("bad.r16"), [0u8; 6]).unwrap();
        assert!(
            import_heightmap(&mut field(5), &dir.path().join("bad.r16"), UNITS, [0., 1.]).is_err()
        );
    }

    #[test]
    fn r32_and_eight_bit_images_import_in_metres() {
        let dir = tempfile::tempdir().unwrap();
        let raw: Vec<u8> = (0..9).flat_map(|i| (i as f32 * 2.).to_le_bytes()).collect();
        std::fs::write(dir.path().join("h.r32"), raw).unwrap();
        let mut f = field(3);
        import_heightmap(&mut f, &dir.path().join("h.r32"), UNITS, [0., 1.]).unwrap();
        assert_eq!(f.at(2, 2), 16.);
        let grey = image::GrayImage::from_fn(3, 3, |x, _| image::Luma([(x * 255 / 2) as u8]));
        grey.save(dir.path().join("h8.png")).unwrap();
        import_heightmap(&mut f, &dir.path().join("h8.png"), UNITS, [-10., 10.]).unwrap();
        assert_eq!((f.at(0, 1), f.at(2, 1)), (-10., 10.));
    }

    #[test]
    fn malformed_heightmaps_are_rejected_and_leave_the_field_alone() {
        let dir = tempfile::tempdir().unwrap();
        let nan: Vec<u8> = (0..9)
            .flat_map(|i| if i == 4 { f32::NAN } else { 1. }.to_le_bytes())
            .collect();
        let cases: [(&str, Vec<u8>); 4] = [
            // A trailing byte used to be dropped, loading these as 2×2.
            ("odd.r16", vec![0; 9]),
            ("odd.r32", vec![0; 17]),
            ("nan.r32", nan),
            ("short.r16", vec![0; 2]),
        ];
        for (name, bytes) in cases {
            let path = dir.path().join(name);
            std::fs::write(&path, bytes).unwrap();
            let mut f = field(5);
            let before = f.heights.clone();
            assert!(
                import_heightmap(&mut f, &path, UNITS, [0., 1.]).is_err(),
                "{name}"
            );
            assert_eq!(f.heights, before, "{name}");
        }
        let good = dir.path().join("good.r16");
        std::fs::write(&good, [0u8; 8]).unwrap();
        for units in [
            Units16 {
                per_metre: 0.,
                zero: 0.,
            },
            Units16 {
                per_metre: f32::NAN,
                zero: 0.,
            },
            Units16 {
                per_metre: 1.,
                zero: f32::INFINITY,
            },
        ] {
            assert!(read_heightmap(&good, units, [0., 1.]).is_err(), "{units:?}");
        }
        assert!(read_heightmap(&good, UNITS, [0., f32::NAN]).is_err());
        assert_eq!(read_heightmap(&good, UNITS, [0., 1.]).unwrap().width, 2);
    }

    #[test]
    fn oversized_raw_heightmaps_are_refused_before_reading() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("huge.r16");
        // Sparse: the size check must not need the bytes.
        let side = 8194u64;
        std::fs::File::create(&path)
            .unwrap()
            .set_len(side * side * 2)
            .unwrap();
        let error = read_heightmap(&path, UNITS, [0., 1.]).unwrap_err();
        assert!(format!("{error}").contains("8193 × 8193"), "{error}");
    }

    #[test]
    fn height_images_sample_their_corners_exactly() {
        let dir = tempfile::tempdir().unwrap();
        let raw: Vec<u8> = [1f32, 2., 3., 4.]
            .iter()
            .flat_map(|v| v.to_le_bytes())
            .collect();
        std::fs::write(dir.path().join("h.r32"), raw).unwrap();
        let image = read_heightmap(&dir.path().join("h.r32"), UNITS, [0., 1.]).unwrap();
        assert_eq!(
            [
                image.sample(0., 0.),
                image.sample(1., 0.),
                image.sample(0., 1.),
                image.sample(1., 1.)
            ],
            [1., 2., 3., 4.]
        );
        assert_eq!(image.sample(0.5, 0.5), 2.5);
        assert_eq!(image.sample(-1., 2.), 3.);
    }

    #[test]
    fn obj_import_casts_down_onto_the_mesh() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ramp.obj");
        // A ramp from y=0 at x=0 to y=10 at x=10 over a 10×10 square.
        std::fs::write(
            &path,
            "v 0 0 0\nv 10 10 0\nv 10 10 10\nv 0 0 10\nf 1 2 3\nf 1 3 4\n",
        )
        .unwrap();
        let mut f = field(33); // 16 m × 16 m
        let report = import_mesh(&mut f, &path, MeshFit::default()).unwrap();
        assert_eq!(report.triangles, 2);
        assert_eq!(report.covered, report.samples, "fit fills the grid");
        // Fitted to 16 m: height rises 1 m per metre along X.
        assert!(
            (f.height([8., 8.]) - 8.).abs() < 0.05,
            "{}",
            f.height([8., 8.])
        );
    }
}
