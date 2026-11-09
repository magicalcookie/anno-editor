// `.rdm` mesh containers: the block framing, the mesh record (vertex
// layout, vertex and index buffers, material ranges) and the per-vertex
// attribute decode. Animation-only files and skeletons are not read; skinned
// meshes come back in their bind pose with `skinned` set.
//
// Layout and vertex codes follow lukts30/rdm4 (MIT, Anno 1800):
// https://github.com/lukts30/rdm4, rdm4lib/src/{rdm_data_main,vertex}.rs.
use std::fmt;

use anyhow::{Context, Result, bail, ensure};

/// Byte ranges of one material inside [`Rdm::indices`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MaterialGroup {
    /// Index into the cfg model's `<Materials>` list (and [`Rdm::materials`]).
    pub material: u32,
    /// First index in [`Rdm::indices`].
    pub start: u32,
    /// Number of indices, a multiple of 3.
    pub count: u32,
}

/// Material slot names baked into the rdm by the exporter. The cfg's
/// `<Materials>` list is authoritative for shading; this is for matching and
/// debugging.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RdmMaterial {
    pub name: String,
    /// Source texture path from the artist's machine, often empty.
    pub texture: String,
}

/// What a vertex attribute means. The numbers are the on-disk semantic ids.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Semantic {
    Position,
    Normal,
    Tangent,
    Bitangent,
    Texcoord,
    Color,
    Weight,
    JointIndex,
    Unknown(u32),
}

impl Semantic {
    fn from_id(id: u32) -> Semantic {
        match id {
            0 => Semantic::Position,
            1 => Semantic::Normal,
            2 => Semantic::Tangent,
            3 => Semantic::Bitangent,
            4 => Semantic::Texcoord,
            5 => Semantic::Color,
            6 => Semantic::Weight,
            7 => Semantic::JointIndex,
            n => Semantic::Unknown(n),
        }
    }

    fn letter(self) -> char {
        match self {
            Semantic::Position => 'P',
            Semantic::Normal => 'N',
            Semantic::Tangent => 'G',
            Semantic::Bitangent => 'B',
            Semantic::Texcoord => 'T',
            Semantic::Color => 'C',
            Semantic::Weight => 'W',
            Semantic::JointIndex => 'I',
            Semantic::Unknown(_) => '?',
        }
    }
}

/// Scalar storage. The same codes describe index buffers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Storage {
    U16,
    /// One packed word of four bytes when `count == 1` (`4b`/`4c` formats).
    U32,
    F16,
    F32,
}

impl Storage {
    fn from_code(code: u32) -> Option<Storage> {
        Some(match code {
            3 => Storage::U16,
            5 => Storage::U32,
            6 => Storage::F16,
            7 => Storage::F32,
            _ => return None,
        })
    }

    fn size(self) -> usize {
        match self {
            Storage::U16 | Storage::F16 => 2,
            Storage::U32 | Storage::F32 => 4,
        }
    }
}

/// One 16-byte vertex attribute descriptor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Attribute {
    pub semantic: Semantic,
    pub storage: Storage,
    /// How packed words decode: 0 floats / joint indices, 2 weights and some
    /// colors, 4 colors, 6 signed-normalized vectors. Kept for diagnostics;
    /// the decode goes by semantic.
    pub interpretation: u32,
    pub count: u32,
}

impl Attribute {
    fn size(&self) -> usize {
        self.storage.size() * self.count as usize
    }
}

impl fmt::Display for Attribute {
    // The names the cfg `<VertexFormat>` uses: P4h, N4b, T2h, C4c, ...
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let (n, suffix) = match self.storage {
            Storage::F16 => (self.count, 'h'),
            Storage::F32 => (self.count, 'f'),
            Storage::U32 if self.count == 1 && (3..=5).contains(&self.interpretation) => (4, 'c'),
            Storage::U32 if self.count == 1 => (4, 'b'),
            Storage::U32 => (self.count, 'i'),
            Storage::U16 => (self.count, 's'),
        };
        write!(f, "{}{n}{suffix}", self.semantic.letter())
    }
}

