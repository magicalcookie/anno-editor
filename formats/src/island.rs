// Editable island model over the two `.a7m` members (`gamedata.data`,
// `rd3d.data`) and the optional `.a7minfo`. Owns the editor-facing grids
// (heights, displacement, materials, buildable mask, props) and writes them
// back, re-deriving what the engine expects to agree with them. Everything
// untouched stays byte-identical.
//
// Derived data is merged per cell: a derived cell is rewritten only where
// recomputing it from the edited inputs gives a different answer than
// recomputing it from the original inputs. Shipped cells the recipe cannot
// reproduce exactly (see `level0_min_max`, `used_material_bits`) therefore
// survive unrelated edits.

use anyhow::{Context, Result, bail, ensure};

use crate::adf::{Adf, ItemId, utf16_bytes};
use crate::ctt::{self, HeightPage};
use crate::tmc::{self, MeshPatch, Tile};

/// rd3d `CoarseHeightMap` units per metre. gamedata `HeightMap` stores half
/// that (1/128 m) with its own rounding, see `gamedata_height`.
pub const HEIGHT_UNITS_PER_METRE: f32 = 256.0;

/// Row-major 2D grid.
#[derive(Debug, Clone, PartialEq)]
pub struct Grid<T> {
    pub width: usize,
    pub height: usize,
    pub data: Vec<T>,
}

impl<T: Copy> Grid<T> {
    pub fn get(&self, x: usize, y: usize) -> T {
        self.data[y * self.width + x]
    }

    pub fn set(&mut self, x: usize, y: usize, value: T) {
        self.data[y * self.width + x] = value;
    }
}

/// Borrowed map whose cell layout is not decoded yet (water, fog).
#[derive(Debug, Clone, Copy)]
pub struct RawMap<'a> {
    pub width: usize,
    pub height: usize,
    pub data: &'a [u8],
}

/// One `PropGrid/Instances` entry. `file` indexes [`Island::prop_files`].
#[derive(Debug, Clone, PartialEq)]
pub struct Prop {
    pub file: u32,
    pub position: [f32; 3],
    /// Quaternion in stored order (x, y, z, w).
    pub rotation: [f32; 4],
    pub scale: [f32; 3],
    pub color: [f32; 4],
    /// Present (always 1 so far) on props that follow terrain edits.
    pub adapt_terrain_height: Option<u8>,
}

/// Full-resolution material layer: `materialmaps/0/matid.dds` and
/// `matalpha.dds` (DXGI R16G16_UNORM, read as one little-endian u32 per
/// texel in the same slot packing as the rd3d arrays). Ids are one word per
/// 2x2 alpha texels, alphas `MaterialMapWidth` (8 per metre) square.
#[derive(Debug, Clone)]
struct FullMaterials {
    ids: Grid<u32>,
    alphas: Grid<u32>,
    original: (Vec<u32>, Vec<u32>),
}

#[derive(Debug, Clone)]
struct Displacement {
    grid: Grid<u8>,
    gamedata: ItemId,
    rd3d: ItemId,
}

/// Material slots: `MaterialIDs2` packs six 5-bit material ids per 2 m block,
/// `MaterialAlphas2` six 5-bit weights per 1 m texel; slot k of a texel pairs
/// with slot k of its block. Bits 30..31 are unused.
pub fn unpack_slots(word: u32) -> [u8; 6] {
    std::array::from_fn(|k| ((word >> (5 * k)) & 31) as u8)
}

pub fn pack_slots(slots: [u8; 6]) -> u32 {
    slots
        .iter()
        .enumerate()
        .fold(0, |w, (k, &s)| w | (u32::from(s & 31) << (5 * k)))
}

#[derive(Debug, Clone)]
struct Level {
    width: usize,
    height: usize,
    min_y: ItemId,
    max_y: ItemId,
    has_mesh: ItemId,
}

/// Island editor model. Build with [`Island::parse`], edit through the `_mut`
/// accessors, write with [`Island::to_members`] / [`Island::to_info`].
#[derive(Debug, Clone)]
pub struct Island {
    gamedata: Adf,
    rd3d: Adf,
    info: Option<Adf>,

    world_size: [u32; 2],
    heights: Grid<f32>,
    displacement: Option<Displacement>,
    material_ids: Grid<u32>,
    material_alphas: Grid<u32>,
    material_set: String,
    active_map: Grid<bool>,
    prop_files: Vec<String>,
    props: Vec<Prop>,

    gd_heights: ItemId,
    rd_heights: ItemId,
    ids_item: ItemId,
    alphas_item: ItemId,
    used_bits: Option<(ItemId, ItemId)>,
    material_map_size: [usize; 2],
    full_materials: Option<FullMaterials>,
    material_set_item: Option<ItemId>,
    levels: Vec<Level>,
    min_mesh_level: usize,
    // Metres per normal-map page on level 0 (PageSize × UnitScale).
    page_metres: f32,
    // AllNormalMapLevels: which `.ctt` pages exist, per normal-map level.
    normal_pages: Vec<Grid<bool>>,
    mesh_patches: Vec<MeshPatch>,
    // No shipped terrain files to patch: saves write every tile and page.
    unshipped: bool,
    active_bits: ItemId,
    info_active_bits: Option<ItemId>,
    info_active_rect: Option<ItemId>,
    file_names: ItemId,
    instances: ItemId,
    water_infos: Option<(usize, usize, ItemId)>,
    water_flow: Option<(usize, usize, ItemId)>,
    fog: Option<ItemId>,
}

// `{width,height,map}` style node (rd3d lowercase, gamedata capitalised).
fn map_node(adf: &Adf, node: ItemId, names: [&str; 3]) -> Result<(usize, usize, ItemId)> {
    let get = |n: &str| {
        adf.child_value(node, n)
            .with_context(|| format!("map node {:?}: no {n}", adf.name(node)))
    };
    let w = adf.read_u32(get(names[0])?)? as usize;
    let h = adf.read_u32(get(names[1])?)? as usize;
    Ok((w, h, get(names[2])?))
}

fn lower_map(adf: &Adf, parent: ItemId, name: &str) -> Result<(usize, usize, ItemId)> {
    let node = adf
        .child_node(parent, name)
        .with_context(|| format!("rd3d: no {name} node"))?;
    map_node(adf, node, ["width", "height", "map"])
}

fn i16s(bytes: &[u8]) -> Vec<i16> {
    bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|&c| i16::from_le_bytes(c))
        .collect()
}

fn u32s(bytes: &[u8]) -> Vec<u32> {
    bytes
        .as_chunks::<4>()
        .0
        .iter()
        .map(|&c| u32::from_le_bytes(c))
        .collect()
}

fn i16_bytes(v: &[i16]) -> Vec<u8> {
    v.iter().flat_map(|x| x.to_le_bytes()).collect()
}

fn u32_bytes(v: &[u32]) -> Vec<u8> {
    v.iter().flat_map(|x| x.to_le_bytes()).collect()
}

fn f32_bytes(v: &[f32]) -> Vec<u8> {
    v.iter().flat_map(|x| x.to_le_bytes()).collect()
}

// Bit masks (ActiveMap, HasMeshArray): LSB-first, each row padded to whole
// u32 words, e.g. 40 cells -> 8 bytes per row. The padding of shipped masks
// is uninitialised engine memory (ASCII fragments in deco_01's 16-wide
// levels), so rewrites keep the old bytes and only touch real cells.
fn row_bytes(width: usize) -> usize {
    width.div_ceil(32) * 4
}

fn unpack_bits(bytes: &[u8], width: usize, height: usize) -> Result<Vec<bool>> {
    let stride = row_bytes(width);
    ensure!(
        bytes.len() == stride * height,
        "bit mask {width}x{height}: {} bytes, expected {}",
        bytes.len(),
        stride * height
    );
    Ok((0..width * height)
        .map(|i| {
            let (y, x) = (i / width, i % width);
            bytes[y * stride + x / 8] >> (x % 8) & 1 != 0
        })
        .collect())
}

fn pack_bits_over(base: &[u8], bits: &[bool], width: usize) -> Vec<u8> {
    let stride = row_bytes(width);
    let mut out = base.to_vec();
    for (i, &b) in bits.iter().enumerate() {
        let (byte, mask) = (
            (i / width) * stride + (i % width) / 8,
            1u8 << (i % width % 8),
        );
        if b {
            out[byte] |= mask;
        } else {
            out[byte] &= !mask;
        }
    }
    out
}

fn quantize_height(metres: f32) -> i16 {
    // Saturating cast: out-of-range clamps, NaN becomes 0.
    (metres * HEIGHT_UNITS_PER_METRE).round() as i16
}

// gamedata HeightMap from the rd3d value. Matches every interior texel of the
// shipped islands; negatives round differently from positives (both sources
// are quantised from the editor's float heights, gamedata with a +1 bias
// below zero).
fn gamedata_height(coarse: i16) -> i16 {
    let c = i32::from(coarse);
    (if c >= 0 { (c + 1) >> 1 } else { (c >> 1) + 1 }) as i16
}

// TODO(alex): only approximate. Shipped gamedata DisplacementHeightMap holds
// the finer source the u8 map was quantised from (u8 ~ 128 + floor(gd / 2)
// with +-1 noise), so edited cells get the coarse value back.
fn gamedata_displacement(coarse: u8) -> i16 {
    (i16::from(coarse) - 128) * 2
}

// Level-0 MinY/MaxY: one cell per 2x2 height texels plus the shared far
// edge (3x3 window), in 1 m units (256 coarse units). The final texel row and
// column never contribute; the last cell clamps to the one before.
//
// MinY truncates toward zero and matches shipped data everywhere. MaxY is the
// ceiling and matches except where the window max is an exact multiple of
// 1 m: there the engine sometimes stores +1 because its float source sat a
// hair above the quantised value. That bit is not recoverable from the file
// (about a third of those cells, 0.04-0.2% of all cells per island).
fn level0_min_max(coarse: &Grid<i16>, w: usize, h: usize) -> (Vec<i16>, Vec<i16>) {
    let mut min_y = Vec::with_capacity(w * h);
    let mut max_y = Vec::with_capacity(w * h);
    let last_x = coarse.width.saturating_sub(2);
    let last_y = coarse.height.saturating_sub(2);
    for cy in 0..h {
        for cx in 0..w {
            let (mut lo, mut hi) = (i32::MAX, i32::MIN);
            for y in 2 * cy..=(2 * cy + 2).min(last_y) {
                for x in 2 * cx..=(2 * cx + 2).min(last_x) {
                    let v = i32::from(coarse.get(x, y));
                    lo = lo.min(v);
                    hi = hi.max(v);
                }
            }
            min_y.push((lo / 256) as i16);
            max_y.push((-(-hi).div_euclid(256)) as i16);
        }
    }
    (min_y, max_y)
}

// Coarser levels: min/max over the 2x2 children, clipped at odd edges.
// Exact on every shipped island checked.
fn mip_min_max(
    min_y: &[i16],
    max_y: &[i16],
    pw: usize,
    ph: usize,
    w: usize,
    h: usize,
) -> (Vec<i16>, Vec<i16>) {
    let mut lo = Vec::with_capacity(w * h);
    let mut hi = Vec::with_capacity(w * h);
    for y in 0..h {
        for x in 0..w {
            let (mut a, mut b) = (i16::MAX, i16::MIN);
            for cy in 2 * y..(2 * y + 2).min(ph) {
                for cx in 2 * x..(2 * x + 2).min(pw) {
                    a = a.min(min_y[cy * pw + cx]);
                    b = b.max(max_y[cy * pw + cx]);
                }
            }
            lo.push(a);
            hi.push(b);
        }
    }
    (lo, hi)
}

// HasMeshArray: level 0 is the ActiveMap scaled up (8 m cells -> 1 m), the
// rest OR their 2x2 children. Exact on every shipped island checked.
fn has_mesh_levels(active: &Grid<bool>, sizes: &[(usize, usize)]) -> Vec<Vec<bool>> {
    let mut out: Vec<Vec<bool>> = Vec::with_capacity(sizes.len());
    for (k, &(w, h)) in sizes.iter().enumerate() {
        let level = if k == 0 {
            (0..w * h)
                .map(|i| {
                    let (y, x) = (i / w, i % w);
                    active.get(x * active.width / w, y * active.height / h)
                })
                .collect()
        } else {
            let (pw, ph) = sizes[k - 1];
            let prev = &out[k - 1];
            (0..w * h)
                .map(|i| {
                    let (y, x) = (i / w, i % w);
                    (2 * y..(2 * y + 2).min(ph))
                        .any(|cy| (2 * x..(2 * x + 2).min(pw)).any(|cx| prev[cy * pw + cx]))
                })
                .collect()
        };
        out.push(level);
    }
    out
}

