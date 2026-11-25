// Terrain height pages: `_gamedata/<island>/normalmaps/<level>/<x>x<z>.ctt`.
// Absolute heights at 32 texels per metre on level 0, halving per level,
// from which the game synthesises the per-pixel terrain normal map and the
// tessellation height offset (`shaders/fx/terrain_compress.fx`). They follow
// the terrain, so a height edit has to rewrite the pages it touches.
//
// File: `_CTT`, u32 inflated size, zlib stream of an ADF
// `HeightMap { Downsampled { width, height, map }, Deltas { 3 × { Data {
// width, height, map }, ScaleFactor, QuantBits } } }`. `Downsampled` is 66²
// unorm16 heights (`h = unorm * 256 - 128` m); each delta level is a u8 map
// at 132², 264², 528² added to the bicubic upsample of the level below. A
// page is 512 texels plus an 8-texel border on every side.

use std::io::{Read, Write};

use anyhow::{Context, Result, bail, ensure};

use crate::a7m::island::Grid;
use crate::adf::Adf;

// (ScaleFactor, QuantBits) per delta level, finest first. Same in all 666
// shipped pages; steps are 1/256 m everywhere, ranges +-0.125 / 0.25 / 0.5 m.
const DELTAS: [(f32, u32); 3] = [(2048., 6), (1024., 7), (512., 8)];

/// One page in its stored pyramid form.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct HeightPage {
    /// 66² unorm16, row-major (row along z).
    pub(crate) downsampled: Vec<u16>,
    /// Quantised deltas at 528², 264², 132².
    deltas: [Vec<u8>; 3],
}

// The dictionary listing of every shipped page; a new page lists its names
// the same way. Ids follow first use.
const NODE_NAMES: [&str; 4] = ["HeightMap", "Downsampled", "Deltas", "Data"];
const VALUE_NAMES: [&str; 5] = ["width", "ScaleFactor", "height", "map", "QuantBits"];
/// `QuantBits` when a delta level leaves it out.
const DEFAULT_QUANT_BITS: u32 = 8;

// The file: `MAGIC`, the inflated ADF's size as a u32, then the zlib stream.
const MAGIC: &[u8; 4] = b"_CTT";
const HEADER: usize = MAGIC.len() + size_of::<u32>();

// Heights are stored as unorm16 over a fixed range: `HEIGHT_FLOOR` m at 0,
// `HEIGHT_FLOOR + HEIGHT_SPAN` m at 1.
const HEIGHT_FLOOR: f32 = -128.;
const HEIGHT_SPAN: f32 = 256.;
const UNORM16: f32 = u16::MAX as f32;

fn delta_side(k: usize) -> usize {
    HeightPage::SIDE >> k
}

impl HeightPage {
    /// Texels per page side without the border (rd3d `PageSize`).
    pub(crate) const TEXELS: usize = 512;

    /// Extra texels on each side (rd3d `HeightPageBorderTexels`).
    pub(crate) const BORDER: usize = 8;

    /// Full page side, border included.
    pub(crate) const SIDE: usize = HeightPage::TEXELS + 2 * HeightPage::BORDER;

    const DOWNSAMPLED: usize = HeightPage::SIDE / 8;

    pub(crate) fn parse(bytes: &[u8]) -> Result<HeightPage> {
        ensure!(
            bytes.len() >= HEADER && bytes.starts_with(MAGIC),
            "not a _CTT file"
        );
        let size = u32::from_le_bytes(bytes[MAGIC.len()..HEADER].try_into().expect("four bytes"));
        let size = size as usize;
        let mut adf = Vec::with_capacity(size);
        flate2::read::ZlibDecoder::new(&bytes[HEADER..])
            .read_to_end(&mut adf)
            .context("ctt inflate")?;
        ensure!(
            adf.len() == size,
            "ctt inflates to {} B, header says {size}",
            adf.len()
        );
        Self::parse_adf(&adf)
    }