/// The vertex layout, in buffer order.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct VertexFormat(pub Vec<Attribute>);

impl VertexFormat {
    pub fn stride(&self) -> usize {
        self.0.iter().map(Attribute::size).sum()
    }

    fn find(&self, semantic: Semantic) -> Option<(usize, Attribute)> {
        let mut offset = 0;
        for a in &self.0 {
            if a.semantic == semantic {
                return Some((offset, *a));
            }
            offset += a.size();
        }
        None
    }
}

impl fmt::Display for VertexFormat {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        for (i, a) in self.0.iter().enumerate() {
            if i > 0 {
                f.write_str("_")?;
            }
            write!(f, "{a}")?;
        }
        Ok(())
    }
}

/// The file is an animation (no mesh record). Check with
/// `err.is::<AnimationOnly>()`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AnimationOnly;

impl fmt::Display for AnimationOnly {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("rdm holds an animation, not a mesh")
    }
}

impl std::error::Error for AnimationOnly {}

/// One decoded mesh. All vertex arrays are either empty (attribute absent)
/// or `positions.len()` long. Coordinates are the engine's: Y up, metres.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Rdm {
    pub format: VertexFormat,
    pub positions: Vec<[f32; 3]>,
    /// Unit vectors.
    pub normals: Vec<[f32; 3]>,
    /// Unit tangent with handedness in `w` (+1/-1), derived from the stored
    /// bitangent. Empty when the format has no tangent.
    pub tangents: Vec<[f32; 4]>,
    /// First texcoord set, D3D convention: (0,0) is the texture's top-left
    /// texel, so DDS data samples without a V flip.
    pub uvs: Vec<[f32; 2]>,
    /// First color set as stored bytes. Vegetation uses it for wind and
    /// AO masks, not as albedo tint.
    pub colors: Option<Vec<[u8; 4]>>,
    pub indices: Vec<u32>,
    pub groups: Vec<MaterialGroup>,
    pub materials: Vec<RdmMaterial>,
    /// Stored AABB.
    pub bounds: ([f32; 3], [f32; 3]),
    /// Has a skeleton or joint/weight attributes. Positions are the bind
    /// pose; joints and weights are not decoded.
    pub skinned: bool,
    /// Exporter's object name, e.g. `tree_07`.
    pub name: String,
}

// Root record fields (all offsets): export info, mesh meta, materials,
// skeleton, animation meta.
const ROOT_META: usize = 4;
const ROOT_MATERIALS: usize = 8;
const ROOT_SKIN: usize = 12;
const ROOT_ANIM: usize = 16;