// UsedMaterialBits, after the engine's generator (run when the
// field is missing): one u32 material mask per 4x4 material-map texels plus a
// one-texel border, then mips that halve (floor, min 1) and OR a 4x4 window
// around the 2x2 children.
//
// The generator runs at MaterialMapWidth (8 texels per metre), i.e. on the
// island's `materialmaps/0/{matid,matalpha}.dds`; fed those it reproduces
// the shipped bits exactly. The rd3d MaterialIDs2/MaterialAlphas2 are only
// the 1 m copy (`materialmaps/3`); upsampled they give the right mip shape
// but not the bits (~25% of base cells differ, and at that resolution the
// shipped bits are a strict superset). Lower-resolution inputs are
// nearest-upsampled here and callers merge them as `shipped | generated`.
fn used_material_bits(ids: &Grid<u32>, alphas: &Grid<u32>, material_map: [usize; 2]) -> Vec<u32> {
    let (aw, ah) = (alphas.width, alphas.height);
    let texel = material_mask(ids, alphas).data;
    let [mw, mh] = material_map;
    let (up_x, up_y) = ((mw / aw).max(1), (mh / ah).max(1));
    let (mut w, mut h) = ((mw / 4).max(1), (mh / 4).max(1));
    let mut out = Vec::new();
    for by in 0..h {
        let (y0, y1) = ((4 * by).saturating_sub(1), (4 * by + 5).min(mh));
        for bx in 0..w {
            let (x0, x1) = ((4 * bx).saturating_sub(1), (4 * bx + 5).min(mw));
            let mut m = 0;
            for y in y0 / up_y..=(y1 - 1) / up_y {
                for x in x0 / up_x..=(x1 - 1) / up_x {
                    m |= texel[y * aw + x];
                }
            }
            out.push(m);
        }
    }
    let mut start = 0;
    while w > 1 || h > 1 {
        let (nw, nh) = ((w >> 1).max(1), (h >> 1).max(1));
        let level_start = out.len();
        for y in 0..nh {
            for x in 0..nw {
                let mut m = 0;
                for cy in (2 * y).saturating_sub(1)..(2 * y + 3).min(h) {
                    for cx in (2 * x).saturating_sub(1)..(2 * x + 3).min(w) {
                        m |= out[start + cy * w + cx];
                    }
                }
                out.push(m);
            }
        }
        start = level_start;
        (w, h) = (nw, nh);
    }
    out
}

// Bit per material id with a non-zero weight, per alpha texel.
fn material_mask(ids: &Grid<u32>, alphas: &Grid<u32>) -> Grid<u32> {
    let (aw, ah) = (alphas.width, alphas.height);
    let block = (aw / ids.width).max(1);
    Grid {
        width: aw,
        height: ah,
        data: (0..aw * ah)
            .map(|i| {
                let (y, x) = (i / aw, i % aw);
                let id = unpack_slots(ids.get(x / block, y / block));
                let alpha = unpack_slots(alphas.data[i]);
                (0..6)
                    .filter(|&k| alpha[k] != 0)
                    .fold(0u32, |m, k| m | 1 << id[k])
            })
            .collect(),
    }
}

// Per-node `UsedMaterialBits` of a mesh level: the materials under the node
// plus a one-texel border for bilinear filtering. Shipped bits are a superset
// of the footprint in every node checked (exact rule open; they also carry
// bits no texel nearby uses), so unchanged nodes keep their shipped value.
fn node_material_bits(
    mask: &Grid<u32>,
    world: [u32; 2],
    level: usize,
    nodes: (usize, usize),
) -> Vec<u32> {
    let per_metre = (mask.width / world[0].max(1) as usize).max(1);
    let span = per_metre << level;
    let (nw, nh) = nodes;
    let mut out = Vec::with_capacity(nw * nh);
    for ny in 0..nh {
        let (y0, y1) = (
            (ny * span).saturating_sub(1),
            ((ny + 1) * span + 1).min(mask.height),
        );
        for nx in 0..nw {
            let (x0, x1) = (
                (nx * span).saturating_sub(1),
                ((nx + 1) * span + 1).min(mask.width),
            );
            let mut m = 0;
            for y in y0..y1 {
                for x in x0..x1 {
                    m |= mask.data[y * mask.width + x];
                }
            }
            out.push(m);
        }
    }
    out
}

fn used_mip_count(material_map: [usize; 2]) -> u32 {
    let (mut w, mut h) = ((material_map[0] / 4).max(1), (material_map[1] / 4).max(1));
    let mut n = 1;
    while w > 1 || h > 1 {
        (w, h) = ((w >> 1).max(1), (h >> 1).max(1));
        n += 1;
    }
    n
}

// Keep shipped cells unless the edit changed what the recipe yields there.
fn merge<T: Copy + PartialEq>(shipped: &[T], before: &[T], after: &[T]) -> Vec<T> {
    shipped
        .iter()
        .zip(before.iter().zip(after))
        .map(|(&s, (b, &a))| if *b == a { s } else { a })
        .collect()
}

fn parse_props(
    adf: &Adf,
    file_names: ItemId,
    instances: ItemId,
) -> Result<(Vec<String>, Vec<Prop>)> {
    let files = adf
        .children(file_names)
        .map(|f| adf.read_utf16(f))
        .collect::<Result<Vec<_>>>()?;
    let mut props = Vec::new();
    for inst in adf.children(instances) {
        ensure!(adf.is_node(inst), "rd3d: value inside Instances");
        let mut prop = Prop {
            file: u32::MAX,
            position: [0.0; 3],
            rotation: [0.0, 0.0, 0.0, 1.0],
            scale: [1.0; 3],
            color: [1.0; 4],
            adapt_terrain_height: None,
        };
        let mut seen = 0u8;
        for field in adf.children(inst) {
            match adf.name(field) {
                Some("Index") => (prop.file, seen) = (adf.read_u32(field)?, seen | 1),
                Some("Position") => (prop.position, seen) = (adf.read_f32s(field)?, seen | 2),
                Some("Rotation") => (prop.rotation, seen) = (adf.read_f32s(field)?, seen | 4),
                Some("Scale") => (prop.scale, seen) = (adf.read_f32s(field)?, seen | 8),
                Some("Color") => (prop.color, seen) = (adf.read_f32s(field)?, seen | 16),
                Some("AdaptTerrainHeight") => prop.adapt_terrain_height = Some(adf.read_u8(field)?),
                other => bail!("rd3d: prop {} has unsupported field {other:?}", props.len()),
            }
        }
        // Every shipped prop carries all five; a file eliding defaults would
        // not roundtrip through the rebuild below, so refuse it loudly.
        ensure!(
            seen == 31,
            "rd3d: prop {} misses a transform field",
            props.len()
        );
        ensure!(
            (prop.file as usize) < files.len(),
            "rd3d: prop {} Index {} out of {} FileNames",
            props.len(),
            prop.file,
            files.len()
        );
        props.push(prop);
    }
    Ok((files, props))
}

impl Island {
    /// Builds the model from the decompressed `.a7m` members.
    pub fn parse(gamedata: &[u8], rd3d: &[u8]) -> Result<Island> {
        Island::load(gamedata, rd3d, None)
    }

    /// Like [`Island::parse`], plus the `.a7minfo` next to the `.a7m` so
    /// buildable-mask edits reach it through [`Island::to_info`].
    pub fn parse_with_info(gamedata: &[u8], rd3d: &[u8], info: &[u8]) -> Result<Island> {
        Island::load(gamedata, rd3d, Some(info))
    }

    fn load(gamedata: &[u8], rd3d: &[u8], info: Option<&[u8]>) -> Result<Island> {
        let gd = Adf::decode(gamedata).context("gamedata.data")?;
        let rd = Adf::decode(rd3d).context("rd3d.data")?;
        let info = info.map(Adf::decode).transpose().context(".a7minfo")?;

        let tm = gd
            .node(gd.root(), "GameSessionManager/TerrainManager")
            .context("gamedata: no GameSessionManager/TerrainManager")?;
        let ws = gd.read_u64(
            gd.child_value(tm, "WorldSize")
                .context("gamedata: no WorldSize")?,
        )?;
        let world_size = [ws as u32, (ws >> 32) as u32];
        let gd_map = |name: &str| -> Result<(usize, usize, ItemId)> {
            let node = gd
                .child_node(tm, name)
                .with_context(|| format!("gamedata: no {name}"))?;
            map_node(&gd, node, ["Width", "Height", "HeightMap"])
        };
        let (gw, gh, gd_heights) = gd_map("HeightMap")?;

        let terrain = rd
            .child_node(rd.root(), "Terrain")
            .context("rd3d: no Terrain")?;
        let (w, h, rd_heights) = lower_map(&rd, terrain, "CoarseHeightMap")?;
        ensure!(
            (gw, gh) == (w, h),
            "height grids disagree: rd3d {w}x{h}, gamedata {gw}x{gh}"
        );
        ensure!(
            rd.payload(rd_heights).len() == w * h * 2 && gd.payload(gd_heights).len() == w * h * 2,
            "height map payload size mismatch"
        );
        let heights = Grid {
            width: w,
            height: h,
            data: i16s(rd.payload(rd_heights))
                .into_iter()
                .map(|v| f32::from(v) / HEIGHT_UNITS_PER_METRE)
                .collect(),
        };
        let displacement = match (
            gd.child_node(tm, "DisplacementHeightMap"),
            rd.child_node(terrain, "CoarseDisplacementMap"),
        ) {
            (None, None) => None,
            (Some(_), Some(_)) => {
                let (dw, dh, gamedata) = gd_map("DisplacementHeightMap")?;
                let (cw, ch, rd3d) = lower_map(&rd, terrain, "CoarseDisplacementMap")?;
                ensure!(
                    (cw, ch) == (w, h)
                        && (dw, dh) == (w, h)
                        && rd.payload(rd3d).len() == w * h
                        && gd.payload(gamedata).len() == w * h * 2,
                    "displacement grid mismatch"
                );
                Some(Displacement {
                    grid: Grid {
                        width: w,
                        height: h,
                        data: rd.payload(rd3d).to_vec(),
                    },
                    gamedata,
                    rd3d,
                })
            }
            _ => bail!("gamedata and rd3d disagree about displacement maps"),
        };

        let terrain_u32 = |name: &str| -> Result<usize> {
            let v = rd
                .child_value(terrain, name)
                .with_context(|| format!("rd3d: no Terrain/{name}"))?;
            Ok(rd.read_u32(v)? as usize)
        };
        let material_map_size = [
            terrain_u32("MaterialMapWidth")?,
            terrain_u32("MaterialMapHeight")?,
        ];
        let ids_item = rd
            .child_value(terrain, "MaterialIDs2")
            .context("rd3d: no MaterialIDs2")?;
        let alphas_item = rd
            .child_value(terrain, "MaterialAlphas2")
            .context("rd3d: no MaterialAlphas2")?;
        let alphas = u32s(rd.payload(alphas_item));
        // 1800 stores its full 2 texels/m maps here. 117 keeps a smaller
        // square copy here and its full-resolution maps in external DDS
        // files: 1 texel/m on most islands, 2 on the 64 m deco ones (128²).
        let [aw, ah] = if alphas.len() == material_map_size[0] * material_map_size[1] {
            material_map_size
        } else {
            let side = (alphas.len() as f64).sqrt().round() as usize;
            ensure!(
                side * side == alphas.len() && world_size[0] == world_size[1],
                "MaterialAlphas2: {} texels is not a square copy of a {}x{} island",
                alphas.len(),
                world_size[0],
                world_size[1]
            );
            [side, side]
        };
        ensure!(
            alphas.len() == aw * ah,
            "MaterialAlphas2: {} texels for {aw}x{ah}",
            alphas.len()
        );
        let ids = u32s(rd.payload(ids_item));
        let (iw, ih) = (aw / 2, ah / 2);
        ensure!(
            ids.len() == iw * ih,
            "MaterialIDs2: {} blocks for {iw}x{ih}",
            ids.len()
        );
        let used_bits = match (
            rd.child_value(terrain, "UsedMaterialBits"),
            rd.child_value(terrain, "NumUsedMatBitsMips"),
        ) {
            (Some(b), Some(n)) => Some((b, n)),
            _ => None,
        };
        let material_set_item = rd.child_value(terrain, "MaterialSetFileName");
        // 1800 elides the default Old World material set.
        let material_set = material_set_item
            .map(|v| rd.read_utf16(v))
            .transpose()?
            .unwrap_or_else(|| "data/config/engine/material_sets/terra_01.xml".into());

        let mut levels = Vec::new();
        let levels_node = rd
            .child_node(terrain, "Levels")
            .context("rd3d: no Terrain/Levels")?;
        for level in rd.children(levels_node).filter(|&l| rd.is_node(l)) {
            let (w, h, min_y) = lower_map(&rd, level, "MinYArray")?;
            let (_, _, max_y) = lower_map(&rd, level, "MaxYArray")?;
            let hm = rd
                .child_node(level, "HasMeshArray")
                .context("rd3d: no HasMeshArray")?;
            let (bw, bh, has_mesh) = map_node(&rd, hm, ["x", "y", "bits"])?;
            ensure!(
                (bw, bh) == (w, h)
                    && rd.payload(min_y).len() == w * h * 2
                    && rd.payload(max_y).len() == w * h * 2,
                "rd3d: inconsistent normal-map level {}",
                levels.len()
            );
            levels.push(Level {
                width: w,
                height: h,
                min_y,
                max_y,
                has_mesh,
            });
        }
        ensure!(!levels.is_empty(), "rd3d: Terrain/Levels is empty");
        let optional_u32 = |name: &str| -> Result<Option<u32>> {
            rd.child_value(terrain, name)
                .map(|v| rd.read_u32(v))
                .transpose()
        };
        let min_mesh_level = optional_u32("MinMeshLevel")?.unwrap_or(2) as usize;
        let unit_scale = match rd.child_value(terrain, "UnitScale") {
            Some(v) => rd.read_f32(v)?,
            None => 1. / 32.,
        };
        let page_metres = optional_u32("PageSize")?.unwrap_or(512) as f32 * unit_scale;
        ensure!(
            optional_u32("HeightPageBorderTexels")?.is_none_or(|b| b as usize == ctt::BORDER),
            "rd3d: HeightPageBorderTexels is not {}",
            ctt::BORDER
        );
        let mut normal_pages = Vec::new();
        if let Some(list) = rd.node(terrain, "AllNormalMapLevels/Levels") {
            for level in rd.children(list).filter(|&l| rd.is_node(l)) {
                let flags = rd
                    .child_node(level, "Flags")
                    .context("rd3d: normal-map level without Flags")?;
                let (w, h, map) = map_node(&rd, flags, ["width", "height", "map"])?;
                let bytes = rd.payload(map);
                ensure!(
                    bytes.len() == w * h,
                    "rd3d: normal-map Flags {w}x{h} has {} B",
                    bytes.len()
                );
                normal_pages.push(Grid {
                    width: w,
                    height: h,
                    data: bytes.iter().map(|&b| b != 0).collect(),
                });
            }
        }

        let active = rd
            .child_node(terrain, "ActiveMap")
            .context("rd3d: no ActiveMap")?;
        let (ax, ay, active_bits) = map_node(&rd, active, ["x", "y", "bits"])?;
        let active_map = Grid {
            width: ax,
            height: ay,
            data: unpack_bits(rd.payload(active_bits), ax, ay)?,
        };
        let (info_active_bits, info_active_rect) = match &info {
            Some(i) => (
                i.value(i.root(), "ActiveMapGrid/bits"),
                i.child_value(i.root(), "ActiveMapRect"),
            ),
            None => (None, None),
        };

        let prop_grid = rd
            .child_node(rd.root(), "PropGrid")
            .context("rd3d: no PropGrid")?;
        let file_names = rd
            .child_node(prop_grid, "FileNames")
            .context("rd3d: no FileNames")?;
        let instances = rd
            .child_node(prop_grid, "Instances")
            .context("rd3d: no Instances")?;
        let (prop_files, props) = parse_props(&rd, file_names, instances)?;

        let water_infos = lower_map(&rd, rd.root(), "WaterInfos").ok();
        let water_flow = lower_map(&rd, rd.root(), "WaterFlow").ok();
        let fog = rd.child_value(rd.root(), "Fog");

        Ok(Island {
            world_size,
            heights,
            displacement,
            material_ids: Grid {
                width: iw,
                height: ih,
                data: ids,
            },
            material_alphas: Grid {
                width: aw,
                height: ah,
                data: alphas,
            },
            material_set,
            active_map,
            prop_files,
            props,
            gd_heights,
            rd_heights,
            ids_item,
            alphas_item,
            used_bits,
            material_map_size,
            full_materials: None,
            material_set_item,
            levels,
            min_mesh_level,
            page_metres,
            normal_pages,
            mesh_patches: Vec::new(),
            unshipped: false,
            active_bits,
            info_active_bits,
            info_active_rect,
            file_names,
            instances,
            water_infos,
            water_flow,
            fog,
            gamedata: gd,
            rd3d: rd,
            info,
        })
    }

