// Island terrain mesh tiles: `_gamedata/<island>/meshes/<level>/<x>x<y>.tmc`,
// read, written byte-exactly, and rebuilt from the heightfield.
//
// ADF `Meshes { width=32, height=32, map { 1024 × node } }`, nodes row-major
// with the row along z. Each node holds `Indices` (u16 triangle list),
// `Vertices` (16-byte records, see `Vertex`), `UsedMaterialBits` and an empty
// `GroundObjects`. A level-L node spans 2^L m; levels below rd3d
// `MinMeshLevel` (2) carry no geometry. Geometry exists exactly where rd3d
// `HasMeshArray` is set (the ActiveMap area): the mesh is the island's ground
// there, the coarse heightfield only draws the rest.

use std::collections::HashMap;

use anyhow::{Context, Result, bail, ensure};

use crate::a7m::island::Grid;
use crate::adf::{Adf, ItemId};

/// Bytes per stored vertex (`Vertex::read`).
const VERTEX: usize = 16;
/// Vertex x and z units per metre (rd3d `UnitScale`, 1/32 m).
const XZ_PER_METRE: f32 = 32.;
/// Vertex y units per metre and the y of 0 m: 1/256 m, biased to unsigned.
const Y_PER_METRE: f32 = crate::a7m::island::Island::HEIGHT_UNITS_PER_METRE;
const Y_ZERO: f32 = 32768.;

/// Skirt depth in node sizes: every node border edge gets a vertical strip
/// this far down so LOD and node seams never open a hole.
const SKIRT_DEPTH: f32 = 0.04;

/// One vertex as stored (`VSInputTerrain` in `shaders/include/terrain.fxh`):
/// `[u16 x, u16 y, u16 z, u16 u]`, then the normal and tangent as B8G8R8A8
/// whose alpha bytes carry the low and high byte of `v`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct Vertex {
    /// x and z in 1/32 m (rd3d `UnitScale`), y in 1/256 m biased by 32768.
    pub(crate) position: [u16; 3],
    /// Texcoord over the node's vertex bounding box, unorm16.
    uv: [u16; 2],
    /// Normal x, y, z as unorm8 (`n * 0.5 + 0.5`). The shader reads y < 0 as
    /// a skirt flag for tessellation; shipped files never set it.
    normal: [u8; 3],
    /// Tangent x, y, z, unorm8. Shipped: normalize(-ny, nx, 0).
    tangent: [u8; 3],
}

// The record, in byte order: x, y, z and u as u16, then normal and tangent
// as B8G8R8A8 with the low and high byte of v in their alpha. `read` and
// `write` spell it with the same pattern.
impl Vertex {
    fn read(b: &[u8; VERTEX]) -> Vertex {
        let [
            x0,
            x1,
            y0,
            y1,
            z0,
            z1,
            u0,
            u1,
            nz,
            ny,
            nx,
            v0,
            tz,
            ty,
            tx,
            v1,
        ] = *b;
        let u16s = |lo, hi| u16::from_le_bytes([lo, hi]);
        Vertex {
            position: [u16s(x0, x1), u16s(y0, y1), u16s(z0, z1)],
            uv: [u16s(u0, u1), u16s(v0, v1)],
            normal: [nx, ny, nz],
            tangent: [tx, ty, tz],
        }
    }

    fn write(&self, out: &mut Vec<u8>) {
        let [[x0, x1], [y0, y1], [z0, z1]] = self.position.map(u16::to_le_bytes);
        let [[u0, u1], [v0, v1]] = self.uv.map(u16::to_le_bytes);
        let ([nx, ny, nz], [tx, ty, tz]) = (self.normal, self.tangent);
        out.extend_from_slice(&[
            x0, x1, y0, y1, z0, z1, u0, u1, nz, ny, nx, v0, tz, ty, tx, v1,
        ]);
    }

    /// Position in island metres.
    pub(crate) fn metres(&self) -> [f32; 3] {
        let [x, y, z] = self.position.map(f32::from);
        [
            x / XZ_PER_METRE,
            (y - Y_ZERO) / Y_PER_METRE,
            z / XZ_PER_METRE,
        ]
    }
}

