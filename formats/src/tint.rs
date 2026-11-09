// Island tint maps: the BC1 tiles under `_gamedata/<island>/tintmaps/<level>/
// <x>x<y>.dds`, their luma/chroma decode into an albedo multiplier, and the
// rd3d `TintMapLevels` flags and `BaseTextureData` that mirror the tiles.
//
// A texel stores (Cb, Y, Cr) in (R, G, B), not a color. The terrain shader
// decodes it as
//
//   cb = s(2R - 1), cr = s(2B - 1), s(v) = v * |v|
//   rgb' = (Y + 0.701 cr, Y - 0.172 cb - 0.357 cr, Y + 0.886 cb)
//   multiplier = (2 * max(rgb', 0)) ^ 2.2
//   albedo = saturate(albedo * multiplier)
//
// so (0.5, 0.5, 0.5) is neutral, luma reaches 4.6x at Y = 1 and the chroma
// curve is finer near grey. Images here stay in that encoded space; box
// filtering it is what the shipped mips do.

use anyhow::{Context, Result, ensure};

use crate::adf::{Adf, ItemId};
use crate::game::Game;
use crate::island::Grid;
use crate::vfs::Install;

/// Tile side in texels (rd3d `TintPageSize`).
pub const TILE: usize = 512;
/// Level 0 texels per metre. One level-0 tile is a 16 m page.
pub const LEVEL0_PER_METRE: u32 = 32;
/// BC1 `0x8410` decoded: what shipped tiles hold outside the island and where
/// nothing was painted. It decodes to a 1.04x luma multiplier; 565 cannot hit
/// 0.5 exactly and this is the value the game's tools write.
pub const NEUTRAL: [u8; 3] = [132, 130, 132];

/// One level of an island's tint as a single image, row-major, `y` down the
/// island's height-grid rows (the same orientation as `island::Grid`s).
#[derive(Debug, Clone, PartialEq)]
pub struct TintMap {
    pub level: u32,
    pub per_metre: f32,
    pub width: usize,
    pub height: usize,
    /// Encoded (Cb, Y, Cr) texels as stored; see [`decode`].
    pub texels: Vec<[u8; 3]>,
}

impl TintMap {
    /// All-neutral map of `level` covering a `world` (metres) island.
    pub fn neutral(level: u32, world: [u32; 2]) -> TintMap {
        let (width, height) = level_size(level, world);
        TintMap {
            level,
            per_metre: per_metre(level),
            width,
            height,
            texels: vec![NEUTRAL; width * height],
        }
    }

    /// Map of `level` whose texel at world position (x, z) metres carries the
    /// multiplier `f(x, z)`. Seeds height- or material-driven variation.
    pub fn from_fn(
        level: u32,
        world: [u32; 2],
        mut f: impl FnMut(f32, f32) -> [f32; 3],
    ) -> TintMap {
        let mut map = TintMap::neutral(level, world);
        let step = 1. / map.per_metre;
        for y in 0..map.height {
            for x in 0..map.width {
                let m = f((x as f32 + 0.5) * step, (y as f32 + 0.5) * step);
                map.texels[y * map.width + x] = encode(m);
            }
        }
        map
    }

    /// Reads every tile of `level` from `<gamedata_dir>/tintmaps/<level>/`.
    /// `gamedata_dir` is `data/.../<island>/_gamedata/<island>`. Absent tiles
    /// (open sea) read as neutral.
    pub fn read(
        install: &Install,
        gamedata_dir: &str,
        level: u32,
        world: [u32; 2],
    ) -> Result<TintMap> {
        let dir = format!("{gamedata_dir}/tintmaps/{level}");
        let files = install.list(&dir);
        ensure!(!files.is_empty(), "no tint folder {dir}");
        let mut tiles = Vec::new();
        for path in files {
            let Some(at) = tile_coords(path.rsplit('/').next().unwrap_or_default()) else {
                continue;
            };
            tiles.push((at, install.read(&path)?));
        }
        TintMap::from_tiles(
            level,
            world,
            tiles.iter().map(|(at, b)| (*at, b.as_slice())),
        )
        .with_context(|| format!("tint level {level} of {gamedata_dir}"))
    }

    /// Assembles `((x, y), dds bytes)` tiles into one image.
    pub fn from_tiles<'a>(
        level: u32,
        world: [u32; 2],
        tiles: impl IntoIterator<Item = ((u32, u32), &'a [u8])>,
    ) -> Result<TintMap> {
        let mut map = TintMap::neutral(level, world);
        for ((tx, ty), dds) in tiles {
            let (w, h, texels) = decode_dds(dds).with_context(|| format!("tile {tx}x{ty}"))?;
            ensure!((w, h) == (TILE, TILE), "tile {tx}x{ty} is {w}x{h}");
            let (x0, y0) = (tx as usize * TILE, ty as usize * TILE);
            for row in 0..TILE {
                let y = y0 + row;
                if y >= map.height {
                    break;
                }
                for col in 0..TILE.min(map.width.saturating_sub(x0)) {
                    map.texels[y * map.width + x0 + col] = texels[row * TILE + col];
                }
            }
        }
        Ok(map)
    }

    pub fn get(&self, x: usize, y: usize) -> [u8; 3] {
        self.texels[y * self.width + x]
    }

    /// Linear RGB multipliers, one per texel, for display.
    pub fn multipliers(&self) -> Vec<[f32; 3]> {
        self.texels.iter().map(|&t| decode(t)).collect()
    }