    /// Island extent in metres (gamedata `WorldSize`).
    pub fn world_size(&self) -> [u32; 2] {
        self.world_size
    }

    pub fn game(&self) -> crate::game::Game {
        match self.displacement {
            Some(_) => crate::game::Game::Anno117,
            None => crate::game::Game::Anno1800,
        }
    }

    /// Terrain heights in metres, `2 * world + 1` texels per side (0.5 m
    /// spacing plus the far border). Saved at 1/256 m precision.
    pub fn heights(&self) -> &Grid<f32> {
        &self.heights
    }

    pub fn heights_mut(&mut self) -> &mut Grid<f32> {
        &mut self.heights
    }

    /// Render-time displacement (`CoarseDisplacementMap`), 128 = none; same
    /// grid as the heights.
    pub fn displacement(&self) -> Option<&Grid<u8>> {
        self.displacement.as_ref().map(|d| &d.grid)
    }

    pub fn displacement_mut(&mut self) -> Option<&mut Grid<u8>> {
        self.displacement.as_mut().map(|d| &mut d.grid)
    }

    /// `MaterialIDs2`, one word per 2 m block; see [`unpack_slots`].
    pub fn material_ids(&self) -> &Grid<u32> {
        &self.material_ids
    }

    pub fn material_ids_mut(&mut self) -> &mut Grid<u32> {
        &mut self.material_ids
    }

    /// `MaterialAlphas2`, one word per metre; see [`unpack_slots`].
    pub fn material_alphas(&self) -> &Grid<u32> {
        &self.material_alphas
    }

    pub fn material_alphas_mut(&mut self) -> &mut Grid<u32> {
        &mut self.material_alphas
    }

    /// Attaches `materialmaps/0` (see [`Island::full_material_ids`]). With it
    /// attached, `UsedMaterialBits` is regenerated exactly from this layer
    /// on save; without it, from the 1 m rd3d copy (conservative superset).
    pub fn attach_full_material_map(&mut self, ids: Grid<u32>, alphas: Grid<u32>) -> Result<()> {
        let [mw, mh] = self.material_map_size;
        ensure!(
            (alphas.width, alphas.height) == (mw, mh) && alphas.data.len() == mw * mh,
            "matalpha is {}x{}, MaterialMapWidth/Height say {mw}x{mh}",
            alphas.width,
            alphas.height
        );
        ensure!(
            (ids.width, ids.height) == (mw / 2, mh / 2) && ids.data.len() == ids.width * ids.height,
            "matid is {}x{}, expected {}x{}",
            ids.width,
            ids.height,
            mw / 2,
            mh / 2
        );
        let original = (ids.data.clone(), alphas.data.clone());
        self.full_materials = Some(FullMaterials {
            ids,
            alphas,
            original,
        });
        Ok(())
    }

    /// Attached `materialmaps/0/matid.dds` texels, one word per 2x2 alpha
    /// texels. The editor writes the DDS levels itself; levels 1..3 are
    /// downsamples whose rule is not pinned yet, and level 3 must equal
    /// [`Island::material_ids`] / [`Island::material_alphas`].
    pub fn full_material_ids(&self) -> Option<&Grid<u32>> {
        self.full_materials.as_ref().map(|f| &f.ids)
    }

    pub fn full_material_ids_mut(&mut self) -> Option<&mut Grid<u32>> {
        self.full_materials.as_mut().map(|f| &mut f.ids)
    }

    /// Attached `materialmaps/0/matalpha.dds` texels.
    pub fn full_material_alphas(&self) -> Option<&Grid<u32>> {
        self.full_materials.as_ref().map(|f| &f.alphas)
    }

    pub fn full_material_alphas_mut(&mut self) -> Option<&mut Grid<u32>> {
        self.full_materials.as_mut().map(|f| &mut f.alphas)
    }

    /// Palette the material ids index into (`.xml` under
    /// `graphics/library/environment/material_sets/`).
    /// How many words of the stored `UsedMaterialBits` (all mips) lack a
    /// material the given full-resolution layer paints there, of how many.
    /// The game's terrain bake skips a triangle for every material whose
    /// bit is missing, so a short word leaves ground unpainted (white).
    pub fn used_bits_shortfall(
        &self,
        ids: &Grid<u32>,
        alphas: &Grid<u32>,
    ) -> Option<(usize, usize)> {
        let (bits, _) = self.used_bits?;
        let stored = u32s(self.rd3d.payload(bits));
        let needed = used_material_bits(ids, alphas, self.material_map_size);
        let short = stored
            .iter()
            .zip(&needed)
            .filter(|&(&s, &n)| n & !s != 0)
            .count();
        Some((short, needed.len().min(stored.len())))
    }

    pub fn material_set_file_name(&self) -> &str {
        &self.material_set
    }

    pub fn set_material_set_file_name(&mut self, path: &str) {
        self.material_set = path.to_owned();
    }

    /// Buildable mask (`ActiveMap`), one cell per 8 m.
    pub fn active_map(&self) -> &Grid<bool> {
        &self.active_map
    }

    pub fn active_map_mut(&mut self) -> &mut Grid<bool> {
        &mut self.active_map
    }

    /// Mesh paths (`.cfg`) referenced by [`Prop::file`].
    pub fn prop_files(&self) -> &[String] {
        &self.prop_files
    }

    /// Index of `path` in [`Island::prop_files`], appending it if new.
    /// Unreferenced files are dropped on save.
    pub fn prop_file_index(&mut self, path: &str) -> u32 {
        match self.prop_files.iter().position(|f| f == path) {
            Some(i) => i as u32,
            None => {
                self.prop_files.push(path.to_owned());
                (self.prop_files.len() - 1) as u32
            }
        }
    }

    pub fn props(&self) -> &[Prop] {
        &self.props
    }

    /// Add, remove or move props freely; indices are validated on save.
    // TODO(alex): new props are appended. Shipped lists are clustered
    // spatially (PropGrid); whether the engine relies on that is untested.
    pub fn props_mut(&mut self) -> &mut Vec<Prop> {
        &mut self.props
    }

