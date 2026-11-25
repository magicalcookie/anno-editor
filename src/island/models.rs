// Owns the island's model library: `.cfg` → GPU meshes (one per part
// material, nested props flattened in), loaded off the UI thread in batches
// and instanced across every prop and object that uses them, each with
// the pose it moves by (motion.rs).
use super::motion::{self, Hang, Link};
use anno_formats::{
    model::{Alpha, Model, Part, Shader, Specular, Transform},
    rdm::Rdm,
    vfs::Install,
};
use anyhow::Result;
use gpui_editor_kit::{
    gpu::{self, Mesh, MeshData},
    math::Mat4,
    task::Batches,
    texture::{Compressed, Dds, Image, Releasable},
};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

/// Textures upload at about this many texels per metre of the model they
/// cover as placed, within `TEXTURE_SIZES`: an island references a few
/// hundred, each with diffuse, normal and metal maps, shipped up to 4096².
/// A 2 m bush takes 256², an 8 m tree 512², a 30 m rock 2048². Only for
/// textures residency does not stream (`Textures::whole`, loads without
/// the app's install); streamed ones start from their shipped top level.
const TEXELS_PER_METRE: f32 = 48.;
const TEXTURE_SIZES: std::ops::RangeInclusive<u32> = 256..=2048;

/// Texture side for a model `extent` metres across.
fn texture_size(extent: f32) -> u32 {
    ((extent * TEXELS_PER_METRE) as u32)
        .next_power_of_two()
        .clamp(*TEXTURE_SIZES.start(), *TEXTURE_SIZES.end())
}

/// How far `part`, its mesh-local box `(lo, hi)`, reaches across inside
/// `at`, in metres of the model.
fn part_extent(at: &Transform, part: &Part, (lo, hi): ([f32; 3], [f32; 3])) -> f32 {
    let scale = (at.then(&part.transform).scale.iter()).fold(0f32, |m, s| m.max(s.abs()));
    (0..3).map(|a| hi[a] - lo[a]).fold(0., f32::max) * scale
}

/// Texture side for a splat layer repeating `tiling` times over the UVs of
/// a part `extent` metres across as placed, whose rdm is `own` metres
/// across. Under SPLAT_SCALE_INVARIANT the layer UVs grow with the
/// placement's scale, so one repeat spans about the rdm's own extent
/// however large it is placed. Under cSplatWorldTiling a repeat is
/// `1 / tiling` metres, shrunk by that scale when it is invariant too.
fn layer_size(
    own: f32,
    extent: f32,
    placed: f32,
    tiling: f32,
    invariant: bool,
    world: bool,
) -> u32 {
    let span = match (world, invariant) {
        (true, true) => 1. / placed.max(1e-3),
        (true, false) => 1.,
        (false, true) => own,
        (false, false) => extent,
    };
    texture_size(span / tiling)
}

/// The largest side of an rdm's bounds.
fn own_extent((lo, hi): ([f32; 3], [f32; 3])) -> f32 {
    (0..3).map(|a| hi[a] - lo[a]).fold(0., f32::max)
}

/// Texture side for a box decal in a model drawn at `placed` times its size.
fn decal_texture_size(decal: &anno_formats::model::Decal, placed: f32) -> u32 {
    let [ex, _, ez] = decal.extents;
    texture_size(2. * ex.max(ez) * placed)
}

/// Nested prop cfgs deeper than this are skipped (guards against cycles).
const MAX_DEPTH: usize = 4;
/// Cfgs per background job: every missing one. Each job's models give the
/// renderer a new piece list, which rebuilds its arena and re-bakes its
/// impostors; in batches of 96 the continental island's trees vanished
/// and came back, with a hitch, six times on load.
pub const BATCH: usize = usize::MAX;

/// One drawable piece of a model in model space.
#[derive(Clone)]
pub struct Piece {
    pub mesh: Arc<Mesh>,
    /// The part's mesh file. Nothing draws by it; tests and a debugger
    /// tell a model's pieces apart with it.
    #[allow(dead_code)]
    pub rdm: Arc<str>,
    pub local: Mat4,
    /// The root cfg's entry it came from: its part, decal, or the nested
    /// prop entry it lies under. The model editor picks and outlines by it.
    pub element: Option<anno_formats::xml::Id>,
    /// The rdm group it was built from; 0 for a box decal.
    pub group: u32,
    /// Coarser levels, coarsest last: the game's own where it ships them,
    /// else simplified copies (foliage thins on the GPU at every level,
    /// `MeshData::thinning`); empty for meshes too coarse to simplify.
    pub lods: Vec<Arc<Mesh>>,
    /// Pixels per metre from which each level draws, the full mesh first;
    /// one per level but the last, which draws below every threshold.
    pub ppm: Vec<f32>,
    /// Cutout foliage that thins or has levels: an impostor far away.
    pub foliage: bool,
    /// Placed through a cfg sub-file with `AdaptTerrainHeight`: the piece
    /// keeps its height above the ground under its own origin.
    pub follow: bool,
    /// How it moves by itself: its skeleton playing the cfg's default
    /// sequence, the joint it hangs on, its own motion and that of the
    /// entries above it (motion.rs). Island loads only.
    pub pose: Option<Arc<gpu::Pose>>,
}

/// Where a nested cfg sits in the model being loaded, and what its sub-file
/// entries say about the terrain.
struct Outer {
    at: Transform,
    /// Parts that hang on a joint draw at their joint-space placement
    /// instead of being left out: the model editor poses them after.
    rigged: bool,
    /// The root cfg's `<Files>` entry this nested cfg lies under.
    element: Option<anno_formats::xml::Id>,
    /// A `DeformToTerrainHeight` sub-file: every part bends to the ground.
    deform: bool,
    /// An `AdaptTerrainHeight` sub-file: parts ride the ground.
    follow: bool,
    /// Island loads pose pieces that move (`Piece::pose`); the asset
    /// preview and the Model workspace do not.
    posed: bool,
    /// How each nested entry above this cfg stands and moves, outermost
    /// first; `at` is their still placements, one after another.
    chain: Vec<Link>,
}

/// Foliage below this many (logical) pixels per unit draws as an
/// impostor: a quad blending the three nearest of 64 baked directions.
/// Trees switch at 11; ground cover keeps 8: lit by the ground's normal
/// through thin blades, its impostors read darker and patchier at 11.
const IMPOSTOR_PPM: f32 = 8.;
const TREE_IMPOSTOR_PPM: f32 = 11.;
/// Foliage at least this tall (world units) counts as a tree: celtic
/// beeches stand 2.3 to 4, bushes, reeds and grass at most 1.6.
const TREE_HEIGHT: f32 = 2.;

/// The impostor threshold of a foliage piece `height` world units tall.
pub fn impostor_ppm(height: f32) -> f32 {
    if height >= TREE_HEIGHT {
        TREE_IMPOSTOR_PPM
    } else {
        IMPOSTOR_PPM
    }
}

/// A coarser opaque level takes over once its triangles would be this many
/// pixels across, taking a triangle as the model's extent over the square
/// root of its count. The game ships no switch distances, and levels fall
/// off unevenly: celtic_rock_06_a goes 28,435, 1,338, 389, 126, 36
/// triangles over 6 m, so its `_lod1` waits until 30 px/m (a 180 px rock)
/// and `_lod2` until 16 px/m, where a fixed 16 px/m for every model had
/// shown 389 triangles at nearly 100 px.
const EDGE_PIXELS: f32 = 5.;

/// Pixels per metre below which each level after the first takes over, for
/// a model `extent` metres across: one per entry of `levels` after the
/// full mesh, never rising. `strays` gives, per level after the full mesh,
/// how far a simplified level strays from the full mesh's surface (metres
/// of the mesh), where it is known; with ANNO_EDITOR_LOD_ERROR set (logical
/// pixels), such a level takes over once that projects below it instead.
fn level_ppm(extent: f32, levels: &[usize], strays: &[Option<f32>]) -> Vec<f32> {
    let mut last = f32::INFINITY;
    (levels.iter().skip(1))
        .enumerate()
        .map(|(k, &tris)| {
            let edge = extent / (tris.max(1) as f32).sqrt();
            let by_edge = EDGE_PIXELS / edge.max(1e-4);
            let ppm = match (lod_error(), strays.get(k).copied().flatten()) {
                (Some(pixels), Some(strays)) => pixels / strays.max(1e-6),
                _ => by_edge,
            };
            last = ppm.min(last);
            last
        })
        .collect()
}

/// ANNO_EDITOR_LOD_ERROR: the logical pixels a simplified level may stray
/// from its full mesh's surface on screen before the finer level draws
/// instead; unset, levels switch by their triangles' size (`EDGE_PIXELS`).
/// A switch for comparing the two, read once.
fn lod_error() -> Option<f32> {
    static PIXELS: std::sync::OnceLock<Option<f32>> = std::sync::OnceLock::new();
    *PIXELS.get_or_init(|| {
        let pixels = std::env::var("ANNO_EDITOR_LOD_ERROR")
            .ok()?
            .parse::<f32>()
            .ok()?;
        (pixels > 0.).then_some(pixels)
    })
}

/// How foliage thins with distance (`gpu::Thinning`): every leaf clump
/// draws from 20 pixels per metre up (a 160 px tree), and the share kept
/// falls with the tree's area on screen down to a fifth, the clumps growing
/// to keep its coverage. The renderer fades clumps out one by one, so the
/// thinning does not pop. Fixed levels (half the clumps below 20, a fifth
/// below 6) drew 22M triangles in a low view over a 768 m island.
const THIN_FULL: f32 = 20.;
const THIN_LEAST: f32 = 0.2;
/// Shipped trees are hundreds of separate leaf clumps (laurel: 830–1160);
/// meshes with fewer loose parts are not thinned.
const MIN_PARTS: usize = 40;
/// A part this many triangles or larger can be a trunk or bough, drawn at
/// every distance (with any part at least half its size).
const TRUNK_TRIS: usize = 16;