/// One node of a tile.
#[derive(Debug, Clone, PartialEq, Default)]
pub(crate) struct Node {
    pub(crate) indices: Vec<u16>,
    pub(crate) vertices: Vec<Vertex>,
    /// Material ids (bit per id) the node's ground uses. The reader's default
    /// is all ones; shipped files always store it.
    pub(crate) used_material_bits: u32,
}

/// One `.tmc` file: `Tile::NODES`² nodes, row-major, row along z.
#[derive(Debug, Clone)]
pub(crate) struct Tile {
    pub(crate) nodes: Vec<Node>,
    // Ground-object scatter stays in the original tree. Geometry edits
    // patch only the mesh fields, retaining every scatter record.
    source: Option<Vec<u8>>,
}

impl PartialEq for Tile {
    fn eq(&self, other: &Self) -> bool {
        self.nodes == other.nodes
    }
}

// The dictionary listing of every shipped tile (606 files, all levels): a
// new tile lists its names the same way. Ids follow first use.
const NODE_NAMES: [&str; 3] = ["Meshes", "map", "GroundObjects"];
const VALUE_NAMES: [&str; 5] = ["width", "Indices", "height", "Vertices", "UsedMaterialBits"];
/// What every node holds, in write order.
const NODE_FIELDS: [&str; 4] = ["Indices", "Vertices", "UsedMaterialBits", "GroundObjects"];

impl Tile {
    /// Nodes per tile side.
    const NODES: usize = 32;

    /// A tile of empty nodes.
    fn empty() -> Tile {
        Tile {
            nodes: vec![Node::default(); Tile::NODES * Tile::NODES],
            source: None,
        }
    }

    pub(crate) fn parse(bytes: &[u8]) -> Result<Tile> {
        let adf = Adf::parse(bytes).context("tmc")?;
        let meshes = adf
            .child_node(adf.root(), "Meshes")
            .context("tmc has no Meshes")?;
        let size = |name: &str| -> Result<usize> {
            Ok(adf.read_u32(adf.child_value(meshes, name).context("tmc size")?)? as usize)
        };
        ensure!(
            (size("width")?, size("height")?) == (Tile::NODES, Tile::NODES),
            "tmc is not {n}x{n} nodes",
            n = Tile::NODES
        );
        let map = adf.child_node(meshes, "map").context("tmc has no map")?;
        let mut nodes = Vec::with_capacity(Tile::NODES * Tile::NODES);
        for item in adf.children(map) {
            nodes.push(
                Self::parse_node(&adf, item)
                    .with_context(|| format!("tmc node {}", nodes.len()))?,
            );
        }
        ensure!(
            nodes.len() == Tile::NODES * Tile::NODES,
            "tmc has {} nodes",
            nodes.len()
        );
        Ok(Tile {
            nodes,
            source: Some(bytes.to_vec()),
        })
    }

    fn parse_node(adf: &Adf, item: ItemId) -> Result<Node> {
        let mut node = Node {
            used_material_bits: u32::MAX,
            ..Node::default()
        };
        // Which of `NODE_FIELDS` the node has, a bit each.
        let mut seen = 0u8;
        for field in adf.children(item) {
            let p = adf.payload(field);
            let name = adf.name(field);
            if let Some(k) = NODE_FIELDS.iter().position(|&f| Some(f) == name) {
                seen |= 1 << k;
            }
            match name {
                Some("Indices") => {
                    ensure!(
                        p.len().is_multiple_of(3 * size_of::<u16>()),
                        "indices are {} B, not whole triangles",
                        p.len()
                    );
                    node.indices = p
                        .as_chunks()
                        .0
                        .iter()
                        .map(|&b| u16::from_le_bytes(b))
                        .collect();
                }
                Some("Vertices") => {
                    ensure!(p.len().is_multiple_of(VERTEX), "vertices are {} B", p.len());
                    node.vertices = p.as_chunks::<VERTEX>().0.iter().map(Vertex::read).collect();
                }
                Some("UsedMaterialBits") => {
                    node.used_material_bits = adf.read_u32(field)?;
                }
                // Ground object scatter (FileName, PositionsXZ, RotationsY,
                // Coverages per entry). The parsed tile's
                // source tree retains these during geometry rewrites.
                Some("GroundObjects") => {}
                other => bail!("unexpected field {other:?}"),
            }
        }
        ensure!(
            seen == (1 << NODE_FIELDS.len()) - 1,
            "node misses fields (mask {seen:#x}); rewrite would not roundtrip"
        );
        let count = node.vertices.len();
        if let Some(&i) = node.indices.iter().find(|&&i| usize::from(i) >= count) {
            bail!("index {i} out of {count} vertices");
        }
        Ok(node)
    }

