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

use crate::adf::{Adf, ItemId};
use crate::island::Grid;

/// Nodes per tile side.
pub const TILE_NODES: usize = 32;
const VERTEX: usize = 16;

/// Skirt depth in node sizes: every node border edge gets a vertical strip
/// this far down so LOD and node seams never open a hole.
const SKIRT_DEPTH: f32 = 0.04;

/// One vertex as stored (`VSInputTerrain` in `shaders/include/terrain.fxh`):
/// `[u16 x, u16 y, u16 z, u16 u]`, then the normal and tangent as B8G8R8A8
/// whose alpha bytes carry the low and high byte of `v`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Vertex {
    /// x and z in 1/32 m (rd3d `UnitScale`), y in 1/256 m biased by 32768.
    pub position: [u16; 3],
    /// Texcoord over the node's vertex bounding box, unorm16.
    pub uv: [u16; 2],
    /// Normal x, y, z as unorm8 (`n * 0.5 + 0.5`). The shader reads y < 0 as
    /// a skirt flag for tessellation; shipped files never set it.
    pub normal: [u8; 3],
    /// Tangent x, y, z, unorm8. Shipped: normalize(-ny, nx, 0).
    pub tangent: [u8; 3],
}

impl Vertex {
    fn read(b: &[u8; VERTEX]) -> Vertex {
        let u = |i: usize| u16::from_le_bytes([b[i], b[i + 1]]);
        Vertex {
            position: [u(0), u(2), u(4)],
            uv: [u(6), u16::from_le_bytes([b[11], b[15]])],
            normal: [b[10], b[9], b[8]],
            tangent: [b[14], b[13], b[12]],
        }
    }

    fn write(&self, out: &mut Vec<u8>) {
        for v in [
            self.position[0],
            self.position[1],
            self.position[2],
            self.uv[0],
        ] {
            out.extend_from_slice(&v.to_le_bytes());
        }
        let [v_lo, v_hi] = self.uv[1].to_le_bytes();
        let ([nx, ny, nz], [tx, ty, tz]) = (self.normal, self.tangent);
        out.extend_from_slice(&[nz, ny, nx, v_lo, tz, ty, tx, v_hi]);
    }

    /// Position in island metres.
    pub fn metres(&self) -> [f32; 3] {
        let [x, y, z] = self.position.map(f32::from);
        [x / 32., (y - 32768.) / 256., z / 32.]
    }
}

/// One node of a tile.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Node {
    pub indices: Vec<u16>,
    pub vertices: Vec<Vertex>,
    /// Material ids (bit per id) the node's ground uses. The reader's default
    /// is all ones; shipped files always store it.
    pub used_material_bits: u32,
}

/// One `.tmc` file: `TILE_NODES`² nodes, row-major, row along z.
#[derive(Debug, Clone)]
pub struct Tile {
    pub nodes: Vec<Node>,
    // Ground-object scatter stays in the original tree. Geometry edits
    // patch only the mesh fields, retaining every scatter record.
    source: Option<Vec<u8>>,
}

impl PartialEq for Tile {
    fn eq(&self, other: &Self) -> bool {
        self.nodes == other.nodes
    }
}

// Every shipped tile uses the same dictionaries (606 files, all levels).
const NODE_NAMES: &[u8] = b"\x03\0\0\0\x02\0\x03\0\x04\0Meshes\0map\0GroundObjects\0\0\0\0\0\0";
const VALUE_NAMES: &[u8] = b"\x05\0\0\0\x01\x80\x03\x80\x02\x80\x04\x80\x05\x80\
width\0Indices\0height\0Vertices\0UsedMaterialBits\0\0\0\0";
// Record ids those dictionaries bind; unnamed array nodes are id 1.
const NODE: u16 = 1;
const MESHES: u16 = 2;
const MAP: u16 = 3;
const GROUND_OBJECTS: u16 = 4;
const WIDTH: u16 = 0x8001;
const HEIGHT: u16 = 0x8002;
const INDICES: u16 = 0x8003;
const VERTICES: u16 = 0x8004;
const USED_BITS: u16 = 0x8005;

