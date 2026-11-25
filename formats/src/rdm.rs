// `.rdm` containers: the block framing, the mesh record (vertex layout,
// vertex and index buffers, material ranges), the per-vertex attribute
// decode, skeletons, and animation-only files with their sampler, writer
// and key edits. The skeleton and animation layout is in
// docs/model-animation.md.
//
// Layout and vertex codes follow lukts30/rdm4 (MIT, Anno 1800):
// https://github.com/lukts30/rdm4, rdm4lib/src/{rdm_data_main,rdm_data_anim,vertex}.rs.
// Engine addresses here are Anno117.exe build 24988830 (Steam).
use std::fmt;

use anyhow::{Context, Result, bail, ensure};

/// Byte ranges of one material inside [`Rdm::indices`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MaterialGroup {
    /// Index into the cfg model's `<Materials>` list (and `Rdm::materials`).
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
pub(crate) struct RdmMaterial {
    pub(crate) name: String,
    /// Source texture path from the artist's machine, often empty.
    texture: String,
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
    /// Each known semantic's on-disk id, and the letter a cfg's
    /// `<VertexFormat>` names it by.
    const KNOWN: [(Semantic, u32, char); 8] = [
        (Semantic::Position, 0, 'P'),
        (Semantic::Normal, 1, 'N'),
        (Semantic::Tangent, 2, 'G'),
        (Semantic::Bitangent, 3, 'B'),
        (Semantic::Texcoord, 4, 'T'),
        (Semantic::Color, 5, 'C'),
        (Semantic::Weight, 6, 'W'),
        (Semantic::JointIndex, 7, 'I'),
    ];

    fn from_id(id: u32) -> Semantic {
        Semantic::KNOWN
            .iter()
            .find(|k| k.1 == id)
            .map_or(Semantic::Unknown(id), |k| k.0)
    }

    fn known(self) -> Option<&'static (Semantic, u32, char)> {
        Semantic::KNOWN.iter().find(|k| k.0 == self)
    }

    /// The on-disk semantic id, as [`Semantic::from_id`] reads it.
    fn id(self) -> u32 {
        match self {
            Semantic::Unknown(n) => n,
            known => known.known().expect("a known semantic").1,
        }
    }

    fn letter(self) -> char {
        self.known().map_or('?', |k| k.2)
    }
}

/// Scalar storage, numbered by its on-disk code. The same codes describe
/// index buffers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u32)]
pub enum Storage {
    U16 = 3,
    /// One packed word of four bytes when `count == 1` (`4b`/`4c` formats).
    U32 = 5,
    F16 = 6,
    F32 = 7,
}

impl Storage {
    const ALL: [Storage; 4] = [Storage::U16, Storage::U32, Storage::F16, Storage::F32];

    fn from_code(code: u32) -> Option<Storage> {
        Storage::ALL.into_iter().find(|s| s.code() == code)
    }

    /// The on-disk storage code, as [`Storage::from_code`] reads it.
    fn code(self) -> u32 {
        self as u32
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
    /// Interpretations of a packed word the cfg names `4c` (colors) rather
    /// than `4b`.
    const COLOR_WORDS: std::ops::RangeInclusive<u32> = 3..=5;

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
            Storage::U32
                if self.count == 1 && Attribute::COLOR_WORDS.contains(&self.interpretation) =>
            {
                (4, 'c')
            }
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
    fn stride(&self) -> usize {
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
    tangents: Vec<[f32; 4]>,
    /// First texcoord set, D3D convention: (0,0) is the texture's top-left
    /// texel, so DDS data samples without a V flip.
    pub uvs: Vec<[f32; 2]>,
    /// First color set as stored bytes. Vegetation uses it for wind and
    /// AO masks, not as albedo tint.
    pub colors: Option<Vec<[u8; 4]>>,
    pub indices: Vec<u32>,
    pub groups: Vec<MaterialGroup>,
    pub(crate) materials: Vec<RdmMaterial>,
    /// Stored AABB.
    pub bounds: ([f32; 3], [f32; 3]),
    /// Has a skeleton or joint/weight attributes. Positions are the bind
    /// pose.
    pub skinned: bool,
    /// Four joints per vertex, indices into [`Rdm::skeleton`]. Empty unless
    /// the format has `I4b`.
    pub joints: Vec<[u8; 4]>,
    /// The weights of `joints`, unorm8 as stored. Shipped sums are 254 to 256,
    /// so divide by the sum rather than by 255. A format with `I4b` and no
    /// `W4b` binds each vertex rigidly to its first joint: `[255, 0, 0, 0]`.
    /// 1800's portraits store sixteen influences; the four heaviest stay,
    /// summing lower. A few of their vertices weigh 0 in every slot, so
    /// guard the divide.
    pub weights: Vec<[u8; 4]>,
    /// The joints the mesh is bound to. `None` on a static mesh, and on a
    /// skinned one whose skeleton record has no joints (117's Egypt
    /// `ship_01`) or does not read; it then draws in its bind pose.
    pub skeleton: Option<Skeleton>,
    /// Exporter's object name, e.g. `tree_07`.
    name: String,
}

// ------------------------------------------------------------------ layout
//
// Both byte orders lay a file out the same way: the preamble, then blocks of
// `count` records of `stride` bytes, each after its two u32 counts. Pointers
// name a block's payload, and records grow at their ends, so readers check
// a minimum stride. Field offsets below count from a record's start.

/// The fourth magic byte: the order of every word, index and vertex scalar.
/// `RDM\0` is big-endian: 97 of 2070's 3,836 meshes (vehicles and props);
/// no other game ships one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum ByteOrder {
    Big,
    #[default]
    Little,
}

impl ByteOrder {
    fn of(byte: u8) -> Option<ByteOrder> {
        match byte {
            0 => Some(ByteOrder::Big),
            1 => Some(ByteOrder::Little),
            _ => None,
        }
    }

    #[cfg(test)]
    fn byte(self) -> u8 {
        match self {
            ByteOrder::Big => 0,
            ByteOrder::Little => 1,
        }
    }

    fn u16(self, b: [u8; 2]) -> u16 {
        match self {
            ByteOrder::Big => u16::from_be_bytes(b),
            ByteOrder::Little => u16::from_le_bytes(b),
        }
    }

    fn u32(self, b: [u8; 4]) -> u32 {
        match self {
            ByteOrder::Big => u32::from_be_bytes(b),
            ByteOrder::Little => u32::from_le_bytes(b),
        }
    }

    #[cfg(test)]
    fn u16_bytes(self, v: u16) -> [u8; 2] {
        match self {
            ByteOrder::Big => v.to_be_bytes(),
            ByteOrder::Little => v.to_le_bytes(),
        }
    }

    fn u32_bytes(self, v: u32) -> [u8; 4] {
        match self {
            ByteOrder::Big => v.to_be_bytes(),
            ByteOrder::Little => v.to_le_bytes(),
        }
    }

    // A packed `4b`/`4c` word's bytes in x, y, z, w order. Big-endian files
    // store the word swapped like any other: x last. (Read raw, a big-endian
    // bear's normals agree with its faces half the time; swapped, 98%.)
    fn packed(self, b: &[u8]) -> [u8; 4] {
        match self {
            ByteOrder::Big => [b[3], b[2], b[1], b[0]],
            ByteOrder::Little => [b[0], b[1], b[2], b[3]],
        }
    }
}

/// `RDM`, then the [`ByteOrder`] byte.
const MAGIC: &[u8; 3] = b"RDM";
struct Preamble;
impl Preamble {
    const ORDER: usize = MAGIC.len();
    /// Holds [`Preamble::SIZE`], which says the file is an rdm.
    const SIZE_AT: usize = 4;
    const SIZE: u32 = 0x14;
    /// The root record's pointer.
    const ROOT: usize = 0x10;
    /// Where the root's block payload normally starts, and the shortest file.
    const LEN: usize = 0x1c;
}

/// Each block's `count` and `stride`, before the payload pointers name.
struct BlockHead;
impl BlockHead {
    const COUNT: usize = 8;
    const STRIDE: usize = 4;
}

/// Pointers to export info, mesh meta, materials, skeleton, animation meta.
/// 48- and 52-byte root records both occur.
struct RootRecord;
impl RootRecord {
    const META: usize = 4;
    const MATERIALS: usize = 8;
    const SKIN: usize = 12;
    const ANIM: usize = 16;
    const MIN_STRIDE: usize = 20;
}

/// One mesh: pointers to its name, vertex format, index format, vertex and
/// index buffer and material ranges, then its skeleton index and stored
/// bounds. 68- and 92-byte strides occur; the tail is unknown.
struct MeshRecord;
impl MeshRecord {
    const NAME: usize = 0;
    const FORMAT: usize = 4;
    const INDEX_FORMAT: usize = 8;
    const VERTICES: usize = 12;
    const INDICES: usize = 16;
    const RANGES: usize = 20;
    const SKELETON: usize = 24;
    const MIN: usize = 28;
    const MAX: usize = 40;
    const MIN_STRIDE: usize = 52;
}

/// A vertex attribute descriptor. A vertex format record's first word
/// points at a block of them.
struct Descriptor;
impl Descriptor {
    const SEMANTIC: usize = 0;
    const STORAGE: usize = 4;
    const INTERPRETATION: usize = 8;
    const COUNT: usize = 12;
    const SIZE: usize = 16;
}

/// A material: pointers to its name and source texture strings.
struct MaterialRecord;
impl MaterialRecord {
    const NAME: usize = 0;
    const TEXTURE: usize = 4;
}

/// A material range: first index, index count, material, then 16 unknown
/// bytes.
struct RangeRecord;
impl RangeRecord {
    const START: usize = 0;
    const COUNT: usize = 4;
    const MATERIAL: usize = 8;
    const MIN_STRIDE: usize = 12;
}

/// What the root's skeleton pointer names: a block of these, 32 bytes
/// each, one per skeleton a mesh record can pick. Most files hold one; a
/// 117 horse three. The 28 bytes after the pointer are zero.
struct SkeletonRecord;
impl SkeletonRecord {
    const JOINTS: usize = 0;
    const MIN_STRIDE: usize = 4;
}

/// A joint, 84 bytes: its name, then its inverse bind transform
/// (translation, then rotation, in the file's quaternion order, see
/// [`Reader::quat`]), its parent (`u32::MAX` for a root), then six floats
/// (two points in joint space, mirrored between left and right joints:
/// bone bounds, unused here) and six zero words.
struct JointRecord;
impl JointRecord {
    const NAME: usize = 0;
    const TRANSLATION: usize = 4;
    const ROTATION: usize = 16;
    const PARENT: usize = 32;
    const MIN_STRIDE: usize = 36;
}

/// What the root's animation pointer names: a block of these, 48 bytes
/// each: the name of the mesh the record drives (often null), the tracks,
/// the duration in milliseconds and the rate the exporter sampled at, then
/// 32 zero bytes.
struct AnimRecord;
impl AnimRecord {
    const MESH: usize = 0;
    const TRACKS: usize = 4;
    const DURATION_MS: usize = 8;
    const SAMPLE_RATE: usize = 12;
    const MIN_STRIDE: usize = 16;
    const STRIDE: usize = 48;
}

/// One joint's keys: its name, its key block, then 16 zero bytes.
struct TrackRecord;
impl TrackRecord {
    const NAME: usize = 0;
    const KEYS: usize = 4;
    const MIN_STRIDE: usize = 8;
    const STRIDE: usize = 24;
}

/// A key, 32 bytes: parent-relative rotation (file order) and translation,
/// then its time in seconds.
struct KeyRecord;
impl KeyRecord {
    const ROTATION: usize = 0;
    const TRANSLATION: usize = 16;
    const TIME: usize = 28;
    const MIN_STRIDE: usize = 32;
    const STRIDE: usize = 32;
}

/// A record or bound component: a u32 or f32.
const WORD: usize = size_of::<u32>();

impl Rdm {
    pub fn parse(bytes: &[u8]) -> Result<Rdm> {
        let r = Reader::new(bytes)?;
        let root = r.root()?;
        let meta = r.u32(root + RootRecord::META)? as usize;
        if meta == 0 {
            if r.u32(root + RootRecord::ANIM)? != 0 {
                bail!(AnimationOnly);
            }
            bail!("rdm has neither mesh nor animation");
        }
        let skin = r.u32(root + RootRecord::SKIN)? as usize;

        // Mesh meta is a block of records, one per mesh. Most files hold
        // one; assembled models hold several (1800's mine rail: trestle
        // halves, rails, sleepers and conveyor belts, 8 meshes), which
        // merge into one here: indices and material ranges offset by the
        // meshes before them, attributes one mesh lacks filled with
        // neutral values so every array stays `positions.len()` long.
        let m = r.block(meta)?;
        ensure!(
            m.stride >= MeshRecord::MIN_STRIDE,
            "rdm mesh meta is {} bytes",
            m.stride
        );
        ensure!(m.count >= 1, "rdm mesh meta block is empty");
        // Each mesh names its skeleton record, and meshes may name different
        // ones: a 117 horse's body, tail and mane each carry their own copy
        // of the rig, in their own joint order. Every record a mesh uses
        // becomes a part of one skeleton, and the mesh's joint indices move
        // past the parts before its own. A skeleton that does not read
        // leaves the mesh in its bind pose, as skinned meshes drew before
        // skeletons were read.
        let mut skeleton = (skin != 0).then(Skeleton::default);
        let mut parts: Vec<(u32, usize)> = Vec::new();
        let mut out = Rdm::default();
        for k in 0..m.count {
            let rec = m.start + k * m.stride;
            // A broken later mesh ends the merge rather than the file: the
            // first mesh drew alone before later ones were read at all, so
            // nothing that loaded then fails now.
            let mut more = match mesh(&r, rec) {
                Ok(more) => more,
                Err(e) if k == 0 => return Err(e),
                Err(_) => break,
            };
            let index = r.u32(rec + MeshRecord::SKELETON)?;
            if let Some(s) = skeleton.as_mut().filter(|_| index != u32::MAX) {
                let first = match parts.iter().find(|p| p.0 == index) {
                    Some(p) => Ok(p.1),
                    None => (s.read_part(&r, skin, index as usize, &more.name))
                        .inspect(|&first| parts.push((index, first))),
                };
                if first.and_then(|first| more.rebase_joints(first)).is_err() {
                    skeleton = None;
                }
            }
            match k {
                0 => out = more,
                _ => out.append(more),
            }
        }
        out.skinned |= skin != 0;
        out.skeleton = skeleton.filter(|s| !s.is_empty());

        // Materials: a block of pointers, each at a one-record block whose
        // first two fields are name and texture strings.
        let mats = r.u32(root + RootRecord::MATERIALS)? as usize;
        if mats != 0 {
            let mats = r.block(mats).context("materials")?;
            for i in 0..mats.count {
                let rec = r.u32(mats.start + i * mats.stride)? as usize;
                let mut m = RdmMaterial::default();
                if rec != 0 {
                    m.name = r.string(r.u32(rec + MaterialRecord::NAME)? as usize)?;
                    m.texture = r.string(r.u32(rec + MaterialRecord::TEXTURE)? as usize)?;
                }
                out.materials.push(m);
            }
        }

        Ok(out)
    }

    /// Moves this mesh's joint indices past the `first` joints of the parts
    /// before its skeleton's.
    fn rebase_joints(&mut self, first: usize) -> Result<()> {
        for j in self.joints.iter_mut().flatten() {
            let moved = *j as usize + first;
            ensure!(moved <= u8::MAX as usize, "joint {moved} past a byte");
            *j = moved as u8;
        }
        Ok(())
    }