    /// `WaterInfos`, u32 per 2 m cell; bytes in `crate::water`.
    pub fn water_infos(&self) -> Option<RawMap<'_>> {
        self.raw_map(self.water_infos)
    }

    /// `WaterFlow`, u32 per 2 m cell: flow x/z biased 127, coast distance.
    pub fn water_flow(&self) -> Option<RawMap<'_>> {
        self.raw_map(self.water_flow)
    }

    /// Root `Fog` blob, layout undecoded.
    pub fn fog(&self) -> Option<&[u8]> {
        self.fog.map(|f| self.rd3d.payload(f))
    }

    fn raw_map(&self, m: Option<(usize, usize, ItemId)>) -> Option<RawMap<'_>> {
        m.map(|(width, height, item)| RawMap {
            width,
            height,
            data: self.rd3d.payload(item),
        })
    }

    /// Decoded member trees, for fields this model does not cover.
    /// For editors of other gamedata sections (placed objects). Tracked
    /// handles stay valid across edits and `to_members` re-encodes this tree.
    pub fn gamedata_mut(&mut self) -> &mut Adf {
        &mut self.gamedata
    }

    pub fn gamedata(&self) -> &Adf {
        &self.gamedata
    }

    pub fn rd3d(&self) -> &Adf {
        &self.rd3d
    }

    pub fn info(&self) -> Option<&Adf> {
        self.info.as_ref()
    }

    fn coarse_grid(&self, payload: &[u8]) -> Grid<i16> {
        Grid {
            width: self.heights.width,
            height: self.heights.height,
            data: i16s(payload),
        }
    }

    /// Encodes `(gamedata.data, rd3d.data)` with all edits applied.
    pub fn to_members(&self) -> Result<(Vec<u8>, Vec<u8>)> {
        let mut gd = self.gamedata.clone();
        let mut rd = self.rd3d.clone();
        let (w, h) = (self.heights.width, self.heights.height);
        let edge = |i: usize| i % w == w - 1 || i / w == h - 1;

        // Heights: rd3d keeps the full grid; gamedata keeps its last row and
        // column at zero like every shipped island.
        let old = self.coarse_grid(self.rd3d.payload(self.rd_heights));
        let mut new = old.clone();
        let mut gd_h = i16s(self.gamedata.payload(self.gd_heights));
        let mut heights_changed = false;
        for (i, &m) in self.heights.data.iter().enumerate() {
            let q = quantize_height(m);
            if q != old.data[i] {
                heights_changed = true;
                new.data[i] = q;
                if !edge(i) {
                    gd_h[i] = gamedata_height(q);
                }
            }
        }
        if heights_changed {
            rd.set_payload(self.rd_heights, &i16_bytes(&new.data));
            gd.set_payload(self.gd_heights, &i16_bytes(&gd_h));
            self.write_min_max(&mut rd, &old, &new);
        }
        self.widen_min_max_for_patches(&mut rd);
        let pages = self.edited_normal_pages();
        if let Some(list) = rd.node(rd.root(), "Terrain/AllNormalMapLevels/Levels") {
            let levels: Vec<_> = rd.children(list).filter(|n| rd.is_node(*n)).collect();
            for ((node, flags), original) in levels.into_iter().zip(&pages).zip(&self.normal_pages)
            {
                let item = rd.value(node, "Flags/map").context("normal-map flags")?;
                if flags.data != original.data {
                    let mut bytes = rd.payload(item).to_vec();
                    for (i, (&now, &old)) in flags.data.iter().zip(&original.data).enumerate() {
                        if now != old {
                            bytes[i] = u8::from(now);
                        }
                    }
                    rd.set_payload(item, &bytes);
                }
            }
        }

        if let Some(disp) = &self.displacement {
            let old_disp = self.rd3d.payload(disp.rd3d);
            if old_disp != disp.grid.data.as_slice() {
                let mut gd_d = i16s(self.gamedata.payload(disp.gamedata));
                for (i, (&a, &b)) in old_disp.iter().zip(&disp.grid.data).enumerate() {
                    if a != b && !edge(i) {
                        gd_d[i] = gamedata_displacement(b);
                    }
                }
                rd.set_payload(disp.rd3d, &disp.grid.data);
                gd.set_payload(disp.gamedata, &i16_bytes(&gd_d));
            }
        }

        self.write_materials(&mut rd)?;
        match self.material_set_item {
            Some(item) if utf16_bytes(&self.material_set) != self.rd3d.payload(item) => {
                rd.set_payload(item, &utf16_bytes(&self.material_set))
            }
            None if self.material_set != "data/config/engine/material_sets/terra_01.xml" => {
                let terrain = rd.child_node(rd.root(), "Terrain").expect("parsed terrain");
                rd.add_value(
                    terrain,
                    Some("MaterialSetFileName"),
                    &utf16_bytes(&self.material_set),
                )?;
            }
            _ => {}
        }

        let old_active = unpack_bits(
            self.rd3d.payload(self.active_bits),
            self.active_map.width,
            self.active_map.height,
        )?;
        if old_active != self.active_map.data {
            let packed = pack_bits_over(
                self.rd3d.payload(self.active_bits),
                &self.active_map.data,
                self.active_map.width,
            );
            rd.set_payload(self.active_bits, &packed);
            self.write_has_mesh(&mut rd, &old_active)?;
        }

        self.write_props(&mut rd)?;
        Ok((gd.encode(), rd.encode()))
    }

    /// Re-encodes the `.a7minfo` with the edited buildable mask
    /// (`ActiveMapGrid` and `ActiveMapRect`); `None` without one.
    pub fn to_info(&self) -> Option<Vec<u8>> {
        let info = self.info.as_ref()?;
        let mut info = info.clone();
        let (aw, ah) = (self.active_map.width, self.active_map.height);
        if let Some(bits) = self.info_active_bits
            && info.payload(bits).len() == row_bytes(aw) * ah
        {
            let packed = pack_bits_over(info.payload(bits), &self.active_map.data, aw);
            if info.payload(bits) != packed.as_slice() {
                info.set_payload(bits, &packed);
            }
        }
        // ActiveMapRect = bounding box of active cells in metres
        // (x0, y0, x1, y1), exact on all 38 shipped roman islands.
        // TODO(alex): unknown what the editor writes for an empty mask; the
        // old rect is kept.
        let cell = self.world_size[0] as usize / aw.max(1);
        let active: Vec<(usize, usize)> = (0..aw * ah)
            .filter(|&i| self.active_map.data[i])
            .map(|i| (i % aw, i / aw))
            .collect();
        if let (Some(rect), false) = (self.info_active_rect, active.is_empty()) {
            let x0 = active.iter().map(|p| p.0).min().unwrap();
            let x1 = active.iter().map(|p| p.0).max().unwrap();
            let y0 = active.iter().map(|p| p.1).min().unwrap();
            let y1 = active.iter().map(|p| p.1).max().unwrap();
            let r = [x0 * cell, y0 * cell, (x1 + 1) * cell, (y1 + 1) * cell];
            let bytes: Vec<u8> = r.iter().flat_map(|&v| (v as u32).to_le_bytes()).collect();
            if info.payload(rect) != bytes.as_slice() {
                info.set_payload(rect, &bytes);
            }
        }
        Some(info.encode())
    }

    fn write_min_max(&self, rd: &mut Adf, old: &Grid<i16>, new: &Grid<i16>) {
        let l0 = &self.levels[0];
        let (mut before, mut after) = (
            level0_min_max(old, l0.width, l0.height),
            level0_min_max(new, l0.width, l0.height),
        );
        for (k, level) in self.levels.iter().enumerate() {
            if k > 0 {
                let p = &self.levels[k - 1];
                let (pw, ph, w, h) = (p.width, p.height, level.width, level.height);
                before = mip_min_max(&before.0, &before.1, pw, ph, w, h);
                after = mip_min_max(&after.0, &after.1, pw, ph, w, h);
            }
            let shipped_min = i16s(self.rd3d.payload(level.min_y));
            let shipped_max = i16s(self.rd3d.payload(level.max_y));
            let min_y = merge(&shipped_min, &before.0, &after.0);
            let max_y = merge(&shipped_max, &before.1, &after.1);
            // Coarser levels follow the merged level, not the raw recompute,
            // so a kept shipped cell propagates exactly as shipped.
            before = (shipped_min, shipped_max);
            after = (min_y, max_y);
            rd.set_payload(level.min_y, &i16_bytes(&after.0));
            rd.set_payload(level.max_y, &i16_bytes(&after.1));
        }
    }

    // MinY/MaxY bound what the level draws; patches can reach above or below
    // the heightfield (overhangs), so every level's cells under a patch
    // triangle widen to cover it.
    fn widen_min_max_for_patches(&self, rd: &mut Adf) {
        if self.mesh_patches.is_empty() {
            return;
        }
        for (k, level) in self.levels.iter().enumerate() {
            let cell = (1u32 << k) as f32;
            let mut min_y = i16s(rd.payload(level.min_y));
            let mut max_y = i16s(rd.payload(level.max_y));
            for t in self.mesh_patches.iter().flat_map(|p| &p.triangles) {
                let lo = |a: usize| t.iter().map(|p| p[a]).fold(f32::MAX, f32::min);
                let hi = |a: usize| t.iter().map(|p| p[a]).fold(f32::MIN, f32::max);
                let cells = |a: usize, n: usize| {
                    let first = (lo(a) / cell).floor().clamp(0., n as f32 - 1.) as usize;
                    let last = (hi(a) / cell).floor().clamp(0., n as f32 - 1.) as usize;
                    first..=last
                };
                let (bottom, top) = (lo(1).floor() as i16, hi(1).ceil() as i16);
                for y in cells(2, level.height) {
                    for x in cells(0, level.width) {
                        let i = y * level.width + x;
                        min_y[i] = min_y[i].min(bottom);
                        max_y[i] = max_y[i].max(top);
                    }
                }
            }
            rd.set_payload(level.min_y, &i16_bytes(&min_y));
            rd.set_payload(level.max_y, &i16_bytes(&max_y));
        }
    }

    fn write_has_mesh(&self, rd: &mut Adf, old_active: &[bool]) -> Result<()> {
        let sizes: Vec<(usize, usize)> = self.levels.iter().map(|l| (l.width, l.height)).collect();
        let old_grid = Grid {
            width: self.active_map.width,
            height: self.active_map.height,
            data: old_active.to_vec(),
        };
        let before = has_mesh_levels(&old_grid, &sizes);
        let after = has_mesh_levels(&self.active_map, &sizes);
        for (k, level) in self.levels.iter().enumerate() {
            let shipped =
                unpack_bits(self.rd3d.payload(level.has_mesh), level.width, level.height)?;
            let bits = merge(&shipped, &before[k], &after[k]);
            let packed = pack_bits_over(self.rd3d.payload(level.has_mesh), &bits, level.width);
            rd.set_payload(level.has_mesh, &packed);
        }
        Ok(())
    }

    fn write_materials(&self, rd: &mut Adf) -> Result<()> {
        let old_ids = u32s(self.rd3d.payload(self.ids_item));
        let old_alphas = u32s(self.rd3d.payload(self.alphas_item));
        let coarse_changed =
            old_ids != self.material_ids.data || old_alphas != self.material_alphas.data;
        if coarse_changed {
            rd.set_payload(self.ids_item, &u32_bytes(&self.material_ids.data));
            rd.set_payload(self.alphas_item, &u32_bytes(&self.material_alphas.data));
        }
        let full_changed = self
            .full_materials
            .as_ref()
            .is_some_and(|f| f.original.0 != f.ids.data || f.original.1 != f.alphas.data);
        if !coarse_changed && !full_changed {
            return Ok(());
        }
        let Some((bits_item, mips_item)) = self.used_bits else {
            // TODO(alex): without UsedMaterialBits the engine regenerates it;
            // no shipped island lacks it, so nothing is added here.
            return Ok(());
        };
        let map = self.material_map_size;
        let regrid = |data: &[u32], like: &Grid<u32>| Grid {
            width: like.width,
            height: like.height,
            data: data.to_vec(),
        };
        // Exact path: regenerate from the attached full-resolution layer.
        // Fallback: the 1 m copy, OR-ed into the shipped bits.
        let (before, after, exact) = match &self.full_materials {
            Some(f) => (
                used_material_bits(
                    &regrid(&f.original.0, &f.ids),
                    &regrid(&f.original.1, &f.alphas),
                    map,
                ),
                used_material_bits(&f.ids, &f.alphas, map),
                true,
            ),
            None => (
                used_material_bits(
                    &regrid(&old_ids, &self.material_ids),
                    &regrid(&old_alphas, &self.material_alphas),
                    map,
                ),
                used_material_bits(&self.material_ids, &self.material_alphas, map),
                false,
            ),
        };
        let shipped = u32s(self.rd3d.payload(bits_item));
        ensure!(
            shipped.len() == after.len() && self.rd3d.read_u32(mips_item)? == used_mip_count(map),
            "UsedMaterialBits: shipped layout ({} words) does not match MaterialMapWidth",
            shipped.len()
        );
        // With level 0 attached the generator reproduces shipped bits
        // exactly, so every word is written from it. Keeping stored words
        // where this edit changed nothing carried a stale value forward from
        // save to save (a new island kept its template's bits); the game
        // bakes no material whose bit is missing, and such ground drew white.
        let merged: Vec<u32> = if exact {
            after
        } else {
            shipped
                .iter()
                .zip(before.iter().zip(&after))
                .map(|(&s, (&b, &a))| if b == a { s } else { s | a })
                .collect()
        };
        rd.set_payload(bits_item, &u32_bytes(&merged));
        Ok(())
    }

    fn write_props(&self, rd: &mut Adf) -> Result<()> {
        let (old_files, old_props) = parse_props(&self.rd3d, self.file_names, self.instances)?;
        if old_files == self.prop_files && old_props == self.props {
            return Ok(());
        }
        // Keep file order, drop unreferenced names, remap indices.
        let mut used = vec![false; self.prop_files.len()];
        for (i, p) in self.props.iter().enumerate() {
            let slot = used
                .get_mut(p.file as usize)
                .with_context(|| format!("prop {i}: file index {} out of range", p.file))?;
            *slot = true;
        }
        let mut remap = vec![u32::MAX; used.len()];
        rd.clear_children(self.file_names);
        let kept = self.prop_files.iter().enumerate().filter(|(i, _)| used[*i]);
        for (next, (i, file)) in kept.enumerate() {
            rd.add_value(self.file_names, None, &utf16_bytes(file))?;
            remap[i] = next as u32;
        }
        rd.clear_children(self.instances);
        for p in &self.props {
            let n = rd.add_node(self.instances, None)?;
            rd.add_value(n, Some("Index"), &remap[p.file as usize].to_le_bytes())?;
            rd.add_value(n, Some("Position"), &f32_bytes(&p.position))?;
            rd.add_value(n, Some("Rotation"), &f32_bytes(&p.rotation))?;
            rd.add_value(n, Some("Scale"), &f32_bytes(&p.scale))?;
            rd.add_value(n, Some("Color"), &f32_bytes(&p.color))?;
            if let Some(a) = p.adapt_terrain_height {
                rd.add_value(n, Some("AdaptTerrainHeight"), &[a])?;
            }
        }
        Ok(())
    }
}

// ------------------------------------------------------------ material maps
//
// The game renders materials from `_gamedata/<island>/materialmaps/<level>/
// {matid,matalpha}.dds`, levels 0..3 halving each time; rd3d's 1 m arrays
// equal level 3. Weights are not normalized on disk (single-material texels
// hold 31, mixes sum higher), so painting blends weights and lets the shader
// normalize.

/// Header bytes before the texels of a DX10 DDS.
const DDS_DX10_HEADER: usize = 148;

/// Texels of a materialmaps DDS (DX10, R16G16_UNORM) as packed slot words.
pub fn material_words(dds: &[u8]) -> Result<Grid<u32>> {
    ensure!(
        dds.len() >= DDS_DX10_HEADER && dds.starts_with(b"DDS ") && &dds[84..88] == b"DX10",
        "not a DX10 DDS"
    );
    let word = |at: usize| u32::from_le_bytes(dds[at..at + 4].try_into().expect("four bytes"));
    ensure!(
        word(128) == 36,
        "material map is DXGI {}, expected 36 (R16G16_UNORM)",
        word(128)
    );
    let (height, width) = (word(12) as usize, word(16) as usize);
    let end = DDS_DX10_HEADER + width * height * 4;
    ensure!(
        dds.len() >= end,
        "material map {width}x{height} is truncated"
    );
    let data = dds[DDS_DX10_HEADER..end]
        .as_chunks::<4>()
        .0
        .iter()
        .map(|b| u32::from_le_bytes(*b))
        .collect();
    Ok(Grid {
        width,
        height,
        data,
    })
}

/// Replaces the texels of a materialmaps DDS, keeping its header (and any
/// trailing bytes) exactly as shipped.
pub fn material_dds(template: &[u8], grid: &Grid<u32>) -> Result<Vec<u8>> {
    let current = material_words(template)?;
    ensure!(
        (current.width, current.height) == (grid.width, grid.height),
        "material map is {}x{}, edit is {}x{}",
        current.width,
        current.height,
        grid.width,
        grid.height
    );
    let mut out = template.to_vec();
    for (dst, w) in out[DDS_DX10_HEADER..]
        .as_chunks_mut::<4>()
        .0
        .iter_mut()
        .zip(&grid.data)
    {
        *dst = w.to_le_bytes();
    }
    Ok(out)
}

/// Paints material `id` at alpha texel (x, y) with `amount` 0..1. When the
/// texel's 2x2 id block lacks `id`, the slot with the least weight across the
/// block's four texels takes it over and its old weights are cleared.
pub fn paint_material(
    ids: &mut Grid<u32>,
    alphas: &mut Grid<u32>,
    x: usize,
    y: usize,
    id: u8,
    amount: f32,
) {
    let (bx, by) = (x / 2, y / 2);
    if bx >= ids.width || by >= ids.height || x >= alphas.width || y >= alphas.height {
        return;
    }
    let mut block = unpack_slots(ids.get(bx, by));
    let slot = match block.iter().position(|&s| s == id & 31) {
        Some(slot) => slot,
        None => {
            // Painting runs this per texel; only a takeover needs the block.
            let texels: Vec<(usize, usize)> = [(0, 0), (1, 0), (0, 1), (1, 1)]
                .iter()
                .map(|(dx, dy)| (bx * 2 + dx, by * 2 + dy))
                .filter(|&(tx, ty)| tx < alphas.width && ty < alphas.height)
                .collect();
            let weight = |k: usize| -> u32 {
                texels
                    .iter()
                    .map(|&(tx, ty)| u32::from(unpack_slots(alphas.get(tx, ty))[k]))
                    .sum()
            };
            let slot = (0..6).min_by_key(|&k| weight(k)).expect("six slots");
            block[slot] = id & 31;
            ids.set(bx, by, pack_slots(block));
            for &(tx, ty) in &texels {
                let mut w = unpack_slots(alphas.get(tx, ty));
                w[slot] = 0;
                alphas.set(tx, ty, pack_slots(w));
            }
            slot
        }
    };
    let amount = amount.clamp(0., 1.);
    let mut w = unpack_slots(alphas.get(x, y));
    for (k, v) in w.iter_mut().enumerate() {
        let target = if k == slot { 31. } else { 0. };
        *v = (f32::from(*v) + (target - f32::from(*v)) * amount).round() as u8;
    }
    alphas.set(x, y, pack_slots(w));
}