    fn parse_adf(bytes: &[u8]) -> Result<HeightPage> {
        let adf = Adf::parse(bytes).context("ctt")?;
        let hm = adf
            .child_node(adf.root(), "HeightMap")
            .context("ctt: no HeightMap")?;
        let map = |node| -> Result<(usize, &[u8])> {
            let get = |n: &str| {
                adf.child_value(node, n)
                    .with_context(|| format!("ctt: no {n}"))
            };
            let (w, h) = (adf.read_u32(get("width")?)?, adf.read_u32(get("height")?)?);
            ensure!(w == h, "ctt map is {w}x{h}");
            Ok((w as usize, adf.payload(get("map")?)))
        };
        let (w, down) = map(adf
            .child_node(hm, "Downsampled")
            .context("ctt: no Downsampled")?)?;
        ensure!(
            w == HeightPage::DOWNSAMPLED && down.len() == w * w * size_of::<u16>(),
            "ctt Downsampled is {w}²"
        );
        let list = adf.child_node(hm, "Deltas").context("ctt: no Deltas")?;
        let mut deltas: [Vec<u8>; DELTAS.len()] = Default::default();
        let mut k = 0;
        for entry in adf.children(list) {
            ensure!(
                k < DELTAS.len(),
                "ctt has more than {} delta levels",
                DELTAS.len()
            );
            let (w, data) = map(adf.child_node(entry, "Data").context("ctt: no Data")?)?;
            ensure!(
                w == delta_side(k) && data.len() == w * w,
                "ctt delta {k} is {w}²"
            );
            let scale = adf
                .child_value(entry, "ScaleFactor")
                .map(|v| adf.read_f32(v))
                .transpose()?;
            let bits = adf
                .child_value(entry, "QuantBits")
                .map(|v| adf.read_u32(v))
                .transpose()?;
            // The default QuantBits is elided; the encoder relies on every
            // page using the shipped parameters.
            if (scale, bits.unwrap_or(DEFAULT_QUANT_BITS)) != (Some(DELTAS[k].0), DELTAS[k].1) {
                bail!("ctt delta {k}: ScaleFactor {scale:?} QuantBits {bits:?} not supported");
            }
            deltas[k] = data.to_vec();
            k += 1;
        }
        ensure!(k == DELTAS.len(), "ctt has {k} delta levels");
        Ok(HeightPage {
            downsampled: down
                .as_chunks()
                .0
                .iter()
                .map(|&b| u16::from_le_bytes(b))
                .collect(),
            deltas,
        })
    }

    /// The page as an ADF tree, laid out as the game's tools write it.
    fn to_adf(&self) -> Adf {
        let mut adf = Adf::new();
        let build = |adf: &mut Adf| -> Result<()> {
            let map = |adf: &mut Adf, parent, side: usize, bytes: &[u8]| -> Result<()> {
                let side = (side as u32).to_le_bytes();
                adf.add_value(parent, Some("width"), &side)?;
                adf.add_value(parent, Some("height"), &side)?;
                adf.add_value(parent, Some("map"), bytes)?;
                Ok(())
            };
            let hm = adf.add_node(adf.root(), Some("HeightMap"))?;
            let down = adf.add_node(hm, Some("Downsampled"))?;
            let bytes: Vec<u8> = (self.downsampled.iter())
                .flat_map(|v| v.to_le_bytes())
                .collect();
            map(adf, down, HeightPage::DOWNSAMPLED, &bytes)?;
            let list = adf.add_node(hm, Some("Deltas"))?;
            for (k, data) in self.deltas.iter().enumerate() {
                let (scale, bits) = DELTAS[k];
                let entry = adf.add_node(list, None)?;
                let node = adf.add_node(entry, Some("Data"))?;
                map(adf, node, delta_side(k), data)?;
                adf.add_value(entry, Some("ScaleFactor"), &scale.to_le_bytes())?;
                if bits != DEFAULT_QUANT_BITS {
                    adf.add_value(entry, Some("QuantBits"), &bits.to_le_bytes())?;
                }
            }
            Ok(())
        };
        build(&mut adf).expect("a new page only takes its own names");
        adf.set_listing(&NODE_NAMES, &VALUE_NAMES);
        adf
    }

    pub(crate) fn encode_like(&self, template: &Adf) -> Vec<u8> {
        Self::compress(&self.to_adf().encode_like(template))
    }

