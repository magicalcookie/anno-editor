// An island's `Terrain` as typed 16 m chunks: native heights, `Flags` and
// paint layers, read in place from an `isd::Doc` and checked against the
// rules every shipped island keeps. Edits go through two owned forms, the
// heights as one grid of 1 m corners and the paint as per-chunk layers, and
// each writes back only what it changed plus what the game derives from it.
// Also owns the ground texture registry (`groundtextures.xml`), where a
// paint layer's draw rank comes from.
// Engine addresses here are Anno5.exe build 11514956 (Steam).

use anyhow::{Context, Result, anyhow, bail, ensure};

use super::grids::{self, Sculpt};
use super::{Doc, NodeId, Value};

// A chunk's height map form, which its `VertexResolution` names: how many
// samples a side its `HeightMap` holds, or none for an empty ocean chunk.
// Every shipped chunk is one of these (2026-11-19 census: 9,367 empty,
// 35,812 at 1 m, 827 at 0.5 m, 4 at 0.25 m). Every form stores its samples
// the same way: little-endian f32 metres, row by row, both edges included.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Layout {
    // `VertexResolution`.
    resolution: i8,
    // Samples a side; 0 for none.
    side: u8,
}

impl Layout {
    // Empty ocean: no `HeightMap`.
    const EMPTY: Layout = Layout {
        resolution: -1,
        side: 0,
    };
    // A sample a metre: the form a chunk a sculpt creates takes.
    const METRE: Layout = Layout {
        resolution: 4,
        side: Terrain::CHUNK as u8 + 1,
    };
    const HALF_METRE: Layout = Layout {
        resolution: 5,
        side: 2 * Terrain::CHUNK as u8 + 1,
    };
    const QUARTER_METRE: Layout = Layout {
        resolution: 6,
        side: 4 * Terrain::CHUNK as u8 + 1,
    };
    const ALL: [Layout; 4] = [
        Layout::EMPTY,
        Layout::METRE,
        Layout::HALF_METRE,
        Layout::QUARTER_METRE,
    ];

    fn of_side(side: usize) -> Option<Layout> {
        Layout::ALL
            .into_iter()
            .find(|l| l.side() == side && side > 0)
    }

    fn side(self) -> usize {
        self.side as usize
    }

    // Samples from one metre corner to the next.
    fn step(self) -> usize {
        (self.side() - 1) / Terrain::CHUNK
    }

    // Bytes its `Data` holds.
    fn bytes(self) -> usize {
        self.side() * self.side() * SAMPLE
    }
}

// Bytes in one height sample, in every form.
const SAMPLE: usize = size_of::<f32>();

// The editor's grid holds a corner a metre, so a chunk at 1 m holds its
// corners as they are and a created chunk takes that form.
const _: () = assert!(Layout::METRE.side as usize == Terrain::PLANE_SIDE);

// `Flags` bit 0: on most chunks, "has heights, all above `ABOVE` metres";
// 1,290 shipped chunks disagree, so a write flips it only when that
// predicate changes. A chunk a sculpt creates takes `CREATED_FLAGS` and
// the bit by the predicate, as shipped chunks with heights hold them.
const FLAG_ABOVE: u32 = 1;
const ABOVE: f32 = 0.25;
const CREATED_FLAGS: u32 = 0x20000;

/// A read-only view of an island's terrain. Chunks are numbered row by row,
/// `z * width + x`, x and z as the file stores them; samples inside a chunk
/// run the same way.
pub struct Terrain<'a> {
    doc: &'a Doc,
    width: u32,
    height: u32,
    chunks: Vec<Chunk>,
    /// Every chunk's layers back to back: `TextureIndex`, and the node
    /// holding its coverage plane.
    ids: Vec<u32>,
    planes: Vec<NodeId>,
}

#[derive(Clone, Copy)]
struct Chunk {
    element: NodeId,
    /// The `HeightMap`'s `Data`; unset for an empty ocean chunk.
    heights: NodeId,
    flags: u32,
    /// Where its layers start in `ids` and `planes`.
    first: u32,
    layers: u8,
    form: Layout,
}

// One chunk's height samples at their native spacing.
#[derive(Clone, Copy)]
struct Heights<'a> {
    form: Layout,
    samples: &'a [u8],
}

impl Heights<'_> {
    fn side(&self) -> usize {
        self.form.side()
    }

    fn at(&self, x: usize, z: usize) -> f32 {
        f32::from_le_bytes(self.bits(z * self.side() + x))
    }

    fn bits(&self, i: usize) -> [u8; SAMPLE] {
        self.samples[i * SAMPLE..][..SAMPLE]
            .try_into()
            .expect("a whole sample")
    }
}

impl Terrain<'_> {
    /// Metres along a chunk's side.
    pub const CHUNK: usize = 16;
    /// Coverage samples along a chunk's side: one per metre, both edges
    /// included.
    pub const PLANE_SIDE: usize = Terrain::CHUNK + 1;
    /// Bytes in one paint layer's coverage plane.
    pub const PLANE: usize = Terrain::PLANE_SIDE * Terrain::PLANE_SIDE;
    /// Most paint layers one chunk holds: the most any shipped chunk has. The
    /// shader's own 24 is a commented-out default (terrain.fx:8-9), so this
    /// is a content limit, not an engine one.
    pub const MAX_LAYERS: usize = 24;
}

impl<'a> Terrain<'a> {
    /// Reads `doc`'s terrain. Fails, naming `file` and the chunk, when a
    /// chunk's heights or planes are the wrong size, its layers are not
    /// ascending from 0 or number over [`Terrain::MAX_LAYERS`], or two neighbours
    /// disagree along their shared edge.
    pub fn read(doc: &'a Doc, file: &str) -> Result<Terrain<'a>> {
        let named = |e: anyhow::Error| anyhow!("{file}: {e:#}");
        let terrain = Self::index(doc).map_err(named)?;
        terrain.check_borders().map_err(named)?;
        Ok(terrain)
    }

    fn index(doc: &'a Doc) -> Result<Terrain<'a>> {
        let root = doc.root("Terrain").context("no <Terrain>")?;
        let map = child(doc, root, "ChunkMap")?;
        let width: u32 = number(doc, child(doc, map, "Width")?)?;
        let height: u32 = number(doc, child(doc, map, "Height")?)?;
        let count = width as usize * height as usize;
        let mut terrain = Terrain {
            doc,
            width,
            height,
            // The size is the file's claim; the nodes are what it holds.
            chunks: Vec::with_capacity(count.min(doc.len())),
            ids: Vec::new(),
            planes: Vec::new(),
        };
        for element in doc.children(map).filter(|&c| doc.name(c) == b"Element") {
            let k = terrain.chunks.len();
            ensure!(k < count, "more chunks than its {width}×{height} map");
            terrain.push_chunk(element).map_err(|e| {
                anyhow!("chunk {},{}: {e:#}", k % width as usize, k / width as usize)
            })?;
        }
        ensure!(
            terrain.chunks.len() == count,
            "{} chunks in a {width}×{height} map",
            terrain.chunks.len()
        );
        Ok(terrain)
    }

    fn push_chunk(&mut self, element: NodeId) -> Result<()> {
        let doc = self.doc;
        let mut chunk = Chunk {
            element,
            heights: element,
            flags: 0,
            first: self.ids.len() as u32,
            layers: 0,
            form: Layout::EMPTY,
        };
        let mut resolution = None;
        for node in doc.children(element) {
            match doc.name(node) {
                b"VertexResolution" => resolution = Some(number::<i32>(doc, node)?),
                b"Flags" => chunk.flags = number(doc, node)?,
                b"HeightMap" => {
                    ensure!(chunk.form == Layout::EMPTY, "two height maps");
                    let side: usize = number(doc, child(doc, node, "Width")?)?;
                    let form = Layout::of_side(side)
                        .with_context(|| format!("heights {side} samples wide"))?;
                    let (data, samples) = blob(doc, child(doc, node, "Data")?)?;
                    ensure!(
                        samples.len() == form.bytes(),
                        "heights hold {} bytes, {side}×{side} samples need {}",
                        samples.len(),
                        form.bytes()
                    );
                    chunk.heights = data;
                    chunk.form = form;
                }
                b"TexIndexData" => {
                    let id: u32 = number(doc, child(doc, node, "TextureIndex")?)?;
                    ensure!(
                        (chunk.layers as usize) < Terrain::MAX_LAYERS,
                        "more than {} layers",
                        Terrain::MAX_LAYERS
                    );
                    // The engine keeps a chunk's layers in a map keyed by
                    // `TextureIndex` (0x1408FC7C0), so files list them
                    // ascending. The view relies on it to find a layer.
                    match self.ids[chunk.first as usize..].last() {
                        None => ensure!(id == 0, "layers start with {id}, not the base layer 0"),
                        Some(&before) => {
                            ensure!(before < id, "layer {id} after {before}: not ascending")
                        }
                    }
                    let alpha = child(doc, node, "AlphaMap")?;
                    let side: usize = number(doc, child(doc, alpha, "Width")?)?;
                    let (data, plane) = blob(doc, child(doc, alpha, "Data")?)?;
                    ensure!(
                        side == Terrain::PLANE_SIDE && plane.len() == Terrain::PLANE,
                        "layer {id}: plane {side} wide with {} bytes, not {} and {}",
                        plane.len(),
                        Terrain::PLANE_SIDE,
                        Terrain::PLANE
                    );
                    self.ids.push(id);
                    self.planes.push(data);
                    chunk.layers += 1;
                }
                _ => {}
            }
        }
        let resolution = resolution.context("no <VertexResolution>")?;
        ensure!(
            resolution == i32::from(chunk.form.resolution),
            "VertexResolution {resolution} with heights {} samples wide",
            chunk.form.side
        );
        self.chunks.push(chunk);
        Ok(())
    }

    // Neighbours hold the same height bits and the same coverage along the
    // edge they share. Paint, heights and their writers all lean on it: a
    // vertex on an edge has one value, whichever chunk is asked.
    fn check_borders(&self) -> Result<()> {
        let (w, h) = self.size();
        for a in 0..w * h {
            let (cx, cz) = (a % w, a / w);
            for right in [true, false] {
                let b = match right {
                    true if cx + 1 < w => a + 1,
                    false if cz + 1 < h => a + w,
                    _ => continue,
                };
                self.check_edge(a, b, right).map_err(|e| {
                    let side = if right { "right" } else { "lower" };
                    anyhow!("chunk {cx},{cz}, {side} edge: {e:#}")
                })?;
            }
        }
        Ok(())
    }

    fn check_edge(&self, a: usize, b: usize, right: bool) -> Result<()> {
        if let (Some(ha), Some(hb)) = (self.heights(a), self.heights(b)) {
            for k in 0..Terrain::PLANE_SIDE {
                let (i, j) = (
                    edge(k, ha.side(), true, right),
                    edge(k, hb.side(), false, right),
                );
                ensure!(
                    ha.bits(i) == hb.bits(j),
                    "height {} at metre {k}, the neighbour holds {}",
                    f32::from_le_bytes(ha.bits(i)),
                    f32::from_le_bytes(hb.bits(j))
                );
            }
        }
        // An unpainted chunk says nothing about its edge.
        let (ia, ib) = (self.ids(a), self.ids(b));
        if ia.is_empty() || ib.is_empty() {
            return Ok(());
        }
        // Both lists ascend, so one walk visits every id either holds. A
        // layer one side lacks covers nothing there.
        let (mut i, mut j) = (0, 0);
        while i < ia.len() || j < ib.len() {
            let id = match (ia.get(i), ib.get(j)) {
                (Some(&x), Some(&y)) => x.min(y),
                (Some(&x), None) | (None, Some(&x)) => x,
                (None, None) => unreachable!(),
            };
            let plane = |ids: &[u32], at: &mut usize, chunk: usize| {
                (ids.get(*at) == Some(&id)).then(|| {
                    *at += 1;
                    self.plane(chunk, *at - 1)
                })
            };
            let (pa, pb) = (plane(ia, &mut i, a), plane(ib, &mut j, b));
            for k in 0..Terrain::PLANE_SIDE {
                let ca = pa.map_or(0, |p| p[edge(k, Terrain::PLANE_SIDE, true, right)]);
                let cb = pb.map_or(0, |p| p[edge(k, Terrain::PLANE_SIDE, false, right)]);
                ensure!(
                    ca == cb,
                    "layer {id} covers {ca} at metre {k}, the neighbour's {cb}"
                );
            }
        }
        Ok(())
    }

    /// Chunks per row, and rows.
    pub fn size(&self) -> (usize, usize) {
        (self.width as usize, self.height as usize)
    }

    // The chunk's heights; `None` for an empty ocean chunk.
    fn heights(&self, chunk: usize) -> Option<Heights<'a>> {
        let c = self.chunks[chunk];
        (c.form != Layout::EMPTY).then(|| Heights {
            form: c.form,
            samples: bytes(self.doc, c.heights),
        })
    }

    /// The chunk's paint layers by `TextureIndex`, ascending as stored and
    /// starting with the base layer 0; empty for an unpainted chunk. File
    /// order is not draw order: see [`GroundTextures::rank`].
    pub fn ids(&self, chunk: usize) -> &[u32] {
        let c = self.chunks[chunk];
        &self.ids[c.first as usize..][..c.layers as usize]
    }

    /// Coverage of the chunk's `layer`th id, a byte per metre vertex, row by
    /// row. The base layer's bytes are kept as stored (713 shipped base
    /// planes hold values under 255) although the shader draws it opaque.
    pub fn plane(&self, chunk: usize, layer: usize) -> &'a [u8; Terrain::PLANE] {
        let c = self.chunks[chunk];
        assert!(
            layer < c.layers as usize,
            "chunk {chunk} has no layer {layer}"
        );
        bytes(self.doc, self.planes[c.first as usize + layer])
            .try_into()
            .expect("plane size checked on read")
    }