/// The six ids of a block with repeats replaced by the lowest ids the
/// block does not use. Shipped blocks always hold six different ids, the
/// unused slots padded with low ids and zero weight; the game draws blocks
/// with a repeated id as hard-edged patches.
pub fn distinct_ids(block: [u8; 6]) -> [u8; 6] {
    let mut out = block;
    let mut spare = (0u8..32).filter(|id| !block.contains(id));
    for k in 1..6 {
        if out[..k].contains(&out[k]) {
            out[k] = spare.next().expect("32 ids, at most 6 used");
        }
    }
    out
}

/// Gives every id block six different ids. A repeated id's weight moves to
/// its first slot (same material, same total) and the repeat becomes a
/// spare id with zero weight. Returns how many blocks changed.
pub fn make_slots_distinct(ids: &mut Grid<u32>, alphas: &mut Grid<u32>) -> usize {
    let per = (alphas.width / ids.width.max(1)).max(1);
    let mut changed = 0;
    for by in 0..ids.height {
        for bx in 0..ids.width {
            let block = unpack_slots(ids.get(bx, by));
            let fixed = distinct_ids(block);
            if fixed == block {
                continue;
            }
            changed += 1;
            ids.set(bx, by, pack_slots(fixed));
            for y in by * per..((by + 1) * per).min(alphas.height) {
                for x in bx * per..((bx + 1) * per).min(alphas.width) {
                    let mut w = unpack_slots(alphas.get(x, y));
                    for k in 1..6 {
                        if fixed[k] != block[k] {
                            let first = block.iter().position(|&id| id == block[k]).unwrap();
                            w[first] = (w[first] + w[k]).min(31);
                            w[k] = 0;
                        }
                    }
                    alphas.set(x, y, pack_slots(w));
                }
            }
        }
    }
    changed
}

/// Next coarser material level. Each coarse id block takes the top-left fine
/// block's ids; each coarse alpha takes its top-left fine texel, remapped onto
/// that block by id (ids the block lacks are dropped).
// This is how shipped levels are made: it matches their dominant material
// on 99.4/98.7/97.1 % of level 1/2/3 texels.
pub fn downsample_materials(ids: &Grid<u32>, alphas: &Grid<u32>) -> (Grid<u32>, Grid<u32>) {
    let (iw, ih) = (ids.width.div_ceil(2), ids.height.div_ceil(2));
    let (aw, ah) = (alphas.width.div_ceil(2), alphas.height.div_ceil(2));
    let coarse_ids = Grid {
        width: iw,
        height: ih,
        data: (0..ih)
            .flat_map(|y| (0..iw).map(move |x| (x, y)))
            .map(|(x, y)| ids.get((x * 2).min(ids.width - 1), (y * 2).min(ids.height - 1)))
            .collect(),
    };
    let coarse_alphas = Grid {
        width: aw,
        height: ah,
        data: (0..ah)
            .flat_map(|y| (0..aw).map(move |x| (x, y)))
            .map(|(x, y)| {
                let (fx, fy) = (
                    (x * 2).min(alphas.width - 1),
                    (y * 2).min(alphas.height - 1),
                );
                let fine_ids = unpack_slots(
                    ids.get((fx / 2).min(ids.width - 1), (fy / 2).min(ids.height - 1)),
                );
                let fine_w = unpack_slots(alphas.get(fx, fy));
                let block = unpack_slots(coarse_ids.get((x / 2).min(iw - 1), (y / 2).min(ih - 1)));
                let mut out = [0u8; 6];
                for k in 0..6 {
                    if fine_w[k] == 0 {
                        continue;
                    }
                    if let Some(slot) = block.iter().position(|&b| b == fine_ids[k]) {
                        out[slot] = out[slot].saturating_add(fine_w[k]).min(31);
                    }
                }
                pack_slots(out)
            })
            .collect(),
    };
    (coarse_ids, coarse_alphas)
}

impl Island {
    /// Recomputes levels 1..3 from the attached level 0 and makes rd3d's 1 m
    /// arrays equal level 3. Returns all four levels for the DDS files, or
    /// `None` without an attached level 0.
    pub fn sync_material_levels(&mut self) -> Option<[(Grid<u32>, Grid<u32>); 4]> {
        // Whatever painted or imported them, blocks leave with six
        // different ids, as shipped.
        if let Some(f) = self.full_materials.as_mut() {
            make_slots_distinct(&mut f.ids, &mut f.alphas);
        }
        let full = self.full_materials.as_ref()?;
        let l0 = (full.ids.clone(), full.alphas.clone());
        let l1 = downsample_materials(&l0.0, &l0.1);
        let l2 = downsample_materials(&l1.0, &l1.1);
        let l3 = downsample_materials(&l2.0, &l2.1);
        if (l3.0.width, l3.0.height) == (self.material_ids.width, self.material_ids.height)
            && (l3.1.width, l3.1.height)
                == (self.material_alphas.width, self.material_alphas.height)
        {
            self.material_ids = l3.0.clone();
            self.material_alphas = l3.1.clone();
        }
        Some([l0, l1, l2, l3])
    }
}

// ------------------------------------------------- terrain meshes and pages
//
// `meshes/<level>/<x>x<y>.tmc` and `normalmaps/<level>/<x>x<z>.ctt` under
// `_gamedata/<island>/` follow the heights, buildable mask and materials.
// Both rebuilds generate everything from the shipped inputs and from the
// edited ones and only rewrite what differs, keeping shipped nodes and page
// detail the 0.5 m heightfield cannot reproduce.

/// A file under `_gamedata/<island>/` to stage in the mod archive.
#[derive(Debug, Clone, PartialEq)]
pub struct TerrainFile {
    /// Relative path, e.g. `meshes/2/1x0.tmc`.
    pub path: String,
    pub bytes: Vec<u8>,
}

// What the meshes are generated from.
struct MeshInputs {
    heights: Grid<f32>,
    active: Grid<bool>,
    materials: Grid<u32>,
}

impl Island {
    /// Triangle patches (overhangs, imported meshes) that replace the
    /// finest mesh level in the 4 m nodes they cover. They must sit inside
    /// the buildable area; MinY/MaxY widen to cover them on save.
    /// Marks the island as new under its own path: there are no shipped
    /// terrain files to patch, so [`Island::rebuild_meshes`] and
    /// [`Island::rebuild_height_pages`] return every tile and page.
    pub fn mark_unshipped(&mut self) {
        self.unshipped = true;
    }

    pub fn mesh_patches(&self) -> &[MeshPatch] {
        &self.mesh_patches
    }

    pub fn mesh_patches_mut(&mut self) -> &mut Vec<MeshPatch> {
        &mut self.mesh_patches
    }

    fn mesh_inputs(&self, edited: bool) -> MeshInputs {
        let (w, h) = (self.heights.width, self.heights.height);
        let heights = if edited {
            self.heights
                .data
                .iter()
                .map(|&m| f32::from(quantize_height(m)) / HEIGHT_UNITS_PER_METRE)
                .collect()
        } else {
            i16s(self.rd3d.payload(self.rd_heights))
                .into_iter()
                .map(|v| f32::from(v) / HEIGHT_UNITS_PER_METRE)
                .collect()
        };
        let active = if edited {
            self.active_map.clone()
        } else {
            Grid {
                width: self.active_map.width,
                height: self.active_map.height,
                data: unpack_bits(
                    self.rd3d.payload(self.active_bits),
                    self.active_map.width,
                    self.active_map.height,
                )
                .expect("ActiveMap validated on parse"),
            }
        };
        let regrid = |data: Vec<u32>, like: &Grid<u32>| Grid {
            width: like.width,
            height: like.height,
            data,
        };
        let materials = match (&self.full_materials, edited) {
            (Some(f), true) => material_mask(&f.ids, &f.alphas),
            (Some(f), false) => material_mask(
                &regrid(f.original.0.clone(), &f.ids),
                &regrid(f.original.1.clone(), &f.alphas),
            ),
            (None, true) => material_mask(&self.material_ids, &self.material_alphas),
            (None, false) => material_mask(
                &regrid(u32s(self.rd3d.payload(self.ids_item)), &self.material_ids),
                &regrid(
                    u32s(self.rd3d.payload(self.alphas_item)),
                    &self.material_alphas,
                ),
            ),
        };
        MeshInputs {
            heights: Grid {
                width: w,
                height: h,
                data: heights,
            },
            active,
            materials,
        }
    }

    // Every tile of every level, `(level, x, y, tile)`.
    fn generate_meshes(
        &self,
        inputs: &MeshInputs,
        patches: &[MeshPatch],
    ) -> Result<Vec<(usize, u32, u32, Tile)>> {
        // Levels are independent; the finest dominate, so one thread each
        // is enough parallelism.
        let levels: Vec<Result<Vec<_>>> = std::thread::scope(|scope| {
            let workers: Vec<_> = (0..self.levels.len())
                .map(|k| scope.spawn(move || self.generate_level(inputs, patches, k)))
                .collect();
            workers
                .into_iter()
                .map(|w| w.join().expect("mesh level"))
                .collect()
        });
        let mut out = Vec::new();
        for level in levels {
            out.extend(level?);
        }
        Ok(out)
    }

    fn generate_level(
        &self,
        inputs: &MeshInputs,
        patches: &[MeshPatch],
        k: usize,
    ) -> Result<Vec<(usize, u32, u32, Tile)>> {
        let mut out = Vec::new();
        {
            let level = &self.levels[k];
            let nodes = (level.width, level.height);
            let bits = node_material_bits(&inputs.materials, self.world_size, k, nodes);
            let source = tmc::LevelSource {
                level: k as u32,
                heights: &inputs.heights,
                active: &inputs.active,
                active_cell: self.world_size[0] as usize / inputs.active.width.max(1),
                nodes,
                material_bits: &bits,
                geometry: k >= self.min_mesh_level,
                patches: if k == self.min_mesh_level {
                    patches
                } else {
                    &[]
                },
            };
            for (x, y, tile) in
                tmc::build_level(&source).with_context(|| format!("mesh level {k}"))?
            {
                out.push((k, x, y, tile));
            }
        }
        Ok(out)
    }

    /// Mesh tiles that differ after the edits. `shipped` returns the current
    /// bytes of a relative path (`meshes/2/1x0.tmc`), or `None` for a tile
    /// the island does not have yet. Nodes whose inputs did not change keep
    /// their shipped geometry; an unedited island yields nothing.
    pub fn rebuild_meshes(
        &self,
        shipped: &mut dyn FnMut(&str) -> Option<Vec<u8>>,
    ) -> Result<Vec<TerrainFile>> {
        let (before, after) = (self.mesh_inputs(false), self.mesh_inputs(true));
        if !self.unshipped
            && before.heights == after.heights
            && before.active == after.active
            && before.materials == after.materials
            && self.mesh_patches.is_empty()
        {
            return Ok(Vec::new());
        }
        let (old, new) = std::thread::scope(|scope| {
            let old = scope.spawn(|| self.generate_meshes(&before, &[]));
            let new = self.generate_meshes(&after, &self.mesh_patches);
            (old.join().expect("mesh generation"), new)
        });
        let (old, new) = (old?, new?);
        let mut files = Vec::new();
        for ((level, x, y, was), (_, _, _, now)) in old.iter().zip(&new) {
            let changed: Vec<usize> = (0..now.nodes.len())
                .filter(|&i| self.unshipped || was.nodes[i] != now.nodes[i])
                .collect();
            if changed.is_empty() {
                continue;
            }
            let path = format!("meshes/{level}/{x}x{y}.tmc");
            let mut tile = match shipped(&path).filter(|_| !self.unshipped) {
                Some(bytes) => Tile::parse(&bytes).with_context(|| path.clone())?,
                None => now.clone(),
            };
            for i in changed {
                tile.nodes[i] = now.nodes[i].clone();
            }
            files.push(TerrainFile {
                path,
                bytes: Adf::decode(&tile.encode())?.encode_like(&self.rd3d),
            });
        }
        Ok(files)
    }

    // Missing pages are added where edited heights intersect active terrain.
    // Existing flags stay intact, including noncanonical nonzero flag bytes.
    fn edited_normal_pages(&self) -> Vec<Grid<bool>> {
        let mut pages = self.normal_pages.clone();
        if self.unshipped {
            for grid in &mut pages {
                grid.data.fill(false);
            }
        }
        let old = self.coarse_grid(self.rd3d.payload(self.rd_heights));
        let new = &self.heights;
        let old_active = unpack_bits(
            self.rd3d.payload(self.active_bits),
            self.active_map.width,
            self.active_map.height,
        )
        .expect("ActiveMap validated on parse");
        for z in 0..new.height {
            for x in 0..new.width {
                let i = z * new.width + x;
                let (wx, wz) = (x as f32 * 0.5, z as f32 * 0.5);
                let active = &self.active_map;
                let ax = (wx / self.world_size[0] as f32 * active.width as f32) as usize;
                let az = (wz / self.world_size[1] as f32 * active.height as f32) as usize;
                let (ax, az) = (ax.min(active.width - 1), az.min(active.height - 1));
                if !active.get(ax, az) {
                    continue;
                }
                if !self.unshipped
                    && old.data[i] == quantize_height(new.data[i])
                    && old_active[az * active.width + ax]
                {
                    continue;
                }
                for (k, grid) in pages.iter_mut().enumerate() {
                    let side = self.page_metres * (1u32 << k) as f32;
                    let border = side * ctt::BORDER as f32 / ctt::PAGE_TEXELS as f32;
                    let span = |p: f32, n: usize| {
                        let lo = ((p - border) / side).floor().max(0.) as usize;
                        let hi = (((p + border) / side).floor().max(0.) as usize + 1).min(n);
                        lo.min(n)..hi
                    };
                    for py in span(wz, grid.height) {
                        for px in span(wx, grid.width) {
                            grid.set(px, py, true);
                        }
                    }
                }
            }
        }
        pages
    }

