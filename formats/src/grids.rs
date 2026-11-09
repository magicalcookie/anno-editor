// Gameplay tile grids and the terrain detail layer of an island: water,
// river, ford, trees, herds, ambience environment, area ids, forest chunks,
// grit (BC5 detail mask) and the water/fog render maps. One `Layer` per
// grid with plain values for painting; writes patch only what changed so an
// unedited save stays byte-identical.
//
// Layers are found by path on every write, not by stored item handles:
// `Island::to_members` re-encodes rd3d, and props added there shift every
// handle behind them once the bytes are decoded again.

use anyhow::{Context, Result, bail, ensure};
use std::ops::Range;
use std::sync::Arc;

use crate::adf::{Adf, ItemId};
use crate::game::Game;
use crate::island::Island;

/// Which grid a [`Layer`] holds. Names follow the ADF field they come from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Kind {
    /// `ActiveMap` (rd3d), 8 m: the island's active region, land plus a sea
    /// margin of a few cells; `Island` calls it the buildable mask. Written
    /// through [`Island::active_map_mut`], which keeps `HasMeshArray` and
    /// the `.a7minfo` mask in step.
    ActiveMap,
    /// `WorldManager/Water`, 1 m bits: island ground including the shallow
    /// coast band and rivers; 0 is open sea.
    Water,
    /// `WorldManager/RiverGrid`, 1 m bits: river and lake water on the island.
    River,
    /// `WorldManager/FordGrid`, 1 m bits: walkable crossings, always inside
    /// `River`.
    Ford,
    /// `RegrowManager/TreeMap`, 1 m bits: harvestable tree cells. The trees
    /// themselves are `PropGrid` instances.
    Trees,
    /// `RegrowManager/ForestChunkMap`, sparse u8 per 1 m: forest chunk id
    /// for difficulty thinning, 0 = none.
    ForestChunks,
    /// `AnimalManager/HerdGrid`, 1 m bits: where herds roam.
    Herds,
    /// `WorldManager/EnvironmentGrid/EnvironmentGRid`, u8 per 4 m: ambience
    /// type, see [`Profile::environment_types`](crate::game::Profile).
    Environment,
    /// `GameSessionManager/AreaIDs`, sparse u16 per 1 m: island area key.
    AreaIds,
    /// `Terrain/GritMap2` (rd3d) red channel, u8 per 0.5 m texel.
    Grit,
    /// `Terrain/GritMap2` green channel; zero on nearly every island.
    GritAux,
    /// `WaterInfos` (rd3d), u32 per 2 m: byte 0 water depth (see
    /// [`crate::water`]), bytes 1..3 hand-painted shore masks.
    WaterInfos,
    /// `WaterFlow` (rd3d), u32 per 2 m: bytes 0/1 flow x/z biased by 127,
    /// byte 2 the distance from the coast (see [`crate::water`]).
    WaterFlow,
    /// `Fog` (rd3d), u16 per 2 m: two u8 channels (often equal), zero on
    /// most islands, painted mostly over marsh on celtic ones. Undecoded.
    Fog,
    /// `IrrigationManager/m_StaticTileGrid`, sparse u8 per 1 m: the marsh
    /// mask of celtic islands, [`MARSH`] on marsh ground. Roman islands
    /// store only the header (no size) and have no such layer.
    Marsh,
}

/// The value celtic islands store in the marsh grid (all but 17 of 128k
/// set cells; the rest hold 3).
pub const MARSH: u32 = 10;

impl Kind {
    /// Legend for UI pickers: (value, label). Empty for continuous layers.
    pub fn legend(self, game: Game) -> Vec<(u32, &'static str)> {
        let bits = |off: &'static str, on: &'static str| vec![(0, off), (1, on)];
        match self {
            Kind::ActiveMap => bits("outside", "active"),
            Kind::Water => bits("open sea", "island ground"),
            Kind::River => bits("no river", "river"),
            Kind::Ford => bits("no ford", "ford"),
            Kind::Trees => bits("no tree", "tree"),
            Kind::Herds => bits("no herd", "herd area"),
            Kind::Marsh => vec![(0, "dry"), (MARSH, "marsh")],
            Kind::Environment => (game.profile().environment_types.iter())
                .enumerate()
                .map(|(i, &(name, _))| (i as u32, name))
                .collect(),
            _ => Vec::new(),
        }
    }
}

/// Cell values of one layer, row-major, `x` east (grid column), `y` down the
/// rows (island `z`), same frame as the height grid.
#[derive(Debug, Clone, PartialEq)]
pub enum Values {
    U8(Vec<u8>),
    U16(Vec<u16>),
    U32(Vec<u32>),
}

impl Values {
    fn get(&self, i: usize) -> u32 {
        match self {
            Values::U8(v) => v[i].into(),
            Values::U16(v) => v[i].into(),
            Values::U32(v) => v[i],
        }
    }

    // Truncates to the element width; callers paint legend values.
    fn set(&mut self, i: usize, value: u32) {
        match self {
            Values::U8(v) => v[i] = value as u8,
            Values::U16(v) => v[i] = value as u16,
            Values::U32(v) => v[i] = value,
        }
    }

    fn u8s(&self) -> &[u8] {
        match self {
            Values::U8(v) => v,
            _ => unreachable!("layer storage expects u8 cells"),
        }
    }
}