    /// The island's heights at its 1 m corners. Neighbours hold the same
    /// bits along the edge they share (checked on read), so an edge corner
    /// has one value whichever chunk gives it.
    pub fn corners(&self) -> Corners {
        let (w, h) = self.size();
        let (width, height) = (w * Terrain::CHUNK + 1, h * Terrain::CHUNK + 1);
        let mut corners = Corners {
            width,
            height,
            heights: vec![0.; width * height],
            known: Bits::new(width * height),
            held: Bits::new(w * h),
        };
        for k in 0..w * h {
            let Some(native) = self.heights(k) else {
                continue;
            };
            corners.held.set(k);
            let step = native.form.step();
            let first = (k / w * Terrain::CHUNK) * width + k % w * Terrain::CHUNK;
            for z in 0..Terrain::PLANE_SIDE {
                for x in 0..Terrain::PLANE_SIDE {
                    let i = first + z * width + x;
                    corners.heights[i] = native.at(x * step, z * step);
                    corners.known.set(i);
                }
            }
        }
        corners
    }

    /// The island's paint as an owned copy to edit.
    pub fn layers(&self) -> Layers {
        let count = self.chunks.len();
        let mut layers = Layers {
            width: self.width,
            height: self.height,
            ids: vec![0; count * Terrain::MAX_LAYERS],
            slots: vec![0; count * Terrain::MAX_LAYERS],
            counts: vec![0; count],
            planes: Vec::with_capacity(self.planes.len() * Terrain::PLANE),
            free: Vec::new(),
        };
        for k in 0..count {
            for (layer, &id) in self.ids(k).iter().enumerate() {
                layers.ids[k * Terrain::MAX_LAYERS + layer] = id;
                layers.slots[k * Terrain::MAX_LAYERS + layer] =
                    (layers.planes.len() / Terrain::PLANE) as u32;
                layers.planes.extend_from_slice(self.plane(k, layer));
            }
            layers.counts[k] = self.ids(k).len() as u8;
        }
        layers
    }
}

/// Heights at an island's 1 m corners, the grid the editor sculpts:
/// `16 w + 1` by `16 h + 1` values for `w` by `h` chunks, row by row. A
/// chunk stored finer than 1 m gives its samples at whole metres. Corners
/// that only empty ocean chunks touch hold no height.
#[derive(Clone)]
pub struct Corners {
    width: usize,
    height: usize,
    heights: Vec<f32>,
    /// Corners that hold a height: those of a held chunk.
    known: Bits,
    /// Chunks that hold heights, as loaded or since a first sculpt.
    held: Bits,
}

impl Corners {
    /// Corners per row, and rows.
    pub fn size(&self) -> (usize, usize) {
        (self.width, self.height)
    }

    fn chunks(&self) -> (usize, usize) {
        (
            (self.width - 1) / Terrain::CHUNK,
            (self.height - 1) / Terrain::CHUNK,
        )
    }

    // Corner `n` of `chunk`'s 17×17.
    fn corner(&self, chunk: usize, n: usize) -> usize {
        let w = self.chunks().0;
        (chunk / w * Terrain::CHUNK + n / Terrain::PLANE_SIDE) * self.width
            + chunk % w * Terrain::CHUNK
            + n % Terrain::PLANE_SIDE
    }

    /// The height at corner (x, z) in island metres; `None` in empty ocean.
    pub fn get(&self, x: usize, z: usize) -> Option<f32> {
        assert!(x < self.width && z < self.height, "corner {x},{z}");
        let i = z * self.width + x;
        self.known.get(i).then(|| self.heights[i])
    }

    /// Moves corner (x, z) to `height` metres. A corner in empty ocean
    /// gives every chunk around it heights; the write puts their other
    /// corners at the sculpt's seabed unless a neighbour holds them, and
    /// makes their records. A corner a chunk already holds changes in that
    /// chunk only.
    pub fn set(&mut self, x: usize, z: usize, height: f32) {
        assert!(x < self.width && z < self.height, "corner {x},{z}");
        let i = z * self.width + x;
        if !self.known.get(i) {
            let (w, h) = self.chunks();
            for cz in sharing(z, h) {
                for cx in sharing(x, w) {
                    self.held.set(cz * w + cx);
                }
            }
            self.known.set(i);
        }
        self.heights[i] = height;
    }

    // A copy with every corner of a held chunk known, those no chunk gave a
    // height at `seabed`; `None` when there are none to fill.
    fn filled(&self, seabed: f32) -> Option<Corners> {
        let (w, h) = self.chunks();
        let empty =
            |c: &Corners, k: usize| (0..Terrain::PLANE).any(|n| !c.known.get(c.corner(k, n)));
        let mut held = (0..w * h).filter(|&k| self.held.get(k) && empty(self, k));
        let first = held.next()?;
        let mut filled = self.clone();
        for k in std::iter::once(first).chain(held) {
            for n in 0..Terrain::PLANE {
                let i = self.corner(k, n);
                if !filled.known.get(i) {
                    filled.heights[i] = seabed;
                    filled.known.set(i);
                }
            }
        }
        Some(filled)
    }

    /// Writes the heights into `doc`, a copy of the island they were read
    /// from. Only what the moved corners reach changes:
    /// - each chunk holding one gets its 17×17 samples from the grid, so
    ///   chunks sharing a moved corner get the same bits
    /// - a chunk stored at 0.5 m or 0.25 m keeps its samples and adds the
    ///   corners' change, bilinear across each 1 m cell; only samples in
    ///   the cells around a moved corner change, and those on it take its
    ///   new value exactly
    /// - its `Flags` bit 0 flips when "all heights above 0.25 m" turns true
    ///   or false; 1,290 shipped chunks disagree with that predicate, so a
    ///   flag whose predicate held still is left as loaded
    /// - the gameplay heights of the cells around a moved corner follow
    ///   [`gameplay_height`](super::grids::gameplay_height); every other cell stays
    ///   as loaded, the 9,778 shipped cells off the rule included. An
    ///   island with v1 gameplay heights gets v2 by the rule for every cell
    /// - a cell lifted from `sculpt.block_lifted_above` or below to above
    ///   it is blocked in `m_PathBlockerLayer`; the submarine blocker is
    ///   rebuilt around cells that crossed its level
    ///
    /// An empty ocean chunk the grid now holds gets its records: 17×17
    /// heights, those no neighbour holds at `sculpt.seabed`, `Flags`
    /// 0x20000 (bit 0 by the predicate), and paint that agrees with its
    /// painted neighbours along its edges, the base layer opaque inside. A
    /// chunk the grid holds no heights for is left as `doc` has it, so
    /// write into a fresh copy of the loaded island.
    ///
    /// Fails before changing `doc` on an edit it cannot write. Reads the
    /// terrain back afterwards, so a result that breaks a rule fails too;
    /// `doc` is only good to drop then.
    pub fn write(&self, doc: &mut Doc, file: &str, sculpt: Sculpt) -> Result<()> {
        let named = |e: anyhow::Error| anyhow!("{file}: {e:#}");
        let terrain = Terrain::read(doc, file)?;
        let filled = self.filled(sculpt.seabed);
        let corners = filled.as_ref().unwrap_or(self);
        let changes = corners.changes(&terrain, sculpt).map_err(named)?;
        if changes.values.is_empty() && changes.created.is_empty() {
            return Ok(());
        }
        for (node, value, blob) in changes.values {
            doc.set(node, &value, blob).map_err(named)?;
        }
        for (node, name) in changes.renames {
            doc.rename(node, name).map_err(named)?;
        }
        // An insert moves every node after it. Last chunk first, the
        // elements still to come keep their ids.
        for (element, flags, samples) in changes.created.iter().rev() {
            create_heights(doc, *element, *flags, samples).map_err(named)?;
        }
        match changes.paint {
            // Inserts the new chunks' layers, and reads the result back.
            Some(paint) => paint.write(doc, file, sculpt),
            None => Terrain::read(doc, file).map(drop),
        }
    }

    fn changes(&self, terrain: &Terrain, sculpt: Sculpt) -> Result<HeightChanges> {
        let doc = terrain.doc;
        let loaded = terrain.corners();
        ensure!(
            self.size() == loaded.size(),
            "{}×{} corners for an island of {}×{}",
            self.width,
            self.height,
            loaded.width,
            loaded.height
        );
        let (width, height) = self.size();
        let w = terrain.size().0;
        // A corner moved when the grid holds it and the island holds it
        // elsewhere, or not at all. One the island holds and the grid does
        // not belongs to a chunk the grid leaves alone.
        let mut moved = Bits::new(width * height);
        let mut any = false;
        for i in 0..width * height {
            let same =
                loaded.known.get(i) && loaded.heights[i].to_bits() == self.heights[i].to_bits();
            if self.known.get(i) && !same {
                moved.set(i);
                any = true;
            }
        }
        let mut changes = HeightChanges::default();
        if !any {
            return Ok(changes);
        }

        let above = |samples: &[u8]| {
            let (heights, _) = samples.as_chunks::<SAMPLE>();
            heights.iter().all(|&v| f32::from_le_bytes(v) > ABOVE)
        };
        let mut paint = None;
        for (k, chunk) in terrain.chunks.iter().enumerate() {
            let at = |n: usize| self.corner(k, n);
            if !(0..Terrain::PLANE).any(|n| moved.get(at(n))) {
                continue;
            }
            let Some(native) = terrain.heights(k) else {
                if !self.held.get(k) {
                    continue;
                }
                let samples: Vec<u8> = (0..Terrain::PLANE)
                    .flat_map(|n| self.heights[at(n)].to_le_bytes())
                    .collect();
                let flags = CREATED_FLAGS | if above(&samples) { FLAG_ABOVE } else { 0 };
                changes.created.push((chunk.element, flags, samples));
                // Shipped empty chunks hold no paint, so this one takes
                // its base and its neighbours' edges.
                if chunk.layers == 0 {
                    let paint = paint.get_or_insert_with(|| terrain.layers());
                    ensure!(
                        paint.create(k),
                        "chunk {},{}: its neighbours hold more than {} layers along its edges",
                        k % w,
                        k / w,
                        Terrain::MAX_LAYERS
                    );
                }
                continue;
            };
            let samples = match native.form {
                Layout::METRE => (0..Terrain::PLANE)
                    .flat_map(|n| {
                        let i = at(n);
                        match moved.get(i) {
                            true => self.heights[i].to_le_bytes(),
                            false => native.bits(n),
                        }
                    })
                    .collect(),
                _ => self.fine(native, at(0), &loaded, &moved),
            };
            if above(native.samples) != above(&samples) {
                let flags =
                    (chunk.flags & !FLAG_ABOVE) | if above(&samples) { FLAG_ABOVE } else { 0 };
                let node = child(doc, chunk.element, "Flags")?;
                changes
                    .values
                    .push((node, flags.to_string().into_bytes(), false));
            }
            changes.values.push((chunk.heights, samples, true));
        }
        changes.paint = paint;
        changes.renames = grids::sculpted(doc, &loaded, self, &moved, sculpt, &mut changes.values)?;
        Ok(changes)
    }