    /// Height pages (`normalmaps/<level>/<x>x<z>.ctt`) under edited heights.
    /// Each touched page gets the height change added to its shipped heights,
    /// so detail finer than the heightfield survives. Missing pages under
    /// edited active terrain are generated from the heightfield.
    pub fn rebuild_height_pages(
        &self,
        shipped: &mut dyn FnMut(&str) -> Option<Vec<u8>>,
    ) -> Result<Vec<TerrainFile>> {
        let (before, after) = (self.mesh_inputs(false), self.mesh_inputs(true));
        if !self.unshipped && before.heights == after.heights {
            return Ok(Vec::new());
        }
        let (old, new) = (&before.heights, &after.heights);
        let sample = |g: &Grid<f32>, x: f32, z: f32| {
            let (gx, gz) = (
                (x * 2.).clamp(0., (g.width - 1) as f32),
                (z * 2.).clamp(0., (g.height - 1) as f32),
            );
            let (x0, z0) = (
                (gx as usize).min(g.width - 2),
                (gz as usize).min(g.height - 2),
            );
            let (fx, fz) = (gx - x0 as f32, gz - z0 as f32);
            let at = |x, z| g.data[z * g.width + x];
            (at(x0, z0) * (1. - fx) + at(x0 + 1, z0) * fx) * (1. - fz)
                + (at(x0, z0 + 1) * (1. - fx) + at(x0 + 1, z0 + 1) * fx) * fz
        };
        let mut files = Vec::new();
        for (k, flags) in self.edited_normal_pages().iter().enumerate() {
            let page = self.page_metres * (1u32 << k) as f32;
            let texel = page / ctt::PAGE_TEXELS as f32;
            for (i, _) in flags.data.iter().enumerate().filter(|(_, f)| **f) {
                let (px, pz) = (i % flags.width, i / flags.width);
                let origin = |p: usize| p as f32 * page - ctt::BORDER as f32 * texel;
                let (x0, z0) = (origin(px), origin(pz));
                // Height samples the page reads, one extra for bilinear.
                let span = |o: f32, n: usize| {
                    let lo = ((o * 2.).floor() as isize - 1).clamp(0, n as isize - 1) as usize;
                    let hi = (((o + ctt::SIDE as f32 * texel) * 2.).ceil() as isize + 1)
                        .clamp(0, n as isize - 1) as usize;
                    lo..=hi
                };
                let touched = self.unshipped
                    || !self.normal_pages[k].get(px, pz)
                    || span(z0, old.height).any(|z| {
                        span(x0, old.width)
                            .any(|x| old.data[z * old.width + x] != new.data[z * new.width + x])
                    });
                if !touched {
                    continue;
                }
                let path = format!("normalmaps/{k}/{px}x{pz}.ctt");
                let base = shipped(&path)
                    .filter(|_| !self.unshipped && self.normal_pages[k].get(px, pz))
                    .map(|b| HeightPage::parse(&b).with_context(|| path.clone()))
                    .transpose()?
                    .map(|p| p.heights());
                let mut heights = Grid {
                    width: ctt::SIDE,
                    height: ctt::SIDE,
                    data: vec![0.; ctt::SIDE * ctt::SIDE],
                };
                for j in 0..ctt::SIDE * ctt::SIDE {
                    let (x, z) = (
                        x0 + (j % ctt::SIDE) as f32 * texel,
                        z0 + (j / ctt::SIDE) as f32 * texel,
                    );
                    heights.data[j] = match &base {
                        Some(b) => b.data[j] + sample(new, x, z) - sample(old, x, z),
                        None => sample(new, x, z),
                    };
                }
                files.push(TerrainFile {
                    path,
                    bytes: HeightPage::from_heights(&heights).encode_like(&self.rd3d),
                });
            }
        }
        Ok(files)
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn edited_terrain_adds_missing_height_pages() {
        use crate::{adf::Adf, island::Island, rda::Archive, vfs::Install};
        let Some(dir) = std::env::var_os("ANNO1800_DIR") else {
            return;
        };
        let install = Install::open(dir).unwrap();
        let path = "data/sessions/islands/pool/moderate/moderate_m_01/moderate_m_01.a7m";
        let archive = Archive::from_bytes(install.read(path).unwrap(), path).unwrap();
        let gd = archive.read_path("gamedata.data").unwrap();
        let mut rd = Adf::decode(&archive.read_path("rd3d.data").unwrap()).unwrap();
        let levels = rd
            .node(rd.root(), "Terrain/AllNormalMapLevels/Levels")
            .unwrap();
        let level = rd.children(levels).find(|n| rd.is_node(*n)).unwrap();
        let flags = rd.value(level, "Flags/map").unwrap();
        rd.set_payload(flags, &vec![0; rd.payload(flags).len()]);
        let mut island = Island::parse(&gd, &rd.encode()).unwrap();
        let (x, z) = (320, 320);
        let height = island.heights().get(x, z);
        island.heights_mut().set(x, z, height + 2.);
        let pages = island.rebuild_height_pages(&mut |_| None).unwrap();
        assert!(
            pages.iter().any(|p| p.path.starts_with("normalmaps/0/")),
            "no fine height page generated"
        );
        let (_, rd) = island.to_members().unwrap();
        let rd = Adf::decode(&rd).unwrap();
        let levels = rd
            .node(rd.root(), "Terrain/AllNormalMapLevels/Levels")
            .unwrap();
        let level = rd.children(levels).find(|n| rd.is_node(*n)).unwrap();
        let flags = rd.value(level, "Flags/map").unwrap();
        assert!(rd.payload(flags).iter().any(|b| *b != 0));
    }

    use std::path::{Path, PathBuf};

    use super::*;
    use crate::rda::Archive;

    #[test]
    fn installed_1800_playable_islands_and_headers_roundtrip() {
        let Some(dir) = std::env::var_os("ANNO1800_DIR") else {
            return;
        };
        let install = crate::vfs::Install::open(dir).unwrap();
        let mut checked = 0;
        for id in install.search(".a7m", 100_000) {
            let path = install.file_path(id);
            if !path.ends_with(".a7m") || path.contains("/_benchmark/") {
                continue;
            }
            let archive = Archive::from_bytes(install.read_file(id).unwrap(), path).unwrap();
            let gd = archive.read_path("gamedata.data").unwrap();
            let rd = archive.read_path("rd3d.data").unwrap();
            let header = install.read(&format!("{path}info")).unwrap();
            let island = Island::parse_with_info(&gd, &rd, &header)
                .unwrap_or_else(|e| panic!("{path}: {e:#}"));
            assert_eq!(island.game(), crate::game::Game::Anno1800);
            assert!(island.displacement().is_none());
            assert_eq!(
                island.material_alphas().width,
                island.world_size()[0] as usize * 2
            );
            let (g, r) = island.to_members().unwrap();
            assert_eq!(g, gd, "gamedata {path}");
            assert_eq!(r, rd, "rd3d {path}");
            let info = crate::a7minfo::A7mInfo::parse(&header).unwrap();
            assert_eq!(info.encode(), header, "header {path}");
            crate::grids::Grids::read(&island).unwrap();
            checked += 1;
        }
        assert!(checked >= 200, "only {checked} islands checked");
    }

    fn islands_dir() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../extract/provinces_roman/data/base/provinces/roman/islands")
    }

    // (gamedata, rd3d, a7minfo) of an extracted island, e.g. "pool/roman_island_small_07".
    fn members(rel: &str) -> Option<(Vec<u8>, Vec<u8>, Vec<u8>)> {
        let dir = islands_dir().join(rel);
        let name = dir.file_name()?.to_str()?.to_owned();
        let a7m = dir.join(format!("{name}.a7m"));
        if !a7m.exists() {
            eprintln!("skip: {} missing", a7m.display());
            return None;
        }
        let archive = Archive::open(&a7m).unwrap();
        let read = |member: &str| {
            let e = archive
                .entries()
                .iter()
                .find(|e| e.key().ends_with(member))
                .unwrap_or_else(|| panic!("{name}: no {member}"));
            archive.read(e).unwrap()
        };
        let info = std::fs::read(dir.join(format!("{name}.a7minfo"))).unwrap();
        Some((read("gamedata.data"), read("rd3d.data"), info))
    }

    // Different sizes: 128, 256, 320 and 512 m.
    const ISLANDS: [&str; 4] = [
        "deco/roman_island_deco_01",
        "pool/roman_island_small_07",
        "3rdparty/roman_island_3rdparty_pirate_01",
        "pool/roman_island_large_01",
    ];

    // Island plus the shipped (gamedata, rd3d, a7minfo) bytes.
    type Loaded = (Island, Vec<u8>, Vec<u8>, Vec<u8>);

    fn load(rel: &str) -> Option<Loaded> {
        let (gd, rd, info) = members(rel)?;
        let island = Island::parse_with_info(&gd, &rd, &info).unwrap();
        Some((island, gd, rd, info))
    }

    fn level_i16(island: &Island, k: usize) -> (Vec<i16>, Vec<i16>) {
        let l = &island.levels[k];
        (
            i16s(island.rd3d.payload(l.min_y)),
            i16s(island.rd3d.payload(l.max_y)),
        )
    }

    #[test]
    fn unedited_island_is_byte_identical() {
        for rel in ISLANDS {
            let Some((island, gd, rd, info)) = load(rel) else {
                return;
            };
            let (gd2, rd2) = island.to_members().unwrap();
            assert!(gd2 == gd, "{rel}: gamedata differs");
            assert!(rd2 == rd, "{rel}: rd3d differs");
            assert!(island.to_info().unwrap() == info, "{rel}: a7minfo differs");
            let ws = island.world_size();
            assert_eq!(island.heights().width, 2 * ws[0] as usize + 1);
            assert_eq!(island.material_alphas().width, ws[0] as usize);
            assert!(!island.props().is_empty());
            assert!(island.water_infos().is_some() && island.fog().is_some());
            assert!(island.material_set_file_name().ends_with(".xml"));
        }
    }

    #[test]
    fn height_edit_reaches_both_maps_and_mips() {
        let Some((mut island, _, _, _)) = load("pool/roman_island_small_07") else {
            return;
        };
        let w = island.heights().width;
        // A 9x9 plateau at 12.5 m (3200 rd3d units) around texel (100, 120).
        for y in 116..125 {
            for x in 96..105 {
                island.heights_mut().set(x, y, 12.5);
            }
        }
        island.heights_mut().set(w - 1, 3, -3.125); // border texel: rd3d only
        let (gd, rd) = island.to_members().unwrap();
        let back = Island::parse(&gd, &rd).unwrap();
        assert_eq!(back.heights().get(100, 120), 12.5);
        assert_eq!(back.heights().get(w - 1, 3), -3.125);
        let coarse = i16s(back.rd3d.payload(back.rd_heights));
        let gdh = i16s(back.gamedata.payload(back.gd_heights));
        let i = 120 * w + 100;
        assert_eq!(coarse[i], 3200);
        assert_eq!(gdh[i], 1600, "gamedata = rd3d / 2");
        assert_eq!(gdh[3 * w + w - 1], 0, "gamedata border stays zero");
        // Level 0 cell (50, 60) is inside the plateau; Min/MaxY are whole metres.
        let l0 = &back.levels[0];
        let (min_y, max_y) = level_i16(&back, 0);
        assert_eq!(
            (min_y[60 * l0.width + 50], max_y[60 * l0.width + 50]),
            (12, 13)
        );
        // Coarse levels saw the new peak where it wasn't already higher.
        let top = back.levels.len() - 1;
        assert!(level_i16(&back, top).1[0] >= 13);
        // Untouched cells keep shipped values.
        let (orig_min, _) = level_i16(&island, 0);
        assert_eq!(min_y[0], orig_min[0]);
    }

    fn quantized(island: &Island) -> Grid<i16> {
        island.coarse_grid(island.rd3d.payload(island.rd_heights))
    }