    /// Map from linear RGB multipliers (e.g. a painted color image divided
    /// by the bare albedo, or a user's tint layer). `values` is row-major
    /// `level_size(level, world)`.
    pub fn from_multipliers(level: u32, world: [u32; 2], values: &[[f32; 3]]) -> Result<TintMap> {
        let mut map = TintMap::neutral(level, world);
        ensure!(
            values.len() == map.texels.len(),
            "tint level {level} needs {}x{} values, got {}",
            map.width,
            map.height,
            values.len()
        );
        for (t, &v) in map.texels.iter_mut().zip(values) {
            *t = encode(v);
        }
        Ok(map)
    }

    /// The next coarser level: 2x2 box filter of the encoded texels, which is
    /// how the shipped levels relate (mean error < 1.1 per channel, i.e. BC1
    /// noise, on pirate_01 levels 1-5).
    pub fn downsample(&self) -> TintMap {
        let (w, h) = (self.width.div_ceil(2), self.height.div_ceil(2));
        let mut texels = Vec::with_capacity(w * h);
        for y in 0..h {
            for x in 0..w {
                let mut sum = [0u32; 3];
                for (dx, dy) in [(0, 0), (1, 0), (0, 1), (1, 1)] {
                    let t = self.get(
                        (2 * x + dx).min(self.width - 1),
                        (2 * y + dy).min(self.height - 1),
                    );
                    for c in 0..3 {
                        sum[c] += u32::from(t[c]);
                    }
                }
                texels.push(sum.map(|s| ((s + 2) / 4) as u8));
            }
        }
        TintMap {
            level: self.level + 1,
            per_metre: self.per_metre / 2.,
            width: w,
            height: h,
            texels,
        }
    }

    /// Tiles across and down (the last ones may stick out past the island).
    pub fn tile_count(&self) -> (u32, u32) {
        (
            self.width.div_ceil(TILE) as u32,
            self.height.div_ceil(TILE) as u32,
        )
    }

    /// Tile (tx, ty) as a shipped-layout DDS, neutral past the island edge.
    pub fn tile_dds(&self, tx: u32, ty: u32) -> Vec<u8> {
        let (x0, y0) = (tx as usize * TILE, ty as usize * TILE);
        let texels = tile_texels(|x, y| {
            let (x, y) = (x0 + x, y0 + y);
            if x < self.width && y < self.height {
                self.get(x, y)
            } else {
                NEUTRAL
            }
        });
        encode_dds(TILE, TILE, &texels)
    }

    // Bilinear sample in this map's texel space, edge-clamped.
    fn sample(&self, x: f32, y: f32) -> [u8; 3] {
        let x = x.clamp(0., (self.width - 1) as f32);
        let y = y.clamp(0., (self.height - 1) as f32);
        let (x0, y0) = (x.floor() as usize, y.floor() as usize);
        let (x1, y1) = ((x0 + 1).min(self.width - 1), (y0 + 1).min(self.height - 1));
        let (fx, fy) = (x - x0 as f32, y - y0 as f32);
        let (a, b, c, d) = (
            self.get(x0, y0),
            self.get(x1, y0),
            self.get(x0, y1),
            self.get(x1, y1),
        );
        std::array::from_fn(|i| {
            let top = f32::from(a[i]) * (1. - fx) + f32::from(b[i]) * fx;
            let bottom = f32::from(c[i]) * (1. - fx) + f32::from(d[i]) * fx;
            (top * (1. - fy) + bottom * fy).round() as u8
        })
    }
}

fn per_metre(level: u32) -> f32 {
    LEVEL0_PER_METRE as f32 / (1u32 << level) as f32
}

/// Texels across and down at `level` for a `world` (metres) island.
pub fn level_size(level: u32, world: [u32; 2]) -> (usize, usize) {
    let side = |m: u32| (m as usize * LEVEL0_PER_METRE as usize).div_ceil(1 << level);
    (side(world[0]), side(world[1]))
}

/// `x` and `y` of a `<x>x<y>.dds` tile name: `x` is the column. The game
/// formats it as `"%ls%d/%d%ls%d%ls"` (dir, level, page x, "x", page y,
/// ".dds") in `rd3d::CStreamedTexture::DoActualSplitImageDataLoading`.
pub fn tile_coords(file: &str) -> Option<(u32, u32)> {
    let stem = file.rsplit('/').next()?.strip_suffix(".dds")?;
    let (x, y) = stem.split_once('x')?;
    Some((x.parse().ok()?, y.parse().ok()?))
}

// ------------------------------------------------------------------ encoding

// Chroma weights of the shader's YCbCr inverse with Cb/Cr pre-doubled
// (JPEG's 1.402, 0.344, 0.714, 1.772 halved).
const CR_R: f32 = 0.701;
const CB_G: f32 = 0.172;
const CR_G: f32 = 0.357;
const CB_B: f32 = 0.886;
const GAMMA: f32 = 2.2;

/// Linear RGB multiplier of an encoded texel, exactly as the terrain pixel
/// shader computes it (`UncompressTintColor`).
pub fn decode(texel: [u8; 3]) -> [f32; 3] {
    let unit = |v: u8| f32::from(v) / 255.;
    let signed_sq = |v: f32| {
        let s = 2. * v - 1.;
        s * s.abs()
    };
    let (cb, y, cr) = (
        signed_sq(unit(texel[0])),
        unit(texel[1]),
        signed_sq(unit(texel[2])),
    );
    [y + CR_R * cr, y - CB_G * cb - CR_G * cr, y + CB_B * cb].map(|v| (2. * v.max(0.)).powf(GAMMA))
}