    // The samples of a chunk stored finer than 1 m, `first` its first
    // corner, after the corners in `moved` left `loaded`. Each sample keeps
    // its native value plus the corners' change, interpolated bilinearly
    // from the four corners of its 1 m cell, so relief between the corners
    // stays. The footprint is the cells around a moved corner: a sample in
    // a cell none of whose corners moved keeps its bits, and one on a moved
    // corner takes the corner's new value exactly, as a 1 m chunk would.
    fn fine(&self, native: Heights, first: usize, loaded: &Corners, moved: &Bits) -> Vec<u8> {
        let (side, step) = (native.side(), native.form.step());
        let width = self.width;
        let delta = |i: usize| match moved.get(i) {
            true => self.heights[i] as f64 - loaded.heights[i] as f64,
            false => 0.,
        };
        // Weights of exactly 0 and 1 give back the other corner's delta
        // bit for bit, so two fine chunks agree along the edge they share.
        let lerp = |a: f64, b: f64, t: f64| a * (1. - t) + b * t;
        // A sample's cell and where in it the sample lies; the last sample
        // of a row lies at the far end of the last cell.
        let cell = |s: usize| {
            let c = (s / step).min(Terrain::CHUNK - 1);
            (c, (s - c * step) as f64 / step as f64)
        };
        let mut samples = native.samples.to_vec();
        for sz in 0..side {
            for sx in 0..side {
                let ((cx, fx), (cz, fz)) = (cell(sx), cell(sz));
                let corner = first + cz * width + cx;
                let corners = [corner, corner + 1, corner + width, corner + width + 1];
                if !corners.iter().any(|&i| moved.get(i)) {
                    continue;
                }
                let value = if sx % step == 0 && sz % step == 0 {
                    let i = first + sz / step * width + sx / step;
                    if !moved.get(i) {
                        continue;
                    }
                    self.heights[i]
                } else {
                    let [a, b, c, d] = corners.map(delta);
                    let change = lerp(lerp(a, b, fx), lerp(c, d, fx), fz);
                    (native.at(sx, sz) as f64 + change) as f32
                };
                let n = (sz * side + sx) * SAMPLE;
                samples[n..n + SAMPLE].copy_from_slice(&value.to_le_bytes());
            }
        }
        samples
    }
}

// What a height write sets and creates, found before it changes anything.
#[derive(Default)]
struct HeightChanges {
    /// (node, bytes, binary) of each value to set.
    values: Vec<(NodeId, Vec<u8>, bool)>,
    /// (chunk element, `Flags`, 17×17 samples) of each empty chunk that
    /// takes heights, in document order.
    created: Vec<(NodeId, u32, Vec<u8>)>,
    /// The island's paint with a base for each created chunk that had none.
    paint: Option<Layers>,
    /// Nodes to rename: v1 gameplay heights once written as v2.
    renames: Vec<(NodeId, &'static str)>,
}

// Gives the empty chunk `element` its heights: `VertexResolution` 4, a
// `HeightMap` copied from another chunk's before its `Flags`, as shipped
// chunks order them, and the new `Flags`.
fn create_heights(doc: &mut Doc, element: NodeId, flags: u32, samples: &[u8]) -> Result<()> {
    let resolution = child(doc, element, "VertexResolution")?;
    let form = Layout::METRE;
    doc.set(resolution, form.resolution.to_string().as_bytes(), false)?;
    let flag = child(doc, element, "Flags")?;
    doc.set(flag, flags.to_string().as_bytes(), false)?;
    let template = in_chunks(doc, "HeightMap").context("no chunk with heights to copy")?;
    let new = doc.insert_copy(template, element, Some(flag))?;
    doc.set(
        child(doc, new, "Width")?,
        form.side.to_string().as_bytes(),
        false,
    )?;
    doc.set(child(doc, new, "Data")?, samples, true)
}

// The first node named `name` in any chunk of the chunk map.
fn in_chunks(doc: &Doc, name: &str) -> Option<NodeId> {
    let map = doc.child(doc.root("Terrain")?, "ChunkMap")?;
    (doc.children(map)).find_map(|element| doc.child(element, name))
}

/// An island's paint in 2070's own form, owned, to change chunk by chunk
/// with [`set`](Self::set): per chunk an ascending list of layer ids and a
/// 17×17 coverage plane for each. Chunks
/// and planes are laid out as in [`Terrain`].
pub struct Layers {
    width: u32,
    height: u32,
    /// [`Terrain::MAX_LAYERS`] slots per chunk. The first `counts[chunk]` hold its
    /// ids, ascending.
    ids: Vec<u32>,
    /// Beside each id, its plane's number in `planes`.
    slots: Vec<u32>,
    counts: Vec<u8>,
    /// Every plane, [`Terrain::PLANE`] bytes each. No plane moves, so slots stay
    /// good; a new one takes a free slot or goes on the end.
    planes: Vec<u8>,
    /// Slots no layer uses since [`set`](Self::set) replaced theirs.
    free: Vec<u32>,
}

impl Layers {
    /// Chunks per row, and rows.
    fn size(&self) -> (usize, usize) {
        (self.width as usize, self.height as usize)
    }

    // The chunk's layer ids, ascending; empty for an unpainted chunk.
    fn ids(&self, chunk: usize) -> &[u32] {
        &self.ids[chunk * Terrain::MAX_LAYERS..][..self.counts[chunk] as usize]
    }

    // Coverage of the chunk's `layer`th id.
    fn plane(&self, chunk: usize, layer: usize) -> &[u8; Terrain::PLANE] {
        assert!(
            layer < self.counts[chunk] as usize,
            "chunk {chunk} has no layer {layer}"
        );
        self.pooled(self.slots[chunk * Terrain::MAX_LAYERS + layer])
    }

    fn pooled(&self, slot: u32) -> &[u8; Terrain::PLANE] {
        self.planes[slot as usize * Terrain::PLANE..][..Terrain::PLANE]
            .try_into()
            .expect("a whole plane")
    }

    // A slot holding a plane of zeros.
    fn alloc(&mut self) -> u32 {
        match self.free.pop() {
            Some(slot) => {
                self.planes[slot as usize * Terrain::PLANE..][..Terrain::PLANE].fill(0);
                slot
            }
            None => {
                self.planes.resize(self.planes.len() + Terrain::PLANE, 0);
                (self.planes.len() / Terrain::PLANE - 1) as u32
            }
        }
    }

    fn plane_mut(&mut self, slot: u32) -> &mut [u8] {
        &mut self.planes[slot as usize * Terrain::PLANE..][..Terrain::PLANE]
    }

    // Paints the unpainted chunk so it agrees with every painted chunk it
    // shares a vertex with: along its edges each of their layers covers
    // what it covers there, inside it only the base, opaque. Where no
    // painted chunk holds an edge vertex the base is opaque there too.
    // False, painting nothing, when that takes more than
    // [`Terrain::MAX_LAYERS`].
    fn create(&mut self, chunk: usize) -> bool {
        debug_assert_eq!(self.counts[chunk], 0, "chunk {chunk} is painted");
        let (w, h) = self.size();
        let (cx, cz) = (chunk % w, chunk / w);
        let mut ids = vec![0u32];
        let mut planes = vec![[255u8; Terrain::PLANE]];
        for n in 0..Terrain::PLANE {
            let (lx, lz) = (n % Terrain::PLANE_SIDE, n / Terrain::PLANE_SIDE);
            if !(lx == 0 || lx == Terrain::CHUNK || lz == 0 || lz == Terrain::CHUNK) {
                continue;
            }
            let (x, z) = (cx * Terrain::CHUNK + lx, cz * Terrain::CHUNK + lz);
            // Painted chunks agree on every vertex they share, so the first
            // one holding it speaks for all.
            let holder = chunks_at((w, h), x, z).find(|&k| k != chunk && self.counts[k] > 0);
            let Some(k) = holder else {
                continue;
            };
            let at =
                (z - k / w * Terrain::CHUNK) * Terrain::PLANE_SIDE + x - k % w * Terrain::CHUNK;
            for (layer, &id) in self.ids(k).iter().enumerate() {
                let v = self.plane(k, layer)[at];
                let j = match ids.binary_search(&id) {
                    Ok(j) => j,
                    Err(_) if v == 0 => continue,
                    Err(j) => {
                        ids.insert(j, id);
                        planes.insert(j, [0; Terrain::PLANE]);
                        j
                    }
                };
                planes[j][n] = v;
            }
        }
        if ids.len() > Terrain::MAX_LAYERS {
            return false;
        }
        for (layer, (id, plane)) in ids.into_iter().zip(&planes).enumerate() {
            let slot = self.alloc();
            self.plane_mut(slot).copy_from_slice(plane);
            self.ids[chunk * Terrain::MAX_LAYERS + layer] = id;
            self.slots[chunk * Terrain::MAX_LAYERS + layer] = slot;
        }
        self.counts[chunk] = planes.len() as u8;
        true
    }

    // Unpaints the chunk, freeing its planes.
    fn clear(&mut self, chunk: usize) {
        let base = chunk * Terrain::MAX_LAYERS;
        let count = self.counts[chunk] as usize;
        self.free.extend_from_slice(&self.slots[base..base + count]);
        self.counts[chunk] = 0;
    }

    /// Gives the chunk exactly these layers: `ids` ascending from the base
    /// 0, at most [`Terrain::MAX_LAYERS`], and their planes back to back; none
    /// leaves it unpainted. For an editor holding its own copy of the paint
    /// to write it through [`write`](Self::write), which reads every shared
    /// edge back.
    pub fn set(&mut self, chunk: usize, ids: &[u32], planes: &[u8]) -> Result<()> {
        ensure!(
            ids.len() <= Terrain::MAX_LAYERS && planes.len() == ids.len() * Terrain::PLANE,
            "chunk {chunk}: {} layers in {} bytes",
            ids.len(),
            planes.len()
        );
        ensure!(
            ids.first().is_none_or(|&id| id == 0) && ids.is_sorted_by(|a, b| a < b),
            "chunk {chunk}: layers {ids:?} don't ascend from the base 0"
        );
        self.clear(chunk);
        for (layer, &id) in ids.iter().enumerate() {
            let slot = self.alloc();
            self.plane_mut(slot)
                .copy_from_slice(&planes[layer * Terrain::PLANE..][..Terrain::PLANE]);
            self.ids[chunk * Terrain::MAX_LAYERS + layer] = id;
            self.slots[chunk * Terrain::MAX_LAYERS + layer] = slot;
        }
        self.counts[chunk] = ids.len() as u8;
        Ok(())
    }

    /// Writes the paint into `doc`, a copy of the island it was read from:
    /// changed planes in place, new layers as new `TexIndexData` in id
    /// order, and layers the paint no longer holds removed. A chunk painted
    /// in empty ocean first gets heights from its neighbours' edges, at
    /// `sculpt.seabed` inside, as [`Corners::write`] makes them; an unpainted
    /// chunk is left as `doc` has it. Then reads the terrain back, so a
    /// result that breaks a rule fails here; `doc` is half written then and
    /// only good to drop.
    pub fn write(&self, doc: &mut Doc, file: &str, sculpt: Sculpt) -> Result<()> {
        let named = |e: anyhow::Error| anyhow!("{file}: {e:#}");
        let terrain = Terrain::read(doc, file)?;
        ensure!(
            self.size() == terrain.size(),
            "{file}: paint for {}×{} chunks on an island of {}×{}",
            self.width,
            self.height,
            terrain.width,
            terrain.height
        );
        let bare: Vec<usize> = (0..self.counts.len())
            .filter(|&k| self.counts[k] > 0 && terrain.heights(k).is_none())
            .collect();
        if !bare.is_empty() {
            let mut corners = terrain.corners();
            for &k in &bare {
                corners.held.set(k);
            }
            corners.write(doc, file, sculpt)?;
        }
        let terrain = Terrain::read(doc, file)?;
        let changes = self.changes(&terrain).map_err(named)?;
        if changes.planes.is_empty() && changes.layers.is_empty() {
            return Ok(());
        }
        for (node, slot) in changes.planes {
            doc.set(node, self.pooled(slot), true).map_err(named)?;
        }
        // An insert or removal moves every node after it. Going backwards,
        // last chunk first and highest id first within a chunk, the nodes
        // still to be used keep their ids.
        for &(element, id, slot) in changes.layers.iter().rev() {
            match slot {
                Some(slot) => self.insert_into(doc, element, id, slot),
                None => remove_layer(doc, element, id),
            }
            .map_err(named)?;
        }
        Terrain::read(doc, file)?;
        Ok(())
    }