    /// Encodes the tile the way the game's tools write it; saves go through
    /// `to_adf` to take the island's ADF generation.
    #[cfg(test)]
    pub(crate) fn encode(&self) -> Vec<u8> {
        self.to_adf().encode()
    }

    /// The tile as an ADF tree: its parsed source with the mesh fields
    /// replaced, or a new tree as the game's tools lay one out.
    pub(crate) fn to_adf(&self) -> Adf {
        assert_eq!(
            self.nodes.len(),
            Tile::NODES * Tile::NODES,
            "tile node count"
        );
        let fields = |node: &Node| {
            let indices: Vec<u8> = node.indices.iter().flat_map(|v| v.to_le_bytes()).collect();
            let mut vertices = Vec::with_capacity(node.vertices.len() * VERTEX);
            node.vertices.iter().for_each(|v| v.write(&mut vertices));
            [
                ("Indices", indices),
                ("Vertices", vertices),
                (
                    "UsedMaterialBits",
                    node.used_material_bits.to_le_bytes().to_vec(),
                ),
            ]
        };
        if let Some(source) = &self.source {
            let mut adf = Adf::parse(source).expect("parsed terrain tile");
            let map = adf.node(adf.root(), "Meshes/map").expect("parsed tile map");
            let items: Vec<_> = adf.children(map).collect();
            for (node, item) in self.nodes.iter().zip(items) {
                for (name, bytes) in fields(node) {
                    let field = adf.child_value(item, name).expect("parsed tile field");
                    adf.set_payload(field, &bytes);
                }
            }
            return adf;
        }
        let mut adf = Adf::new();
        let new = |adf: &mut Adf| -> Result<()> {
            let side = (Tile::NODES as u32).to_le_bytes();
            let meshes = adf.add_node(adf.root(), Some("Meshes"))?;
            adf.add_value(meshes, Some("width"), &side)?;
            adf.add_value(meshes, Some("height"), &side)?;
            let map = adf.add_node(meshes, Some("map"))?;
            for node in &self.nodes {
                let item = adf.add_node(map, None)?;
                for (name, bytes) in fields(node) {
                    adf.add_value(item, Some(name), &bytes)?;
                }
                adf.add_node(item, Some("GroundObjects"))?;
            }
            Ok(())
        };
        new(&mut adf).expect("a new tile only takes its own names");
        adf.set_listing(&NODE_NAMES, &VALUE_NAMES);
        adf
    }
}

/// Merged triangles of one tile file (skirts included), in island metres.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TileMesh {
    pub positions: Vec<[f32; 3]>,
    pub indices: Vec<u32>,
}

impl TileMesh {
    pub fn parse(bytes: &[u8]) -> Result<TileMesh> {
        let tile = Tile::parse(bytes)?;
        let mut out = TileMesh::default();
        for node in &tile.nodes {
            let base = out.positions.len() as u32;
            out.positions
                .extend(node.vertices.iter().map(Vertex::metres));
            out.indices
                .extend(node.indices.iter().map(|&i| base + u32::from(i)));
        }
        Ok(out)
    }
}

/// `x` and `y` of a `<x>x<y>.tmc` file name; tests find tiles by it.
#[cfg(test)]
pub(crate) fn tile_coords(file: &str) -> Option<(u32, u32)> {
    let stem = file.rsplit('/').next()?.strip_suffix(".tmc")?;
    let (x, y) = stem.split_once('x')?;
    Some((x.parse().ok()?, y.parse().ok()?))
}