/// `data` ready to thin on the GPU: its loose parts (leaf cards and clumps)
/// ranked at random, triangles in rank order, each vertex with its part's
/// centre and rank. Parts at least half the size of the largest (trunks,
/// big boughs) rank 0 and always draw. `None` when there is too little to
/// thin.
// Shipped 117 trees carry no LOD meshes, and ~3000 laurels at 8.6k
// triangles each are half of a large island's 52M triangles.
fn rank(data: &MeshData) -> Option<MeshData> {
    let n = data.positions.len();
    let mut parent: Vec<u32> = (0..n as u32).collect();
    fn root(p: &mut [u32], mut i: u32) -> u32 {
        while p[i as usize] != i {
            p[i as usize] = p[p[i as usize] as usize];
            i = p[i as usize];
        }
        i
    }
    for t in data.indices.chunks(3) {
        let a = root(&mut parent, t[0]);
        for &v in &t[1..] {
            let b = root(&mut parent, v);
            parent[b as usize] = a;
        }
    }
    let roots: Vec<u32> = (0..n as u32).map(|i| root(&mut parent, i)).collect();
    // Per root vertex: triangles, then the centre's sum and vertex count.
    let mut size = vec![0usize; n];
    for t in data.indices.chunks(3) {
        size[roots[t[0] as usize] as usize] += 1;
    }
    if size.iter().filter(|&&s| s > 0).count() < MIN_PARTS {
        return None;
    }
    let largest = size.iter().copied().max().unwrap_or(0);
    // A stable pseudo-random rank per part (splitmix64 of its root vertex),
    // never 0, which is for parts that always draw.
    // Parts of a mesh of equal cards (reeds, grass: 35k two-triangle
    // quads) are all "large", and none would ever go; a trunk or bough is
    // tens of triangles or more.
    let always = |s: usize| s * 2 >= largest && largest >= TRUNK_TRIS;
    let rank_of = |r: u32| {
        if always(size[r as usize]) {
            return 0.;
        }
        let mut z = u64::from(r).wrapping_add(0x9e37_79b9_7f4a_7c15);
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        (((z ^ (z >> 31)) as f64 / u64::MAX as f64) as f32).max(1e-6)
    };
    let mut centre = vec![([0f32; 3], 0f32); n];
    for (i, p) in data.positions.iter().enumerate() {
        let c = &mut centre[roots[i] as usize];
        for (sum, v) in c.0.iter_mut().zip(p) {
            *sum += v;
        }
        c.1 += 1.;
    }
    let parts: Vec<[f32; 4]> = (0..n)
        .map(|i| {
            let r = roots[i];
            let (sum, count) = centre[r as usize];
            [sum[0] / count, sum[1] / count, sum[2] / count, rank_of(r)]
        })
        .collect();
    let mut tris: Vec<[u32; 3]> = data.indices.as_chunks::<3>().0.to_vec();
    tris.sort_by(|a, b| parts[a[0] as usize][3].total_cmp(&parts[b[0] as usize][3]));
    Some(MeshData {
        indices: tris.into_iter().flatten().collect(),
        thinning: Some(gpu::Thinning {
            parts,
            full: THIN_FULL,
            least: THIN_LEAST,
        }),
        ..data.clone()
    })
}

pub type Loaded = Arc<Vec<Piece>>;

/// GPU-ready textures shared across models, each virtual path interned once
/// into a slot: the side the largest user in the loads so far needs
/// (`Textures::size`), the side loaded, and the image. A model placed larger
/// in a later batch loads it again bigger; meshes built earlier keep the
/// smaller. Block-compressed data goes to the GPU as is, never decoded on
/// the CPU.
#[derive(Default)]
pub struct Textures(Mutex<TextureTable>);

#[derive(Default)]
struct TextureTable {
    ids: HashMap<String, u32>,
    /// Per texture: the side its users need, the side loaded, the image.
    slots: Vec<(u32, u32, Option<Image>)>,
}

impl TextureTable {
    fn id(&mut self, path: &str) -> u32 {
        if let Some(&id) = self.ids.get(path) {
            return id;
        }
        let id = self.slots.len() as u32;
        self.ids.insert(path.to_owned(), id);
        self.slots.push((0, 0, None));
        id
    }
}

/// One texture a read model asks for: from part `source`, sized by the
/// part's extent, or past its parts from decal `source - parts`, sized by
/// its box (`Side`).
#[derive(Clone, Copy)]
struct Ask {
    source: u32,
    texture: u32,
    side: Side,
}

/// The side an asked texture loads at: its part's or decal's, half that
/// (metal and height maps), or a splat's, which the renderer keeps whole
/// and the table keys apart (`whole_key`): its control map at the part's,
/// a layer at one repeat (`layer_size`).
#[derive(Clone, Copy, PartialEq)]
enum Side {
    Full,
    Half,
    Control,
    Layer {
        tiling: f32,
        invariant: bool,
        world: bool,
    },
}

/// The table's key for `path` loaded whole (`Textures::whole`).
fn whole_key(path: &str) -> String {
    format!("{path}#whole")
}

/// The textures `model` asks for, as `add_parts` and `add_box_decal` ask;
/// `room` is the game's window interior map (`Cache::window_interior`).
fn asks(model: &Model, room: Option<&str>, ask: &mut impl FnMut(u32, Option<&str>, Side)) {
    for (p, part) in model.parts.iter().enumerate() {
        let Some(rdm) = part.mesh.as_ref().filter(|_| !part.bone_linked) else {
            continue;
        };
        for group in &rdm.groups {
            let Some(material) = part.materials.get(group.material as usize) else {
                continue;
            };
            let decal = material.mesh_decal.as_ref();
            let hidden = matches!(material.alpha, Alpha::Hidden)
                || matches!(material.alpha, Alpha::Decal if decal.is_none());
            if hidden {
                continue;
            }
            let p = p as u32;
            if material.diffuse.is_some() {
                ask(p, material.diffuse.as_deref(), Side::Full);
                ask(p, material.normal.as_deref(), Side::Full);
                ask(p, material.metallic.as_deref(), Side::Half);
                ask(p, material.dye_mask.as_deref(), Side::Full);
                // 2070's own maps, which its building family takes as slots.
                if let Shader::ModelFx(_, own) = &material.shader {
                    ask(p, own.mask.as_deref(), Side::Full);
                    ask(p, own.ripples.as_deref(), Side::Full);
                    ask(p, own.height.as_deref(), Side::Full);
                }
                if let Some(glow) = &material.glow {
                    ask(p, glow.night_map.as_deref(), Side::Full);
                    if glow.interior.is_some() {
                        ask(p, room, Side::Full);
                    }
                }
                if let Some(splat) = material.splat.as_ref() {
                    ask(p, Some(&splat.map), Side::Control);
                    for l in splat.layers.iter().flatten() {
                        let side = Side::Layer {
                            tiling: l.tiling,
                            invariant: splat.scale_invariant,
                            world: splat.world_tiling,
                        };
                        ask(p, Some(&l.diffuse), side);
                        ask(p, l.normal.as_deref(), side);
                    }
                }
            }
            if let Some(d) = decal {
                ask(p, d.alpha_map.as_deref(), Side::Full);
            }
        }
    }
    for (k, decal) in model.decals.iter().enumerate() {
        let Some(material) = &decal.material else {
            continue;
        };
        let [ex, _, ez] = decal.extents;
        let plain = decal.kind == "Vehicle" || decal.opacity <= 0. || material.diffuse.is_none();
        if plain || ex <= 0. || ez <= 0. {
            continue;
        }
        let source = (model.parts.len() + k) as u32;
        ask(source, material.diffuse.as_deref(), Side::Full);
        ask(source, decal.height_map.as_deref(), Side::Half);
        ask(source, material.normal.as_deref(), Side::Full);
        ask(source, material.metallic.as_deref(), Side::Half);
    }
}

/// A DDS header with its DX10 extension.
const DDS_HEADER: u64 = 148;

/// `path`'s mips from the first that fits `size`, still compressed, or
/// that mip decoded when the file is not block-compressed. The header,
/// then only those mips: a 4k BC7 atlas wanted at 1024² reads 1.4 of its 22
/// MB. Reading whole files moved 7 GB for the continental island's 542
/// models. `skip` leaves that many more levels out at the top: a texture
/// read again for a view that asks for its fourth level needs none above.
fn read_texture(
    install: &Install,
    path: &str,
    size: u32,
    skip: u32,
) -> Option<Result<Compressed, Image>> {
    let dds = Dds::parse(&install.read_range(path, 0, DDS_HEADER).ok()?).ok()?;
    let chain = dds.mips.min(32 - dds.width.max(dds.height).leading_zeros());
    let first = (dds.mip_for(size) + skip).min(chain.saturating_sub(1));
    let at = dds.mip_offset(first) as u64;
    Some(
        match dds.compressed_from(first, &install.read_range(path, at, u64::MAX).ok()?) {
            Ok(c) => Ok(c),
            Err(_) => {
                let b = install.read(path).ok()?;
                Err(dds.decode(&b, first).ok().map(Image::from)?)
            }
        },
    )
}

/// `path` as `read_texture` gives it, but holding only the coarse levels a
/// placement texture starts from (`gpu::floor_level`): the renderer reads
/// the chain from `install` when views ask for finer. The continental
/// island's 542 models held 412 MB of texels while they loaded, now a few
/// MB. `None` for files that are not block-compressed or keep no level
/// below the floor.
fn read_tail(install: &Arc<Install>, path: &str, size: u32) -> Option<Image> {
    let dds = Dds::parse(&install.read_range(path, 0, DDS_HEADER).ok()?).ok()?;
    let first = dds.mip_for(size);
    // Headers sometimes count past 1x1 (`Dds::compressed_from`).
    let chain = dds.mips.min(32 - dds.width.max(dds.height).leading_zeros());
    let (width, height) = dds.mip_size(first);
    let levels = chain.checked_sub(first)? as usize;
    let floor = gpu::floor_level(width, height, levels) as u32;
    if floor == 0 {
        return None;
    }
    let at = dds.mip_offset(first + floor) as u64;
    let tail = dds
        .compressed_from(first + floor, &install.read_range(path, at, u64::MAX).ok()?)
        .ok()?;
    let (install, path) = (install.clone(), path.to_owned());
    let reload = move |skip| read_texture(&install, &path, size, skip as u32)?.ok();
    Releasable::from_tail(width, height, levels, tail, reload)
        .ok()
        .map(Image::from)
}