impl Rdm {
    pub fn parse(bytes: &[u8]) -> Result<Rdm> {
        let r = Reader(bytes);
        ensure!(bytes.len() >= 0x1c && &bytes[..3] == b"RDM", "not an rdm");
        // Big-endian files (byte 3 == 0) are Anno 2070 only.
        ensure!(bytes[3] == 1, "big-endian rdm is not supported");
        ensure!(r.u32(4)? == 0x14, "bad rdm preamble");
        // The root payload is normally at 0x1c, but a few hand-made files
        // (ui/placeholder/rdm/anchor.rdm) store it last; follow the pointer.
        let root = r.block(r.u32(0x10)? as usize)?;
        // 48- and 52-byte root records both occur.
        ensure!(root.count == 1 && root.stride >= 20, "bad rdm root record");
        let root = root.start;
        let meta = r.u32(root + ROOT_META)? as usize;
        if meta == 0 {
            if r.u32(root + ROOT_ANIM)? != 0 {
                bail!(AnimationOnly);
            }
            bail!("rdm has neither mesh nor animation");
        }
        let skin = r.u32(root + ROOT_SKIN)? != 0;

        // Mesh meta: name, vertex format, index format, vertex buffer, index
        // buffer, material ranges, skeleton index, bounds min/max. 68- and
        // 92-byte strides occur; the tail is unknown.
        let m = r.block(meta)?;
        ensure!(m.stride >= 52, "rdm mesh meta is {} bytes", m.stride);
        let ptr = |i: usize| r.u32(meta + 4 * i).map(|p| p as usize);
        let skeleton = r.u32(meta + 24)? as i32;
        let bounds = (r.vec3(meta + 28)?, r.vec3(meta + 40)?);

        let name = match ptr(0)? {
            0 => String::new(),
            p => r.string(r.u32(p)? as usize)?,
        };

        // Format record -> block of 16-byte descriptors.
        let fmt_rec = ptr(1)?;
        ensure!(fmt_rec != 0, "rdm has no vertex format");
        let ids = r.block(r.u32(fmt_rec)? as usize)?;
        ensure!(ids.stride == 16, "vertex descriptor stride {}", ids.stride);
        let mut format = VertexFormat::default();
        for i in 0..ids.count {
            let at = ids.start + i * 16;
            let code = r.u32(at + 4)?;
            let storage =
                Storage::from_code(code).with_context(|| format!("vertex storage type {code}"))?;
            format.0.push(Attribute {
                semantic: Semantic::from_id(r.u32(at)?),
                storage,
                interpretation: r.u32(at + 8)?,
                count: r.u32(at + 12)?,
            });
        }

        let index_storage = match ptr(2)? {
            0 => Storage::U16,
            p => {
                let code = r.u32(p)?;
                Storage::from_code(code).with_context(|| format!("index type {code}"))?
            }
        };
        ensure!(
            matches!(index_storage, Storage::U16 | Storage::U32),
            "index type {index_storage:?}"
        );

        let verts = r.block(ptr(3)?).context("vertex buffer")?;
        ensure!(
            verts.stride == format.stride(),
            "vertex stride {} but format {format} is {} bytes",
            verts.stride,
            format.stride()
        );
        let skinned = skin
            || skeleton >= 0
            || format.find(Semantic::JointIndex).is_some()
            || format.find(Semantic::Weight).is_some();
        let mut out = decode_vertices(format, r.bytes(verts.start, verts.len())?, verts.count)?;

        let tris = r.block(ptr(4)?).context("index buffer")?;
        ensure!(
            tris.stride == index_storage.size(),
            "index stride {} for {index_storage:?}",
            tris.stride
        );
        let data = r.bytes(tris.start, tris.len())?;
        out.indices = match index_storage {
            Storage::U16 => data
                .as_chunks::<2>()
                .0
                .iter()
                .map(|&c| u16::from_le_bytes(c) as u32)
                .collect(),
            _ => data
                .as_chunks::<4>()
                .0
                .iter()
                .map(|&c| u32::from_le_bytes(c))
                .collect(),
        };
        let n = out.positions.len() as u32;
        if let Some(bad) = out.indices.iter().find(|&&i| i >= n) {
            bail!("index {bad} out of {n} vertices");
        }

        // Material ranges: start, count, material, then 16 unknown bytes.
        let groups = r.block(ptr(5)?).context("material ranges")?;
        ensure!(groups.stride >= 12, "mesh info stride {}", groups.stride);
        for i in 0..groups.count {
            let at = groups.start + i * groups.stride;
            let g = MaterialGroup {
                start: r.u32(at)?,
                count: r.u32(at + 4)?,
                material: r.u32(at + 8)?,
            };
            ensure!(
                g.start as usize + g.count as usize <= out.indices.len(),
                "material range {}+{} past {} indices",
                g.start,
                g.count,
                out.indices.len()
            );
            out.groups.push(g);
        }

        // Materials: block of 28-byte records, each pointing at a one-record
        // block whose first two fields are name and texture strings.
        let mats = r.u32(root + ROOT_MATERIALS)? as usize;
        if mats != 0 {
            let mats = r.block(mats).context("materials")?;
            for i in 0..mats.count {
                let rec = r.u32(mats.start + i * mats.stride)? as usize;
                let mut m = RdmMaterial::default();
                if rec != 0 {
                    m.name = r.string(r.u32(rec)? as usize)?;
                    m.texture = r.string(r.u32(rec + 4)? as usize)?;
                }
                out.materials.push(m);
            }
        }

        out.bounds = bounds;
        out.skinned = skinned;
        out.name = name;
        Ok(out)
    }