// How a layer sits in its member. Paths are resolved on write.
#[derive(Debug, Clone)]
enum Storage {
    // Island::active_map.
    ActiveMap,
    // `{x, y, bits}` node, LSB-first, rows padded to u32 words.
    Bits {
        node: &'static str,
    },
    // `{x, y, val}` node, one typed value per cell.
    Bytes {
        node: &'static str,
    },
    // `{SparseEnabled, x, y, block*}` node, see `Sparse`.
    Sparse {
        node: &'static str,
        block: [usize; 2],
        elem: usize,
    },
    // `Terrain/GritMap2` BC5 blocks; channel 0 = red, 1 = green.
    Bc4 {
        channel: usize,
    },
    // `{width, height, map}` node of little-endian words.
    Words {
        node: &'static str,
    },
    // Root `Fog` blob, u16 per cell.
    Fog,
}

/// One editable grid. Change `values` (or use [`Layer::set`]) and write back
/// with [`Grids::save`].
#[derive(Debug, Clone)]
pub struct Layer {
    pub kind: Kind,
    /// ADF path in its member, for display and debugging.
    pub name: &'static str,
    pub width: usize,
    pub height: usize,
    /// Edge length of one cell in metres.
    pub cell_metres: f32,
    /// Copy-on-write, so copies of a `Grids` (undo steps) share every
    /// layer an edit leaves alone: a sculpt stroke's land sync copied all
    /// 14 layers and their originals, 20 MB a step on the continental
    /// island.
    pub values: Arc<Values>,
    original: Arc<Values>,
    storage: Storage,
}

impl Layer {
    /// Columns and rows of the cells whose centres lie in `window`, world
    /// metres `[x0, z0, x1, z1]`; every cell without one.
    pub fn cells_within(&self, window: Option<[f32; 4]>) -> (Range<usize>, Range<usize>) {
        let Some([x0, z0, x1, z1]) = window else {
            return (0..self.width, 0..self.height);
        };
        let cell = self.cell_metres;
        let span = |lo: f32, hi: f32, n: usize| {
            let first = ((lo / cell - 0.5).ceil().max(0.) as usize).min(n);
            let end = (((hi / cell - 0.5).floor() + 1.).max(0.) as usize).min(n);
            first..end.max(first)
        };
        (span(x0, x1, self.width), span(z0, z1, self.height))
    }

    pub fn get(&self, x: usize, y: usize) -> u32 {
        self.values.get(y * self.width + x)
    }

    pub fn set(&mut self, x: usize, y: usize, value: u32) {
        if self.get(x, y) != value {
            Arc::make_mut(&mut self.values).set(y * self.width + x, value);
        }
    }

    /// Cell under island metres `(x, z)`, if inside the grid.
    pub fn cell_at(&self, x: f32, z: f32) -> Option<(usize, usize)> {
        let (cx, cy) = (x / self.cell_metres, z / self.cell_metres);
        (cx >= 0.0 && cy >= 0.0 && (cx as usize) < self.width && (cy as usize) < self.height)
            .then_some((cx as usize, cy as usize))
    }

    pub fn is_edited(&self) -> bool {
        !Arc::ptr_eq(&self.values, &self.original) && self.values != self.original
    }

    /// Stored in `rd3d.data` (grit, water maps, fog), written by
    /// [`Grids::write_rd3d`] rather than [`Grids::apply`].
    pub fn in_rd3d(&self) -> bool {
        matches!(
            self.storage,
            Storage::Bc4 { .. } | Storage::Words { .. } | Storage::Fog
        )
    }
}

/// All grid layers of one island.
#[derive(Debug, Clone)]
pub struct Grids {
    pub layers: Vec<Layer>,
    game: Game,
}

impl Grids {
    /// A copy with only the layers of `kinds`.
    pub fn only(&self, kinds: &[Kind]) -> Grids {
        Grids {
            layers: (self.layers.iter())
                .filter(|l| kinds.contains(&l.kind))
                .cloned()
                .collect(),
            game: self.game,
        }
    }