    // Recomputes every derived height/mask layer from the shipped inputs and
    // compares with the shipped bytes.
    #[test]
    fn derived_layers_match_shipped_data() {
        for rel in ISLANDS {
            let Some((island, _, _, _)) = load(rel) else {
                return;
            };
            let coarse = quantized(&island);
            let l0 = &island.levels[0];
            let (min0, max0) = level0_min_max(&coarse, l0.width, l0.height);
            let (ship_min, ship_max) = level_i16(&island, 0);
            assert_eq!(min0, ship_min, "{rel}: level-0 MinY");
            // MaxY: exact except the documented +1 on exact whole-metre maxima.
            let mut ambiguous = 0;
            for (i, (&ours, &shipped)) in max0.iter().zip(&ship_max).enumerate() {
                if ours != shipped {
                    assert_eq!(shipped, ours + 1, "{rel}: level-0 MaxY cell {i}");
                    ambiguous += 1;
                }
            }
            let share = ambiguous as f64 / max0.len() as f64;
            assert!(share < 0.005, "{rel}: {ambiguous} MaxY cells off");
            eprintln!(
                "{rel}: level-0 MaxY off by +1 in {ambiguous} of {} cells",
                max0.len()
            );

            // Mips from the shipped level 0 are exact.
            let (mut lo, mut hi) = (ship_min, ship_max);
            for k in 1..island.levels.len() {
                let (p, l) = (&island.levels[k - 1], &island.levels[k]);
                (lo, hi) = mip_min_max(&lo, &hi, p.width, p.height, l.width, l.height);
                assert_eq!(
                    (&lo, &hi),
                    (&level_i16(&island, k).0, &level_i16(&island, k).1),
                    "{rel}: level {k}"
                );
            }

            let sizes: Vec<_> = island.levels.iter().map(|l| (l.width, l.height)).collect();
            for (k, bits) in has_mesh_levels(island.active_map(), &sizes)
                .iter()
                .enumerate()
            {
                let l = &island.levels[k];
                let shipped =
                    unpack_bits(island.rd3d.payload(l.has_mesh), l.width, l.height).unwrap();
                assert!(*bits == shipped, "{rel}: HasMesh level {k}");
            }

            // Shipped gamedata heights follow from rd3d on every interior texel.
            let gdh = i16s(island.gamedata.payload(island.gd_heights));
            let w = coarse.width;
            for (i, (&c, &g)) in coarse.data.iter().zip(&gdh).enumerate() {
                let edge = i % w == w - 1 || i / w == coarse.height - 1;
                assert_eq!(
                    g,
                    if edge { 0 } else { gamedata_height(c) },
                    "{rel}: texel {i}"
                );
            }

            // UsedMaterialBits from the 1 m copy: right layout, conservative
            // shipped data (shipped is a superset).
            let (bits, mips) = island.used_bits.unwrap();
            let generated = used_material_bits(
                &island.material_ids,
                &island.material_alphas,
                island.material_map_size,
            );
            let shipped = u32s(island.rd3d.payload(bits));
            assert_eq!(generated.len(), shipped.len(), "{rel}");
            assert_eq!(
                island.rd3d.read_u32(mips).unwrap(),
                used_mip_count(island.material_map_size)
            );
        }
    }

    #[test]
    fn active_map_edit_updates_mesh_mask_and_info() {
        let Some((mut island, _, _, _)) = load("pool/roman_island_small_07") else {
            return;
        };
        let am = island.active_map().clone();
        let (x, y) = (0..am.width * am.height)
            .map(|i| (i % am.width, i / am.width))
            .find(|&(x, y)| !am.get(x, y) && x == 0)
            .unwrap();
        island.active_map_mut().set(x, y, true);
        let (gd, rd) = island.to_members().unwrap();
        let info = island.to_info().unwrap();
        let back = Island::parse_with_info(&gd, &rd, &info).unwrap();
        assert!(back.active_map().get(x, y));
        let l0 = &back.levels[0];
        let mesh = unpack_bits(back.rd3d.payload(l0.has_mesh), l0.width, l0.height).unwrap();
        let f = l0.width / am.width;
        assert!(mesh[(y * f) * l0.width + x * f]);
        let info = Adf::decode(&info).unwrap();
        let grid = info.payload(info.value(info.root(), "ActiveMapGrid/bits").unwrap());
        assert_eq!(
            unpack_bits(grid, am.width, am.height).unwrap(),
            back.active_map().data
        );
        let rect = u32s(info.payload(info.child_value(info.root(), "ActiveMapRect").unwrap()));
        assert_eq!(rect[0], 0, "column 0 is active now");
    }

    #[test]
    fn props_roundtrip_and_edit() {
        let Some((mut island, _, rd, _)) = load("3rdparty/roman_island_3rdparty_pirate_01") else {
            return;
        };
        let files_before = island.prop_files().to_vec();
        // Rebuild with identical content reproduces the shipped bytes.
        let props = island.props().to_vec();
        let mut rebuilt = island.rd3d.clone();
        island.props_mut().push(props[0].clone());
        island.write_props(&mut rebuilt).unwrap();
        island.props_mut().pop();
        let mut again = rebuilt.clone();
        let mut reparsed = Island::parse(&island.gamedata.encode(), &rebuilt.encode()).unwrap();
        assert_eq!(reparsed.props().len(), props.len() + 1);
        reparsed.props_mut().pop();
        reparsed.write_props(&mut again).unwrap();
        assert!(
            again.encode() == rd,
            "rebuilt prop arrays differ from shipped"
        );

        // Move one, drop every prop of file 0, add one with a new file.
        let moved = props[5].position;
        island.props_mut()[5].position = [1.0, 2.0, 3.0];
        let new_file = island.prop_file_index("data/test/new_prop.cfg");
        island.props_mut().push(Prop {
            file: new_file,
            position: [10.0, 0.5, 20.0],
            rotation: [0.0, 0.0, 0.0, 1.0],
            scale: [2.0; 3],
            color: [1.0; 4],
            adapt_terrain_height: Some(1),
        });
        island.props_mut().retain(|p| p.file != 0);
        let expected: Vec<(String, [f32; 3])> = island
            .props()
            .iter()
            .map(|p| (island.prop_files()[p.file as usize].clone(), p.position))
            .collect();
        let (gd, rd) = island.to_members().unwrap();
        let back = Island::parse(&gd, &rd).unwrap();
        let got: Vec<(String, [f32; 3])> = back
            .props()
            .iter()
            .map(|p| (back.prop_files()[p.file as usize].clone(), p.position))
            .collect();
        assert_eq!(got, expected);
        assert_ne!(moved, [1.0, 2.0, 3.0]);
        assert_eq!(
            back.prop_files().len(),
            files_before.len(),
            "file 0 dropped, one added"
        );
        assert!(!back.prop_files().contains(&files_before[0]));
        assert_eq!(back.prop_files().last().unwrap(), "data/test/new_prop.cfg");
        assert_eq!(back.props().last().unwrap().adapt_terrain_height, Some(1));
    }

    #[test]
    fn material_edit_keeps_used_bits_conservative() {
        let Some((mut island, _, _, _)) = load("pool/roman_island_small_07") else {
            return;
        };
        let (bits, _) = island.used_bits.unwrap();
        let shipped = u32s(island.rd3d.payload(bits));
        // Paint material 30 into slot 5 of one block and its texels.
        let (bx, by) = (40, 40);
        let mut slots = unpack_slots(island.material_ids().get(bx, by));
        slots[5] = 30;
        island.material_ids_mut().set(bx, by, pack_slots(slots));
        for (x, y) in [(80, 80), (81, 80), (80, 81), (81, 81)] {
            let mut a = unpack_slots(island.material_alphas().get(x, y));
            a[5] = 31;
            island.material_alphas_mut().set(x, y, pack_slots(a));
        }
        let (gd, rd) = island.to_members().unwrap();
        let back = Island::parse(&gd, &rd).unwrap();
        assert_eq!(unpack_slots(back.material_ids().get(bx, by))[5], 30);
        let bits_after = u32s(back.rd3d.payload(bits));
        assert!(
            shipped.iter().zip(&bits_after).all(|(s, a)| a & s == *s),
            "bits only grow"
        );
        // Metre 80 -> material-map texel 640 -> base cell 160; top mip has it too.
        let base_w = island.material_map_size[0] / 4;
        assert!(bits_after[160 * base_w + 160] & (1 << 30) != 0);
        assert!(bits_after.last().unwrap() & (1 << 30) != 0);
    }

    // DX10 DDS with one R16G16 surface -> u32 texels.
    fn dds_words(bytes: &[u8]) -> Grid<u32> {
        let h = u32::from_le_bytes(bytes[12..16].try_into().unwrap()) as usize;
        let w = u32::from_le_bytes(bytes[16..20].try_into().unwrap()) as usize;
        assert_eq!(&bytes[84..88], b"DX10");
        assert_eq!(u32::from_le_bytes(bytes[128..132].try_into().unwrap()), 36);
        Grid {
            width: w,
            height: h,
            data: u32s(&bytes[148..148 + w * h * 4]),
        }
    }

    // With the island's materialmaps/0 DDS attached, UsedMaterialBits is
    // regenerated exactly, and materialmaps/3 equals the rd3d copy.
    #[test]
    fn used_material_bits_exact_from_full_material_map() {
        let Ok(dir) = std::env::var("ANNO117_DIR") else {
            eprintln!("skip: ANNO117_DIR unset");
            return;
        };
        let Some((mut island, _, _, _)) = load("3rdparty/roman_island_3rdparty_pirate_01") else {
            return;
        };
        let archive = Archive::open(Path::new(&dir).join("maindata/provinces_roman.rda")).unwrap();
        let base = "data/base/provinces/roman/islands/3rdparty/roman_island_3rdparty_pirate_01/\
                    _gamedata/roman_island_3rdparty_pirate_01/materialmaps";
        let dds = |level: u32, name: &str| {
            dds_words(
                &archive
                    .read_path(&format!("{base}/{level}/{name}.dds"))
                    .unwrap(),
            )
        };
        assert_eq!(dds(3, "matid"), *island.material_ids());
        assert_eq!(dds(3, "matalpha"), *island.material_alphas());

        let (ids, alphas) = (dds(0, "matid"), dds(0, "matalpha"));
        let generated = used_material_bits(&ids, &alphas, island.material_map_size);
        let (bits, _) = island.used_bits.unwrap();
        assert!(
            generated == u32s(island.rd3d.payload(bits)),
            "UsedMaterialBits differ"
        );

        island.attach_full_material_map(ids, alphas).unwrap();
        let (_, rd) = island.to_members().unwrap();
        assert!(
            rd == island.rd3d.encode(),
            "attaching alone changes nothing"
        );
        // The exact path recomputes instead of OR-ing into the shipped bits:
        // with every weight cleared, no material is used anywhere.
        island.full_material_alphas_mut().unwrap().data.fill(0);
        let (_, rd) = island.to_members().unwrap();
        let back = Island::parse(&island.gamedata.encode(), &rd).unwrap();
        assert!(u32s(back.rd3d.payload(bits)).iter().all(|&b| b == 0));
    }

    fn install_tiles(rel: &str) -> Option<impl FnMut(&str) -> Option<Vec<u8>>> {
        let dir = std::env::var_os("ANNO117_DIR")?;
        let install = crate::vfs::Install::open(dir).unwrap();
        let name = rel.rsplit('/').next().unwrap().to_owned();
        let base = format!("data/base/provinces/roman/islands/{rel}/_gamedata/{name}");
        Some(move |path: &str| install.read(&format!("{base}/{path}")).ok())
    }

    // Top triangles of a node in metres; skirt vertices (same x/z as a higher
    // vertex of the node) are left out.
    fn top_triangles(node: &tmc::Node) -> Vec<[[f32; 3]; 3]> {
        let mut top: std::collections::HashMap<(u16, u16), u16> = Default::default();
        for v in &node.vertices {
            let e = top
                .entry((v.position[0], v.position[2]))
                .or_insert(v.position[1]);
            *e = (*e).max(v.position[1]);
        }
        node.indices
            .as_chunks::<3>()
            .0
            .iter()
            .map(|t| t.map(|i| node.vertices[usize::from(i)]))
            .filter(|t| {
                t.iter()
                    .all(|v| top[&(v.position[0], v.position[2])] == v.position[1])
            })
            .map(|t| t.map(|v| v.metres()))
            .collect()
    }

    #[derive(Debug, Default)]
    struct SurfaceStats {
        nodes: usize,
        triangles: usize,
        samples: usize,
        sum_sq: f64,
        errors: Vec<f32>,
    }

    impl SurfaceStats {
        // Vertical distance from each top triangle to the height samples it
        // covers.
        fn add(&mut self, tris: &[[[f32; 3]; 3]], heights: &Grid<f32>) {
            self.nodes += 1;
            self.triangles += tris.len();
            for t in tris {
                let lo = |a: usize| t.iter().map(|p| p[a]).fold(f32::MAX, f32::min);
                let hi = |a: usize| t.iter().map(|p| p[a]).fold(f32::MIN, f32::max);
                let ([x0, z0], [x1, z1]) = ([lo(0), lo(2)], [hi(0), hi(2)]);
                let d = (t[1][2] - t[2][2]) * (t[0][0] - t[2][0])
                    + (t[2][0] - t[1][0]) * (t[0][2] - t[2][2]);
                if d.abs() < 1e-9 {
                    continue;
                }
                for gz in
                    (z0 * 2.).ceil() as usize..=((z1 * 2.).floor() as usize).min(heights.height - 1)
                {
                    for gx in (x0 * 2.).ceil() as usize
                        ..=((x1 * 2.).floor() as usize).min(heights.width - 1)
                    {
                        let (x, z) = (gx as f32 / 2., gz as f32 / 2.);
                        let a = ((t[1][2] - t[2][2]) * (x - t[2][0])
                            + (t[2][0] - t[1][0]) * (z - t[2][2]))
                            / d;
                        let b = ((t[2][2] - t[0][2]) * (x - t[2][0])
                            + (t[0][0] - t[2][0]) * (z - t[2][2]))
                            / d;
                        let c = 1. - a - b;
                        if a < -1e-4 || b < -1e-4 || c < -1e-4 {
                            continue;
                        }
                        let e = a * t[0][1] + b * t[1][1] + c * t[2][1] - heights.get(gx, gz);
                        self.samples += 1;
                        self.sum_sq += f64::from(e * e);
                        self.errors.push(e.abs());
                    }
                }
            }
        }

        fn summary(&mut self) -> (f64, f32, f32) {
            self.errors.sort_by(f32::total_cmp);
            let p = |q: usize| {
                self.errors
                    .get(self.errors.len() * q / 1000)
                    .copied()
                    .unwrap_or(0.)
            };
            (
                (self.sum_sq / self.samples.max(1) as f64).sqrt(),
                p(990),
                self.errors.last().copied().unwrap_or(0.),
            )
        }
    }