    /// Recomputed AABB of the positions (the stored one may be pre-quantization).
    pub fn computed_bounds(&self) -> ([f32; 3], [f32; 3]) {
        let mut lo = [f32::INFINITY; 3];
        let mut hi = [f32::NEG_INFINITY; 3];
        for p in &self.positions {
            for k in 0..3 {
                lo[k] = lo[k].min(p[k]);
                hi[k] = hi[k].max(p[k]);
            }
        }
        (lo, hi)
    }
}

// Decodes `count` vertices of `format` from `data` (tightly packed).
// Fills the vertex arrays and `format` of the returned mesh.
fn decode_vertices(format: VertexFormat, data: &[u8], count: usize) -> Result<Rdm> {
    let stride = format.stride();
    ensure!(stride > 0, "empty vertex format");
    ensure!(
        data.len() == stride * count,
        "vertex buffer {} bytes, want {count} x {stride}",
        data.len()
    );
    let verts = || data.chunks_exact(stride);
    let mut out = Rdm::default();

    let (off, a) = format
        .find(Semantic::Position)
        .context("vertex format has no position")?;
    ensure!(a.count >= 3, "position {a}");
    out.positions = verts().map(|v| floats(a, &v[off..])).collect();

    if let Some((off, a)) = format.find(Semantic::Normal) {
        out.normals = verts()
            .map(|v| vector(a, &v[off..]).map(normalize))
            .collect::<Result<_>>()?;
    }
    if let Some((off, a)) = format.find(Semantic::Tangent) {
        let bitangent = format.find(Semantic::Bitangent);
        out.tangents = verts()
            .enumerate()
            .map(|(i, v)| {
                let t = normalize(vector(a, &v[off..])?);
                // Handedness: does the stored bitangent agree with N x T?
                let w = match (bitangent, out.normals.get(i)) {
                    (Some((boff, b)), Some(n)) => {
                        let b = vector(b, &v[boff..])?;
                        let c = cross(*n, t);
                        if c[0] * b[0] + c[1] * b[1] + c[2] * b[2] < 0.0 {
                            -1.0
                        } else {
                            1.0
                        }
                    }
                    _ => 1.0,
                };
                Ok([t[0], t[1], t[2], w])
            })
            .collect::<Result<_>>()?;
    }
    if let Some((off, a)) = format.find(Semantic::Texcoord) {
        ensure!(a.count >= 2 && a.storage != Storage::U32, "texcoord {a}");
        out.uvs = verts()
            .map(|v| {
                let f = floats(a, &v[off..]);
                [f[0], f[1]]
            })
            .collect();
    }
    if let Some((off, a)) = format.find(Semantic::Color) {
        out.colors = Some(match (a.storage, a.count) {
            (Storage::U32, 1) => verts()
                .map(|v| [v[off], v[off + 1], v[off + 2], v[off + 3]])
                .collect(),
            // C4h (a few villa ruin pieces): 0..1 floats.
            (Storage::F16, 4) => verts()
                .map(|v| {
                    [0, 1, 2, 3].map(|k| {
                        let c = f16(u16::from_le_bytes([v[off + 2 * k], v[off + 2 * k + 1]]));
                        (c.clamp(0.0, 1.0) * 255.0).round() as u8
                    })
                })
                .collect(),
            _ => bail!("unsupported color {a}"),
        });
    }
    out.format = format;
    Ok(out)
}

// First three components of a float attribute (P4h, P3f, T2h -> padded).
fn floats(a: Attribute, b: &[u8]) -> [f32; 3] {
    let mut out = [0.0; 3];
    for (k, o) in out.iter_mut().enumerate().take(a.count as usize) {
        *o = match a.storage {
            Storage::F16 => f16(u16::from_le_bytes([b[2 * k], b[2 * k + 1]])),
            Storage::F32 => {
                f32::from_le_bytes([b[4 * k], b[4 * k + 1], b[4 * k + 2], b[4 * k + 3]])
            }
            Storage::U16 => u16::from_le_bytes([b[2 * k], b[2 * k + 1]]) as f32,
            Storage::U32 => {
                u32::from_le_bytes([b[4 * k], b[4 * k + 1], b[4 * k + 2], b[4 * k + 3]]) as f32
            }
        };
    }
    out
}

