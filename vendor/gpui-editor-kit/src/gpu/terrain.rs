// Owns the kit's ground: a heightfield drawn from its GPU height map as
// patches whose density follows the screen, and the pass that draws it
// (`TerrainPass`): its tiles per view and its opaque and shadow drawing.
// A game draws its own ground instead with a replacing Opaque pass
// (`Pass::replaces`) or a scene without the kit's (`super::GroundDraw::Data`);
// `Scene::terrain` still supplies the heights and colors placements drape
// on and take their tint from.
use super::LockAny as _;
use super::*;

/// A device-local surface for the kit's heightfield geometry. Group 3 is the
/// surface's own. The kit supplies world position, geometry normal and two
/// explicit world gradients to both shader functions.
/// `ground(uv, world, normal, dw)` returns a GroundSample (colour with coverage,
/// world normal, gloss, displacement). `ground_displacement` has the same arguments
/// and returns displacement in metres; it must be callable from a vertex stage.
pub trait Ground: Send + Sync {
    fn wgsl(&self) -> &str;
    fn layout(&self) -> &wgpu::BindGroupLayout;
    /// None keeps the terrain material visible while uploads finish.
    fn bind_group(&self) -> Option<&wgpu::BindGroup>;
    /// Upload within the frame budget. Return false to request another frame;
    /// an existing bind group can continue drawing during incremental edits.
    fn prepare(&mut self, _frame: &mut GroundPrepare<'_>) -> bool {
        true
    }
    /// Append uploaded changes since the given edit serial as world XZ bounds
    /// `[min_x, min_z, max_x, max_z]` and return the current serial. Report
    /// the whole extent when the requested serial has fallen out of history.
    fn edits(&self, since: u64, _out: &mut Vec<[f32; 4]>) -> u64 {
        since
    }
    /// Bump when all pages need baking again, including shader or layout changes.
    fn version(&self) -> u64 {
        0
    }
    /// Counts the uploads that moved the displaced surface, so the shadow
    /// maps redraw for those alone: paint over ground that displaces
    /// nothing leaves the caster where it was. `None`, the default, takes
    /// every edit of a ground with relief for one.
    fn relief_moves(&self) -> Option<u64> {
        None
    }
    /// Largest absolute displacement from the heightfield, in metres.
    fn relief_bound(&self) -> f32 {
        0.
    }
    /// A displacement map may be enabled even when all of its ranges are zero.
    fn has_relief(&self) -> bool {
        self.relief_bound() > 0.
    }
    /// Whether this world XZ area benefits from sub-sample geometry.
    fn rough(&self, _area: [f32; 4]) -> bool {
        false
    }
    /// Largest world footprint that reads surface normals. None for an
    /// unperturbed surface. Relief needs a normal page at every footprint.
    /// The ground's WGSL sees which as `ground_normals()`, so a ground
    /// without normal maps compiles their path out.
    fn normal_footprint(&self) -> Option<f32> {
        None
    }
    /// Whether `ground()` sets `GroundSample.glow`. Pages keep a glow atlas
    /// only for a ground that does, so others pay nothing for it.
    fn glow(&self) -> bool {
        false
    }
    /// Which specular terms light the ground; its shader compiles them in.
    fn specular(&self) -> super::Specular {
        super::Specular::FULL
    }
}

pub struct GroundPrepare<'a> {
    pub(super) gpu: &'a Gpu,
    pub encoder: &'a mut wgpu::CommandEncoder,
    pub budget: &'a mut Budget,
}
impl GroundPrepare<'_> {
    pub fn device(&self) -> &wgpu::Device {
        &self.gpu.device
    }
    pub fn queue(&self) -> &wgpu::Queue {
        &self.gpu.queue
    }
}

