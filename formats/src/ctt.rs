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

use crate::adf::Adf;
use crate::island::Grid;

/// Texels per page side without the border (rd3d `PageSize`).
pub const PAGE_TEXELS: usize = 512;
/// Extra texels on each side (rd3d `HeightPageBorderTexels`).
pub const BORDER: usize = 8;
/// Full page side, border included.
pub const SIDE: usize = PAGE_TEXELS + 2 * BORDER;
const DOWNSAMPLED: usize = SIDE / 8;

// (ScaleFactor, QuantBits) per delta level, finest first. Same in all 666
// shipped pages; steps are 1/256 m everywhere, ranges +-0.125 / 0.25 / 0.5 m.
const DELTAS: [(f32, u32); 3] = [(2048., 6), (1024., 7), (512., 8)];

/// One page in its stored pyramid form.
#[derive(Debug, Clone, PartialEq)]
pub struct HeightPage {
    /// 66² unorm16, row-major (row along z).
    pub downsampled: Vec<u16>,
    /// Quantised deltas at 528², 264², 132².
    pub deltas: [Vec<u8>; 3],
}

const NODE_NAMES: &[u8] =
    b"\x04\0\0\0\x02\0\x03\0\x04\0\x05\0HeightMap\0Downsampled\0Deltas\0Data\0\0\0";
const VALUE_NAMES: &[u8] = b"\x05\0\0\0\x01\x80\x04\x80\x02\x80\x03\x80\x05\x80\
width\0ScaleFactor\0height\0map\0QuantBits\0\0\0\0";
// Record ids bound by those dictionaries; delta entries are unnamed (1).
const ENTRY: u16 = 1;
const HEIGHT_MAP: u16 = 2;
const DOWN: u16 = 3;
const DELTA_LIST: u16 = 4;
const DATA: u16 = 5;
const WIDTH: u16 = 0x8001;
const HEIGHT: u16 = 0x8002;
const MAP: u16 = 0x8003;
const SCALE: u16 = 0x8004;
const QUANT: u16 = 0x8005;

fn record(out: &mut Vec<u8>, id: u16, payload: &[u8]) {
    out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    out.extend_from_slice(&u32::from(id).to_le_bytes());
    out.extend_from_slice(payload);
    out.resize(out.len() + (8 - payload.len() % 8) % 8, 0);
}

fn delta_side(k: usize) -> usize {
    SIDE >> k
}