// A direction: packed `4b` bytes map 0..255 to -1..1 (x, y, z, then a
// spare byte), float formats are taken as stored.
fn vector(a: Attribute, b: &[u8]) -> Result<[f32; 3]> {
    Ok(match (a.storage, a.count) {
        (Storage::U32, 1) => [0, 1, 2].map(|k| b[k] as f32 * (2.0 / 255.0) - 1.0),
        (Storage::F16 | Storage::F32, 3..) => floats(a, b),
        _ => bail!("unsupported direction {a}"),
    })
}

fn normalize(v: [f32; 3]) -> [f32; 3] {
    let len = (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt();
    // A zero vector would be an exporter bug; keep the mesh and point it up.
    if len < 1e-6 {
        return [0.0, 1.0, 0.0];
    }
    v.map(|c| c / len)
}

fn cross(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    ]
}

/// IEEE half to f32.
fn f16(h: u16) -> f32 {
    let sign = ((h & 0x8000) as u32) << 16;
    let exp = ((h >> 10) & 0x1f) as u32;
    let man = (h & 0x3ff) as u32;
    let bits = match exp {
        0 if man == 0 => sign,
        0 => {
            // Subnormal: man * 2^-24.
            let v = man as f32 * (1.0 / 16_777_216.0);
            return if sign != 0 { -v } else { v };
        }
        31 => sign | 0x7f80_0000 | (man << 13),
        _ => sign | ((exp + 112) << 23) | (man << 13),
    };
    f32::from_bits(bits)
}

#[derive(Debug, Clone, Copy)]
struct Block {
    start: usize,
    count: usize,
    stride: usize,
}

impl Block {
    fn len(&self) -> usize {
        self.count * self.stride
    }
}

// Bounds-checked little-endian access. Offsets point at a block's payload;
// count and stride sit in the 8 bytes before it.
struct Reader<'a>(&'a [u8]);