    fn changes(&self, terrain: &Terrain) -> Result<PaintChanges> {
        let mut changes = PaintChanges::default();
        for (k, chunk) in terrain.chunks.iter().enumerate() {
            if self.counts[k] == 0 {
                continue;
            }
            let (cx, cz) = (k % self.width as usize, k / self.width as usize);
            let (stored, painted) = (terrain.ids(k), self.ids(k));
            // Both lists ascend; one walk pairs them up.
            let (mut i, mut j) = (0, 0);
            while i < stored.len() || j < painted.len() {
                match (stored.get(i), painted.get(j)) {
                    (Some(&held), Some(&id)) if held == id => {
                        let slot = self.slots[k * Terrain::MAX_LAYERS + j];
                        if terrain.plane(k, i) != self.pooled(slot) {
                            let node = terrain.planes[chunk.first as usize + i];
                            changes.planes.push((node, slot));
                        }
                        i += 1;
                        j += 1;
                    }
                    (Some(&held), id) if id.is_none_or(|&id| held < id) => {
                        ensure!(held != 0, "chunk {cx},{cz}: the paint lost its base layer");
                        changes.layers.push((chunk.element, held, None));
                        i += 1;
                    }
                    (_, Some(&id)) => {
                        let slot = self.slots[k * Terrain::MAX_LAYERS + j];
                        changes.layers.push((chunk.element, id, Some(slot)));
                        j += 1;
                    }
                    (_, None) => unreachable!("one list still has ids"),
                }
            }
        }
        Ok(changes)
    }

    // A new `TexIndexData` for `id` in the chunk `element`: a copy of a
    // stored layer with the id and plane replaced, before the first layer
    // with a higher id.
    fn insert_into(&self, doc: &mut Doc, element: NodeId, id: u32, slot: u32) -> Result<()> {
        let mut before = None;
        for layer in doc.children(element) {
            if doc.name(layer) == b"TexIndexData"
                && number::<u32>(doc, child(doc, layer, "TextureIndex")?)? > id
            {
                before = Some(layer);
                break;
            }
        }
        let template = in_chunks(doc, "TexIndexData").context("no layer to copy")?;
        let new = doc.insert_copy(template, element, before)?;
        let index = child(doc, new, "TextureIndex")?;
        doc.set(index, id.to_string().as_bytes(), false)?;
        let data = child(doc, child(doc, new, "AlphaMap")?, "Data")?;
        doc.set(data, self.pooled(slot), true)
    }
}

// Removes layer `id` from the chunk `element`.
fn remove_layer(doc: &mut Doc, element: NodeId, id: u32) -> Result<()> {
    for layer in doc.children(element) {
        if doc.name(layer) == b"TexIndexData"
            && number::<u32>(doc, child(doc, layer, "TextureIndex")?)? == id
        {
            doc.remove(layer);
            return Ok(());
        }
    }
    bail!("no layer {id} to remove")
}

// What a paint write has to do, found before it changes anything.
#[derive(Default)]
struct PaintChanges {
    /// (`Data` node, slot) of each stored plane whose bytes changed.
    planes: Vec<(NodeId, u32)>,
    /// (chunk element, id, slot) of each layer to insert, or with no slot
    /// to remove, in document order.
    layers: Vec<(NodeId, u32, Option<u32>)>,
}

/// The chunks that hold corner or vertex (x, z), in island metres, on an
/// island of `chunks` (per row, rows): one inside a chunk, two on an edge,
/// four where chunks meet. An edit there changes these.
pub fn chunks_at(
    chunks: (usize, usize),
    x: usize,
    z: usize,
) -> impl Iterator<Item = usize> + Clone {
    let (w, h) = chunks;
    sharing(z, h).flat_map(move |cz| sharing(x, w).map(move |cx| cz * w + cx))
}

// The chunks, of `chunks` in a row, that hold metre `v`: two where it lies
// on an inner chunk edge.
fn sharing(v: usize, chunks: usize) -> std::ops::Range<usize> {
    let chunk = v / Terrain::CHUNK;
    let first = chunk - (v.is_multiple_of(Terrain::CHUNK) && chunk > 0) as usize;
    first..(chunk + 1).min(chunks)
}

// A bit per cell of a grid.
#[derive(Clone)]
pub(super) struct Bits(Vec<u64>);

impl Bits {
    pub(super) fn new(len: usize) -> Bits {
        Bits(vec![0; len.div_ceil(64)])
    }

    pub(super) fn get(&self, i: usize) -> bool {
        self.0[i / 64] >> (i % 64) & 1 == 1
    }

    pub(super) fn set(&mut self, i: usize) {
        self.0[i / 64] |= 1 << (i % 64);
    }
}

// Where metre `k` of the edge two neighbours share sits in a grid `side`
// samples wide: in the first chunk's last column (`right`) or row, or the
// second chunk's first.
fn edge(k: usize, side: usize, last: bool, right: bool) -> usize {
    let step = (side - 1) / Terrain::CHUNK;
    let across = if last { side - 1 } else { 0 };
    if right {
        k * step * side + across
    } else {
        across * side + k * step
    }
}

pub(super) fn child(doc: &Doc, parent: NodeId, name: &str) -> Result<NodeId> {
    doc.child(parent, name).with_context(|| {
        format!(
            "no <{name}> in <{}>",
            String::from_utf8_lossy(doc.name(parent))
        )
    })
}

pub(super) fn number<T: std::str::FromStr>(doc: &Doc, id: NodeId) -> Result<T> {
    let name = || String::from_utf8_lossy(doc.name(id));
    let Value::Text(text) = doc.value(id) else {
        return Err(anyhow!("<{}> holds no number", name()));
    };
    std::str::from_utf8(text)
        .ok()
        .and_then(|t| t.parse().ok())
        .with_context(|| {
            format!(
                "<{}> holds `{}`, not a number in range",
                name(),
                String::from_utf8_lossy(text)
            )
        })
}

pub(super) fn blob(doc: &Doc, id: NodeId) -> Result<(NodeId, &[u8])> {
    match doc.value(id) {
        Value::Blob(b) => Ok((id, b)),
        _ => Err(anyhow!(
            "<{}> holds no binary value",
            String::from_utf8_lossy(doc.name(id))
        )),
    }
}

fn bytes(doc: &Doc, id: NodeId) -> &[u8] {
    match doc.value(id) {
        Value::Blob(b) => b,
        _ => unreachable!("checked on read"),
    }
}

/// The ground textures of `data/config/engine/groundtextures.xml`, keyed by
/// the `GUID` an island's `TextureIndex` names.
pub struct GroundTextures {
    /// By id: `SplatID` in the low 16 bits, [`KNOWN`], [`SCATTER`] and
    /// [`PLACEHOLDER`] above.
    /// Zero for an id the file lacks.
    packed: Vec<u32>,
    textures: Vec<GroundTexture>,
    /// `TextureScale`, the file's one: metres one repeat of a layer's
    /// textures spans.
    scale: f32,
}

/// One registry entry, as the file has it.
pub struct GroundTexture {
    /// `GUID`: the `TextureIndex` of the layers painted with it.
    pub id: u32,
    /// `SplatID`, the draw rank.
    pub rank: u16,
    pub name: String,
    /// Texture paths with the file's backslashes. A scatter layer has none.
    pub diffuse: Option<String>,
    pub normal: Option<String>,
    /// `GrassTexture`: what a scatter layer's grass or bush objects are
    /// textured with, which every shipped scatter layer names and no
    /// colour layer does.
    pub grass: Option<String>,
}

const KNOWN: u32 = 1 << 31;
const SCATTER: u32 = 1 << 30;
const PLACEHOLDER: u32 = 1 << 29;
/// The developers' stand-in diffuse, a magenta checker (Data2.rda). Ids 9,
/// 11, 35 and 88 name it, and 45 shipped islands paint with them.
const PLACEHOLDER_DIFFUSE: &str = "wiki_dummy_diff";
// Ids index a flat table. Shipped ids reach 108.
const MAX_ID: u32 = 4095;

impl GroundTextures {
    pub fn parse(xml: &str) -> Result<GroundTextures> {
        let mut packed = Vec::new();
        let mut textures = Vec::new();
        // Before the first entry, so no texture's own field can answer;
        // 4 m where the file gives none, as the shipped one says.
        let head = xml.split("<Texture>").next().unwrap_or(xml);
        let scale = crate::xml::tag(head, "TextureScale")
            .map(|s| {
                s.trim()
                    .parse()
                    .with_context(|| format!("TextureScale {s:?}"))
            })
            .transpose()?
            .unwrap_or(4.);
        for block in xml.split("<Texture>").skip(1) {
            let block = block.split("</Texture>").next().unwrap_or(block);
            let field = |name: &str| crate::xml::tag(block, name);
            let id: u32 = field("GUID")
                .and_then(|v| v.parse().ok())
                .context("ground texture without a GUID")?;
            ensure!(id <= MAX_ID, "ground texture GUID {id} over {MAX_ID}");
            // The engine sorts layers on a u16 (0x1408317A0).
            let rank: u16 = field("SplatID")
                .and_then(|v| v.parse().ok())
                .with_context(|| format!("ground texture {id} without a SplatID"))?;
            let diffuse = field("DiffuseTexture").map(str::to_owned);
            if packed.len() <= id as usize {
                packed.resize(id as usize + 1, 0);
            }
            ensure!(packed[id as usize] == 0, "ground texture {id} listed twice");
            let placeholder = diffuse.as_deref().is_some_and(|d| {
                let file = d.rsplit(['\\', '/']).next().unwrap_or(d);
                file.split('.').next() == Some(PLACEHOLDER_DIFFUSE)
            });
            packed[id as usize] = KNOWN
                | if diffuse.is_none() { SCATTER } else { 0 }
                | if placeholder { PLACEHOLDER } else { 0 }
                | rank as u32;
            let name = field("Name").unwrap_or_default();
            textures.push(GroundTexture {
                id,
                rank,
                name: quick_xml::escape::unescape(name)
                    .map_or_else(|_| name.to_owned(), |n| n.into_owned()),
                diffuse,
                normal: field("NormalTexture").map(str::to_owned),
                grass: field("GrassTexture").map(str::to_owned),
            });
        }
        ensure!(!textures.is_empty(), "no ground textures");
        Ok(GroundTextures {
            packed,
            textures,
            scale,
        })
    }

    /// Metres one repeat of a layer's textures spans (`TextureScale`).
    pub fn scale(&self) -> f32 {
        self.scale
    }

    fn packed(&self, id: u32) -> Option<u32> {
        let p = *self.packed.get(id as usize)?;
        (p & KNOWN != 0).then_some(p)
    }

    /// Draw rank of layer `id`: a chunk's layers stack in ascending rank,
    /// whatever order the file lists them in. `None` for an unregistered id.
    pub fn rank(&self, id: u32) -> Option<u16> {
        self.packed(id).map(|p| p as u16)
    }

    /// Whether `id` places grass or bush objects instead of drawing colour.
    pub fn scatter(&self, id: u32) -> bool {
        self.packed(id).is_some_and(|p| p & SCATTER != 0)
    }

    /// Whether `id` draws the developers' magenta stand-in texture. The game
    /// draws it as any other layer at its default High preset and at Very
    /// High, so painted ground shows magenta in game too.
    pub fn placeholder(&self, id: u32) -> bool {
        self.packed(id).is_some_and(|p| p & PLACEHOLDER != 0)
    }