// ------------------------------------------------------------------ remesher
//
// Each level is one right-triangulated irregular network (RTIN, the
// Martini/4-8 scheme) over the 0.5 m height samples, padded to a 2^n + 1
// grid. Splits are forced wherever a triangle would cross a node border or
// mix active and inactive ActiveMap cells, so every node is a union of whole
// triangles and the mesh covers exactly the active area. One RTIN per level
// makes neighbouring nodes share their border vertices (no T-junctions inside
// a level); skirts cover seams between levels.

/// Inputs for one level, all in the island frame.
pub(crate) struct LevelSource<'a> {
    pub level: u32,
    /// Heights in metres, 0.5 m texels.
    pub heights: &'a Grid<f32>,
    /// Mesh domain: ActiveMap cells of `active_cell` metres.
    pub active: &'a Grid<bool>,
    pub active_cell: usize,
    /// Node grid of this level (rd3d LevelSizeX/Z).
    pub nodes: (usize, usize),
    /// UsedMaterialBits per node, row-major.
    pub material_bits: &'a [u32],
    /// False below MinMeshLevel: nodes keep only their material bits.
    pub geometry: bool,
}

/// Vertical error bound of a level's heightfield mesh, metres. Doubles per
/// level like the node size; picked so triangle counts land near the shipped
/// ones.
pub(crate) fn max_error(level: u32) -> f32 {
    0.08 * std::f32::consts::SQRT_2.powi(level.saturating_sub(2) as i32)
}

/// u16 indices: a node holds at most this many vertices.
const MAX_NODE_VERTICES: usize = u16::MAX as usize + 1;

struct Rtin {
    size: usize,
    heights: Vec<f32>,
    errors: Vec<f32>,
}

impl Rtin {
    // Errors of every triangle's hypotenuse midpoint, children before
    // parents (Martini). `force` marks triangles that must split.
    fn new(src: &Grid<f32>, force: impl Fn([usize; 4]) -> bool) -> Rtin {
        let span = (src.width.max(src.height) - 1).max(2).next_power_of_two();
        let size = span + 1;
        let heights: Vec<f32> = (0..size * size)
            .map(|i| {
                src.get(
                    (i % size).min(src.width - 1),
                    (i / size).min(src.height - 1),
                )
            })
            .collect();
        let mut errors = vec![0f32; size * size];
        let tile = span;
        let triangles = tile * tile * 2 - 2;
        let parents = triangles - tile * tile;
        for i in (0..triangles).rev() {
            let mut id = i + 2;
            let (mut ax, mut ay, mut bx, mut by, mut cx, mut cy) = (0, 0, 0, 0, 0, 0);
            if id & 1 == 1 {
                (bx, by, cx) = (tile, tile, tile);
            } else {
                (ax, ay, cy) = (tile, tile, tile);
            }
            loop {
                id >>= 1;
                if id <= 1 {
                    break;
                }
                let (mx, my) = ((ax + bx) >> 1, (ay + by) >> 1);
                if id & 1 == 1 {
                    (bx, by, ax, ay) = (ax, ay, cx, cy);
                } else {
                    (ax, ay, bx, by) = (bx, by, cx, cy);
                }
                (cx, cy) = (mx, my);
            }
            let (mx, my) = ((ax + bx) >> 1, (ay + by) >> 1);
            let (cx, cy) = (mx + my - ay, my + ax - mx);
            let mid = my * size + mx;
            let interpolated = (heights[ay * size + ax] + heights[by * size + bx]) / 2.;
            let mut err = errors[mid].max((interpolated - heights[mid]).abs());
            if i < parents {
                let left = ((ay + cy) >> 1) * size + ((ax + cx) >> 1);
                let right = ((by + cy) >> 1) * size + ((bx + cx) >> 1);
                err = err.max(errors[left]).max(errors[right]);
            }
            let bbox = [
                ax.min(bx).min(cx),
                ay.min(by).min(cy),
                ax.max(bx).max(cx),
                ay.max(by).max(cy),
            ];
            if force(bbox) {
                err = f32::INFINITY;
            }
            errors[mid] = err;
        }
        Rtin {
            size,
            heights,
            errors,
        }
    }