    // Re-meshing unedited islands from their own heights: same node coverage
    // as shipped on every level, and vertical error / triangle counts in the
    // shipped range. Prints the per-level table.
    #[test]
    fn remesh_matches_shipped_coverage_and_error() {
        for rel in ISLANDS {
            let (Some((island, _, _, _)), Some(mut shipped)) = (load(rel), install_tiles(rel))
            else {
                return;
            };
            let inputs = island.mesh_inputs(false);
            let ours = island.generate_meshes(&inputs, &[]).unwrap();
            let mut level = usize::MAX;
            let (mut a, mut b) = (SurfaceStats::default(), SurfaceStats::default());
            let report = |level: usize, a: &mut SurfaceStats, b: &mut SurfaceStats| {
                if level == usize::MAX || level < island.min_mesh_level {
                    return;
                }
                let ((ra, pa, ma), (rb, pb, mb)) = (a.summary(), b.summary());
                eprintln!(
                    "{rel} L{level}: nodes {}/{} tris {}/{} ({:.2}x) rms {ra:.3}/{rb:.3} p99 {pa:.3}/{pb:.3} max {ma:.2}/{mb:.2} m",
                    a.nodes,
                    b.nodes,
                    a.triangles,
                    b.triangles,
                    a.triangles as f64 / b.triangles.max(1) as f64
                );
                // The RTIN error is measured at hypotenuse midpoints, so the
                // surface can stray a little past max_error between them.
                assert!(
                    ma <= tmc::max_error(level as u32) * 2. + 0.01,
                    "{rel} L{level}: max {ma}"
                );
                assert!(
                    ra <= rb * 1.25 + 0.005,
                    "{rel} L{level}: rms {ra} vs shipped {rb}"
                );
                let ratio = a.triangles as f64 / b.triangles as f64;
                assert!(
                    (0.5..2.).contains(&ratio),
                    "{rel} L{level}: {ratio:.2}x the shipped triangles"
                );
            };
            for (k, x, y, tile) in &ours {
                if *k != level {
                    report(level, &mut a, &mut b);
                    (a, b, level) = (SurfaceStats::default(), SurfaceStats::default(), *k);
                }
                let path = format!("meshes/{k}/{x}x{y}.tmc");
                let theirs =
                    Tile::parse(&shipped(&path).unwrap_or_else(|| panic!("{rel}: no {path}")))
                        .unwrap();
                for (i, (o, t)) in tile.nodes.iter().zip(&theirs.nodes).enumerate() {
                    assert_eq!(
                        o.vertices.is_empty(),
                        t.vertices.is_empty(),
                        "{rel} {path} node {i}: coverage differs"
                    );
                    if !o.vertices.is_empty() {
                        a.add(&top_triangles(o), &inputs.heights);
                        b.add(&top_triangles(t), &inputs.heights);
                    }
                    // Our bits: the footprint plus a texel; shipped is a
                    // superset of the footprint alone.
                    assert_eq!(
                        o.used_material_bits == 0,
                        t.used_material_bits == 0,
                        "{rel} {path} node {i}"
                    );
                }
            }
            report(level, &mut a, &mut b);
        }
    }

    // A sculpted bump rewrites only the tiles and pages around it; nodes away
    // from it keep their shipped bytes, the bumped node carries the new
    // height, and the island still saves and parses.
    #[test]
    fn height_edit_rebuilds_nearby_meshes_and_pages() {
        let rel = "pool/roman_island_small_07";
        let (Some((mut island, _, _, _)), Some(mut shipped)) = (load(rel), install_tiles(rel))
        else {
            return;
        };
        assert!(island.rebuild_meshes(&mut shipped).unwrap().is_empty());
        assert!(
            island
                .rebuild_height_pages(&mut shipped)
                .unwrap()
                .is_empty()
        );
        // 3 m bump, 4 m across, centred on (130, 130) m: inside the active area.
        let (cx, cz) = (260usize, 260usize);
        for z in cz - 4..=cz + 4 {
            for x in cx - 4..=cx + 4 {
                let v = island.heights().get(x, z) + 3.;
                island.heights_mut().set(x, z, v);
            }
        }
        let meshes = island.rebuild_meshes(&mut shipped).unwrap();
        let levels: std::collections::BTreeSet<&str> = meshes
            .iter()
            .map(|f| f.path.split('/').nth(1).unwrap())
            .collect();
        // Geometry levels 2..=8 change; material bits alone do not.
        assert_eq!(
            levels.len(),
            island.levels.len() - island.min_mesh_level,
            "{levels:?}"
        );
        // Level 2: only nodes near the bump's node (32, 32, 128..132 m)
        // change. The bump touches the node border, so the neighbour's
        // triangulation changes, and with it the normals one node further.
        let mut bump_node = None;
        for f in meshes.iter().filter(|f| f.path.starts_with("meshes/2/")) {
            let (tx, ty) = tmc::tile_coords(&f.path).unwrap();
            let old = Tile::parse(&shipped(&f.path).unwrap()).unwrap();
            let new = Tile::parse(&f.bytes).unwrap();
            for i in (0..new.nodes.len()).filter(|&i| old.nodes[i] != new.nodes[i]) {
                let (x, y) = (tx as usize * 32 + i % 32, ty as usize * 32 + i / 32);
                assert!(
                    x.abs_diff(32) <= 2 && y.abs_diff(32) <= 2,
                    "{} node ({x}, {y}) changed",
                    f.path
                );
                if (x, y) == (32, 32) {
                    bump_node = Some(new.nodes[i].clone());
                }
            }
        }
        let peak = island.heights().get(cx, cz);
        let top = bump_node
            .expect("bump node rewritten")
            .vertices
            .iter()
            .map(|v| v.metres())
            .find(|p| (p[0], p[2]) == (130., 130.))
            .expect("vertex on the bump peak");
        assert!((top[1] - peak).abs() < 0.01, "{} vs {peak}", top[1]);

        let pages = island.rebuild_height_pages(&mut shipped).unwrap();
        // Pages over the bump on every normal level that lists one (two where
        // it straddles a page border: 128 m is one on levels 0..2).
        let top = format!("normalmaps/{}/0x0.ctt", island.normal_pages.len() - 1);
        assert!(pages.iter().any(|p| p.path == top), "{top} missing");
        for p in &pages {
            let page = HeightPage::parse(&p.bytes).unwrap();
            assert_eq!(page.downsampled.len(), 66 * 66);
        }
        let level2 = pages
            .iter()
            .find(|p| p.path == "normalmaps/2/2x2.ctt")
            .expect("64 m page over the bump");
        let h = HeightPage::parse(&level2.bytes).unwrap().heights();
        // 64 m page 2 starts at 128 m; 1/8 m texels -> 130 m is texel 16 + 8.
        let got = h.get(ctt::BORDER + 16, ctt::BORDER + 16);
        assert!((got - peak).abs() < 0.15, "page {got} vs {peak}");

        let (gd, rd) = island.to_members().unwrap();
        Island::parse(&gd, &rd).unwrap();
    }

    // A patch replaces one finest-level node with its own triangles (plus
    // skirts) and widens MinY/MaxY to the overhang.
    #[test]
    fn mesh_patch_replaces_a_node_and_widens_bounds() {
        let rel = "pool/roman_island_small_07";
        let (Some((mut island, _, _, _)), Some(mut shipped)) = (load(rel), install_tiles(rel))
        else {
            return;
        };
        let y = island.heights().get(260, 260);
        // An overhang roof 6 m above the ground in node (32, 32) = 128..132 m.
        island.mesh_patches_mut().push(MeshPatch {
            triangles: vec![
                [
                    [128.5, y + 6., 128.5],
                    [128.5, y + 6., 131.5],
                    [131.5, y + 6., 128.5],
                ],
                [
                    [131.5, y + 6., 128.5],
                    [128.5, y + 6., 131.5],
                    [131.5, y + 6., 131.5],
                ],
            ],
        });
        let meshes = island.rebuild_meshes(&mut shipped).unwrap();
        assert_eq!(meshes.len(), 1, "only the finest level sees the patch");
        let old = Tile::parse(&shipped(&meshes[0].path).unwrap()).unwrap();
        let tile = Tile::parse(&meshes[0].bytes).unwrap();
        let node = &tile.nodes[0];
        assert_eq!(node.vertices.len(), 8, "4 corners + 4 skirt vertices");
        assert_eq!(node.indices.len(), 6 + 4 * 6, "2 triangles + 4 skirt quads");
        assert!(
            (1..1024).all(|i| old.nodes[i] == tile.nodes[i]),
            "neighbours untouched"
        );
        let (_, rd) = island.to_members().unwrap();
        let back = Island::parse(&island.gamedata.encode(), &rd).unwrap();
        let l2 = &back.levels[2];
        let max_y = i16s(back.rd3d.payload(l2.max_y));
        assert!(f32::from(max_y[32 * l2.width + 32]) >= y + 6.);

        island.mesh_patches_mut()[0].triangles[0] =
            [[-5., 0., -5.], [-4., 0., -5.], [-5., 0., -4.]];
        assert!(
            island.rebuild_meshes(&mut shipped).is_err(),
            "outside the island"
        );
    }

    #[test]
    fn painting_takes_over_the_weakest_slot_and_blends_weights() {
        let mut ids = Grid {
            width: 1,
            height: 1,
            data: vec![pack_slots([7, 20, 9, 0, 1, 2])],
        };
        let mut alphas = Grid {
            width: 2,
            height: 2,
            data: vec![pack_slots([31, 10, 3, 0, 0, 0]); 4],
        };
        paint_material(&mut ids, &mut alphas, 0, 0, 20, 1.);
        assert_eq!(unpack_slots(alphas.get(0, 0)), [0, 31, 0, 0, 0, 0]);
        assert_eq!(unpack_slots(alphas.get(1, 1)), [31, 10, 3, 0, 0, 0]);
        // 12 is absent: slot 3 (all-zero across the block) takes it.
        paint_material(&mut ids, &mut alphas, 1, 0, 12, 0.5);
        assert_eq!(unpack_slots(ids.get(0, 0)), [7, 20, 9, 12, 1, 2]);
        assert_eq!(unpack_slots(alphas.get(1, 0)), [16, 5, 2, 16, 0, 0]);
    }

    #[test]
    fn downsampling_halves_and_remaps_onto_the_coarse_block() {
        let ids = Grid {
            width: 2,
            height: 2,
            data: vec![
                pack_slots([1, 2, 0, 0, 0, 0]),
                pack_slots([3, 0, 0, 0, 0, 0]),
                pack_slots([1, 0, 0, 0, 0, 0]),
                pack_slots([1, 0, 0, 0, 0, 0]),
            ],
        };
        let alphas = Grid {
            width: 4,
            height: 4,
            data: vec![pack_slots([0, 31, 0, 0, 0, 0]); 16],
        };
        let (ci, ca) = downsample_materials(&ids, &alphas);
        assert_eq!((ci.width, ca.width), (1, 2));
        assert_eq!(unpack_slots(ci.get(0, 0)), [1, 2, 0, 0, 0, 0]);
        // Top-left texel weights slot 1 = id 2, present in the coarse block.
        assert_eq!(unpack_slots(ca.get(0, 0)), [0, 31, 0, 0, 0, 0]);
        // Texel (1,0) came from fine block (1,0): slot 1 there is id 0, which
        // is coarse slot 2 (first 0) -> weight moves there.
        assert_eq!(unpack_slots(ca.get(1, 0)), [0, 0, 31, 0, 0, 0]);
    }

    #[test]
    fn repeated_ids_become_spare_ids_and_keep_their_weight() {
        assert_eq!(distinct_ids([7, 3, 7, 7, 1, 3]), [7, 3, 0, 2, 1, 4]);
        assert_eq!(distinct_ids([5, 4, 3, 2, 1, 0]), [5, 4, 3, 2, 1, 0]);
        let mut ids = Grid {
            width: 1,
            height: 1,
            data: vec![pack_slots([7, 3, 7, 7, 1, 3])],
        };
        let mut alphas = Grid {
            width: 2,
            height: 2,
            data: vec![pack_slots([10, 6, 5, 0, 0, 4]); 4],
        };
        assert_eq!(make_slots_distinct(&mut ids, &mut alphas), 1);
        let block = unpack_slots(ids.get(0, 0));
        let mut seen = block.to_vec();
        seen.sort();
        seen.dedup();
        assert_eq!(seen.len(), 6, "{block:?}");
        // Material 7 keeps 10 + 5, material 3 keeps 6 + 4; spares weigh 0.
        assert_eq!(unpack_slots(alphas.get(1, 1)), [15, 10, 0, 0, 0, 0]);
        assert_eq!(make_slots_distinct(&mut ids, &mut alphas), 0);
    }

    #[test]
    fn material_dds_roundtrips_and_keeps_its_header() {
        let mut dds = vec![0u8; DDS_DX10_HEADER];
        dds[..4].copy_from_slice(b"DDS ");
        dds[12..16].copy_from_slice(&1u32.to_le_bytes());
        dds[16..20].copy_from_slice(&2u32.to_le_bytes());
        dds[84..88].copy_from_slice(b"DX10");
        dds[128..132].copy_from_slice(&36u32.to_le_bytes());
        dds.extend([1, 0, 0, 0, 2, 0, 0, 0]);
        let mut grid = material_words(&dds).unwrap();
        assert_eq!(grid.data, vec![1, 2]);
        grid.data[1] = 0x3fff_ffff;
        let out = material_dds(&dds, &grid).unwrap();
        assert_eq!(&out[..DDS_DX10_HEADER], &dds[..DDS_DX10_HEADER]);
        assert_eq!(material_words(&out).unwrap().data, vec![1, 0x3fff_ffff]);
        assert!(
            material_dds(
                &dds,
                &Grid {
                    width: 1,
                    height: 1,
                    data: vec![0]
                }
            )
            .is_err()
        );
    }
}