    /// Reads every layer the island has. Missing grids are skipped; all 88
    /// shipped islands that `Island` parses have all 14.
    pub fn read(island: &Island) -> Result<Grids> {
        let gd = island.gamedata();
        let rd = island.rd3d();
        let world = island.world_size()[0] as f32;
        let mut layers = Vec::new();
        let mut push = |kind, name, width: usize, height, values: Values, storage| {
            // Original and values share their cells until an edit.
            let values = Arc::new(values);
            layers.push(Layer {
                kind,
                name,
                width,
                height,
                cell_metres: world / width as f32,
                original: values.clone(),
                values,
                storage,
            });
        };

        let active = island.active_map();
        push(
            Kind::ActiveMap,
            "Terrain/ActiveMap",
            active.width,
            active.height,
            Values::U8(active.data.iter().map(|&b| b as u8).collect()),
            Storage::ActiveMap,
        );

        for (kind, node) in [
            (Kind::Water, "GameSessionManager/WorldManager/Water"),
            (Kind::River, "GameSessionManager/WorldManager/RiverGrid"),
            (Kind::Ford, "GameSessionManager/WorldManager/FordGrid"),
            (Kind::Trees, "GameSessionManager/RegrowManager/TreeMap"),
            (Kind::Herds, "GameSessionManager/AnimalManager/HerdGrid"),
        ] {
            let Some(n) = gd.node(gd.root(), node) else {
                continue;
            };
            let (w, h) = dims(gd, n, ["x", "y"])?;
            let bits = gd
                .value(n, "bits")
                .with_context(|| format!("{node}: no bits"))?;
            let cells = unpack_bits(gd.payload(bits), w, h).with_context(|| node.to_owned())?;
            push(kind, node, w, h, Values::U8(cells), Storage::Bits { node });
        }

        let env = "GameSessionManager/WorldManager/EnvironmentGrid/EnvironmentGRid";
        if let Some(n) = gd.node(gd.root(), env) {
            let (w, h) = dims(gd, n, ["x", "y"])?;
            let val = gd.value(n, "val").context("EnvironmentGRid: no val")?;
            let cells = gd.payload(val).to_vec();
            ensure!(
                cells.len() == w * h,
                "EnvironmentGRid: {} bytes for {w}x{h}",
                cells.len()
            );
            push(
                Kind::Environment,
                env,
                w,
                h,
                Values::U8(cells),
                Storage::Bytes { node: env },
            );
        }

        for (kind, node) in [
            (Kind::AreaIds, "GameSessionManager/AreaIDs"),
            (
                Kind::ForestChunks,
                "GameSessionManager/RegrowManager/ForestChunkMap",
            ),
            (
                Kind::Marsh,
                "GameSessionManager/IrrigationManager/m_StaticTileGrid",
            ),
        ] {
            let Some(n) = gd.node(gd.root(), node) else {
                continue;
            };
            if let Some(val) = gd.child_value(n, "val") {
                let (w, h) = dims(gd, n, ["x", "y"])?;
                let bytes = gd.payload(val);
                let values = match kind {
                    Kind::AreaIds => {
                        ensure!(bytes.len() == w * h * 2, "AreaIDs dense payload size");
                        Values::U16(
                            bytes
                                .as_chunks::<2>()
                                .0
                                .iter()
                                .map(|b| u16::from_le_bytes(*b))
                                .collect(),
                        )
                    }
                    _ => {
                        ensure!(bytes.len() == w * h, "{node} dense payload size");
                        Values::U8(bytes.to_vec())
                    }
                };
                push(kind, node, w, h, values, Storage::Bytes { node });
                continue;
            }
            let s = Sparse::read(gd, n).with_context(|| node.to_owned())?;
            // A header without a size (roman irrigation) is no grid yet.
            if s.width == 0 || s.height == 0 {
                continue;
            }
            let values = s.dense();
            push(
                kind,
                node,
                s.width,
                s.height,
                values,
                Storage::Sparse {
                    node,
                    block: s.block,
                    elem: s.elem,
                },
            );
        }

        let terrain = rd
            .child_node(rd.root(), "Terrain")
            .context("rd3d: no Terrain")?;
        if let Some(grit) = rd.child_node(terrain, "GritMap2") {
            let (bw, bh) = dims(rd, grit, ["width", "height"])?;
            let map = rd.payload(rd.value(grit, "map").context("GritMap2: no map")?);
            ensure!(
                map.len() == bw * bh * 16,
                "GritMap2: {} bytes for {bw}x{bh} blocks",
                map.len()
            );
            let [r, g] = decode_bc5(map, bw, bh);
            let (w, h) = (bw * 4, bh * 4);
            push(
                Kind::Grit,
                "Terrain/GritMap2.r",
                w,
                h,
                Values::U8(r),
                Storage::Bc4 { channel: 0 },
            );
            push(
                Kind::GritAux,
                "Terrain/GritMap2.g",
                w,
                h,
                Values::U8(g),
                Storage::Bc4 { channel: 1 },
            );
        }

        for (kind, node) in [
            (Kind::WaterInfos, "WaterInfos"),
            (Kind::WaterFlow, "WaterFlow"),
        ] {
            let Some(n) = rd.child_node(rd.root(), node) else {
                continue;
            };
            let (w, h) = dims(rd, n, ["width", "height"])?;
            let map = rd.payload(
                rd.value(n, "map")
                    .with_context(|| format!("{node}: no map"))?,
            );
            ensure!(
                map.len() == w * h * 4,
                "{node}: {} bytes for {w}x{h}",
                map.len()
            );
            let words = map
                .as_chunks::<4>()
                .0
                .iter()
                .map(|&c| u32::from_le_bytes(c))
                .collect();
            push(
                kind,
                node,
                w,
                h,
                Values::U32(words),
                Storage::Words { node },
            );
        }

        if let Some(fog) = rd.child_value(rd.root(), "Fog") {
            let p = rd.payload(fog);
            // Square u16 grid; 2 m cells on every island checked.
            let side = ((p.len() / 2) as f64).sqrt() as usize;
            if side * side * 2 == p.len() {
                let cells = p
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .map(|&c| u16::from_le_bytes(c))
                    .collect();
                push(
                    Kind::Fog,
                    "Fog",
                    side,
                    side,
                    Values::U16(cells),
                    Storage::Fog,
                );
            }
        }

        Ok(Grids {
            layers,
            game: island.game(),
        })
    }

    pub fn game(&self) -> Game {
        self.game
    }

    pub fn layer(&self, kind: Kind) -> Option<&Layer> {
        self.layers.iter().find(|l| l.kind == kind)
    }

    pub fn layer_mut(&mut self, kind: Kind) -> Option<&mut Layer> {
        self.layers.iter_mut().find(|l| l.kind == kind)
    }