impl Textures {
    /// `path` from the level that fits `size` down, every level held: for
    /// textures the renderer keeps whole, which residency does not size
    /// (`gpu::MeshSplat`). Keyed apart from `get`'s.
    fn whole(&self, install: &Install, path: &str, size: u32) -> Option<Image> {
        let (id, size) = {
            let mut table = self.0.lock().unwrap();
            let id = table.id(&whole_key(path)) as usize;
            let (wanted, have, image) = &table.slots[id];
            let size = size.max(*wanted);
            if *have >= size {
                return image.clone();
            }
            (id, size)
        };
        let image = read_texture(install, path, size, 0).map(|read| match read {
            Ok(c) => Image::from(c),
            Err(rgba) => rgba,
        });
        let slot = &mut self.0.lock().unwrap().slots[id];
        if slot.1 <= size {
            (slot.1, slot.2) = (size, image.clone());
        }
        image
    }

    /// With `shared`, the app's install, compressed textures are
    /// `Releasable` and hold only their coarse levels (`read_tail`): the
    /// renderer reads the rest as views ask, lets it go again once on the
    /// GPU (418 MB on the continental island), and reads it anew should
    /// it need it.
    fn get(
        &self,
        install: &Install,
        shared: Option<&Arc<Install>>,
        path: &str,
        size: u32,
    ) -> Option<Image> {
        let (id, size) = {
            let mut table = self.0.lock().unwrap();
            let id = table.id(path) as usize;
            let (wanted, have, image) = &table.slots[id];
            let size = size.max(*wanted);
            if *have >= size {
                return image.clone();
            }
            (id, size)
        };
        // A tail starts from the shipped top level, not `size`: residency
        // keeps only the levels views sample, by the mesh's own UVs
        // (cull.wgsl `want_texels`), so a side from the part's extent only
        // capped close-ups. It also rounded down: a 1536² atlas asked for
        // at 1024 loaded from 768², half what the game shows.
        let tail = shared.and_then(|shared| read_tail(shared, path, u32::MAX));
        let size = if tail.is_some() { u32::MAX } else { size };
        let image = tail.or_else(|| {
            read_texture(install, path, size, 0).and_then(|read| match read {
                Ok(c) => match shared {
                    Some(shared) => {
                        let (install, path) = (shared.clone(), path.to_owned());
                        let reload =
                            move |skip| read_texture(&install, &path, size, skip as u32)?.ok();
                        Releasable::new(c, reload).ok().map(Image::from)
                    }
                    None => Some(Image::from(c)),
                },
                Err(rgba) => Some(rgba),
            })
        });
        let slot = &mut self.0.lock().unwrap().slots[id];
        if slot.1 <= size {
            (slot.1, slot.2) = (size, image.clone());
        }
        image
    }

    /// Sizes every texture `read`'s models ask for to its largest user,
    /// `roots` (model, placed scale) drawn as `add_parts` draws them, before
    /// any loads. Built as they loaded, a shared texture took the side of
    /// whichever load asked first for meshes built before a larger one
    /// asked: a model's detail hung on which cfg a core took first.
    fn size(&self, read: &Read, roots: &[(u32, f32)], room: Option<&str>) {
        let mut table = self.0.lock().unwrap();
        // Each model's asks once, against interned paths; then the walk
        // runs on flat arrays.
        let mut all = Vec::new();
        let mut ranges = Vec::with_capacity(read.models.len());
        for model in &read.models {
            let start = all.len();
            if let Ok(model) = model {
                asks(model, room, &mut |source, path, side| {
                    if let Some(path) = path {
                        let texture = match side {
                            Side::Control | Side::Layer { .. } => table.id(&whole_key(path)),
                            Side::Full | Side::Half => table.id(path),
                        };
                        all.push(Ask {
                            source,
                            texture,
                            side,
                        });
                    }
                });
            }
            ranges.push(start..all.len());
        }
        let mut sides = vec![0u32; table.slots.len()];
        let mut walk = Walk {
            read,
            asks: &all,
            ranges: &ranges,
            sides: &mut sides,
        };
        for &(m, placed) in roots {
            walk.model(m, &Transform::IDENTITY, placed, 0);
        }
        for (slot, side) in table.slots.iter_mut().zip(sides) {
            slot.0 = slot.0.max(side);
        }
    }
}

/// `Textures::size`'s walk over placed models and their nested props.
struct Walk<'a> {
    read: &'a Read,
    asks: &'a [Ask],
    ranges: &'a [std::ops::Range<usize>],
    sides: &'a mut [u32],
}

impl Walk<'_> {
    fn model(&mut self, m: u32, at: &Transform, placed: f32, depth: usize) {
        let Ok(model) = &self.read.models[m as usize] else {
            return;
        };
        for a in &self.asks[self.ranges[m as usize].clone()] {
            let side = match model.parts.get(a.source as usize) {
                Some(part) => {
                    let bounds = part.mesh.as_ref().expect("asked from its mesh").bounds;
                    let extent = part_extent(at, part, bounds) * placed;
                    match a.side {
                        Side::Layer {
                            tiling,
                            invariant,
                            world,
                        } => {
                            layer_size(own_extent(bounds), extent, placed, tiling, invariant, world)
                        }
                        _ => texture_size(extent),
                    }
                }
                None => {
                    let decal = &model.decals[a.source as usize - model.parts.len()];
                    decal_texture_size(decal, placed)
                }
            };
            let s = &mut self.sides[a.texture as usize];
            *s = (*s).max(side >> u32::from(a.side == Side::Half));
        }
        if depth < MAX_DEPTH {
            for prop in &model.props {
                if let Some(&n) = self.read.ids.get(&prop.cfg) {
                    self.model(n, &at.then(&prop.transform), placed, depth + 1);
                }
            }
        }
    }
}

/// Every cfg a load draws, nested props included, each read once: the
/// first phase of a load. The meshes stay until the load has built from
/// them.
#[derive(Default)]
struct Read {
    ids: HashMap<String, u32>,
    models: Vec<Result<Model>>,
}

impl Read {
    /// `roots` and every prop cfg they nest down to `MAX_DEPTH`, read on
    /// every core a nesting level at a time.
    fn new(install: &Install, roots: impl IntoIterator<Item = String>) -> Read {
        let mut read = Read::default();
        let mut level: Vec<String> = roots.into_iter().collect();
        for depth in 0..=MAX_DEPTH {
            level.retain(|c| !read.ids.contains_key(c));
            level.sort_unstable();
            level.dedup();
            if level.is_empty() {
                break;
            }
            let models = parallel(&level, |cfg| Model::load(install, cfg));
            let nested = (models.iter().flatten())
                .filter(|_| depth < MAX_DEPTH)
                .flat_map(|m| m.props.iter().map(|p| p.cfg.clone()))
                .collect();
            for (cfg, model) in std::mem::replace(&mut level, nested)
                .into_iter()
                .zip(models)
            {
                read.ids.insert(cfg, read.models.len() as u32);
                read.models.push(model);
            }
        }
        read
    }

    fn get(&self, cfg: &str) -> Option<&Result<Model>> {
        self.ids.get(cfg).map(|&m| &self.models[m as usize])
    }
}

/// `f` over `items` on every core, each taking the next as it finishes,
/// since one cfg can take 30 times another's time; in `items`' order.
pub(super) fn parallel<T: Sync, U: Send>(items: &[T], f: impl Fn(&T) -> U + Sync) -> Vec<U> {
    let threads = std::thread::available_parallelism().map_or(4, |n| n.get());
    let next = std::sync::atomic::AtomicUsize::new(0);
    let mut out: Vec<(usize, U)> = std::thread::scope(|scope| {
        let workers: Vec<_> = (0..threads.min(items.len()))
            .map(|_| {
                scope.spawn(|| {
                    let mut out = Vec::new();
                    loop {
                        let k = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        let Some(item) = items.get(k) else {
                            return out;
                        };
                        out.push((k, f(item)));
                    }
                })
            })
            .collect();
        (workers.into_iter())
            .flat_map(|w| w.join().expect("model loader"))
            .collect()
    });
    out.sort_unstable_by_key(|(k, _)| *k);
    out.into_iter().map(|(_, u)| u).collect()
}

/// What model loads share: textures, and the meshes built from rdm groups.
pub struct Cache {
    textures: Textures,
    meshes: Meshes,
    /// The app's install, for textures to read again (`Textures::get`).
    install: std::sync::OnceLock<Arc<Install>>,
    /// How the game's materials glow (`deferred_glow`), the wall behind
    /// its windows (`window_interior`), and its own surface families,
    /// which dress its lit meshes (`surfaces`).
    look: anno_core::files::ModelLook,
    /// Each cfg's default sequence, baked for poses.
    clips: motion::Clips,
}

impl Cache {
    /// A texture by the path a cfg or rdp names (`maps/x.psd` ships as
    /// `maps/x_0.dds`), at most `side` texels across: for meshes the model
    /// editor builds itself, such as particle sprites.
    pub fn texture(&self, install: &Install, source: &str, side: u32) -> Option<Image> {
        let path = anno_formats::model::texture_path(source);
        self.textures.get(install, self.install.get(), &path, side)
    }

    /// An empty cache for models drawn with `look`. No default: a cache
    /// without the game's look built 117's windows without their rooms.
    pub fn new(look: &anno_core::files::ModelLook) -> Self {
        Self {
            textures: Textures::default(),
            meshes: Meshes::default(),
            install: std::sync::OnceLock::new(),
            look: *look,
            clips: motion::Clips::default(),
        }
    }
}

/// An rdm group as the renderer draws it, for every part, nested prop and
/// model that draws it with the same material at the same texture size:
/// the continental island's 542 models built 6,668 meshes, 1,249 of them
/// different, and the arena packed each copy.
struct Built {
    mesh: Arc<Mesh>,
    lods: Vec<Arc<Mesh>>,
    /// Triangles of the mesh and each level.
    counts: Vec<usize>,
    foliage: bool,
    thinned: bool,
}