    // Leaf triangles under `max_error`, as grid indices.
    fn triangles(&self, max_error: f32, out: &mut Vec<[u32; 3]>) {
        let t = self.size - 1;
        self.split([0, 0], [t, t], [t, 0], max_error, out);
        self.split([t, t], [0, 0], [0, t], max_error, out);
    }

    fn split(
        &self,
        a: [usize; 2],
        b: [usize; 2],
        c: [usize; 2],
        max_error: f32,
        out: &mut Vec<[u32; 3]>,
    ) {
        let m = [(a[0] + b[0]) >> 1, (a[1] + b[1]) >> 1];
        let leg = a[0].abs_diff(c[0]) + a[1].abs_diff(c[1]);
        if leg > 1 && self.errors[m[1] * self.size + m[0]] > max_error {
            self.split(c, a, m, max_error, out);
            self.split(b, c, m, max_error, out);
        } else {
            let at = |p: [usize; 2]| (p[1] * self.size + p[0]) as u32;
            out.push([at(a), at(b), at(c)]);
        }
    }
}

// A vertex before quantisation, with its accumulated normal.
#[derive(Clone, Copy)]
struct Point {
    pos: [f32; 3],
    normal: [f32; 3],
}

fn cross(u: [f32; 3], v: [f32; 3]) -> [f32; 3] {
    [
        u[1] * v[2] - u[2] * v[1],
        u[2] * v[0] - u[0] * v[2],
        u[0] * v[1] - u[1] * v[0],
    ]
}

fn sub(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}

fn unorm8(v: f32) -> u8 {
    ((v * 0.5 + 0.5) * 255.).round().clamp(0., 255.) as u8
}

// Shipped winding: cross(b - a, c - a) points up (+y).
fn upward(p: [[f32; 3]; 3]) -> bool {
    cross(sub(p[1], p[0]), sub(p[2], p[0]))[1] >= 0.
}

