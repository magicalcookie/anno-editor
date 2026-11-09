// Owns the island's model library: `.cfg` → GPU meshes (one per part
// material, nested props flattened in), loaded off the UI thread in batches
// and instanced across every prop and object that uses them.
use anno_formats::{
    model::{Alpha, Model, Transform},
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
/// A 2 m bush takes 256², an 8 m tree 512², a 30 m rock 2048².
const TEXELS_PER_METRE: f32 = 48.;
const TEXTURE_SIZES: std::ops::RangeInclusive<u32> = 256..=2048;

/// Texture side for a model `extent` metres across.
fn texture_size(extent: f32) -> u32 {
    ((extent * TEXELS_PER_METRE) as u32)
        .next_power_of_two()
        .clamp(*TEXTURE_SIZES.start(), *TEXTURE_SIZES.end())
}
/// Nested prop cfgs deeper than this are skipped (guards against cycles).
const MAX_DEPTH: usize = 4;
/// Cfgs per background job: every missing one. Each job's models give the
/// renderer a new piece list, which rebuilds its arena and re-bakes its
/// impostors; in batches of 96 the continental island's trees vanished
/// and came back, with a hitch, six times on load.
pub const BATCH: usize = usize::MAX;

/// One drawable piece of a model in model space.
pub struct Piece {
    pub mesh: Arc<Mesh>,
    /// The part's mesh file, as the cfg names it: what an asset preview of
    /// that `.rdm` borrows this piece's materials by.
    pub rdm: Arc<str>,
    pub local: Mat4,
    /// Coarser levels, coarsest last: the game's own where it ships them,
    /// else simplified copies (foliage thins on the GPU at every level,
    /// `MeshData::thinning`); empty for meshes too coarse to simplify.
    pub lods: Vec<Arc<Mesh>>,
    /// Pixels per metre from which each level draws, the full mesh first;
    /// one per level but the last, which draws below every threshold.
    pub ppm: Vec<f32>,
    /// Cutout foliage that thins or has levels: an impostor far away.
    pub foliage: bool,
}

/// Foliage below this many (logical) pixels per metre draws as an
/// impostor: a quad showing the tree baked from the nearest of 64
/// directions, 40 px each. An 8 m tree is then under 64 px. At a 5K
/// viewport (scale 2) the low view over the continental island takes 3.06
/// ms on the GPU instead of 3.52 at 5 px/m, the nearest impostors a little
/// softer; at 12 it was 2.85, but trees in the close view lost their leaf
/// detail.
pub const IMPOSTOR_PPM: f32 = 8.;

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
/// full mesh, never rising.
fn level_ppm(extent: f32, levels: &[usize]) -> Vec<f32> {
    let mut last = f32::INFINITY;
    (levels.iter().skip(1))
        .map(|&tris| {
            let edge = extent / (tris.max(1) as f32).sqrt();
            last = (EDGE_PIXELS / edge.max(1e-4)).min(last);
            last
        })
        .collect()
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

/// GPU-ready textures shared across models; keyed by virtual path, with
/// the largest side loaded so far. A larger model asking for the same
/// texture loads it again bigger; meshes built earlier keep the smaller.
/// Block-compressed data goes to the GPU as is, never decoded on the CPU.
#[derive(Default)]
pub struct Textures(Mutex<HashMap<String, (u32, Option<Image>)>>);

/// A DDS header with its DX10 extension.
const DDS_HEADER: u64 = 148;

/// `path`'s mips from the first that fits `size`, still compressed, or
/// that mip decoded when the file is not block-compressed. The header,
/// then only those mips: a 4k BC7 atlas wanted at 1024² reads 1.4 of its 22
/// MB. Reading whole files moved 7 GB for the continental island's 542
/// models.
fn read_texture(install: &Install, path: &str, size: u32) -> Option<Result<Compressed, Image>> {
    let dds = Dds::parse(&install.read_range(path, 0, DDS_HEADER).ok()?).ok()?;
    let first = dds.mip_for(size);
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

impl Textures {
    /// With `shared`, the app's install, compressed textures are
    /// `Releasable`: the renderer lets their texels go once they are on
    /// the GPU (418 MB on the continental island) and reads them again
    /// should it need them anew.
    fn get(
        &self,
        install: &Install,
        shared: Option<&Arc<Install>>,
        path: &str,
        size: u32,
    ) -> Option<Image> {
        if let Some((have, t)) = self.0.lock().unwrap().get(path)
            && *have >= size
        {
            return t.clone();
        }
        let image = read_texture(install, path, size).and_then(|read| match read {
            Ok(c) => match shared {
                Some(shared) => {
                    let (install, path) = (shared.clone(), path.to_owned());
                    let reload = move || read_texture(&install, &path, size)?.ok();
                    Releasable::new(c, reload).ok().map(Image::from)
                }
                None => Some(Image::from(c)),
            },
            Err(rgba) => Some(rgba),
        });
        self.0
            .lock()
            .unwrap()
            .insert(path.to_owned(), (size, image.clone()));
        image
    }
}

/// What model loads share: textures, and the meshes built from rdm groups.
#[derive(Default)]
pub struct Cache {
    textures: Textures,
    meshes: Meshes,
    /// The app's install, for textures to read again (`Textures::get`).
    install: std::sync::OnceLock<Arc<Install>>,
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
#[derive(Default)]
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
            // Each cfg is independent (rdm parse, texture decode); the cores
            // take the next one as they finish, since one cfg can take 30
            // times another's time.
            let threads = std::thread::available_parallelism().map_or(4, |n| n.get());
            let next = std::sync::atomic::AtomicUsize::new(0);
            std::thread::scope(|scope| {
                let workers: Vec<_> = (0..threads.min(batch.len()))
                    .map(|_| {
                        let (install, cache, batch, next) = (&install, &cache, &batch, &next);
                        scope.spawn(move || {
                            let mut out = Vec::new();
                            loop {
                                let k = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                                let Some((cfg, placed)) = batch.get(k) else {
                                    return out;
                                };
                                let loaded = load(install, cache, cfg, *placed)
                                    .map_err(|e| eprintln!("{cfg}: {e:#}"))
                                    .ok()
                                    .map(Arc::new);
                                out.push((cfg.clone(), loaded));
                            }
                        })
                    })
                    .collect();
                workers
                    .into_iter()
                    .flat_map(|w| w.join().expect("model loader"))
                    .collect()
            })
        }
    }
}

/// Loads one cfg into pieces, its textures sized for the model drawn at
/// `placed` times its own size (the largest scale it is placed at). Missing
/// parts and textures are skipped, so a building with a dangling reference
/// still shows what it has.
// TODO(alex): a model scaled up after it loaded keeps its texture size until
// the island reopens.
pub fn load(install: &Install, cache: &Cache, cfg: &str, placed: f32) -> Result<Vec<Piece>> {
    let mut pieces = Vec::new();
    add_model(
        install,
        cache,
        cfg,
        &Transform::default(),
        placed.max(1.),
        0,
        &mut pieces,
    )?;
    Ok(pieces)
}

fn add_model(
    install: &Install,
    cache: &Cache,
    cfg: &str,
    outer: &Transform,
    placed: f32,
    depth: usize,
    out: &mut Vec<Piece>,
) -> Result<()> {
    let model = Model::load(install, cfg)?;
    for part in &model.parts {
        if part.bone_linked {
            // Positioned relative to a bone we do not pose.
            continue;
        }
        let Some(rdm) = &part.mesh else { continue };
        let local = outer.then(&part.transform).matrix();
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
        let (lo, hi) = rdm.bounds;
        let scale = (outer.then(&part.transform).scale.iter()).fold(0f32, |m, s| m.max(s.abs()));
        let extent = (0..3).map(|a| hi[a] - lo[a]).fold(0., f32::max) * scale;
        // celtic_mountain_element_01 is 16 m across but placed at up to
        // 3.6×: sized for 16 m its 2048² atlas loaded at 1024², 18 texels
        // per placed metre.
        let size = texture_size(extent * placed);
        for (g, group) in rdm.groups.iter().enumerate() {
            let Some(material) = part.materials.get(group.material as usize) else {
                continue;
            };
            let cutout = match material.alpha {
                Alpha::Opaque => false,
                Alpha::Cutout(_) | Alpha::Blend(_) => true,
                Alpha::Decal | Alpha::Hidden => continue,
            };
            let key = format!("{}#{g}@{size}{material:?}", part.rdm);
            let built = cache.meshes.get(key, || {
                let map = |t: &Option<String>, size| {
                    t.as_deref()
                        .and_then(|t| cache.textures.get(install, cache.install.get(), t, size))
                };
                let texture = map(&material.diffuse, size);
                // Normals in red/green with gloss in alpha; the metal map is
                // metalness in red, occlusion in alpha (model_pbr.fx via
                // functions_pbr.fxh), smooth enough for half the size. Blue of
                // the normal map is glow, unused.
                let normal_map = texture.as_ref().and(map(&material.normal, size));
                let surface_map = texture.as_ref().and(map(&material.metallic, size / 2));
                let tint = material
                    .diffuse_color
                    .map(|c| (c.clamp(0., 1.) * 255.) as u32);
                let color = tint[0] << 16 | tint[1] << 8 | tint[2];
                let build = |rdm: &Rdm, g: usize| -> Option<MeshData> {
                    let group = rdm.groups.get(g)?;
                    let range = group.start as usize..(group.start + group.count) as usize;
                    let indices = rdm.indices.get(range)?;
                    // Compact the group's vertices so each mesh only uploads its own.
                    let mut remap = HashMap::new();
                    let mut data = MeshData {
                        material: gpu::Material::Lit,
                        ..Default::default()
                    };
                    for &i in indices {
                        let next = remap.len() as u32;
                        let v = *remap.entry(i).or_insert_with(|| {
                            let i = i as usize;
                            data.positions.push(rdm.positions[i]);
                            data.uvs.push(rdm.uvs.get(i).copied().unwrap_or_default());
                            data.normals
                                .push(rdm.normals.get(i).copied().unwrap_or([0., 1., 0.]));
                            data.colors
                                .push(if texture.is_some() { color } else { 0xa49a88 });
                            next
                        });
                        data.indices.push(v);
                    }
                    if texture.is_none() {
                        data.uvs.clear();
                    }
                    data.texture = texture.clone();
                    data.normal_map = normal_map.clone();
                    data.surface_map = surface_map.clone();
                    data.terrain_tint = material.terrain_tint;
                    // Diffuse alpha carries gloss/height on opaque materials; only
                    // cutout and blend materials use it as coverage.
                    data.ignore_alpha = !cutout;
                    (!data.indices.is_empty()).then_some(data)
                };
                let mut data = build(rdm, g)?;
                let shipped = shipped();
                // The renderer draws four levels; cutouts keep one for their
                // impostor.
                let room = if cutout { 2 } else { 3 };
                let ranked = (cutout && shipped.is_empty())
                    .then(|| rank(&data))
                    .flatten();
                let thinned = ranked.is_some();
                let lods: Vec<MeshData> = if let Some(ranked) = ranked {
                    // Thinned at every level, then the impostor. Laurels'
                    // 17-triangle leaf clumps go from 8.8k triangles to 5.0k
                    // and 3.7k; pine needle cards stay as they are. In the
                    // close view over the continental island at 5K, 6.1M
                    // triangles became 4.0M and 2.92 ms 2.65.
                    data = ranked;
                    [0.4, 0.15]
                        .into_iter()
                        .map_while(|keep| data.simplified(keep))
                        .collect()
                } else if shipped.is_empty() {
                    // Opaque meshes the game ships no levels for (7,349 in
                    // 117): simplified copies at a half and a sixth of the
                    // triangles.
                    [0.5, 0.15]
                        .into_iter()
                        .map_while(|keep| data.simplified(keep))
                        .collect()
                } else {
                    // The finest shipped levels; past the renderer's room
                    // the coarsest go, which only ever draw a few pixels.
                    (shipped.iter().map_while(|r| build(r, g)))
                        .take(room)
                        .collect()
                };
                let counts: Vec<usize> = std::iter::once(&data)
                    .chain(&lods)
                    .map(|d| d.indices.len() / 3)
                    .collect();
                let lods: Vec<Arc<Mesh>> = lods
                    .into_iter()
                    .filter_map(|d| Mesh::compact(d).ok().map(Arc::new))
                    .collect();
                // Compressed: the renderer decodes them as it packs.
                let mesh = Mesh::compact(data)
                    .map_err(|e| eprintln!("{cfg} part {}: {e:#}", part.name))
                    .ok()?;
                Some(Built {
                    mesh: Arc::new(mesh),
                    // Cutouts with shipped levels (stone pines) too.
                    foliage: cutout && (thinned || !lods.is_empty()),
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
                level_ppm(extent, &built.counts)
            };
            out.push(Piece {
                mesh: built.mesh.clone(),
                rdm: part.rdm.as_str().into(),
                local,
                foliage: built.foliage,
                ppm: ppm[..built.lods.len()].to_vec(),
                lods: built.lods.clone(),
            });
        }
    }
    if depth < MAX_DEPTH {
        for prop in &model.props {
            let at = outer.then(&prop.transform);
            if let Err(e) = add_model(install, cache, &prop.cfg, &at, placed, depth + 1, out) {
                // Shipped cfgs reference ~130 missing prop cfgs; skip quietly.
                let _ = e;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{Cache, level_ppm, load, rank, texture_size};
    use gpui_editor_kit::gpu::MeshData;
    use std::sync::Arc;

    #[test]
    fn coarser_levels_wait_until_their_triangles_are_small_on_screen() {
        // celtic_rock_06_a and its four shipped levels, 6 m across.
        let ppm = level_ppm(6., &[28435, 1338, 389, 126, 36]);
        let expect = [30.5, 16.4, 9.4, 5.];
        assert!(
            ppm.iter().zip(expect).all(|(p, e)| (p - e).abs() < 0.1),
            "{ppm:?}"
        );
        // A level finer than the one before never takes over earlier.
        assert_eq!(level_ppm(1., &[100, 10, 400]), [15.811388, 15.811388]);
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

    // Needs the game: loads sharing a cache share the meshes they build, so
    // the renderer packs each once; a placement big enough for larger
    // textures builds its own.
    #[test]
    fn loads_share_the_meshes_they_build() {
        let Some(dir) = std::env::var_os("ANNO117_DIR") else {
            eprintln!("skipping: ANNO117_DIR unset");
            return;
        };
        let install = Install::open(dir).unwrap();
        let cfg = "data/base/graphics/roman/environment/props/vegetation/trees/olive_trees/olive_tree_07.cfg";
        let cache = Cache::default();
        let a = load(&install, &cache, cfg, 1.).unwrap();
        let b = load(&install, &cache, cfg, 1.).unwrap();
        assert!(!a.is_empty());
        assert!(a.iter().zip(&b).all(|(a, b)| Arc::ptr_eq(&a.mesh, &b.mesh)));
        let alone = load(&install, &Cache::default(), cfg, 1.).unwrap();
        assert!(!Arc::ptr_eq(&a[0].mesh, &alone[0].mesh));
        let big = load(&install, &cache, cfg, 16.).unwrap();
        assert!(!Arc::ptr_eq(&a[0].mesh, &big[0].mesh));
    }

    // Needs the game: the olive tree has bark and cutout leaf pieces.
    #[test]
    fn olive_tree_loads_textured_pieces() {
        let Some(dir) = std::env::var_os("ANNO117_DIR") else {
            eprintln!("skipping: ANNO117_DIR unset");
            return;
        };
        let install = Install::open(dir).unwrap();
        let cfg = "data/base/graphics/roman/environment/props/vegetation/trees/olive_trees/olive_tree_07.cfg";
        let pieces = load(&install, &Cache::default(), cfg, 1.).unwrap();
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

    // Needs the game: a 16 m rock placed at 3.6× loads its 2048² atlas whole.
    #[test]
    fn scaled_placements_load_bigger_textures() {
        let Some(dir) = std::env::var_os("ANNO117_DIR") else {
            eprintln!("skipping: ANNO117_DIR unset");
            return;
        };
        let install = Install::open(dir).unwrap();
        let cfg = "data/base/graphics/celtic/environment/props/rocks/celtic_mountain_element_01/celtic_mountain_element_01.cfg";
        let side = |placed| {
            let pieces = load(&install, &Cache::default(), cfg, placed).unwrap();
            let texture = pieces[0].mesh.data().texture.as_ref().unwrap();
            texture.size().0
        };
        assert_eq!((side(1.), side(3.6)), (1024, 2048));
    }
}