/// Resources available when mounting a surface for a renderer and terrain.
pub struct GroundInit<'a> {
    pub device: &'a wgpu::Device,
    pub grid: Grid,
}
pub type GroundFactory = Arc<dyn Fn(GroundInit<'_>) -> Box<dyn Ground> + Send + Sync>;

/// A heightfield drawn from its GPU height map as grid patches whose density
/// follows the screen: each frame every tile is culled and takes the
/// coarsest of the strides (1 to 16 samples, down to a quarter sample under
/// relief) whose cells still cover a few pixels, so distant ground costs a
/// few triangles a tile. Skirts hide the
/// cracks where strides meet. Nothing is meshed on the CPU: sculpting
/// updates `heights`, recoloring writes `colors`. Not pickable; ray-cast
/// the `Heightfield`. Keep the same `Arc` across scenes to keep the patch
/// meshes.
pub struct Terrain {
    pub heights: Arc<HeightMap>,
    /// RGBA8 per height sample, as `MeshData::color_map`.
    pub colors: Arc<ControlMap>,
    pub color_gain: f32,
    /// `colors` are a tint models take on (`MeshData::terrain_tint`); false
    /// while they show something else, such as heights or overlays.
    pub tints: bool,
    /// `Lit` (with `texture`), drawn while no ground is mounted or its
    /// ground prepares; UVs run 0..1 across the grid.
    pub material: Material,
    pub texture: Option<Image>,
}

/// Strides of the terrain's patch levels, in samples. Below one sample the
/// heights only interpolate; those levels exist for a ground's relief
/// (`Ground::rough`), which Anno tessellates its terrain for: at one
/// sample (0.5 m on 117's islands) its raised stones and cracks were
/// barely there.
const TERRAIN_STRIDES: [f32; 7] = [0.25, 0.5, 1., 2., 4., 8., 16.];
/// The first level at least a sample wide: the finest without relief, and
/// on tiles whose relief is flat.
const TERRAIN_SAMPLE_LEVEL: usize = 2;
/// Cells of a terrain tile at stride 1. A tile is one patch instance.
const TERRAIN_TILE: u32 = 64;
/// Tiles take the coarsest stride whose cells cover at most this many
/// pixels: finer would spend triangles on sub-pixel detail.
const TERRAIN_CELL_PX: f32 = 3.;
/// Tiles without relief coarsen further while the coarser patch strays
/// from the heights by at most this many pixels (`HeightMap::
/// patch_errors`, at the tile's nearest point): flat ground then draws a
/// few large triangles, which shade the same, and only bends keep cells
/// of TERRAIN_CELL_PX.
const TERRAIN_ERROR_PX: f32 = 0.5;
/// The levels whose stride spans more than a sample, the ones a tile can
/// coarsen to.
const TERRAIN_COARSE: usize = TERRAIN_STRIDES.len() - TERRAIN_SAMPLE_LEVEL - 1;
/// The shadow map draws every tile at stride 4: shadows are soft anyway.
const TERRAIN_SHADOW_LEVEL: usize = 4;
/// Skirt vertices store `SKIRT - drop` in y (gpu.wgsl): far below anything
/// a displaced mesh means by y, which it otherwise ignores.
const SKIRT: f32 = -1e6;
/// Chunks along a patch's side, its triangles stored chunk by chunk, so a
/// tile reaching past the view's edges draws only the chunks in it: the
/// nearest tiles are the finest and the likeliest to hang off the screen
/// (a quarter of the pirate island's quarter-sample chunks).
const PATCH_CHUNKS: u32 = 8;
/// Cells along a block's side: a patch's triangles go block by block,
/// each row by row, so a vertex's neighbours follow it closely. GPUs reuse
/// a shaded vertex only within a short window (NVIDIA's batches hold about
/// 32 vertices, a 4x4 block's 25); rows of 32 or 64 cells left that window
/// before the next row came back to them. With relief each vertex runs the
/// splat for its height: 1.38 to 1.30 ms of ground on pirate at 5K (2 and
/// 8 measured about the same as 4).
const PATCH_BLOCK: u32 = 4;

/// Cells along a patch's side: `cells` samples at `stride`.
fn patch_cells(cells: u32, stride: f32) -> u32 {
    ((cells as f32 / stride) as u32).max(1)
}

impl Terrain {
    /// The patch for stride `stride`: a `cells`² grid of cells `stride`
    /// samples wide, with a skirt along each edge as deep as a few cells.
    /// `chunked` stores its triangles chunk by chunk (PATCH_CHUNKS); a
    /// chunk, or else the whole patch, block by block (PATCH_BLOCK).
    fn patch(&self, cells: u32, stride: f32, chunked: bool) -> MeshData {
        let spacing = self.heights.grid().spacing;
        let n = patch_cells(cells, stride);
        let step = spacing * stride;
        let drop = (step * 4.).max(2.);
        let side = n + 1;
        let mut positions: Vec<Vec3> = (0..side * side)
            .map(|i| [(i % side) as f32 * step, 0., (i / side) as f32 * step])
            .collect();
        let at = |x: u32, z: u32| z * side + x;
        let mut indices = Vec::with_capacity((n * n * 6 + n * 24) as usize);
        // Chunk by chunk, each block by block; the skirts after them all.
        let chunks = if chunked { PATCH_CHUNKS } else { 1 };
        let c = n / chunks;
        for (cz, cx) in (0..chunks).flat_map(|z| (0..chunks).map(move |x| (z, x))) {
            for bz in (cz * c..(cz + 1) * c).step_by(PATCH_BLOCK as usize) {
                for bx in (cx * c..(cx + 1) * c).step_by(PATCH_BLOCK as usize) {
                    for z in bz..(bz + PATCH_BLOCK).min((cz + 1) * c) {
                        for x in bx..(bx + PATCH_BLOCK).min((cx + 1) * c) {
                            let (a, b, c, d) =
                                (at(x, z), at(x + 1, z), at(x + 1, z + 1), at(x, z + 1));
                            indices.extend([a, b, c, a, c, d]);
                        }
                    }
                }
            }
        }
        // Each edge in order, with a skirt vertex hanging under each of its
        // vertices.
        let edges: [Vec<u32>; 4] = [
            (0..side).map(|x| at(x, 0)).collect(),
            (0..side).map(|x| at(x, n)).collect(),
            (0..side).map(|z| at(0, z)).collect(),
            (0..side).map(|z| at(n, z)).collect(),
        ];
        for edge in edges {
            let first = positions.len() as u32;
            for &v in &edge {
                let [x, _, z] = positions[v as usize];
                positions.push([x, SKIRT - drop, z]);
            }
            for k in 0..edge.len() as u32 - 1 {
                let (a, b) = (edge[k as usize], edge[k as usize + 1]);
                let (c, d) = (first + k + 1, first + k);
                indices.extend([a, b, c, a, c, d]);
            }
        }
        MeshData {
            positions,
            indices,
            texture: self.texture.clone(),
            color_gain: self.color_gain,
            material: self.material.clone(),
            heights: Some(self.heights.clone()),
            color_map: Some(self.colors.clone()),
            ..Default::default()
        }
    }
}

/// The terrain of a prepared scene: its patch meshes sit in the scene's
/// batches from `first`, one per stride, without instances, so they upload
/// and stay resident with the rest of the scene; each frame places tiles on
/// them.
#[derive(Clone)]
pub(super) struct TerrainTiles {
    pub(super) source: Arc<Terrain>,
    pub(super) first: usize,
    /// South-west corner of each tile.
    origins: Vec<[f32; 2]>,
    /// Metres a tile spans.
    size: f32,
    /// Triangles of each level's patch.
    pub(super) triangles: Vec<usize>,
    /// Tiles across, and cells a tile spans.
    tiles_x: usize,
    tile: u32,
    /// Per level, the indices of one chunk of its patch when it is stored
    /// in PATCH_CHUNKS² chunks, and the indices before its skirts.
    chunked: Vec<(Option<u32>, u32)>,

    level: Arc<Mutex<LevelTiles>>,
    chunks: Arc<Mutex<ChunkRanges>>,
}

/// The lowest and highest ground of every chunk, `side` across the
/// terrain, found a tile at a time when a view first draws the tile across
/// its edge and found again where heights change; as of the heights' edit
/// serial `serial`.
#[derive(Default)]
struct ChunkRanges {
    serial: Option<u64>,
    known: Vec<u64>,
    side: usize,
    ranges: Vec<[f32; 3]>,
}

/// How much of each tile is steeper than pages take (page_flat, from the
/// heights: `HeightMap::steep_share`), found when a view first draws it
/// paged, and how far each coarse level strays from its heights
/// (`HeightMap::patch_errors`, `measured`), found when a view first places
/// it; both found again where heights change, as of the heights' edit
/// serial `serial`.
#[derive(Default)]
struct LevelTiles {
    serial: Option<u64>,
    known: Vec<u64>,
    steep: Vec<u8>,
    measured: Vec<u64>,
    errors: Vec<[f32; TERRAIN_COARSE]>,
}

fn bit(bits: &[u64], i: usize) -> bool {
    bits[i / 64] >> (i % 64) & 1 == 1
}

/// Whether `bounds` lies wholly inside every one of `planes` (Frustum's).
fn contains(planes: &[[f64; 4]; 6], bounds: [Vec3; 2]) -> bool {
    planes.iter().all(|p| {
        let mut least = p[3];
        for a in 0..3 {
            let [low, high] = [bounds[0][a], bounds[1][a]].map(f64::from);
            least += p[a] * if p[a] >= 0. { low } else { high };
        }
        least >= 0.
    })
}

fn set_bit(bits: &mut [u64], i: usize, on: bool) {
    let mask = 1u64 << (i % 64);
    if on {
        bits[i / 64] |= mask;
    } else {
        bits[i / 64] &= !mask;
    }
}

impl TerrainTiles {
    /// Appends `source`'s patch meshes to `batches`. The same terrain as
    /// `previous` scene's keeps its patch meshes, and with them their GPU
    /// copies.
    pub(super) fn new(
        source: &Arc<Terrain>,
        previous: Option<&PreparedScene>,
        batches: &mut Vec<Prepared>,
    ) -> Self {
        let kept = previous.and_then(|s| {
            let t = s.terrain.as_ref()?;
            Arc::ptr_eq(&t.source, source).then_some((s, t))
        });
        let grid = source.heights.grid();
        let cells = grid.width.max(grid.depth).saturating_sub(1).max(1) as u32;
        // Tiles of 64 cells, or one tile when the grid does not split.
        let tile = if cells.is_multiple_of(TERRAIN_TILE) {
            TERRAIN_TILE
        } else {
            cells
        };
        // Only levels finer than a sample draw by chunk: their chunks hold
        // 512 triangles and more. A sample's hold 128, and the hundreds of
        // draws 1800's moderate island took for them cost more than the
        // triangles they left out.
        let chunked: Vec<(Option<u32>, u32)> = (TERRAIN_STRIDES.iter().enumerate())
            .map(|(k, &s)| {
                let n = patch_cells(tile, s.min(tile as f32));
                let per = (k < TERRAIN_SAMPLE_LEVEL && n.is_multiple_of(PATCH_CHUNKS))
                    .then(|| (n / PATCH_CHUNKS).pow(2) * 6);
                (per, n * n * 6)
            })
            .collect();
        let meshes: Vec<Arc<Mesh>> = match kept {
            Some((s, t)) => (s.batches[t.first..t.first + TERRAIN_STRIDES.len()].iter())
                .map(|p| p.batch.mesh.clone())
                .collect(),
            None => (TERRAIN_STRIDES.iter().zip(&chunked))
                .map(|(&s, c)| {
                    let data = source.patch(tile, s.min(tile as f32), c.0.is_some());
                    Arc::new(Mesh::new(data).expect("terrain patches are valid"))
                })
                .collect(),
        };
        let first = batches.len();
        let triangles = meshes.iter().map(|m| m.triangles()).collect();
        batches.extend(meshes.into_iter().map(|m| Prepared::terrain(m, grid)));
        let size = tile as f32 * grid.spacing;
        let tiles_x = (grid.width as u32 - 1).max(1).div_ceil(tile);
        let tiles_z = (grid.depth as u32 - 1).max(1).div_ceil(tile);
        let origins = (0..tiles_z)
            .flat_map(|z| (0..tiles_x).map(move |x| (x, z)))
            .map(|(x, z)| {
                [
                    grid.origin[0] + x as f32 * size,
                    grid.origin[1] + z as f32 * size,
                ]
            })
            .collect();
        let words = ((tiles_x * tiles_z) as usize).div_ceil(64);
        // The same terrain keeps its slope and height ranges across scenes.
        let (level, chunks) = match kept {
            Some((_, t)) => (t.level.clone(), t.chunks.clone()),
            None => (
                Arc::new(Mutex::new(LevelTiles {
                    serial: None,
                    known: vec![0; words],
                    steep: vec![0; (tiles_x * tiles_z) as usize],
                    measured: vec![0; words],
                    errors: vec![[0.; TERRAIN_COARSE]; (tiles_x * tiles_z) as usize],
                })),
                Arc::new(Mutex::new({
                    let side = (tiles_x.max(tiles_z) * PATCH_CHUNKS) as usize;
                    ChunkRanges {
                        serial: None,
                        known: vec![0; words],
                        side,
                        ranges: vec![[0.; 3]; side * side],
                    }
                })),
            ),
        };
        TerrainTiles {
            source: source.clone(),
            first,
            origins,
            size,
            triangles,
            tiles_x: tiles_x as usize,
            tile,
            chunked,
            level,
            chunks,
        }
    }

    /// The samples whose slopes reach tile `i`'s ground: its own and the
    /// row and column past its far edges, which pixels on them blend in.
    fn tile_samples(&self, i: usize) -> Rect {
        let (x, z) = ((i % self.tiles_x) as u32, (i / self.tiles_x) as u32);
        let t = self.tile as usize;
        Rect {
            x0: x as usize * t,
            z0: z as usize * t,
            x1: (x as usize + 1) * t + 2,
            z1: (z as usize + 1) * t + 2,
        }
    }

    /// The level-tile cache brought up to the heights as of `serial`, the
    /// edit serial the GPU's copy holds; `None` while the heights have
    /// moved past it (the tiles would describe ground not yet drawn).
    fn level_tiles(&self, serial: u64) -> Option<std::sync::MutexGuard<'_, LevelTiles>> {
        let mut state = self.level.lock_any();
        let (now, rects) = self.source.heights.edits_since(state.serial);
        if now != serial {
            return None;
        }
        for rect in rects {
            // A sample's slope reads its neighbours.
            let r = Rect {
                x0: rect.x0.saturating_sub(1),
                z0: rect.z0.saturating_sub(1),
                x1: rect.x1 + 1,
                z1: rect.z1 + 1,
            };
            for i in 0..self.origins.len() {
                let t = self.tile_samples(i);
                if t.x0 < r.x1 && r.x0 < t.x1 && t.z0 < r.z1 && r.z0 < t.z1 {
                    set_bit(&mut state.known, i, false);
                    set_bit(&mut state.measured, i, false);
                }
            }
        }
        state.serial = Some(now);
        Some(state)
    }

    /// Per instance of `tiles` (at `levels`' ranges), which of its chunks
    /// `frustum` holds, a bit each in chunk order, all of them for a tile
    /// wholly inside it, an unchunked level or ground drawn from heights
    /// not yet `serial` (the GPU's copy). `reach` is how far relief moves
    /// the ground off its heights.
    fn chunk_masks(
        &self,
        frustum: &Frustum,
        tiles: &[u32],
        levels: &[std::ops::Range<u32>],
        serial: Option<u64>,
        reach: f32,
    ) -> Vec<u64> {
        let all = vec![u64::MAX; tiles.len()];
        let Some(serial) =
            serial.filter(|_| CHUNK_GROUND.load(std::sync::atomic::Ordering::Relaxed))
        else {
            return all;
        };
        let mut state = self.chunks.lock_any();
        let (now, rects) = self.source.heights.edits_since(state.serial);
        if now != serial {
            return all;
        }
        for rect in rects {
            for i in 0..self.origins.len() {
                let t = self.tile_samples(i);
                if t.x0 < rect.x1 && rect.x0 < t.x1 && t.z0 < rect.z1 && rect.z0 < t.z1 {
                    set_bit(&mut state.known, i, false);
                }
            }
        }
        state.serial = Some(now);
        let [lo, hi] = self.source.heights.range();
        let planes = frustum.planes();
        let mut masks = all;
        let per = PATCH_CHUNKS as usize;
        let c = self.size / PATCH_CHUNKS as f32;
        for (k, r) in levels.iter().enumerate() {
            if self.chunked[k].0.is_none() {
                continue;
            }
            for instance in r.clone() {
                let i = tiles[instance as usize] as usize;
                let o = self.origins[i];
                let tile = [
                    [o[0] - reach, lo - reach, o[1] - reach],
                    [
                        o[0] + self.size + reach,
                        hi + reach,
                        o[1] + self.size + reach,
                    ],
                ];
                if contains(&planes, tile) {
                    continue;
                }
                let side = state.side;
                if !bit(&state.known, i) {
                    let ChunkRanges { known, ranges, .. } = &mut *state;
                    // Each chunk's samples, the far row and column included.
                    self.source
                        .heights
                        .cell_ranges(c, side, ranges, self.tile_samples(i), 0.);
                    set_bit(known, i, true);
                }
                let (tx, tz) = ((i % self.tiles_x) * per, (i / self.tiles_x) * per);
                let mut mask = 0u64;
                for z in 0..per {
                    for x in 0..per {
                        let [a, b, _] = state.ranges[(tz + z) * side + tx + x];
                        let (x0, z0) = (o[0] + x as f32 * c, o[1] + z as f32 * c);
                        let chunk = [
                            [x0 - reach, a - reach, z0 - reach],
                            [x0 + c + reach, b + reach, z0 + c + reach],
                        ];
                        if !frustum.excludes(chunk) {
                            mask |= 1 << (z * per + x);
                        }
                    }
                }
                masks[instance as usize] = mask;
            }
        }
        masks
    }

    /// Per tile, whether every finest-page cell its ground spans holds a
    /// baked page in `entries` (a view's PageIndex, `side` cells a row from
    /// `origin`, `leaf` metres each), and after those bits whether any does.
    pub(super) fn paged_tiles(
        &self,
        entries: &[u32],
        side: u32,
        origin: [f32; 2],
        leaf: f32,
    ) -> Vec<u64> {
        let words = self.origins.len().div_ceil(64);
        let mut bits = vec![0; 2 * words];
        let cells = |o: f32, from: f32| {
            let first = ((o - from) / leaf).floor() as i64;
            let last = ((o + self.size - from) / leaf).floor() as i64;
            (first, last)
        };
        for (i, o) in self.origins.iter().enumerate() {
            let ((x0, x1), (z0, z1)) = (cells(o[0], origin[0]), cells(o[1], origin[1]));
            let last = i64::from(side) - 1;
            let (mut all, mut any) = (x0 >= 0 && z0 >= 0 && x1 <= last && z1 <= last, false);
            for z in z0.max(0)..=z1.min(last) {
                let row = z as usize * side as usize;
                for &e in &entries[row + x0.max(0) as usize..=row + x1.min(last) as usize] {
                    all &= e >= 0x8000_0000;
                    any |= e >= 0x8000_0000;
                }
            }
            set_bit(&mut bits, i, all);
            set_bit(&mut bits, words * 64 + i, any);
        }
        bits
    }

    /// The tiles a view shows as instance matrices, grouped by level, and
    /// each level's range of them. `rows` are the view's clip rows,
    /// `height` its height in pixels, `serial` the heights' edit serial
    /// the GPU's copy holds.
    fn place(
        &self,
        ground: Option<&dyn Ground>,
        frustum: &Frustum,
        rows: &[[f64; 4]; 4],
        height: f32,
        serial: Option<u64>,
    ) -> (Vec<Mat4>, Vec<u32>, Vec<std::ops::Range<u32>>) {
        let [lo, hi] = self.source.heights.range();
        let spacing = f64::from(self.source.heights.grid().spacing);
        // Without the GPU's heights measured, tiles keep TERRAIN_CELL_PX.
        let mut measures = serial.and_then(|s| Some((s, self.level_tiles(s)?)));
        let relief = ground.is_some_and(|g| g.has_relief());

        let w = rows[3];
        let scale = (rows[1][0].powi(2) + rows[1][1].powi(2) + rows[1][2].powi(2)).sqrt()
            * f64::from(height)
            * 0.5;
        // Each level's tiles with how far their nearest point is.
        let mut levels: Vec<Vec<(f64, Mat4, u32)>> = vec![Vec::new(); TERRAIN_STRIDES.len()];
        for (i, o) in self.origins.iter().enumerate() {
            let bounds = [[o[0], lo, o[1]], [o[0] + self.size, hi, o[1] + self.size]];
            if frustum.excludes(bounds) {
                continue;
            }
            // Pixels per metre at the tile's nearest point, as cull.wgsl
            // measures placements.
            let c: [f64; 3] = std::array::from_fn(|a| f64::from(bounds[0][a] + bounds[1][a]) * 0.5);
            let e: [f64; 3] = std::array::from_fn(|a| f64::from(bounds[1][a] - bounds[0][a]) * 0.5);
            let nearest = (0..3).map(|a| w[a] * c[a] - w[a].abs() * e[a]).sum::<f64>() + w[3];
            let ppm = if nearest > 1e-6 {
                scale / nearest
            } else {
                f64::INFINITY
            };
            let k = TERRAIN_STRIDES
                .iter()
                .rposition(|&s| spacing * f64::from(s) * ppm <= f64::from(TERRAIN_CELL_PX))
                .unwrap_or(0);
            // Finer than a sample only where the relief is rough, found
            // the first time a tile asks.
            let k = if k < TERRAIN_SAMPLE_LEVEL
                && !ground.is_some_and(|ground| {
                    ground.rough([o[0], o[1], o[0] + self.size, o[1] + self.size])
                }) {
                TERRAIN_SAMPLE_LEVEL
            } else {
                k
            };
            // Coarser still where that patch strays less than
            // TERRAIN_ERROR_PX from the heights. Not on rough tiles: relief
            // moves their vertices by what the CPU does not know.
            let k = match &mut measures {
                Some((serial, level))
                    if k >= TERRAIN_SAMPLE_LEVEL
                        && !(relief
                            && ground.is_some_and(|g| {
                                g.rough([o[0], o[1], o[0] + self.size, o[1] + self.size])
                            })) =>
                {
                    if !bit(&level.measured, i) {
                        let (x, z) = (i % self.tiles_x, i / self.tiles_x);
                        let t = self.tile as usize;
                        let strides = std::array::from_fn(|j| {
                            (TERRAIN_STRIDES[TERRAIN_SAMPLE_LEVEL + 1 + j] as usize).min(t)
                        });
                        match self
                            .source
                            .heights
                            .patch_errors([x * t, z * t], t, strides, *serial)
                        {
                            Some(errors) => {
                                level.errors[i] = errors;
                                set_bit(&mut level.measured, i, true);
                            }
                            None => level.errors[i] = [f32::INFINITY; TERRAIN_COARSE],
                        }
                    }
                    let most = f64::from(TERRAIN_ERROR_PX) / ppm;
                    (level.errors[i].iter())
                        .rposition(|&e| f64::from(e) <= most)
                        .map_or(k, |j| k.max(TERRAIN_SAMPLE_LEVEL + 1 + j))
                }
                _ => k,
            };
            levels[k].push((nearest, math::translate([o[0], 0., o[1]]), i as u32));
        }
        let mut ranges = Vec::with_capacity(levels.len());
        let mut matrices = Vec::new();
        let mut tiles = Vec::new();
        // Nearest first, and the finer (nearer) levels first: the depth test
        // then skips ground a hill in front already covers, before the
        // ground shades it.
        for mut level in levels {
            level.sort_by(|a, b| a.0.total_cmp(&b.0));
            let start = matrices.len() as u32;
            matrices.extend(level.iter().map(|&(_, m, _)| m));
            tiles.extend(level.iter().map(|&(_, _, i)| i));
            ranges.push(start..matrices.len() as u32);
        }
        (matrices, tiles, ranges)
    }
}