/// Built groups by rdm path, group, texture size and material. The first
/// load to ask builds; others asking meanwhile wait for it.
#[derive(Default)]
struct Meshes(Mutex<HashMap<String, Arc<Slot>>>);

/// One mesh's build: set once by the load that asked first.
type Slot = std::sync::OnceLock<Option<Arc<Built>>>;

impl Meshes {
    fn get(&self, key: String, build: impl FnOnce() -> Option<Built>) -> Option<Arc<Built>> {
        let cell = self.0.lock().unwrap().entry(key).or_default().clone();
        cell.get_or_init(|| build().map(Arc::new)).clone()
    }
}

/// Loaded cfgs, and what their loads share.
pub struct Library {
    pub models: Batches<String, Loaded>,
    pub cache: Arc<Cache>,
}

/// A load batch: cfgs, each with the largest scale it is placed at.
pub type Batch = Vec<(String, f32)>;

impl Library {
    /// The cfgs of `wanted` (cfg, and a matrix it is placed with) not yet
    /// loaded or queued, now queued, each with the largest scale it is
    /// placed at (column lengths of its matrices), so big scaled rocks load
    /// their textures big enough.
    pub fn batch<'a>(
        &mut self,
        wanted: impl IntoIterator<Item = (&'a str, Option<&'a Mat4>)>,
    ) -> Batch {
        let wanted: Vec<_> = wanted.into_iter().collect();
        let batch = self
            .models
            .next_batch(wanted.iter().map(|(c, _)| *c), BATCH);
        let mut placed: HashMap<&str, f32> = batch.iter().map(|c| (c.as_str(), 1.)).collect();
        for (cfg, m) in &wanted {
            if let (Some(s), Some(m)) = (placed.get_mut(cfg), m) {
                let column = |i: usize| gpui_editor_kit::math::length([m[i], m[i + 1], m[i + 2]]);
                *s = s.max(column(0)).max(column(4)).max(column(8));
            }
        }
        batch
            .iter()
            .map(|c| (c.clone(), placed[c.as_str()]))
            .collect()
    }

    /// Loads `batch` off the UI thread; `models.finish` takes the result.
    pub fn job(
        &self,
        install: Arc<Install>,
        batch: Batch,
    ) -> impl FnOnce() -> Vec<(String, Option<Loaded>)> + Send + 'static {
        let cache = self.cache.clone();
        cache.install.get_or_init(|| install.clone());
        move || {
            let _span = gpui_editor_kit::profile::span("models: load batch");
            let loaded = load_batch(&install, &cache, &batch);
            (batch.into_iter().zip(loaded))
                .map(|((cfg, _), pieces)| {
                    let pieces = pieces.map_err(|e| eprintln!("{cfg}: {e:#}")).ok();
                    (cfg, pieces.map(Arc::new))
                })
                .collect()
        }
    }
}

/// Loads `batch` (cfg, largest placed scale) into pieces in three phases:
/// every model read once, every texture sized for its largest user, then
/// the pieces built on every core. Missing parts and textures are skipped,
/// so a building with a dangling reference still shows what it has.
fn load_batch(
    install: &Install,
    cache: &Cache,
    batch: &[(String, f32)],
) -> Vec<Result<Vec<Piece>>> {
    let read = {
        let _span = gpui_editor_kit::profile::span("models: read");
        Read::new(install, batch.iter().map(|(cfg, _)| cfg.clone()))
    };
    {
        let _span = gpui_editor_kit::profile::span("models: size textures");
        let roots: Vec<(u32, f32)> = (batch.iter())
            .filter_map(|(cfg, placed)| Some((*read.ids.get(cfg)?, placed.max(1.))))
            .collect();
        cache
            .textures
            .size(&read, &roots, cache.look.window_interior);
    }
    let _span = gpui_editor_kit::profile::span("models: build");
    parallel(batch, |(cfg, placed)| match read.get(cfg) {
        Some(Ok(model)) => {
            let mut pieces = Vec::new();
            let origin = &Outer {
                posed: true,
                ..Outer::ORIGIN
            };
            add_parts(
                install,
                cache,
                &read,
                model,
                origin,
                placed.max(1.),
                0,
                &mut pieces,
            );
            Ok(pieces)
        }
        Some(Err(e)) => Err(anyhow::anyhow!("{e:#}")),
        None => Err(anyhow::anyhow!("not read")),
    })
}

/// Loads one cfg into pieces, its textures sized for the model drawn at
/// `placed` times its own size (the largest scale it is placed at), as a
/// batch of one.
// TODO(alex): a model scaled up after it loaded keeps its texture size until
// the island reopens.
pub fn load(install: &Install, cache: &Cache, cfg: &str, placed: f32) -> Result<Vec<Piece>> {
    let batch = [(cfg.to_owned(), placed)];
    load_batch(install, cache, &batch).pop().expect("one cfg")
}

/// [`load`] for a model already read, which the caller may have changed:
/// the asset preview draws one mesh file with its cfg's materials this way.
/// `rigged` keeps parts that hang on a joint, for a caller that poses them.
pub fn build(
    install: &Install,
    cache: &Cache,
    model: &Model,
    placed: f32,
    rigged: bool,
) -> Vec<Piece> {
    let read = Read::new(install, model.props.iter().map(|p| p.cfg.clone()));
    let mut pieces = Vec::new();
    let origin = &Outer {
        rigged,
        ..Outer::ORIGIN
    };
    add_parts(
        install,
        cache,
        &read,
        model,
        origin,
        placed.max(1.),
        0,
        &mut pieces,
    );
    pieces
}

/// The pieces of `cfg` as [`build`] nests it under a model with `rigged`
/// parts, at its own origin and a nesting level down, on a sub-file entry
/// with `deform` (`DeformToTerrainHeight`) and `follow`
/// (`AdaptTerrainHeight`): for the model editor, which loads a model's
/// props one by one off the UI thread and places them itself.
pub fn nested(
    install: &Install,
    cache: &Cache,
    cfg: &str,
    deform: bool,
    follow: bool,
) -> Result<Vec<Piece>> {
    let read = Read::new(install, [cfg.to_owned()]);
    let model = match read.get(cfg) {
        Some(Ok(model)) => model,
        Some(Err(e)) => anyhow::bail!("{e:#}"),
        None => anyhow::bail!("{cfg} not read"),
    };
    let outer = Outer {
        rigged: true,
        deform,
        follow,
        ..Outer::ORIGIN
    };
    let mut pieces = Vec::new();
    add_parts(install, cache, &read, model, &outer, 1., 1, &mut pieces);
    Ok(pieces)
}

impl Outer {
    const ORIGIN: Outer = Outer {
        at: Transform::IDENTITY,
        rigged: false,
        element: None,
        deform: false,
        follow: false,
        posed: false,
        chain: Vec::new(),
    };
}

