// Owns the sky cube maps ambient presets name (clouds, stars, prefiltered
// light): read once from the install at a size the editor can show,
// shared between presets, and sampled on the CPU as the shaders sample them.
use anno_formats::vfs::Install;
use anyhow::{Context, Result, bail, ensure};
use gpui_editor_kit::texture::{Dds, Texture};
use std::{collections::HashMap, sync::Arc};

/// Cube faces are read at this side or the next mip below; the sky's maps
/// cannot show more.
const CUBE_SIDE: u32 = 256;

/// A cube map, linear RGBA per face, in Direct3D's face order and
/// orientation (+X, -X, +Y, -Y, +Z, -Z).
#[derive(Debug)]
pub struct Cube {
    side: u32,
    faces: [Vec<[f32; 4]>; 6],
}

// Cubes are shared between presets and never change once read: the same
// cube is the same texture.
impl PartialEq for Cube {
    fn eq(&self, other: &Self) -> bool {
        std::ptr::eq(self, other)
    }
}

impl Cube {
    /// Texels per face side.
    pub fn side(&self) -> u32 {
        self.side
    }

    /// The faces' texels, row by row, in Direct3D's order.
    pub fn faces(&self) -> &[Vec<[f32; 4]>; 6] {
        &self.faces
    }

    /// Bilinear within the face direction `d` points into, as
    /// SAMPLETEXTURECUBE with a clamping sampler.
    pub fn sample(&self, d: [f32; 3]) -> [f32; 4] {
        let [x, y, z] = d;
        let (ax, ay, az) = (x.abs(), y.abs(), z.abs());
        let (face, sc, tc, ma) = if ax >= ay && ax >= az {
            if x > 0. {
                (0, -z, -y, ax)
            } else {
                (1, z, -y, ax)
            }
        } else if ay >= az {
            if y > 0. {
                (2, x, z, ay)
            } else {
                (3, x, -z, ay)
            }
        } else if z > 0. {
            (4, x, -y, az)
        } else {
            (5, -x, -y, az)
        };
        let ma = ma.max(1e-12);
        let n = self.side as f32;
        let u = ((sc / ma + 1.) * 0.5 * n - 0.5).clamp(0., n - 1.);
        let v = ((tc / ma + 1.) * 0.5 * n - 0.5).clamp(0., n - 1.);
        let (x0, y0) = (u.floor() as usize, v.floor() as usize);
        let (x1, y1) = ((x0 + 1).min(n as usize - 1), (y0 + 1).min(n as usize - 1));
        let (fx, fy) = (u - x0 as f32, v - y0 as f32);
        let texels = &self.faces[face];
        let at = |x: usize, y: usize| texels[y * self.side as usize + x];
        let (a, b, c, e) = (at(x0, y0), at(x1, y0), at(x0, y1), at(x1, y1));
        std::array::from_fn(|i| {
            let top = a[i] + (b[i] - a[i]) * fx;
            let bottom = c[i] + (e[i] - c[i]) * fx;
            top + (bottom - top) * fy
        })
    }