impl Device {
    /// Bind group 2 over a view's terrain tile matrices, written to the
    /// view's own tile buffer, which grows to fit.
    fn terrain_group(
        &self,
        gpu: &Gpu,
        ids: &wgpu::Buffer,
        targets: &mut Targets,
        matrices: &[Mat4],
    ) -> wgpu::BindGroup {
        let bytes: &[u8] = bytemuck::cast_slice(matrices);
        if targets
            .terrain
            .as_ref()
            .is_none_or(|b| b.size() < bytes.len() as u64)
        {
            targets.terrain = Some(gpu.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("editor terrain tiles"),
                size: (bytes.len() as u64).next_power_of_two(),
                usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            }));
        }
        let buffer = targets.terrain.as_ref().expect("created above");
        gpu.queue.write_buffer(buffer, 0, bytes);
        self.draw_group(gpu, ids, Some(buffer))
    }
}

/// The kit's ground as a [`Pass`]: draws [`Scene::terrain`] at the Opaque
/// stage, after the kit's meshes and placements, and casts its shadow. A
/// scene lists it first unless [`Scene::ground`] says otherwise; a game's
/// ready Opaque pass that replaces takes its place, drawing and shadow
/// both. It works from the kit's internals ([`Prepare`]'s and [`Record`]'s
/// crate-private parts): its patch meshes upload with the scene's batches,
/// and it draws with the kit's pipelines and draw groups.
#[derive(Default)]
pub struct TerrainPass {
    /// This view's tiles, from its prepare to its record; views draw one
    /// after another.
    view: std::sync::Mutex<Option<TerrainView>>,
    /// Every tile at the shadow stride, made once: the tiles never change
    /// for the scene this pass belongs to.
    shadow_tiles: std::sync::Mutex<super::OnDevice<wgpu::Buffer>>,
}

struct TerrainView {
    /// Each level's range of the tile matrices `group` holds; no group when
    /// no tile is in view.
    ranges: Vec<std::ops::Range<u32>>,
    /// The tile each instance draws, in the matrices' order.
    tiles: Vec<u32>,
    group: Option<wgpu::BindGroup>,
    /// Draw-id slots (all zero: the ground picks nothing) for the view's
    /// tiles and the shadow draw.
    offset: u32,
    shadow_offset: u32,
    revision: u64,
    shadow_revision: u64,
    triangles: usize,
    /// World bounds while the terrain casts.
    casts: Option<[Vec3; 2]>,
    /// The view's, for the chunks of tiles across its edges.
    frustum: Frustum,
}

impl TerrainPass {
    /// Triangles the last prepared view draws.
    pub(super) fn triangles(&self) -> usize {
        (self.view.lock_any().as_ref()).map_or(0, |v| v.triangles)
    }
}

impl Pass for TerrainPass {
    fn stage(&self) -> Stage {
        Stage::Opaque
    }

    fn prepare(&self, frame: &mut Prepare<'_>) -> bool {
        let mut view = self.view.lock_any();
        *view = None;
        // Outside a viewport's frame there is nothing to draw with.
        let Some(kit) = frame.kit.as_mut() else {
            return true;
        };
        let Some(t) = kit.scene.terrain.as_ref() else {
            return true;
        };
        let ground = kit.device.active_ground().map(|(g, _, _)| g);
        let serial =
            (kit.device.resident.get(t.first)).and_then(|d| d.mesh.heights.as_ref()?.serial());
        let (matrices, tiles, ranges) = t.place(ground, kit.frustum, kit.rows, kit.height, serial);
        let group = (!matrices.is_empty())
            .then(|| (kit.device).terrain_group(kit.gpu, kit.ids, kit.targets, &matrices));
        let triangles = (ranges.iter().zip(&t.triangles))
            .map(|(r, &n)| r.len() * n)
            .sum();
        let casts = (ground.is_some()
            || casts_shadow(kit.scene.batches[t.first].batch.mesh.data()))
        .then(|| {
            let grid = t.source.heights.grid();
            let [lo, hi] = t.source.heights.range();
            let [w, d] = grid.size();
            let o = grid.origin;
            [[o[0], lo, o[1]], [o[0] + w, hi, o[1] + d]]
        });
        *view = Some(TerrainView {
            ranges,
            tiles,
            group,
            offset: (kit.slot)([0; 4]),
            shadow_offset: (kit.slot)([0; 4]),
            revision: kit.device.surface.as_ref().map_or(0, |s| s.revision),
            shadow_revision: kit
                .device
                .surface
                .as_ref()
                .filter(|s| s.ground.relief_bound() > 0.)
                .map_or(0, |s| s.caster),
            triangles,
            casts,
            frustum: *kit.frustum,
        });
        true
    }

    fn record(&self, frame: &Record<'_>, pass: &mut wgpu::RenderPass<'_>) {
        let (Some(kit), Some(view)) = (&frame.kit, &*self.view.lock_any()) else {
            return;
        };
        let (Some(t), Some(group)) = (kit.scene.terrain.as_ref(), view.group.as_ref()) else {
            return;
        };
        let device = kit.device;
        let active = device.active_ground();
        let serial = (device.resident[t.first].mesh.heights.as_ref()).and_then(|h| h.serial());
        let masks = t.chunk_masks(
            &view.frustum,
            &view.tiles,
            &view.ranges,
            serial,
            device
                .active_ground()
                .map_or(0., |(g, _, _)| g.relief_bound()),
        );
        // Tiles wholly in view in runs of instances; each across the view's
        // edge alone, its chunks in view in runs, then its skirts.
        let draw = |pass: &mut wgpu::RenderPass<'_>, k: usize, r: std::ops::Range<u32>| {
            let draw = &device.resident[t.first + k];
            bind_patch(pass, &draw.mesh, active);
            pass.set_bind_group(2, group, &[view.offset]);
            let (per, surface) = t.chunked[k];
            let mut start = r.start;
            for instance in r.clone() {
                let mask = masks[instance as usize];
                let Some(per) = per.filter(|_| mask != u64::MAX) else {
                    continue;
                };
                if start < instance {
                    draw.mesh.draw(pass, start..instance);
                }
                start = instance + 1;
                if mask == 0 {
                    continue;
                }
                let one = instance..instance + 1;
                let mut c = 0;
                while c < 64 {
                    if mask >> c & 1 == 0 {
                        c += 1;
                        continue;
                    }
                    let first = c;
                    while c < 64 && mask >> c & 1 == 1 {
                        c += 1;
                    }
                    draw.mesh
                        .draw_indices(pass, first * per..c * per, one.clone());
                }
                draw.mesh.draw_indices(pass, surface..draw.mesh.count, one);
            }
            if start < r.end {
                draw.mesh.draw(pass, start..r.end);
            }
        };
        let pipeline = if let Some((_, program, _)) = active {
            &program.live
        } else {
            match &t.source.material {
                Material::Lit => &device.lit,
                Material::Flat => &device.flat,
                Material::Blend(_) | Material::Decal(_) => return,
            }
        };
        let drawn = (view.ranges.iter().enumerate()).filter(|(_, r)| !r.is_empty());
        // With pages, what they shade draws apart from the rest (gpu.wgsl
        // fs_ground_paged), the tiles sorted by what they can hold. Not
        // under relief: its tiles take a quarter sample's stride and
        // displace each vertex by the splat's heights, and a second raster
        // of them cost more than the live pixels saved (pirate island, 2.10
        // to 2.71 ms).
        let two = active
            .filter(|(g, _, _)| {
                SPLIT_GROUND.load(std::sync::atomic::Ordering::Relaxed) && !g.has_relief()
            })
            .and_then(|(_, p, _)| Some((&p.paged, kit.paged?, serial?, t.level_tiles(serial?)?)));
        let Some((pipelines, paged, serial, mut level)) = two else {
            pass.set_pipeline(pipeline);
            for (k, r) in drawn {
                draw(pass, k, r.clone());
            }
            return;
        };
        // Per tile instance: 1 the paged draw alone, 2 one draw as without
        // pages, 3 the paged draw and then fs_ground over the rest.
        let any = paged.len() / 2 * 64;
        let draws: Vec<u8> = (view.tiles.iter())
            .map(|&i| {
                let i = i as usize;
                let steep = if bit(&level.known, i) {
                    level.steep[i]
                } else {
                    let rect = t.tile_samples(i);
                    // Relief draws once, so the threshold is page_flat's
                    // without it.
                    match t.source.heights.steep_share(
                        rect,
                        page_flat(false) + LEVEL_MARGIN,
                        serial,
                    ) {
                        Some(share) => {
                            set_bit(&mut level.known, i, true);
                            level.steep[i] = share;
                            share
                        }
                        // Edited since the GPU's copy: one draw for it.
                        None => u8::MAX,
                    }
                };
                // Tiles mostly steep, or without a page, take one draw: the
                // paged one would mostly raster them, and the ground it
                // leaves undrawn gives no depth to stop what lies behind.
                let paged_draw = bit(paged, any + i) && steep < STEEP_TILE;
                let rest = !paged_draw || steep > 0 || !bit(paged, i);
                u8::from(paged_draw) | u8::from(rest) << 1
            })
            .collect();
        // Tiles the paged draw skips first, in one draw as without pages:
        // often nearest, their depth then stops the paged draw behind them.
        let passes = [(pipeline, 2), (&pipelines[0], 1), (&pipelines[1], 3)];
        for (pipeline, kind) in passes {
            pass.set_pipeline(pipeline);
            for (k, r) in drawn.clone() {
                let mut run: Option<std::ops::Range<u32>> = None;
                for instance in r.clone() {
                    let d = draws[instance as usize];
                    let wanted = if kind == 1 { d & 1 != 0 } else { d == kind };
                    match &mut run {
                        Some(run) if wanted && run.end == instance => run.end += 1,
                        _ if wanted => {
                            if let Some(done) = run.replace(instance..instance + 1) {
                                draw(pass, k, done);
                            }
                        }
                        _ => {}
                    }
                }
                if let Some(done) = run {
                    draw(pass, k, done);
                }
            }
        }
    }

    // What the ground draws follows the camera, the scene and its height
    // and paint edits, which the kit keys on itself; the rough tiles'
    // scan is cached per tile but its answer is the same whenever made.
    fn version(&self) -> Option<u64> {
        Some(self.view.lock_any().as_ref()?.revision)
    }

    // Heightfield edits have their own shadow serial. Paint can move the
    // caster too when the surface displaces vertices, as far as the ground
    // says (`Ground::relief_moves`).
    fn shadow(&self) -> Option<Caster> {
        let view = self.view.lock_any();
        let view = view.as_ref()?;
        Some(Caster {
            version: view.shadow_revision,
            bounds: view.casts?,
        })
    }

    /// Into the scene's map, every tile at one coarse stride: its texels are
    /// coarse anyway. Into a view's cascade, the tiles it holds at about
    /// four texels a cell.
    fn record_shadow(&self, frame: &Record<'_>, pass: &mut wgpu::RenderPass<'_>) {
        let (Some(kit), Some(view)) = (&frame.kit, &*self.view.lock_any()) else {
            return;
        };
        let Some(t) = kit.scene.terrain.as_ref() else {
            return;
        };
        // A map drawn again in part takes the tiles under that part alone;
        // the scissor drops the rest's texels anyway. Relief can lift a
        // tile past the heights' range.
        let [lo, hi] = t.source.heights.range();
        let relief = kit
            .device
            .active_ground()
            .map_or(0., |(g, _, _)| g.relief_bound());
        let part = kit.shadow_part.map(|rows| Frustum::from_rows(&rows));
        let reaches = |o: &[f32; 2], frustum: &Frustum| {
            !frustum.excludes([
                [o[0], lo - relief, o[1]],
                [o[0] + t.size, hi + relief, o[1] + t.size],
            ])
        };
        if let Some((rows, texel)) = kit.cascade {
            let frustum = Frustum::from_rows(&rows);
            let matrices: Vec<Mat4> = (t.origins.iter())
                .filter(|o| {
                    !frustum.excludes([[o[0], lo, o[1]], [o[0] + t.size, hi, o[1] + t.size]])
                        && part.as_ref().is_none_or(|f| reaches(o, f))
                })
                .map(|o| math::translate([o[0], 0., o[1]]))
                .collect();
            if matrices.is_empty() {
                return;
            }
            // Coarser than the ground the view draws, the cascade's ground
            // would rise over it on every crest and shade it.
            let spacing = t.source.heights.grid().spacing;
            let level = (TERRAIN_STRIDES.iter())
                .rposition(|&s| spacing * s <= texel * 4.)
                .unwrap_or(0)
                .clamp(TERRAIN_SAMPLE_LEVEL, TERRAIN_SHADOW_LEVEL);
            let buffer = buffer_init(
                kit.gpu,
                "terrain cascade tiles",
                bytemuck::cast_slice(&matrices),
                wgpu::BufferUsages::STORAGE,
            );
            let group = kit.device.draw_group(kit.gpu, kit.ids, Some(&buffer));
            let draw = &kit.device.resident[t.first + level];
            let active = kit.device.active_ground();
            pass.set_pipeline(active.map_or(&kit.device.shadow, |(_, p, _)| &p.shadow));
            bind_patch(pass, &draw.mesh, active);
            pass.set_bind_group(2, &group, &[view.shadow_offset]);
            draw.mesh.draw(pass, 0..matrices.len() as u32);
            return;
        }
        if let Some(part) = &part {
            let matrices: Vec<Mat4> = (t.origins.iter())
                .filter(|o| reaches(o, part))
                .map(|o| math::translate([o[0], 0., o[1]]))
                .collect();
            if matrices.is_empty() {
                return;
            }
            let buffer = buffer_init(
                kit.gpu,
                "terrain shadow part tiles",
                bytemuck::cast_slice(&matrices),
                wgpu::BufferUsages::STORAGE,
            );
            let group = kit.device.draw_group(kit.gpu, kit.ids, Some(&buffer));
            let draw = &kit.device.resident[t.first + TERRAIN_SHADOW_LEVEL];
            let active = kit.device.active_ground();
            pass.set_pipeline(active.map_or(&kit.device.shadow, |(_, p, _)| &p.shadow));
            bind_patch(pass, &draw.mesh, active);
            pass.set_bind_group(2, &group, &[view.shadow_offset]);
            draw.mesh.draw(pass, 0..matrices.len() as u32);
            return;
        }
        let mut tiles = self.shadow_tiles.lock_any();
        let buffer = tiles.get_or_insert_with(&kit.gpu.device, || {
            let matrices: Vec<Mat4> = (t.origins.iter())
                .map(|o| math::translate([o[0], 0., o[1]]))
                .collect();
            buffer_init(
                kit.gpu,
                "terrain shadow tiles",
                bytemuck::cast_slice(&matrices),
                wgpu::BufferUsages::STORAGE,
            )
        });
        let group = kit.device.draw_group(kit.gpu, kit.ids, Some(buffer));
        let draw = &kit.device.resident[t.first + TERRAIN_SHADOW_LEVEL];
        // Casters draw after the kit's placements, which leave their own
        // pipeline bound.
        let active = kit.device.active_ground();
        pass.set_pipeline(active.map_or(&kit.device.shadow, |(_, p, _)| &p.shadow));
        bind_patch(pass, &draw.mesh, active);
        pass.set_bind_group(2, &group, &[view.shadow_offset]);
        draw.mesh.draw(pass, 0..t.origins.len() as u32);
    }
}