// Packs one node: top vertices, then skirts under every boundary edge.
fn pack_node(points: &[Point], tris: &[[u32; 3]], skirt: f32, out: &mut Node) -> Result<()> {
    let mut local: HashMap<u32, u16> = HashMap::new();
    let mut verts: Vec<Point> = Vec::new();
    let mut indices: Vec<u16> = Vec::with_capacity(tris.len() * 3);
    for t in tris {
        for &g in t {
            let next = verts.len();
            let i = *local.entry(g).or_insert_with(|| {
                verts.push(points[g as usize]);
                next as u16
            });
            ensure!(
                verts.len() <= MAX_NODE_VERTICES,
                "node over {MAX_NODE_VERTICES} vertices"
            );
            indices.push(i);
        }
    }
    // Boundary edges are used by one triangle; their winding gives the side.
    let mut edges: HashMap<(u16, u16), (u32, u16, u16)> = HashMap::new();
    for t in indices.as_chunks::<3>().0 {
        for k in 0..3 {
            let (a, b) = (t[k], t[(k + 1) % 3]);
            let e = edges.entry((a.min(b), a.max(b))).or_insert((0, a, b));
            e.0 += 1;
        }
    }
    let mut boundary: Vec<(u16, u16)> = edges
        .values()
        .filter(|e| e.0 == 1)
        .map(|e| (e.1, e.2))
        .collect();
    boundary.sort_unstable();
    let mut below: HashMap<u16, u16> = HashMap::new();
    for &(a, b) in &boundary {
        let mut down = |v: u16| -> Result<u16> {
            if let Some(&s) = below.get(&v) {
                return Ok(s);
            }
            let mut p = verts[usize::from(v)];
            p.pos[1] -= skirt;
            verts.push(p);
            ensure!(
                verts.len() <= MAX_NODE_VERTICES,
                "node over {MAX_NODE_VERTICES} vertices"
            );
            let s = (verts.len() - 1) as u16;
            below.insert(v, s);
            Ok(s)
        };
        let (sa, sb) = (down(a)?, down(b)?);
        indices.extend_from_slice(&[a, sa, b, sa, sb, b]);
    }

    let quantised: Vec<[u16; 3]> = verts
        .iter()
        .map(|p| {
            [
                (p.pos[0] * XZ_PER_METRE)
                    .round()
                    .clamp(0., f32::from(u16::MAX)) as u16,
                (p.pos[1] * Y_PER_METRE + Y_ZERO)
                    .round()
                    .clamp(0., f32::from(u16::MAX)) as u16,
                (p.pos[2] * XZ_PER_METRE)
                    .round()
                    .clamp(0., f32::from(u16::MAX)) as u16,
            ]
        })
        .collect();
    let range = |axis: usize| {
        let lo = quantised.iter().map(|q| q[axis]).min().unwrap_or(0);
        let hi = quantised.iter().map(|q| q[axis]).max().unwrap_or(0);
        (f32::from(lo), (f32::from(hi) - f32::from(lo)).max(1.))
    };
    let ((x0, xs), (z0, zs)) = (range(0), range(2));
    out.vertices = verts
        .iter()
        .zip(&quantised)
        .map(|(p, q)| {
            let n = p.normal;
            let len = (n[0] * n[0] + n[1] * n[1] + n[2] * n[2]).sqrt();
            let n = if len > 0. {
                n.map(|c| c / len)
            } else {
                [0., 1., 0.]
            };
            let t = (n[1] * n[1] + n[0] * n[0]).sqrt();
            let t = if t > 1e-6 {
                [-n[1] / t, n[0] / t, 0.]
            } else {
                [-1., 0., 0.]
            };
            // y < 0 would read as the skirt flag; overhang normals clamp flat.
            let ny = unorm8(n[1]).max(128);
            Vertex {
                position: *q,
                uv: [
                    ((f32::from(q[0]) - x0) / xs * 65535.).floor() as u16,
                    ((f32::from(q[2]) - z0) / zs * 65535.).floor() as u16,
                ],
                normal: [unorm8(n[0]), ny, unorm8(n[2])],
                tangent: t.map(unorm8),
            }
        })
        .collect();
    out.indices = indices;
    Ok(())
}

/// Builds every tile of one level, `(x, y, tile)` row-major.
pub(crate) fn build_level(src: &LevelSource) -> Result<Vec<(u32, u32, Tile)>> {
    let (nw, nh) = src.nodes;
    ensure!(
        src.material_bits.len() == nw * nh,
        "material bits for {}x{} nodes",
        nw,
        nh
    );
    let mut nodes: Vec<Node> = src
        .material_bits
        .iter()
        .map(|&used_material_bits| Node {
            used_material_bits,
            ..Node::default()
        })
        .collect();

    if src.geometry {
        mesh_level(src, &mut nodes)?;
    }

    let (tw, th) = (nw.div_ceil(Tile::NODES), nh.div_ceil(Tile::NODES));
    let mut tiles = Vec::with_capacity(tw * th);
    for ty in 0..th {
        for tx in 0..tw {
            let mut tile = Tile::empty();
            for (i, slot) in tile.nodes.iter_mut().enumerate() {
                let (x, y) = (
                    tx * Tile::NODES + i % Tile::NODES,
                    ty * Tile::NODES + i / Tile::NODES,
                );
                if x < nw && y < nh {
                    *slot = std::mem::take(&mut nodes[y * nw + x]);
                }
            }
            tiles.push((tx as u32, ty as u32, tile));
        }
    }
    Ok(tiles)
}