    /// Writes edited gamedata layers and the active map into `island`.
    /// rd3d layers need [`Grids::write_rd3d`] (or [`Grids::save`]): `Island`
    /// has no mutable rd3d tree.
    pub fn apply(&self, island: &mut Island) -> Result<()> {
        for l in self.layers.iter().filter(|l| l.is_edited()) {
            match l.storage {
                Storage::ActiveMap => {
                    let a = island.active_map_mut();
                    for (dst, &v) in a.data.iter_mut().zip(l.values.u8s()) {
                        *dst = v != 0;
                    }
                }
                _ if l.in_rd3d() => {}
                _ => l.write(island.gamedata_mut())?,
            }
        }
        Ok(())
    }

    /// Writes edited rd3d layers (grit, water maps, fog) into a decoded
    /// `rd3d.data` tree.
    pub fn write_rd3d(&self, rd3d: &mut Adf) -> Result<()> {
        for l in self.layers.iter().filter(|l| l.is_edited() && l.in_rd3d()) {
            l.write(rd3d)?;
        }
        Ok(())
    }

    /// Applies every edit and encodes `(gamedata.data, rd3d.data)`; the
    /// one-call save path for the editor.
    // TODO(alex): drop the rd3d decode/encode round once Island exposes its
    // rd3d tree mutably.
    pub fn save(&self, island: &mut Island) -> Result<(Vec<u8>, Vec<u8>)> {
        self.apply(island)?;
        let (gd, rd) = island.to_members()?;
        if !self.layers.iter().any(|l| l.is_edited() && l.in_rd3d()) {
            return Ok((gd, rd));
        }
        let mut tree = Adf::decode(&rd)?;
        self.write_rd3d(&mut tree)?;
        Ok((gd, tree.encode()))
    }
}

impl Layer {
    fn write(&self, adf: &mut Adf) -> Result<()> {
        let root = adf.root();
        match self.storage {
            Storage::ActiveMap => bail!("ActiveMap is written through Island"),
            Storage::Bits { node } => {
                let bits = adf
                    .value(root, &format!("{node}/bits"))
                    .with_context(|| node.to_owned())?;
                let cells: Vec<bool> = self.values.u8s().iter().map(|&v| v != 0).collect();
                let packed = pack_bits_over(adf.payload(bits), &cells, self.width);
                adf.set_payload(bits, &packed);
            }
            Storage::Bytes { node } => {
                let val = adf
                    .value(root, &format!("{node}/val"))
                    .with_context(|| node.to_owned())?;
                let bytes = match &*self.values {
                    Values::U8(v) => v.clone(),
                    Values::U16(v) => v.iter().flat_map(|v| v.to_le_bytes()).collect(),
                    Values::U32(v) => v.iter().flat_map(|v| v.to_le_bytes()).collect(),
                };
                adf.set_payload(val, &bytes);
            }
            Storage::Sparse { node, block, elem } => {
                let n = adf.node(root, node).with_context(|| node.to_owned())?;
                let old = Sparse::read(adf, n)?;
                ensure!(
                    old.block == block && old.elem == elem && old.width == self.width,
                    "{node}: layout changed since read"
                );
                old.write(adf, n, &self.values)?;
            }
            Storage::Bc4 { channel } => {
                let grit = adf
                    .node(root, "Terrain/GritMap2")
                    .context("rd3d: no GritMap2")?;
                let map = adf.value(grit, "map").context("GritMap2: no map")?;
                let (bw, bh) = (self.width / 4, self.height / 4);
                let mut bytes = adf.payload(map).to_vec();
                let (new, old) = (self.values.u8s(), self.original.u8s());
                // Only blocks whose texels changed are re-encoded; BC4 is
                // lossy, so untouched blocks keep their shipped bytes.
                for by in 0..bh {
                    for bx in 0..bw {
                        let texels = |v: &[u8]| -> [u8; 16] {
                            std::array::from_fn(|i| {
                                v[(by * 4 + i / 4) * self.width + bx * 4 + i % 4]
                            })
                        };
                        let t = texels(new);
                        if t != texels(old) {
                            let at = (by * bw + bx) * 16 + channel * 8;
                            bytes[at..at + 8].copy_from_slice(&encode_bc4(&t));
                        }
                    }
                }
                adf.set_payload(map, &bytes);
            }
            Storage::Words { node } => {
                let map = adf
                    .value(root, &format!("{node}/map"))
                    .with_context(|| node.to_owned())?;
                let Values::U32(v) = &*self.values else {
                    unreachable!()
                };
                let bytes: Vec<u8> = v.iter().flat_map(|w| w.to_le_bytes()).collect();
                adf.set_payload(map, &bytes);
            }
            Storage::Fog => {
                let fog = adf.child_value(root, "Fog").context("rd3d: no Fog")?;
                let Values::U16(v) = &*self.values else {
                    unreachable!()
                };
                let bytes: Vec<u8> = v.iter().flat_map(|w| w.to_le_bytes()).collect();
                adf.set_payload(fog, &bytes);
            }
        }
        Ok(())
    }
}

fn dims(adf: &Adf, node: ItemId, names: [&str; 2]) -> Result<(usize, usize)> {
    let get = |n: &str| -> Result<usize> {
        let v = adf
            .child_value(node, n)
            .with_context(|| format!("{:?}: no {n}", adf.name(node)))?;
        Ok(adf.read_u32(v)? as usize)
    };
    Ok((get(names[0])?, get(names[1])?))
}

// Bit grids share the ActiveMap layout: LSB-first, each row padded to whole
// u32 words (the engine sizes rows as (w + 31) / 32 words). Padding bits are
// kept from the original payload.
fn row_bytes(width: usize) -> usize {
    width.div_ceil(32) * 4
}