/// Encoded texel for a linear RGB multiplier. Inverse of [`decode`] up to
/// u8 rounding; multipliers above ~4.6 or chroma beyond the curve's reach
/// clamp.
pub fn encode(multiplier: [f32; 3]) -> [u8; 3] {
    let [r, g, b] = multiplier.map(|m| m.max(0.).powf(1. / GAMMA) / 2.);
    // Luma weights that make the shader's 0.172/0.357 green terms consistent.
    let y = 0.299 * r + 0.587 * g + 0.114 * b;
    let (cb, cr) = ((b - y) / CB_B, (r - y) / CR_R);
    let unsigned_sqrt = |v: f32| 0.5 + v.signum() * v.abs().sqrt() / 2.;
    [unsigned_sqrt(cb), y, unsigned_sqrt(cr)].map(|v| (v.clamp(0., 1.) * 255.).round() as u8)
}

// ---------------------------------------------------------------- DDS / BC1

// Every shipped tile carries this header byte for byte: legacy DDS, DXT1,
// 512x512, one mip, pitch field 1024, caps 0x04001000. `encode_dds` patches
// only width, height and pitch.
const DDS_HEADER: [u8; 128] = {
    let mut h = [0u8; 128];
    let words: [(usize, u32); 9] = [
        (0, u32::from_le_bytes(*b"DDS ")),
        (4, 124),
        (8, 0x0008_3007),
        (12, 512),
        (16, 512),
        (20, 1024),
        (28, 1),
        (76, 32),
        (80, 4),
    ];
    let mut i = 0;
    while i < words.len() {
        let (at, v) = words[i];
        let b = v.to_le_bytes();
        h[at] = b[0];
        h[at + 1] = b[1];
        h[at + 2] = b[2];
        h[at + 3] = b[3];
        i += 1;
    }
    h[84] = b'D';
    h[85] = b'X';
    h[86] = b'T';
    h[87] = b'1';
    h[109] = 0x10;
    h[111] = 0x04;
    h
};

/// RGB texels of a DXT1 DDS (any size that is a multiple of 4).
pub fn decode_dds(dds: &[u8]) -> Result<(usize, usize, Vec<[u8; 3]>)> {
    ensure!(dds.len() >= 128 && dds.starts_with(b"DDS "), "not a DDS");
    ensure!(&dds[84..88] == b"DXT1", "tint DDS is not DXT1");
    let word =
        |at: usize| u32::from_le_bytes(dds[at..at + 4].try_into().expect("four bytes")) as usize;
    let (height, width) = (word(12), word(16));
    ensure!(
        width % 4 == 0 && height % 4 == 0,
        "DXT1 {width}x{height} not block aligned"
    );
    let blocks = &dds[128..];
    ensure!(
        blocks.len() >= width * height / 2,
        "DXT1 {width}x{height} is truncated"
    );
    Ok((width, height, decode_blocks(blocks, width, height)))
}

/// DXT1 DDS of `texels` with the shipped tile header.
pub fn encode_dds(width: usize, height: usize, texels: &[[u8; 3]]) -> Vec<u8> {
    let mut out = DDS_HEADER.to_vec();
    out[12..16].copy_from_slice(&(height as u32).to_le_bytes());
    out[16..20].copy_from_slice(&(width as u32).to_le_bytes());
    out[20..24].copy_from_slice(&((width / 4 * 8) as u32).to_le_bytes());
    out.extend(encode_blocks(texels, width, height).iter().flatten());
    out
}

/// Texels of `width` x `height` raw BC1 blocks.
pub fn decode_blocks(blocks: &[u8], width: usize, height: usize) -> Vec<[u8; 3]> {
    let mut out = vec![[0; 3]; width * height];
    for (i, block) in blocks
        .as_chunks::<8>()
        .0
        .iter()
        .take(width * height / 16)
        .enumerate()
    {
        let (bx, by) = (i % (width / 4) * 4, i / (width / 4) * 4);
        let palette = palette(block);
        let bits = u32::from_le_bytes(block[4..8].try_into().expect("four bytes"));
        for p in 0..16 {
            out[(by + p / 4) * width + bx + p % 4] = palette[(bits >> (2 * p) & 3) as usize];
        }
    }
    out
}

/// Raw BC1 blocks (row-major) of a block-aligned image.
///
/// Quality: principal-axis range fit, one least-squares endpoint refinement,
/// always 4-color mode like the shipped tiles. On shipped tiles re-encoded
/// from their own decode the error is ~1 per channel RMS (see tests); fine
/// for a signal that is mostly smooth grey with chroma fringes.
pub fn encode_blocks(texels: &[[u8; 3]], width: usize, height: usize) -> Vec<[u8; 8]> {
    let mut out = Vec::with_capacity(width * height / 16);
    for by in (0..height).step_by(4) {
        for bx in (0..width).step_by(4) {
            let px: [[u8; 3]; 16] =
                std::array::from_fn(|p| texels[(by + p / 4) * width + bx + p % 4]);
            out.push(encode_block(&px));
        }
    }
    out
}

fn tile_texels(f: impl Fn(usize, usize) -> [u8; 3]) -> Vec<[u8; 3]> {
    (0..TILE * TILE).map(|i| f(i % TILE, i / TILE)).collect()
}

fn unpack565(c: u16) -> [u8; 3] {
    let (r, g, b) = ((c >> 11) as u8, (c >> 5 & 63) as u8, (c & 31) as u8);
    [r << 3 | r >> 2, g << 2 | g >> 4, b << 3 | b >> 2]
}

fn pack565(c: [f32; 3]) -> u16 {
    let q = |v: f32, max: f32| (v.clamp(0., 255.) * max / 255.).round() as u16;
    q(c[0], 31.) << 11 | q(c[1], 63.) << 5 | q(c[2], 31.)
}

fn palette(block: &[u8; 8]) -> [[u8; 3]; 4] {
    let c0 = u16::from_le_bytes([block[0], block[1]]);
    let c1 = u16::from_le_bytes([block[2], block[3]]);
    let (a, b) = (unpack565(c0), unpack565(c1));
    let mix = |wa: u16, wb: u16| -> [u8; 3] {
        std::array::from_fn(|i| {
            ((wa * u16::from(a[i]) + wb * u16::from(b[i]) + (wa + wb) / 2) / (wa + wb)) as u8
        })
    };
    if c0 > c1 {
        [a, b, mix(2, 1), mix(1, 2)]
    } else {
        [a, b, mix(1, 1), [0; 3]]
    }
}