fn mesh_level(src: &LevelSource, nodes: &mut [Node]) -> Result<()> {
    let (nw, nh) = src.nodes;
    let h = src.heights;
    // Samples per node and per active cell (0.5 m texels).
    let node_samples = 2usize << src.level;
    let cell_samples = 2 * src.active_cell;
    let active = |cx: usize, cy: usize| {
        cx < src.active.width && cy < src.active.height && src.active.get(cx, cy)
    };
    let mixed = |[x0, y0, x1, y1]: [usize; 4]| {
        let (cx0, cy0, cx1, cy1) = (
            x0 / cell_samples,
            y0 / cell_samples,
            (x1 - 1) / cell_samples,
            (y1 - 1) / cell_samples,
        );
        if (cx0, cy0) == (cx1, cy1) {
            return false;
        }
        let first = active(cx0, cy0);
        (cy0..=cy1).any(|cy| (cx0..=cx1).any(|cx| active(cx, cy) != first))
    };
    let rtin = Rtin::new(h, |b| {
        b[0] / node_samples != (b[2] - 1) / node_samples
            || b[1] / node_samples != (b[3] - 1) / node_samples
            || mixed(b)
    });
    let mut error = max_error(src.level);
    let size = rtin.size;
    loop {
        let mut tris = Vec::new();
        rtin.triangles(error, &mut tris);
        let mut per_node: Vec<Vec<[u32; 3]>> = vec![Vec::new(); nw * nh];
        let mut points: Vec<Point> = (0..size * size)
            .map(|i| Point {
                pos: [
                    (i % size) as f32 * 0.5,
                    rtin.heights[i],
                    (i / size) as f32 * 0.5,
                ],
                normal: [0.; 3],
            })
            .collect();
        for t in tris {
            let xy = |g: u32| ((g as usize) % size, (g as usize) / size);
            let p = t.map(xy);
            let (sx, sy) = (p[0].0 + p[1].0 + p[2].0, p[0].1 + p[1].1 + p[2].1);
            // Inside the real grid and an active cell (centroid decides; forced
            // splits made the triangle's cells agree).
            if p.iter().any(|&(x, y)| x >= h.width || y >= h.height)
                || !active(sx / 3 / cell_samples, sy / 3 / cell_samples)
            {
                continue;
            }
            let (nx, ny) = (sx / 3 / node_samples, sy / 3 / node_samples);
            if nx >= nw || ny >= nh {
                continue;
            }
            let pos = t.map(|g| points[g as usize].pos);
            let t = if upward(pos) { t } else { [t[0], t[2], t[1]] };
            let pos = t.map(|g| points[g as usize].pos);
            let face = cross(sub(pos[1], pos[0]), sub(pos[2], pos[0]));
            for &g in &t {
                let n = &mut points[g as usize].normal;
                (0..3).for_each(|k| n[k] += face[k]);
            }
            per_node[ny * nw + nx].push(t);
        }
        let skirt = SKIRT_DEPTH * (1u32 << src.level) as f32;
        let mut fits = true;
        for (i, tris) in per_node.iter().enumerate() {
            if tris.is_empty() {
                continue;
            }
            if pack_node(&points, tris, skirt, &mut nodes[i]).is_err() {
                fits = false;
                break;
            }
        }
        if !fits {
            // Too fine for u16 indices: loosen this level and retry.
            error *= 2.;
            continue;
        }
        return Ok(());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vfs::Install;

    #[test]
    fn installed_1800_ground_scatter_survives_geometry_edits() {
        let Some(dir) = crate::vfs::install_dir("ANNO1800_DIR") else {
            return;
        };
        let install = Install::open(dir, crate::vfs::Install::DATA_FIRST).unwrap();
        let path = "data/sessions/islands/pool/moderate/moderate_m_01/_gamedata/moderate_m_01/meshes/2/0x0.tmc";
        let bytes = install.read(path).unwrap();
        let tree = Adf::parse(&bytes).unwrap();
        let map = tree.node(tree.root(), "Meshes/map").unwrap();
        assert!(tree.children(map).any(|n| {
            tree.child_node(n, "GroundObjects")
                .is_some_and(|g| tree.children(g).next().is_some())
        }));
        let mut tile = Tile::parse(&bytes).unwrap();
        assert_eq!(tile.encode(), bytes);
        let vertex = &mut tile
            .nodes
            .iter_mut()
            .find(|n| !n.vertices.is_empty())
            .unwrap()
            .vertices[0];
        let height = vertex.position[1];
        vertex.position[1] = height.wrapping_add(1);
        let changed = tile.encode();
        assert_eq!(changed.len(), bytes.len());
        assert!((1..=2).contains(&bytes.iter().zip(&changed).filter(|(a, b)| a != b).count()));
        let back = Tile::parse(&changed).unwrap();
        assert_eq!(
            back.nodes
                .iter()
                .find(|n| !n.vertices.is_empty())
                .unwrap()
                .vertices[0]
                .position[1],
            height.wrapping_add(1)
        );
    }

    pub(crate) fn install() -> Option<Install> {
        let Some(dir) = crate::vfs::install_dir("ANNO117_DIR") else {
            eprintln!("skipping: ANNO117_DIR unset");
            return None;
        };
        Some(Install::open(dir, crate::vfs::Install::DATA_FIRST).unwrap())
    }

    pub(crate) const PIRATE: &str = "data/base/provinces/roman/islands/3rdparty/roman_island_3rdparty_pirate_01/_gamedata/roman_island_3rdparty_pirate_01";

    // The coarsest pirate_01 tile spans the island's active rect (48..288 m)
    // and its heights sit inside the coarse heightmap's range.
    #[test]
    fn pirate_island_tile_fits_the_active_rect() {
        let Some(install) = install() else { return };
        let mesh =
            TileMesh::parse(&install.read(format!("{PIRATE}/meshes/9/0x0.tmc")).unwrap()).unwrap();
        assert_eq!((mesh.positions.len(), mesh.indices.len()), (5650, 32505));
        let (lo, hi) = mesh
            .positions
            .iter()
            .fold(([f32::MAX; 3], [f32::MIN; 3]), |(lo, hi), p| {
                (
                    std::array::from_fn(|i| lo[i].min(p[i])),
                    std::array::from_fn(|i| hi[i].max(p[i])),
                )
            });
        assert_eq!((lo[0], hi[0], lo[2], hi[2]), (48., 288., 48., 272.));
        assert!(hi[1] < 21., "{hi:?}");
    }

    #[test]
    fn vertex_bytes_roundtrip() {
        let bytes: [u8; 16] = [
            0x18, 0x10, 0xa9, 0x85, 0x10, 0x10, 0xff, 0x2f, 0x83, 0xaf, 0x09, 0xff, 0x80, 0x09,
            0x50, 0x1f,
        ];
        let v = Vertex::read(&bytes);
        assert_eq!(
            v.metres(),
            [128.75, (0x85a9 as f32 - Y_ZERO) / Y_PER_METRE, 128.5]
        );
        // u = 0.75 m into a 4 m node, v = 0.5 m.
        assert_eq!(v.uv, [0x2fff, 0x1fff]);
        assert_eq!(
            (v.normal, v.tangent),
            ([0x09, 0xaf, 0x83], [0x50, 0x09, 0x80])
        );
        let mut out = Vec::new();
        v.write(&mut out);
        assert_eq!(out, bytes);
    }

    // Every tile of every level of four islands (128..512 m) re-encodes to
    // the shipped bytes.
    #[test]
    fn shipped_tiles_roundtrip() {
        let Some(install) = install() else { return };
        let islands = [
            "data/base/provinces/roman/islands/deco/roman_island_deco_01/_gamedata/roman_island_deco_01",
            "data/base/provinces/roman/islands/pool/roman_island_small_07/_gamedata/roman_island_small_07",
            PIRATE,
            "data/base/provinces/roman/islands/pool/roman_island_large_01/_gamedata/roman_island_large_01",
        ];
        let mut files = 0;
        for island in islands {
            let meshes = install.folder(&format!("{island}/meshes")).unwrap();
            for &level in install.subfolders(meshes) {
                for &file in install.folder_files(level) {
                    let bytes = install.read_file(file).unwrap();
                    let tile = Tile::parse(&bytes).unwrap();
                    assert!(
                        tile.encode() == bytes,
                        "{} differs",
                        install.file_path(file)
                    );
                    files += 1;
                }
            }
        }
        assert!(files > 600, "{files} tiles");
    }
}