    fn compress(adf: &[u8]) -> Vec<u8> {
        let mut z = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::fast());
        z.write_all(adf).expect("in-memory zlib");
        let body = z.finish().expect("in-memory zlib");
        let mut out = Vec::with_capacity(body.len() + HEADER);
        out.extend_from_slice(MAGIC);
        out.extend_from_slice(&(adf.len() as u32).to_le_bytes());
        out.extend_from_slice(&body);
        out
    }

    /// Synthesised page heights in metres, `HeightPage::SIDE`² (csSynthesize).
    pub(crate) fn heights(&self) -> Grid<f32> {
        let mut cur = Grid {
            width: HeightPage::DOWNSAMPLED,
            height: HeightPage::DOWNSAMPLED,
            data: self
                .downsampled
                .iter()
                .map(|&v| f32::from(v) / UNORM16)
                .collect(),
        };
        for k in (0..DELTAS.len()).rev() {
            let (scale, bits) = DELTAS[k];
            let half = (1u32 << (bits - 1)) as f32;
            let mut up = upsample(&cur, delta_side(k));
            for (u, &d) in up.data.iter_mut().zip(&self.deltas[k]) {
                *u += (f32::from(d) - half) / (scale * half);
            }
            cur = up;
        }
        cur.data
            .iter_mut()
            .for_each(|v| *v = *v * HEIGHT_SPAN + HEIGHT_FLOOR);
        cur
    }

    /// Encodes page heights (metres, `HeightPage::SIDE`²) into the pyramid: bicubic
    /// downsamples (csDownsample), then deltas against the reconstructed
    /// coarser level so quantisation does not drift. Deltas beyond the
    /// format's range (steep cliffs) clamp, as in shipped pages.
    pub(crate) fn from_heights(heights: &Grid<f32>) -> HeightPage {
        assert_eq!(
            (heights.width, heights.height),
            (HeightPage::SIDE, HeightPage::SIDE),
            "page size"
        );
        let mut chain = vec![Grid {
            width: HeightPage::SIDE,
            height: HeightPage::SIDE,
            data: (heights.data.iter())
                .map(|h| (h - HEIGHT_FLOOR) / HEIGHT_SPAN)
                .collect(),
        }];
        for _ in 0..DELTAS.len() {
            chain.push(downsample(chain.last().expect("one level")));
        }
        let downsampled: Vec<u16> = chain[DELTAS.len()]
            .data
            .iter()
            .map(|v| (v * UNORM16).round().clamp(0., UNORM16) as u16)
            .collect();
        let mut rec = Grid {
            width: HeightPage::DOWNSAMPLED,
            height: HeightPage::DOWNSAMPLED,
            data: downsampled
                .iter()
                .map(|&v| f32::from(v) / UNORM16)
                .collect(),
        };
        let mut deltas: [Vec<u8>; DELTAS.len()] = Default::default();
        for k in (0..DELTAS.len()).rev() {
            let (scale, bits) = DELTAS[k];
            let half = 1i32 << (bits - 1);
            let mut up = upsample(&rec, delta_side(k));
            let mut q = Vec::with_capacity(up.data.len());
            for (u, &target) in up.data.iter_mut().zip(&chain[k].data) {
                let v = ((target - *u) * scale * half as f32).round() as i32;
                let v = v.clamp(-half, half - 1);
                *u += v as f32 / (scale * half as f32);
                q.push((v + half) as u8);
            }
            deltas[k] = q;
            rec = up;
        }
        HeightPage {
            downsampled,
            deltas,
        }
    }
}

// Hermite (a = -0.5) bicubic weight, as GetBicubicWeight1D_Hermite.
fn hermite(x: f32) -> f32 {
    let (a, x) = (-0.5f32, x.abs());
    if x <= 1. {
        (a + 2.) * x * x * x - (a + 3.) * x * x + 1.
    } else if x < 2. {
        a * x * x * x - 5. * a * x * x + 8. * a * x - 4. * a
    } else {
        0.
    }
}

// Separable bicubic sampling with mirror-once addressing at coordinates
// `coord(i)` for each output texel, normalised by the weight sum like the
// shader's SampleBicubic.
fn resample(src: &Grid<f32>, side: usize, coord: impl Fn(usize) -> f32) -> Grid<f32> {
    let n = src.width as i32;
    let mirror = |c: i32| {
        let c = c.abs();
        (if c >= n { 2 * (n - 1) - c } else { c }) as usize
    };
    let taps: Vec<([usize; 4], [f32; 4])> = (0..side)
        .map(|i| {
            let c = coord(i);
            let base = c.floor() as i32;
            let idx = std::array::from_fn(|k| mirror(base - 1 + k as i32));
            let mut w: [f32; 4] =
                std::array::from_fn(|k| hermite((base - 1 + k as i32) as f32 - c));
            let sum: f32 = w.iter().sum();
            w.iter_mut().for_each(|v| *v /= sum);
            (idx, w)
        })
        .collect();
    // Rows first, then columns.
    let mut rows = vec![0f32; src.height * side];
    for y in 0..src.height {
        let line = &src.data[y * src.width..(y + 1) * src.width];
        for (x, (idx, w)) in taps.iter().enumerate() {
            rows[y * side + x] = (0..4).map(|k| line[idx[k]] * w[k]).sum();
        }
    }
    let mut data = vec![0f32; side * side];
    for (y, (idx, w)) in taps.iter().enumerate() {
        for x in 0..side {
            data[y * side + x] = (0..4).map(|k| rows[idx[k] * side + x] * w[k]).sum();
        }
    }
    Grid {
        width: side,
        height: side,
        data,
    }
}