fn dist2(a: [u8; 3], b: [u8; 3]) -> u32 {
    (0..3)
        .map(|i| (i32::from(a[i]) - i32::from(b[i])).pow(2) as u32)
        .sum()
}

// Picks the nearest palette entry per texel; returns the block and its error.
fn fit_indices(px: &[[u8; 3]; 16], c0: u16, c1: u16) -> ([u8; 8], u32) {
    let mut block = [0u8; 8];
    block[0..2].copy_from_slice(&c0.to_le_bytes());
    block[2..4].copy_from_slice(&c1.to_le_bytes());
    let pal = palette(&block);
    let (mut bits, mut err) = (0u32, 0u32);
    for (p, &t) in px.iter().enumerate() {
        let (k, e) = (0..4)
            .map(|k| (k, dist2(pal[k], t)))
            .min_by_key(|&(_, e)| e)
            .expect("four entries");
        bits |= (k as u32) << (2 * p);
        err += e;
    }
    block[4..8].copy_from_slice(&bits.to_le_bytes());
    (block, err)
}

// 4-color mode needs c0 > c1. Swapping endpoints keeps the same palette
// (fit_indices re-picks); equal endpoints are nudged apart one step per
// channel, which is what shipped solid blocks do (0x8410 / 0x7bef).
fn order(c0: u16, c1: u16) -> (u16, u16) {
    match c0.cmp(&c1) {
        std::cmp::Ordering::Greater => (c0, c1),
        std::cmp::Ordering::Less => (c1, c0),
        std::cmp::Ordering::Equal if c0 == 0 => (0x0020, 0),
        std::cmp::Ordering::Equal => {
            let (r, g, b) = (c0 >> 11, c0 >> 5 & 63, c0 & 31);
            let lower = r.saturating_sub(1) << 11 | g.saturating_sub(1) << 5 | b.saturating_sub(1);
            (c0, lower)
        }
    }
}

fn encode_block(px: &[[u8; 3]; 16]) -> [u8; 8] {
    let f = |t: [u8; 3]| t.map(f32::from);
    let mean = px.iter().fold([0f32; 3], |m, &t| {
        std::array::from_fn(|i| m[i] + f(t)[i] / 16.)
    });
    let mut cov = [[0f32; 3]; 3];
    for &t in px {
        let d: [f32; 3] = std::array::from_fn(|i| f(t)[i] - mean[i]);
        for i in 0..3 {
            for j in 0..3 {
                cov[i][j] += d[i] * d[j];
            }
        }
    }
    // Power iteration for the principal axis; a flat block keeps the
    // luma diagonal and collapses to a solid color below.
    let mut axis = [1f32, 1., 1.];
    for _ in 0..8 {
        let next: [f32; 3] = std::array::from_fn(|i| (0..3).map(|j| cov[i][j] * axis[j]).sum());
        let len = next.iter().map(|v| v * v).sum::<f32>().sqrt();
        if len < 1e-6 {
            break;
        }
        axis = next.map(|v| v / len);
    }
    let proj = |t: [u8; 3]| (0..3).map(|i| (f(t)[i] - mean[i]) * axis[i]).sum::<f32>();
    let (lo, hi) = px.iter().fold((f32::MAX, f32::MIN), |(lo, hi), &t| {
        let p = proj(t);
        (lo.min(p), hi.max(p))
    });
    let at = |s: f32| -> [f32; 3] { std::array::from_fn(|i| mean[i] + axis[i] * s) };
    let (c0, c1) = order(pack565(at(hi)), pack565(at(lo)));
    let (mut best, best_err) = fit_indices(px, c0, c1);
    if best_err == 0 {
        return best;
    }

    // Least squares: with each texel's weight on c0 fixed by its index,
    // solve for the endpoints that minimise the error, then re-fit.
    let bits = u32::from_le_bytes(best[4..8].try_into().expect("four bytes"));
    let (mut aa, mut ab, mut bb) = (0f32, 0f32, 0f32);
    let (mut ax, mut bx) = ([0f32; 3], [0f32; 3]);
    for (p, &t) in px.iter().enumerate() {
        let w = [1., 0., 2. / 3., 1. / 3.][(bits >> (2 * p) & 3) as usize];
        aa += w * w;
        ab += w * (1. - w);
        bb += (1. - w) * (1. - w);
        for i in 0..3 {
            ax[i] += w * f(t)[i];
            bx[i] += (1. - w) * f(t)[i];
        }
    }
    let det = aa * bb - ab * ab;
    if det.abs() > 1e-6 {
        let e0 = std::array::from_fn(|i| (ax[i] * bb - bx[i] * ab) / det);
        let e1 = std::array::from_fn(|i| (bx[i] * aa - ax[i] * ab) / det);
        let (c0, c1) = order(pack565(e0), pack565(e1));
        let (block, err) = fit_indices(px, c0, c1);
        if err < best_err {
            best = block;
        }
    }
    best
}

// ------------------------------------------------------------ rd3d mirror

/// rd3d `Terrain/TintMapLevels`: one presence flag per tile per level
/// (`flags[level]` is tiles-across x tiles-down, 1 = `<x>x<y>.dds` ships),
/// and `BaseTextureData`, the base level's texels as raw BC1 blocks
/// (`base_width` x `base_height` blocks) that the engine keeps resident
/// for the coarse `cTerrainTintTex`.
#[derive(Debug, Clone, PartialEq)]
pub struct Rd3dTint {
    pub flags: Vec<Grid<u8>>,
    pub base_width: usize,
    pub base_height: usize,
    pub base_blocks: Vec<u8>,
}