/// `model`'s parts, decals and nested props (from `read`) as pieces.
#[allow(clippy::too_many_arguments)]
fn add_parts(
    install: &Install,
    cache: &Cache,
    read: &Read,
    model: &Model,
    outer: &Outer,
    placed: f32,
    depth: usize,
    out: &mut Vec<Piece>,
) {
    let cfg = &model.path;
    let clip = (outer.posed && !outer.rigged)
        .then(|| cache.clips.get(install, model))
        .flatten();
    for (p, part) in model.parts.iter().enumerate() {
        // A part on a joint stands on the joint's bind pose here; the model
        // editor (`rigged`) poses it itself from its joint-space placement.
        // One a condition shows (a tool while working) stays out: nothing
        // raises conditions here, and a worker would hold every tool at once.
        let joint = match (part.bone_linked, outer.rigged) {
            (false, _) | (true, true) => Transform::IDENTITY,
            (true, false) if part.rig.shown_if.is_some() => continue,
            (true, false) => match part.rig.link.and_then(|l| model.joint_placement(l)) {
                Some(joint) => joint,
                None => continue,
            },
        };
        let Some(rdm) = &part.mesh else { continue };
        let local = outer.at.then(&joint).then(&part.transform).matrix();
        // The part's own link: the joint as it hangs, its motion.
        let pose = outer.posed.then(|| {
            let hang = match part.rig.link.filter(|_| part.bone_linked) {
                Some(l) => Hang::of(model, clip.as_ref(), l).unwrap_or(Hang::None),
                None => Hang::None,
            };
            let own = Link {
                hang,
                transform: part.transform,
                motion: part.rig.motion,
            };
            let chain: Vec<&Link> = outer.chain.iter().chain([&own]).collect();
            let skin = (clip.as_ref())
                .filter(|c| rdm.joints.len() == rdm.positions.len() && c.plays(p))
                .map(|clip| motion::Skinned { rdm, clip, part: p });
            let bounds = [rdm.bounds.0, rdm.bounds.1];
            motion::part_pose(&chain, skin, bounds)
        });
        let pose = pose.flatten();
        // Shipped detail levels next to the mesh, `<stem>_lod1.rdm`… where
        // the full mesh is `<stem>.rdm` or (most buildings) `<stem>_lod0.rdm`.
        // Rocks and buildings ship them; 117's trees do not.
        let stem = (part.rdm.strip_suffix(".rdm")).map(|s| s.strip_suffix("_lod0").unwrap_or(s));
        // Parsed only for groups not yet built.
        let shipped = std::cell::OnceCell::new();
        let shipped = || {
            shipped.get_or_init(|| -> Vec<Rdm> {
                stem.map_or_else(Vec::new, |stem| {
                    (1..=4)
                        .map_while(|k| {
                            Rdm::parse(&install.read(&format!("{stem}_lod{k}.rdm")).ok()?).ok()
                        })
                        .collect()
                })
            })
        };
        let deform = part.deform_to_terrain || outer.deform;
        let extent = part_extent(&outer.at, part, rdm.bounds);
        // celtic_mountain_element_01 is 16 m across but placed at up to
        // 3.6×: sized for 16 m its 2048² atlas loaded at 1024², 18 texels
        // per placed metre.
        let size = texture_size(extent * placed);
        for (g, group) in rdm.groups.iter().enumerate() {
            let Some(material) = part.materials.get(group.material as usize) else {
                continue;
            };
            // Mesh decals blend over what is under them; projected decals
            // (the cfg's box projectors and their shaders) are not drawn.
            let decal = material.mesh_decal.as_ref();
            // Tested surfaces cut where alpha says but are no foliage: no
            // impostor, no thinning.
            let cutout = match material.alpha {
                Alpha::Opaque | Alpha::Tested(_) => false,
                Alpha::Cutout(_) | Alpha::Blend(_) => true,
                Alpha::Decal if decal.is_some() => false,
                Alpha::Decal | Alpha::Hidden => continue,
            };
            let key = format!("{}#{g}@{size}{material:?}{deform}", part.rdm);
            // The game bends a material set to adjust to the terrain, or any
            // under DeformToTerrainHeight, relative unless it says absolute.
            let adapt = (material.terrain_adapt.is_some() || deform).then(|| {
                match material.terrain_adapt.is_some_and(|a| a.absolute) {
                    true => gpu::TerrainAdapt::Absolute,
                    false => gpu::TerrainAdapt::Relative,
                }
            });
            // Vertex blue is how rigid a vertex stays (strength 1 - blue);
            // a local pivot (green) adapts fully. The renderer reads it from
            // the color's top byte.
            // TODO(alex): local pivots sample the ground at the vertex, not
            // at the pivot; the unpacking is not in the shipped shaders.
            let rigid = |rdm: &Rdm, i: usize| -> u32 {
                let Some(a) = material.terrain_adapt.filter(|a| a.vertex_colored) else {
                    return 0;
                };
                match rdm.colors.as_ref().and_then(|c| c.get(i)) {
                    Some(c) if !(a.local_pivots && c[1] != 0) => u32::from(c[2]),
                    _ => 0,
                }
            };
            // Skinned vertices carry their joints, numbered as the full
            // mesh's skeleton numbers them, for poses to move.
            let skinned = |r: &Rdm| {
                let base = rdm.skeleton.as_ref()?;
                (r.joints.len() == r.positions.len() && !r.positions.is_empty()).then_some(base)
            };
            let built = cache.meshes.get(key, || {
                let map = |t: &Option<String>, size| {
                    t.as_deref()
                        .and_then(|t| cache.textures.get(install, cache.install.get(), t, size))
                };
                let texture = map(&material.diffuse, size);
                // Normals in red/green with gloss in alpha; the metal map is
                // metalness in red, occlusion in alpha (model_pbr.fx via
                // functions_pbr.fxh), smooth enough for half the size. Blue of
                // the normal map is glow (`emissive`); dye and night masks
                // and rooms are the look's surfaces' (`dress`).
                let normal_map = texture.as_ref().and(map(&material.normal, size));
                let surface_map = texture.as_ref().and(map(&material.metallic, size / 2));
                // Kept whole, a layer sized for one repeat (`layer_size`).
                let splat = (texture.as_ref().and(material.splat.as_ref())).and_then(|s| {
                    let whole = |t: &str, side| cache.textures.whole(install, t, side);
                    let layers = s.layers.each_ref().map(|l| {
                        let l = l.as_ref()?;
                        let (own, world) = (own_extent(rdm.bounds), s.world_tiling);
                        let side = layer_size(
                            own,
                            extent * placed,
                            placed,
                            l.tiling,
                            s.scale_invariant,
                            world,
                        );
                        Some(gpu::MeshSplatLayer {
                            albedo: whole(&l.diffuse, side)?,
                            normal: l.normal.as_deref().and_then(|n| whole(n, side)),
                            tiling: l.tiling,
                        })
                    });
                    layers.iter().any(Option::is_some).then(|| {
                        Some(Arc::new(anno_render::a7m_models::mesh_splat(
                            whole(&s.map, size)?,
                            layers,
                            s,
                        )))
                    })?
                });
                let alpha_map = decal.and_then(|d| map(&d.alpha_map, size));
                let emissive = texture.as_ref().and(material.glow.as_ref()).map(|glow| {
                    let deferred = cache.look.deferred_glow;
                    anno_render::a7m_models::emissive(glow, material.diffuse_color, deferred).0
                });
                let tint = material
                    .diffuse_color
                    .map(|c| (c.clamp(0., 1.) * 255.) as u32);
                let color = tint[0] << 16 | tint[1] << 8 | tint[2];
                let build = |rdm: &Rdm, g: usize| -> Option<MeshData> {
                    let group = rdm.groups.get(g)?;
                    let range = group.start as usize..(group.start + group.count) as usize;
                    let indices = rdm.indices.get(range)?;
                    // Compact the group's vertices so each mesh only uploads
                    // its own, numbered by first use: a table over the span
                    // of vertices the group uses (a hash map per index had
                    // been 3% of a load's CPU).
                    let (lo, hi) =
                        (indices.iter()).fold((u32::MAX, 0), |(lo, hi), &i| (lo.min(i), hi.max(i)));
                    let mut remap = vec![u32::MAX; hi.saturating_sub(lo) as usize + 1];
                    let mut data = MeshData {
                        material: gpu::Material::Lit,
                        indices: Vec::with_capacity(indices.len()),
                        ..Default::default()
                    };
                    for &i in indices {
                        let slot = &mut remap[(i - lo) as usize];
                        if *slot == u32::MAX {
                            *slot = data.positions.len() as u32;
                            let i = i as usize;
                            data.positions.push(rdm.positions[i]);
                            data.uvs.push(rdm.uvs.get(i).copied().unwrap_or_default());
                            data.normals
                                .push(rdm.normals.get(i).copied().unwrap_or([0., 1., 0.]));
                            let tint = if texture.is_some() { color } else { 0xa49a88 };
                            data.colors.push(tint | rigid(rdm, i) << 24);
                            if let Some(base) = skinned(rdm) {
                                data.skin.push(motion::skin(rdm, base, i));
                            }
                        }
                        data.indices.push(*slot);
                    }
                    if texture.is_none() {
                        data.uvs.clear();
                    }
                    data.texture = texture.clone();
                    data.normal_map = normal_map.clone();
                    data.surface_map = surface_map.clone();
                    data.emissive = emissive;
                    data.splat = splat.clone();
                    data.terrain_tint = material.terrain_tint;
                    data.specular = match material.specular {
                        Specular::Full => gpu::Specular::FULL,
                        Specular::MatteFades => gpu::Specular::MATTE_FADES,
                        Specular::NoSkyMatteFades => {
                            gpu::Specular::NO_SKY | gpu::Specular::MATTE_FADES
                        }
                        Specular::NoSun => gpu::Specular::NO_SUN,
                    };
                    data.terrain_adapt = adapt;
                    data.world_tiling = material.world_tiling.map(|t| gpu::WorldTiling {
                        scale: t.scale,
                        diagonal: t.diagonal,
                    });
                    // Diffuse alpha carries gloss/height on opaque materials; only
                    // cutout, blend, decal and tested materials use it as coverage.
                    let tested = match material.alpha {
                        Alpha::Tested(r) => Some(r),
                        _ => None,
                    };
                    data.ignore_alpha = !cutout && decal.is_none() && tested.is_none();
                    data.alpha_ref = tested;
                    if let (Some(surfaces), None) = (cache.look.surfaces, decal) {
                        surfaces
                            .0
                            .dress(&cache.look, material, &mut data, &mut |t| {
                                cache.textures.get(install, cache.install.get(), t, size)
                            });
                    }
                    if let Some(d) = decal {
                        // On the model, it blends over everything opaque;
                        // 117's grounds and plaza tiles draw with the ground.
                        data.material = gpu::Material::Decal(gpu::Decal {
                            opacity: d.opacity,
                            bias: d.bias,
                            grazing: cache.look.decals.grazing,
                            ground: d.before_decals,
                            edge: None,
                        });
                        data.alpha_map = alpha_map.clone();
                        if cache.look.decals.mesh_as_ground {
                            data.specular = gpu::Specular::MATTE_FADES;
                        }
                    }
                    (!data.indices.is_empty()).then_some(data)
                };
                let mut data = build(rdm, g)?;
                let shipped = shipped();
                // The renderer draws four levels; cutouts keep one for their
                // impostor.
                let room = if cutout { 2 } else { 3 };
                // Skinned meshes move as their joints do, not thin.
                let ranked = (cutout && shipped.is_empty() && data.skin.is_empty())
                    .then(|| rank(&data))
                    .flatten();
                let thinned = ranked.is_some();
                let report = |e: anyhow::Error| eprintln!("{cfg} part {}: {e:#}", part.name);
                // Compressed: the renderer decodes them as it packs.
                let (mesh, lods) = if ranked.is_some() || shipped.is_empty() {
                    // Simplified levels draw from the full mesh's vertices.
                    let keeps = match ranked {
                        // Thinned at every level, then the impostor. Laurels'
                        // 17-triangle leaf clumps go from 8.8k triangles to
                        // 5.0k and 3.7k; pine needle cards stay as they are.
                        // In the close view over the continental island at
                        // 5K, 6.1M triangles became 4.0M and 2.92 ms 2.65.
                        Some(ranked) => {
                            data = ranked;
                            [0.4, 0.15]
                        }
                        // Opaque meshes the game ships no levels for (7,349
                        // in 117): simplified copies at a half and a sixth
                        // of the triangles.
                        None => [0.5, 0.15],
                    };
                    Mesh::compact_levels(data, &keeps).map_err(report).ok()?
                } else {
                    // The finest shipped levels; past the renderer's room
                    // the coarsest go, which only ever draw a few pixels.
                    let lods = (shipped.iter().map_while(|r| build(r, g)))
                        .take(room)
                        .filter_map(|d| Mesh::compact(d).ok().map(Arc::new))
                        .collect();
                    (Arc::new(Mesh::compact(data).map_err(report).ok()?), lods)
                };
                let counts: Vec<usize> = std::iter::once(&mesh)
                    .chain(&lods)
                    .map(|m| m.triangles())
                    .collect();
                Some(Built {
                    mesh,
                    // Cutouts with shipped levels (stone pines) too. Never
                    // one that dyes or glows: impostors bake neither, and
                    // a far window should still light. No shipped cutout
                    // material does either (their shaders, 21 to 23, have
                    // no such switches).
                    foliage: cutout
                        && (thinned || !lods.is_empty())
                        && material.dye_mask.is_none()
                        && material.glow.is_none(),
                    lods,
                    counts,
                    thinned,
                })
            });
            let Some(built) = built else { continue };
            // Foliage levels count world metres whatever the placement's
            // scale, which a size-based switch cannot know. From twice the
            // size thinning starts at, the coarser levels looked identical
            // at 5K; drawn at every size, the same, but zoomed-in trees
            // should keep their full mesh.
            let ppm = if built.thinned {
                vec![THIN_FULL * 2., THIN_FULL * 1.2]
            } else {
                let strays: Vec<Option<f32>> = built.lods.iter().map(|l| l.strays()).collect();
                level_ppm(extent, &built.counts, &strays)
            };
            // Thinning meshes and decals keep still: the renderer poses
            // lit meshes whole. Posed pieces get no impostor, which would
            // bake one moment.
            // TODO(alex): a moving cutout without shipped levels thins and
            // so stands still; none moves on the islands surveyed.
            let lit = matches!(built.mesh.data().material, gpu::Material::Lit);
            let pose = pose.clone().filter(|_| lit && !built.thinned);
            out.push(Piece {
                mesh: built.mesh.clone(),
                rdm: part.rdm.as_str().into(),
                local,
                element: outer.element.or(Some(part.element)),
                group: g as u32,
                foliage: built.foliage && pose.is_none(),
                follow: outer.follow,
                ppm: ppm[..built.lods.len()].to_vec(),
                lods: built.lods.clone(),
                pose,
            });
        }
    }
    for (k, decal) in model.decals.iter().enumerate() {
        add_box_decal(install, cache, cfg, k, decal, outer, placed, out);
    }
    if depth < MAX_DEPTH {
        for prop in &model.props {
            // Props on a joint stand on its bind pose, except the ones a
            // condition shows, as for parts above.
            if !outer.rigged && prop.rig.link.is_some() && prop.rig.shown_if.is_some() {
                continue;
            }
            let joint = (prop.rig.link)
                .filter(|_| !outer.rigged)
                .and_then(|l| model.joint_placement(l))
                .unwrap_or(Transform::IDENTITY);
            let mut chain = Vec::new();
            if outer.posed {
                let hang = (prop.rig.link.filter(|_| !outer.rigged))
                    .and_then(|l| Hang::of(model, clip.as_ref(), l))
                    .unwrap_or(Hang::None);
                chain = outer.chain.clone();
                chain.push(Link {
                    hang,
                    transform: prop.transform,
                    motion: prop.rig.motion,
                });
            }
            let nested = Outer {
                at: outer.at.then(&joint).then(&prop.transform),
                rigged: outer.rigged,
                element: outer.element.or(Some(prop.element)),
                deform: outer.deform || prop.deform_to_terrain,
                follow: outer.follow || prop.adapt_terrain_height,
                posed: outer.posed,
                chain,
            };
            // Shipped cfgs reference ~130 missing prop cfgs; skip quietly.
            if let Some(Ok(model)) = read.get(&prop.cfg) {
                add_parts(install, cache, read, model, &nested, placed, depth + 1, out);
            }
        }
    }
}