    /// Every texture, in file order.
    pub fn textures(&self) -> &[GroundTexture] {
        &self.textures
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::isd::grids::Version;

    // 2070's values, as games/anno2070 holds them (`island::SCULPT`): lifts
    // past the land's buildable band block, and the seabed.
    const SCULPT: Sculpt = Sculpt {
        block_lifted_above: Some(768),
        seabed: -40.,
    };

    // An island in the shipped layout, spelled out without the writer: the
    // chunk map closes its tags with a bare `</>`.
    #[derive(Clone, Default)]
    struct TestChunk {
        heights: Option<(Layout, Vec<f32>)>,
        flags: u32,
        layers: Vec<(u32, Vec<u8>)>,
    }

    fn island(width: usize, height: usize, chunks: &[TestChunk]) -> Vec<u8> {
        island_with(width, height, chunks, None)
    }

    // `gameplay` adds the grid manager, with gameplay heights of that
    // version.
    fn island_with(
        width: usize,
        height: usize,
        chunks: &[TestChunk],
        gameplay: Option<(Version, &[i16])>,
    ) -> Vec<u8> {
        island_grids(width, height, chunks, gameplay, b"")
    }

    // `grids` follow the gameplay heights in the grid manager.
    fn island_grids(
        width: usize,
        height: usize,
        chunks: &[TestChunk],
        gameplay: Option<(Version, &[i16])>,
        grids: &[u8],
    ) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend(format!("<Width>{}</Width>\r\n", width * Terrain::CHUNK).bytes());
        b.extend(format!("<Height>{}</Height>\r\n", height * Terrain::CHUNK).bytes());
        if let Some((version, cells)) = gameplay {
            let tag = version.layout().tag;
            b.extend(format!("<m_GOPManager><m_GRIDManager><{tag}>CDATA[").bytes());
            b.extend((size_of_val(cells) as u32).to_le_bytes());
            b.extend(cells.iter().flat_map(|c| c.to_le_bytes()));
            b.extend(format!("]</{tag}>\r\n").bytes());
            b.extend(grids);
            b.extend(b"</m_GRIDManager>\r\n</m_GOPManager>\r\n");
        }
        b.extend(b"<Terrain><Version>3</>\r\n<ChunkMap>");
        b.extend(format!("<Width>{width}</>\r\n<Height>{height}</>\r\n").bytes());
        let cdata = |b: &mut Vec<u8>, data: &[u8]| {
            b.extend(b"<Data>CDATA[");
            b.extend((data.len() as u32).to_le_bytes());
            b.extend(data);
            b.extend(b"]</>\r\n");
        };
        for c in chunks {
            b.extend(b"<Element>");
            match &c.heights {
                Some((form, samples)) => {
                    let (resolution, side) = (form.resolution, form.side);
                    b.extend(format!("<VertexResolution>{resolution}</>\r\n").bytes());
                    b.extend(format!("<HeightMap><Width>{side}</>\r\n").bytes());
                    let data: Vec<u8> = samples.iter().flat_map(|h| h.to_le_bytes()).collect();
                    cdata(&mut b, &data);
                    b.extend(b"</>\r\n");
                }
                None => {
                    let resolution = Layout::EMPTY.resolution;
                    b.extend(format!("<VertexResolution>{resolution}</>\r\n").bytes());
                }
            }
            b.extend(format!("<Flags>{}</>\r\n", c.flags).bytes());
            for (id, plane) in &c.layers {
                b.extend(format!("<TexIndexData><TextureIndex>{id}</>\r\n").bytes());
                b.extend(format!("<AlphaMap><Width>{}</>\r\n", Terrain::PLANE_SIDE).bytes());
                cdata(&mut b, plane);
                b.extend(b"</>\r\n</>\r\n");
            }
            b.extend(b"</>\r\n");
        }
        b.extend(b"</>\r\n</Terrain>\r\n");
        b
    }

    // A grid of `side`² values over the 16 m chunk at chunk position
    // (cx, cz), from a function of island metres.
    fn grid<T>(side: usize, cx: usize, cz: usize, f: impl Fn(f32, f32) -> T) -> Vec<T> {
        let step = Terrain::CHUNK as f32 / (side - 1) as f32;
        (0..side * side)
            .map(|i| {
                f(
                    (cx * Terrain::CHUNK) as f32 + (i % side) as f32 * step,
                    (cz * Terrain::CHUNK) as f32 + (i / side) as f32 * step,
                )
            })
            .collect()
    }

    // 2×2 chunks. Heights are x + 100 z in island metres, so every value is
    // known from its position: (0,0) stores them at 1 m, (1,0) at 0.5 m,
    // (0,1) is empty ocean, (1,1) at 0.25 m. Paint: the base, with one
    // vertex under 255; layer 18 fading to nothing at x = 16, where (1,0)
    // lacks it; layer 52 rising from x = 16, where (0,0) lacks it.
    fn sample() -> Vec<TestChunk> {
        let height = |x: f32, z: f32| x + 100. * z;
        let base = |x: f32, z: f32| if (x, z) == (2., 3.) { 200 } else { 255 };
        let earth = |x: f32, _: f32| (16. - x) as u8 * 10;
        let grass = |x: f32, _: f32| (x - 16.) as u8 * 7;
        vec![
            TestChunk {
                heights: Some((Layout::METRE, grid(Layout::METRE.side(), 0, 0, height))),
                flags: 0x20001,
                layers: vec![
                    (0, grid(Terrain::PLANE_SIDE, 0, 0, base)),
                    (18, grid(Terrain::PLANE_SIDE, 0, 0, earth)),
                ],
            },
            TestChunk {
                heights: Some((
                    Layout::HALF_METRE,
                    grid(Layout::HALF_METRE.side(), 1, 0, height),
                )),
                flags: 0x20000,
                layers: vec![
                    (0, grid(Terrain::PLANE_SIDE, 1, 0, base)),
                    (52, grid(Terrain::PLANE_SIDE, 1, 0, grass)),
                ],
            },
            TestChunk {
                heights: None,
                flags: 0x10000,
                layers: Vec::new(),
            },
            TestChunk {
                heights: Some((
                    Layout::QUARTER_METRE,
                    grid(Layout::QUARTER_METRE.side(), 1, 1, height),
                )),
                flags: 0x30000,
                layers: vec![
                    (0, grid(Terrain::PLANE_SIDE, 1, 1, base)),
                    (52, grid(Terrain::PLANE_SIDE, 1, 1, grass)),
                ],
            },
        ]
    }

    #[test]
    fn a_hand_written_island_reads_as_written() {
        let bytes = island(2, 2, &sample());
        let doc = Doc::parse(&bytes).unwrap();
        assert_eq!(doc.encode(), bytes);
        let t = Terrain::read(&doc, "isle.isd").unwrap();
        assert_eq!(t.size(), (2, 2));
        assert_eq!(
            [0, 1, 2, 3].map(|k| t.chunks[k].flags),
            [131073, 131072, 65536, 196608]
        );

        // Native heights: 1 m, 0.5 m and 0.25 m samples, and none.
        let h = t.heights(0).unwrap();
        assert_eq!(
            (h.form, h.samples.len()),
            (Layout::METRE, Layout::METRE.bytes())
        );
        assert_eq!([h.at(0, 0), h.at(3, 5), h.at(16, 16)], [0., 503., 1616.]);
        let h = t.heights(1).unwrap();
        assert_eq!(h.form, Layout::HALF_METRE);
        // Sample (5, 7) of 33 sits at x = 16 + 2.5, z = 3.5.
        assert_eq!([h.at(0, 0), h.at(5, 7), h.at(32, 32)], [16., 368.5, 1632.]);
        assert!(t.heights(2).is_none());
        let h = t.heights(3).unwrap();
        assert_eq!(h.form, Layout::QUARTER_METRE);
        // Sample (1, 2) of 65 sits at x = 16.25, z = 16.5.
        assert_eq!([h.at(1, 2), h.at(64, 64)], [1666.25, 3232.]);

        // Layers as stored: ascending ids, raw planes, a base under 255.
        assert_eq!(t.ids(0), [0, 18]);
        assert_eq!(t.ids(1), [0, 52]);
        assert!(t.ids(2).is_empty());
        assert_eq!(t.ids(3), [0, 52]);
        let base = t.plane(0, 0);
        assert_eq!(base[3 * Terrain::PLANE_SIDE + 2], 200);
        assert_eq!(base.iter().filter(|&&a| a == 255).count(), 288);
        let earth = t.plane(0, 1);
        assert_eq!([earth[0], earth[4], earth[16]], [160, 120, 0]);
        assert_eq!(earth[16 * Terrain::PLANE_SIDE + 4], 120);
        let grass = t.plane(3, 1);
        assert_eq!([grass[0], grass[5], grass[16]], [0, 35, 112]);
    }

    #[test]
    fn corners_hold_whole_metre_samples_and_none_in_empty_ocean() {
        let doc = Doc::parse(&island(2, 2, &sample())).unwrap();
        let corners = Terrain::read(&doc, "isle.isd").unwrap().corners();
        assert_eq!(corners.size(), (33, 33));
        let at = |x, z| corners.get(x, z);
        // Inside the 1 m, 0.5 m and 0.25 m chunks: x + 100 z.
        assert_eq!(
            [at(3, 5), at(21, 7), at(20, 20)].map(Option::unwrap),
            [503., 721., 2020.]
        );
        assert_eq!([at(0, 0), at(32, 32)].map(Option::unwrap), [0., 3232.]);
        // On the edge the 1 m and 0.5 m chunks share, and where all four
        // chunks meet.
        assert_eq!([at(16, 5), at(16, 16)].map(Option::unwrap), [516., 1616.]);
        // The empty chunk's edges come from its neighbours; inside it and
        // on the island's rim there is nothing.
        assert_eq!([at(5, 16), at(16, 20)].map(Option::unwrap), [1605., 2016.]);
        assert_eq!([at(5, 20), at(0, 17), at(15, 32)], [None; 3]);
        let known = (0..33 * 33).filter(|i| at(i % 33, i / 33).is_some());
        assert_eq!(known.count(), 33 * 33 - 16 * 16);
    }

    fn error(chunks: &[TestChunk]) -> String {
        let doc = Doc::parse(&island(2, 2, chunks)).unwrap();
        match Terrain::read(&doc, "isle.isd") {
            Ok(_) => panic!("read a broken island"),
            Err(e) => e.to_string(),
        }
    }

    #[test]
    fn a_broken_island_fails_with_its_file_and_chunk() {
        // Layers out of order, repeated, or not starting with the base.
        let mut c = sample();
        c[3].layers.swap(0, 1);
        assert!(error(&c).starts_with("isle.isd: chunk 1,1: layers start with 52"));
        let mut c = sample();
        let earth = c[0].layers[1].clone();
        c[0].layers.push((7, earth.1.clone()));
        assert_eq!(
            error(&c),
            "isle.isd: chunk 0,0: layer 7 after 18: not ascending"
        );
        c[0].layers[2].0 = 18;
        assert_eq!(
            error(&c),
            "isle.isd: chunk 0,0: layer 18 after 18: not ascending"
        );

        // More layers than a chunk holds.
        let mut c = sample();
        let plane = vec![0; Terrain::PLANE];
        c[2].layers = (0..25).map(|id| (id, plane.clone())).collect();
        assert_eq!(error(&c), "isle.isd: chunk 0,1: more than 24 layers");

        // Short planes and short heights.
        let mut c = sample();
        c[1].layers[1].1.pop();
        assert_eq!(
            error(&c),
            "isle.isd: chunk 1,0: layer 52: plane 17 wide with 288 bytes, not 17 and 289"
        );
        let mut c = sample();
        c[3].heights.as_mut().unwrap().1.pop();
        assert_eq!(
            error(&c),
            "isle.isd: chunk 1,1: heights hold 16896 bytes, 65×65 samples need 16900"
        );
        let mut c = sample();
        c[0].heights.as_mut().unwrap().0 = Layout::HALF_METRE;
        assert!(error(&c).starts_with("isle.isd: chunk 0,0: heights hold 1156 bytes"));

        // Neighbours that disagree along their edge: coverage of a layer
        // both hold, of one only the first holds, and a height.
        let mut c = sample();
        c[3].layers[1].1[5] = 36;
        assert_eq!(
            error(&c),
            "isle.isd: chunk 1,0, lower edge: layer 52 covers 35 at metre 5, the neighbour's 36"
        );
        let mut c = sample();
        c[0].layers[1].1[2 * Terrain::PLANE_SIDE + 16] = 9;
        assert_eq!(
            error(&c),
            "isle.isd: chunk 0,0, right edge: layer 18 covers 9 at metre 2, the neighbour's 0"
        );
        let mut c = sample();
        c[3].heights.as_mut().unwrap().1[4 * 3] = 1619.5;
        assert_eq!(
            error(&c),
            "isle.isd: chunk 1,0, lower edge: height 1619 at metre 3, the neighbour holds 1619.5"
        );

        // A chunk missing from the map.
        let c = sample();
        let doc = Doc::parse(&island(2, 2, &c[..3])).unwrap();
        let e = Terrain::read(&doc, "isle.isd").err().unwrap().to_string();
        assert_eq!(e, "isle.isd: 3 chunks in a 2×2 map");
    }