fn unpack_bits(bytes: &[u8], width: usize, height: usize) -> Result<Vec<u8>> {
    let stride = row_bytes(width);
    ensure!(
        bytes.len() == stride * height,
        "bit grid {width}x{height}: {} bytes, expected {}",
        bytes.len(),
        stride * height
    );
    Ok((0..width * height)
        .map(|i| bytes[(i / width) * stride + (i % width) / 8] >> (i % width % 8) & 1)
        .collect())
}

fn pack_bits_over(base: &[u8], cells: &[bool], width: usize) -> Vec<u8> {
    let stride = row_bytes(width);
    let mut out = base.to_vec();
    for (i, &b) in cells.iter().enumerate() {
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

// Sparse block grid (`AreaIDs`, `ForestChunkMap`, irrigation):
//
//   SparseEnabled u8, x u32, y u32          grid size in cells
//   block { mode=1, x, y, default }         header: block size and fill value
//   block { [x] [y] values }                dense block at (x, y), row-major
//   block { mode=2, [x] [y] default }       uniform block
//   block { mode=0 }                        end marker
//
// x/y of blocks are cell offsets, elided when 0. Blocks that are not listed
// hold the header default. Block order is not sorted (ForestChunkMap), so new
// blocks are appended before the end marker.
#[derive(Debug)]
struct Sparse {
    width: usize,
    height: usize,
    block: [usize; 2],
    elem: usize,
    fill: u32,
    blocks: Vec<SparseBlock>,
}

#[derive(Debug)]
struct SparseBlock {
    item: ItemId,
    at: [usize; 2],
    // None = uniform block with this value.
    values: Option<Vec<u32>>,
    uniform: u32,
}

fn read_le(p: &[u8]) -> u32 {
    p.iter().rev().fold(0, |acc, &b| acc << 8 | u32::from(b))
}

impl Sparse {
    fn read(adf: &Adf, node: ItemId) -> Result<Sparse> {
        let opt =
            |parent: ItemId, name: &str| adf.child_value(parent, name).map(|v| adf.payload(v));
        let width = opt(node, "x").map_or(0, read_le) as usize;
        let height = opt(node, "y").map_or(0, read_le) as usize;
        let mut header = None;
        let mut blocks = Vec::new();
        for b in adf.children_named(node, "block", false) {
            let mode = opt(b, "mode").map(read_le);
            let at = [
                opt(b, "x").map_or(0, read_le) as usize,
                opt(b, "y").map_or(0, read_le) as usize,
            ];
            match (mode, opt(b, "values")) {
                (Some(1), _) => {
                    let d = opt(b, "default").context("sparse header without default")?;
                    header = Some((at, d.len().max(1), read_le(d)));
                }
                (Some(0), None) => {}
                (None, Some(v)) => {
                    let (_, elem, _) = header.context("sparse block before header")?;
                    let vals = v.chunks(elem).map(read_le).collect();
                    blocks.push(SparseBlock {
                        item: b,
                        at,
                        values: Some(vals),
                        uniform: 0,
                    });
                }
                (Some(2), None) => {
                    let d = opt(b, "default").map_or(0, read_le);
                    blocks.push(SparseBlock {
                        item: b,
                        at,
                        values: None,
                        uniform: d,
                    });
                }
                (m, v) => bail!("sparse block: mode {m:?} with values {}", v.is_some()),
            }
        }
        let (block, elem, fill) = header.context("sparse grid without header block")?;
        ensure!(block[0] > 0 && block[1] > 0, "sparse grid: zero block size");
        for b in &blocks {
            if let Some(v) = &b.values {
                ensure!(
                    v.len() == block[0] * block[1],
                    "sparse block at {:?}: {} cells",
                    b.at,
                    v.len()
                );
            }
            ensure!(
                b.at[0] + block[0] <= width.max(block[0])
                    && b.at[1] + block[1] <= height.max(block[1]),
                "sparse block at {:?} outside {width}x{height}",
                b.at
            );
        }
        Ok(Sparse {
            width,
            height,
            block,
            elem,
            fill,
            blocks,
        })
    }

    fn dense(&self) -> Values {
        let mut out = vec![self.fill; self.width * self.height];
        let [bw, bh] = self.block;
        for b in &self.blocks {
            for y in 0..bh {
                for x in 0..bw {
                    let v = b.values.as_ref().map_or(b.uniform, |v| v[y * bw + x]);
                    out[(b.at[1] + y) * self.width + b.at[0] + x] = v;
                }
            }
        }
        match self.elem {
            1 => Values::U8(out.into_iter().map(|v| v as u8).collect()),
            2 => Values::U16(out.into_iter().map(|v| v as u16).collect()),
            _ => Values::U32(out),
        }
    }

    // Rewrites changed blocks in place; blocks that gain non-fill cells are
    // appended. The node is rebuilt only when blocks change kind or appear,
    // since the tree API cannot insert before the end marker.
    fn write(&self, adf: &mut Adf, node: ItemId, values: &Values) -> Result<()> {
        let [bw, bh] = self.block;
        let cells = |at: [usize; 2]| -> Vec<u32> {
            (0..bw * bh)
                .map(|i| values.get((at[1] + i / bw) * self.width + at[0] + i % bw))
                .collect()
        };
        let encode = |v: &[u32]| -> Vec<u8> {
            v.iter()
                .flat_map(|&x| x.to_le_bytes()[..self.elem].to_vec())
                .collect()
        };

        let mut rebuild = false;
        let mut dense_now: Vec<(ItemId, Vec<u32>)> = Vec::new();
        for b in &self.blocks {
            let now = cells(b.at);
            match &b.values {
                Some(old) if *old != now => {
                    let v = adf.value(b.item, "values").expect("dense block has values");
                    adf.set_payload(v, &encode(&now));
                }
                None if now.iter().any(|&c| c != b.uniform) => {
                    rebuild = true;
                    dense_now.push((b.item, now));
                }
                _ => {}
            }
        }
        let mut added = Vec::new();
        for by in (0..self.height).step_by(bh) {
            for bx in (0..self.width).step_by(bw) {
                if self.blocks.iter().any(|b| b.at == [bx, by]) {
                    continue;
                }
                let now = cells([bx, by]);
                if now.iter().any(|&c| c != self.fill) {
                    added.push(([bx, by], now));
                }
            }
        }
        if !rebuild && added.is_empty() {
            return Ok(());
        }

        // Re-add every child with its payload, swapping converted uniform
        // blocks for dense ones and appending new blocks before the end.
        let old: Vec<ItemId> = adf.children(node).collect();
        adf.clear_children(node);
        let block_node = |adf: &mut Adf, at: [usize; 2], v: &[u32]| -> Result<()> {
            let b = adf.add_node(node, Some("block"))?;
            for (name, c) in [("x", at[0]), ("y", at[1])] {
                if c != 0 {
                    adf.add_value(b, Some(name), &(c as u16).to_le_bytes())?;
                }
            }
            adf.add_value(b, Some("values"), &encode(v))?;
            Ok(())
        };
        for item in old {
            let name = adf.name(item).map(str::to_owned);
            if !adf.is_node(item) {
                let p = adf.payload(item).to_vec();
                adf.add_value(node, name.as_deref(), &p)?;
                continue;
            }
            if let Some((_, v)) = dense_now.iter().find(|(i, _)| *i == item) {
                let at = self.blocks.iter().find(|b| b.item == item).unwrap().at;
                block_node(adf, at, v)?;
                continue;
            }
            let kids: Vec<ItemId> = adf.children(item).collect();
            let is_end = kids.len() == 1
                && adf.name(kids[0]) == Some("mode")
                && read_le(adf.payload(kids[0])) == 0;
            if is_end {
                for (at, v) in &added {
                    block_node(adf, *at, v)?;
                }
            }
            let b = adf.add_node(node, name.as_deref())?;
            for k in kids {
                ensure!(!adf.is_node(k), "sparse block with nested node");
                let (kn, p) = (adf.name(k).map(str::to_owned), adf.payload(k).to_vec());
                adf.add_value(b, kn.as_deref(), &p)?;
            }
        }
        Ok(())
    }
}

// GritMap2 is a BC5 texture: per 4x4 texel block two BC4 blocks (red, then
// green), each `[a0][a1][16 x 3-bit indices]`. Block rows are row-major.
fn bc4_palette(a0: u8, a1: u8) -> [u8; 8] {
    let (a, b) = (u32::from(a0), u32::from(a1));
    std::array::from_fn(|i| {
        let i = i as u32;
        let v = match i {
            0 => a,
            1 => b,
            // a0 > a1: six interpolated steps; else four plus 0 and 255.
            _ if a > b => ((8 - i) * a + (i - 1) * b) / 7,
            6 => 0,
            7 => 255,
            _ => ((6 - i) * a + (i - 1) * b) / 5,
        };
        v as u8
    })
}

fn decode_bc4(b: &[u8]) -> [u8; 16] {
    let pal = bc4_palette(b[0], b[1]);
    let bits = b[2..8]
        .iter()
        .rev()
        .fold(0u64, |acc, &x| acc << 8 | u64::from(x));
    std::array::from_fn(|i| pal[(bits >> (3 * i) & 7) as usize])
}

// Min/max endpoints in 8-value mode, nearest palette index per texel. Flat
// blocks store a0 == a1 with zero indices, which decodes to a0.
fn encode_bc4(t: &[u8; 16]) -> [u8; 8] {
    let (lo, hi) = (*t.iter().min().unwrap(), *t.iter().max().unwrap());
    let mut out = [0u8; 8];
    out[0] = hi;
    out[1] = lo;
    if lo == hi {
        return out;
    }
    let palette = bc4_palette(hi, lo);
    let mut bits = 0u64;
    for (i, &v) in t.iter().enumerate() {
        let k = (0..8)
            .min_by_key(|&k| (i32::from(palette[k]) - i32::from(v)).abs())
            .unwrap();
        bits |= (k as u64) << (3 * i);
    }
    out[2..8].copy_from_slice(&bits.to_le_bytes()[..6]);
    out
}

fn decode_bc5(map: &[u8], bw: usize, bh: usize) -> [Vec<u8>; 2] {
    let w = bw * 4;
    let mut out = [vec![0u8; w * bh * 4], vec![0u8; w * bh * 4]];
    for (k, block) in map.as_chunks::<16>().0.iter().enumerate() {
        let (bx, by) = (k % bw, k / bw);
        for (c, ch) in out.iter_mut().enumerate() {
            let t = decode_bc4(&block[c * 8..c * 8 + 8]);
            for (i, &v) in t.iter().enumerate() {
                ch[(by * 4 + i / 4) * w + bx * 4 + i % 4] = v;
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::*;
    use crate::rda::Archive;

    fn islands_dir() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../extract/provinces_roman/data/base/provinces/roman/islands")
    }

    // (island, gamedata, rd3d) of an extracted island; None when the sample
    // is not on disk.
    fn load(rel: &str) -> Option<(Island, Vec<u8>, Vec<u8>)> {
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
                .unwrap();
            archive.read(e).unwrap()
        };
        let (gd, rd) = (read("gamedata.data"), read("rd3d.data"));
        Some((Island::parse(&gd, &rd).unwrap(), gd, rd))
    }

    // small_07 and large_01 have rivers and fords; pirate_01 and deco_01
    // have none. Sizes 256, 512, 320 and 128 m.
    const ISLANDS: [&str; 4] = [
        "pool/roman_island_small_07",
        "pool/roman_island_large_01",
        "3rdparty/roman_island_3rdparty_pirate_01",
        "deco/roman_island_deco_01",
    ];

    fn cells(l: &Layer) -> impl Iterator<Item = (usize, usize, u32)> + '_ {
        (0..l.width * l.height).map(|i| (i % l.width, i / l.width, l.get(i % l.width, i / l.width)))
    }

    #[test]
    fn bc4_known_block_and_roundtrip() {
        // The most common shipped block: a0=1, a1=0, every index 1 -> 0.
        assert_eq!(
            decode_bc4(&[1, 0, 0x49, 0x92, 0x24, 0x49, 0x92, 0x24]),
            [0; 16]
        );
        let flat = [77u8; 16];
        assert_eq!(decode_bc4(&encode_bc4(&flat)), flat);
        let ramp: [u8; 16] = std::array::from_fn(|i| (i * 17) as u8);
        let back = decode_bc4(&encode_bc4(&ramp));
        for (a, b) in ramp.iter().zip(back) {
            assert!(a.abs_diff(b) <= 19, "{ramp:?} -> {back:?}");
        }
        // Endpoints survive exactly.
        assert_eq!((back[0], back[15]), (0, 255));
    }

    #[test]
    fn layers_read_with_consistent_semantics() {
        for rel in ISLANDS {
            let Some((island, _, _)) = load(rel) else {
                continue;
            };
            let g = Grids::read(&island).unwrap();
            let world = island.world_size()[0] as f32;
            for l in &g.layers {
                assert!(
                    (l.cell_metres * l.width as f32 - world).abs() < 1e-3,
                    "{rel} {:?}: {} x {} m",
                    l.kind,
                    l.width,
                    l.cell_metres
                );
            }
            let layer = |k| g.layer(k).unwrap_or_else(|| panic!("{rel}: no {k:?}"));
            let (water, river, ford) = (layer(Kind::Water), layer(Kind::River), layer(Kind::Ford));
            let (trees, chunks, areas) = (
                layer(Kind::Trees),
                layer(Kind::ForestChunks),
                layer(Kind::AreaIds),
            );
            for (x, y, w) in cells(water) {
                // Ford inside river inside island ground, exact on all islands.
                assert!(
                    ford.get(x, y) <= river.get(x, y),
                    "{rel}: ford off river at {x},{y}"
                );
                assert!(river.get(x, y) <= w, "{rel}: river off ground at {x},{y}");
                // Forest chunks are numbered exactly where TreeMap is set.
                assert_eq!(
                    trees.get(x, y) != 0,
                    chunks.get(x, y) != 0,
                    "{rel}: {x},{y}"
                );
                assert!(matches!(areas.get(x, y), 0 | 0x0001 | 0x2001 | 0x4001));
            }
            // Environment: ocean types dominate where the ground layer is 0.
            let env = layer(Kind::Environment);
            let scale = env.cell_metres as usize;
            let (mut sea, mut open) = (0, 0);
            for (x, y, w) in cells(water) {
                if w == 0 && x % scale == 0 && y % scale == 0 {
                    open += 1;
                    sea += matches!(env.get(x / scale, y / scale), 20 | 21) as usize;
                }
            }
            assert!(
                sea * 10 > open * 8,
                "{rel}: {sea} of {open} open-sea cells are Sea/SeaClose"
            );
            let types = Game::Anno117.profile().environment_types;
            assert!(cells(env).all(|(_, _, v)| (v as usize) < types.len()));
            if rel.contains("pool") {
                assert!(cells(river).any(|c| c.2 != 0), "{rel}: expected a river");
                assert!(cells(ford).any(|c| c.2 != 0), "{rel}: expected a ford");
            }
        }
    }

    #[test]
    fn unedited_save_is_byte_identical() {
        for rel in ISLANDS {
            let Some((mut island, gd, rd)) = load(rel) else {
                continue;
            };
            let g = Grids::read(&island).unwrap();
            assert!(g.layers.iter().all(|l| !l.is_edited()));
            let (gd2, rd2) = g.save(&mut island).unwrap();
            assert!(gd2 == gd, "{rel}: gamedata changed");
            assert!(rd2 == rd, "{rel}: rd3d changed");
        }
    }

    // One cell in every storage kind, including a uniform sparse block that
    // turns dense and a forest chunk in a block the file does not list.
    #[test]
    fn single_cell_edits_roundtrip() {
        let Some((mut island, _, _)) = load("pool/roman_island_large_01") else {
            return;
        };
        let mut g = Grids::read(&island).unwrap();
        let before = g.clone();

        let sparse = |node: &str| {
            let gd = island.gamedata();
            Sparse::read(gd, gd.node(gd.root(), node).unwrap()).unwrap()
        };
        let areas = sparse("GameSessionManager/AreaIDs");
        let uniform = areas
            .blocks
            .iter()
            .find(|b| b.values.is_none())
            .expect("uniform block")
            .at;
        let dense = areas.blocks.iter().find(|b| b.values.is_some()).unwrap().at;
        let chunks = sparse("GameSessionManager/RegrowManager/ForestChunkMap");
        let [bw, bh] = chunks.block;
        let missing = (0..chunks.height / bh)
            .flat_map(|y| (0..chunks.width / bw).map(move |x| [x * bw, y * bh]))
            .find(|at| chunks.blocks.iter().all(|b| b.at != *at))
            .expect("unlisted chunk block");

        let flip = |l: &mut Layer, x: usize, y: usize| {
            let v = l.get(x, y);
            l.set(x, y, if v == 0 { 1 } else { 0 });
            (x, y, l.get(x, y))
        };
        let mut edits: Vec<(Kind, (usize, usize, u32))> = Vec::new();
        for k in [
            Kind::Water,
            Kind::River,
            Kind::Ford,
            Kind::Trees,
            Kind::Herds,
            Kind::ActiveMap,
        ] {
            let l = g.layer_mut(k).unwrap();
            let (x, y) = (l.width * 3 / 8, l.height * 5 / 8);
            edits.push((k, flip(l, x, y)));
        }
        let mut set = |k, x, y, v| {
            g.layer_mut(k).unwrap().set(x, y, v);
            edits.push((k, (x, y, v)));
        };
        set(Kind::Environment, 10, 20, 8);
        set(Kind::AreaIds, uniform[0] + 3, uniform[1] + 5, 0x2001);
        set(Kind::AreaIds, dense[0] + 1, dense[1] + 2, 0x4001);
        set(Kind::ForestChunks, missing[0] + 7, missing[1] + 9, 42);
        // A flat-zero grit block keeps 0 and 255 exact after BC4.
        set(Kind::Grit, 0, 0, 255);
        set(Kind::WaterInfos, 5, 6, 0xdead_beef);
        set(Kind::WaterFlow, 6, 5, 0x0012_3456);
        set(Kind::Fog, 7, 7, 0x1234);

        let (gd, rd) = g.save(&mut island).unwrap();
        let back = Grids::read(&Island::parse(&gd, &rd).unwrap()).unwrap();
        for (l, (old, now)) in back.layers.iter().zip(before.layers.iter().zip(&g.layers)) {
            assert_eq!(l.kind, now.kind);
            for (x, y, v) in cells(l) {
                let edited = edits
                    .iter()
                    .any(|&(k, (ex, ey, _))| k == l.kind && (ex, ey) == (x, y));
                let want = if edited { now.get(x, y) } else { old.get(x, y) };
                assert_eq!(v, want, "{:?} at {x},{y}", l.kind);
            }
        }
        // The new chunk block went in before the end marker.
        let gd_tree = Adf::decode(&gd).unwrap();
        let node = gd_tree
            .node(
                gd_tree.root(),
                "GameSessionManager/RegrowManager/ForestChunkMap",
            )
            .unwrap();
        let after = Sparse::read(&gd_tree, node).unwrap();
        assert_eq!(after.blocks.len(), chunks.blocks.len() + 1);
        let last = gd_tree.children_named(node, "block", false).last().unwrap();
        assert_eq!(gd_tree.children(last).count(), 1, "end marker stays last");
    }

    // Every island in the install reads and saves unchanged.
    #[test]
    fn installed_islands_roundtrip() {
        let Ok(dir) = std::env::var("ANNO117_DIR") else {
            eprintln!("skip: ANNO117_DIR unset");
            return;
        };
        // (archive, island, has a marsh grid)
        let samples = [
            (
                "provinces_celtic.rda",
                "data/base/provinces/celtic/islands/pool/celtic_island_large_01/celtic_island_large_01.a7m",
                true,
            ),
            (
                "provinces_celtic.rda",
                "data/base/provinces/celtic/islands/3rdparty/celtic_island_3rdparty_pirate_01/celtic_island_3rdparty_pirate_01.a7m",
                false,
            ),
            (
                "dlc01_provinces.rda",
                "data/dlc01/provinces/roman/islands/pool/roman_dlc01_island_continental_01/roman_dlc01_island_continental_01.a7m",
                false,
            ),
        ];
        for (rda, path, marsh) in samples {
            let Ok(outer) = Archive::open(Path::new(&dir).join("maindata").join(rda)) else {
                eprintln!("skip: {rda} missing");
                continue;
            };
            let a7m = Archive::from_bytes(outer.read_path(path).unwrap(), path).unwrap();
            let gd = a7m.read_path("gamedata.data").unwrap();
            let rd = a7m.read_path("rd3d.data").unwrap();
            let mut island = Island::parse(&gd, &rd).unwrap();
            let g = Grids::read(&island).unwrap();
            assert_eq!(g.layers.len(), 14 + usize::from(marsh), "{path}");
            if let Some(m) = g.layer(Kind::Marsh) {
                let set = (0..m.width * m.height)
                    .filter(|&i| m.get(i % m.width, i / m.width) == MARSH)
                    .count();
                assert!(set > 1000, "{path}: {set} marsh cells");
            }
            let (gd2, rd2) = g.save(&mut island).unwrap();
            assert!(
                gd2 == gd && rd2 == rd,
                "{path}: unedited save changed bytes"
            );
        }
    }
}