/// Grid cells per metre of a box decal, which the renderer drapes vertex by
/// vertex over the terrain: two, the 117 terrain's sample spacing, up to 64
/// a side (decal_pbr.fx's tessellation factor).
const DECAL_CELLS_PER_METRE: f32 = 2.;

/// Box decal `k` of `cfg` as a piece: a grid over the box's unit square,
/// textured as the game maps it, draped over the terrain and faded at the
/// box's sides by the renderer. Vehicle tracks (drawn at run time),
/// decals without a texture and fully clear ones (footprint placeholders
/// with cOpacity 0) are left out.
#[allow(clippy::too_many_arguments)]
fn add_box_decal(
    install: &Install,
    cache: &Cache,
    cfg: &str,
    k: usize,
    decal: &anno_formats::model::Decal,
    outer: &Outer,
    placed: f32,
    out: &mut Vec<Piece>,
) {
    use anno_formats::model::DecalFade;
    let Some(material) = &decal.material else {
        return;
    };
    if decal.kind == "Vehicle" || decal.opacity <= 0. || material.diffuse.is_none() {
        return;
    }
    let [ex, _, ez] = decal.extents;
    if ex <= 0. || ez <= 0. {
        return;
    }
    let size = decal_texture_size(decal, placed);
    let key = format!("{cfg}#decal{k}@{size}");
    let built = cache.meshes.get(key, || {
        let map = |t: &Option<String>, size| {
            t.as_deref()
                .and_then(|t| cache.textures.get(install, cache.install.get(), t, size))
        };
        let texture = map(&material.diffuse, size)?;
        let height = map(&decal.height_map, size / 2);
        let cells = |e: f32| ((2. * e * DECAL_CELLS_PER_METRE).ceil() as u32).clamp(1, 64);
        let (nx, nz) = (cells(ex), cells(ez));
        let tint = material
            .diffuse_color
            .map(|c| (c.clamp(0., 1.) * 255.) as u32);
        let [u0, v0, u1, v1] = decal.uv_rect;
        let mut data = MeshData {
            texture: Some(texture),
            normal_map: map(&material.normal, size),
            surface_map: map(&material.metallic, size / 2),
            terrain_tint: material.terrain_tint,
            terrain_adapt: Some(gpu::TerrainAdapt::Drape),
            world_tiling: material.world_tiling.map(|t| gpu::WorldTiling {
                scale: t.scale,
                diagonal: t.diagonal,
            }),
            // 1800's decal_pbr.fx fades matte specular; 117 lights box
            // decals as the ground they cover (`DecalLook::mesh_as_ground`).
            specular: gpu::Specular::MATTE_FADES,
            ..Default::default()
        };
        for j in 0..=nz {
            for i in 0..=nx {
                let (x, z) = (i as f32 / nx as f32 - 0.5, j as f32 / nz as f32 - 0.5);
                data.positions.push([x, 0., z]);
                data.normals.push([0., 1., 0.]);
                data.colors.push(tint[0] << 16 | tint[1] << 8 | tint[2]);
                // U along +X and V against +Z, into the atlas cell.
                let (u, v) = (x + 0.5, 0.5 - z);
                let uv = [u0 + u * (u1 - u0), v0 + v * (v1 - v0)];
                data.uvs
                    .push(decal.uv_transform.map_or(uv, |t| t.apply(uv)));
            }
        }
        for j in 0..nz {
            for i in 0..nx {
                let a = j * (nx + 1) + i;
                data.indices
                    .extend([a, a + nx + 1, a + 1, a + 1, a + nx + 1, a + nx + 2]);
            }
        }
        // Without a fade the whole box draws, as in game.
        let edge = decal.fade.map(|f| gpu::DecalEdge {
            extents: [ex, ez],
            fade: match f {
                DecalFade::Grid {
                    distance,
                    threshold,
                    falloff,
                } => gpu::EdgeFade::Grid {
                    distance,
                    height: height.as_ref().map(|_| [threshold, falloff]),
                },
                DecalFade::Smooth { border, fade } => gpu::EdgeFade::Smooth { border, fade },
            },
        });
        // The height map frays a grid fade; it rides in the alpha map's slot.
        if let Some(gpu::DecalEdge {
            fade: gpu::EdgeFade::Grid {
                height: Some(_), ..
            },
            ..
        }) = edge
        {
            data.alpha_map = height;
        }
        // decal_pbr.fx moves every box decal toward the camera by the
        // game's bias, and its frame draws them on the ground before the
        // models (`DecalLook`).
        let look = cache.look.decals;
        data.material = gpu::Material::Decal(gpu::Decal {
            opacity: decal.opacity,
            bias: look.box_bias,
            grazing: look.grazing,
            ground: look.box_on_ground,
            edge,
        });
        let mesh = Mesh::compact(data)
            .map_err(|e| eprintln!("{cfg} decal {k}: {e:#}"))
            .ok()?;
        Some(Built {
            mesh: Arc::new(mesh),
            lods: Vec::new(),
            counts: vec![(nx * nz * 2) as usize],
            foliage: false,
            thinned: false,
        })
    });
    let Some(built) = built else { return };
    // The box's unit square scaled to its extents; Y stays 1 (the drape
    // replaces it).
    let box_scale = Transform {
        scale: [2. * ex, 1., 2. * ez],
        ..Transform::IDENTITY
    };
    out.push(Piece {
        mesh: built.mesh.clone(),
        rdm: "".into(),
        local: outer.at.then(&decal.transform).then(&box_scale).matrix(),
        element: outer.element.or(Some(decal.element)),
        group: 0,
        lods: Vec::new(),
        ppm: Vec::new(),
        foliage: false,
        follow: false,
        pose: None,
    });
}

#[cfg(test)]
mod tests {
    use super::{Cache, level_ppm, load, rank, texture_size};
    use anno_core::files::ModelLook;
    use anno_core::game::Module;
    use gpui_editor_kit::gpu::MeshData;
    use std::sync::Arc;

    #[test]
    fn coarser_levels_wait_until_their_triangles_are_small_on_screen() {
        // celtic_rock_06_a and its four shipped levels, 6 m across.
        let ppm = level_ppm(6., &[28435, 1338, 389, 126, 36], &[]);
        let expect = [30.5, 16.4, 9.4, 5.];
        assert!(
            ppm.iter().zip(expect).all(|(p, e)| (p - e).abs() < 0.1),
            "{ppm:?}"
        );
        // A level finer than the one before never takes over earlier.
        assert_eq!(level_ppm(1., &[100, 10, 400], &[]), [15.811388, 15.811388]);
    }