    // 3×2 chunks, 48 m by 32 m. Top row: two 1 m chunks and empty ocean.
    // Bottom row: a 1 m chunk, a 0.25 m chunk and empty ocean. Every chunk
    // with heights holds a base layer.
    fn coast(height: impl Fn(f32, f32) -> f32 + Copy, flags: [u32; 4]) -> Vec<TestChunk> {
        let land = |form: Layout, cx: usize, cz: usize, flags: u32| TestChunk {
            heights: Some((form, grid(form.side(), cx, cz, height))),
            flags,
            layers: vec![(0, vec![255; Terrain::PLANE])],
        };
        let ocean = TestChunk {
            heights: None,
            flags: 0x10000,
            layers: Vec::new(),
        };
        vec![
            land(Layout::METRE, 0, 0, flags[0]),
            land(Layout::METRE, 1, 0, flags[1]),
            ocean.clone(),
            land(Layout::METRE, 0, 1, flags[2]),
            land(Layout::QUARTER_METRE, 1, 1, flags[3]),
            ocean,
        ]
    }

    // The coast's gameplay heights, 48×32 cells: 716 on the 0.7 m plateau
    // (z < 16), the drop's -20121, the gameplay floor under the -40 m deep,
    // and -4096 in the empty ocean, where no rule applies. Then `cells`.
    fn coast_gameplay(cells: &[(usize, usize, i16)]) -> Vec<i16> {
        let mut grid: Vec<i16> = (0..48 * 32)
            .map(|i| match (i % 48, i / 48) {
                (x, _) if x >= 32 => -4096,
                (_, z) if z < 16 => 716,
                (_, 16) => -20121,
                _ => grids::Layout::FLOOR,
            })
            .collect();
        for &(x, z, v) in cells {
            grid[z * 48 + x] = v;
        }
        grid
    }

    // The coast as loaded: a 0.7 m plateau with one corner at 0.2 m, and a
    // drop to -40 m below z = 16.
    fn coast_height(x: f32, z: f32) -> f32 {
        if (x, z) == (24., 5.) {
            0.2
        } else if z <= 16. {
            0.7
        } else {
            -40.
        }
    }

    // Flags as loaded. Chunk 0,0 has bit 0 and all heights above 0.25 m;
    // 1,0 has neither; 0,1 has the bit without the heights for it, as 1,290
    // shipped chunks disagree with the predicate.
    const COAST_FLAGS: [u32; 4] = [0x20001, 0x30000, 0x20001, 0x30000];

    fn coast_loaded() -> Vec<u8> {
        // Cells off the rule as loaded: (20, 3) far from the sculpt, (4, 4)
        // and (3, 23) under it. The cells around the 0.2 m corner follow it.
        let cells = [
            (20, 3, 999),
            (4, 4, 1000),
            (3, 23, -30000),
            (23, 4, 588),
            (24, 4, 588),
            (23, 5, 588),
            (24, 5, 588),
        ];
        island_with(
            3,
            2,
            &coast(coast_height, COAST_FLAGS),
            Some((Version::V2, &coast_gameplay(&cells))),
        )
    }

    // Every expected value is spelled out and the whole document compared,
    // so anything the sculpt should not reach is checked byte for byte.
    #[test]
    fn a_sculpt_rewrites_only_its_chunks_and_derived_cells() {
        let before = coast_loaded();
        let mut doc = Doc::parse(&before).unwrap();
        let mut corners = Terrain::read(&doc, "isle.isd").unwrap().corners();
        // Nothing moved, nothing written.
        corners.write(&mut doc, "isle.isd", SCULPT).unwrap();
        assert!(doc.encode() == before);

        corners.set(5, 5, 1.7); // inside chunk 0,0
        corners.set(2, 2, 0.1); // takes 0,0 under 0.25 m
        corners.set(16, 8, 1.2); // on the edge of 0,0 and 1,0
        corners.set(24, 5, 0.7); // lifts 1,0 over 0.25 m everywhere
        corners.set(32, 4, 1.0); // on the edge of 1,0 and empty ocean
        corners.set(4, 24, -39.5); // in the deep of 0,1
        corners.set(8, 8, 0.7); // as it was: not a move
        corners.write(&mut doc, "isle.isd", SCULPT).unwrap();

        let moves = [
            (5., 5., 1.7),
            (2., 2., 0.1),
            (16., 8., 1.2),
            (24., 5., 0.7),
            (32., 4., 1.0),
            (4., 24., -39.5),
        ];
        let sculpted = |x: f32, z: f32| {
            let moved = moves.iter().find(|m| (m.0, m.1) == (x, z));
            moved.map_or(coast_height(x, z), |m| m.2)
        };
        // Bit 0 follows 0,0 down and 1,0 up. 0,1 keeps its odd flag: its
        // heights were not all above 0.25 m before or after.
        let flags = [0x20000, 0x30001, 0x20001, 0x30000];
        // The mean of a cell's corners in 1/1024 m, truncated:
        // (3 × 0.7 + 1.7) / 4 = 0.95 m is 972, and so on.
        let cells = [
            (20, 3, 999),
            (4, 4, 972),
            (5, 4, 972),
            (4, 5, 972),
            (5, 5, 972),
            (1, 1, 563),
            (2, 1, 563),
            (1, 2, 563),
            (2, 2, 563),
            (15, 7, 844),
            (16, 7, 844),
            (15, 8, 844),
            (16, 8, 844),
            // Left of the moved edge corner. The cells to its right reach
            // into empty ocean and keep their -4096.
            (31, 3, 793),
            (31, 4, 793),
        ];
        let after = island_with(
            3,
            2,
            &coast(sculpted, flags),
            Some((Version::V2, &coast_gameplay(&cells))),
        );
        assert!(doc.encode() == after);

        // The rule on its own: -39.875 m is below the floor.
        assert_eq!(
            [(4, 4), (1, 1), (15, 8), (31, 3), (3, 23), (20, 3)]
                .map(|(x, z)| corners.gameplay_height(x, z)),
            [972, 563, 844, 793, grids::Layout::FLOOR, 716].map(Some)
        );
        assert_eq!(corners.gameplay_height(32, 4), None);
    }

    // 3×1 chunks stored at 1 m, 0.5 m and 0.25 m. Heights are x + 100 z,
    // plus 0.5 m on every sample between whole metres: relief only the fine
    // chunks hold, which a sculpt must keep.
    fn relief(x: f32, z: f32) -> f32 {
        let between = x.fract() != 0. || z.fract() != 0.;
        x + 100. * z + if between { 0.5 } else { 0. }
    }

    fn strip(height: impl Fn(f32, f32) -> f32 + Copy, flags: [u32; 3]) -> Vec<TestChunk> {
        (0..3)
            .map(|cx| {
                let form = [Layout::METRE, Layout::HALF_METRE, Layout::QUARTER_METRE][cx];
                TestChunk {
                    heights: Some((form, grid(form.side(), cx, 0, height))),
                    flags: flags[cx],
                    layers: vec![(0, vec![255; Terrain::PLANE])],
                }
            })
            .collect()
    }

    // Four corners moved, each in or on the edge of a fine chunk, with
    // footprints apart. The whole document is spelled out from formulas:
    // every sample keeps its relief and adds the move times the tent
    // max(0, 1 - |dx|) × max(0, 1 - |dz|) around the moved corner.
    #[test]
    fn a_sculpt_keeps_fine_samples_and_adds_its_change_around_each_corner() {
        let flags = [0x20000, 0x20001, 0x20001];
        let before = island_with(
            3,
            1,
            &strip(relief, flags),
            Some((Version::V2, &[0; 48 * 16])),
        );
        let mut doc = Doc::parse(&before).unwrap();
        let mut corners = Terrain::read(&doc, "isle.isd").unwrap().corners();
        // Whole-metre samples of the fine chunks are corners.
        assert_eq!(corners.get(20, 4), Some(420.));
        assert_eq!(corners.get(40, 8), Some(840.));
        corners.write(&mut doc, "isle.isd", SCULPT).unwrap();
        assert!(doc.encode() == before);

        let moves = [
            (20., 4., 2.),    // inside the 0.5 m chunk
            (16., 8., 3.),    // on the edge of the 1 m and 0.5 m chunks
            (32., 12., -1.),  // on the edge of the 0.5 m and 0.25 m chunks
            (40., 8., -850.), // inside the 0.25 m chunk, taking it under 0.25 m
        ];
        for &(x, z, by) in &moves {
            let (x, z) = (x as usize, z as usize);
            corners.set(x, z, corners.get(x, z).unwrap() + by);
        }
        corners.write(&mut doc, "isle.isd", SCULPT).unwrap();

        let sculpted = |x: f32, z: f32| {
            let tent = |d: f32| (1. - d.abs()).max(0.);
            let lift: f32 = (moves.iter())
                .map(|&(mx, mz, by)| by * tent(x - mx) * tent(z - mz))
                .sum();
            relief(x, z) + lift
        };
        // The cells around each moved corner by the gameplay rule, from the
        // sculpted corners; the rest stay 0 as loaded.
        let mut cells = [0i16; 48 * 16];
        for &(mx, mz, _) in &moves {
            let (mx, mz) = (mx as usize, mz as usize);
            for (x, z) in [(mx - 1, mz - 1), (mx, mz - 1), (mx - 1, mz), (mx, mz)] {
                let corner = |dx: usize, dz: usize| sculpted((x + dx) as f32, (z + dz) as f32);
                let sum = corner(0, 0) + corner(1, 0) + corner(0, 1) + corner(1, 1);
                cells[z * 48 + x] = (sum as f64 / 4. * grids::Layout::UNITS)
                    .trunc()
                    .max(grids::Layout::FLOOR as f64) as i16;
            }
        }
        // The 0.25 m chunk dips to -10 m, so bit 0 goes.
        let after = island_with(
            3,
            1,
            &strip(sculpted, [0x20000, 0x20001, 0x20000]),
            Some((Version::V2, &cells)),
        );
        assert!(doc.encode() == after);
        // Spot checks: the relief between corners stays, plus the tent.
        let t = Terrain::read(&doc, "isle.isd").unwrap();
        let half = t.heights(1).unwrap();
        assert_eq!(
            [half.at(8, 8), half.at(9, 8), half.at(9, 9), half.at(10, 9)],
            [422., 422., 471.5, 471.5]
        );
        let quarter = t.heights(2).unwrap();
        assert_eq!([quarter.at(32, 32), quarter.at(33, 33)], [-10., 387.625]);
        assert_eq!(quarter.at(0, 47), relief(32., 11.75) - 0.75);
    }

    // A bit grid in the grid manager's layout, `on` saying which cells are
    // set, with `head` before it (the submarine layer's level and radius).
    fn bitgrid(
        tag: &str,
        head: &str,
        cols: usize,
        rows: usize,
        on: impl Fn(usize, usize) -> bool,
    ) -> Vec<u8> {
        let ints = cols.div_ceil(32);
        let mut words = vec![0u32; ints * rows];
        for z in 0..rows {
            for x in 0..cols {
                words[z * ints + x / 32] |= (on(x, z) as u32) << (x % 32);
            }
        }
        let mut b = format!(
            "<{tag}>{head}<BitGrid><m_XSize>{cols}</m_XSize>\r\n<m_YSize>{rows}</m_YSize>\r\n\
             <m_IntXSize>{ints}</m_IntXSize>\r\n<m_BitGrid>CDATA["
        )
        .into_bytes();
        b.extend(((words.len() * size_of::<u32>()) as u32).to_le_bytes());
        b.extend(words.iter().flat_map(|w| w.to_le_bytes()));
        b.extend(format!("]</m_BitGrid>\r\n</BitGrid>\r\n</{tag}>\r\n").bytes());
        b
    }