fn tint_levels(rd3d: &Adf) -> Result<ItemId> {
    rd3d.node(rd3d.root(), "Terrain/TintMapLevels")
        .context("rd3d: no Terrain/TintMapLevels")
}

// (width, height, map) items of a `{width, height, map}` node.
fn map_items(rd3d: &Adf, node: ItemId) -> Result<(usize, usize, ItemId)> {
    let get = |name: &str| {
        rd3d.child_value(node, name)
            .with_context(|| format!("rd3d tint: no {name}"))
    };
    Ok((
        rd3d.read_u32(get("width")?)? as usize,
        rd3d.read_u32(get("height")?)? as usize,
        get("map")?,
    ))
}

fn level_maps(rd3d: &Adf) -> Result<Vec<(usize, usize, ItemId)>> {
    let levels = rd3d
        .child_node(tint_levels(rd3d)?, "Levels")
        .context("rd3d: no TintMapLevels/Levels")?;
    rd3d.children(levels)
        .filter(|&l| rd3d.is_node(l))
        .map(|l| {
            map_items(
                rd3d,
                rd3d.child_node(l, "Flags").context("rd3d tint: no Flags")?,
            )
        })
        .collect()
}

fn base_map(rd3d: &Adf) -> Result<(usize, usize, ItemId)> {
    let tl = tint_levels(rd3d)?;
    map_items(
        rd3d,
        rd3d.child_node(tl, "BaseTextureData")
            .context("rd3d: no BaseTextureData")?,
    )
}

/// Reads `TintMapLevels` out of rd3d.
pub fn read_rd3d(rd3d: &Adf) -> Result<Rd3dTint> {
    let flags = level_maps(rd3d)?
        .into_iter()
        .map(|(width, height, map)| {
            let data = rd3d.payload(map).to_vec();
            ensure!(
                data.len() == width * height,
                "rd3d tint flags {width}x{height} hold {} bytes",
                data.len()
            );
            Ok(Grid {
                width,
                height,
                data,
            })
        })
        .collect::<Result<_>>()?;
    let (base_width, base_height, map) = base_map(rd3d)?;
    Ok(Rd3dTint {
        flags,
        base_width,
        base_height,
        base_blocks: rd3d.payload(map).to_vec(),
    })
}

/// Writes `tint` into rd3d. Sizes must match what is there (they follow the
/// island's world size, which this does not change).
pub fn write_rd3d(rd3d: &mut Adf, tint: &Rd3dTint) -> Result<()> {
    let maps = level_maps(rd3d)?;
    ensure!(
        maps.len() == tint.flags.len(),
        "rd3d has {} tint levels, bake has {}",
        maps.len(),
        tint.flags.len()
    );
    for (&(w, h, map), flags) in maps.iter().zip(&tint.flags) {
        ensure!(
            (w, h) == (flags.width, flags.height),
            "rd3d tint level is {w}x{h}, bake {}x{}",
            flags.width,
            flags.height
        );
        rd3d.set_payload(map, &flags.data);
    }
    let (w, h, map) = base_map(rd3d)?;
    ensure!(
        (w, h) == (tint.base_width, tint.base_height) && tint.base_blocks.len() == w * h * 8,
        "rd3d BaseTextureData is {w}x{h} blocks, bake {}x{}",
        tint.base_width,
        tint.base_height
    );
    rd3d.set_payload(map, &tint.base_blocks);
    // 1800 also keeps the same resident BC1 image in the older TintsMap.
    // Other generations use a different size there and retain their cache.
    if let Some(cache) = rd3d.node(rd3d.root(), "Terrain/TintsMap") {
        let (cw, ch, bytes) = map_items(rd3d, cache)?;
        if (cw, ch) == (w, h) && rd3d.payload(bytes).len() == tint.base_blocks.len() {
            rd3d.set_payload(bytes, &tint.base_blocks);
        }
    }
    Ok(())
}

/// Tile levels an island of `world` metres ships: each halves the previous
/// resolution until one 512² tile covers the island. Matches every shipped
/// size: 128 m → 4, 192/256 m → 5, 320–512 m → 6, 768 m → 7.
pub fn level_count(world: [u32; 2]) -> u32 {
    let mut level = 0;
    loop {
        let (w, h) = level_size(level, world);
        if w <= TILE && h <= TILE {
            return level + 1;
        }
        level += 1;
    }
}

/// Tile presence per level from the island's `ActiveMap`: a level-0 tile
/// (16 m page) ships when any active cell overlaps it, each coarser level
/// ORs its 2x2 children. Matches the shipped tiles and rd3d flags exactly on
/// pirate_01, small_07 and large_01.
pub fn level_flags(active: &Grid<bool>, world: [u32; 2]) -> Vec<Grid<u8>> {
    let page = (TILE / LEVEL0_PER_METRE as usize) as f64;
    let (w0, h0) = (
        (world[0] as f64 / page).ceil() as usize,
        (world[1] as f64 / page).ceil() as usize,
    );
    let cell = (
        world[0] as f64 / active.width as f64,
        world[1] as f64 / active.height as f64,
    );
    let mut level = Grid {
        width: w0,
        height: h0,
        data: vec![0u8; w0 * h0],
    };
    for y in 0..active.height {
        for x in 0..active.width {
            if active.get(x, y) {
                let tx = ((x as f64 * cell.0) / page) as usize;
                let ty = ((y as f64 * cell.1) / page) as usize;
                let tx1 = (((x + 1) as f64 * cell.0) / page).ceil() as usize;
                let ty1 = (((y + 1) as f64 * cell.1) / page).ceil() as usize;
                for ty in ty..ty1.min(h0) {
                    for tx in tx..tx1.min(w0) {
                        level.data[ty * w0 + tx] = 1;
                    }
                }
            }
        }
    }
    let mut out = vec![level];
    for _ in 1..level_count(world) {
        let prev = out.last().expect("level 0");
        let (w, h) = (prev.width.div_ceil(2), prev.height.div_ceil(2));
        let mut next = Grid {
            width: w,
            height: h,
            data: vec![0u8; w * h],
        };
        for y in 0..prev.height {
            for x in 0..prev.width {
                if prev.get(x, y) != 0 {
                    next.data[y / 2 * w + x / 2] = 1;
                }
            }
        }
        out.push(next);
    }
    out
}