fn upsample(src: &Grid<f32>, side: usize) -> Grid<f32> {
    resample(src, side, |i| i as f32 * 0.5 - 0.25)
}

fn downsample(src: &Grid<f32>) -> Grid<f32> {
    resample(src, src.width / 2, |i| i as f32 * 2. + 0.5)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vfs::Install;

    const PIRATE: &str = "data/base/provinces/roman/islands/3rdparty/roman_island_3rdparty_pirate_01/_gamedata/roman_island_3rdparty_pirate_01";

    fn install() -> Option<Install> {
        let Some(dir) = crate::vfs::install_dir("ANNO117_DIR") else {
            eprintln!("skipping: ANNO117_DIR unset");
            return None;
        };
        Some(Install::open(dir, crate::vfs::Install::DATA_FIRST).unwrap())
    }

    // The inflated ADF of every pirate_01 page is what our writer builds
    // from the parsed pyramid, byte for byte, and our file reads back to the
    // same pyramid.
    #[test]
    fn shipped_pages_roundtrip() {
        let Some(install) = install() else { return };
        let maps = install.folder(&format!("{PIRATE}/normalmaps")).unwrap();
        let mut pages = 0;
        for &level in install.subfolders(maps) {
            for &file in install.folder_files(level) {
                let bytes = install.read_file(file).unwrap();
                let page = HeightPage::parse(&bytes).unwrap();
                let mut adf = Vec::new();
                flate2::read::ZlibDecoder::new(&bytes[HEADER..])
                    .read_to_end(&mut adf)
                    .unwrap();
                assert!(
                    page.to_adf().encode() == adf,
                    "{} differs",
                    install.file_path(file)
                );
                let template = Adf::parse(&adf).unwrap();
                assert_eq!(
                    HeightPage::parse(&page.encode_like(&template)).unwrap(),
                    page
                );
                pages += 1;
            }
        }
        assert_eq!(pages, 176);
    }

    // Level 5 is one page over the whole 320 m island at 1 texel per metre:
    // texel HeightPage::BORDER + i sits at x = i m and matches the 0.5 m heightfield.
    #[test]
    fn coarse_page_follows_the_heightfield() {
        let Some(install) = install() else { return };
        let page = HeightPage::parse(
            &install
                .read(format!("{PIRATE}/normalmaps/5/0x0.ctt"))
                .unwrap(),
        )
        .unwrap();
        let h = page.heights();
        let a7m = crate::rda::Archive::parse(
            install
                .read("data/base/provinces/roman/islands/3rdparty/roman_island_3rdparty_pirate_01/roman_island_3rdparty_pirate_01.a7m")
                .unwrap(),
            "a7m",
        )
        .unwrap();
        let read = |m: &str| {
            a7m.read(a7m.entries().iter().find(|e| e.key().ends_with(m)).unwrap())
                .unwrap()
        };
        let island =
            crate::a7m::island::Island::parse(&read("gamedata.data"), &read("rd3d.data")).unwrap();
        let mut diffs: Vec<f32> = (0..320 * 320)
            .map(|i| {
                let (x, z) = (i % 320, i / 320);
                (h.get(HeightPage::BORDER + x, HeightPage::BORDER + z)
                    - island.heights().get(2 * x, 2 * z))
                .abs()
            })
            .collect();
        diffs.sort_by(f32::total_cmp);
        assert!(
            diffs[diffs.len() / 2] < 0.02,
            "median {}",
            diffs[diffs.len() / 2]
        );

        // Re-encoding the synthesised heights: within a quantisation step
        // almost everywhere; cliffs steeper than the delta range clamp (p99
        // is 3.7 cm on this page).
        let again = HeightPage::from_heights(&h).heights();
        let mut err: Vec<f32> = h
            .data
            .iter()
            .zip(&again.data)
            .map(|(a, b)| (a - b).abs())
            .collect();
        err.sort_by(f32::total_cmp);
        let (median, p99) = (err[err.len() / 2], err[err.len() * 99 / 100]);
        assert!(median < 0.004 && p99 < 0.05, "median {median} p99 {p99}");
    }
}