    // The coast's three grids. Path blockers at 0.5 m: an authored block in
    // the corner, plus `path`. High ground: one authored bit. Submarine
    // blocker at 1 m: `sub`.
    fn coast_grids(
        path: impl Fn(usize, usize) -> bool,
        sub: impl Fn(usize, usize) -> bool,
    ) -> Vec<u8> {
        let mut b = bitgrid("m_PathBlockerLayer", "", 96, 64, |x, z| {
            (x < 6 && z < 6) || path(x / 2, z / 2)
        });
        b.extend(bitgrid("m_HeightPathBlockerLayer", "", 96, 64, |x, z| {
            (x, z) == (50, 10)
        }));
        let head = "<SubmarineLevelHeight>-98304</SubmarineLevelHeight>\r\n\
                    <SubmarineIslandBlockerRadius>16384</SubmarineIslandBlockerRadius>\r\n";
        b.extend(bitgrid("m_SubmarineBlockerLayer", head, 48, 32, sub));
        b
    }

    // The coast's submarine blocker by the rule: cells within 4 m of the
    // plateau and the drop (corner mean at or above -24 m: x < 32, z <= 16),
    // and three exceptions as shipped grids have them.
    fn coast_sub(x: usize, z: usize) -> bool {
        match (x, z) {
            (40, 30) => true,
            (2, 2) | (21, 6) => false,
            _ => x <= 35 && z <= 20,
        }
    }

    // Each rule against a document spelled out from the sculpted corners:
    // - path blockers gain the cells lifted from the band to above it
    //   (972 from 716; not (4, 4), stored at 1000 already above, nor the
    //   cells lifted into the band), and only when the edit asks
    // - the high-ground layer stays
    // - the submarine blocker is rebuilt within 4 m of the one cell whose
    //   side of -24 m changed, and carries elsewhere: its three exceptions,
    //   and the cells around a corner lowered without crossing
    #[test]
    fn a_sculpt_blocks_lifted_cells_and_rebuilds_the_submarine_blocker_around_it() {
        let loaded_cells = [
            (20, 3, 999),
            (4, 4, 1000),
            (3, 23, -30000),
            (23, 4, 588),
            (24, 4, 588),
            (23, 5, 588),
            (24, 5, 588),
        ];
        let flags = COAST_FLAGS;
        let none = |_: usize, _: usize| false;
        let before = island_grids(
            3,
            2,
            &coast(coast_height, flags),
            Some((Version::V2, &coast_gameplay(&loaded_cells))),
            &coast_grids(none, coast_sub),
        );
        let moves = [
            (5., 5., 1.7),   // lifts three cells over the band
            (24., 5., 0.7),  // lifts four into the band
            (20., 5., -30.), // lowers four, still above -24 m
            (8., 26., -10.), // with the next three, raises the cell
            (9., 26., -10.), // (8, 26) over -24 m
            (8., 27., -10.),
            (9., 27., -10.),
        ];
        let sculpted = |x: f32, z: f32| {
            let moved = moves.iter().find(|m| (m.0, m.1) == (x, z));
            moved.map_or(coast_height(x, z), |m| m.2)
        };
        let cells: Vec<i16> = {
            let loaded = coast_gameplay(&loaded_cells);
            (0..48 * 32)
                .map(|n| {
                    let (x, z) = ((n % 48) as f32, (n / 48) as f32);
                    let near = moves
                        .iter()
                        .any(|m| (m.0 - 1.0..=m.0).contains(&x) && (m.1 - 1.0..=m.1).contains(&z));
                    if !near {
                        return loaded[n];
                    }
                    let corner = |dx: f32, dz: f32| sculpted(x + dx, z + dz) as f64;
                    let mean =
                        (corner(0., 0.) + corner(1., 0.) + corner(0., 1.) + corner(1., 1.)) / 4.;
                    (mean * grids::Layout::UNITS)
                        .trunc()
                        .max(grids::Layout::FLOOR as f64) as i16
                })
                .collect()
        };
        assert_eq!(
            [cells[4 * 48 + 5], cells[4 * 48 + 23], cells[26 * 48 + 8]],
            [972, 716, -10240]
        );
        let lifted = |x: usize, z: usize| matches!((x, z), (5, 4) | (4, 5) | (5, 5));
        let rebuilt =
            |x: usize, z: usize| (4..=12).contains(&x) && (22..=30).contains(&z) || coast_sub(x, z);
        for (sculpt, path) in [
            (SCULPT, true),
            (
                Sculpt {
                    block_lifted_above: None,
                    ..SCULPT
                },
                false,
            ),
        ] {
            let mut doc = Doc::parse(&before).unwrap();
            let mut corners = Terrain::read(&doc, "isle.isd").unwrap().corners();
            corners.write(&mut doc, "isle.isd", sculpt).unwrap();
            assert!(doc.encode() == before);
            for &(x, z, h) in &moves {
                corners.set(x as usize, z as usize, h);
            }
            corners.write(&mut doc, "isle.isd", sculpt).unwrap();
            let after = island_grids(
                3,
                2,
                &coast(sculpted, flags),
                Some((Version::V2, &cells)),
                &coast_grids(|x, z| path && lifted(x, z), rebuilt),
            );
            assert!(doc.encode() == after, "{sculpt:?}");
        }
    }

    // v1 gameplay heights off every rule carry while the island is not
    // sculpted. The first sculpt writes v2 in their place, every cell by
    // the rule from the sculpted corners, and the floor where a cell
    // reaches into empty ocean.
    #[test]
    fn a_v1_island_takes_v2_gameplay_heights_on_its_first_sculpt() {
        let chunks = coast(coast_height, COAST_FLAGS);
        let v1 = island_with(3, 2, &chunks, Some((Version::V1, &[7; 48 * 32])));
        let mut doc = Doc::parse(&v1).unwrap();
        let terrain = Terrain::read(&doc, "isle.isd").unwrap();
        let (mut corners, mut layers) = (terrain.corners(), terrain.layers());
        corners.write(&mut doc, "isle.isd", SCULPT).unwrap();
        // Paint is not a sculpt.
        let mut ids = layers.ids(0).to_vec();
        let mut planes: Vec<u8> = (0..ids.len()).flat_map(|l| *layers.plane(0, l)).collect();
        ids.push(18);
        planes.extend(plane(0, &[(5, 5, 255)]));
        layers.set(0, &ids, &planes).unwrap();
        layers.write(&mut doc, "isle.isd", SCULPT).unwrap();
        let painted = doc.encode();
        assert!(painted != v1);
        let v1 = format!("<{}>", Version::V1.layout().tag);
        assert!(painted.windows(v1.len()).any(|w| w == v1.as_bytes()));

        corners.set(5, 5, 1.7);
        corners.write(&mut doc, "isle.isd", SCULPT).unwrap();
        let sculpted = |x: f32, z: f32| {
            if (x, z) == (5., 5.) {
                1.7
            } else {
                coast_height(x, z)
            }
        };
        let cells: Vec<i16> = (0..48 * 32)
            .map(|i| {
                let (x, z) = ((i % 48) as f32, (i / 48) as f32);
                if x >= 32. {
                    return grids::Layout::FLOOR;
                }
                let sum: f64 = [(0., 0.), (1., 0.), (0., 1.), (1., 1.)]
                    .iter()
                    .map(|&(dx, dz)| sculpted(x + dx, z + dz) as f64)
                    .sum();
                (sum / 4. * grids::Layout::UNITS)
                    .trunc()
                    .max(grids::Layout::FLOOR as f64) as i16
            })
            .collect();
        // Spot checks: the plateau, the sculpted cell, the drop, the deep.
        assert_eq!(
            [cells[0], cells[4 * 48 + 4], cells[16 * 48], cells[20 * 48]],
            [716, 972, -20121, grids::Layout::FLOOR]
        );
        let mut after = coast(sculpted, COAST_FLAGS);
        after[0].layers.push((18, plane(0, &[(5, 5, 255)])));
        assert!(doc.encode() == island_with(3, 2, &after, Some((Version::V2, &cells))));
    }

    #[test]
    fn a_sculpt_without_its_grids_or_of_another_size_fails() {
        let chunks = coast(coast_height, COAST_FLAGS);
        let mut doc = Doc::parse(&island(3, 2, &chunks)).unwrap();
        let mut corners = Terrain::read(&doc, "isle.isd").unwrap().corners();
        corners.set(5, 5, 1.7);
        let e = corners.write(&mut doc, "isle.isd", SCULPT).err().unwrap();
        assert_eq!(e.to_string(), "isle.isd: no <m_GRIDManager>");

        let other = Doc::parse(&island(2, 2, &sample())).unwrap();
        let corners = Terrain::read(&other, "other.isd").unwrap().corners();
        assert_eq!(
            corners
                .write(&mut doc, "isle.isd", SCULPT)
                .err()
                .unwrap()
                .to_string(),
            "isle.isd: 33×33 corners for an island of 49×33"
        );
    }

    // A plane of `fill` with single vertices set, at chunk-local (x, z).
    fn plane(fill: u8, vertices: &[(usize, usize, u8)]) -> Vec<u8> {
        let mut plane = vec![fill; Terrain::PLANE];
        for &(x, z, v) in vertices {
            plane[z * Terrain::PLANE_SIDE + x] = v;
        }
        plane
    }

    fn painted(layers: Vec<(u32, Vec<u8>)>) -> TestChunk {
        TestChunk {
            heights: Some((Layout::METRE, vec![0.7; Terrain::PLANE])),
            flags: 0x20001,
            layers,
        }
    }

    // Paint set on a chunk writes changed planes in place and new layers
    // as new `TexIndexData` before the first higher id; a second write has
    // nothing left to do.
    #[test]
    fn set_layers_write_changed_planes_in_place_and_new_ones_in_id_order() {
        let before = island(
            1,
            1,
            &[painted(vec![
                (0, plane(255, &[(3, 4, 200)])),
                (18, plane(60, &[])),
                (52, plane(100, &[])),
                (80, plane(200, &[])),
                (94, plane(77, &[])),
            ])],
        );
        let mut doc = Doc::parse(&before).unwrap();
        let mut layers = Terrain::read(&doc, "isle.isd").unwrap().layers();
        layers.write(&mut doc, "isle.isd", SCULPT).unwrap();
        assert!(doc.encode() == before);

        let now = vec![
            (0, plane(255, &[(3, 4, 200)])),
            (18, plane(60, &[(3, 4, 30), (9, 9, 0), (1, 1, 0)])),
            (52, plane(100, &[(9, 9, 0)])),
            (72, plane(0, &[(3, 4, 128), (1, 1, 255)])),
            (80, plane(200, &[(3, 4, 100), (9, 9, 0), (1, 1, 0)])),
            (94, plane(77, &[(12, 2, 166)])),
        ];
        let ids: Vec<u32> = now.iter().map(|(id, _)| *id).collect();
        let planes: Vec<u8> = now.iter().flat_map(|(_, p)| p.clone()).collect();
        layers.set(0, &ids, &planes).unwrap();
        layers.write(&mut doc, "isle.isd", SCULPT).unwrap();
        let after = island(1, 1, &[painted(now)]);
        assert!(doc.encode() == after);
        layers.write(&mut doc, "isle.isd", SCULPT).unwrap();
        assert!(doc.encode() == after);
        // Ids that don't ascend from the base, or planes of another count.
        assert!(
            layers
                .set(0, &[18, 0], &planes[..2 * Terrain::PLANE])
                .is_err()
        );
        assert!(layers.set(0, &[0, 18], &planes[..Terrain::PLANE]).is_err());
    }

    #[test]
    fn paint_of_another_island_is_refused_and_paint_it_lacks_is_removed() {
        let before = coast_loaded();
        let mut doc = Doc::parse(&before).unwrap();
        let mut layers = Terrain::read(&doc, "isle.isd").unwrap().layers();
        // On the edge of a painted chunk and empty ocean, the painted one
        // takes earth_01 there.
        let mut planes = layers.plane(1, 0).to_vec();
        planes.extend(plane(0, &[(16, 8, 255)]));
        layers.set(1, &[0, 18], &planes).unwrap();
        assert_eq!([0, 1, 2].map(|k| layers.ids(k).len()), [1, 2, 0]);
        layers.write(&mut doc, "isle.isd", SCULPT).unwrap();
        let after = Terrain::read(&doc, "isle.isd").unwrap();
        assert_eq!(after.ids(1), [0, 18]);
        assert_eq!(after.plane(1, 1)[8 * Terrain::PLANE_SIDE + 16], 255);
        assert_eq!(after.plane(1, 1).iter().filter(|&&a| a > 0).count(), 1);

        let mut other = Doc::parse(&island(2, 2, &sample())).unwrap();
        assert_eq!(
            layers
                .write(&mut other, "other.isd", SCULPT)
                .err()
                .unwrap()
                .to_string(),
            "other.isd: paint for 3×2 chunks on an island of 2×2"
        );
        // Paint without the layer takes it out again.
        let loaded = Terrain::read(&Doc::parse(&before).unwrap(), "isle.isd")
            .unwrap()
            .layers();
        loaded.write(&mut doc, "isle.isd", SCULPT).unwrap();
        assert!(doc.encode() == before);
    }