impl HeightPage {
    pub fn parse(bytes: &[u8]) -> Result<HeightPage> {
        ensure!(
            bytes.len() >= 8 && &bytes[..4] == b"_CTT",
            "not a _CTT file"
        );
        let size = u32::from_le_bytes(bytes[4..8].try_into().expect("four bytes")) as usize;
        let mut adf = Vec::with_capacity(size);
        flate2::read::ZlibDecoder::new(&bytes[8..])
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
        let adf = Adf::decode(bytes).context("ctt")?;
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
            w == DOWNSAMPLED && down.len() == w * w * 2,
            "ctt Downsampled is {w}²"
        );
        let list = adf.child_node(hm, "Deltas").context("ctt: no Deltas")?;
        let mut deltas: [Vec<u8>; 3] = Default::default();
        let mut k = 0;
        for entry in adf.children(list) {
            ensure!(k < 3, "ctt has more than 3 delta levels");
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
            // QuantBits 8 is the default and elided; the encoder relies on
            // every page using the shipped parameters.
            if (scale, bits.unwrap_or(8)) != (Some(DELTAS[k].0), DELTAS[k].1) {
                bail!("ctt delta {k}: ScaleFactor {scale:?} QuantBits {bits:?} not supported");
            }
            deltas[k] = data.to_vec();
            k += 1;
        }
        ensure!(k == 3, "ctt has {k} delta levels");
        Ok(HeightPage {
            downsampled: down
                .as_chunks::<2>()
                .0
                .iter()
                .map(|&b| u16::from_le_bytes(b))
                .collect(),
            deltas,
        })
    }

    fn encode_adf(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(SIDE * SIDE * 2);
        let u32b = |v: u32| v.to_le_bytes();
        record(&mut out, HEIGHT_MAP, &[]);
        record(&mut out, DOWN, &[]);
        record(&mut out, WIDTH, &u32b(DOWNSAMPLED as u32));
        record(&mut out, HEIGHT, &u32b(DOWNSAMPLED as u32));
        let down: Vec<u8> = self
            .downsampled
            .iter()
            .flat_map(|v| v.to_le_bytes())
            .collect();
        record(&mut out, MAP, &down);
        record(&mut out, 0, &[]);
        record(&mut out, DELTA_LIST, &[]);
        for (k, data) in self.deltas.iter().enumerate() {
            let side = u32b(delta_side(k) as u32);
            record(&mut out, ENTRY, &[]);
            record(&mut out, DATA, &[]);
            record(&mut out, WIDTH, &side);
            record(&mut out, HEIGHT, &side);
            record(&mut out, MAP, data);
            record(&mut out, 0, &[]);
            record(&mut out, SCALE, &DELTAS[k].0.to_le_bytes());
            if DELTAS[k].1 != 8 {
                record(&mut out, QUANT, &u32b(DELTAS[k].1));
            }
            record(&mut out, 0, &[]);
        }
        for _ in 0..3 {
            record(&mut out, 0, &[]); // Deltas, HeightMap, root
        }
        let dict_a = out.len() as u32;
        out.extend_from_slice(NODE_NAMES);
        let dict_b = out.len() as u32;
        out.extend_from_slice(VALUE_NAMES);
        // 2 + 1 + 3 × 2 openers, 3 + 3 × 3 + 3 + 2 values, + 1.
        for w in [0, 27, dict_a, dict_b, 8, -3i32 as u32] {
            out.extend_from_slice(&w.to_le_bytes());
        }
        out
    }

    /// Encodes the `.ctt` file. Shipped files use zlib level 1 (`78 01`).
    pub fn encode(&self) -> Vec<u8> {
        let adf = self.encode_adf();
        Self::compress(&adf)
    }

    pub(crate) fn encode_like(&self, template: &Adf) -> Vec<u8> {
        let adf = Adf::decode(&self.encode_adf()).expect("generated height page");
        Self::compress(&adf.encode_like(template))
    }

    fn compress(adf: &[u8]) -> Vec<u8> {
        let mut z = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::fast());
        z.write_all(adf).expect("in-memory zlib");
        let body = z.finish().expect("in-memory zlib");
        let mut out = Vec::with_capacity(body.len() + 8);
        out.extend_from_slice(b"_CTT");
        out.extend_from_slice(&(adf.len() as u32).to_le_bytes());
        out.extend_from_slice(&body);
        out
    }

    /// Synthesised page heights in metres, `SIDE`² (csSynthesize).
    pub fn heights(&self) -> Grid<f32> {
        let mut cur = Grid {
            width: DOWNSAMPLED,
            height: DOWNSAMPLED,
            data: self
                .downsampled
                .iter()
                .map(|&v| f32::from(v) / 65535.)
                .collect(),
        };
        for k in (0..3).rev() {
            let (scale, bits) = DELTAS[k];
            let half = (1u32 << (bits - 1)) as f32;
            let mut up = upsample(&cur, delta_side(k));
            for (u, &d) in up.data.iter_mut().zip(&self.deltas[k]) {
                *u += (f32::from(d) - half) / (scale * half);
            }
            cur = up;
        }
        cur.data.iter_mut().for_each(|v| *v = *v * 256. - 128.);
        cur
    }

    /// Encodes page heights (metres, `SIDE`²) into the pyramid: bicubic
    /// downsamples (csDownsample), then deltas against the reconstructed
    /// coarser level so quantisation does not drift. Deltas beyond the
    /// format's range (steep cliffs) clamp, as in shipped pages.
    pub fn from_heights(heights: &Grid<f32>) -> HeightPage {
        assert_eq!((heights.width, heights.height), (SIDE, SIDE), "page size");
        let mut chain = vec![Grid {
            width: SIDE,
            height: SIDE,
            data: heights.data.iter().map(|h| (h + 128.) / 256.).collect(),
        }];
        for _ in 0..3 {
            chain.push(downsample(chain.last().expect("one level")));
        }
        let downsampled: Vec<u16> = chain[3]
            .data
            .iter()
            .map(|v| (v * 65535.).round().clamp(0., 65535.) as u16)
            .collect();
        let mut rec = Grid {
            width: DOWNSAMPLED,
            height: DOWNSAMPLED,
            data: downsampled.iter().map(|&v| f32::from(v) / 65535.).collect(),
        };
        let mut deltas: [Vec<u8>; 3] = Default::default();
        for k in (0..3).rev() {
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

/// `x` and `z` of a `<x>x<z>.ctt` file name.
pub fn page_coords(file: &str) -> Option<(u32, u32)> {
    let stem = file.rsplit('/').next()?.strip_suffix(".ctt")?;
    let (x, z) = stem.split_once('x')?;
    Some((x.parse().ok()?, z.parse().ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vfs::Install;

    const PIRATE: &str = "data/base/provinces/roman/islands/3rdparty/roman_island_3rdparty_pirate_01/_gamedata/roman_island_3rdparty_pirate_01";

    fn install() -> Option<Install> {
        let Some(dir) = std::env::var_os("ANNO117_DIR") else {
            eprintln!("skipping: ANNO117_DIR unset");
            return None;
        };
        Some(Install::open(dir).unwrap())
    }

    #[test]
    fn page_names_parse() {
        assert_eq!(page_coords("a/normalmaps/0/10x12.ctt"), Some((10, 12)));
        assert_eq!(page_coords("a/meshes/0/1x2.tmc"), None);
    }

    // The inflated ADF of every pirate_01 page re-encodes byte-exactly, and
    // our file reads back to the same pyramid.
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
                flate2::read::ZlibDecoder::new(&bytes[8..])
                    .read_to_end(&mut adf)
                    .unwrap();
                assert!(
                    page.encode_adf() == adf,
                    "{} differs",
                    install.file_path(file)
                );
                assert_eq!(HeightPage::parse(&page.encode()).unwrap(), page);
                pages += 1;
            }
        }
        assert_eq!(pages, 176);
    }

    // Level 5 is one page over the whole 320 m island at 1 texel per metre:
    // texel BORDER + i sits at x = i m and matches the 0.5 m heightfield.
    #[test]
    fn coarse_page_follows_the_heightfield() {
        let Some(install) = install() else { return };
        let page = HeightPage::parse(
            &install
                .read(&format!("{PIRATE}/normalmaps/5/0x0.ctt"))
                .unwrap(),
        )
        .unwrap();
        let h = page.heights();
        let a7m = crate::rda::Archive::from_bytes(
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
            crate::island::Island::parse(&read("gamedata.data"), &read("rd3d.data")).unwrap();
        let mut diffs: Vec<f32> = (0..320 * 320)
            .map(|i| {
                let (x, z) = (i % 320, i / 320);
                (h.get(BORDER + x, BORDER + z) - island.heights().get(2 * x, 2 * z)).abs()
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