impl Reader<'_> {
    fn bytes(&self, at: usize, len: usize) -> Result<&[u8]> {
        at.checked_add(len)
            .and_then(|end| self.0.get(at..end))
            .with_context(|| {
                format!(
                    "rdm read {len} bytes at {at:#x} past end {:#x}",
                    self.0.len()
                )
            })
    }

    fn u32(&self, at: usize) -> Result<u32> {
        let b = self.bytes(at, 4)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    fn vec3(&self, at: usize) -> Result<[f32; 3]> {
        Ok([
            f32::from_bits(self.u32(at)?),
            f32::from_bits(self.u32(at + 4)?),
            f32::from_bits(self.u32(at + 8)?),
        ])
    }

    fn block(&self, payload: usize) -> Result<Block> {
        ensure!(payload >= 8, "rdm null or bad block offset {payload:#x}");
        let count = self.u32(payload - 8)? as usize;
        let stride = self.u32(payload - 4)? as usize;
        let len = count
            .checked_mul(stride)
            .context("rdm block size overflow")?;
        self.bytes(payload, len)?;
        Ok(Block {
            start: payload,
            count,
            stride,
        })
    }

    fn string(&self, payload: usize) -> Result<String> {
        if payload == 0 {
            return Ok(String::new());
        }
        let b = self.block(payload)?;
        ensure!(b.stride == 1, "rdm string stride {}", b.stride);
        Ok(String::from_utf8_lossy(self.bytes(b.start, b.count)?).into_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vfs::{FileId, Install};
    use std::collections::BTreeMap;

    fn attr(semantic: Semantic, storage: Storage, interpretation: u32, count: u32) -> Attribute {
        Attribute {
            semantic,
            storage,
            interpretation,
            count,
        }
    }

    // The common static prop layout, P4h_N4b_G4b_B4b_T2h_C4b.
    fn prop_format() -> VertexFormat {
        VertexFormat(vec![
            attr(Semantic::Position, Storage::F16, 0, 4),
            attr(Semantic::Normal, Storage::U32, 6, 1),
            attr(Semantic::Tangent, Storage::U32, 6, 1),
            attr(Semantic::Bitangent, Storage::U32, 6, 1),
            attr(Semantic::Texcoord, Storage::F16, 0, 2),
            attr(Semantic::Color, Storage::U32, 2, 1),
        ])
    }

    // half bits: 1.0 = 0x3c00, -2.0 = 0xc000, 0.5 = 0x3800, 0.25 = 0x3400.
    fn prop_vertex(bitangent: [u8; 4]) -> Vec<u8> {
        let mut v = Vec::new();
        for h in [0x3c00u16, 0xc000, 0x3800, 0x3c00, 0x3400, 0x3800] {
            v.extend(h.to_le_bytes());
            if v.len() == 8 {
                // N +Y, T +X, then the bitangent.
                v.extend([127, 255, 127, 255, 255, 127, 127, 255]);
                v.extend(bitangent);
            }
        }
        v.extend([10, 20, 30, 40]);
        v
    }

    #[test]
    fn decodes_packed_prop_vertex() {
        let format = prop_format();
        assert_eq!(format.to_string(), "P4h_N4b_G4b_B4b_T2h_C4b");
        assert_eq!(format.stride(), 28);
        let mut data = prop_vertex([127, 127, 255, 255]); // B +Z
        data.extend(prop_vertex([127, 127, 0, 255])); // B -Z: mirrored UVs
        let m = decode_vertices(format, &data, 2).unwrap();
        assert_eq!(m.positions, vec![[1.0, -2.0, 0.5]; 2]);
        assert_eq!(m.uvs, vec![[0.25, 0.5]; 2]);
        assert_eq!(m.colors, Some(vec![[10, 20, 30, 40]; 2]));
        // 127 and 255 are -0.004 and 1.0; normalized they snap to the axis.
        let n = m.normals[0];
        assert!((n[1] - 1.0).abs() < 1e-4 && n[0].abs() < 0.01, "{n:?}");
        assert!(m.tangents[0][0] > 0.999);
        // N x T = +Y x +X = -Z, so a +Z bitangent means w = -1.
        assert_eq!(m.tangents[0][3], -1.0);
        assert_eq!(m.tangents[1][3], 1.0);
    }

    #[test]
    fn decodes_float_and_half_color_formats() {
        let format = VertexFormat(vec![
            attr(Semantic::Position, Storage::F32, 0, 3),
            attr(Semantic::Normal, Storage::F32, 0, 3),
            attr(Semantic::Color, Storage::F16, 0, 4),
        ]);
        assert_eq!(format.to_string(), "P3f_N3f_C4h");
        let mut v = Vec::new();
        for f in [1.5f32, 2.0, -3.0, 0.0, 0.0, 2.0] {
            v.extend(f.to_le_bytes());
        }
        for h in [0x3c00u16, 0x3800, 0, 0x3c00] {
            v.extend(h.to_le_bytes());
        }
        let m = decode_vertices(format, &v, 1).unwrap();
        assert_eq!(m.positions, vec![[1.5, 2.0, -3.0]]);
        assert_eq!(m.normals, vec![[0.0, 0.0, 1.0]]);
        assert_eq!(m.colors, Some(vec![[255, 128, 0, 255]]));
        assert!(m.uvs.is_empty() && m.tangents.is_empty());
    }

    #[test]
    fn half_floats() {
        assert_eq!(f16(0x3c00), 1.0);
        assert_eq!(f16(0xc000), -2.0);
        assert_eq!(f16(0x7bff), 65504.0);
        assert_eq!(f16(0x0001), 2f32.powi(-24));
        assert_eq!(f16(0x8000).to_bits(), (-0.0f32).to_bits());
        assert!(f16(0x7c00).is_infinite());
    }

    // Lays out blocks the way exporters do: header (count, stride) then
    // payload. Offsets are patched once every block has a position.
    struct Builder(Vec<u8>);

    impl Builder {
        fn block(&mut self, count: u32, stride: u32, payload: &[u8]) -> u32 {
            self.0.extend(count.to_le_bytes());
            self.0.extend(stride.to_le_bytes());
            let at = self.0.len() as u32;
            self.0.extend(payload);
            at
        }

        fn patch(&mut self, at: u32, value: u32) {
            let at = at as usize;
            self.0[at..at + 4].copy_from_slice(&value.to_le_bytes());
        }
    }

    fn le(words: &[u32]) -> Vec<u8> {
        words.iter().flat_map(|w| w.to_le_bytes()).collect()
    }

    fn synthetic_rdm(indices: &[u16]) -> Vec<u8> {
        let mut b = Builder(b"RDM\x01".to_vec());
        b.0.extend(le(&[0x14, 0, 4, 0x1c]));
        let root = b.block(1, 48, &[0; 48]);
        let meta = b.block(1, 92, &[0; 92]);
        b.patch(root + 4, meta);
        let name_rec = b.block(1, 28, &[0; 28]);
        let name = b.block(4, 1, b"quad");
        b.patch(name_rec, name);
        let fmt_rec = b.block(1, 24, &[0; 24]);
        let mut ids = Vec::new();
        for a in &prop_format().0 {
            let sem = match a.semantic {
                Semantic::Position => 0,
                Semantic::Normal => 1,
                Semantic::Tangent => 2,
                Semantic::Bitangent => 3,
                Semantic::Texcoord => 4,
                _ => 5,
            };
            let st = if a.storage == Storage::F16 { 6 } else { 5 };
            ids.extend(le(&[sem, st, a.interpretation, a.count]));
        }
        let ids = b.block(6, 16, &ids);
        b.patch(fmt_rec, ids);
        let idx_fmt = b.block(1, 20, &le(&[3, 0, 0, 0, 0]));
        let groups = b.block(1, 28, &[le(&[0, 3, 0]), vec![0; 16]].concat());
        let mut verts = Vec::new();
        for _ in 0..4 {
            verts.extend(prop_vertex([127, 127, 255, 255]));
        }
        let verts = b.block(4, 28, &verts);
        let idx: Vec<u8> = indices.iter().flat_map(|i| i.to_le_bytes()).collect();
        let idx = b.block(indices.len() as u32, 2, &idx);
        for (i, p) in [name_rec, fmt_rec, idx_fmt, verts, idx, groups]
            .into_iter()
            .enumerate()
        {
            b.patch(meta + 4 * i as u32, p);
        }
        b.patch(meta + 24, u32::MAX);
        b.0
    }

    #[test]
    fn parses_synthetic_container() {
        let m = Rdm::parse(&synthetic_rdm(&[0, 1, 2, 2, 1, 3])).unwrap();
        assert_eq!(m.name, "quad");
        assert_eq!(m.positions.len(), 4);
        assert_eq!(m.indices, vec![0, 1, 2, 2, 1, 3]);
        assert_eq!(
            m.groups,
            vec![MaterialGroup {
                material: 0,
                start: 0,
                count: 3
            }]
        );
        assert!(!m.skinned && m.materials.is_empty());

        let err = Rdm::parse(&synthetic_rdm(&[0, 1, 9])).unwrap_err();
        assert!(err.to_string().contains("out of 4"), "{err}");
        let mut cut = synthetic_rdm(&[0, 1, 2]);
        cut.truncate(cut.len() - 3);
        assert!(Rdm::parse(&cut).is_err());
    }

    fn install() -> Option<Install> {
        let Ok(dir) = std::env::var("ANNO117_DIR") else {
            eprintln!("skip: ANNO117_DIR unset");
            return None;
        };
        Some(Install::open(dir).unwrap())
    }

    // Golden values from lukts30/rdm4's glTF export of the same file: the
    // decode matched it to 1.5e-7 on every position, normal and UV.
    #[test]
    fn olive_tree_matches_rdm4() {
        let Some(install) = install() else { return };
        let path = "data/base/graphics/roman/environment/props/vegetation/trees/olive_trees/rdm/tree_07.rdm";
        let m = Rdm::parse(&install.read(path).unwrap()).unwrap();
        assert_eq!(m.format.to_string(), "P4h_N4b_G4b_B4b_T2h_C4b");
        assert_eq!((m.positions.len(), m.indices.len()), (1273, 2685));
        assert_eq!(&m.indices[..6], &[1, 0, 2, 2, 3, 1]);
        let names: Vec<_> = m.materials.iter().map(|m| m.name.as_str()).collect();
        assert_eq!(names, ["olive_bark", "olive_leaf"]);
        assert_eq!(m.groups[1].start, 2361);
        // UV orientation: the leaf texture has its stem at the bottom
        // (v = 1 with a top-left origin). On the mesh, the v = 1 edge of
        // the leaf cards must be the inner, lower end of each branch.
        let leaf = m.groups[1];
        let leaf_idx = &m.indices[leaf.start as usize..(leaf.start + leaf.count) as usize];
        let (mut inner, mut outer) = ((0.0, 0.0, 0), (0.0, 0.0, 0));
        for &i in leaf_idx {
            let (p, uv) = (m.positions[i as usize], m.uvs[i as usize]);
            let r = (p[0] - 0.02).hypot(p[2] + 0.23);
            let side = if uv[1] > 0.5 { &mut inner } else { &mut outer };
            *side = (side.0 + r, side.1 + p[1], side.2 + 1);
        }
        let mean = |s: (f32, f32, i32)| (s.0 / s.2 as f32, s.1 / s.2 as f32);
        let (inner, outer) = (mean(inner), mean(outer));
        assert!(
            inner.0 < outer.0 && inner.1 < outer.1,
            "{inner:?} {outer:?}"
        );
    }

    // ANNO117_SWEEP=1: every rdm in the install, counts per format and
    // failures by reason. ~15 s in release.
    #[test]
    fn sweep_install() {
        if std::env::var_os("ANNO117_SWEEP").is_none() {
            return;
        }
        let Some(install) = install() else { return };
        let mut formats: BTreeMap<String, usize> = BTreeMap::new();
        let mut failures: BTreeMap<String, (usize, String)> = BTreeMap::new();
        let (mut anim, mut skinned, mut ok) = (0, 0, 0);
        for i in 0..install.file_count() {
            let id = FileId(i as u32);
            let path = install.file_path(id);
            if !path.ends_with(".rdm") {
                continue;
            }
            match install.read_file(id).and_then(|b| Rdm::parse(&b)) {
                Ok(m) => {
                    ok += 1;
                    skinned += m.skinned as usize;
                    *formats.entry(m.format.to_string()).or_default() += 1;
                    let n = m.positions.len() as u32;
                    assert!(m.indices.iter().all(|&i| i < n), "{path}");
                    for v in &m.normals {
                        let len = (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt();
                        assert!((len - 1.0).abs() < 1e-3, "{path}: normal {v:?}");
                    }
                }
                Err(e) if e.is::<AnimationOnly>() => anim += 1,
                Err(e) => {
                    // Top context plus root cause, numbers stripped so
                    // reasons group.
                    let reason = format!("{e}: {}", e.root_cause());
                    let reason: String = reason.chars().filter(|c| !c.is_ascii_digit()).collect();
                    failures.entry(reason).or_insert((0, path.to_string())).0 += 1;
                }
            }
        }
        for (f, n) in &formats {
            eprintln!("{n:6} {f}");
        }
        eprintln!("{ok} meshes ({skinned} skinned), {anim} animation-only");
        for (reason, (n, example)) in &failures {
            eprintln!("FAIL {n:5} {reason} (e.g. {example})");
        }
        // Known bad shipped files: reed_02.rdm has a CR inserted before
        // every LF byte (text-mode copy), so all offsets after it are off.
        let failed: usize = failures.values().map(|f| f.0).sum();
        assert!(failed <= 1, "{failed} rdm failures");
    }
}