// ------------------------------------------------------------------- baker

/// One baked tile, for `<gamedata_dir>/tintmaps/<level>/<x>x<y>.dds`.
#[derive(Debug, Clone)]
pub struct Tile {
    pub level: u32,
    pub x: u32,
    pub y: u32,
    pub dds: Vec<u8>,
}

impl Tile {
    pub fn path(&self, gamedata_dir: &str) -> String {
        format!(
            "{gamedata_dir}/tintmaps/{}/{}x{}.dds",
            self.level, self.x, self.y
        )
    }
}

/// Everything a save writes for tint: the tiles of all levels and the rd3d
/// mirror. Tiles flagged absent are not produced; a save should delete
/// (tombstone) shipped tiles that are no longer present.
#[derive(Debug, Clone)]
pub struct Bake {
    pub tiles: Vec<Tile>,
    pub rd3d: Rd3dTint,
}

/// Bakes all levels from `source` (any level; finer levels are bilinearly
/// upsampled from it, coarser ones box-filtered). Presence follows
/// `active`, see [`level_flags`]. `BaseTextureData` is the game's
/// [`Profile::tint_base_level`](crate::game::Profile). A neutral island:
/// `bake(game, &TintMap::neutral(5, world), island.active_map(), world)`.
///
/// Level 0 of a 512 m island is 16384² texels, so finer-than-source levels
/// are generated tile by tile and never held whole.
pub fn bake(game: Game, source: &TintMap, active: &Grid<bool>, world: [u32; 2]) -> Bake {
    let base_level = game.profile().tint_base_level;
    let flags = level_flags(active, world);
    let mut tiles = Vec::new();
    // Tiles encode independently; BC1 fitting dominates the bake.
    let threads = std::thread::available_parallelism().map_or(4, |n| n.get());
    let mut emit = |level: u32, f: &(dyn Fn(u32, u32) -> Vec<u8> + Sync)| {
        let g = &flags[level as usize];
        let present: Vec<(u32, u32)> = (0..g.height as u32)
            .flat_map(|ty| (0..g.width as u32).map(move |tx| (tx, ty)))
            .filter(|&(tx, ty)| g.get(tx as usize, ty as usize) != 0)
            .collect();
        let chunk = present.len().div_ceil(threads).max(1);
        std::thread::scope(|scope| {
            let workers: Vec<_> = present
                .chunks(chunk)
                .map(|part| {
                    scope.spawn(move || {
                        part.iter()
                            .map(|&(x, y)| Tile {
                                level,
                                x,
                                y,
                                dds: f(x, y),
                            })
                            .collect::<Vec<_>>()
                    })
                })
                .collect();
            for w in workers {
                tiles.extend(w.join().expect("tint tile"));
            }
        });
    };

    let levels = level_count(world);
    for level in 0..source.level.min(levels) {
        let (w, h) = level_size(level, world);
        let scale = (1u32 << (source.level - level)) as f32;
        emit(level, &|tx, ty| {
            let (x0, y0) = (tx as usize * TILE, ty as usize * TILE);
            let texels = tile_texels(|x, y| {
                let (x, y) = (x0 + x, y0 + y);
                if x >= w || y >= h {
                    return NEUTRAL;
                }
                // Texel centres line up across levels: level-k texel x covers
                // source texels around (x + 0.5) / scale - 0.5.
                source.sample(
                    (x as f32 + 0.5) / scale - 0.5,
                    (y as f32 + 0.5) / scale - 0.5,
                )
            });
            encode_dds(TILE, TILE, &texels)
        });
    }

    let mut map = source.clone();
    let mut base = None;
    while map.level < levels {
        emit(map.level, &|tx, ty| map.tile_dds(tx, ty));
        if map.level == base_level {
            base = Some(map.clone());
        }
        if map.level + 1 == levels {
            break;
        }
        map = map.downsample();
    }
    // BaseTextureData is always 1 texel per metre: an emitted level from
    // 320 m up, past the last tile level on smaller islands.
    let base = base.unwrap_or_else(|| {
        if map.level > base_level {
            return TintMap::neutral(base_level, world);
        }
        while map.level < base_level {
            map = map.downsample();
        }
        map
    });
    let (bw, bh) = (base.width.div_ceil(4) * 4, base.height.div_ceil(4) * 4);
    let padded: Vec<[u8; 3]> = (0..bw * bh)
        .map(|i| {
            let (x, y) = (i % bw, i / bw);
            if x < base.width && y < base.height {
                base.get(x, y)
            } else {
                NEUTRAL
            }
        })
        .collect();
    let base_blocks = encode_blocks(&padded, bw, bh).concat();
    Bake {
        tiles,
        rd3d: Rd3dTint {
            flags,
            base_width: bw / 4,
            base_height: bh / 4,
            base_blocks,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rda::Archive;

    const PIRATE: &str =
        "data/base/provinces/roman/islands/3rdparty/roman_island_3rdparty_pirate_01";
    const PIRATE_WORLD: [u32; 2] = [320, 320];

    fn gamedata_dir() -> String {
        format!("{PIRATE}/_gamedata/roman_island_3rdparty_pirate_01")
    }

    fn install() -> Option<Install> {
        let Some(dir) = std::env::var_os("ANNO117_DIR") else {
            eprintln!("skip: ANNO117_DIR unset");
            return None;
        };
        Some(Install::open(dir).unwrap())
    }

    fn rd3d(install: &Install) -> Adf {
        let a7m = install
            .read(&format!("{PIRATE}/roman_island_3rdparty_pirate_01.a7m"))
            .unwrap();
        let archive = Archive::from_bytes(a7m, "pirate_01.a7m").unwrap();
        let entry = archive
            .entries()
            .iter()
            .find(|e| e.key().ends_with("rd3d.data"))
            .unwrap();
        Adf::decode(&archive.read(entry).unwrap()).unwrap()
    }

    fn rms(a: &[[u8; 3]], b: &[[u8; 3]]) -> f64 {
        let sum: u64 = a.iter().zip(b).map(|(&x, &y)| u64::from(dist2(x, y))).sum();
        (sum as f64 / (a.len() * 3) as f64).sqrt()
    }

    #[test]
    fn neutral_decodes_to_about_one() {
        // 128 is a hair above 0.5: (256 / 255)^2.2.
        assert!(decode([128; 3]).iter().all(|&v| (v - 1.0086).abs() < 1e-3));
        let n = decode(NEUTRAL);
        assert!(n.iter().all(|&v| (1.0..1.06).contains(&v)), "{n:?}");
        assert_eq!(encode([1.; 3]), [128, 128, 128]);
    }

    #[test]
    fn encode_inverts_decode() {
        for m in [
            [1., 1., 1.],
            [0.8, 0.85, 0.9],
            [1.2, 0.9, 0.7],
            [0.5, 0.5, 0.6],
            [2., 2., 2.],
        ] {
            let back = decode(encode(m));
            for i in 0..3 {
                assert!(
                    (back[i] - m[i]).abs() < 0.03 * m[i].max(1.),
                    "{m:?} -> {back:?}"
                );
            }
        }
        // Texel round trip: u8 -> multiplier -> u8 is exact for in-gamut texels.
        for t in [[132, 117, 132], [117, 118, 134], [120, 115, 129], NEUTRAL] {
            assert_eq!(encode(decode(t)), t);
        }
    }

    #[test]
    fn tile_names_are_column_by_row() {
        assert_eq!(tile_coords("a/tintmaps/0/17x3.dds"), Some((17, 3)));
        assert_eq!(tile_coords("a/meshes/0/1x2.tmc"), None);
        let map = TintMap::neutral(3, PIRATE_WORLD);
        assert_eq!(
            (map.width, map.height, map.tile_count()),
            (1280, 1280, (3, 3))
        );
    }

    #[test]
    fn bc1_solid_neutral_matches_shipped_block() {
        let block = encode_block(&[NEUTRAL; 16]);
        assert_eq!(block, [0x10, 0x84, 0xef, 0x7b, 0, 0, 0, 0]);
    }

    // Smooth gradients plus noise: bounds what the range-fit encoder costs.
    #[test]
    fn bc1_round_trip_error_is_small() {
        let (w, h) = (64, 64);
        let mut seed = 1u32;
        let mut noise = || {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (seed >> 24) as i32 % 7 - 3
        };
        let texels: Vec<[u8; 3]> = (0..w * h)
            .map(|i| {
                let (x, y) = ((i % w) as i32, (i / w) as i32);
                [
                    100 + x / 2 + noise(),
                    110 + y / 3 + noise(),
                    140 - (x + y) / 4 + noise(),
                ]
                .map(|v| v.clamp(0, 255) as u8)
            })
            .collect();
        let back = decode_blocks(&encode_blocks(&texels, w, h).concat(), w, h);
        let max = texels
            .iter()
            .zip(&back)
            .map(|(&a, &b)| dist2(a, b))
            .max()
            .unwrap();
        let err = rms(&texels, &back);
        assert!(err < 3.0, "rms {err}");
        assert!(max < 15 * 15, "max sq {max}");
    }

    #[test]
    fn downsample_halves_and_rounds() {
        let mut map = TintMap::neutral(0, [1, 1]);
        assert_eq!((map.width, map.height), (32, 32));
        map.texels[0] = [0, 0, 0];
        let d = map.downsample();
        assert_eq!((d.level, d.width, d.per_metre), (1, 16, 16.));
        assert_eq!(d.get(0, 0), [99, 98, 99]);
        assert_eq!(d.get(1, 0), NEUTRAL);
    }

    #[test]
    fn bake_writes_present_tiles_and_rd3d_mirror() {
        // 32 m island, active only in its top-left 8 m cell.
        let world = [32, 32];
        let mut active = Grid {
            width: 4,
            height: 4,
            data: vec![false; 16],
        };
        active.set(0, 0, true);
        let source = TintMap::from_fn(3, world, |x, _| [1. + x / 64., 1., 1.]);
        let bake = bake(Game::Anno117, &source, &active, world);
        // 32 m: level 1 is 512² already, so two levels.
        let per_level: Vec<usize> = (0..level_count(world))
            .map(|l| bake.tiles.iter().filter(|t| t.level == l).count())
            .collect();
        assert_eq!(per_level, [1; 2]);
        assert_eq!(bake.rd3d.flags[0].data, [1, 0, 0, 0]);
        assert_eq!(bake.tiles[0].path("g"), "g/tintmaps/0/0x0.dds");
        // 32 texels per metre * 32 m = one 1024² level, so a 2x2 tile grid.
        assert_eq!(bake.rd3d.flags[0].width, 2);
        assert_eq!((bake.rd3d.base_width, bake.rd3d.base_height), (8, 8));

        // Level 0 is the source upsampled.
        let l0 = bake.tiles.iter().find(|t| t.level == 0).unwrap();
        let (_, _, l0) = decode_dds(&l0.dds).unwrap();
        let mid = decode(l0[256 * TILE + 256])[0];
        assert!((mid - (1. + 8. / 64.)).abs() < 0.03, "{mid}");
        // BaseTextureData is 1 texel per metre even past the tile levels:
        // the source box-filtered twice.
        let base = source.downsample().downsample();
        let blocks: Vec<u8> = bake.rd3d.base_blocks.clone();
        let back = decode_blocks(&blocks, 32, 32);
        let err = rms(&back, &base.texels);
        assert!(err < 2.5, "rms {err}");
    }

    #[test]
    fn level_count_matches_every_shipped_size() {
        let shipped = [
            (128, 4),
            (192, 5),
            (256, 5),
            (320, 6),
            (384, 6),
            (448, 6),
            (512, 6),
            (768, 7),
        ];
        for (world, levels) in shipped {
            assert_eq!(level_count([world, world]), levels, "{world} m");
        }
    }

    // Shipped level 3 (4 texels per metre): mostly a slight darkening with
    // chroma fringes, neutral around the island.
    #[test]
    fn pirate_level3_reads_and_decodes() {
        let Some(install) = install() else { return };
        let map = TintMap::read(&install, &gamedata_dir(), 3, PIRATE_WORLD).unwrap();
        assert_eq!((map.width, map.height, map.per_metre), (1280, 1280, 4.));
        let n = map.texels.len() as f64;
        let mean: [f64; 3] =
            std::array::from_fn(|c| map.texels.iter().map(|t| f64::from(t[c])).sum::<f64>() / n);
        assert!(
            (mean[0] - 127.3).abs() < 1.
                && (mean[1] - 122.2).abs() < 1.
                && (mean[2] - 132.7).abs() < 1.,
            "{mean:?}"
        );
        assert_eq!(map.get(0, 0), NEUTRAL);
        let m = map.multipliers();
        let lum = m.iter().map(|v| f64::from(v[1])).sum::<f64>() / n;
        assert!((0.85..1.0).contains(&lum), "mean green multiplier {lum}");
        // Painted spots darken down to ~0 but never brighten past ~1.2x.
        assert!(m.iter().all(|v| v.iter().all(|&c| c < 1.3)));
        let dark = m.iter().filter(|v| v[1] < 0.3).count() as f64 / n;
        assert!(dark < 0.01, "{dark}");
    }

    // Tile names, rd3d flags and the ActiveMap-derived presence agree, and
    // BaseTextureData is the level-5 tile's top-left 80x80 blocks.
    #[test]
    fn pirate_flags_match_tiles_and_active_map() {
        let Some(install) = install() else { return };
        let rd = rd3d(&install);
        let tint = read_rd3d(&rd).unwrap();
        assert_eq!(
            tint.flags.iter().map(|g| g.width).collect::<Vec<_>>(),
            [20, 10, 5, 3, 2, 1]
        );
        for (level, flags) in tint.flags.iter().enumerate() {
            let dir = format!("{}/tintmaps/{level}", gamedata_dir());
            let folder = install.folder(&dir).unwrap();
            let mut shipped = vec![0u8; flags.data.len()];
            for &f in install.folder_files(folder) {
                let (x, y) = tile_coords(install.file_name(f)).unwrap();
                shipped[y as usize * flags.width + x as usize] = 1;
            }
            assert_eq!(shipped, flags.data, "level {level}");
        }

        let terrain = rd.node(rd.root(), "Terrain").unwrap();
        let am = rd.child_node(terrain, "ActiveMap").unwrap();
        let (w, h) = (
            rd.read_u32(rd.child_value(am, "x").unwrap()).unwrap() as usize,
            rd.read_u32(rd.child_value(am, "y").unwrap()).unwrap() as usize,
        );
        let bits = rd.payload(rd.child_value(am, "bits").unwrap());
        let row = bits.len() / h;
        let active = Grid {
            width: w,
            height: h,
            data: (0..w * h)
                .map(|i| bits[i / w * row + i % w / 8] >> (i % 8) & 1 != 0)
                .collect(),
        };
        assert_eq!(level_flags(&active, PIRATE_WORLD), tint.flags);

        let l5 = install
            .read(&format!("{}/tintmaps/5/0x0.dds", gamedata_dir()))
            .unwrap();
        assert_eq!((tint.base_width, tint.base_height), (80, 80));
        for by in 0..80 {
            let shipped = &l5[128 + by * 128 * 8..][..80 * 8];
            assert_eq!(
                shipped,
                &tint.base_blocks[by * 80 * 8..][..80 * 8],
                "block row {by}"
            );
        }

        // Writing what was read back leaves rd3d byte-identical.
        let mut copy = rd.clone();
        write_rd3d(&mut copy, &tint).unwrap();
        assert!(copy.encode() == rd.encode());
    }

    // Re-encoding a shipped tile's decode costs about one level per channel.
    #[test]
    fn pirate_tile_reencode_error() {
        let Some(install) = install() else { return };
        let dds = install
            .read(&format!("{}/tintmaps/2/2x2.dds", gamedata_dir()))
            .unwrap();
        assert_eq!(&dds[..128], &DDS_HEADER);
        let (w, h, texels) = decode_dds(&dds).unwrap();
        let again = encode_dds(w, h, &texels);
        assert_eq!(&again[..128], &DDS_HEADER);
        let (_, _, back) = decode_dds(&again).unwrap();
        let err = rms(&texels, &back);
        assert!(err < 1.5, "rms {err}");
    }
}