    /// Appends `other`'s geometry after this mesh's.
    fn append(&mut self, other: Rdm) {
        let (n, m) = (self.positions.len(), other.positions.len());
        // An attribute either side has: the other side padded to match.
        fn join<T: Copy>(a: &mut Vec<T>, b: Vec<T>, n: usize, m: usize, fill: T) {
            if a.is_empty() && b.is_empty() {
                return;
            }
            a.resize(n, fill);
            if b.is_empty() {
                a.resize(n + m, fill);
            } else {
                a.extend(b);
            }
        }
        join(&mut self.normals, other.normals, n, m, [0., 1., 0.]);
        join(&mut self.tangents, other.tangents, n, m, [1., 0., 0., 1.]);
        join(&mut self.uvs, other.uvs, n, m, [0., 0.]);
        // A static mesh merged into a skinned one follows the first joint.
        join(&mut self.joints, other.joints, n, m, [0; 4]);
        join(&mut self.weights, other.weights, n, m, [255, 0, 0, 0]);
        match (&mut self.colors, other.colors) {
            (None, None) => {}
            (colors, other) => {
                let mut a = colors.take().unwrap_or_default();
                join(&mut a, other.unwrap_or_default(), n, m, [255; 4]);
                *colors = Some(a);
            }
        }
        self.positions.extend(other.positions);
        let base = self.indices.len() as u32;
        self.indices
            .extend(other.indices.iter().map(|i| i + n as u32));
        self.groups
            .extend(other.groups.into_iter().map(|g| MaterialGroup {
                start: g.start + base,
                ..g
            }));
        let (lo, hi) = (other.bounds.0, other.bounds.1);
        for a in 0..3 {
            self.bounds.0[a] = self.bounds.0[a].min(lo[a]);
            self.bounds.1[a] = self.bounds.1[a].max(hi[a]);
        }
        self.skinned |= other.skinned;
    }