// --- Baked ground pages -------------------------------------------------------
// The ground's shading before lighting (`Ground::wgsl`'s albedo, normal,
// gloss and glow), baked into pages of a quadtree over the ground, so flat ground
// samples two textures instead of blending its layers per pixel. Each view
// picks per tile the coarsest page whose texels are still no larger than
// its pixels there; where even the finest page is too coarse, or a page is
// not baked yet, or the ground is steep, fs_ground shades live. Edits
// (heights, the ground's own, tint) unbake the pages they touch, which rebake
// nearest first within a per-frame budget; until then they shade live.
// Anno bakes its terrain into quadtree node textures the same way
// (terrain_baking.fx), from the same code that shades it live.

/// Texels across a page at its finest mip (gpu.wgsl PAGE_TEXELS), and
/// baked round it (PAGE_BORDER) so filtering near its edge reads the
/// ground beyond, never a neighbouring layer: anisotropic taps reach a few
/// texels out. 16, not the 8 those need, so every mip's page starts and
/// ends on a whole 4x4 block: pages are kept block compressed.
const PAGE_TEXELS: u32 = 256;
const PAGE_BORDER: u32 = 16;
const PAGE_SIDE: u32 = PAGE_TEXELS + 2 * PAGE_BORDER;
/// Mips per page, each baked from the ground at its own texel size, down to
/// 32 texels with a two-texel border.
const PAGE_MIPS: u32 = 4;
/// Metres the finest pages span: texels of 1/64 m. Ground whose pixels
/// are finer than that, close under the camera, shades live.
const PAGE_LEAF: f32 = 4.;
/// Ground at least this level (normal y) draws from pages; steeper shades
/// live. A page is flat in X/Z, so up a slope its texels stretch by 1 /
/// normal y and the side projections squeeze: at 0.6 cliffs still read
/// as sharp as live shading (Albion 62 dB against it), at 0.5 they began
/// to soften. Relief keeps 0.7: its displaced vertices move the side
/// projections off the bake's, and its slopes lit from the page's height
/// lost their grain (pirate 41 dB at 0.6).
fn page_flat(relief: bool) -> f32 {
    if relief { 0.7 } else { 0.6 }
}
/// How much more level than page_flat a tile's samples must be, measured
/// on the CPU, to count as wholly paged: the GPU's slopes round
/// differently.
const LEVEL_MARGIN: f32 = 0.002;
/// Steep share (of 255) from which a tile takes one draw: past a quarter,
/// 1800's mountains left the paged draw shading valleys behind them (128
/// lost 0.2 ms on its moderate island; 32 gave back half of Albion's
/// gain).
const STEEP_TILE: u8 = 64;
/// Pixels a page texel may cover at the page's nearest point. With the
/// bake's mip bias (PAGE_BAKE_GRADIENT) two keep the ground as sharp as
/// live shading (high-frequency energy of near Albion ground within 3%);
/// one needed 835 pages there, two need 255.
const PAGE_TEXEL_PIXELS: f64 = 2.;
/// The bake samples layers at this share of a texel's footprint, a mip
/// sharper: the page is filtered again when drawn, and without it ground
/// came out visibly softer. Anno bakes half a mip sharp (terrain_baking.fx
/// TEXTURE_BIAS).
const PAGE_BAKE_GRADIENT: f32 = 0.5;
/// Pages sit in sheets, the layers of a 2D array (gpu.wgsl baked_ground),
/// `PAGE_SHEET_SIDE` pages a side: page `layer` in cell (layer % side,
/// layer / side % side) of sheet layer / (side * side), in every mip. A
/// page's UVs are a share of its sheet, which never changes size, so it
/// draws the same bytes however far the pool has grown: as one 2D atlas
/// growing a row at a time, the sampler's precision followed the atlas's
/// height, and a kept view drew a few pixels up to 6 of 255 off a fresh
/// one with the same pages. Seven a side is 2016 texels, which NVIDIA's
/// driver pads by 2%; a sheet per page row (288 tall) it padded to 512,
/// 79% more, and 384 one-page layers took 374 MB.
const PAGE_SHEET_SIDE: u32 = 7;
const PAGE_SHEET: u32 = PAGE_SHEET_SIDE * PAGE_SHEET_SIDE;
/// Sheets the pool holds at most: 392 pages of 288² texels of albedo and
/// gloss (BC3, 1 byte) and normal (BC5, 1 byte, kept only for ground that
/// reads it) with mips, 0.22 MB each, 88 MB in all; as RGBA8 and RG8, on
/// devices without BC, 259 MB; a glowing ground's R8 glow adds 43 MB. The
/// benchmark views at 5K pick 125 to 327. A view picking more than
/// `PAGE_PICK` takes coarser pages, leaving the rest for pages another
/// view or the last frames still use.
const PAGE_SHEETS: u32 = 8;
const PAGE_CAPACITY: usize = (PAGE_SHEETS * PAGE_SHEET) as usize;
const PAGE_PICK: usize = PAGE_CAPACITY * 7 / 8;
/// Sheets a pool starts with. It grows a sheet at a time to what its views
/// use and an eighth more (`GroundPages::fit`), so a small pane's pool
/// stays small: the whole 85 MB went to a 1280x720 view of a small island.
const PAGE_FIRST_SHEETS: u32 = 1;
/// View draws back that a page counts as in use for `fit`: both island
/// panes, over two frames. Sized to one view's picks, two panes would take
/// each other's pages every frame and bake them again.
const PAGE_RECENT: u64 = 4;
/// Pages baked a frame once a view has most of its own: a few new ones
/// as the camera moves, about 0.15 ms. A view still missing most of its
/// pages bakes `PAGE_BURST`.
const PAGE_BUDGET: usize = 8;
const PAGE_BURST: usize = 40;
/// Pages the staging atlas holds: bakes draw there in batches this size,
/// then encode (or copy, without BC) into the pool. 5 MB.
const PAGE_STAGING: u32 = 8;
const ALBEDO_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8UnormSrgb;
const NORMAL_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rg8Unorm;
/// Glow in red, a byte a texel, staged and pooled alike: BC has no
/// one-channel format the encoder writes, and an R8 texel is a byte, as a
/// BC3 albedo texel is.
const GLOW_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::R8Unorm;
/// fs_page_bake's targets; fs_page_bake_glow adds GLOW_FORMAT.
const PAGE_FORMATS: [wgpu::TextureFormat; 2] = [ALBEDO_FORMAT, NORMAL_FORMAT];
/// No page: `node_layer`'s and `owner`'s empty value.
const NONE: u32 = u32::MAX;

/// Where an edit reached the GPU, for the pages over it to rebake.
pub(super) enum GroundEdit {
    /// Height samples of the terrain's grid.
    Heights(Rect),
    /// World X and Z, min then max.
    World([f32; 4]),
}

/// What pages bake from besides the ground's own maps; a change unbakes
/// them all.
#[derive(Clone, Copy, PartialEq)]
pub(super) struct PageKey {
    terrain: usize,
    /// The mounted ground (`MountedGround::identity`).
    ground: usize,
    /// `Lights::tone`'s exposure sign (decoding) and the ground tint
    /// (`Lights::ground_tint`, zero until its map is in).
    toned: bool,
    tint: [u32; 4],
}

/// A page's place in the quadtree: level 0 is the finest, spanning
/// `PAGE_LEAF` metres; each level up doubles.
#[derive(Clone, Copy)]
struct Node {
    level: u32,
    x: u32,
    z: u32,
}

/// The device's page pool for the presented terrain: the pages' layers and
/// which quadtree node each holds. Views share it; each keeps its own
/// index of the nodes it picked (`PageIndex`).
pub(super) struct GroundPages {
    key: PageKey,
    /// The grid's corner, sample spacing and size in metres; the quadtree
    /// starts at the corner and has `levels` levels, one node at the top.
    origin: [f32; 2],
    spacing: f32,
    extent: [f32; 2],
    levels: u32,
    /// Per node, the lowest and highest ground in it, for how near it
    /// comes to a view, and its mean normal Y (no lower than `flat`), for
    /// how far its texels stretch along the slope; and the height samples
    /// edited since.
    ranges: Vec<[f32; 3]>,
    stale: Option<Rect>,
    /// page_flat for the ground the pages bake.
    flat: f32,
    /// Where each level's nodes start in `node_layer`.
    offsets: Vec<u32>,
    /// Per node, the layer holding its page, or NONE.
    node_layer: Vec<u32>,
    /// Per layer, the node (`offsets` index) it holds or NONE, and the
    /// last frame a view wanted it.
    owner: Vec<u32>,
    wanted: Vec<u64>,
    /// Layers whose page is baked and current, a bit each.
    baked: Vec<u64>,
    /// Sheets the atlases hold now; `owner` and `wanted` have a slot per
    /// page in them.
    pub(super) sheets: u32,
    frame: u64,
    /// The pool's albedo atlas: BC3 where the device samples BC, its
    /// bake's 4 to 8 of 255 off lost in the layers' own grain.
    albedo: wgpu::Texture,
    pub(super) albedo_view: wgpu::TextureView,
    /// The normal atlas, while the ground has normal maps or relief: the
    /// pages' normals are flat and unread otherwise, as on Albion, whose
    /// set lacks two normal maps and has no relief. BC5 where the device
    /// has it: relief's lighting moves 0.008% of the pirate and continental
    /// views' pixels by more than 8 of 255 against RG8 (40 MB).
    normals: Option<(wgpu::Texture, wgpu::TextureView)>,
    /// The glow atlas and its staging, while the ground declares glow
    /// (`Ground::glow`).
    glow: Option<GlowPages>,
    staging: Staging,
}

struct GlowPages {
    atlas: wgpu::Texture,
    view: wgpu::TextureView,
    staging: wgpu::Texture,
    /// Per mip, the view the bake draws into.
    mips: Vec<wgpu::TextureView>,
}