    // Every texture an island's models load comes at the side sized for its
    // largest user before any loaded (Textures::size): no load asks past
    // it, so which cfg a core takes first no longer decides a shared
    // texture's side. The pirate island's near cliffs came out a level
    // blurrier in some runs before.
    #[test]
    fn island_textures_load_at_their_largest_users_size() {
        let Some((install, mut editor)) =
            super::super::test_support::open(super::super::test_support::PIRATE)
        else {
            return;
        };
        let install = Arc::new(install);
        while let Some(job) = editor.models_job(install.clone()) {
            editor.models_loaded(job());
        }
        let table = editor.library.cache.textures.0.lock().unwrap();
        let loaded: Vec<_> = table.slots.iter().filter(|s| s.2.is_some()).collect();
        assert!(loaded.len() > 100, "{} textures", loaded.len());
        for (path, &id) in &table.ids {
            let (wanted, have, _) = &table.slots[id as usize];
            assert!(
                *have == 0 || have == wanted,
                "{path}: {have} loaded, {wanted} wanted"
            );
        }
    }

    #[test]
    fn textures_follow_model_size() {
        assert_eq!(texture_size(2.), 256);
        assert_eq!(texture_size(8.), 512);
        assert_eq!(texture_size(30.), 2048);
        assert_eq!(texture_size(300.), 2048);
    }

    #[test]
    fn foliage_ranks_its_leaf_clumps_behind_the_trunk() {
        let mut data = MeshData::default();
        // A trunk of 20 joined triangles (a fan), then 200 one-triangle clumps.
        data.positions.push([0., 0., 0.]);
        for k in 0..21 {
            let a = k as f32 / 20. * std::f32::consts::TAU;
            data.positions.push([a.cos(), 5., a.sin()]);
        }
        for k in 0..20u32 {
            data.indices.extend([0, k + 1, k + 2]);
        }
        for k in 0..200u32 {
            let base = data.positions.len() as u32;
            let x = k as f32;
            data.positions
                .extend([[x, 10., 0.], [x + 0.2, 10., 0.], [x, 10.2, 0.]]);
            data.indices.extend([base, base + 1, base + 2]);
        }
        let ranked = rank(&data).unwrap();
        let thinning = ranked.thinning.as_ref().unwrap();
        // Every triangle stays, the trunk's first at rank 0.
        assert_eq!(ranked.indices.len(), data.indices.len());
        let ranks: Vec<f32> = (ranked.indices.as_chunks::<3>().0.iter())
            .map(|t| thinning.parts[t[0] as usize][3])
            .collect();
        assert!(ranks[..20].iter().all(|&r| r == 0.));
        assert!(ranks[20..].iter().all(|&r| r > 0. && r <= 1.));
        assert!(ranks.is_sorted());
        // About a quarter of the clumps rank within the first quarter.
        let quarter = ranks.iter().filter(|&&r| r > 0. && r <= 0.25).count();
        assert!((30..=70).contains(&quarter), "{quarter} of 200 clumps");
        // Clump vertices carry their clump's centre: the first clump is
        // vertices 22..25 around (0.2 / 3, 10 + 0.2 / 3, 0).
        let c = thinning.parts[22];
        let expect = [0.2 / 3., 10. + 0.2 / 3., 0.];
        assert!((0..3).all(|a| (c[a] - expect[a]).abs() < 1e-5), "{c:?}");
        // Too few loose parts: nothing to thin.
        data.indices.truncate(60 + 3 * 10);
        assert!(rank(&data).is_none());
    }

    #[test]
    fn meshes_of_equal_cards_thin_every_card() {
        // 100 two-triangle quads and nothing larger, like a cypress: no
        // trunk, so none always draws.
        let mut data = MeshData::default();
        for k in 0..100u32 {
            let x = k as f32;
            data.positions.extend([
                [x, 0., 0.],
                [x + 0.5, 0., 0.],
                [x + 0.5, 1., 0.],
                [x, 1., 0.],
            ]);
            data.indices.extend([0, 1, 2, 0, 2, 3].map(|i| 4 * k + i));
        }
        let ranked = rank(&data).unwrap();
        let parts = &ranked.thinning.as_ref().unwrap().parts;
        assert!(parts.iter().all(|p| p[3] > 0.));
    }
    use anno_formats::vfs::Install;

    // Needs the games: a 117 residence's windows light by night over a
    // room; an 1800 factory's windows light by night; an 1800 harbour's
    // trim takes the owner's colour.
    #[test]
    fn buildings_carry_their_dye_and_glow() {
        use anno_render::a7m_models::worn;
        let data = |p: &super::Piece| p.mesh.data().clone();
        if let Some(dir) = anno_formats::vfs::install_dir("ANNO117_DIR") {
            let families = crate::games::profile(anno_core::game::Game::Anno117).archive_families;
            let install = Install::open(dir, families).unwrap();
            let cfg = "data/base/graphics/roman/buildings/residences/residence_tier01_09/residence_tier01_09.cfg";
            let pieces = load(
                &install,
                &Cache::new(&anno117::MODULE.model_look()),
                cfg,
                1.,
            )
            .unwrap();
            let window = (pieces.iter().map(data))
                .filter_map(|d| worn(&d))
                .find(|w| w.room.is_some())
                .expect("a window with a room");
            assert!(window.night && window.steady);
            assert_eq!(window.room, Some([0.2, 2., 0., 0.]));
            assert!(window.night_gain < 0.1, "dim: {}", window.night_gain);
        }
        if let Some(dir) = anno_formats::vfs::install_dir("ANNO1800_DIR") {
            let families = crate::games::profile(anno_core::game::Game::Anno1800).archive_families;
            let install = Install::open(dir, families).unwrap();
            let cache = Cache::new(&anno1800::MODULE.model_look());
            let factory = "data/graphics/buildings/production/heavy_08/heavy_08.cfg";
            let pieces = load(&install, &cache, factory, 1.).unwrap();
            let night = (pieces.iter().map(data))
                .filter(|d| worn(d).is_some_and(|w| w.night))
                .count();
            assert!(night >= 6, "{night} pieces light by night");
            let harbour = "data/graphics/buildings/special/harbor_06/harbor_06.cfg";
            let pieces = load(&install, &cache, harbour, 1.).unwrap();
            assert!(
                pieces
                    .iter()
                    .any(|p| worn(p.mesh.data()).is_some_and(|w| w.dye))
            );
        }
    }

    // Needs the game: loads sharing a cache share the meshes they build, so
    // the renderer packs each once; a placement big enough for larger
    // textures builds its own.
    #[test]
    fn loads_share_the_meshes_they_build() {
        let Some(dir) = anno_formats::vfs::install_dir("ANNO117_DIR") else {
            eprintln!("skipping: ANNO117_DIR unset");
            return;
        };
        let families = crate::games::profile(anno_core::game::Game::Anno117).archive_families;
        let install = Install::open(dir, families).unwrap();
        let cfg = "data/base/graphics/roman/environment/props/vegetation/trees/olive_trees/olive_tree_07.cfg";
        let cache = Cache::new(&ModelLook::PLAIN);
        let a = load(&install, &cache, cfg, 1.).unwrap();
        let b = load(&install, &cache, cfg, 1.).unwrap();
        assert!(!a.is_empty());
        assert!(a.iter().zip(&b).all(|(a, b)| Arc::ptr_eq(&a.mesh, &b.mesh)));
        let alone = load(&install, &Cache::new(&ModelLook::PLAIN), cfg, 1.).unwrap();
        assert!(!Arc::ptr_eq(&a[0].mesh, &alone[0].mesh));
        let big = load(&install, &cache, cfg, 16.).unwrap();
        assert!(!Arc::ptr_eq(&a[0].mesh, &big[0].mesh));
    }

    // Needs the game: the bronze smithy's ground plate drapes by vertex
    // color (its foot follows, its top stays), its props deform whole, its
    // building stays rigid, and its bushes ride the ground.
    #[test]
    fn building_parts_carry_the_terrain_adaption_their_cfg_asks_for() {
        use gpui_editor_kit::gpu::TerrainAdapt;
        let Some(dir) = anno_formats::vfs::install_dir("ANNO117_DIR") else {
            eprintln!("skipping: ANNO117_DIR unset");
            return;
        };
        let families = crate::games::profile(anno_core::game::Game::Anno117).archive_families;
        let install = Install::open(dir, families).unwrap();
        let cfg = "data/base/graphics/celtic/buildings/production/heavy/bronze/heavy_bronze.cfg";
        let pieces = load(&install, &Cache::new(&ModelLook::PLAIN), cfg, 1.).unwrap();
        let named = |stem: &str| {
            let p = (pieces.iter())
                .find(|p| p.rdm.contains(stem))
                .unwrap_or_else(|| panic!("no {stem}"));
            (
                p.mesh.data().terrain_adapt,
                p.mesh.geometry().colors,
                p.follow,
            )
        };
        let (adapt, colors, _) = named("heavy_celtic_bronze_ground");
        assert_eq!(adapt, Some(TerrainAdapt::Relative));
        let rigid: Vec<u32> = colors.iter().map(|c| c >> 24).collect();
        assert!(
            rigid.contains(&0) && rigid.iter().any(|&r| r > 64),
            "{rigid:?}"
        );
        assert_eq!(
            named("heavy_celtic_bronze_props").0,
            Some(TerrainAdapt::Relative)
        );
        assert_eq!(named("heavy_celtic_bronze_building").0, None);
        assert!(
            pieces
                .iter()
                .any(|p| p.rdm.contains("/bushes/") && p.follow)
        );
    }

    // Needs the game: a dirt ground ornament is one mesh decal, blended with
    // its gradient alpha map and the higher bias, drawn with the ground
    // (RENDER_BEFORE_DECALS); it casts no shadow.
    #[test]
    fn dirt_grounds_load_as_mesh_decals() {
        use gpui_editor_kit::gpu::{Decal, Material};
        let Some(dir) = anno_formats::vfs::install_dir("ANNO117_DIR") else {
            eprintln!("skipping: ANNO117_DIR unset");
            return;
        };
        let families = crate::games::profile(anno_core::game::Game::Anno117).archive_families;
        let install = Install::open(dir, families).unwrap();
        let cfg = "data/base/graphics/celtic/buildings/ornaments/grounds/dirt_ground/dirt_ground_02x02_v1.cfg";
        let pieces = load(&install, &Cache::new(&ModelLook::PLAIN), cfg, 1.).unwrap();
        assert!(!pieces.is_empty());
        for p in &pieces {
            let data = p.mesh.data();
            assert_eq!(
                data.material,
                Material::Decal(Decal {
                    opacity: 1.,
                    bias: 0.2,
                    grazing: true,
                    ground: true,
                    edge: None,
                })
            );
            assert!(data.texture.is_some() && data.alpha_map.is_some());
            assert!(!data.ignore_alpha && !p.foliage);
        }
    }