    /// Recomputed AABB of the positions (the stored one may be pre-quantization).
    pub(crate) fn computed_bounds(&self) -> ([f32; 3], [f32; 3]) {
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

/// The vertex layout of the mesh record at `meta`, and its descriptor block:
/// the record's second field points at a format record whose first word
/// points at the 16-byte descriptors.
fn vertex_format(r: &Reader, meta: usize) -> Result<(VertexFormat, Block)> {
    let fmt_rec = r.u32(meta + MeshRecord::FORMAT)? as usize;
    ensure!(fmt_rec != 0, "rdm has no vertex format");
    let ids = r.block(r.u32(fmt_rec)? as usize)?;
    ensure!(
        ids.stride == Descriptor::SIZE,
        "vertex descriptor stride {}",
        ids.stride
    );
    let mut format = VertexFormat::default();
    for i in 0..ids.count {
        let at = ids.start + i * Descriptor::SIZE;
        let code = r.u32(at + Descriptor::STORAGE)?;
        let storage =
            Storage::from_code(code).with_context(|| format!("vertex storage type {code}"))?;
        format.0.push(Attribute {
            semantic: Semantic::from_id(r.u32(at + Descriptor::SEMANTIC)?),
            storage,
            interpretation: r.u32(at + Descriptor::INTERPRETATION)?,
            count: r.u32(at + Descriptor::COUNT)?,
        });
    }
    Ok((format, ids))
}

/// How [`rebake`] rewrites a mesh for a slot that places it with no
/// transform of its own (an 1800 prp).
#[derive(Debug, Clone, Default)]
pub struct Rebake {
    /// The cfg part's placement, baked into positions, directions and the
    /// stored bounds.
    pub transform: crate::model::Transform,
    /// The vertex layout to write; `None` keeps the source's.
    pub layout: Option<VertexFormat>,
    /// What each color slot of `layout` holds when the source has no color
    /// for it (a source's one color fills slot 0).
    pub color_fill: Vec<[u8; 4]>,
    /// Texcoords map into this `[u0, v0, u1, v1]` rectangle of the texture
    /// (a decal's atlas cell).
    pub uv_rect: Option<[f32; 4]>,
}

/// Rewrites a static rdm as `how` says. If the layout stays, the vertices
/// change in place. If it changes, each mesh record gets a new vertex buffer
/// and descriptor list appended to the file and points at those; the old ones
/// stay behind as dead bytes, so no other offset moves. A source without
/// tangents (117's common `P4h_N4b_T2h_C4b`) gets them computed from its
/// texcoords.
pub fn rebake(bytes: &[u8], how: &Rebake) -> Result<Vec<u8>> {
    let parsed = Rdm::parse(bytes)?;
    ensure!(!parsed.skinned, "skinned meshes keep their bind pose");
    let r = Reader::new(bytes)?;
    // It writes little-endian words into a copy of the file.
    ensure!(
        r.order == ByteOrder::Little,
        "big-endian rdm (2070) cannot be rebaked"
    );
    let root = r.root()?;
    let meta = r.block(r.u32(root + RootRecord::META)? as usize)?;
    let mut out = bytes.to_vec();
    let t = &how.transform;
    // Directions: normals take the inverse scale, tangents the scale, so a
    // squashed part keeps its normals perpendicular to its surface.
    let turn = |v: [f32; 3], scale: [f32; 3]| {
        let only = crate::model::Transform {
            position: [0.; 3],
            rotation: t.rotation,
            scale,
        };
        normalize(only.apply(v))
    };
    let inverse = t.scale.map(|s| 1. / s);
    let uv = |p: [f32; 2]| match how.uv_rect {
        Some([u0, v0, u1, v1]) => [u0 + p[0] * (u1 - u0), v0 + p[1] * (v1 - v0)],
        None => p,
    };
    for k in 0..meta.count {
        let rec = meta.start + k * meta.stride;
        let (source, _) = vertex_format(&r, rec)?;
        let verts = r.block(r.u32(rec + MeshRecord::VERTICES)? as usize)?;
        ensure!(verts.stride == source.stride(), "vertex stride");
        let layout = how.layout.clone().unwrap_or_else(|| source.clone());
        let decoded = mesh(&r, rec)?;
        let n = decoded.positions.len();
        let positions: Vec<[f32; 3]> = decoded.positions.iter().map(|p| t.apply(*p)).collect();
        let normals: Vec<[f32; 3]> = match decoded.normals.len() {
            0 => vec![[0., 1., 0.]; n],
            _ => decoded.normals.iter().map(|v| turn(*v, inverse)).collect(),
        };
        let tangents: Vec<[f32; 4]> = match decoded.tangents.len() {
            0 => tangents_from_uvs(&decoded),
            _ => decoded.tangents.clone(),
        }
        .into_iter()
        .map(|g| {
            let d = turn([g[0], g[1], g[2]], t.scale);
            [d[0], d[1], d[2], g[3]]
        })
        .collect();
        let colors = decoded.colors.unwrap_or_default();
        // Bounds of the values as stored, after the halves round.
        let (mut lo, mut hi) = ([f32::INFINITY; 3], [f32::NEG_INFINITY; 3]);
        let mut data = Vec::with_capacity(n * layout.stride());
        let same = layout == source && how.uv_rect.is_none();
        let raw = r.bytes(verts.start, verts.len())?;
        for i in 0..n {
            let at = data.len();
            if same {
                data.extend_from_slice(&raw[i * verts.stride..(i + 1) * verts.stride]);
            } else {
                data.resize(at + layout.stride(), 0);
            }
            let mut off = at;
            let mut color_slot = 0;
            for a in &layout.0 {
                let field = &mut data[off..off + a.size()];
                off += a.size();
                let dir = |v: [f32; 3], field: &mut [u8]| {
                    for c in 0..3 {
                        field[c] = ((v[c] + 1.) * 127.5).round().clamp(0., 255.) as u8;
                    }
                };
                match (a.semantic, a.storage, a.count) {
                    (Semantic::Position, Storage::F16 | Storage::F32, 3..) => {
                        for (c, v) in positions[i].into_iter().enumerate() {
                            let stored = match a.storage {
                                Storage::F16 => {
                                    let h = half(v);
                                    field[2 * c..2 * c + 2].copy_from_slice(&h.to_le_bytes());
                                    f16(h)
                                }
                                _ => {
                                    field[4 * c..4 * c + 4].copy_from_slice(&v.to_le_bytes());
                                    v
                                }
                            };
                            lo[c] = lo[c].min(stored);
                            hi[c] = hi[c].max(stored);
                        }
                    }
                    (Semantic::Normal, Storage::U32, 1) => dir(normals[i], field),
                    (Semantic::Tangent, Storage::U32, 1) => {
                        let g = tangents[i];
                        dir([g[0], g[1], g[2]], field)
                    }
                    (Semantic::Bitangent, Storage::U32, 1) => {
                        let g = tangents[i];
                        let b = cross(normals[i], [g[0], g[1], g[2]]).map(|c| c * g[3]);
                        dir(b, field)
                    }
                    (Semantic::Texcoord, Storage::F16 | Storage::F32, 2)
                        if !same || how.uv_rect.is_some() =>
                    {
                        let p = uv(decoded.uvs.get(i).copied().unwrap_or_default());
                        for c in 0..2 {
                            match a.storage {
                                Storage::F16 => field[2 * c..2 * c + 2]
                                    .copy_from_slice(&half(p[c]).to_le_bytes()),
                                _ => field[4 * c..4 * c + 4].copy_from_slice(&p[c].to_le_bytes()),
                            }
                        }
                    }
                    (Semantic::Texcoord, ..) if same => {}
                    (Semantic::Color, Storage::U32, 1) => {
                        if !same {
                            let c = match (color_slot, colors.get(i)) {
                                (0, Some(c)) => *c,
                                _ => how.color_fill.get(color_slot).copied().unwrap_or([255; 4]),
                            };
                            field.copy_from_slice(&c);
                        }
                        color_slot += 1;
                    }
                    _ if same => {}
                    _ => bail!("cannot write {a}"),
                }
            }
        }
        if n > 0 {
            for c in 0..3 {
                out[rec + MeshRecord::MIN + WORD * c..][..WORD]
                    .copy_from_slice(&lo[c].to_le_bytes());
                out[rec + MeshRecord::MAX + WORD * c..][..WORD]
                    .copy_from_slice(&hi[c].to_le_bytes());
            }
        }
        if same {
            out[verts.start..verts.start + verts.len()].copy_from_slice(&data);
            continue;
        }
        // Appends a block (count, stride, payload) and returns its payload
        // offset, which is what pointers name.
        let append = |out: &mut Vec<u8>, count: usize, stride: usize, payload: &[u8]| {
            out.extend((count as u32).to_le_bytes());
            out.extend((stride as u32).to_le_bytes());
            let at = out.len() as u32;
            out.extend_from_slice(payload);
            at
        };
        let descriptors: Vec<u8> = (layout.0.iter())
            .flat_map(|a| [a.semantic.id(), a.storage.code(), a.interpretation, a.count])
            .flat_map(u32::to_le_bytes)
            .collect();
        let ids_at = append(&mut out, layout.0.len(), Descriptor::SIZE, &descriptors);
        let fmt_rec = r.block(r.u32(rec + MeshRecord::FORMAT)? as usize)?;
        let mut record = r.bytes(fmt_rec.start, fmt_rec.len())?.to_vec();
        record[..WORD].copy_from_slice(&ids_at.to_le_bytes());
        let fmt_at = append(&mut out, fmt_rec.count, fmt_rec.stride, &record);
        let verts_at = append(&mut out, n, layout.stride(), &data);
        let mut point = |field: usize, at: u32| {
            out[rec + field..][..WORD].copy_from_slice(&at.to_le_bytes());
        };
        point(MeshRecord::FORMAT, fmt_at);
        point(MeshRecord::VERTICES, verts_at);
    }
    Ok(out)
}

/// Per-vertex tangents from texcoords (Lengyel): each triangle's direction
/// of increasing u, summed per vertex, made perpendicular to the normal,
/// with the handedness of increasing v.
fn tangents_from_uvs(m: &Rdm) -> Vec<[f32; 4]> {
    let n = m.positions.len();
    let (mut tan, mut bit) = (vec![[0f32; 3]; n], vec![[0f32; 3]; n]);
    if m.uvs.len() == n {
        for tri in m.indices.as_chunks::<3>().0 {
            let [a, b, c] = tri.map(|i| i as usize);
            let (p, q, r) = (m.positions[a], m.positions[b], m.positions[c]);
            let (s, t, w) = (m.uvs[a], m.uvs[b], m.uvs[c]);
            let e1 = [0, 1, 2].map(|k| q[k] - p[k]);
            let e2 = [0, 1, 2].map(|k| r[k] - p[k]);
            let (du1, dv1, du2, dv2) = (t[0] - s[0], t[1] - s[1], w[0] - s[0], w[1] - s[1]);
            let det = du1 * dv2 - du2 * dv1;
            if det.abs() < 1e-12 {
                continue;
            }
            let f = 1. / det;
            let sdir = [0, 1, 2].map(|k| (e1[k] * dv2 - e2[k] * dv1) * f);
            let tdir = [0, 1, 2].map(|k| (e2[k] * du1 - e1[k] * du2) * f);
            for v in [a, b, c] {
                for k in 0..3 {
                    tan[v][k] += sdir[k];
                    bit[v][k] += tdir[k];
                }
            }
        }
    }
    (0..n)
        .map(|i| {
            let nrm = m.normals.get(i).copied().unwrap_or([0., 1., 0.]);
            let t = tan[i];
            let d = t[0] * nrm[0] + t[1] * nrm[1] + t[2] * nrm[2];
            let mut g = [0, 1, 2].map(|k| t[k] - nrm[k] * d);
            if g.iter().map(|c| c * c).sum::<f32>() < 1e-12 {
                // No usable texcoords here: any direction across the normal.
                let side = if nrm[0].abs() < 0.9 {
                    [1., 0., 0.]
                } else {
                    [0., 0., 1.]
                };
                g = cross(nrm, side);
            }
            let g = normalize(g);
            let c = cross(nrm, g);
            let w = if c[0] * bit[i][0] + c[1] * bit[i][1] + c[2] * bit[i][2] < 0. {
                -1.
            } else {
                1.
            };
            [g[0], g[1], g[2], w]
        })
        .collect()
}

/// f32 to IEEE half, rounded to nearest; out of range saturates to
/// infinity, tiny values flush to zero (mesh coordinates never get there).
fn half(v: f32) -> u16 {
    let bits = v.to_bits();
    let sign = ((bits >> 16) & 0x8000) as u16;
    let exp = ((bits >> 23) & 0xff) as i32 - 127 + 15;
    if v.is_nan() {
        return sign | 0x7e00;
    }
    if exp >= 31 {
        return sign | 0x7c00;
    }
    if exp <= 0 {
        return sign;
    }
    let man = bits & 0x7f_ffff;
    let h = ((exp as u32) << 10) | (man >> 13);
    // Round half up on the dropped bits; a carry rolls into the exponent,
    // which is the right answer (0x3bff + 1 = 1.0).
    let h = h + ((man >> 12) & 1);
    sign | h.min(0x7c00) as u16
}

/// The [`MeshRecord`] at `meta`.
fn mesh(r: &Reader, meta: usize) -> Result<Rdm> {
    let ptr = |field: usize| r.u32(meta + field).map(|p| p as usize);
    let skeleton = r.u32(meta + MeshRecord::SKELETON)? as i32;
    let bounds = (
        r.vec3(meta + MeshRecord::MIN)?,
        r.vec3(meta + MeshRecord::MAX)?,
    );

    let name = match ptr(MeshRecord::NAME)? {
        0 => String::new(),
        p => r.string(r.u32(p)? as usize)?,
    };

    let (format, _) = vertex_format(r, meta)?;

    let index_storage = match ptr(MeshRecord::INDEX_FORMAT)? {
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

    let verts = r
        .block(ptr(MeshRecord::VERTICES)?)
        .context("vertex buffer")?;
    ensure!(
        verts.stride == format.stride(),
        "vertex stride {} but format {format} is {} bytes",
        verts.stride,
        format.stride()
    );
    // The root's skin flag joins in `parse`.
    let skinned = skeleton >= 0
        || format.find(Semantic::JointIndex).is_some()
        || format.find(Semantic::Weight).is_some();
    let data = r.bytes(verts.start, verts.len())?;
    let mut out = decode_vertices(format, data, verts.count, r.order)?;

    let tris = r.block(ptr(MeshRecord::INDICES)?).context("index buffer")?;
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
            .map(|&c| r.order.u16(c) as u32)
            .collect(),
        _ => data
            .as_chunks::<4>()
            .0
            .iter()
            .map(|&c| r.order.u32(c))
            .collect(),
    };
    let n = out.positions.len() as u32;
    if let Some(bad) = out.indices.iter().find(|&&i| i >= n) {
        bail!("index {bad} out of {n} vertices");
    }

    let groups = r
        .block(ptr(MeshRecord::RANGES)?)
        .context("material ranges")?;
    ensure!(
        groups.stride >= RangeRecord::MIN_STRIDE,
        "mesh info stride {}",
        groups.stride
    );
    for i in 0..groups.count {
        let at = groups.start + i * groups.stride;
        let g = MaterialGroup {
            start: r.u32(at + RangeRecord::START)?,
            count: r.u32(at + RangeRecord::COUNT)?,
            material: r.u32(at + RangeRecord::MATERIAL)?,
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

    // 2070's big-endian meshes store a zero box.
    out.bounds = match bounds.0 == bounds.1 && !out.positions.is_empty() {
        true => out.computed_bounds(),
        false => bounds,
    };
    out.skinned = skinned;
    out.name = name;
    Ok(out)
}

// Decodes `count` vertices of `format` from `data` (tightly packed), each
// scalar in the file's byte order. Fills the vertex arrays and `format` of
// the returned mesh.
fn decode_vertices(
    format: VertexFormat,
    data: &[u8],
    count: usize,
    order: ByteOrder,
) -> Result<Rdm> {
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
    out.positions = verts().map(|v| floats(a, &v[off..], order)).collect();

    if let Some((off, a)) = format.find(Semantic::Normal) {
        out.normals = verts()
            .map(|v| vector(a, &v[off..], order).map(normalize))
            .collect::<Result<_>>()?;
    }
    if let Some((off, a)) = format.find(Semantic::Tangent) {
        let bitangent = format.find(Semantic::Bitangent);
        out.tangents = verts()
            .enumerate()
            .map(|(i, v)| {
                let t = normalize(vector(a, &v[off..], order)?);
                // Handedness: does the stored bitangent agree with N x T?
                let w = match (bitangent, out.normals.get(i)) {
                    (Some((boff, b)), Some(n)) => {
                        let b = vector(b, &v[boff..], order)?;
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
                let f = floats(a, &v[off..], order);
                [f[0], f[1]]
            })
            .collect();
    }
    if let Some((off, a)) = format.find(Semantic::Color) {
        out.colors = Some(match (a.storage, a.count) {
            (Storage::U32, 1) => verts().map(|v| order.packed(&v[off..])).collect(),
            // C4h (a few villa ruin pieces): 0..1 floats.
            (Storage::F16, 4) => verts()
                .map(|v| {
                    [0, 1, 2, 3].map(|k| {
                        let c = f16(order.u16([v[off + 2 * k], v[off + 2 * k + 1]]));
                        (c.clamp(0.0, 1.0) * 255.0).round() as u8
                    })
                })
                .collect(),
            _ => bail!("unsupported color {a}"),
        });
    }
    // Packed bytes like a normal, but read as stored. Another storage would
    // be new; it stays undecoded and the mesh in its bind pose.
    let packed = |semantic| {
        let mut offset = 0;
        let mut sets = Vec::new();
        for a in &format.0 {
            if a.semantic == semantic && a.storage == Storage::U32 && a.count == 1 {
                sets.push(offset);
            }
            offset += a.size();
        }
        sets
    };
    let (joint_sets, weight_sets) = (packed(Semantic::JointIndex), packed(Semantic::Weight));
    match (joint_sets.as_slice(), weight_sets.as_slice()) {
        ([], _) => {}
        ([j], []) => {
            out.joints = verts().map(|v| order.packed(&v[*j..])).collect();
            out.weights = vec![[255, 0, 0, 0]; count];
        }
        ([j], [w, ..]) => {
            out.joints = verts().map(|v| order.packed(&v[*j..])).collect();
            out.weights = verts().map(|v| order.packed(&v[*w..])).collect();
        }
        // 1800's portraits and kraken: four sets, sixteen influences. Keep
        // the four heaviest, weights as stored: their sum drops below 255,
        // which the divide by the sum absorbs.
        (joints, weights) => {
            (out.joints, out.weights) = verts()
                .map(|v| {
                    let mut best = ([0u8; 4], [0u8; 4]);
                    for (&j, &w) in joints.iter().zip(weights) {
                        let (j, w) = (order.packed(&v[j..]), order.packed(&v[w..]));
                        for k in 0..4 {
                            // Insert below the first lighter one.
                            if let Some(at) = (0..4).find(|&i| w[k] > best.1[i]) {
                                best.0.copy_within(at..3, at + 1);
                                best.1.copy_within(at..3, at + 1);
                                (best.0[at], best.1[at]) = (j[k], w[k]);
                            }
                        }
                    }
                    best
                })
                .unzip();
        }
    }
    out.format = format;
    Ok(out)
}

// First three components of a float attribute (P4h, P3f, T2h -> padded).
fn floats(a: Attribute, b: &[u8], order: ByteOrder) -> [f32; 3] {
    let mut out = [0.0; 3];
    for (k, o) in out.iter_mut().enumerate().take(a.count as usize) {
        let half = || order.u16([b[2 * k], b[2 * k + 1]]);
        let word = || order.u32([b[4 * k], b[4 * k + 1], b[4 * k + 2], b[4 * k + 3]]);
        *o = match a.storage {
            Storage::F16 => f16(half()),
            Storage::F32 => f32::from_bits(word()),
            Storage::U16 => half() as f32,
            Storage::U32 => word() as f32,
        };
    }
    out
}

// A direction: packed `4b` bytes map 0..255 to -1..1 (x, y, z, then a
// spare byte), float formats are taken as stored.
fn vector(a: Attribute, b: &[u8], order: ByteOrder) -> Result<[f32; 3]> {
    Ok(match (a.storage, a.count) {
        (Storage::U32, 1) => {
            let b = order.packed(b);
            [0, 1, 2].map(|k| b[k] as f32 * (2.0 / 255.0) - 1.0)
        }
        (Storage::F16 | Storage::F32, 3..) => floats(a, b, order),
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

// ------------------------------------------------------- skeletons and animation
//
// A skeleton is the joints' inverse bind transforms; an animation file holds
// one track of parent-relative keys per joint name. Both store quaternions
// conjugated (see `Reader::quat`); everything below is in the usual
// convention, a rotation q turning v into q v q*.

/// [`Skeleton::parents`] of a root joint.
pub const NO_PARENT: u16 = u16::MAX;

/// The hash cfgs name joints by (`BONELINK_TRANSFORM`'s `BoneHash`): 32-bit
/// FNV-1a over the name's bytes, case kept. For a name a file stores in
/// Latin-1 (not UTF-8: an elephant's `Rüßel_0`) the game hashes those
/// bytes, and [`Skeleton::hashes`] holds that hash, not this one of the
/// UTF-8.
pub fn bone_hash(name: &str) -> u32 {
    fnv1a(name.as_bytes())
}

fn fnv1a(bytes: &[u8]) -> u32 {
    (bytes.iter()).fold(0x811c_9dc5, |h, &b| {
        (h ^ b as u32).wrapping_mul(0x0100_0193)
    })
}

/// A stored name: UTF-8 when it is, else Latin-1, byte for char. 3ds Max
/// exports carry Windows-1252 names: an elephant's trunk bones are
/// `R\xfc\xdfel_0` (`Rüßel_0`). Latin-1 maps every byte, so the name
/// writes back to the same bytes.
fn text(bytes: &[u8]) -> (String, bool) {
    match std::str::from_utf8(bytes) {
        Ok(s) => (s.to_owned(), false),
        Err(_) => (bytes.iter().map(|&b| b as char).collect(), true),
    }
}

/// A rigid transform: rotate, then translate. Joints carry no scale.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct JointPose {
    /// Unit quaternion `[x, y, z, w]`.
    pub rotation: [f32; 4],
    pub translation: [f32; 3],
}

impl Default for JointPose {
    fn default() -> JointPose {
        JointPose::IDENTITY
    }
}

impl JointPose {
    pub const IDENTITY: JointPose = JointPose {
        rotation: [0., 0., 0., 1.],
        translation: [0.; 3],
    };

    /// `child` placed under `self`: for a parent's model-space transform and
    /// a child's parent-relative one, the child's model-space transform.
    pub fn mul(&self, child: &JointPose) -> JointPose {
        let t = self.apply(child.translation);
        JointPose {
            rotation: quat_mul(self.rotation, child.rotation),
            translation: t,
        }
    }

    pub fn inverse(&self) -> JointPose {
        let rotation = quat_conj(self.rotation);
        JointPose {
            rotation,
            translation: quat_rotate(rotation, self.translation).map(|c| -c),
        }
    }

    pub fn apply(&self, p: [f32; 3]) -> [f32; 3] {
        let r = quat_rotate(self.rotation, p);
        [0, 1, 2].map(|k| r[k] + self.translation[k])
    }

    /// Column-major 4x4: `m[col][row]`, as `glam::Mat4::from_cols_array_2d`
    /// and WGSL's `mat4x4<f32>` take it.
    pub fn matrix(&self) -> [[f32; 4]; 4] {
        let [x, y, z, w] = self.rotation;
        let t = self.translation;
        [
            [
                1. - 2. * (y * y + z * z),
                2. * (x * y + w * z),
                2. * (x * z - w * y),
                0.,
            ],
            [
                2. * (x * y - w * z),
                1. - 2. * (x * x + z * z),
                2. * (y * z + w * x),
                0.,
            ],
            [
                2. * (x * z + w * y),
                2. * (y * z - w * x),
                1. - 2. * (x * x + y * y),
                0.,
            ],
            [t[0], t[1], t[2], 1.],
        ]
    }
}

/// Where entry `i` of a back-to-back list ends and starts: `ends[i]` is its
/// end, the previous entry's end its start.
fn span(ends: &[u32], i: usize) -> std::ops::Range<usize> {
    let start = match i {
        0 => 0,
        i => ends[i - 1] as usize,
    };
    start..ends[i] as usize
}

/// A mesh's joints as flat arrays, one entry per joint, parents before
/// children (every shipped skeleton is stored that way; the reader checks).
/// Vertex [`Rdm::joints`] index these arrays.
///
/// A file may hold several skeleton records, one per mesh that uses its own:
/// a 117 horse's body, tail and mane each carry a copy of the rig, in their
/// own joint order. Each record a mesh uses is a part here, its joints
/// after the parts before it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Skeleton {
    /// Joint names back to back; `name_ends[j]` is where joint j's ends.
    names: String,
    name_ends: Vec<u32>,
    /// [`bone_hash`] of each name. Animation tracks and cfg bone links
    /// match joints by it.
    pub hashes: Vec<u32>,
    /// The parent's index, or [`NO_PARENT`].
    pub parents: Vec<u16>,
    /// Model-space bind pose: the space the mesh's positions are in.
    pub bind: Vec<JointPose>,
    /// The bind pose relative to the parent. Joints an animation has no
    /// track for keep it.
    pub bind_local: Vec<JointPose>,
    /// Model space to joint space, as stored: `bind[j].inverse()`.
    pub inverse_bind: Vec<JointPose>,
    /// Part p's joints end at `part_ends[p]`.
    pub part_ends: Vec<u32>,
    /// [`bone_hash`] of the name of the mesh that first used each part. An
    /// animation record of the same name drives the part.
    part_meshes: Vec<u32>,
}

impl Skeleton {
    pub fn len(&self) -> usize {
        self.parents.len()
    }

    pub fn is_empty(&self) -> bool {
        self.parents.is_empty()
    }

    pub fn name(&self, joint: usize) -> &str {
        &self.names[span(&self.name_ends, joint)]
    }

    /// The first joint with this [`bone_hash`].
    pub fn find_hash(&self, hash: u32) -> Option<usize> {
        self.hashes.iter().position(|&h| h == hash)
    }

    pub fn find(&self, name: &str) -> Option<usize> {
        (0..self.len()).find(|&j| self.name(j) == name)
    }

    /// Turns parent-relative transforms (an [`Animation::sample`]) into
    /// model-space ones, in place: parents come first, so each parent is
    /// already in model space when its children need it.
    pub fn to_model(&self, pose: &mut [JointPose]) {
        for j in 0..self.len().min(pose.len()) {
            let p = self.parents[j];
            if p != NO_PARENT {
                pose[j] = pose[p as usize].mul(&pose[j]);
            }
        }
    }

    /// Skinning matrices from model-space joint transforms: model times
    /// inverse bind, column-major ([`JointPose::matrix`]). A bind-pose
    /// vertex `p` with joints `i` and weights `w` lands at
    /// `sum(w[k] * out[i[k]] * p) / sum(w)`.
    pub fn skinning_matrices(&self, model: &[JointPose], out: &mut [[[f32; 4]; 4]]) {
        for ((o, m), ib) in out.iter_mut().zip(model).zip(&self.inverse_bind) {
            *o = m.mul(ib).matrix();
        }
    }

    /// Appends skeleton record `index` of the block at `skin` as a part,
    /// first used by the mesh named `mesh`. Returns its first joint.
    fn read_part(&mut self, r: &Reader, skin: usize, index: usize, mesh: &str) -> Result<usize> {
        let skeletons = r.block(skin)?;
        ensure!(
            skeletons.stride >= SkeletonRecord::MIN_STRIDE && index < skeletons.count,
            "skeleton {index} of {} ({} bytes each)",
            skeletons.count,
            skeletons.stride
        );
        let at = skeletons.start + index * skeletons.stride;
        let joints = r.block(r.u32(at + SkeletonRecord::JOINTS)? as usize)?;
        ensure!(
            joints.stride >= JointRecord::MIN_STRIDE,
            "joint stride {}",
            joints.stride
        );
        let first = self.len();
        ensure!(
            first + joints.count < NO_PARENT as usize,
            "{} joints",
            first + joints.count
        );
        for j in 0..joints.count {
            let at = joints.start + j * joints.stride;
            let raw = r.raw_string(r.u32(at + JointRecord::NAME)? as usize)?;
            let name = text(raw).0;
            let parent = match r.u32(at + JointRecord::PARENT)? {
                u32::MAX => NO_PARENT,
                p => {
                    ensure!((p as usize) < j, "joint {j} ({name}) has parent {p}");
                    (first + p as usize) as u16
                }
            };
            let inverse = JointPose {
                rotation: r.quat(at + JointRecord::ROTATION)?,
                translation: r.vec3(at + JointRecord::TRANSLATION)?,
            };
            let bind = inverse.inverse();
            self.hashes.push(fnv1a(raw));
            self.names.push_str(&name);
            self.name_ends.push(self.names.len() as u32);
            self.parents.push(parent);
            self.bind_local.push(match parent {
                NO_PARENT => bind,
                p => self.inverse_bind[p as usize].mul(&bind),
            });
            self.inverse_bind.push(inverse);
            self.bind.push(bind);
        }
        self.part_ends.push(self.len() as u32);
        self.part_meshes.push(bone_hash(mesh));
        Ok(first)
    }
}

/// An animation-only rdm: per joint name, a track of parent-relative keys,
/// interpolated linearly (rotations by slerp). Tracks keep only the keys the
/// exporter's reduction left, so their counts differ.
///
/// Tracks come in records named after the mesh they drive (often unnamed,
/// one record). Several records occur where several meshes have their own
/// skeleton (a ship's hull and sail) or where a prop moves along (a
/// checklist animation's `notepad` and `pen`). The game loads the first
/// record only (docs/model-animation.md, Which record drives which mesh).
///
/// [`Animation::encode`] writes the file back; what parse read and nobody
/// changed keeps its bytes. The edit calls ([`Animation::set_key`],
/// [`Animation::remove_key`], [`Animation::add_track`],
/// [`Animation::set_duration`]) keep each track's keys in time order.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Animation {
    /// Seconds, the longest record's. A cfg sequence's `EndTime` for a full
    /// play is this in ms. The file stores whole milliseconds: change it
    /// with [`Animation::set_duration`], which also cuts the keys past a
    /// shorter end. Set directly, [`Animation::encode`] writes it to every
    /// record, rounded to the millisecond.
    pub duration: f32,
    /// Keys per second the exporter sampled at before reduction (15 in
    /// most files, 30 in a sixth of 1800's). Informational: keys carry
    /// their own times. Changed, [`Animation::encode`] writes it to every
    /// record.
    pub sample_rate: u32,
    /// Track names back to back; `name_ends[k]` is where track k's ends.
    names: String,
    name_ends: Vec<u32>,
    /// [`bone_hash`] of each track's joint name.
    hashes: Vec<u32>,
    /// Track k's keys are `span(key_ends, k)` of the key arrays.
    key_ends: Vec<u32>,
    /// Key times in seconds, ascending within a track.
    times: Vec<f32>,
    /// Parent-relative rotations, conjugated back from the file's order but
    /// not renormalized, so that they encode to the bytes they came from.
    /// Shipped keys are unit to f32 rounding; [`Animation::sample`]
    /// normalizes what it returns.
    rotations: Vec<[f32; 4]>,
    /// Parent-relative translations.
    translations: Vec<[f32; 3]>,
    /// Record r's tracks are `span(record_ends, r)`.
    record_ends: Vec<u32>,
    /// [`bone_hash`] of each record's mesh name (`""` when unnamed).
    record_meshes: Vec<u32>,
    /// Record mesh names back to back; `record_name_ends[r]` is where
    /// record r's ends.
    record_names: String,
    record_name_ends: Vec<u32>,
    /// Each record's duration and sample rate as stored. 1800 files with
    /// several records give them different durations.
    record_ms: Vec<u32>,
    record_rates: Vec<u32>,
    /// The file before the animation records: preamble, root record,
    /// export info and its strings, kept verbatim (the exporter's source
    /// path and command line; nothing the game needs from an animation).
    /// Empty for an animation built in memory; encode then writes a bare
    /// preamble and root.
    head: Vec<u8>,
    order: ByteOrder,
    /// Some name was not UTF-8 and read as Latin-1 ([`text`]): names go
    /// back as Latin-1.
    latin1: bool,
    /// What the file holds after the animation's blocks, verbatim, and
    /// where it started.
    tail: Vec<u8>,
    tail_at: u32,
}

/// The exporter's sample rate in 2,261 of 117's 2,286 animations, 1,891 of
/// 1800's 2,338 and 720 of 2070's 1,056 (`/AnimSampleRate 15`).
pub const SHIPPED_RATE: u32 = 15;

/// Two keys closer than this are the same key to [`Animation::set_key`]:
/// half the file's millisecond resolution for durations. Exported keys sit
/// 1/15 or 1/30 s apart.
pub const KEY_EPSILON: f32 = 0.0005;

impl Animation {
    /// Reads an animation-only rdm (the files [`Rdm::parse`] refuses with
    /// [`AnimationOnly`]).
    pub fn parse(bytes: &[u8]) -> Result<Animation> {
        let r = Reader::new(bytes)?;
        let root = r.root()?;
        let at = r.u32(root + RootRecord::ANIM)? as usize;
        ensure!(at != 0, "rdm holds no animation");
        let meta = r.block(at)?;
        ensure!(
            meta.stride >= AnimRecord::MIN_STRIDE,
            "animation record of {} bytes",
            meta.stride
        );
        let mut a = Animation {
            order: r.order,
            ..Animation::default()
        };
        // Exporters write the records after everything else the root
        // points at, so the bytes before them are the rest of the file. A
        // root stored after them (none ships) gets a fresh head on encode.
        let head = at - BlockHead::COUNT;
        if root + RootRecord::ANIM + WORD <= head {
            a.head = bytes[..head].to_vec();
        }
        // Where the animation's blocks end, for what follows them.
        let mut end = meta.start + meta.len();
        for rec in 0..meta.count {
            let at = meta.start + rec * meta.stride;
            let ms = r.u32(at + AnimRecord::DURATION_MS)?;
            a.record_ms.push(ms);
            a.record_rates.push(r.u32(at + AnimRecord::SAMPLE_RATE)?);
            let p = r.u32(at + AnimRecord::MESH)? as usize;
            let raw = r.raw_string(p)?;
            end = end.max(p + raw.len());
            let (mesh, latin1) = text(raw);
            a.latin1 |= latin1;
            a.record_meshes.push(bone_hash(&mesh));
            a.record_names.push_str(&mesh);
            a.record_name_ends.push(a.record_names.len() as u32);
            if let p @ 1.. = r.u32(at + AnimRecord::TRACKS)? {
                let tracks = r.block(p as usize).context("animation tracks")?;
                ensure!(
                    tracks.stride >= TrackRecord::MIN_STRIDE,
                    "track stride {}",
                    tracks.stride
                );
                end = end.max(tracks.start + tracks.len());
                for k in 0..tracks.count {
                    let e = a.read_track(&r, tracks.start + k * tracks.stride)?;
                    end = end.max(e);
                }
            }
            a.record_ends.push(a.hashes.len() as u32);
        }
        // 117's haystack keeps the four floats its root's word 7 names
        // after the keys. They go back after the keys, and root words
        // that pointed there follow them.
        if !a.head.is_empty() && end < bytes.len() {
            a.tail = bytes[end..].to_vec();
            a.tail_at = end as u32;
        }
        a.duration = a.stored_duration();
        a.sample_rate = a.record_rates.first().copied().unwrap_or(0);
        Ok(a)
    }

    /// Reads the track record at `at`. Returns where its blocks end.
    fn read_track(&mut self, r: &Reader, at: usize) -> Result<usize> {
        let p = r.u32(at + TrackRecord::NAME)? as usize;
        let raw = r.raw_string(p)?;
        let (name, latin1) = text(raw);
        self.latin1 |= latin1;
        let keys = r
            .block(r.u32(at + TrackRecord::KEYS)? as usize)
            .with_context(|| format!("keys of {name}"))?;
        ensure!(
            keys.stride >= KeyRecord::MIN_STRIDE,
            "key stride {}",
            keys.stride
        );
        let mut last = f32::NEG_INFINITY;
        for i in 0..keys.count {
            let at = keys.start + i * keys.stride;
            let time = r.f32(at + KeyRecord::TIME)?;
            ensure!(time >= last, "{name}: key {i} at {time} s goes back");
            last = time;
            self.times.push(time);
            self.rotations
                .push(quat_conj(r.vec4(at + KeyRecord::ROTATION)?));
            self.translations.push(r.vec3(at + KeyRecord::TRANSLATION)?);
        }
        self.hashes.push(fnv1a(raw));
        self.names.push_str(&name);
        self.name_ends.push(self.names.len() as u32);
        self.key_ends.push(self.times.len() as u32);
        Ok((p + raw.len()).max(keys.start + keys.len()))
    }

    /// The duration the records store, in seconds: the longest.
    fn stored_duration(&self) -> f32 {
        self.record_ms.iter().copied().max().unwrap_or(0) as f32 / 1000.
    }

    /// The file: [`Animation::parse`]'s input again when nothing changed
    /// (every shipped animation of 117, 1800 and 2070 comes back byte for
    /// byte), in the byte order it was read in. Blocks go where the
    /// exporter puts them: the records, then per record its mesh name and
    /// track block, then per track its name and keys. Edited keys are
    /// written as given; f32 throughout, so nothing quantizes.
    pub fn encode(&self) -> Result<Vec<u8>> {
        let keys = self.times.len();
        let mut w = Writer {
            bytes: Vec::with_capacity(
                self.head.len()
                    + self.records() * AnimRecord::STRIDE
                    + self.tracks() * (TrackRecord::STRIDE + 3 * BlockHead::COUNT)
                    + self.names.len()
                    + keys * KeyRecord::STRIDE,
            ),
            order: self.order,
        };
        let root = match self.head.is_empty() {
            true => w.head(),
            false => {
                w.bytes.extend_from_slice(&self.head);
                Reader::new(&self.head)?.u32(Preamble::ROOT)? as usize
            }
        };
        let records = w.block(self.records(), AnimRecord::STRIDE);
        w.u32(root + RootRecord::ANIM, records);
        // A duration or rate set on the struct overrides every record's.
        let ms = |r: usize| match self.duration == self.stored_duration() {
            true => self.record_ms[r],
            false => (self.duration * 1000.).round() as u32,
        };
        let rate = |r: usize| match Some(&self.sample_rate) == self.record_rates.first() {
            true => self.record_rates[r],
            false => self.sample_rate,
        };
        for r in 0..self.records() {
            let at = records + r * AnimRecord::STRIDE;
            let mesh = &self.record_names[span(&self.record_name_ends, r)];
            if !mesh.is_empty() {
                let p = w.string(mesh, self.latin1);
                w.u32(at + AnimRecord::MESH, p);
            }
            let tracks = span(&self.record_ends, r);
            let block = w.block(tracks.len(), TrackRecord::STRIDE);
            w.u32(at + AnimRecord::TRACKS, block);
            w.u32(at + AnimRecord::DURATION_MS, ms(r) as usize);
            w.u32(at + AnimRecord::SAMPLE_RATE, rate(r) as usize);
            for (i, k) in tracks.enumerate() {
                let at = block + i * TrackRecord::STRIDE;
                let p = w.string(self.track_name(k), self.latin1);
                w.u32(at + TrackRecord::NAME, p);
                let (times, rotations, translations) = self.keys(k);
                let p = w.block(times.len(), KeyRecord::STRIDE);
                w.u32(at + TrackRecord::KEYS, p);
                for n in 0..times.len() {
                    let at = p + n * KeyRecord::STRIDE;
                    w.f32s(at + KeyRecord::ROTATION, &quat_conj(rotations[n]));
                    w.f32s(at + KeyRecord::TRANSLATION, &translations[n]);
                    w.f32s(at + KeyRecord::TIME, &[times[n]]);
                }
            }
        }
        if !self.tail.is_empty() {
            // Root words that pointed into the tail move with it.
            let moved = w.bytes.len();
            w.bytes.extend_from_slice(&self.tail);
            let r = Reader::new(&self.head)?;
            let stride = r.u32(root - BlockHead::STRIDE)? as usize;
            let tail = self.tail_at as usize..self.tail_at as usize + self.tail.len();
            for word in (0..stride / WORD).filter(|&k| k * WORD != RootRecord::ANIM) {
                let p = r.u32(root + word * WORD)? as usize;
                if tail.contains(&p) {
                    w.u32(root + word * WORD, p - tail.start + moved);
                }
            }
        }
        ensure!(
            w.bytes.len() <= u32::MAX as usize,
            "animation of {} bytes",
            w.bytes.len()
        );
        Ok(w.bytes)
    }

    pub fn tracks(&self) -> usize {
        self.hashes.len()
    }

    pub fn track_name(&self, track: usize) -> &str {
        &self.names[span(&self.name_ends, track)]
    }

    /// [`bone_hash`] of track `track`'s joint name.
    pub fn track_hash(&self, track: usize) -> u32 {
        self.hashes[track]
    }

    /// The first track for the joint of this [`bone_hash`], in record
    /// order. Where records hold the same joint twice (a ship's hull and
    /// sail), [`Animation::track_for`] picks the one that plays.
    pub fn find_track(&self, hash: u32) -> Option<usize> {
        self.hashes.iter().position(|&h| h == hash)
    }

    /// Track `track`'s keys: times (seconds), rotations, translations.
    pub fn keys(&self, track: usize) -> (&[f32], &[[f32; 4]], &[[f32; 3]]) {
        let range = span(&self.key_ends, track);
        (
            &self.times[range.clone()],
            &self.rotations[range.clone()],
            &self.translations[range],
        )
    }

    fn records(&self) -> usize {
        self.record_ends.len()
    }

    /// The record that drives `skeleton`'s part `part`. The game loads the
    /// first record alone and binds it to the model's first skeleton
    /// (`sub_1421A5E50`, `sub_1421A6850`), so part 0 takes record 0. Later
    /// parts (a few multi-skeleton files) are not traced: the record named
    /// like the part's mesh, else the first.
    fn part_record(&self, skeleton: &Skeleton, part: usize) -> usize {
        let mesh = skeleton.part_meshes[part];
        match part {
            0 => 0,
            _ => (self.record_meshes.iter())
                .position(|&m| m == mesh)
                .unwrap_or(0),
        }
    }

    fn part_tracks(&self, skeleton: &Skeleton, part: usize) -> std::ops::Range<usize> {
        match self.record_ends.is_empty() {
            true => 0..0,
            false => span(&self.record_ends, self.part_record(skeleton, part)),
        }
    }

    /// The track of joint hash `h` among `own` (the joint's record), trying
    /// `guess` first. Part 0 stops there, as the game binds names in the
    /// first record only; later parts also look in any record.
    fn pick(
        &self,
        part: usize,
        own: std::ops::Range<usize>,
        guess: usize,
        h: u32,
    ) -> Option<usize> {
        match self.hashes.get(guess) {
            Some(&g) if g == h && own.contains(&guess) => Some(guess),
            _ => (own.clone().find(|&k| self.hashes[k] == h))
                .or_else(|| (part > 0).then(|| self.find_track(h)).flatten()),
        }
    }

    /// The track that drives `joint` of `skeleton`, as
    /// [`Animation::sample`] picks it, or `None` when the joint keeps its
    /// bind pose.
    pub fn track_for(&self, skeleton: &Skeleton, joint: usize) -> Option<usize> {
        let part = skeleton.part_ends.partition_point(|&e| e as usize <= joint);
        if part >= skeleton.part_ends.len() {
            return None;
        }
        let own = self.part_tracks(skeleton, part);
        let first = span(&skeleton.part_ends, part).start;
        self.pick(
            part,
            own.clone(),
            own.start + joint - first,
            skeleton.hashes[joint],
        )
    }

    /// The parent-relative pose at `t` seconds, one entry per joint of
    /// `skeleton` (`out` may be longer; the rest is left alone). Each joint
    /// of the first part takes the track of its name in the first record,
    /// as the game binds it; later parts take [`Animation::part_record`]'s
    /// record, else any record. A joint without a track keeps its bind
    /// pose. The game's shader gives such a joint the identity instead,
    /// which would turn the DLC02 quadriga ponies' root a quarter over, so
    /// that reading is not followed until it is checked in game
    /// (docs/model-animation.md). `t` outside the keys holds the first or
    /// last key: whether to wrap is the caller's (a cfg sequence's
    /// `Looped`). Allocates nothing.
    pub fn sample(&self, t: f32, skeleton: &Skeleton, out: &mut [JointPose]) {
        let n = skeleton.len().min(out.len());
        for part in 0..skeleton.part_ends.len() {
            let joints = span(&skeleton.part_ends, part);
            let own = self.part_tracks(skeleton, part);
            for j in joints.clone().take_while(|&j| j < n) {
                // Exporters write tracks in joint order, so try the joint's
                // own index before searching.
                let guess = own.start + (j - joints.start);
                let track = self.pick(part, own.clone(), guess, skeleton.hashes[j]);
                out[j] = (track.and_then(|k| self.pose_at(k, t))).unwrap_or(skeleton.bind_local[j]);
            }
        }
    }

    /// Track `k` at `t`, unless it has no keys (none ship that way).
    pub fn pose_at(&self, k: usize, t: f32) -> Option<JointPose> {
        let (times, rotations, translations) = self.keys(k);
        if times.is_empty() {
            return None;
        }
        // The first key after t, and the one before it.
        let b = times.partition_point(|&x| x <= t).min(times.len() - 1);
        let a = b.saturating_sub(1);
        let span = times[b] - times[a];
        let f = match span > 0. {
            true => ((t - times[a]) / span).clamp(0., 1.),
            false => 0.,
        };
        let (ta, tb) = (translations[a], translations[b]);
        Some(JointPose {
            rotation: quat_slerp(rotations[a], rotations[b], f),
            translation: [0, 1, 2].map(|c| ta[c] + (tb[c] - ta[c]) * f),
        })
    }

    /// Keys `pose` on track `track` at `t` seconds (clamped to 0): replaces
    /// the key within [`KEY_EPSILON`] of `t` and moves it to `t`, else
    /// inserts one in time order. A key past the end lengthens the
    /// duration. Returns the key's index within the track.
    pub fn set_key(&mut self, track: usize, t: f32, pose: JointPose) -> usize {
        let t = t.max(0.);
        let range = span(&self.key_ends, track);
        let times = &self.times[range.clone()];
        let i = times.partition_point(|&x| x < t - KEY_EPSILON);
        let at = range.start + i;
        let rotation = quat_normalize(pose.rotation);
        if i < times.len() && times[i] - t <= KEY_EPSILON {
            // Moved to t, so sampling at t reads the key; never past the
            // next key (two keys within a millisecond).
            let next = times.get(i + 1).copied().unwrap_or(f32::INFINITY);
            self.times[at] = t.min(next);
            self.rotations[at] = rotation;
            self.translations[at] = pose.translation;
        } else {
            self.times.insert(at, t);
            self.rotations.insert(at, rotation);
            self.translations.insert(at, pose.translation);
            for end in &mut self.key_ends[track..] {
                *end += 1;
            }
        }
        if t > self.duration {
            self.duration = t;
        }
        i
    }

    /// Removes key `key` of track `track`. Removing a track's last key
    /// removes the track, so no empty track reaches a file (none ship, and
    /// the game's handling of one is unknown); the joint then keeps its
    /// bind pose and later tracks move down one. Returns whether the track
    /// went.
    pub fn remove_key(&mut self, track: usize, key: usize) -> bool {
        let range = span(&self.key_ends, track);
        assert!(key < range.len(), "key {key} of {}", range.len());
        if range.len() == 1 {
            self.remove_track(track);
            return true;
        }
        let at = range.start + key;
        self.times.remove(at);
        self.rotations.remove(at);
        self.translations.remove(at);
        for end in &mut self.key_ends[track..] {
            *end -= 1;
        }
        false
    }

    fn remove_track(&mut self, track: usize) {
        let keys = span(&self.key_ends, track);
        let n = keys.len() as u32;
        self.times.drain(keys.clone());
        self.rotations.drain(keys.clone());
        self.translations.drain(keys);
        let name = span(&self.name_ends, track);
        let len = name.len() as u32;
        self.names.replace_range(name, "");
        self.name_ends.remove(track);
        self.key_ends.remove(track);
        self.hashes.remove(track);
        for end in &mut self.key_ends[track..] {
            *end -= n;
        }
        for end in &mut self.name_ends[track..] {
            *end -= len;
        }
        for end in self.record_ends.iter_mut().filter(|e| **e as usize > track) {
            *end -= 1;
        }
    }

    /// Adds a track for `skeleton`'s joint `joint` with one key at 0, its
    /// bind pose, to the record that drives the joint's part (see
    /// [`Animation::sample`]; a new unnamed record when there is none). One
    /// key holds still for the whole animation. Returns the new track's
    /// index; tracks after it move up one.
    pub fn add_track(&mut self, skeleton: &Skeleton, joint: usize) -> usize {
        let part = skeleton.part_ends.partition_point(|&e| e as usize <= joint);
        if self.record_ends.is_empty() {
            self.record_ends.push(self.tracks() as u32);
            self.record_meshes.push(bone_hash(""));
            self.record_name_ends.push(self.record_names.len() as u32);
            self.record_ms.push((self.duration * 1000.).round() as u32);
            self.record_rates.push(self.sample_rate);
        }
        let record = self.part_record(skeleton, part);
        let track = self.record_ends[record] as usize;
        let name = skeleton.name(joint);
        let name_at = match track {
            0 => 0,
            k => self.name_ends[k - 1],
        };
        self.names.insert_str(name_at as usize, name);
        self.name_ends.insert(track, name_at);
        for end in &mut self.name_ends[track..] {
            *end += name.len() as u32;
        }
        let key_at = match track {
            0 => 0,
            k => self.key_ends[k - 1],
        };
        self.key_ends.insert(track, key_at);
        for end in &mut self.key_ends[track..] {
            *end += 1;
        }
        let bind = skeleton.bind_local[joint];
        self.times.insert(key_at as usize, 0.);
        self.rotations.insert(key_at as usize, bind.rotation);
        self.translations.insert(key_at as usize, bind.translation);
        self.hashes.insert(track, skeleton.hashes[joint]);
        for end in &mut self.record_ends[record..] {
            *end += 1;
        }
        track
    }

    /// A new animation of `seconds` (rounded to the millisecond the file
    /// stores) that holds `skeleton`'s bind pose: one unnamed record with a
    /// track per joint, keyed at the start and the end, as the exporter
    /// leaves a joint that does not move (2 keys in 15,731 of 117's 15,800
    /// still tracks), at the commonest sample rate. Every joint gets its
    /// track, so nothing rests on how the game treats an untracked joint
    /// ([`Animation::sample`]).
    pub fn bind_pose(skeleton: &Skeleton, seconds: f32) -> Animation {
        let end = (seconds.max(0.001) * 1000.).round() / 1000.;
        let mut a = Animation {
            duration: end,
            sample_rate: SHIPPED_RATE,
            ..Animation::default()
        };
        for j in 0..skeleton.len() {
            // Parts that copy a rig (a horse's tail and mane) share names:
            // one track serves every copy.
            if a.find_track(skeleton.hashes[j]).is_some() {
                continue;
            }
            let track = a.add_track(skeleton, j);
            a.set_key(track, end, skeleton.bind_local[j]);
        }
        a
    }

    /// Sets the duration of every record to `seconds`, rounded to the
    /// millisecond the file stores. A shorter animation is cut there: each
    /// track that runs past gets a key at the new end with the pose it had
    /// there, and loses the keys after. A longer one holds each track's
    /// last key to the new end.
    pub fn set_duration(&mut self, seconds: f32) {
        let ms = (seconds.max(0.) * 1000.).round() as u32;
        let end = ms as f32 / 1000.;
        // Compact in place: the write position never passes the read
        // position, since a cut track drops at least the key past the end
        // for the one it gains.
        let (mut w, mut read) = (0, 0);
        for k in 0..self.tracks() {
            let keys = read..self.key_ends[k] as usize;
            read = keys.end;
            let kept = self.times[keys.clone()].partition_point(|&x| x <= end + KEY_EPSILON);
            let cut = (kept < keys.len()).then(|| self.pose_at(k, end)).flatten();
            self.times.copy_within(keys.start..keys.start + kept, w);
            self.rotations.copy_within(keys.start..keys.start + kept, w);
            self.translations
                .copy_within(keys.start..keys.start + kept, w);
            w += kept;
            if let Some(pose) = cut {
                let on_end = kept > 0 && (self.times[w - 1] - end).abs() <= KEY_EPSILON;
                if !on_end {
                    self.times[w] = end;
                    self.rotations[w] = pose.rotation;
                    self.translations[w] = pose.translation;
                    w += 1;
                }
            }
            self.key_ends[k] = w as u32;
        }
        self.times.truncate(w);
        self.rotations.truncate(w);
        self.translations.truncate(w);
        self.record_ms.fill(ms);
        self.duration = end;
    }
}

/// Appends blocks the way the exporter lays them out: count and stride,
/// then the payload, zeroed until filled. Pointers are payload offsets.
struct Writer {
    bytes: Vec<u8>,
    order: ByteOrder,
}

impl Writer {
    /// The preamble and an empty 48-byte root record, for an animation
    /// with no head of its own. Returns the root's start.
    fn head(&mut self) -> usize {
        self.bytes.extend_from_slice(MAGIC);
        self.bytes.push(match self.order {
            ByteOrder::Big => 0,
            ByteOrder::Little => 1,
        });
        self.bytes.resize(Preamble::LEN - BlockHead::COUNT, 0);
        self.u32(Preamble::SIZE_AT, Preamble::SIZE as usize);
        // Every shipped animation has a 4 here; what it means is unknown.
        self.u32(Preamble::SIZE_AT + 2 * WORD, 4);
        let root = self.block(1, 48);
        self.u32(Preamble::ROOT, root);
        root
    }

    fn block(&mut self, count: usize, stride: usize) -> usize {
        for v in [count, stride] {
            let b = self.order.u32_bytes(v as u32);
            self.bytes.extend_from_slice(&b);
        }
        let at = self.bytes.len();
        self.bytes.resize(at + count * stride, 0);
        at
    }

    /// `latin1`: as [`text`] read it, a byte per char where every char
    /// fits one.
    fn string(&mut self, s: &str, latin1: bool) -> usize {
        match latin1 && s.chars().all(|c| (c as u32) < 0x100) {
            true => {
                let at = self.block(s.chars().count(), 1);
                for (b, c) in self.bytes[at..].iter_mut().zip(s.chars()) {
                    *b = c as u8;
                }
                at
            }
            false => {
                let at = self.block(s.len(), 1);
                self.bytes[at..].copy_from_slice(s.as_bytes());
                at
            }
        }
    }

    fn u32(&mut self, at: usize, v: usize) {
        let b = self.order.u32_bytes(v as u32);
        self.bytes[at..at + WORD].copy_from_slice(&b);
    }

    fn f32s(&mut self, at: usize, v: &[f32]) {
        for (i, f) in v.iter().enumerate() {
            let b = self.order.u32_bytes(f.to_bits());
            self.bytes[at + i * WORD..at + (i + 1) * WORD].copy_from_slice(&b);
        }
    }
}

fn quat_mul(a: [f32; 4], b: [f32; 4]) -> [f32; 4] {
    let [ax, ay, az, aw] = a;
    let [bx, by, bz, bw] = b;
    [
        aw * bx + ax * bw + ay * bz - az * by,
        aw * by - ax * bz + ay * bw + az * bx,
        aw * bz + ax * by - ay * bx + az * bw,
        aw * bw - ax * bx - ay * by - az * bz,
    ]
}

fn quat_conj(q: [f32; 4]) -> [f32; 4] {
    [-q[0], -q[1], -q[2], q[3]]
}

/// q v q*, expanded: v + 2w (u x v) + 2 u x (u x v), u = q.xyz.
fn quat_rotate(q: [f32; 4], v: [f32; 3]) -> [f32; 3] {
    let u = [q[0], q[1], q[2]];
    let c = cross(u, v).map(|x| 2. * x);
    let d = cross(u, c);
    [0, 1, 2].map(|k| v[k] + q[3] * c[k] + d[k])
}

fn quat_normalize(q: [f32; 4]) -> [f32; 4] {
    let len = q.iter().map(|c| c * c).sum::<f32>().sqrt();
    // A zero (or NaN) quaternion would be an exporter bug; hold still.
    if len.is_nan() || len <= 1e-6 {
        return [0., 0., 0., 1.];
    }
    q.map(|c| c / len)
}

/// Shortest-path slerp; nearly equal keys (most of them, 15 per second)
/// interpolate linearly.
fn quat_slerp(a: [f32; 4], b: [f32; 4], f: f32) -> [f32; 4] {
    let mut d: f32 = (0..4).map(|k| a[k] * b[k]).sum();
    let b = match d < 0. {
        true => {
            d = -d;
            b.map(|c| -c)
        }
        false => b,
    };
    let (wa, wb) = match d > 0.9995 {
        true => (1. - f, f),
        false => {
            let theta = d.acos();
            let s = theta.sin();
            (((1. - f) * theta).sin() / s, (f * theta).sin() / s)
        }
    };
    quat_normalize([0, 1, 2, 3].map(|k| a[k] * wa + b[k] * wb))
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

// Bounds-checked access in the file's byte order, read once from its magic.
struct Reader<'a> {
    bytes: &'a [u8],
    order: ByteOrder,
}

impl<'a> Reader<'a> {
    fn new(bytes: &'a [u8]) -> Result<Reader<'a>> {
        ensure!(
            bytes.len() >= Preamble::LEN && bytes.starts_with(MAGIC),
            "not an rdm"
        );
        let byte = bytes[Preamble::ORDER];
        let order = ByteOrder::of(byte).with_context(|| format!("rdm byte order {byte}"))?;
        let r = Reader { bytes, order };
        ensure!(
            r.u32(Preamble::SIZE_AT)? == Preamble::SIZE,
            "bad rdm preamble"
        );
        Ok(r)
    }

    /// The root record's start. Its payload is normally at [`Preamble::LEN`],
    /// but a few hand-made files (ui/placeholder/rdm/anchor.rdm) store it
    /// last; follow the pointer.
    fn root(&self) -> Result<usize> {
        let root = self.block(self.u32(Preamble::ROOT)? as usize)?;
        ensure!(
            root.count == 1 && root.stride >= RootRecord::MIN_STRIDE,
            "bad rdm root record"
        );
        Ok(root.start)
    }

    fn bytes(&self, at: usize, len: usize) -> Result<&'a [u8]> {
        at.checked_add(len)
            .and_then(|end| self.bytes.get(at..end))
            .with_context(|| {
                format!(
                    "rdm read {len} bytes at {at:#x} past end {:#x}",
                    self.bytes.len()
                )
            })
    }

    fn u32(&self, at: usize) -> Result<u32> {
        let b = self.bytes(at, WORD)?;
        Ok(self.order.u32([b[0], b[1], b[2], b[3]]))
    }

    fn f32(&self, at: usize) -> Result<f32> {
        Ok(f32::from_bits(self.u32(at)?))
    }

    fn vec3(&self, at: usize) -> Result<[f32; 3]> {
        Ok([
            self.f32(at)?,
            self.f32(at + WORD)?,
            self.f32(at + 2 * WORD)?,
        ])
    }

    fn vec4(&self, at: usize) -> Result<[f32; 4]> {
        Ok([
            self.f32(at)?,
            self.f32(at + WORD)?,
            self.f32(at + 2 * WORD)?,
            self.f32(at + 3 * WORD)?,
        ])
    }

    /// A stored `[x, y, z, w]` quaternion, conjugated and normalized. The
    /// engine's quaternions turn the other way: a joint record's rotation,
    /// conjugated, is the inverse bind rotation (each child lies along its
    /// parent's +Y, as rigged), and an animation root's first key is then
    /// its bind rotation. rdm4's glTF export negates `w` for the same reason.
    fn quat(&self, at: usize) -> Result<[f32; 4]> {
        Ok(quat_normalize(quat_conj(self.vec4(at)?)))
    }

    fn block(&self, payload: usize) -> Result<Block> {
        ensure!(
            payload >= BlockHead::COUNT,
            "rdm null or bad block offset {payload:#x}"
        );
        let count = self.u32(payload - BlockHead::COUNT)? as usize;
        let stride = self.u32(payload - BlockHead::STRIDE)? as usize;
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
        Ok(text(self.raw_string(payload)?).0)
    }

    /// A string block's bytes; none for a null pointer.
    fn raw_string(&self, payload: usize) -> Result<&'a [u8]> {
        if payload == 0 {
            return Ok(&[]);
        }
        let b = self.block(payload)?;
        ensure!(b.stride == 1, "rdm string stride {}", b.stride);
        self.bytes(b.start, b.count)
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
        prop_vertex_in(ByteOrder::Little, bitangent)
    }

    // Packed words go in swapped in a big-endian file, as the reader takes
    // them out.
    fn prop_vertex_in(order: ByteOrder, bitangent: [u8; 4]) -> Vec<u8> {
        let packed = |b: [u8; 4]| order.packed(&b);
        let mut v = Vec::new();
        let [p0, p1, p2, p3, t0, t1] = [0x3c00u16, 0xc000, 0x3800, 0x3c00, 0x3400, 0x3800];
        for h in [p0, p1, p2, p3] {
            v.extend(order.u16_bytes(h));
        }
        // N +Y, T +X, then the bitangent.
        v.extend(packed([127, 255, 127, 255]));
        v.extend(packed([255, 127, 127, 255]));
        v.extend(packed(bitangent));
        for h in [t0, t1] {
            v.extend(order.u16_bytes(h));
        }
        v.extend(packed([10, 20, 30, 40]));
        v
    }

    #[test]
    fn decodes_packed_prop_vertex() {
        let format = prop_format();
        assert_eq!(format.to_string(), "P4h_N4b_G4b_B4b_T2h_C4b");
        assert_eq!(format.stride(), 28);
        let mut data = prop_vertex([127, 127, 255, 255]); // B +Z
        data.extend(prop_vertex([127, 127, 0, 255])); // B -Z: mirrored UVs
        let m = decode_vertices(format, &data, 2, ByteOrder::Little).unwrap();
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
        let m = decode_vertices(format, &v, 1, ByteOrder::Little).unwrap();
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

    // Record strides as 1800's exporter writes them; the reader needs only
    // each record's minimum.
    const ROOT_STRIDE: usize = 48;
    const MESH_STRIDE: usize = 92;
    const RANGE_STRIDE: usize = 28;
    const FORMAT_STRIDE: usize = 24;
    const INDEX_FORMAT_STRIDE: usize = 20;

    // Lays out blocks the way exporters do: count and stride, then the
    // payload. Pointers are filled once every block has a position.
    struct Builder(Vec<u8>, ByteOrder);

    impl Builder {
        fn new(order: ByteOrder) -> Builder {
            let mut b = Builder(MAGIC.to_vec(), order);
            b.0.push(order.byte());
            b.0.resize(Preamble::LEN - BlockHead::COUNT, 0);
            b.put(Preamble::SIZE_AT, Preamble::SIZE);
            let root = b.block(1, ROOT_STRIDE, &[0; ROOT_STRIDE]);
            b.put(Preamble::ROOT, root);
            b
        }

        fn root(&self) -> usize {
            Preamble::LEN
        }

        fn words(&self, words: &[u32]) -> Vec<u8> {
            words.iter().flat_map(|&w| self.1.u32_bytes(w)).collect()
        }

        fn block(&mut self, count: usize, stride: usize, payload: &[u8]) -> u32 {
            let counts = self.words(&[count as u32, stride as u32]);
            self.0.extend(counts);
            let at = self.0.len() as u32;
            self.0.extend(payload);
            at
        }

        fn put(&mut self, at: usize, value: u32) {
            let bytes = self.1.u32_bytes(value);
            self.0[at..at + WORD].copy_from_slice(&bytes);
        }

        /// A mesh meta block of `meshes` records, each pointing at the
        /// same `mesh` buffers, `(MeshRecord field, pointer)`.
        fn meshes(&mut self, meshes: usize, mesh: &[(usize, u32)]) {
            let meta = self.block(meshes, MESH_STRIDE, &vec![0; MESH_STRIDE * meshes]);
            self.put(self.root() + RootRecord::META, meta);
            for k in 0..meshes {
                let rec = meta as usize + MESH_STRIDE * k;
                for &(field, p) in mesh {
                    self.put(rec + field, p);
                }
                self.put(rec + MeshRecord::SKELETON, u32::MAX);
            }
        }

        fn descriptors(&mut self, format: &VertexFormat) -> u32 {
            let ids: Vec<u8> = (format.0.iter())
                .flat_map(|a| {
                    let mut d = vec![0; Descriptor::SIZE];
                    for (field, v) in [
                        (Descriptor::SEMANTIC, a.semantic.id()),
                        (Descriptor::STORAGE, a.storage.code()),
                        (Descriptor::INTERPRETATION, a.interpretation),
                        (Descriptor::COUNT, a.count),
                    ] {
                        d[field..field + WORD].copy_from_slice(&self.1.u32_bytes(v));
                    }
                    d
                })
                .collect();
            let ids = self.block(format.0.len(), Descriptor::SIZE, &ids);
            let fmt_rec = self.block(1, FORMAT_STRIDE, &[0; FORMAT_STRIDE]);
            self.put(fmt_rec as usize, ids);
            fmt_rec
        }

        fn index_format(&mut self) -> u32 {
            let mut rec = self.words(&[Storage::U16.code()]);
            rec.resize(INDEX_FORMAT_STRIDE, 0);
            self.block(1, INDEX_FORMAT_STRIDE, &rec)
        }

        fn indices(&mut self, indices: &[u16]) -> u32 {
            let idx: Vec<u8> = indices.iter().flat_map(|&i| self.1.u16_bytes(i)).collect();
            self.block(indices.len(), size_of::<u16>(), &idx)
        }

        /// One material range over the first `count` indices.
        fn ranges(&mut self, count: u32) -> u32 {
            let mut rec = vec![0; RANGE_STRIDE];
            for (field, v) in [
                (RangeRecord::START, 0),
                (RangeRecord::COUNT, count),
                (RangeRecord::MATERIAL, 0),
            ] {
                rec[field..field + WORD].copy_from_slice(&self.1.u32_bytes(v));
            }
            self.block(1, RANGE_STRIDE, &rec)
        }
    }

    fn synthetic_rdm(indices: &[u16]) -> Vec<u8> {
        synthetic_meshes(ByteOrder::Little, indices, 1)
    }

    /// `meshes` mesh records sharing one set of buffers.
    fn synthetic_meshes(order: ByteOrder, indices: &[u16], meshes: usize) -> Vec<u8> {
        let mut b = Builder::new(order);
        let name = b.block(4, 1, b"quad");
        let name_rec = b.block(1, WORD, &order.u32_bytes(name));
        let fmt_rec = b.descriptors(&prop_format());
        let idx_fmt = b.index_format();
        let ranges = b.ranges(3);
        let mut verts = Vec::new();
        for _ in 0..4 {
            verts.extend(prop_vertex_in(order, [127, 127, 255, 255]));
        }
        let verts = b.block(4, prop_format().stride(), &verts);
        let idx = b.indices(indices);
        b.meshes(
            meshes,
            &[
                (MeshRecord::NAME, name_rec),
                (MeshRecord::FORMAT, fmt_rec),
                (MeshRecord::INDEX_FORMAT, idx_fmt),
                (MeshRecord::VERTICES, verts),
                (MeshRecord::INDICES, idx),
                (MeshRecord::RANGES, ranges),
            ],
        );
        b.0
    }

    // 2070's vehicles and props: the same mesh big-endian reads as it does
    // little-endian, but for the stored box, zero in both here.
    #[test]
    fn both_byte_orders_read_alike() {
        let little = Rdm::parse(&synthetic_meshes(ByteOrder::Little, &[0, 1, 2, 2, 1, 3], 1));
        let big = Rdm::parse(&synthetic_meshes(ByteOrder::Big, &[0, 1, 2, 2, 1, 3], 1));
        let (little, big) = (little.unwrap(), big.unwrap());
        assert_eq!(little, big);
        assert_eq!(big.positions, vec![[1.0, -2.0, 0.5]; 4]);
        assert_eq!(big.colors, Some(vec![[10, 20, 30, 40]; 4]));
        let mut bad = synthetic_rdm(&[0, 1, 2]);
        bad[Preamble::ORDER] = 2;
        assert!(Rdm::parse(&bad).is_err());
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

    #[test]
    fn halves_round_trip() {
        for v in [0.0f32, 1.0, -2.0, 0.5, 0.25, 65504.0, 3.25, -0.0023895905] {
            let back = f16(half(v));
            assert!((back - v).abs() <= v.abs() / 1024., "{v} -> {back}");
        }
        assert_eq!(half(1.0), 0x3c00);
        assert_eq!(half(-2.0), 0xc000);
        assert!(f16(half(1e6)).is_infinite());
    }

    // A cfg part's scale and rotation end up in the vertices. Adding a second
    // color gives the 1800 plant layout, and the first color stays as it was.
    #[test]
    fn rebake_bakes_the_part_transform() {
        use crate::model::Transform;
        let bytes = synthetic_rdm(&[0, 1, 2, 2, 1, 3]);
        let before = Rdm::parse(&bytes).unwrap();
        // A quarter turn about +Y: +X goes to -Z.
        let s = std::f32::consts::FRAC_1_SQRT_2;
        let t = Transform {
            position: [0.; 3],
            rotation: [0., s, 0., s],
            scale: [2.; 3],
        };
        let mut layout = prop_format();
        layout.0.push(attr(Semantic::Color, Storage::U32, 6, 1));
        let how = Rebake {
            transform: t,
            layout: Some(layout),
            color_fill: vec![[0; 4], [128; 4]],
            uv_rect: None,
        };
        let out = rebake(&bytes, &how).unwrap();
        let m = Rdm::parse(&out).unwrap();
        assert_eq!(m.format.to_string(), "P4h_N4b_G4b_B4b_T2h_C4b_C4b");
        assert_eq!(m.format.stride(), 32);
        assert_eq!(m.indices, before.indices);
        assert_eq!(m.colors, before.colors);
        assert_eq!(m.uvs, before.uvs);
        // (1, -2, 0.5) scaled to (2, -4, 1), turned to (1, -4, -2).
        let p = m.positions[0];
        assert!((p[0] - 1.).abs() < 1e-2 && (p[1] + 4.).abs() < 1e-2 && (p[2] + 2.).abs() < 1e-2);
        assert_eq!(m.bounds, (p, p));
        assert!(m.normals[0][1] > 0.999, "{:?}", m.normals[0]);
        assert!(m.tangents[0][2] < -0.999, "{:?}", m.tangents[0]);
        assert_eq!(m.tangents[0][3], before.tangents[0][3]);

        // Without a color the layout stays and the bytes change in place.
        let same = rebake(&bytes, &Rebake::default()).unwrap();
        assert_eq!(same.len(), bytes.len());
        assert_eq!(Rdm::parse(&same).unwrap().positions, before.positions);
    }

    // 117's most common layout, P4h_N4b_T2h_C4b, has no tangents. Rewritten as
    // 1800's plant layout it gets tangents from its texcoords (the quad's u
    // runs along +X), bitangents, and the second color.
    #[test]
    fn rebake_writes_another_layout() {
        let mut format = prop_format();
        format
            .0
            .retain(|a| !matches!(a.semantic, Semantic::Tangent | Semantic::Bitangent));
        assert_eq!(format.to_string(), "P4h_N4b_T2h_C4b");
        // A unit quad on the ground, u along +X, v along +Z.
        let corners = [
            ([0., 0., 0.], [0., 0.]),
            ([1., 0., 0.], [1., 0.]),
            ([0., 0., 1.], [0., 1.]),
            ([1., 0., 1.], [1., 1.]),
        ];
        let mut verts = Vec::new();
        for (p, t) in corners {
            for c in [p[0], p[1], p[2], 0.] {
                verts.extend(half(c).to_le_bytes());
            }
            verts.extend([128, 255, 128, 0]);
            for c in t {
                verts.extend(half(c).to_le_bytes());
            }
            verts.extend([10, 20, 30, 40]);
        }
        let bytes = custom_rdm(&format, &verts, 4, &[0, 2, 1, 1, 2, 3]);
        let mut plant = prop_format();
        plant.0.push(attr(Semantic::Color, Storage::U32, 6, 1));
        let how = Rebake {
            layout: Some(plant),
            color_fill: vec![[0; 4], [128; 4]],
            uv_rect: Some([0.5, 0., 1., 0.5]),
            ..Default::default()
        };
        let m = Rdm::parse(&rebake(&bytes, &how).unwrap()).unwrap();
        assert_eq!(m.format.to_string(), "P4h_N4b_G4b_B4b_T2h_C4b_C4b");
        assert_eq!(m.positions[3], [1., 0., 1.]);
        assert!(m.tangents.iter().all(|t| t[0] > 0.99), "{:?}", m.tangents);
        assert_eq!(m.colors.as_ref().unwrap()[0], [10, 20, 30, 40]);
        // The quad's corner (1, 1) lands on the atlas cell's corner.
        assert_eq!(m.uvs[3], [1., 0.5]);
        assert_eq!(m.uvs[0], [0.5, 0.]);
    }

    /// One mesh record of `count` vertices laid out as `format`.
    fn custom_rdm(format: &VertexFormat, verts: &[u8], count: usize, indices: &[u16]) -> Vec<u8> {
        custom_mesh(format, verts, count, indices).0
    }

    fn custom_mesh(format: &VertexFormat, verts: &[u8], count: usize, indices: &[u16]) -> Builder {
        let mut b = Builder::new(ByteOrder::Little);
        let fmt_rec = b.descriptors(format);
        let idx_fmt = b.index_format();
        let ranges = b.ranges(indices.len() as u32);
        let verts = b.block(count, format.stride(), verts);
        let idx = b.indices(indices);
        b.meshes(
            1,
            &[
                (MeshRecord::FORMAT, fmt_rec),
                (MeshRecord::INDEX_FORMAT, idx_fmt),
                (MeshRecord::VERTICES, verts),
                (MeshRecord::INDICES, idx),
                (MeshRecord::RANGES, ranges),
            ],
        );
        b
    }

    // Every mesh record joins: the second's indices and material range
    // come after the first's.
    #[test]
    fn merges_every_mesh() {
        let m = Rdm::parse(&synthetic_meshes(ByteOrder::Little, &[0, 1, 2], 2)).unwrap();
        assert_eq!(m.positions.len(), 8);
        assert_eq!(m.normals.len(), 8);
        assert_eq!(m.indices, vec![0, 1, 2, 4, 5, 6]);
        let starts: Vec<u32> = m.groups.iter().map(|g| g.start).collect();
        assert_eq!(starts, [0, 3]);
    }

    // Record strides as 117's exporter writes them.
    const SKELETON_STRIDE: usize = 32;
    const JOINT_STRIDE: usize = 84;
    const ANIM_STRIDE: usize = 48;
    const TRACK_STRIDE: usize = 24;
    const KEY_STRIDE: usize = 32;

    fn floats(v: &[f32]) -> Vec<u32> {
        v.iter().map(|f| f.to_bits()).collect()
    }

    /// Writes `joints` (name, model-space bind, parent) as the root's
    /// skeleton, each record holding the inverse bind with its rotation in
    /// the file's conjugated order.
    fn put_skeleton(b: &mut Builder, joints: &[(&str, JointPose, u32)]) {
        let names: Vec<u32> = (joints.iter())
            .map(|j| b.block(j.0.len(), 1, j.0.as_bytes()))
            .collect();
        let mut records = Vec::new();
        for ((_, bind, parent), name) in joints.iter().zip(names) {
            let inverse = bind.inverse();
            let mut words = vec![name];
            words.extend(floats(&inverse.translation));
            words.extend(floats(&quat_conj(inverse.rotation)));
            words.push(*parent);
            words.resize(JOINT_STRIDE / WORD, 0);
            records.extend(b.words(&words));
        }
        let at = b.block(joints.len(), JOINT_STRIDE, &records);
        let mut record = b.words(&[at]);
        record.resize(SKELETON_STRIDE, 0);
        let skin = b.block(1, SKELETON_STRIDE, &record);
        b.put(b.root() + RootRecord::SKIN, skin);
    }

    /// An animation-only file: per track, its joint name and
    /// `(seconds, parent-relative pose)` keys. Laid out as the exporter
    /// does: the record, its track block, then each track's name and keys.
    fn animation_rdm(duration_ms: u32, tracks: &[(&str, &[(f32, JointPose)])]) -> Vec<u8> {
        let mut b = Builder::new(ByteOrder::Little);
        let meta = b.block(1, ANIM_STRIDE, &[0; ANIM_STRIDE]) as usize;
        b.put(b.root() + RootRecord::ANIM, meta as u32);
        let block = b.block(
            tracks.len(),
            TRACK_STRIDE,
            &vec![0; TRACK_STRIDE * tracks.len()],
        );
        for (field, v) in [
            (AnimRecord::TRACKS, block),
            (AnimRecord::DURATION_MS, duration_ms),
            (AnimRecord::SAMPLE_RATE, 15),
        ] {
            b.put(meta + field, v);
        }
        for (k, (name, keys)) in tracks.iter().enumerate() {
            let name = b.block(name.len(), 1, name.as_bytes());
            let mut data = Vec::new();
            for (t, pose) in keys.iter() {
                let mut words = floats(&quat_conj(pose.rotation));
                words.extend(floats(&pose.translation));
                words.push(t.to_bits());
                data.extend(b.words(&words));
            }
            let keys = b.block(keys.len(), KEY_STRIDE, &data);
            let at = block as usize + k * TRACK_STRIDE;
            b.put(at + TrackRecord::NAME, name);
            b.put(at + TrackRecord::KEYS, keys);
        }
        b.0
    }

    fn close(a: &[f32], b: &[f32], eps: f32) -> bool {
        a.iter().zip(b).all(|(x, y)| (x - y).abs() <= eps)
    }

    const S: f32 = std::f32::consts::FRAC_1_SQRT_2;
    /// A quarter turn about +X: +Y goes to +Z.
    const QUARTER_X: [f32; 4] = [S, 0., 0., S];
    /// A quarter turn about +Z: +X goes to +Y.
    const QUARTER_Z: [f32; 4] = [0., 0., S, S];

    /// A root turned a quarter about X at the origin, and a tip one unit
    /// along its +Y, which the turn puts at model +Z: two vertices, one on
    /// each joint, the second shared 200/55 with the root.
    fn two_joint_mesh() -> Vec<u8> {
        let format = VertexFormat(vec![
            attr(Semantic::Position, Storage::F16, 0, 4),
            attr(Semantic::JointIndex, Storage::U32, 0, 1),
            attr(Semantic::Weight, Storage::U32, 2, 1),
        ]);
        assert_eq!(format.to_string(), "P4h_I4b_W4b");
        let mut verts = Vec::new();
        for (p, joints, weights) in [
            ([0., 0., 0.], [0, 0, 0, 0], [255, 0, 0, 0]),
            ([0., 0., 1.], [1, 0, 0, 0], [200, 55, 0, 0]),
        ] {
            for c in [p[0], p[1], p[2], 1.] {
                verts.extend(half(c).to_le_bytes());
            }
            verts.extend(joints);
            verts.extend(weights);
        }
        let mut b = custom_mesh(&format, &verts, 2, &[0, 1, 1]);
        let root = JointPose {
            rotation: QUARTER_X,
            translation: [0.; 3],
        };
        let tip = JointPose {
            rotation: QUARTER_X,
            translation: [0., 0., 1.],
        };
        put_skeleton(&mut b, &[("root", root, u32::MAX), ("tip", tip, 0)]);
        let at = b.root() + RootRecord::META;
        let mesh = u32::from_le_bytes(b.0[at..at + WORD].try_into().unwrap());
        b.put(mesh as usize + MeshRecord::SKELETON, 0);
        b.0
    }

    #[test]
    fn reads_skeleton_and_vertex_joints() {
        let m = Rdm::parse(&two_joint_mesh()).unwrap();
        assert!(m.skinned);
        assert_eq!(m.joints, [[0, 0, 0, 0], [1, 0, 0, 0]]);
        assert_eq!(m.weights, [[255, 0, 0, 0], [200, 55, 0, 0]]);
        let s = m.skeleton.unwrap();
        assert_eq!((s.len(), s.name(0), s.name(1)), (2, "root", "tip"));
        assert_eq!(s.parents, [NO_PARENT, 0]);
        assert_eq!((s.find("tip"), s.find("nope")), (Some(1), None));
        assert_eq!(s.hashes[1], bone_hash("tip"));
        assert!(close(&s.bind[1].translation, &[0., 0., 1.], 1e-6));
        // Relative to the root, the tip is one unit up its +Y, unturned.
        let local = s.bind_local[1];
        assert!(close(&local.translation, &[0., 1., 0.], 1e-6), "{local:?}");
        assert!(close(&local.rotation, &[0., 0., 0., 1.], 1e-6), "{local:?}");
        // The bind pose skins every vertex onto itself.
        let mut pose = s.bind_local.clone();
        s.to_model(&mut pose);
        let mut skin = vec![[[0.; 4]; 4]; s.len()];
        s.skinning_matrices(&pose, &mut skin);
        let identity = JointPose::IDENTITY.matrix();
        for m in &skin {
            assert!(
                close(m.as_flattened(), identity.as_flattened(), 1e-6),
                "{m:?}"
            );
        }
    }

    #[test]
    fn samples_between_keys() {
        let m = Rdm::parse(&two_joint_mesh()).unwrap();
        let s = m.skeleton.unwrap();
        let up = |y: f32| [0., y, 0.];
        let keys = [
            (
                0.,
                JointPose {
                    rotation: [0., 0., 0., 1.],
                    translation: up(1.),
                },
            ),
            (
                2.,
                JointPose {
                    rotation: QUARTER_Z,
                    translation: up(3.),
                },
            ),
        ];
        let bytes = animation_rdm(2000, &[("tip", &keys), ("elsewhere", &keys[..1])]);
        let err = Rdm::parse(&bytes).unwrap_err();
        assert!(err.is::<AnimationOnly>(), "{err}");
        assert!(Animation::parse(&two_joint_mesh()).is_err());
        let a = Animation::parse(&bytes).unwrap();
        assert_eq!((a.duration, a.sample_rate, a.tracks()), (2., 15, 2));
        assert_eq!(a.track_name(1), "elsewhere");
        assert_eq!(a.keys(0).0, [0., 2.]);

        // Halfway: an eighth turn about Z, two units up. The root has no
        // track and keeps its bind pose; the unknown track is ignored.
        let mut out = [JointPose::IDENTITY; 3];
        a.sample(1., &s, &mut out);
        let eighth = std::f32::consts::FRAC_PI_8;
        assert!(close(
            &out[1].rotation,
            &[0., 0., eighth.sin(), eighth.cos()],
            1e-6
        ));
        assert!(close(&out[1].translation, &up(2.), 1e-6));
        assert_eq!(out[0], s.bind_local[0]);
        assert_eq!(out[2], JointPose::IDENTITY);
        // Outside the keys: the first or the last.
        a.sample(-1., &s, &mut out);
        assert_eq!(out[1], keys[0].1);
        a.sample(5., &s, &mut out);
        assert!(close(&out[1].rotation, &QUARTER_Z, 1e-6));

        // At the end the tip is three units up the root's +Y, which the
        // root's turn puts at model +Z; a vertex on the tip's bind position
        // follows it there.
        a.sample(2., &s, &mut out);
        s.to_model(&mut out[..2]);
        assert!(
            close(&out[1].translation, &[0., 0., 3.], 1e-5),
            "{:?}",
            out[1]
        );
        let mut skin = [[[0.; 4]; 4]; 2];
        s.skinning_matrices(&out[..2], &mut skin);
        let p = m.positions[1];
        let moved: [f32; 3] =
            [0, 1, 2].map(|r| (0..3).map(|c| skin[1][c][r] * p[c]).sum::<f32>() + skin[1][3][r]);
        assert!(close(&moved, &[0., 0., 3.], 1e-3), "{moved:?}");
    }

    fn pose(rotation: [f32; 4], translation: [f32; 3]) -> JointPose {
        JointPose {
            rotation,
            translation,
        }
    }

    /// `q` and `-q` are one rotation.
    fn close_pose(a: &JointPose, b: &JointPose) -> bool {
        let r = &a.rotation;
        (close(r, &b.rotation, 1e-6) || close(&r.map(|c| -c), &b.rotation, 1e-6))
            && close(&a.translation, &b.translation, 1e-6)
    }

    /// `a` written and read back.
    fn reread(a: &Animation) -> Animation {
        Animation::parse(&a.encode().unwrap()).unwrap()
    }

    // Each edit survives encode and parse, and sampling reads it back at
    // its time; what was not edited keeps its keys.
    #[test]
    fn edits_survive_encode() {
        let m = Rdm::parse(&two_joint_mesh()).unwrap();
        let s = m.skeleton.unwrap();
        let start = pose([0., 0., 0., 1.], [0., 1., 0.]);
        let end = pose(QUARTER_Z, [0., 3., 0.]);
        let bytes = animation_rdm(
            2000,
            &[
                ("tip", &[(0., start), (2., end)]),
                ("elsewhere", &[(0., start)]),
            ],
        );
        let mut a = Animation::parse(&bytes).unwrap();
        assert_eq!(a.encode().unwrap(), bytes);
        let tip = a.find_track(bone_hash("tip")).unwrap();
        assert_eq!(
            (tip, a.track_for(&s, 1), a.track_for(&s, 0)),
            (0, Some(0), None)
        );

        // A key between the two: inserted in order, read back at its time,
        // and the span before it now leads to it.
        let mid = pose(QUARTER_X, [0.5, 1., 0.]);
        assert_eq!(a.set_key(tip, 1., mid), 1);
        let b = reread(&a);
        assert_eq!(b.keys(tip).0, [0., 1., 2.]);
        let mut out = [JointPose::IDENTITY; 2];
        b.sample(1., &s, &mut out);
        assert!(close_pose(&out[1], &mid), "{:?}", out[1]);
        b.sample(0.5, &s, &mut out);
        assert!(close(&out[1].translation, &[0.25, 1., 0.], 1e-6));
        assert_eq!(b.keys(1), a.keys(1));

        // Within KEY_EPSILON: the same key, its time kept.
        let moved = pose(QUARTER_Z, [0., 2., 0.]);
        assert_eq!(a.set_key(tip, 1.0003, moved), 1);
        assert_eq!(a.keys(tip).0, [0., 1.0003, 2.]);
        assert!(close_pose(
            &reread(&a).pose_at(tip, 1.0003).unwrap(),
            &moved
        ));
        a.set_key(tip, 1., moved);

        // Removed again: halfway between the ends, as at the start.
        assert!(!a.remove_key(tip, 1));
        reread(&a).sample(1., &s, &mut out);
        assert!(close(&out[1].translation, &[0., 2., 0.], 1e-6));

        // The root had no track: one from its bind pose holds it there,
        // and a key on it turns it.
        let root = a.add_track(&s, 0);
        assert_eq!((root, a.tracks(), a.track_name(root)), (2, 3, "root"));
        assert_eq!(a.track_for(&s, 0), Some(root));
        let turned = pose(QUARTER_Z, s.bind_local[0].translation);
        a.set_key(root, 1., turned);
        let b = reread(&a);
        b.sample(0., &s, &mut out);
        assert!(close_pose(&out[0], &s.bind_local[0]));
        b.sample(1., &s, &mut out);
        assert!(close_pose(&out[0], &turned));

        // Removing a track's only key removes the track.
        assert!(a.remove_key(1, 0));
        let root = root - 1;
        assert_eq!((a.tracks(), a.track_name(root)), (2, "root"));
        assert_eq!(reread(&a).track_name(root), "root");

        // A key past the end lengthens the animation.
        a.set_key(tip, 2.5, end);
        assert_eq!(reread(&a).duration, 2.5);

        // Shorter: cut at the new end, with the pose it had there.
        let mut before = [JointPose::IDENTITY; 2];
        a.sample(1.5, &s, &mut before);
        a.set_duration(1.5);
        let b = reread(&a);
        assert_eq!((b.duration, b.keys(tip).0), (1.5, &[0., 1.5][..]));
        assert_eq!(b.keys(root).0, [0., 1.]);
        b.sample(1.5, &s, &mut out);
        assert!(
            close_pose(&out[1], &before[1]),
            "{:?} {:?}",
            out[1],
            before[1]
        );
        // Longer: the last keys hold.
        a.set_duration(3.);
        let b = reread(&a);
        assert_eq!((b.duration, b.keys(tip).0), (3., &[0., 1.5][..]));
    }

    // An animation built in memory gets a preamble and root of its own.
    #[test]
    fn new_animation_encodes() {
        let m = Rdm::parse(&two_joint_mesh()).unwrap();
        let s = m.skeleton.unwrap();
        let mut a = Animation {
            duration: 1.,
            sample_rate: 15,
            ..Animation::default()
        };
        let tip = a.add_track(&s, 1);
        a.set_key(tip, 1., pose(QUARTER_Z, [0., 2., 0.]));
        let b = reread(&a);
        assert_eq!((b.duration, b.sample_rate, b.tracks()), (1., 15, 1));
        assert_eq!(b.keys(tip).0, [0., 1.]);
        assert!(close_pose(&b.pose_at(tip, 0.).unwrap(), &s.bind_local[1]));
        assert_eq!(b.encode().unwrap(), a.encode().unwrap());
    }

    // A new animation in the bind pose: a track per joint, still from its
    // first key to its last, at the shipped rate, and it reads back as
    // written.
    #[test]
    fn a_bind_pose_animation_holds_still() {
        let m = Rdm::parse(&two_joint_mesh()).unwrap();
        let s = m.skeleton.unwrap();
        let a = Animation::bind_pose(&s, 1.25);
        assert_eq!((a.duration, a.sample_rate, a.tracks()), (1.25, 15, 2));
        let b = reread(&a);
        assert_eq!(b.encode().unwrap(), a.encode().unwrap());
        assert_eq!((b.duration, b.sample_rate), (1.25, SHIPPED_RATE));
        for j in 0..s.len() {
            let track = b.track_for(&s, j).expect("every joint has a track");
            assert_eq!(b.keys(track).0, [0., 1.25]);
        }
        let mut out = [JointPose::IDENTITY; 2];
        for t in [0., 0.6, 1.25] {
            b.sample(t, &s, &mut out);
            for (posed, bind) in out.iter().zip(&s.bind_local) {
                assert!(close_pose(posed, bind), "{t}: {posed:?}");
            }
        }
    }

    // Every shipped animation encodes to the bytes it was read from, and an
    // edit to any of them reads back. 2070's 25 big-endian files included.
    #[test]
    fn every_animation_round_trips() {
        use rayon::prelude::*;
        for var in ["ANNO117_DIR", "ANNO1800_DIR", "ANNO2070_DIR"] {
            let Some(dir) = crate::vfs::install_dir(var) else {
                eprintln!("skip: {var} unset");
                continue;
            };
            let install = Install::open(dir, crate::vfs::Install::DATA_FIRST).unwrap();
            let results: Vec<Result<bool, String>> = (0..install.file_count())
                .into_par_iter()
                .map(|i| FileId(i as u32))
                .filter(|&id| install.file_name(id).ends_with(".rdm"))
                .map(|id| {
                    let path = install.file_path(id);
                    let bytes = install.read_file(id).map_err(|e| format!("{path}: {e}"))?;
                    let Ok(mut a) = Animation::parse(&bytes) else {
                        return Ok(false);
                    };
                    // The word a new file's head copies.
                    if bytes[12..16] != [4, 0, 0, 0] && bytes[12..16] != [0, 0, 0, 4] {
                        return Err(format!("{path}: preamble word 3 is {:?}", &bytes[12..16]));
                    }
                    let out = a.encode().map_err(|e| format!("{path}: {e}"))?;
                    if out != bytes {
                        let at = (out.iter().zip(&bytes))
                            .position(|(x, y)| x != y)
                            .unwrap_or(out.len().min(bytes.len()));
                        return Err(format!(
                            "{path}: {} bytes for {}, first difference at {at:#x}",
                            out.len(),
                            bytes.len()
                        ));
                    }
                    if a.tracks() > 0 {
                        let t = a.duration / 2.;
                        let p = pose([0., S, 0., S], [0.1, 0.2, 0.3]);
                        a.set_key(0, t, p);
                        let back = reread(&a).pose_at(0, t).unwrap();
                        if !close_pose(&back, &p) {
                            return Err(format!("{path}: edit read back as {back:?}"));
                        }
                    }
                    Ok(true)
                })
                .collect();
            let failures: Vec<&String> = results.iter().filter_map(|r| r.as_ref().err()).collect();
            let animations = results.iter().filter(|r| r == &&Ok(true)).count();
            eprintln!(
                "{var}: {animations} animations, {} failures",
                failures.len()
            );
            for f in failures.iter().take(20) {
                eprintln!("  {f}");
            }
            assert!(failures.is_empty(), "{} failures in {var}", failures.len());
        }
    }

    // FNV-1a's published vectors, and two BoneHash values from
    // worker_lean_f_01.cfg, whose props hang on these joints.
    #[test]
    fn bone_hash_is_fnv1a() {
        assert_eq!(bone_hash(""), 0x811c_9dc5);
        assert_eq!(bone_hash("a"), 0xe40c_292c);
        assert_eq!(bone_hash("r_hand_prop_jnt"), 3_963_261_774);
        assert_eq!(bone_hash("l_hand_prop_jnt"), 734_730_020);
    }

    // Golden counts from the file's own mesh records (8 meshes: trestle
    // halves, rails, sleepers, conveyor belts), read by hand.
    #[test]
    fn mine_rail_has_all_eight_meshes() {
        let Some(dir) = crate::vfs::install_dir("ANNO1800_DIR") else {
            eprintln!("skip: ANNO1800_DIR unset");
            return;
        };
        let install = Install::open(dir, crate::vfs::Install::DATA_FIRST).unwrap();
        let path = "data/graphics/buildings/production/mining_01/rdm/mining_rail_01.rdm";
        let m = Rdm::parse(&install.read(path).unwrap()).unwrap();
        assert_eq!((m.positions.len(), m.indices.len()), (4080, 7272));
        let materials: Vec<u32> = m.groups.iter().map(|g| g.material).collect();
        assert_eq!(materials, [2, 0, 2, 1, 2, 2, 2, 2]);
        assert_eq!(m.groups[1].start, 2856);
        // Both halves of the trestle: the full width.
        assert!(
            m.bounds.0[2] < -0.25 && m.bounds.1[2] > 0.25,
            "{:?}",
            m.bounds
        );
    }

    fn install() -> Option<Install> {
        let Some(dir) = crate::vfs::install_dir("ANNO117_DIR") else {
            eprintln!("skip: ANNO117_DIR unset");
            return None;
        };
        Some(Install::open(dir, crate::vfs::Install::DATA_FIRST).unwrap())
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

    // The big dog's idle starts at its bind pose, as far as the animator
    // left it there: bones keep their bind lengths, and joints sit near
    // their bind positions.
    #[test]
    fn dog_idle_starts_near_bind() {
        let Some(install) = install() else { return };
        let dir = "data/base/graphics/library/wildlife/dog";
        let read = |p: &str| install.read(format!("{dir}/{p}")).unwrap();
        let m = Rdm::parse(&read("rdm/dog-big_01_lod0.rdm")).unwrap();
        assert_eq!(m.format.to_string(), "P4h_N4b_G4b_B4b_T2h_I4b_W4b");
        let s = m.skeleton.as_ref().unwrap();
        assert_eq!(
            (s.len(), s.name(0), s.name(40)),
            (56, "rig.canine_big_dog", "DEF-head")
        );
        assert!(m.joints.iter().flatten().all(|&j| (j as usize) < s.len()));
        let sums = |w: &[u8; 4]| w.iter().map(|&x| x as u32).sum::<u32>();
        assert!(m.weights.iter().all(|w| (254..=256).contains(&sums(w))));

        // Blender bones point along their own +Y: a joint that is its
        // parent's only child sits up the parent's Y axis. The opposite
        // quaternion order leaves most of them off it.
        let only: Vec<usize> = (1..s.len())
            .filter(|&j| s.parents[j] != NO_PARENT)
            .filter(|&j| s.parents.iter().filter(|&&p| p == s.parents[j]).count() == 1)
            .collect();
        let along_y = (only.iter())
            .filter(|&&j| {
                let t = s.bind_local[j].translation;
                t[1] > 0.99 * t.iter().map(|c| c * c).sum::<f32>().sqrt()
            })
            .count();
        assert!(
            along_y * 10 >= only.len() * 9,
            "{along_y} of {}",
            only.len()
        );

        let a = Animation::parse(&read("anim/dog-big_idle_01.rdm")).unwrap();
        assert_eq!((a.duration, a.sample_rate, a.tracks()), (5.333, 15, 56));
        let mut pose = vec![JointPose::IDENTITY; s.len()];
        a.sample(0., s, &mut pose);
        let length = |t: [f32; 3]| t.iter().map(|c| c * c).sum::<f32>().sqrt();
        let stretch = (0..s.len())
            .map(|j| (length(pose[j].translation) - length(s.bind_local[j].translation)).abs())
            .fold(0f32, f32::max);
        s.to_model(&mut pose);
        let moved = (0..s.len())
            .map(|j| {
                let (p, q) = (pose[j].translation, s.bind[j].translation);
                length([0, 1, 2].map(|k| p[k] - q[k]))
            })
            .fold(0f32, f32::max);
        let height = m.bounds.1[1] - m.bounds.0[1];
        // Measured: 5 mm (the hip drops) and 5.5 cm (the tail tip) of
        // 26.6 cm. A wrong quaternion order or composition misses by the
        // dog's size.
        assert!(stretch < 0.006, "{stretch}");
        assert!(moved < 0.06 && moved < 0.25 * height, "{moved} of {height}");
    }

    // A horse's body, tail and mane each carry a copy of the rig in their
    // own joint order: three parts, each mesh's vertices on its own, one
    // animation driving the copies of a joint alike.
    #[test]
    fn horse_parts_share_one_animation() {
        let Some(install) = install() else { return };
        let dir = "data/base/graphics/library/wildlife/horses";
        let m = Rdm::parse(&install.read(format!("{dir}/horse_01.rdm")).unwrap()).unwrap();
        let s = m.skeleton.as_ref().unwrap();
        assert_eq!(s.part_ends, [54, 108, 162]);
        // Vertex counts of the three mesh records: body, tail, mane.
        let (body, tail) = (1581, 86);
        for (verts, joints) in [
            (0..body, 0..54),
            (body..body + tail, 54..108),
            (body + tail..m.positions.len(), 108..162),
        ] {
            for v in verts {
                for k in 0..4 {
                    let j = m.joints[v][k] as usize;
                    assert!(m.weights[v][k] == 0 || joints.contains(&j), "{v}: {j}");
                }
            }
        }
        let copies: Vec<usize> = (0..s.len())
            .filter(|&j| s.name(j) == "DEF-tail.001")
            .collect();
        assert_eq!(copies.len(), 3);
        let a = Animation::parse(
            &install
                .read(format!("{dir}/anims/horse_idle_01.rdm"))
                .unwrap(),
        )
        .unwrap();
        let mut pose = vec![JointPose::IDENTITY; s.len()];
        a.sample(1.5, s, &mut pose);
        s.to_model(&mut pose);
        for &j in &copies[1..] {
            let (p, q) = (pose[j].translation, pose[copies[0]].translation);
            assert!(close(&p, &q, 1e-4), "{p:?} {q:?}");
            assert!(!close(&p, &s.bind[j].translation, 1e-3), "the tail moves");
        }
    }

    // Every animation in 117, 1800 and 2070 reads, and every skinned mesh
    // has a skeleton that its vertices index into. About a second per game.
    #[test]
    fn every_animation_and_skeleton_reads() {
        use rayon::prelude::*;
        for var in ["ANNO117_DIR", "ANNO1800_DIR", "ANNO2070_DIR"] {
            let Some(dir) = crate::vfs::install_dir(var) else {
                eprintln!("skip: {var} unset");
                continue;
            };
            let install = Install::open(dir, crate::vfs::Install::DATA_FIRST).unwrap();
            let seen: Vec<(String, Result<Seen, String>)> = (0..install.file_count())
                .into_par_iter()
                .map(|i| FileId(i as u32))
                .filter(|&id| install.file_name(id).ends_with(".rdm"))
                .map(|id| {
                    let path = install.file_path(id);
                    let seen = install.read_file(id).map_err(|e| e.to_string());
                    (path, seen.and_then(|b| Seen::of(&b)))
                })
                .collect();
            let mut tally: BTreeMap<String, usize> = BTreeMap::new();
            let mut failures = Vec::new();
            let (mut max_joints, mut max_tracks) = (0, 0);
            for (path, seen) in &seen {
                let key = match seen {
                    Ok(Seen::Mesh) => "static mesh".to_string(),
                    Ok(Seen::Unreadable) => "unreadable mesh".to_string(),
                    Ok(Seen::Jointless) => "skinned mesh, no joints".to_string(),
                    Ok(Seen::Skinned {
                        joints,
                        rigid,
                        unweighted,
                    }) => {
                        max_joints = max_joints.max(*joints);
                        let rigid = if *rigid { ", rigid" } else { "" };
                        let unweighted = if *unweighted {
                            ", unweighted vertices"
                        } else {
                            ""
                        };
                        format!("skinned mesh{rigid}{unweighted}")
                    }
                    Ok(Seen::Animation { tracks, rate }) => {
                        max_tracks = max_tracks.max(*tracks);
                        format!("animation at {rate} Hz")
                    }
                    Err(e) => {
                        failures.push(format!("{path}: {e}"));
                        "FAILED".to_string()
                    }
                };
                *tally.entry(key).or_default() += 1;
            }
            eprintln!("{var}: {tally:?}, up to {max_joints} joints, {max_tracks} tracks");
            for f in failures.iter().take(20) {
                eprintln!("  {f}");
            }
            assert!(failures.is_empty(), "{} failures in {var}", failures.len());
        }
    }

    enum Seen {
        Mesh,
        /// Fails `Rdm::parse` for a mesh reason (sweep_install's business).
        Unreadable,
        /// Skinned, with an empty skeleton.
        Jointless,
        Skinned {
            joints: usize,
            rigid: bool,
            unweighted: bool,
        },
        Animation {
            tracks: usize,
            rate: u32,
        },
    }

    impl Seen {
        fn of(bytes: &[u8]) -> Result<Seen, String> {
            let m = match Rdm::parse(bytes) {
                Ok(m) => m,
                Err(e) if e.is::<AnimationOnly>() => {
                    let a = Animation::parse(bytes).map_err(|e| format!("{e:#}"))?;
                    for k in 0..a.tracks() {
                        let (times, rotations, translations) = a.keys(k);
                        let last = times.last().copied().unwrap_or(0.);
                        // Keys end at the duration, up to its ms rounding.
                        if last > a.duration + 0.002 {
                            return Err(format!(
                                "{} ends at {last} s of {}",
                                a.track_name(k),
                                a.duration
                            ));
                        }
                        let finite = rotations
                            .iter()
                            .flatten()
                            .chain(translations.iter().flatten());
                        if !finite.clone().all(|c| c.is_finite()) {
                            return Err(format!("{} has a non-finite key", a.track_name(k)));
                        }
                    }
                    return Ok(Seen::Animation {
                        tracks: a.tracks(),
                        rate: a.sample_rate,
                    });
                }
                Err(_) => return Ok(Seen::Unreadable),
            };
            if !m.skinned {
                return Ok(Seen::Mesh);
            }
            let Some(s) = m.skeleton.as_ref() else {
                // Egypt's ship_01 and its levels: a skeleton record whose
                // joint pointer is null, every vertex on joint 0.
                let r = Reader::new(bytes).unwrap();
                let skin = r.u32(r.root().unwrap() + RootRecord::SKIN).unwrap();
                let joints = r.block(skin as usize).and_then(|b| r.u32(b.start));
                if joints.is_ok_and(|p| p == 0) && m.joints.iter().all(|j| j == &[0; 4]) {
                    return Ok(Seen::Jointless);
                }
                return Err("skinned without a readable skeleton".into());
            };
            if let Some(j) = m.joints.iter().flatten().find(|&&j| j as usize >= s.len()) {
                return Err(format!("joint {j} of {}", s.len()));
            }
            // In five 1800 portrait files, 1 to 23 vertices carry no weight
            // in any of their sixteen slots.
            let unweighted = m.weights.iter().any(|w| w == &[0; 4]);
            Ok(Seen::Skinned {
                joints: s.len(),
                unweighted,
                rigid: m.format.find(Semantic::Weight).is_none(),
            })
        }
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