/// One of the pool's atlases, `sheets` sheets of pages. COPY_SRC for the
/// copy into a bigger one (`GroundPages::fit`).
fn page_atlas(gpu: &Gpu, label: &str, format: wgpu::TextureFormat, sheets: u32) -> wgpu::Texture {
    gpu.device.create_texture(&wgpu::TextureDescriptor {
        label: Some(label),
        size: wgpu::Extent3d {
            width: PAGE_SIDE * PAGE_SHEET_SIDE,
            height: PAGE_SIDE * PAGE_SHEET_SIDE,
            depth_or_array_layers: sheets,
        },
        mip_level_count: PAGE_MIPS,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::TEXTURE_BINDING
            | wgpu::TextureUsages::COPY_DST
            | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    })
}

/// An atlas's view as frame.wgsl samples it: an array even at one sheet.
fn sheets_view(atlas: &wgpu::Texture) -> wgpu::TextureView {
    atlas.create_view(&wgpu::TextureViewDescriptor {
        dimension: Some(wgpu::TextureViewDimension::D2Array),
        ..Default::default()
    })
}

/// Device bytes of `t`'s mips, every layer.
#[cfg(test)]
fn texture_bytes(t: &wgpu::Texture) -> u64 {
    let format = t.format();
    let (bw, bh) = format.block_dimensions();
    let block = u64::from(format.block_copy_size(None).unwrap_or(4));
    (0..t.mip_level_count())
        .map(|mip| {
            let size = t.size().mip_level_size(mip, t.dimension());
            u64::from(size.width.div_ceil(bw))
                * u64::from(size.height.div_ceil(bh))
                * u64::from(size.depth_or_array_layers)
                * block
        })
        .sum()
}

/// Where bakes draw: `PAGE_STAGING` page cells in a row, in the bake's
/// formats, on their way to the pool. A pool drawn into directly could not
/// be block compressed.
struct Staging {
    textures: [wgpu::Texture; 2],
    /// Per mip, the albedo and normal views the bake draws into.
    mips: Vec<[wgpu::TextureView; 2]>,
    /// Albedo's sRGB bytes as stored, for the encoder, and blocks for a
    /// full staging atlas; `None` where the pool keeps RGBA8.
    encode: Option<([wgpu::TextureView; 2], wgpu::Buffer)>,
}

/// The encode of a staged page's mip (bc.rs): albedo as BC3, normals BC5.
fn page_job(cell: u32, mip: u32, kind: bc::Kind) -> bc::Job {
    let side = PAGE_SIDE >> mip;
    bc::Job {
        origin: [cell * side, 0],
        layer: 0,
        mip,
        blocks: [side / 4; 2],
        kind,
    }
}

/// A view's picked pages, as `t_pages` reads them: per finest-page cell
/// the covering page's layer and level, bit 31 when baked.
pub(super) struct PageIndex {
    texture: wgpu::Texture,
    pub(super) view: wgpu::TextureView,
    side: u32,
    entries: Vec<u32>,
    /// Per terrain tile, a bit when pages cover all of it
    /// (`TerrainTiles::paged_tiles`).
    pub(super) tiles: Vec<u64>,
}

impl GroundPages {
    /// A pool for `grid`, its albedo block compressed when `bc`.
    fn new(gpu: &Gpu, grid: Grid, key: PageKey, bc: bool) -> Self {
        let [w, d] = grid.size();
        let leaves = (w.max(d) / PAGE_LEAF).ceil().max(1.) as u32;
        let levels = leaves.next_power_of_two().trailing_zeros() + 1;
        let side = 1u32 << (levels - 1);
        let mut offsets = Vec::with_capacity(levels as usize);
        let mut total = 0;
        for l in 0..levels {
            offsets.push(total);
            total += (side >> l).pow(2);
        }
        let format = if bc {
            wgpu::TextureFormat::Bc3RgbaUnormSrgb
        } else {
            ALBEDO_FORMAT
        };
        let sheets = PAGE_FIRST_SHEETS;
        let albedo = page_atlas(gpu, "editor ground pages", format, sheets);
        let staging = |format, view_formats: &[_]| {
            gpu.device.create_texture(&wgpu::TextureDescriptor {
                label: Some("editor ground page staging"),
                size: wgpu::Extent3d {
                    width: PAGE_SIDE * PAGE_STAGING,
                    height: PAGE_SIDE,
                    depth_or_array_layers: 1,
                },
                mip_level_count: PAGE_MIPS,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format,
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT
                    | wgpu::TextureUsages::TEXTURE_BINDING
                    | wgpu::TextureUsages::COPY_SRC,
                view_formats,
            })
        };
        let textures = [
            staging(ALBEDO_FORMAT, &[ALBEDO_FORMAT.remove_srgb_suffix()]),
            staging(NORMAL_FORMAT, &[]),
        ];
        let mips = (0..PAGE_MIPS)
            .map(|mip| {
                textures.each_ref().map(|t| {
                    t.create_view(&wgpu::TextureViewDescriptor {
                        base_mip_level: mip,
                        mip_level_count: Some(1),
                        ..Default::default()
                    })
                })
            })
            .collect();
        let encode = bc.then(|| {
            let raw = [
                (&textures[0], ALBEDO_FORMAT.remove_srgb_suffix()),
                (&textures[1], NORMAL_FORMAT),
            ]
            .map(|(t, format)| {
                t.create_view(&wgpu::TextureViewDescriptor {
                    format: Some(format),
                    dimension: Some(wgpu::TextureViewDimension::D2Array),
                    ..Default::default()
                })
            });
            // Albedo's blocks, then normals' (the same size).
            let page: u64 = (0..PAGE_MIPS)
                .map(|mip| page_job(0, mip, bc::Kind::Rgba).bytes())
                .sum();
            let blocks = gpu.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("editor ground page blocks"),
                size: 2 * page * u64::from(PAGE_STAGING),
                usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC,
                mapped_at_creation: false,
            });
            (raw, blocks)
        });
        let albedo_view = sheets_view(&albedo);
        Self {
            key,
            origin: grid.origin,
            spacing: grid.spacing,
            extent: [w, d],
            levels,
            ranges: vec![[0., 0., 1.]; total as usize],
            stale: Some(Rect {
                x0: 0,
                z0: 0,
                x1: grid.width,
                z1: grid.depth,
            }),
            flat: page_flat(false),
            offsets,
            node_layer: vec![NONE; total as usize],
            owner: vec![NONE; (sheets * PAGE_SHEET) as usize],
            wanted: vec![0; (sheets * PAGE_SHEET) as usize],
            baked: vec![0; PAGE_CAPACITY.div_ceil(64)],
            sheets,
            frame: 0,
            albedo,
            albedo_view,
            normals: None,
            glow: None,
            staging: Staging {
                textures,
                mips,
                encode,
            },
        }
    }

    /// The normal atlas's view, when it has one.
    pub(super) fn normal_view(&self) -> Option<&wgpu::TextureView> {
        self.normals.as_ref().map(|n| &n.1)
    }

    /// Keeps a normal atlas while the ground reads its pages' normals
    /// (`needed`); a new one bakes every page again.
    fn keep_normals(&mut self, gpu: &Gpu, needed: bool) {
        if needed && self.normals.is_none() {
            let format = match self.staging.encode {
                Some(_) => wgpu::TextureFormat::Bc5RgUnorm,
                None => NORMAL_FORMAT,
            };
            let texture = page_atlas(gpu, "editor ground page normals", format, self.sheets);
            let view = sheets_view(&texture);
            self.normals = Some((texture, view));
            self.baked.fill(0);
        } else if !needed {
            self.normals = None;
        }
    }

    /// The glow atlas's view, when it has one.
    pub(super) fn glow_view(&self) -> Option<&wgpu::TextureView> {
        self.glow.as_ref().map(|g| &g.view)
    }

    /// Keeps a glow atlas and its staging while the ground declares glow;
    /// a new one bakes every page again.
    fn keep_glow(&mut self, gpu: &Gpu, needed: bool) {
        if needed && self.glow.is_none() {
            let atlas = page_atlas(gpu, "editor ground page glow", GLOW_FORMAT, self.sheets);
            let staging = gpu.device.create_texture(&wgpu::TextureDescriptor {
                label: Some("editor ground page glow staging"),
                size: wgpu::Extent3d {
                    width: PAGE_SIDE * PAGE_STAGING,
                    height: PAGE_SIDE,
                    depth_or_array_layers: 1,
                },
                mip_level_count: PAGE_MIPS,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: GLOW_FORMAT,
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
                view_formats: &[],
            });
            let mips = (0..PAGE_MIPS)
                .map(|mip| {
                    staging.create_view(&wgpu::TextureViewDescriptor {
                        base_mip_level: mip,
                        mip_level_count: Some(1),
                        ..Default::default()
                    })
                })
                .collect();
            self.glow = Some(GlowPages {
                view: sheets_view(&atlas),
                atlas,
                staging,
                mips,
            });
            self.baked.fill(0);
        } else if !needed {
            self.glow = None;
        }
    }

    /// Device bytes the pool's textures hold: its atlases and staging,
    /// every mip. The encoder's block buffer comes on top.
    #[cfg(test)]
    pub(super) fn bytes(&self) -> u64 {
        let staging = self.staging.textures.iter();
        let glow = self.glow.iter().flat_map(|g| [&g.atlas, &g.staging]);
        let normals = self.normals.iter().map(|n| &n.0);
        std::iter::once(&self.albedo)
            .chain(normals)
            .chain(staging)
            .chain(glow)
            .map(texture_bytes)
            .sum::<u64>()
    }

    /// Grows the atlases a sheet at a time until they hold the pages views
    /// used over the last `PAGE_RECENT` draws, this one's included, its
    /// `missing` ones and an eighth more, up to `PAGE_CAPACITY`. Pages keep
    /// their cells: sheets are added after, and the copy moves every mip of
    /// what is there. Never shrinks: a pane made smaller keeps its pool
    /// until the island closes.
    fn fit(&mut self, gpu: &Gpu, encoder: &mut wgpu::CommandEncoder, missing: usize) {
        let frame = self.frame;
        let used = (self.wanted.iter())
            .filter(|&&w| w != 0 && w + PAGE_RECENT > frame)
            .count();
        let needed = used + missing;
        let wanted = (needed + needed / 8) as u32;
        let sheets = wanted.div_ceil(PAGE_SHEET).clamp(self.sheets, PAGE_SHEETS);
        if sheets == self.sheets {
            return;
        }
        let had = self.sheets;
        let mut grow = |old: &wgpu::Texture, label: &str| {
            let new = page_atlas(gpu, label, old.format(), sheets);
            for mip in 0..PAGE_MIPS {
                let at = |texture| wgpu::TexelCopyTextureInfo {
                    texture,
                    mip_level: mip,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::All,
                };
                let side = (PAGE_SIDE * PAGE_SHEET_SIDE) >> mip;
                let size = wgpu::Extent3d {
                    width: side,
                    height: side,
                    depth_or_array_layers: had,
                };
                encoder.copy_texture_to_texture(at(old), at(&new), size);
            }
            new
        };
        self.albedo = grow(&self.albedo, "editor ground pages");
        self.albedo_view = sheets_view(&self.albedo);
        if let Some((texture, _)) = &self.normals {
            let texture = grow(texture, "editor ground page normals");
            let view = sheets_view(&texture);
            self.normals = Some((texture, view));
        }
        if let Some(glow) = &mut self.glow {
            glow.atlas = grow(&glow.atlas, "editor ground page glow");
            glow.view = sheets_view(&glow.atlas);
        }
        self.sheets = sheets;
        let pages = (sheets * PAGE_SHEET) as usize;
        self.owner.resize(pages, NONE);
        self.wanted.resize(pages, 0);
    }

    /// Finest-page cells per side: the index's size.
    fn side(&self) -> u32 {
        1 << (self.levels - 1)
    }

    fn id(&self, n: Node) -> usize {
        (self.offsets[n.level as usize] + n.z * (self.side() >> n.level) + n.x) as usize
    }

    fn size(level: u32) -> f32 {
        PAGE_LEAF * (1u32 << level) as f32
    }

    fn is_baked(&self, layer: u32) -> bool {
        self.baked[layer as usize / 64] & (1 << (layer % 64)) != 0
    }

    fn set_baked(&mut self, layer: u32, on: bool) {
        let (word, bit) = (layer as usize / 64, 1u64 << (layer % 64));
        if on {
            self.baked[word] |= bit;
        } else {
            self.baked[word] &= !bit;
        }
    }

    /// Moves the staged pages of `batch` (in staging cell order) into their
    /// layers of the pool: albedo encoded where it is compressed, the rest
    /// copied.
    fn store(
        &self,
        gpu: &Gpu,
        encoder_bc: &bc::Encoder,
        encoder: &mut wgpu::CommandEncoder,
        batch: &[(Node, u32)],
        timing: Option<&mut Timing>,
    ) {
        let staging = &self.staging;
        // A page's corner in its sheet, and the sheet.
        let cell = |layer: u32, mip: u32| {
            let side = PAGE_SIDE >> mip;
            let at = layer % PAGE_SHEET;
            let corner = [at % PAGE_SHEET_SIDE * side, at / PAGE_SHEET_SIDE * side];
            (corner, layer / PAGE_SHEET)
        };
        let normals = self.normals.as_ref().map(|n| &n.0);
        // Uncompressed albedo and normals copy; glow always does.
        let copied = (staging.textures.iter())
            .zip([Some(&self.albedo), normals])
            .filter(|_| staging.encode.is_none())
            .chain(self.glow.as_ref().map(|g| (&g.staging, Some(&g.atlas))));
        for (from, to) in copied {
            let Some(to) = to else {
                continue;
            };
            for (i, &(_, layer)) in batch.iter().enumerate() {
                for mip in 0..PAGE_MIPS {
                    let side = PAGE_SIDE >> mip;
                    let ([x, y], sheet) = cell(layer, mip);
                    encoder.copy_texture_to_texture(
                        wgpu::TexelCopyTextureInfo {
                            texture: from,
                            mip_level: mip,
                            origin: wgpu::Origin3d {
                                x: i as u32 * side,
                                y: 0,
                                z: 0,
                            },
                            aspect: wgpu::TextureAspect::All,
                        },
                        wgpu::TexelCopyTextureInfo {
                            texture: to,
                            mip_level: mip,
                            origin: wgpu::Origin3d { x, y, z: sheet },
                            aspect: wgpu::TextureAspect::All,
                        },
                        wgpu::Extent3d {
                            width: side,
                            height: side,
                            depth_or_array_layers: 1,
                        },
                    );
                }
            }
        }
        let Some((raw, blocks)) = &staging.encode else {
            return;
        };
        let mut timing = timing;
        let mut at = 0;
        for (k, (kind, to)) in [
            (bc::Kind::Rgba, Some(&self.albedo)),
            (bc::Kind::Rg, normals),
        ]
        .into_iter()
        .enumerate()
        {
            let Some(to) = to else {
                continue;
            };
            let jobs: Vec<bc::Job> = (0..batch.len() as u32)
                .flat_map(|i| (0..PAGE_MIPS).map(move |mip| page_job(i, mip, kind)))
                .collect();
            let writes = timing.as_mut().and_then(|t| t.compute("ground pages"));
            let offsets =
                encoder_bc.encode(&gpu.device, encoder, &raw[k], &jobs, blocks, at, writes);
            let layers = batch
                .iter()
                .flat_map(|&(_, layer)| std::iter::repeat_n(layer, PAGE_MIPS as usize));
            for ((job, &offset), layer) in jobs.iter().zip(&offsets).zip(layers) {
                let (corner, sheet) = cell(layer, job.mip);
                job.copy(encoder, blocks, offset, to, job.mip, corner, sheet);
            }
            at += jobs.iter().map(bc::Job::bytes).sum::<u64>();
        }
    }

    /// Unbakes the pages over `edit`, borders included.
    pub(super) fn invalidate(&mut self, edit: GroundEdit) {
        let r = match edit {
            GroundEdit::Heights(rect) => {
                self.stale = Some(self.stale.map_or(rect, |s| s.union(rect)));
                // A sample's normal reads its neighbours.
                let at =
                    |v: usize, pad: f32, a: usize| self.origin[a] + (v as f32 + pad) * self.spacing;
                [
                    at(rect.x0, -2., 0),
                    at(rect.z0, -2., 1),
                    at(rect.x1, 1., 0),
                    at(rect.z1, 1., 1),
                ]
            }
            GroundEdit::World(r) => r,
        };
        for level in 0..self.levels {
            let size = Self::size(level);
            let pad = size * PAGE_BORDER as f32 / PAGE_TEXELS as f32;
            let last = (self.side() >> level) as i64 - 1;
            let cell = |v: f32, o: f32| (((v - o) / size).floor() as i64).clamp(0, last) as u32;
            let (x0, x1) = (
                cell(r[0] - pad, self.origin[0]),
                cell(r[2] + pad, self.origin[0]),
            );
            let (z0, z1) = (
                cell(r[1] - pad, self.origin[1]),
                cell(r[3] + pad, self.origin[1]),
            );
            for z in z0..=z1 {
                for x in x0..=x1 {
                    let layer = self.node_layer[self.id(Node { level, x, z })];
                    if layer != NONE {
                        self.set_baked(layer, false);
                    }
                }
            }
        }
    }

    /// The nodes a view wants, each with how far its nearest point is
    /// (the clip w): per tile the coarsest whose texels cover at most
    /// `pixels` pixels there. Ground needing finer than the finest page
    /// gets none and shades live.
    fn select(
        &self,
        frustum: &Frustum,
        rows: &[[f64; 4]; 4],
        height: f32,
        pixels: f64,
    ) -> Vec<(Node, f64)> {
        let w = rows[3];
        let scale = (rows[1][0].powi(2) + rows[1][1].powi(2) + rows[1][2].powi(2)).sqrt()
            * f64::from(height)
            * 0.5;
        let mut out = Vec::new();
        let mut stack = vec![Node {
            level: self.levels - 1,
            x: 0,
            z: 0,
        }];
        while let Some(n) = stack.pop() {
            let size = Self::size(n.level);
            let o = [
                self.origin[0] + n.x as f32 * size,
                self.origin[1] + n.z as f32 * size,
            ];
            if o[0] >= self.origin[0] + self.extent[0] || o[1] >= self.origin[1] + self.extent[1] {
                continue;
            }
            let range = self.ranges[self.id(n)];
            let bounds = [[o[0], range[0], o[1]], [o[0] + size, range[1], o[1] + size]];
            if frustum.excludes(bounds) {
                continue;
            }
            let c: [f64; 3] = std::array::from_fn(|a| f64::from(bounds[0][a] + bounds[1][a]) * 0.5);
            let e: [f64; 3] = std::array::from_fn(|a| f64::from(bounds[1][a] - bounds[0][a]) * 0.5);
            let nearest = (0..3).map(|a| w[a] * c[a] - w[a].abs() * e[a]).sum::<f64>() + w[3];
            let ppm = if nearest > 1e-6 {
                scale / nearest
            } else {
                f64::INFINITY
            };
            // Up a slope a texel spans 1 / normal y of its width: by the
            // node's mean, so a few steep samples do not make it all finer.
            // Ground steeper than `flat` shades live anyway.
            let slope = f64::from(range[2].max(self.flat));
            let texel = f64::from(size) / f64::from(PAGE_TEXELS) / slope;
            if texel * ppm <= pixels {
                out.push((n, nearest));
            } else if n.level > 0 {
                for (dx, dz) in [(0, 0), (1, 0), (0, 1), (1, 1)] {
                    stack.push(Node {
                        level: n.level - 1,
                        x: n.x * 2 + dx,
                        z: n.z * 2 + dz,
                    });
                }
            }
        }
        out
    }

    /// Brings `ranges` up to the edited heights: the finest nodes from the
    /// samples, each coarser one from its four.
    fn update_ranges(&mut self, heights: &HeightMap) {
        let Some(rect) = self.stale.take() else {
            return;
        };
        let side = self.side() as usize;
        heights.cell_ranges(
            PAGE_LEAF,
            side,
            &mut self.ranges[..side * side],
            rect,
            self.flat,
        );
        let leaf = PAGE_LEAF / self.spacing;
        let (mut x0, mut z0) = (
            (rect.x0 as f32 / leaf) as u32,
            (rect.z0 as f32 / leaf) as u32,
        );
        let (mut x1, mut z1) = (
            (rect.x1 as f32 / leaf) as u32 + 1,
            (rect.z1 as f32 / leaf) as u32 + 1,
        );
        for level in 1..self.levels {
            (x0, z0, x1, z1) = (
                x0.saturating_sub(1) / 2,
                z0.saturating_sub(1) / 2,
                x1 / 2 + 1,
                z1 / 2 + 1,
            );
            let n = self.side() >> level;
            for z in z0..z1.min(n) {
                for x in x0..x1.min(n) {
                    let mut range = [f32::INFINITY, f32::NEG_INFINITY, 0.];
                    for (dx, dz) in [(0, 0), (1, 0), (0, 1), (1, 1)] {
                        let child = self.ranges[self.id(Node {
                            level: level - 1,
                            x: x * 2 + dx,
                            z: z * 2 + dz,
                        })];
                        range = [
                            range[0].min(child[0]),
                            range[1].max(child[1]),
                            range[2] + child[2] * 0.25,
                        ];
                    }
                    let id = self.id(Node { level, x, z });
                    self.ranges[id] = range;
                }
            }
        }
    }

    /// A free layer, or the one no view wanted for longest; none while
    /// every layer was wanted this frame.
    fn take_layer(&mut self) -> Option<u32> {
        let frame = self.frame;
        let layer = (0..self.owner.len())
            .filter(|&l| self.wanted[l] < frame)
            .min_by_key(|&l| (self.owner[l] != NONE, self.wanted[l]))? as u32;
        let old = self.owner[layer as usize];
        if old != NONE {
            self.node_layer[old as usize] = NONE;
        }
        self.set_baked(layer, false);
        Some(layer)
    }
}