    /// The cube at `path`, its faces at [`CUBE_SIDE`] or below. Color cubes
    /// (block compressed, sampled with GammaToLinear in the shaders) decode
    /// with gamma 2.2; the prefiltered diffuse cubes are half floats, linear
    /// as stored. DLC skies keep their six faces side by side in one 2D
    /// texture, in the same order.
    pub fn read(install: &Install, path: &str) -> Result<Self> {
        let header = install.read_range(path, 0, 148)?;
        ensure!(
            header.len() >= 128 && header.starts_with(b"DDS "),
            "{path}: not a DDS file"
        );
        let u32_at = |at: usize| u32::from_le_bytes(header[at..at + 4].try_into().unwrap());
        let (height, width, mips) = (u32_at(12), u32_at(16), u32_at(28).max(1));
        let cube = u32_at(112) & 0xfe00 == 0xfe00;
        // D3DFMT_A16B16G16R16F, which Dds::parse does not take.
        if u32_at(84) == 113 {
            ensure!(
                cube && width == height,
                "{path}: half-float DDS is not a cube"
            );
            let face_bytes = (width * height * 8) as usize;
            let chain: usize = (0..mips).map(|m| face_bytes >> (2 * m)).sum();
            let data = install.read_range(path, 128, (chain * 6) as u64)?;
            ensure!(data.len() >= chain * 6, "{path}: truncated");
            let faces = std::array::from_fn(|f| {
                let (texels, _) = data[f * chain..f * chain + face_bytes].as_chunks::<8>();
                texels
                    .iter()
                    .map(|t| {
                        std::array::from_fn(|c| half(u16::from_le_bytes([t[2 * c], t[2 * c + 1]])))
                    })
                    .collect()
            });
            return Ok(Self { side: width, faces });
        }
        let dds = Dds::parse(&header).with_context(|| path.to_owned())?;
        let gpui_editor_kit::texture::Format::Block(format) = dds.format else {
            bail!("{path}: sky cube is not block compressed");
        };
        let data_at = if dds.dxgi.is_some() { 148 } else { 128 };
        let side = if cube { width } else { height };
        ensure!(
            (cube && width == height) || width == 6 * height,
            "{path}: {width}x{height} is neither a cube nor six faces in a row"
        );
        let mip = (0..mips)
            .find(|m| side >> m <= CUBE_SIDE)
            .unwrap_or(mips - 1);
        let size = |m: u32| format.size((width >> m).max(1), (height >> m).max(1));
        let before: usize = (0..mip).map(size).sum();
        let chain: usize = (0..mips).map(size).sum();
        let (w, h) = ((width >> mip).max(1), (height >> mip).max(1));
        let decode = |at: usize| -> Result<Texture> {
            let bytes = install.read_range(path, (data_at + at) as u64, size(mip) as u64)?;
            Texture::decode(format, w, h, &bytes)
        };
        let to_linear = |rgba: &[u8; 4]| -> [f32; 4] {
            let c = |v: u8| (v as f32 / 255.).powf(2.2);
            [c(rgba[0]), c(rgba[1]), c(rgba[2]), rgba[3] as f32 / 255.]
        };
        let faces: [Vec<[f32; 4]>; 6] = if cube {
            let mut out: [Vec<[f32; 4]>; 6] = Default::default();
            for (f, face) in out.iter_mut().enumerate() {
                let t = decode(f * chain + before)?;
                *face = t.rgba.as_chunks::<4>().0.iter().map(to_linear).collect();
            }
            out
        } else {
            let t = decode(before)?;
            std::array::from_fn(|f| {
                (0..h as usize)
                    .flat_map(|y| {
                        let row = (y * w as usize + f * h as usize) * 4;
                        t.rgba[row..row + h as usize * 4]
                            .as_chunks::<4>()
                            .0
                            .iter()
                            .map(to_linear)
                    })
                    .collect()
            })
        };
        Ok(Self { side: h, faces })
    }
}

fn half(bits: u16) -> f32 {
    let sign = if bits & 0x8000 != 0 { -1. } else { 1. };
    let exp = (bits >> 10) & 0x1f;
    let frac = (bits & 0x3ff) as f32;
    sign * match exp {
        0 => frac * 2f32.powi(-24),
        31 => f32::INFINITY,
        e => (1. + frac / 1024.) * 2f32.powi(e as i32 - 15),
    }
}

/// Cubes read so far, by path, so presets naming the same textures share
/// them while a list of presets is read.
#[derive(Default)]
pub struct Cubes(HashMap<String, Option<Arc<Cube>>>);

impl Cubes {
    pub fn get(&mut self, install: &Install, path: &str) -> Option<Arc<Cube>> {
        let path = path.trim().replace('\\', "/");
        if path.is_empty() {
            return None;
        }
        self.0
            .entry(path.to_ascii_lowercase())
            .or_insert_with(|| {
                Cube::read(install, &path)
                    .map_err(|e| eprintln!("sky: {e:#}"))
                    .ok()
                    .map(Arc::new)
            })
            .clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cube_faces_follow_direct3d() {
        // Each face a flat color: its index.
        let side = 2;
        let cube = Cube {
            side,
            faces: std::array::from_fn(|f| vec![[f as f32; 4]; (side * side) as usize]),
        };
        let face = |d: [f32; 3]| cube.sample(d)[0] as usize;
        assert_eq!(
            [
                [1., 0., 0.],
                [-1., 0., 0.],
                [0., 1., 0.],
                [0., -1., 0.],
                [0., 0., 1.],
                [0., 0., -1.]
            ]
            .map(face),
            [0, 1, 2, 3, 4, 5]
        );
    }

    #[test]
    fn half_floats_decode() {
        assert_eq!(half(0x3c00), 1.);
        assert_eq!(half(0xc000), -2.);
        assert_eq!(half(0x3800), 0.5);
        assert_eq!(half(0), 0.);
    }
}