fn record(out: &mut Vec<u8>, id: u16, payload: &[u8]) {
    out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    out.extend_from_slice(&u32::from(id).to_le_bytes());
    out.extend_from_slice(payload);
    out.resize(out.len() + (8 - payload.len() % 8) % 8, 0);
}

impl Tile {
    /// A tile of empty nodes.
    pub fn empty() -> Tile {
        Tile {
            nodes: vec![Node::default(); TILE_NODES * TILE_NODES],
            source: None,
        }
    }

    pub fn parse(bytes: &[u8]) -> Result<Tile> {
        let adf = Adf::decode(bytes).context("tmc")?;
        let meshes = adf
            .child_node(adf.root(), "Meshes")
            .context("tmc has no Meshes")?;
        let size = |name: &str| -> Result<usize> {
            Ok(adf.read_u32(adf.child_value(meshes, name).context("tmc size")?)? as usize)
        };
        ensure!(
            (size("width")?, size("height")?) == (TILE_NODES, TILE_NODES),
            "tmc is not {TILE_NODES}x{TILE_NODES} nodes"
        );
        let map = adf.child_node(meshes, "map").context("tmc has no map")?;
        let mut nodes = Vec::with_capacity(TILE_NODES * TILE_NODES);
        for item in adf.children(map) {
            nodes.push(
                Self::parse_node(&adf, item)
                    .with_context(|| format!("tmc node {}", nodes.len()))?,
            );
        }
        ensure!(
            nodes.len() == TILE_NODES * TILE_NODES,
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
        let mut seen = 0u8;
        for field in adf.children(item) {
            let p = adf.payload(field);
            match adf.name(field) {
                Some("Indices") => {
                    ensure!(
                        p.len().is_multiple_of(6),
                        "indices are {} B, not whole triangles",
                        p.len()
                    );
                    node.indices = p
                        .as_chunks::<2>()
                        .0
                        .iter()
                        .map(|&b| u16::from_le_bytes(b))
                        .collect();
                    seen |= 1;
                }
                Some("Vertices") => {
                    ensure!(p.len().is_multiple_of(VERTEX), "vertices are {} B", p.len());
                    node.vertices = p.as_chunks::<VERTEX>().0.iter().map(Vertex::read).collect();
                    seen |= 2;
                }
                Some("UsedMaterialBits") => {
                    node.used_material_bits = adf.read_u32(field)?;
                    seen |= 4;
                }
                // Ground object scatter (FileName, PositionsXZ, RotationsY,
                // Coverages per entry). The parsed tile's
                // source tree retains these during geometry rewrites.
                Some("GroundObjects") => {
                    seen |= 8;
                }
                other => bail!("unexpected field {other:?}"),
            }
        }
        ensure!(
            seen == 15,
            "node misses fields (mask {seen:#x}); rewrite would not roundtrip"
        );
        let count = node.vertices.len();
        if let Some(&i) = node.indices.iter().find(|&&i| usize::from(i) >= count) {
            bail!("index {i} out of {count} vertices");
        }
        Ok(node)
    }

    /// Encodes the tile the way the game's tools write it.
    pub fn encode(&self) -> Vec<u8> {
        assert_eq!(self.nodes.len(), TILE_NODES * TILE_NODES, "tile node count");
        if let Some(source) = &self.source {
            let mut adf = Adf::decode(source).expect("parsed terrain tile");
            let map = adf.node(adf.root(), "Meshes/map").expect("parsed tile map");
            let items: Vec<_> = adf.children(map).collect();
            for (node, item) in self.nodes.iter().zip(items) {
                for (name, bytes) in [
                    (
                        "Indices",
                        node.indices
                            .iter()
                            .flat_map(|v| v.to_le_bytes())
                            .collect::<Vec<_>>(),
                    ),
                    ("Vertices", {
                        let mut bytes = Vec::new();
                        node.vertices.iter().for_each(|v| v.write(&mut bytes));
                        bytes
                    }),
                    (
                        "UsedMaterialBits",
                        node.used_material_bits.to_le_bytes().to_vec(),
                    ),
                ] {
                    let field = adf.child_value(item, name).expect("parsed tile field");
                    adf.set_payload(field, &bytes);
                }
            }
            return adf.encode();
        }
        let geometry: usize = self
            .nodes
            .iter()
            .map(|n| n.indices.len() * 2 + n.vertices.len() * VERTEX)
            .sum();
        let mut out = Vec::with_capacity(geometry + 72 * self.nodes.len() + 256);
        let side = (TILE_NODES as u32).to_le_bytes();
        record(&mut out, MESHES, &[]);
        record(&mut out, WIDTH, &side);
        record(&mut out, HEIGHT, &side);
        record(&mut out, MAP, &[]);
        let mut buf = Vec::new();
        for node in &self.nodes {
            record(&mut out, NODE, &[]);
            buf.clear();
            buf.extend(node.indices.iter().flat_map(|i| i.to_le_bytes()));
            record(&mut out, INDICES, &buf);
            buf.clear();
            node.vertices.iter().for_each(|v| v.write(&mut buf));
            record(&mut out, VERTICES, &buf);
            record(&mut out, USED_BITS, &node.used_material_bits.to_le_bytes());
            record(&mut out, GROUND_OBJECTS, &[]);
            record(&mut out, 0, &[]);
            record(&mut out, 0, &[]);
        }
        for _ in 0..3 {
            record(&mut out, 0, &[]); // map, Meshes, root
        }
        let dict_a = out.len() as u32;
        out.extend_from_slice(NODE_NAMES);
        let dict_b = out.len() as u32;
        out.extend_from_slice(VALUE_NAMES);
        // Items: openers + values + 1.
        let items = (2 + 2 * self.nodes.len()) + (2 + 3 * self.nodes.len()) + 1;
        for w in [0, items as u32, dict_a, dict_b, 8, -3i32 as u32] {
            out.extend_from_slice(&w.to_le_bytes());
        }
        out
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

/// `x` and `y` of a `<x>x<y>.tmc` file name.
pub fn tile_coords(file: &str) -> Option<(u32, u32)> {
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
// a level); skirts cover seams between levels and against patched cells.

/// Replacement triangles for overhangs or imported meshes, in island metres.
/// They replace the heightfield mesh in every finest-level (4 m) node their
/// centroids fall in; coarser levels keep meshing the heightfield, so keep it
/// close to the patch's top surface.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct MeshPatch {
    pub triangles: Vec<[[f32; 3]; 3]>,
}

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
    /// Patches, already filtered to the finest level by the caller.
    pub patches: &'a [MeshPatch],
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
                (p.pos[0] * 32.).round().clamp(0., 65535.) as u16,
                (p.pos[1] * 256. + 32768.).round().clamp(0., 65535.) as u16,
                (p.pos[2] * 32.).round().clamp(0., 65535.) as u16,
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

    let (tw, th) = (nw.div_ceil(TILE_NODES), nh.div_ceil(TILE_NODES));
    let mut tiles = Vec::with_capacity(tw * th);
    for ty in 0..th {
        for tx in 0..tw {
            let mut tile = Tile::empty();
            for (i, slot) in tile.nodes.iter_mut().enumerate() {
                let (x, y) = (
                    tx * TILE_NODES + i % TILE_NODES,
                    ty * TILE_NODES + i / TILE_NODES,
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
    // Patched finest-level nodes: their heightfield triangles are dropped.
    let mut patched: HashMap<usize, Vec<[[f32; 3]; 3]>> = HashMap::new();
    for t in src.patches.iter().flat_map(|p| &p.triangles) {
        let (cx, cz) = (
            (t[0][0] + t[1][0] + t[2][0]) / 3.,
            (t[0][2] + t[1][2] + t[2][2]) / 3.,
        );
        let node_m = (1u32 << src.level) as f32;
        let (x, y) = ((cx / node_m).floor(), (cz / node_m).floor());
        ensure!(
            x >= 0. && y >= 0. && (x as usize) < nw && (y as usize) < nh,
            "patch triangle at ({cx}, {cz}) is outside the island"
        );
        // Outside the active region the game reads HasMeshArray as empty and
        // never draws the node.
        ensure!(
            active(
                (cx * 2.) as usize / cell_samples,
                (cz * 2.) as usize / cell_samples
            ),
            "patch triangle at ({cx}, {cz}) is outside the ActiveMap"
        );
        patched
            .entry(y as usize * nw + x as usize)
            .or_default()
            .push(*t);
    }
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
            // Patched nodes still shade their neighbours' border vertices.
            if !patched.contains_key(&(ny * nw + nx)) {
                per_node[ny * nw + nx].push(t);
            }
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
        for (&i, soup) in &patched {
            let (points, tris) = weld(soup);
            pack_node(&points, &tris, skirt, &mut nodes[i])
                .with_context(|| format!("patch node {i}"))?;
        }
        return Ok(());
    }
}

// Patch soup -> shared vertices (welded on the 1/32 m and 1/256 m grid) with
// area-weighted normals.
fn weld(soup: &[[[f32; 3]; 3]]) -> (Vec<Point>, Vec<[u32; 3]>) {
    let mut at: HashMap<[i32; 3], u32> = HashMap::new();
    let mut points: Vec<Point> = Vec::new();
    let mut tris = Vec::with_capacity(soup.len());
    for t in soup {
        let t = if upward(*t) { *t } else { [t[0], t[2], t[1]] };
        let face = cross(sub(t[1], t[0]), sub(t[2], t[0]));
        let idx = t.map(|p| {
            let key = [
                (p[0] * 32.).round() as i32,
                (p[1] * 256.).round() as i32,
                (p[2] * 32.).round() as i32,
            ];
            let next = points.len() as u32;
            let i = *at.entry(key).or_insert_with(|| {
                points.push(Point {
                    pos: p,
                    normal: [0.; 3],
                });
                next
            });
            let n = &mut points[i as usize].normal;
            (0..3).for_each(|k| n[k] += face[k]);
            i
        });
        if idx[0] != idx[1] && idx[1] != idx[2] && idx[0] != idx[2] {
            tris.push(idx);
        }
    }
    (points, tris)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vfs::Install;

    #[test]
    fn installed_1800_ground_scatter_survives_geometry_edits() {
        let Some(dir) = std::env::var_os("ANNO1800_DIR") else {
            return;
        };
        let install = Install::open(dir).unwrap();
        let path = "data/sessions/islands/pool/moderate/moderate_m_01/_gamedata/moderate_m_01/meshes/2/0x0.tmc";
        let bytes = install.read(path).unwrap();
        let tree = Adf::decode(&bytes).unwrap();
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

    #[test]
    fn tile_names_parse() {
        assert_eq!(tile_coords("a/meshes/2/1x2.tmc"), Some((1, 2)));
        assert_eq!(tile_coords("a/meshes/2/1x2.dds"), None);
    }

    pub(crate) fn install() -> Option<Install> {
        let Some(dir) = std::env::var_os("ANNO117_DIR") else {
            eprintln!("skipping: ANNO117_DIR unset");
            return None;
        };
        Some(Install::open(dir).unwrap())
    }

    pub(crate) const PIRATE: &str = "data/base/provinces/roman/islands/3rdparty/roman_island_3rdparty_pirate_01/_gamedata/roman_island_3rdparty_pirate_01";

    // The coarsest pirate_01 tile spans the island's active rect (48..288 m)
    // and its heights sit inside the coarse heightmap's range.
    #[test]
    fn pirate_island_tile_fits_the_active_rect() {
        let Some(install) = install() else { return };
        let mesh =
            TileMesh::parse(&install.read(&format!("{PIRATE}/meshes/9/0x0.tmc")).unwrap()).unwrap();
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
        assert_eq!(v.metres(), [128.75, (0x85a9 as f32 - 32768.) / 256., 128.5]);
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