impl PageIndex {
    fn new(gpu: &Gpu, side: u32) -> Self {
        let (texture, view) = texture_2d(
            gpu,
            "editor ground page index",
            [side, side],
            wgpu::TextureFormat::R32Uint,
            wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        );
        let entries = vec![0; (side * side) as usize];
        write_texture_rect(
            &gpu.queue,
            &texture,
            [0, 0],
            [side, side],
            4,
            bytemuck::cast_slice(&entries),
        );
        Self {
            texture,
            view,
            side,
            entries,
            tiles: Vec::new(),
        }
    }
}

/// How many pages the last view picked, of them baked, and baked that
/// frame.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PageCounts {
    pub wanted: usize,
    pub baked: usize,
    pub new: usize,
}

impl Device {
    /// The view's ground pages for this frame: picks them, bakes what it
    /// can of those missing into the pool, and writes the view's index.
    /// Returns `Lights::pages` and the counts; zero lights while the
    /// ground cannot bake (still preparing, or none mounted).
    #[allow(clippy::too_many_arguments)]
    pub(super) fn ground_pages(
        &mut self,
        gpu: &Gpu,
        encoder: &mut wgpu::CommandEncoder,
        targets: &mut Targets,
        scene: &PreparedScene,
        frustum: &Frustum,
        rows: &[[f64; 4]; 4],
        height: f32,
        globals: &Globals,
        lights: &Lights,
        ids: &wgpu::Buffer,
        slot: &mut dyn FnMut([u32; 4]) -> u32,
        bc: bool,
    ) -> ([f32; 4], PageCounts) {
        let none = ([0.; 4], PageCounts::default());
        let Some(t) = scene.terrain.as_ref().filter(|_| !self.detail.live_ground) else {
            return none;
        };
        let Some(draw) = self.resident.get(t.first) else {
            return none;
        };
        let Some(surface) = self.surface.as_ref() else {
            return none;
        };
        // No program: the ground's shader failed, and the terrain's own
        // material draws without pages.
        let (Some(program), Some(bindings)) =
            (self.ground_program.as_deref(), surface.ground.bind_group())
        else {
            return none;
        };
        // The bake samples the tint only once its map is in.
        let tint = (self.ground_tint.as_ref())
            .filter(|t| t.1.ready())
            .map_or([0; 4], |_| lights.ground_tint.map(f32::to_bits));
        let key = PageKey {
            terrain: Arc::as_ptr(&t.source) as usize,
            ground: surface.identity,
            toned: lights.tone[0] > 0.,
            tint,
        };
        let grid = t.source.heights.grid();
        if self
            .ground_pages
            .as_ref()
            .is_none_or(|p| p.key.terrain != key.terrain)
        {
            self.ground_pages = Some(GroundPages::new(gpu, grid, key, bc));
        }
        let pages = self.ground_pages.as_mut().expect("made above");
        if pages.key != key {
            pages.key = key;
            pages.baked.fill(0);
        }
        let relief = program.relief;
        pages.keep_normals(gpu, program.normals.is_some() || relief);
        pages.keep_glow(gpu, program.glow);
        let flat = page_flat(relief);
        if pages.flat != flat {
            // The ranges' slopes are clamped to it; the bake is the same.
            pages.flat = flat;
            pages.stale = Some(Rect {
                x0: 0,
                z0: 0,
                x1: grid.width,
                z1: grid.depth,
            });
        }
        pages.frame += 1;
        let frame = pages.frame;
        // Coarser until the pool holds the view's pages, with room for
        // another view's.
        pages.update_ranges(&t.source.heights);
        let mut pixels = PAGE_TEXEL_PIXELS;
        let mut picked = pages.select(frustum, rows, height, pixels);
        while picked.len() > PAGE_PICK {
            pixels *= 2.;
            picked = pages.select(frustum, rows, height, pixels);
        }
        let mut missing = Vec::new();
        for &(n, nearest) in &picked {
            let layer = pages.node_layer[pages.id(n)];
            if layer != NONE {
                pages.wanted[layer as usize] = frame;
            }
            if layer == NONE || !pages.is_baked(layer) {
                missing.push((n, nearest));
            }
        }
        pages.fit(gpu, encoder, missing.len());
        missing.sort_by(|a, b| a.1.total_cmp(&b.1));
        let budget = if missing.len() * 2 > picked.len() {
            PAGE_BURST
        } else {
            PAGE_BUDGET
        };
        let mut jobs: Vec<(Node, u32)> = Vec::with_capacity(budget.min(missing.len()));
        for &(n, _) in missing.iter().take(budget) {
            let id = pages.id(n);
            let layer = match pages.node_layer[id] {
                NONE => {
                    let Some(layer) = pages.take_layer() else {
                        break;
                    };
                    pages.node_layer[id] = layer;
                    pages.owner[layer as usize] = id as u32;
                    layer
                }
                layer => layer,
            };
            pages.wanted[layer as usize] = frame;
            jobs.push((n, layer));
        }
        if !jobs.is_empty() {
            let frame_group = self.frame_group(gpu, globals, lights, None, None, None, None);
            let pages = self.ground_pages.as_ref().expect("made above");
            let staging = &pages.staging;
            // Per batch, per mip, then job: its staging cell, its first
            // texel's world corner, side and gradient scale
            // (PAGE_BAKE_GRADIENT), the staging mip's size (vs_page_bake).
            let mut quads: Vec<Mat4> = Vec::with_capacity(jobs.len() * PAGE_MIPS as usize);
            for batch in jobs.chunks(PAGE_STAGING as usize) {
                for mip in 0..PAGE_MIPS {
                    let side = PAGE_SIDE >> mip;
                    let target = [(PAGE_STAGING * side) as f32, side as f32];
                    for (cell, &(n, _)) in batch.iter().enumerate() {
                        let size = GroundPages::size(n.level);
                        let corner = [
                            pages.origin[0] + n.x as f32 * size
                                - size * PAGE_BORDER as f32 / PAGE_TEXELS as f32,
                            pages.origin[1] + n.z as f32 * size
                                - size * PAGE_BORDER as f32 / PAGE_TEXELS as f32,
                        ];
                        let texel = size / (PAGE_TEXELS >> mip) as f32;
                        #[rustfmt::skip]
                        quads.push([
                            (cell as u32 * side) as f32, 0., side as f32, side as f32,
                            corner[0], corner[1], texel, PAGE_BAKE_GRADIENT,
                            target[0], target[1], 0., 0.,
                            0., 0., 0., 0.,
                        ]);
                    }
                }
            }
            let buffer = buffer_init(
                gpu,
                "ground page jobs",
                bytemuck::cast_slice(&quads),
                wgpu::BufferUsages::STORAGE,
            );
            let group = self.draw_group(gpu, ids, Some(&buffer));
            let offset = slot([0; 4]);
            let attachment = |view| {
                Some(wgpu::RenderPassColorAttachment {
                    view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                        store: wgpu::StoreOp::Store,
                    },
                    depth_slice: None,
                })
            };
            let mut first = 0;
            for batch in jobs.chunks(PAGE_STAGING as usize) {
                let count = batch.len() as u32;
                for (mip, [albedo, normal]) in staging.mips.iter().enumerate() {
                    let timestamp_writes = targets
                        .timing
                        .as_mut()
                        .and_then(|t| t.render("ground pages"));
                    // The program's targets: glow exactly when it bakes glow
                    // (`keep_glow` above).
                    let glow = (pages.glow.as_ref()).map(|g| attachment(&g.mips[mip]));
                    let attachments = [attachment(albedo), attachment(normal), glow.flatten()];
                    let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                        label: Some("editor ground pages"),
                        color_attachments: &attachments[..2 + usize::from(pages.glow.is_some())],
                        depth_stencil_attachment: None,
                        timestamp_writes,
                        occlusion_query_set: None,
                        multiview_mask: None,
                    });
                    pass.set_pipeline(&program.bake);
                    pass.set_bind_group(0, &frame_group, &[]);
                    pass.set_bind_group(1, draw.mesh.patch.as_ref().unwrap(), &[]);
                    pass.set_bind_group(3, bindings, &[]);
                    pass.set_bind_group(2, &group, &[offset]);
                    pass.draw(0..6, first..first + count);
                    first += count;
                }
                pages.store(gpu, &self.bc, encoder, batch, targets.timing.as_mut());
            }
        }
        let pages = self.ground_pages.as_mut().expect("made above");
        for &(_, layer) in &jobs {
            pages.set_baked(layer, true);
        }
        // The view's index: each picked node's baked page over its cells.
        let side = pages.side();
        let index = match &mut targets.pages {
            Some(i) if i.side == side => i,
            slot => slot.insert(PageIndex::new(gpu, side)),
        };
        let mut entries = vec![0u32; (side * side) as usize];
        let mut baked = 0;
        for &(n, _) in &picked {
            let layer = pages.node_layer[pages.id(n)];
            if layer == NONE || !pages.is_baked(layer) {
                continue;
            }
            baked += 1;
            let entry = 0x8000_0000 | n.level << 12 | layer;
            let span = 1u32 << n.level;
            let (x0, z0) = (n.x * span, n.z * span);
            for z in z0..(z0 + span).min(side) {
                let row = (z * side) as usize;
                entries[row + x0 as usize..row + (x0 + span).min(side) as usize].fill(entry);
            }
        }
        if entries != index.entries {
            write_texture_rect(
                &gpu.queue,
                &index.texture,
                [0, 0],
                [side, side],
                4,
                bytemuck::cast_slice(&entries),
            );
            index.entries = entries;
        }
        index.tiles = t.paged_tiles(&index.entries, side, pages.origin, PAGE_LEAF);
        (
            [pages.origin[0], pages.origin[1], PAGE_LEAF, pages.flat],
            PageCounts {
                wanted: picked.len(),
                baked,
                new: jobs.len(),
            },
        )
    }
}