    // Needs the game: a fireplace's box decals become draped decal pieces
    // on their box's unit grid; the institution's only box decal is a clear
    // footprint placeholder (cOpacity 0) and is left out.
    #[test]
    fn box_decals_load_as_draped_grid_pieces() {
        use gpui_editor_kit::gpu::{Material, TerrainAdapt};
        let Some(dir) = anno_formats::vfs::install_dir("ANNO117_DIR") else {
            eprintln!("skipping: ANNO117_DIR unset");
            return;
        };
        let families = crate::games::profile(anno_core::game::Game::Anno117).archive_families;
        let install = Install::open(dir, families).unwrap();
        let fireplace = "data/base/graphics/celtic/buildings/ornaments/hall_of_fame/fireplaces/fireplace_01.cfg";
        let pieces = load(&install, &Cache::new(&ModelLook::PLAIN), fireplace, 1.).unwrap();
        let boxed: Vec<_> = (pieces.iter())
            .filter(|p| p.mesh.data().terrain_adapt == Some(TerrainAdapt::Drape))
            .collect();
        assert!(!boxed.is_empty());
        for p in boxed {
            let data = p.mesh.geometry();
            assert!(matches!(data.material, Material::Decal(d) if d.bias == 0.2));
            assert!(data.texture.is_some());
            let (lo, hi) = (data.positions.iter()).fold(([1f32; 3], [-1f32; 3]), |(lo, hi), p| {
                (
                    std::array::from_fn(|a| lo[a].min(p[a])),
                    std::array::from_fn(|a| hi[a].max(p[a])),
                )
            });
            assert_eq!((lo, hi), ([-0.5, 0., -0.5], [0.5, 0., 0.5]));
        }
        let institution =
            "data/base/graphics/celtic/buildings/institution/institution_01/institution_01.cfg";
        let pieces = load(&install, &Cache::new(&ModelLook::PLAIN), institution, 1.).unwrap();
        assert!(
            !pieces
                .iter()
                .any(|p| p.mesh.data().terrain_adapt == Some(TerrainAdapt::Drape))
        );
    }

    // Needs the game: the olive tree has bark and cutout leaf pieces.
    #[test]
    fn olive_tree_loads_textured_pieces() {
        let Some(dir) = anno_formats::vfs::install_dir("ANNO117_DIR") else {
            eprintln!("skipping: ANNO117_DIR unset");
            return;
        };
        let families = crate::games::profile(anno_core::game::Game::Anno117).archive_families;
        let install = Install::open(dir, families).unwrap();
        let cfg = "data/base/graphics/roman/environment/props/vegetation/trees/olive_trees/olive_tree_07.cfg";
        let pieces = load(&install, &Cache::new(&ModelLook::PLAIN), cfg, 1.).unwrap();
        assert!(pieces.len() >= 2, "{}", pieces.len());
        assert!(pieces.iter().all(|p| p.mesh.data().texture.is_some()));
        let (lo, hi) = pieces
            .iter()
            .fold(([f32::MAX; 3], [f32::MIN; 3]), |(lo, hi), p| {
                let [a, b] = p.mesh.bounds();
                (
                    std::array::from_fn(|i| lo[i].min(a[i])),
                    std::array::from_fn(|i| hi[i].max(b[i])),
                )
            });
        // Roughly the cfg's MeshCenter ± MeshExtent (≈ 2 m tree).
        assert!(hi[1] - lo[1] > 1. && hi[1] - lo[1] < 4., "{lo:?} {hi:?}");
    }

    // Needs Anno 1800: the mine rail is one 2 m segment along x, 0.5 m
    // wide with both trestle halves, rails, sleepers and belts (8 meshes in
    // its rdm), its track on top at y 0 and legs below; the editor bends it
    // along work areas' paths (`rail::Template`).
    #[test]
    fn mine_rail_is_a_two_metre_segment_along_x() {
        let Some(dir) = anno_formats::vfs::install_dir("ANNO1800_DIR") else {
            eprintln!("skipping: ANNO1800_DIR unset");
            return;
        };
        let families = crate::games::profile(anno_core::game::Game::Anno1800).archive_families;
        let install = Install::open(dir, families).unwrap();
        let cfg = "data/graphics/buildings/production/mining_01/mining_rail_01.cfg";
        let pieces = load(&install, &Cache::new(&ModelLook::PLAIN), cfg, 1.).unwrap();
        assert!(!pieces.is_empty());
        let (lo, hi) = pieces
            .iter()
            .fold(([f32::MAX; 3], [f32::MIN; 3]), |(lo, hi), p| {
                let [a, b] = p.mesh.bounds();
                let t = |v| gpui_editor_kit::math::transform(&p.local, v);
                let (a, b) = (t(a), t(b));
                (
                    std::array::from_fn(|i| lo[i].min(a[i]).min(b[i])),
                    std::array::from_fn(|i| hi[i].max(a[i]).max(b[i])),
                )
            });
        assert!((hi[0] - lo[0] - 2.).abs() < 0.1, "{lo:?} {hi:?}");
        assert!(
            hi[2] - lo[2] < 0.7 && hi[1].abs() < 0.2 && lo[1] < -2.,
            "{lo:?} {hi:?}"
        );
    }

    // Needs the game: DLC 3's terrain blocker draws its rock and sand from
    // three splat layers over a flat grey albedo that only tints them. Its
    // layers repeat every rdm's width (about 5 m) however large it is
    // placed, so they load at that size, the control map at the model's.
    #[test]
    fn terrain_blockers_load_their_splat_layers() {
        let Some(dir) = anno_formats::vfs::install_dir("ANNO117_DIR") else {
            eprintln!("skipping: ANNO117_DIR unset");
            return;
        };
        let families = crate::games::profile(anno_core::game::Game::Anno117).archive_families;
        let install = Install::open(dir, families).unwrap();
        let cfg = "data/dlc03/graphics/egypt/environment/props/terrainblocker/egypt_terrainblocker_01/egypt_terrainblocker_01_b.cfg";
        let cache = Cache::new(&ModelLook::PLAIN);
        let pieces = load(&install, &cache, cfg, 2.56).unwrap();
        assert!(!pieces.is_empty());
        // Sized before they loaded, as every texture is
        // (island_textures_load_at_their_largest_users_size).
        let table = cache.textures.0.lock().unwrap();
        let whole: Vec<_> = (table.ids.iter())
            .filter(|(p, _)| p.ends_with("#whole"))
            .collect();
        assert_eq!(
            whole.len(),
            7,
            "the control map and three layers' albedo and normal"
        );
        for (path, &id) in &table.ids {
            let (wanted, have, _) = &table.slots[id as usize];
            assert!(
                *have == 0 || have == wanted,
                "{path}: {have} loaded, {wanted} wanted"
            );
        }
        drop(table);
        for p in &pieces {
            let splat = p.mesh.data().splat.clone().expect("a splat");
            assert!(splat.tint == Some(2.) && splat.mix_normals && splat.scale_invariant);
            let layers: Vec<_> = splat.layers.iter().flatten().collect();
            assert_eq!(layers.len(), 3);
            assert!(
                layers
                    .iter()
                    .all(|l| l.albedo.size() == (256, 256) && l.normal.is_some())
            );
            assert_eq!(splat.control.size(), (2048, 2048));
            // Coarser levels keep it.
            assert!(p.lods.iter().all(|l| l.data().splat.is_some()));
        }
    }

    // Needs the game: the pyramid dunes' sand tiles in the world
    // (cSplatWorldTiling), so its layers keep their splat and load sized
    // for one world repeat: 1 / 0.18 m and 1 / 0.035 m at 48 texels a metre.
    #[test]
    fn world_tiled_splat_layers_load_for_a_world_repeat() {
        let Some(dir) = anno_formats::vfs::install_dir("ANNO117_DIR") else {
            eprintln!("skipping: ANNO117_DIR unset");
            return;
        };
        let families = crate::games::profile(anno_core::game::Game::Anno117).archive_families;
        let install = Install::open(dir, families).unwrap();
        let cfg = "data/dlc03/graphics/egypt/environment/props/terrainblocker/egypt_pyramiddune_01/egypt_pyramiddune_01_c.cfg";
        let cache = Cache::new(&ModelLook::PLAIN);
        let pieces = load(&install, &cache, cfg, 1.).unwrap();
        assert!(!pieces.is_empty());
        for p in &pieces {
            let splat = p.mesh.data().splat.clone().expect("a splat");
            assert!(splat.world_tiling && splat.scale_invariant);
            let sizes: Vec<_> = (splat.layers.iter().flatten())
                .map(|l| l.albedo.size())
                .collect();
            assert_eq!(sizes, [(512, 512), (2048, 2048)]);
        }
    }

    // Needs the game: a 16 m rock placed at 3.6× loads its 2048² atlas whole.
    #[test]
    fn scaled_placements_load_bigger_textures() {
        let Some(dir) = anno_formats::vfs::install_dir("ANNO117_DIR") else {
            eprintln!("skipping: ANNO117_DIR unset");
            return;
        };
        let families = crate::games::profile(anno_core::game::Game::Anno117).archive_families;
        let install = Install::open(dir, families).unwrap();
        let cfg = "data/base/graphics/celtic/environment/props/rocks/celtic_mountain_element_01/celtic_mountain_element_01.cfg";
        let side = |placed| {
            let pieces = load(&install, &Cache::new(&ModelLook::PLAIN), cfg, placed).unwrap();
            let texture = pieces[0].mesh.data().texture.as_ref().unwrap();
            texture.size().0
        };
        assert_eq!((side(1.), side(3.6)), (1024, 2048));
    }
}