    // 2×1 chunks: a shore at 1 m beside empty ocean. Heights fall toward
    // the ocean, -30 + x/2 + z/4. The base is opaque but for 200 at the
    // edge vertex (16, 5); earth_01 (18) covers 10 z along that edge and
    // 50 inside; gras_01 (52) only inside, so it stops short of the edge.
    fn shore_height(x: f32, z: f32) -> f32 {
        -30. + x / 2. + z / 4.
    }

    fn shore(ocean: TestChunk) -> Vec<TestChunk> {
        let base = grid(Terrain::PLANE_SIDE, 0, 0, |x, z| {
            if (x, z) == (16., 5.) { 200 } else { 255 }
        });
        let earth = grid(Terrain::PLANE_SIDE, 0, 0, |x, z| {
            if x == 16. { z as u8 * 10 } else { 50 }
        });
        let grass = grid(
            Terrain::PLANE_SIDE,
            0,
            0,
            |x, _| if x == 16. { 0 } else { 30 },
        );
        vec![
            TestChunk {
                heights: Some((
                    Layout::METRE,
                    grid(Layout::METRE.side(), 0, 0, shore_height),
                )),
                flags: 0x20000,
                layers: vec![(0, base), (18, earth), (52, grass)],
            },
            ocean,
        ]
    }

    fn shore_island(ocean: TestChunk, cells: &[i16]) -> Vec<u8> {
        island_with(2, 1, &shore(ocean), Some((Version::V2, cells)))
    }

    const OCEAN: TestChunk = TestChunk {
        heights: None,
        flags: 0x10000,
        layers: Vec::new(),
    };

    // The ocean chunk once it has records: heights from the shore's edge,
    // then `height`; the base and earth's coverage along the shared edge,
    // and `more` layers.
    fn created(height: impl Fn(f32, f32) -> f32, more: Vec<(u32, Vec<u8>)>) -> TestChunk {
        let heights = grid(Terrain::PLANE_SIDE, 1, 0, |x, z| {
            if x == 16. {
                shore_height(x, z)
            } else {
                height(x, z)
            }
        });
        let base = grid(Terrain::PLANE_SIDE, 1, 0, |x, z| {
            if (x, z) == (16., 5.) { 200 } else { 255 }
        });
        let earth = grid(Terrain::PLANE_SIDE, 1, 0, |x, z| {
            if x == 16. { z as u8 * 10 } else { 0 }
        });
        let mut layers = vec![(0, base), (18, earth)];
        layers.extend(more);
        TestChunk {
            heights: Some((Layout::METRE, heights)),
            flags: CREATED_FLAGS,
            layers,
        }
    }

    // Gameplay cells: 1000 on the shore as loaded, the floor in the ocean,
    // and the rule over the ocean chunk's cells (x 16 to 31) once it has
    // heights.
    fn shore_cells(heights: Option<&dyn Fn(f32, f32) -> f32>) -> Vec<i16> {
        (0..32 * 16)
            .map(|i| {
                let (x, z) = (i % 32, i / 32);
                match heights {
                    Some(f) if x >= 16 => {
                        let corner =
                            |dx: usize, dz: usize| f((x + dx) as f32, (z + dz) as f32) as f64;
                        let mean = (corner(0, 0) + corner(1, 0) + corner(0, 1) + corner(1, 1)) / 4.;
                        (mean * grids::Layout::UNITS)
                            .trunc()
                            .max(grids::Layout::FLOOR as f64) as i16
                    }
                    _ if x < 16 => 1000,
                    _ => grids::Layout::FLOOR,
                }
            })
            .collect()
    }

    #[test]
    fn a_first_sculpt_in_empty_ocean_creates_the_chunk() {
        let before = shore_island(OCEAN, &shore_cells(None));
        let mut doc = Doc::parse(&before).unwrap();
        let mut corners = Terrain::read(&doc, "isle.isd").unwrap().corners();
        // The shared edge is the shore's; inside the ocean nothing.
        assert_eq!(corners.get(16, 5), Some(shore_height(16., 5.)));
        assert_eq!(corners.get(24, 8), None);
        // The chunk's other corners take the seabed when it is written.
        corners.set(24, 8, -10.);
        assert_eq!([corners.get(24, 8), corners.get(25, 8)], [Some(-10.), None]);
        corners.write(&mut doc, "isle.isd", SCULPT).unwrap();

        let height = |x: f32, z: f32| match (x, z) {
            (24., 8.) => -10.,
            (x, z) if x == 16. => shore_height(x, z),
            _ => SCULPT.seabed,
        };
        let after = shore_island(created(height, Vec::new()), &shore_cells(Some(&height)));
        assert!(doc.encode() == after);
    }

    // Paint set on an empty ocean chunk gives it heights too: its
    // neighbours' along the shared edge, the seabed inside.
    #[test]
    fn paint_in_empty_ocean_creates_the_chunk() {
        let before = shore_island(OCEAN, &shore_cells(None));
        let mut doc = Doc::parse(&before).unwrap();
        let mut layers = Terrain::read(&doc, "isle.isd").unwrap().layers();
        let height = |x: f32, z: f32| {
            if x == 16. {
                shore_height(x, z)
            } else {
                SCULPT.seabed
            }
        };
        let gras = plane(0, &[(8, 4, 255)]);
        let chunk = created(height, vec![(72, gras)]);
        let ids: Vec<u32> = chunk.layers.iter().map(|(id, _)| *id).collect();
        let planes: Vec<u8> = chunk.layers.iter().flat_map(|(_, p)| p.clone()).collect();
        layers.set(1, &ids, &planes).unwrap();
        layers.write(&mut doc, "isle.isd", SCULPT).unwrap();
        let after = shore_island(chunk, &shore_cells(Some(&height)));
        assert!(doc.encode() == after);
    }

    // A first sculpt in empty ocean paints the new chunk to agree with its
    // painted neighbours along its edges. Where those edges would hold more
    // than 24 layers the write fails; with one full neighbour the chunk
    // takes that one's 24.
    #[test]
    fn a_first_sculpt_in_empty_ocean_takes_its_neighbours_paint_or_fails() {
        let full = TestChunk {
            heights: Some((Layout::METRE, vec![0.7; Terrain::PLANE])),
            flags: 0x20000,
            layers: (0..24)
                .map(|id| (id, plane(if id == 0 { 255 } else { 9 }, &[])))
                .collect(),
        };
        let mut other = full.clone();
        for (id, _) in &mut other.layers[1..] {
            *id += 30;
        }
        let sculpt = |chunks: &[TestChunk]| {
            let cells = vec![grids::Layout::FLOOR; chunks.len() * Terrain::CHUNK * Terrain::CHUNK];
            let bytes = island_with(chunks.len(), 1, chunks, Some((Version::V2, &cells)));
            let mut doc = Doc::parse(&bytes).unwrap();
            let mut corners = Terrain::read(&doc, "isle.isd").unwrap().corners();
            corners.set(24, 8, -10.);
            corners.write(&mut doc, "isle.isd", SCULPT).map(|()| doc)
        };
        // Its edges would hold the 23 colours on its left and 23 others on
        // its right.
        let e = sculpt(&[full.clone(), OCEAN, other]).err().unwrap();
        assert_eq!(
            e.to_string(),
            "isle.isd: chunk 1,0: its neighbours hold more than 24 layers along its edges"
        );
        let doc = sculpt(&[full, OCEAN]).unwrap();
        let after = Terrain::read(&doc, "isle.isd").unwrap();
        assert_eq!(after.ids(1), (0..24).collect::<Vec<_>>());
    }

    #[test]
    fn the_registry_ranks_layers_by_splat_id() {
        // Three entries in the shipped file's layout: earth lists before
        // grass and has the smaller id, but ranks above it.
        let xml = r"<GroundTextures>
  <TextureScale>4</TextureScale>
  <Texture>
    <GUID>52</GUID>
    <SplatID>800</SplatID>
    <Name>gras_01</Name>
    <MeanColor>
      <R>255</R>
      <G>255</G>
      <B>255</B>
    </MeanColor>
    <DiffuseTexture>data\graphics\landscape\terrain\n_gras_01_diff.png</DiffuseTexture>
    <NormalTexture>data\graphics\landscape\terrain\n_gras_01_norm.png</NormalTexture>
    <MappingProfile>0</MappingProfile>
    <MapToGUID>52</MapToGUID>
    <ShallowCoast>0</ShallowCoast>
  </Texture>
  <Texture>
    <GUID>94</GUID>
    <SplatID>700</SplatID>
    <Name>&gt;&gt;&gt; GrassObjects01</Name>
    <MeanColor>
      <R>255</R>
      <G>255</G>
      <B>255</B>
    </MeanColor>
    <GrassMesh>data\graphics\landscape\grass\rdm\grass_01.rdm</GrassMesh>
    <GrassTexture>data\graphics\landscape\grass\maps\grass01_diff.png</GrassTexture>
    <ShallowCoast>0</ShallowCoast>
  </Texture>
  <Texture>
    <GUID>18</GUID>
    <SplatID>812</SplatID>
    <Name>earth_01</Name>
    <DiffuseTexture>data\graphics\landscape\terrain\n_earth_01_diff.png</DiffuseTexture>
    <ShallowCoast>0</ShallowCoast>
  </Texture>
</GroundTextures>
";
        let ground = GroundTextures::parse(xml).unwrap();
        assert_eq!(ground.scale(), 4.);
        let wide = xml.replace(
            "<TextureScale>4</TextureScale>",
            "<TextureScale>6.5</TextureScale>",
        );
        assert_eq!(GroundTextures::parse(&wide).unwrap().scale(), 6.5);
        let none = xml.replace("<TextureScale>4</TextureScale>", "");
        assert_eq!(GroundTextures::parse(&none).unwrap().scale(), 4.);
        assert_eq!(
            [18, 52, 94, 0, 19, 4096].map(|id| ground.rank(id)),
            [Some(812), Some(800), Some(700), None, None, None]
        );
        assert_eq!(
            [18, 52, 94, 0].map(|id| ground.scatter(id)),
            [false, false, true, false]
        );
        let [grass, scatter, earth] = ground.textures() else {
            panic!("three textures");
        };
        assert_eq!((grass.id, grass.rank, &*grass.name), (52, 800, "gras_01"));
        assert_eq!(
            grass.normal.as_deref(),
            Some(r"data\graphics\landscape\terrain\n_gras_01_norm.png")
        );
        assert_eq!(scatter.name, ">>> GrassObjects01");
        assert_eq!((&scatter.diffuse, &scatter.normal), (&None, &None));
        assert_eq!(
            scatter.grass.as_deref(),
            Some(r"data\graphics\landscape\grass\maps\grass01_diff.png")
        );
        assert_eq!((&grass.grass, &earth.grass), (&None, &None));
        assert_eq!((earth.id, earth.rank), (18, 812));

        // Earth pointed at the developers' stand-in is a placeholder; the
        // others, and an unknown id, aren't.
        assert_eq!([18, 52, 94, 0].map(|id| ground.placeholder(id)), [false; 4]);
        let dummy = xml.replace(
            r"n_earth_01_diff.png</DiffuseTexture>",
            r"wiki_dummy_diff.png</DiffuseTexture>",
        );
        let dummy = GroundTextures::parse(&dummy).unwrap();
        assert_eq!(
            [18, 52, 94, 0].map(|id| dummy.placeholder(id)),
            [true, false, false, false]
        );

        let twice = xml.replace("<GUID>94</GUID>", "<GUID>52</GUID>");
        assert!(GroundTextures::parse(&twice).is_err());
        assert!(GroundTextures::parse("<GroundTextures></GroundTextures>").is_err());
    }
}