fn page_pipeline(
    d: &wgpu::Device,
    meshes: &wgpu::PipelineLayout,
    shader: &wgpu::ShaderModule,
    glow: bool,
) -> wgpu::RenderPipeline {
    let formats = PAGE_FORMATS.iter().chain(glow.then_some(&GLOW_FORMAT));
    d.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("editor ground page bake"),
        layout: Some(meshes),
        vertex: wgpu::VertexState {
            module: shader,
            entry_point: Some("vs_page_bake"),
            buffers: &[],
            compilation_options: Default::default(),
        },
        fragment: Some(wgpu::FragmentState {
            module: shader,
            entry_point: Some(if glow {
                "fs_page_bake_glow"
            } else {
                "fs_page_bake"
            }),
            targets: &formats
                .map(|&format| {
                    Some(wgpu::ColorTargetState {
                        format,
                        blend: None,
                        write_mask: wgpu::ColorWrites::ALL,
                    })
                })
                .collect::<Vec<_>>(),
            compilation_options: Default::default(),
        }),
        primitive: wgpu::PrimitiveState::default(),
        depth_stencil: None,
        multisample: Default::default(),
        multiview_mask: None,
        cache: None,
    })
}

const MESH_SHADER: &str = include_str!("../gpu.wgsl");
/// gpu.wgsl before and after its stand-in ground, where a ground program
/// puts the ground's own.
fn shader_parts() -> (&'static str, &'static str) {
    let (before, rest) = MESH_SHADER.split_once("// BEGIN GROUND").unwrap();
    let (_, after) = rest.split_once("// END GROUND").unwrap();
    (before, after)
}

pub(super) fn patch_binding(b: u32) -> bool {
    matches!(b, 0..=3 | 10..=12)
}

/// Caster moves `MountedGround::moved` and height edits
/// `Assets::height_moves` keep: a map that missed more draws whole. One a
/// frame while painting or sculpting; a view's cascades lag the scene's map
/// by at most a few frames.
pub(super) const MOVES_KEPT: usize = 64;

/// Where height edits reached the GPU, `(serial, map key, samples)`, the
/// last `MOVES_KEPT`. Every map synced in one frame shares that frame's
/// serial, so dropping the oldest entry can leave part of a serial behind:
/// the newest serial an entry left from is kept, and a shadow map drawn
/// before it draws whole.
#[derive(Default)]
pub(super) struct HeightMoves {
    moves: std::collections::VecDeque<(u64, usize, Rect)>,
    dropped: u64,
}

impl HeightMoves {
    pub(super) fn push(&mut self, serial: u64, map: usize, rect: Rect) {
        if self.moves.len() == MOVES_KEPT
            && let Some((old, ..)) = self.moves.pop_front()
        {
            self.dropped = self.dropped.max(old);
        }
        self.moves.push_back((serial, map, rect));
    }

    /// Every move after serial `drawn` up to `now`, as (map, samples), or
    /// `None` when the log lost one of them.
    pub(super) fn since(
        &self,
        drawn: u64,
        now: u64,
    ) -> Option<impl Iterator<Item = (usize, Rect)> + '_> {
        (self.dropped <= drawn).then(|| {
            (self.moves.iter())
                .filter(move |m| m.0 > drawn && m.0 <= now)
                .map(|&(_, map, rect)| (map, rect))
        })
    }
}

pub(super) struct MountedGround {
    terrain: Arc<Terrain>,
    factory: GroundFactory,
    identity: usize,
    pub(super) uploading: bool,
    /// The version, edit serial and relief moves last taken in.
    seen: Option<(u64, u64, Option<u64>)>,
    /// Bumped by every change taken in: what the terrain pass draws.
    revision: u64,
    /// Bumped by those that can move the displaced surface: what it casts.
    /// A paint dab redrew the shadow map and cascades (4 ms on the pirate
    /// island) when this followed `revision`.
    caster: u64,
    /// World XZ each of the last caster serials moved the surface in,
    /// oldest first and consecutive: a shadow map drawn at a serial still
    /// here redraws only those (`Device::ground_moved`). Cleared by a move
    /// with no place, such as a version change.
    moved: std::collections::VecDeque<(u64, [f32; 4])>,
    edits: Vec<[f32; 4]>,
    pub(super) ground: Box<dyn Ground>,
}

pub(super) struct GroundProgram {
    layout: wgpu::BindGroupLayout,
    wgsl: String,
    /// The scene's `shade` and `fog` it ends with (`Families::scene`).
    scene: String,
    relief: bool,
    normals: Option<f32>,
    glow: bool,
    specular: super::Specular,
    live: wgpu::RenderPipeline,
    paged: [wgpu::RenderPipeline; 2],
    shadow: wgpu::RenderPipeline,
    bake: wgpu::RenderPipeline,
}

/// Ground programs built so far, per device and target format. Every renderer
/// on a device (the editor's view, catalogue thumbnails, a new island's
/// preview) mounts the same few grounds, and each program is five pipelines
/// over the whole mesh shader. Bounded: a host would need more than 16
/// distinct grounds alive at once to rebuild one.
static PROGRAMS: Mutex<Vec<(wgpu::Device, wgpu::TextureFormat, Arc<GroundProgram>)>> =
    Mutex::new(Vec::new());
const PROGRAMS_KEPT: usize = 16;

/// Lets a lost device's programs go (`gpu::forget`).
pub(super) fn forget(device: &wgpu::Device) {
    (PROGRAMS.lock().unwrap_or_else(|e| e.into_inner())).retain(|(d, ..)| d != device);
}

/// What a ground program is built from: the ground's shader and group 3
/// layout, the scene's `shade` and `fog` it ends with (`Families::scene`),
/// and the features the kit compiles in.
#[derive(Clone, Copy)]
pub(super) struct ProgramKey<'a> {
    layout: &'a wgpu::BindGroupLayout,
    wgsl: &'a str,
    scene: &'a str,
    relief: bool,
    normals: Option<f32>,
    glow: bool,
    specular: super::Specular,
}

impl<'a> ProgramKey<'a> {
    fn of(ground: &'a dyn Ground, scene: &'a str) -> Self {
        Self {
            layout: ground.layout(),
            wgsl: ground.wgsl(),
            scene,
            relief: ground.has_relief(),
            normals: ground.normal_footprint(),
            glow: ground.glow(),
            specular: ground.specular(),
        }
    }
}

impl GroundProgram {
    fn matches(&self, key: ProgramKey<'_>) -> bool {
        &self.layout == key.layout
            && self.wgsl == key.wgsl
            && self.scene == key.scene
            && self.relief == key.relief
            && self.normals == key.normals
            && self.glow == key.glow
            && self.specular == key.specular
    }

    /// `key`'s program on `d`, from the device's cache or built and kept
    /// there; the second is whether it was built now. `None` when its
    /// shader fails to build.
    fn fetch(
        key: ProgramKey<'_>,
        d: &wgpu::Device,
        kit: &Kit,
        format: wgpu::TextureFormat,
    ) -> (Option<Arc<Self>>, bool) {
        let found = PROGRAMS.lock_any().iter().find_map(|(device, f, p)| {
            (device == d && *f == format && p.matches(key)).then(|| p.clone())
        });
        if found.is_some() {
            return (found, false);
        }
        let built = Self::build(key, d, kit, format).map(Arc::new);
        if let Some(built) = &built {
            let mut programs = PROGRAMS.lock_any();
            if programs.len() == PROGRAMS_KEPT {
                programs.remove(0);
            }
            programs.push((d.clone(), format, built.clone()));
        }
        (built, true)
    }

    /// Points `current` at `ground`'s program, built on first use, and
    /// returns whether it was built now. `current` is `None` when its
    /// shader fails to build: the terrain's own material draws instead, as
    /// while a ground prepares.
    fn sync(
        current: &mut Option<Arc<Self>>,
        ground: &dyn Ground,
        scene: &str,
        d: &wgpu::Device,
        kit: &Kit,
        format: wgpu::TextureFormat,
    ) -> bool {
        let key = ProgramKey::of(ground, scene);
        if current.as_ref().is_some_and(|p| p.matches(key)) {
            return false;
        }
        let built;
        (*current, built) = Self::fetch(key, d, kit, format);
        built
    }

    fn build(
        key: ProgramKey<'_>,
        d: &wgpu::Device,
        kit: &Kit,
        format: wgpu::TextureFormat,
    ) -> Option<Self> {
        let ProgramKey {
            layout: ground_layout,
            wgsl,
            scene,
            relief,
            normals,
            glow,
            specular,
        } = key;
        let (before, after) = shader_parts();
        let features = format!(
            "fn relief() -> bool {{ return {relief}; }}\nfn ground_glows() -> bool {{ return {glow}; }}\nfn ground_normals() -> bool {{ return {}; }}\nfn ground_specular() -> u32 {{ return {}u; }}\nfn ground_page_normal(dw: array<vec3<f32>, 2>) -> bool {{ return {relief} || max(length(dw[0]), length(dw[1])) <= {:?}; }}\n",
            normals.is_some(),
            specular.0,
            normals.unwrap_or(-1.).min(f32::MAX)
        );
        let source = [
            pass::half_types(d),
            pass::FRAME_PRELUDE,
            before,
            &features,
            wgsl,
            after,
            scene,
        ]
        .concat();
        // A host's WGSL is checked here, not at its first draw: unscoped, a
        // shader error is wgpu's uncaught-error panic.
        let scope = d.push_error_scope(wgpu::ErrorFilter::Validation);
        let shader = d.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("terrain ground"),
            source: wgpu::ShaderSource::Wgsl(source.into()),
        });
        let layout = d.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("terrain ground"),
            bind_group_layouts: &[
                Some(&kit.frame_layout),
                Some(&kit.patch_layout),
                Some(&kit.draw_layout),
                Some(ground_layout),
            ],
            immediate_size: 0,
        });
        // The ground's own vertices (vs_ground) hand on only what its
        // fragments read; the shadow writes depth alone.
        let mesh = |name, fragment, phase| {
            mesh_pipeline(
                d,
                format,
                name,
                &shader,
                &layout,
                "vs_ground",
                fragment,
                phase,
            )
        };
        let program = GroundProgram {
            layout: ground_layout.clone(),
            wgsl: wgsl.into(),
            scene: scene.into(),
            relief,
            normals,
            glow,
            specular,
            live: mesh("terrain ground", "fs_ground", Phase::Opaque),
            paged: [
                mesh("paged terrain", "fs_ground_paged", Phase::Opaque),
                mesh("live terrain past pages", "fs_ground", Phase::Over),
            ],
            shadow: mesh_pipeline(
                d,
                format,
                "terrain shadow",
                &shader,
                &layout,
                "vs_mesh",
                "fs_shadow",
                Phase::Shadow,
            ),
            bake: page_pipeline(d, &layout, &shader, glow),
        };
        // Native backends report validation as the call returns, so the
        // scope's future is already resolved.
        let mut error = std::pin::pin!(scope.pop());
        let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
        match error.as_mut().poll(&mut cx) {
            std::task::Poll::Ready(Some(e)) => {
                eprintln!("ground shader: {e:?}; drawing the terrain's own material");
                None
            }
            _ => Some(program),
        }
    }
}

impl Warm<'_> {
    /// Builds the programs the kit draws a host ground with, one per
    /// `(relief, normal footprint)` variant the ground can report
    /// (`Ground::has_relief`, `Ground::normal_footprint`), so the first
    /// island on a device doesn't compile them on the render thread: each
    /// is five pipelines over the whole mesh shader. Programs match the
    /// ground's layout by identity, so `layout` must be the object its
    /// `layout()` returns later: one made with `per_device`, or the kit's.
    /// Programs also carry the scene's `shade` and fog, so `families` must
    /// be the set the island's renderer draws with.
    pub fn ground(
        &self,
        families: &super::placed::Families,
        wgsl: &str,
        layout: &wgpu::BindGroupLayout,
        glow: bool,
        specular: super::Specular,
        variants: &[(bool, Option<f32>)],
    ) {
        let scene = families.scene();
        for &(relief, normals) in variants {
            let key = ProgramKey {
                layout,
                wgsl,
                scene: &scene,
                relief,
                normals,
                glow,
                specular,
            };
            GroundProgram::fetch(key, self.device, self.kit, self.format);
        }
    }
}

impl Device {
    /// Mounts the scene's ground (`GroundDraw::Surface`), or none: the
    /// kit's own draws the terrain's material, unpaged.
    pub(super) fn mount_ground(&mut self, gpu: &Gpu, scene: &PreparedScene) {
        let (Some(t), super::GroundDraw::Surface(factory)) = (
            scene.terrain.as_ref().filter(|_| scene.ground.is_some()),
            &scene.ground_draw,
        ) else {
            self.surface = None;
            return;
        };
        if self
            .surface
            .as_ref()
            .is_some_and(|s| Arc::ptr_eq(&s.terrain, &t.source) && Arc::ptr_eq(&s.factory, factory))
        {
            return;
        }
        let ground = factory(GroundInit {
            device: &gpu.device,
            grid: t.source.heights.grid(),
        });
        let identity = self.surface.as_ref().map_or(1, |s| s.identity + 1);
        self.surface = Some(MountedGround {
            terrain: t.source.clone(),
            factory: factory.clone(),
            identity,
            uploading: true,
            seen: None,
            revision: 0,
            caster: 0,
            moved: std::collections::VecDeque::new(),
            edits: Vec::new(),
            ground,
        });
        if let Some(pages) = &mut self.ground_pages {
            pages.baked.fill(0);
        }
    }

    pub(super) fn prepare_ground(&mut self, frame: &mut GroundPrepare) -> bool {
        let Some(surface) = &mut self.surface else {
            return false;
        };
        surface.uploading = !surface.ground.prepare(frame);
        let previous = surface.seen;
        surface.seen = if surface.ground.bind_group().is_some() {
            surface.edits.clear();
            let version = surface.ground.version();
            let since = previous.map_or(0, |(_, s, _)| s);
            let serial = surface.ground.edits(since, &mut surface.edits);
            Some((version, serial, surface.ground.relief_moves()))
        } else {
            None
        };
        if previous == surface.seen {
            return false;
        }
        if let Some((version, _, _)) = surface.seen
            && previous.is_none_or(|(old, _, _)| old != version)
        {
            let built = GroundProgram::sync(
                &mut self.ground_program,
                surface.ground.as_ref(),
                &self.scene,
                frame.device(),
                &self.kit,
                self.format,
            );
            self.ground_builds += u32::from(built);
        }
        surface.revision += 1;
        let moved = match (previous, surface.seen) {
            (Some((v, _, before)), Some((w, _, now))) => v != w || now.is_none() || before != now,
            _ => true,
        };
        if moved {
            surface.caster += 1;
            // Within its edits, unless the version changed or it is new.
            let same = previous.zip(surface.seen).is_some_and(|(a, b)| a.0 == b.0);
            let area = (surface.edits.iter().copied()).reduce(|a, b| {
                [
                    a[0].min(b[0]),
                    a[1].min(b[1]),
                    a[2].max(b[2]),
                    a[3].max(b[3]),
                ]
            });
            match area.filter(|_| same) {
                Some(area) => {
                    if surface.moved.len() == MOVES_KEPT {
                        surface.moved.pop_front();
                    }
                    surface.moved.push_back((surface.caster, area));
                }
                None => surface.moved.clear(),
            }
        }
        if let Some(pages) = &mut self.ground_pages {
            if previous
                .zip(surface.seen)
                .is_some_and(|((v, _, _), (w, _, _))| v == w)
            {
                for &rect in &surface.edits {
                    pages.invalidate(GroundEdit::World(rect));
                }
            } else {
                pages.baked.fill(0);
            }
        }
        true
    }

    /// The world box a shadow map drawn under `drawn` must draw again for
    /// `now`, when the kit's terrain is all that moved since (its ground's
    /// relief, its heights, or both) and the logs still hold where: the
    /// moved area through the heights' range and the relief either way.
    /// Wider by a cell of the coarsest stride shadows draw the ground at,
    /// whose triangles lean from a moved vertex that far out, and by the
    /// relief, which displaces along a leaning normal. A height edit also
    /// moves every placement row that bends to the terrain and stands on
    /// it, all of the row: wider by those rows' footprints and taller by
    /// their reach.
    pub(super) fn ground_moved(
        &self,
        scene: &PreparedScene,
        casters: &[(usize, &Arc<dyn Pass>, Caster)],
        drawn: &ShadowVersion,
        now: &ShadowVersion,
        placed: &super::placed::Placed,
        height_moves: &HeightMoves,
    ) -> Option<[Vec3; 2]> {
        let surface = self.surface.as_ref()?;
        let t = scene.terrain.as_ref()?;
        let terrain = scene.ground.is_some().then_some(0usize)?;
        let (_, terrain_pass, _) = casters.iter().find(|(i, ..)| *i == terrain)?;
        if !Arc::ptr_eq(terrain_pass, &scene.passes[terrain]) {
            return None;
        }
        if drawn.0 != now.0 || drawn.2 != now.2 || drawn.3.len() != now.3.len() {
            return None;
        }
        let union = |a: [f32; 4], b: [f32; 4]| {
            [
                a[0].min(b[0]),
                a[1].min(b[1]),
                a[2].max(b[2]),
                a[3].max(b[3]),
            ]
        };
        // Where the ground's relief moved, from its caster log.
        let mut old = None;
        for (a, b) in drawn.3.iter().zip(&now.3) {
            match (a.0 == b.0, a.1 == b.1) {
                (true, true) => {}
                (true, false) if a.0 == terrain && b.1 == surface.caster => old = Some(a.1),
                _ => return None,
            }
        }
        let relief_area = match old {
            None => None,
            Some(old) => {
                let first = surface.moved.front()?.0;
                if first > old + 1 || surface.moved.back()?.0 != surface.caster {
                    return None;
                }
                let moved = surface.moved.iter().filter(|(serial, _)| *serial > old);
                Some(moved.map(|&(_, a)| a).reduce(union)?)
            }
        };
        // Where the heights moved, from the edits that reached the GPU. Only
        // the terrain's tiles may displace by its map: another mesh on it
        // would move by its own cells.
        let heights = &t.source.heights;
        let height_area = if drawn.1 == now.1 {
            None
        } else {
            let moves = height_moves.since(drawn.1, now.1)?;
            let tiles = t.first..t.first + t.triangles.len();
            let shares = (scene.batches.iter().enumerate())
                .filter(|(i, _)| !tiles.contains(i))
                .any(|(_, b)| {
                    (b.batch.mesh.data().heights.as_ref()).is_some_and(|h| Arc::ptr_eq(h, heights))
                });
            if shares {
                return None;
            }
            let g = heights.grid();
            let mut area = None;
            for (map, r) in moves {
                if map != super::key(heights) {
                    return None;
                }
                let at = |v: usize, a: usize| g.origin[a] + v as f32 * g.spacing;
                let rect = [at(r.x0, 0), at(r.z0, 1), at(r.x1, 0), at(r.z1, 1)];
                area = Some(area.map_or(rect, |a| union(a, rect)));
            }
            Some(area?)
        };
        let area = match (relief_area, height_area) {
            (Some(a), Some(b)) => union(a, b),
            (a, b) => a.or(b)?,
        };
        let [lo, hi] = heights.range();
        let relief = surface.ground.relief_bound();
        let pad = heights.grid().spacing * TERRAIN_STRIDES[TERRAIN_SHADOW_LEVEL] + relief;
        let mut region = [area[0] - pad, area[1] - pad, area[2] + pad, area[3] + pad];
        let mut rise = relief;
        if let Some(h) = height_area {
            // A vertex reads the ground a cell around the samples it sits
            // between.
            let s = heights.grid().spacing;
            let read = [h[0] - s, h[1] - s, h[2] + s, h[3] + s];
            let touched = |f: &[f32; 4]| {
                f[0] <= read[2] && read[0] <= f[2] && f[1] <= read[3] && read[1] <= f[3]
            };
            for (footprint, up) in placed.adapting.iter().filter(|(f, _)| touched(f)) {
                region = union(region, *footprint);
                rise = rise.max(relief + up);
            }
        }
        Some([
            [region[0], lo - rise, region[1]],
            [region[2], hi + rise, region[3]],
        ])
    }

    fn active_ground(&self) -> Option<(&dyn Ground, &GroundProgram, &wgpu::BindGroup)> {
        let g = self.surface.as_ref()?.ground.as_ref();
        Some((g, self.ground_program.as_deref()?, g.bind_group()?))
    }
}

fn bind_patch(
    pass: &mut wgpu::RenderPass<'_>,
    mesh: &ResidentMesh,
    active: Option<(&dyn Ground, &GroundProgram, &wgpu::BindGroup)>,
) {
    if let Some((_, _, group)) = active {
        pass.set_bind_group(1, mesh.patch.as_ref().unwrap(), &[]);
        pass.set_bind_group(3, group, &[]);
    } else {
        pass.set_bind_group(1, &mesh.group, &[]);
    }
}
