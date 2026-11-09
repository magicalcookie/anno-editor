// GPU-driven drawing of placed model pieces. The host hands over a table of
// placements (piece index, transform) once per structural change and moves
// rows in place; nothing here is rebuilt per frame on the CPU. Each frame a
// compute pass (cull.wgsl) culls every placement and picks its detail
// level, filling per-(piece, level) visible lists and indirect arguments
// that the opaque and shadow passes draw from. Where the device allows it,
// every resident piece level's geometry, material and texture also sit in
// one shared arena (placed.wgsl), so each pipeline draws all placements with
// one multi-draw; otherwise each (piece, level) is its own indirect draw.
use super::{Assets, Budget, Gpu, Kit, Material, ResidentMesh, buffer_init, entry, key};
use crate::math::{self, Mat4, Vec3};
use bytemuck::{Pod, Zeroable};
use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::sync::Arc;

use super::Mesh;

/// One drawable part of a model with its detail levels: each level's mesh
/// and the screen size, in pixels per metre of the mesh's own space at the
/// placement's nearest point (so a placement scaled up keeps detail
/// further out), from which it draws. Pieces with an impostor count world
/// metres instead: foliage thresholds go by how a tree looks, and keeping
/// every scaled-up tree detailed twice as far quadrupled an island's
/// triangles. Levels go from full detail down; the last draws below every
/// threshold. At most [`MAX_LEVELS`], the impostor included.
pub struct Piece {
    pub levels: Vec<(Arc<Mesh>, f32)>,
    /// Below this many pixels per metre, draw a quad showing the full-detail
    /// mesh baked from the nearest of [`IMPOSTOR_VIEWS`]² directions above
    /// the horizon, instead of any mesh. For foliage far away, where meshes
    /// cost thousands of triangles for a few pixels. Needs the device to
    /// draw placements from one arena; elsewhere the last level stays.
    pub impostor: Option<f32>,
}

pub const MAX_LEVELS: usize = 4;

/// Impostors bake an 8x8 grid of views over the upper hemisphere, 40 px
/// each: one 320² layer per distinct mesh, 614 KB with its two-byte
/// normals. Trees switch to impostors at around 40 px, where a 20% thinned
/// laurel still costs 1.7k triangles; from above they look much alike. A
/// 768 m island bakes 659 layers, 405 MB (at 24 px it was 145 MB, and
/// trees kept their meshes down to 16 px).
pub const IMPOSTOR_VIEWS: u32 = 8;
pub(super) const IMPOSTOR_TILE: u32 = 40;

/// Words per draw in the indirect arguments: indexed draws' five (index
/// count, instances, first index, base vertex, first instance). Draws
/// without the arena read the first four as a plain draw's, all but the
/// counts zero.
pub(super) const ARGS: u32 = 5;

/// Prefix draws of a thinning mesh (`MeshData::thinning`), cull.wgsl
/// `THIN_BUCKETS`: each placement draws the shortest that holds the parts it
/// keeps. Their ends grow geometrically from `least` to 1 (10% apart for a
/// least of a fifth), so a placement never draws much more than it keeps.
pub(super) const THIN_BUCKETS: u32 = 16;
/// Parts ranked up to this far past the share kept shrink away rather than
/// vanish (placed.wgsl `THIN_FADE`), so buckets hold them too.
const THIN_FADE: f32 = 0.05;

/// The rank up to which bucket `b` of a thinning mesh draws its parts.
pub(super) fn bucket_end(least: f32, b: u32) -> f32 {
    let least = least.max(1e-3);
    least * (1. / least).powf((b + 1) as f32 / THIN_BUCKETS as f32) * (1. + THIN_FADE)
}

/// Bit set in pick handles of placements; the rest is the row.
pub(super) const PLACED: u32 = 1 << 31;

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct GpuDraw {
    albedo: u32,
    /// Bit 0 has normals, bit 1 ignores texture alpha.
    flags: u32,
    gain: f32,
    /// Impostors (flag bit 2): the material of the full-detail level they
    /// bake. `albedo` is then their atlas layer.
    source: u32,
    /// Normal map slot in the low half, surface map slot in the high half;
    /// 0 is none.
    maps: u32,
    /// `MeshData::terrain_tint`.
    tint: f32,
    /// The mesh's first vertex, and where its thinning parts start in the
    /// parts array (`NO_PARTS` without): only thinning meshes store parts.
    vertex: u32,
    parts: u32,
}

/// `GpuDraw::parts` of a mesh that does not thin (placed.wgsl).
const NO_PARTS: u32 = u32::MAX;

/// Draw index ranges by pipeline. Pieces are ordered lit, flat, then the
/// rest (never drawn as placements), shadow casters first in each, so every
/// pipeline's draws are contiguous.
#[derive(Clone, Default)]
pub(super) struct Ranges {
    pub lit: Range<u32>,
    pub flat: Range<u32>,
    pub lit_casters: Range<u32>,
    pub flat_casters: Range<u32>,
    /// Impostor quads, after every mesh draw; they all cast shadows.
    pub impostors: Range<u32>,
}

/// Every draw's geometry, material and albedo in shared arrays: bind group
/// 3 of placed.wgsl.
pub(super) struct Arena {
    pub group: wgpu::BindGroup,
    /// Every draw's indices, rebased onto the shared vertices.
    pub indices: wgpu::Buffer,
    /// Each draw's first index in the shared index array.
    first_index: Vec<u32>,
    /// Per atlas layer: the impostor material that bakes it (others with
    /// the same mesh share it) and its source mesh's index range.
    pub impostors: Vec<(u32, Range<u32>)>,
}

/// Baked impostor views: bind group 1 of the impostor pipelines. Holds its
/// textures through the bind group.
pub(super) struct Atlas {
    pub group: wgpu::BindGroup,
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct GpuPiece {
    lo: [f32; 4],
    hi: [f32; 4],
    levels: [f32; 4],
    draw: [u32; 4],
    /// Thinning: full, least, z 1 for a thinning mesh (THIN_BUCKETS draws
    /// a level), w 1 when on (cull.wgsl).
    thin: [f32; 4],
    /// Each level's material, the impostor's last.
    material: [u32; 4],
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
pub(super) struct CullGlobals {
    pub rows: [[f32; 4]; 4],
    pub planes: [[f32; 4]; 6],
    pub info: [f32; 4],
    pub counts: [u32; 4],
    pub clip: [[f32; 4]; 4],
    pub hzb: [f32; 4],
}

/// One indirect draw: a piece at one level, its visible rows starting at
/// `base` in the visible list.
#[derive(Clone, Copy)]
pub(super) struct PlacedDraw {
    pub piece: u32,
    /// The impostor's level is one past the piece's meshes.
    pub level: u32,
    pub base: u32,
    pub impostor: bool,
    /// Indices a thinning bucket draws; 0 for the whole mesh.
    pub count: u32,
    /// The mesh this draws, shared by a thinning level's buckets and by
    /// every piece drawing the same mesh: what visible entries carry and
    /// the arena's `GpuDraw` table is indexed by.
    pub material: u32,
}

/// The selected rows as select.wgsl reads them; see `Placed::selected`.
#[derive(Default)]
pub(super) struct Selected {
    /// Every row of the selected items.
    pub rows: Vec<u32>,
    /// One marker per item: its origin in a row's space, the row bitcast
    /// into w.
    pub marks: Vec<[f32; 4]>,
    /// The markers' rgb and shape, and radius.
    pub color: [f32; 4],
    pub radius: f32,
    /// While a drag runs, the matrix every row's start transform takes.
    pub drag: Option<Mat4>,
    /// Each row's transform when the drag began, from the host's table.
    starts: Vec<Mat4>,
    pub buffers: Option<SelectedBuffers>,
    dirty: bool,
}

pub(super) struct SelectedBuffers {
    pub rows: wgpu::Buffer,
    pub starts: wgpu::Buffer,
    pub marks: wgpu::Buffer,
}

pub(super) struct PlacedBuffers {
    pub transforms: wgpu::Buffer,
    pub piece_of: wgpu::Buffer,
    pub pieces: wgpu::Buffer,
    pub bases: wgpu::Buffer,
}

/// The placements and their GPU copies, shared by sibling renderers.
#[derive(Default)]
pub(super) struct Placed {
    pieces: Vec<Piece>,
    /// Each piece's level meshes once all are created. Their textures may
    /// still be streaming in (`unstreamed`): they exist empty from the
    /// start, so the arena can bind them and the placements draw while the
    /// texels arrive. Waiting for every texel first kept a 768 m island on
    /// per-piece draws, five times slower, for over a minute of loading.
    pub resident: Vec<Option<Vec<Arc<ResidentMesh>>>>,
    /// Pieces whose textures are still streaming, in upload order.
    pub unstreamed: Vec<u32>,
    pub draws: Vec<PlacedDraw>,
    /// Each piece's first draw; its levels follow.
    first_draw: Vec<u32>,
    /// Distinct meshes the draws use, impostors of a mesh one more each.
    /// Models repeat parts: a 117 campaign island's 66k (piece, level)s
    /// draw far fewer meshes.
    materials: u32,
    pub ranges: Ranges,
    pub arena: Option<Arena>,
    /// The arena while its buffers upload, a share of each frame.
    upload: Option<Upload>,
    /// Each piece's impostor draw, when it asks for one.
    impostor_draw: Vec<Option<u32>>,
    /// Baked impostors; until then pieces draw their meshes only.
    pub atlas: Option<Atlas>,
    /// An arena was attempted for this piece list (it fails when the
    /// textures outnumber the device's binding array).
    arena_tried: bool,
    /// Pieces upload without geometry, for the arena, which draws them all
    /// once they are in: nothing draws them piece by piece meanwhile.
    pub lightweight: bool,
    pub count: u32,
    /// Visible list length: every placement once per level of its piece.
    pub capacity: u32,
    /// Bounds of every placement, for fitting the shadow map.
    pub bounds: [Vec3; 2],
    /// The table as the host gave it, so a released device can upload it
    /// again.
    transforms: Vec<Mat4>,
    piece_of: Vec<u32>,
    /// What the GPU copy lacks: the whole table, or rows moved since.
    stale: bool,
    moved: Vec<std::ops::Range<u32>>,
    table_dirty: bool,
    pub buffers: Option<PlacedBuffers>,
    /// Bumped by every change to what placements draw; the shadow map is
    /// drawn again when it moves.
    pub version: u64,
    pub selected: Selected,
}

impl Placed {
    pub fn set_pieces(&mut self, pieces: Vec<Piece>) {
        assert!(pieces.iter().all(|p| {
            (1..=MAX_LEVELS).contains(&(p.levels.len() + usize::from(p.impostor.is_some())))
        }));
        assert!(
            (pieces.iter()).all(|p| {
                let thin = |(m, _): &(Arc<Mesh>, f32)| m.data().thinning.is_some();
                p.levels.iter().all(thin) || !p.levels.iter().any(thin)
            }),
            "a piece thins at every level or at none"
        );
        self.resident = pieces.iter().map(|_| None).collect();
        self.unstreamed.clear();
        // Per piece, each level's material, then its impostor's: one per
        // distinct mesh, as everything in a `GpuDraw` follows the mesh.
        let mut ids: HashMap<(usize, bool), u32> = HashMap::new();
        let mut id = |mesh: &Arc<Mesh>, impostor: bool| {
            let next = ids.len() as u32;
            *ids.entry((key(mesh), impostor)).or_insert(next)
        };
        let material: Vec<[u32; MAX_LEVELS]> = (pieces.iter())
            .map(|piece| {
                let mut m = [0; MAX_LEVELS];
                for (k, (mesh, _)) in piece.levels.iter().enumerate() {
                    m[k] = id(mesh, false);
                }
                if piece.impostor.is_some() {
                    m[piece.levels.len()] = id(&piece.levels[0].0, true);
                }
                m
            })
            .collect();
        self.materials = ids.len() as u32;
        // (pipeline, casts no shadow): 0 lit, 1 flat, 2 neither.
        let class = |piece: &Piece| {
            let data = piece.levels[0].0.data();
            let pipeline = match data.material {
                Material::Lit => 0,
                Material::Flat => 1,
                _ => 2,
            };
            (pipeline, !super::casts_shadow(data))
        };
        let mut order: Vec<usize> = (0..pieces.len()).collect();
        order.sort_by_key(|&p| class(&pieces[p]));
        self.first_draw = vec![0; pieces.len()];
        self.draws.clear();
        let mut ends = [[0u32; 2]; 3];
        for &p in &order {
            self.first_draw[p] = self.draws.len() as u32;
            if pieces[p].levels[0].0.data().thinning.is_some() {
                // THIN_BUCKETS prefix draws per level, each level's parts
                // ranked as the full mesh's (cull.wgsl).
                for (level, (mesh, _)) in pieces[p].levels.iter().enumerate() {
                    let counts = mesh.thin_counts().expect("asserted above");
                    self.draws.extend(counts.iter().map(|&count| PlacedDraw {
                        piece: p as u32,
                        level: level as u32,
                        base: 0,
                        impostor: false,
                        count,
                        material: material[p][level],
                    }));
                }
            } else {
                self.draws
                    .extend((0..pieces[p].levels.len()).map(|level| PlacedDraw {
                        piece: p as u32,
                        level: level as u32,
                        base: 0,
                        impostor: false,
                        count: 0,
                        material: material[p][level],
                    }));
            }
            let (pipeline, shadowless) = class(&pieces[p]);
            ends[pipeline][usize::from(shadowless)] = self.draws.len() as u32;
        }
        // Empty classes end where the one before did.
        let mut end = 0;
        for class in &mut ends {
            for e in class.iter_mut() {
                end = (*e).max(end);
                *e = end;
            }
        }
        let meshes = self.draws.len() as u32;
        self.impostor_draw = vec![None; pieces.len()];
        for (p, piece) in pieces.iter().enumerate() {
            if piece.impostor.is_some() {
                self.impostor_draw[p] = Some(self.draws.len() as u32);
                self.draws.push(PlacedDraw {
                    piece: p as u32,
                    level: piece.levels.len() as u32,
                    base: 0,
                    impostor: true,
                    count: 0,
                    material: material[p][piece.levels.len()],
                });
            }
        }
        self.ranges = Ranges {
            lit: 0..ends[0][1],
            flat: ends[0][1]..ends[1][1],
            lit_casters: 0..ends[0][0],
            flat_casters: ends[0][1]..ends[1][0],
            impostors: meshes..self.draws.len() as u32,
        };
        self.arena = None;
        self.upload = None;
        self.atlas = None;
        self.arena_tried = false;
        self.pieces = pieces;
        // Rows refer to pieces by index: a new piece list needs new rows.
        self.set_placements(&[], &[]);
    }

    pub fn set_placements(&mut self, piece_of: &[u32], transforms: &[Mat4]) {
        assert_eq!(piece_of.len(), transforms.len());
        let mut per_piece = vec![0u32; self.pieces.len()];
        for &p in piece_of {
            per_piece[p as usize] += 1;
        }
        let mut base = 0;
        for d in &mut self.draws {
            d.base = base;
            base += per_piece[d.piece as usize];
        }
        self.capacity = base;
        self.count = piece_of.len() as u32;
        self.bounds = math::bounds(piece_of.iter().zip(transforms).flat_map(|(&p, m)| {
            let mesh = &self.pieces[p as usize].levels[0].0;
            math::corners(mesh.bounds()).map(|c| math::transform(m, c))
        }));
        self.transforms = transforms.to_vec();
        self.piece_of = piece_of.to_vec();
        self.stale = true;
        self.version += 1;
        self.moved.clear();
        self.table_dirty = true;
        // Rows past the new table would write outside it; the host sends
        // the selection again for the new rows. A drag's starts come from
        // the new table, which still holds where the drag began.
        let count = self.count;
        let sel = &mut self.selected;
        if sel.rows.iter().any(|&r| r >= count) || sel.marks.iter().any(|m| m[3].to_bits() >= count)
        {
            sel.rows.clear();
            sel.marks.clear();
        }
        if sel.drag.is_some() {
            sel.starts = sel
                .rows
                .iter()
                .map(|&r| self.transforms[r as usize])
                .collect();
        }
        sel.dirty = true;
    }

    /// Replaces the selection the GPU outlines, marks and drags.
    pub fn set_selection(
        &mut self,
        rows: Vec<u32>,
        marks: Vec<[f32; 4]>,
        color: [f32; 4],
        radius: f32,
    ) {
        assert!(
            rows.iter().all(|&r| r < self.count),
            "selected rows past the table"
        );
        let sel = &mut self.selected;
        sel.rows = rows;
        sel.marks = marks;
        sel.color = color;
        sel.radius = radius;
        if sel.drag.is_some() {
            sel.starts = sel
                .rows
                .iter()
                .map(|&r| self.transforms[r as usize])
                .collect();
        }
        sel.dirty = true;
    }

    /// Starts, moves or ends a drag of the selected rows on the GPU. The
    /// host's table keeps the rows where the drag began; when it ends they
    /// go back to what the table holds, which the host has set to where
    /// they landed by then.
    pub fn drag_selection(&mut self, delta: Option<Mat4>) {
        let sel = &mut self.selected;
        if sel.drag == delta {
            return;
        }
        self.version += 1;
        match (sel.drag.is_some(), delta.is_some()) {
            (false, true) => {
                sel.starts = sel
                    .rows
                    .iter()
                    .map(|&r| self.transforms[r as usize])
                    .collect();
                sel.dirty = true;
            }
            (true, false) if !self.stale => {
                // Contiguous runs, one write each.
                let mut rows = sel.rows.clone();
                rows.sort_unstable();
                let mut run: Option<Range<u32>> = None;
                for r in rows {
                    run = match run {
                        Some(u) if u.end == r => Some(u.start..r + 1),
                        Some(u) => {
                            self.moved.push(u);
                            Some(r..r + 1)
                        }
                        None => Some(r..r + 1),
                    };
                }
                self.moved.extend(run);
            }
            _ => {}
        }
        sel.drag = delta;
    }

    pub fn move_placements(&mut self, first: u32, transforms: &[Mat4]) {
        self.version += 1;
        let end = first as usize + transforms.len();
        assert!(end <= self.count as usize, "rows past the placement table");
        self.transforms[first as usize..end].copy_from_slice(transforms);
        if !self.stale {
            self.moved.push(first..end as u32);
        }
    }

    fn piece_table(&self) -> Vec<GpuPiece> {
        self.pieces
            .iter()
            .zip(&self.resident)
            .zip(&self.first_draw)
            .zip(&self.impostor_draw)
            .map(|(((piece, resident), &first), impostor)| {
                let [lo, hi] = piece.levels[0].0.bounds();
                let mut levels = [0.; 4];
                for (k, (_, min)) in piece.levels.iter().enumerate() {
                    levels[k] = *min;
                }
                let mut n = piece.levels.len() as u32;
                let mut draw = [first, n, 0, 0];
                let mut material = [0; 4];
                let per_level = match piece.levels[0].0.data().thinning {
                    Some(_) => THIN_BUCKETS,
                    None => 1,
                };
                for (k, m) in material.iter_mut().take(n as usize).enumerate() {
                    *m = self.draws[(first + k as u32 * per_level) as usize].material;
                }
                // With baked impostors the last mesh stops at the impostor's
                // threshold and the impostor takes every size below.
                if let (Some(ppm), Some(d), Some(_)) = (piece.impostor, impostor, &self.atlas) {
                    levels[n as usize - 1] = ppm;
                    material[n as usize] = self.draws[*d as usize].material;
                    n += 1;
                    draw = [first, n, *d, 1];
                }
                // Not resident yet: culled as if it had no levels.
                if resident.is_none() {
                    draw[1] = 0;
                }
                let world = if piece.impostor.is_some() { 1. } else { 0. };
                // Thinning needs the arena's part table; without, the
                // last bucket draws whole meshes.
                let thin = match (&piece.levels[0].0.data().thinning, &self.arena) {
                    (Some(t), Some(_)) => [t.full, t.least, 1., 1.],
                    (Some(_), None) => [0., 0., 1., 0.],
                    _ => [0.; 4],
                };
                GpuPiece {
                    lo: [lo[0], lo[1], lo[2], world],
                    hi: [hi[0], hi[1], hi[2], 0.],
                    levels,
                    draw,
                    thin,
                    material,
                }
            })
            .collect()
    }

    /// The indirect arguments each frame starts from (`ARGS` words a draw):
    /// index count, no instances yet, and with an arena the draw's first
    /// index and visible base. Draws of pieces not yet resident draw
    /// nothing.
    pub fn args_template(&self) -> Vec<u32> {
        let mut out = Vec::with_capacity(self.draws.len() * ARGS as usize);
        for (i, d) in self.draws.iter().enumerate() {
            let count = match self.resident[d.piece as usize].as_ref() {
                // A quad, two triangles, once the views are baked.
                Some(_) if d.impostor => {
                    if self.atlas.is_some() {
                        6
                    } else {
                        0
                    }
                }
                Some(_) if d.count > 0 => d.count,
                Some(levels) => levels[d.level as usize].count,
                None => 0,
            };
            match &self.arena {
                Some(arena) => out.extend([count, 0, arena.first_index[i], 0, d.base]),
                None => out.extend([count, 0, 0, 0, 0]),
            }
        }
        out
    }

    /// Switches impostor levels on: the piece table and indirect arguments
    /// pick them up next frame, after the bake that made the atlas.
    pub(super) fn set_atlas(&mut self, atlas: Atlas) {
        self.atlas = Some(atlas);
        self.table_dirty = true;
        self.version += 1;
    }

    /// Visible-list entries keep the placement row in their low bits and the
    /// material above (cull.wgsl, placed.wgsl). The split follows the
    /// table: as few row bits as its rows need, the rest for materials. A
    /// fixed 17/15 split sent a 189k-row island to per-piece draws though
    /// 18/14 fits. Entries carried the draw index once, but a thinning level
    /// takes `THIN_BUCKETS` draws and models repeat meshes: a 117 campaign
    /// island's 80k rows and 67k draws needed 34 bits and drew piece by
    /// piece; its 1,659 meshes take 11. The editor's ignored
    /// `placement_entries_pack_on_every_shipped_island` checks every island.
    pub(super) fn row_bits(&self) -> u32 {
        u32::BITS - self.count.leading_zeros()
    }

    /// Whether visible entries carry materials: rows and materials fit 32
    /// bits together. Larger tables store bare rows and draw each (piece,
    /// level) on its own, several times slower.
    pub fn packs(&self) -> bool {
        self.row_bits() < 32 && self.counts().bits() <= 32
    }

    pub fn counts(&self) -> super::PlacementCounts {
        super::PlacementCounts {
            rows: self.count,
            materials: self.materials,
            draws: self.draws.len() as u32,
        }
    }

    /// The mask that takes the row out of a visible entry.
    pub fn row_mask(&self) -> u32 {
        if self.packs() {
            (1 << self.row_bits()) - 1
        } else {
            u32::MAX
        }
    }

    /// Whether piece meshes or placement uploads are still to come.
    pub fn pending(&self) -> bool {
        self.resident.iter().any(Option::is_none)
            || !self.unstreamed.is_empty()
            || self.stale
            || !self.moved.is_empty()
    }

    /// Drops the GPU copies, for a new device.
    pub fn release(&mut self) {
        self.buffers = None;
        self.arena = None;
        self.upload = None;
        self.atlas = None;
        self.arena_tried = false;
        // The host's table stays: the next device uploads it again.
        self.resident = self.pieces.iter().map(|_| None).collect();
        self.unstreamed.clear();
        self.stale = !self.transforms.is_empty();
        self.moved.clear();
        self.table_dirty = true;
        self.selected.buffers = None;
        self.selected.dirty = true;
    }
}

impl Assets {
    /// Uploads piece meshes within the budget and placement changes since
    /// the last frame.
    pub(super) fn sync_placed(&mut self, gpu: &Gpu, kit: &Kit, budget: &mut Budget) {
        // Whether pieces can skip their own geometry: an arena is possible.
        // Decided while nothing is resident; a table that outgrows the
        // packed bits uploads its pieces whole again.
        let placed = &mut self.placed;
        let arena = kit.arena.is_some() && self.packed != Some(false) && placed.packs();
        if placed.lightweight && !arena {
            placed.lightweight = false;
            placed.arena = None;
            placed.atlas = None;
            placed.resident.iter_mut().for_each(|r| *r = None);
            placed.unstreamed.clear();
            placed.table_dirty = true;
            placed.version += 1;
        } else if !placed.lightweight
            && arena
            && !placed.arena_tried
            && placed.resident.iter().all(Option::is_none)
        {
            placed.lightweight = true;
        }
        // Meshes the arena will draw keep no geometry of their own, so
        // making them is cheap: every piece at once, unless each carries
        // its geometry.
        let geometry = !self.placed.lightweight;
        let span = crate::profile::span("gpu sync: placed meshes");
        for p in 0..self.placed.pieces.len() {
            if self.placed.resident[p].is_some() {
                continue;
            }
            // Lightweight pieces upload no bytes, but creating 542 models'
            // meshes and textures took a 175 ms frame: they count time.
            if budget.spent() {
                break;
            }
            let meshes: Vec<Arc<Mesh>> = (self.placed.pieces[p].levels.iter())
                .map(|(m, _)| m.clone())
                .collect();
            let resident = (meshes.iter())
                .map(|mesh| self.mesh_with(gpu, kit, mesh, budget, geometry))
                .collect();
            self.placed.resident[p] = Some(resident);
            budget.charge(0);
            self.placed.unstreamed.push(p as u32);
            self.placed.table_dirty = true;
            self.placed.version += 1;
        }
        drop(span);
        // Then their texels, within the budget.
        let span = crate::profile::span("gpu sync: placed texels");
        let mut unstreamed = std::mem::take(&mut self.placed.unstreamed);
        unstreamed.retain(|&p| {
            if budget.spent() {
                return true;
            }
            let levels = self.placed.resident[p as usize].clone().unwrap_or_default();
            // Every level streams, even after one is unfinished.
            let mut done = true;
            for r in &levels {
                done &= self.stream(gpu, r, budget);
            }
            if done {
                // Shadows and impostors follow the finished texels.
                self.placed.version += 1;
            }
            !done
        });
        self.placed.unstreamed = unstreamed;
        drop(span);
        let placed = &mut self.placed;
        if !placed.arena_tried
            && kit.arena.is_some()
            && !placed.packs()
            && !placed.pieces.is_empty()
            && placed.resident.iter().all(Option::is_some)
        {
            placed.arena_tried = true;
            eprintln!(
                "{} placements of {} materials exceed the 32 bits a visible entry packs; drawing pieces one by one",
                placed.count, placed.materials,
            );
        }
        if !placed.arena_tried
            && self.packed != Some(false)
            && placed.packs()
            && !placed.pieces.is_empty()
            && placed.resident.iter().all(Option::is_some)
            && let Some((_, cap)) = &kit.arena
        {
            placed.arena_tried = true;
            let _span = crate::profile::span("gpu sync: arena pack");
            placed.upload = Arena::pack(gpu, kit, *cap, placed);
            if placed.upload.is_none() && placed.lightweight {
                // Drawn piece by piece after all: upload them whole.
                placed.lightweight = false;
                placed.resident.iter_mut().for_each(|r| *r = None);
                placed.unstreamed.clear();
                placed.table_dirty = true;
            }
        }
        if let Some(upload) = &mut placed.upload {
            let _span = crate::profile::span("gpu sync: arena upload");
            if upload.step(gpu, budget)
                && let Some((layout, _)) = &kit.arena
            {
                placed.arena = placed.upload.take().map(|u| u.finish(gpu, layout));
                // The shadow map follows what placements draw with, and the
                // piece table turns thinning on with the arena.
                placed.version += 1;
                placed.table_dirty = true;
            }
        }
        if std::mem::take(&mut placed.stale) {
            placed.moved.clear();
            let (transforms, piece_of) = (&placed.transforms, &placed.piece_of);
            let bases: Vec<u32> = placed.draws.iter().map(|d| d.base).collect();
            budget.charge(transforms.len() * 68);
            let usage = wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST;
            let buffer = |label, bytes: &[u8]| buffer_init(gpu, label, bytes, usage);
            placed.buffers = Some(PlacedBuffers {
                transforms: buffer("placed transforms", bytemuck::cast_slice(transforms)),
                piece_of: buffer("placed pieces of", bytemuck::cast_slice(piece_of)),
                pieces: buffer("placed pieces", bytemuck::cast_slice(&placed.piece_table())),
                bases: buffer("placed bases", bytemuck::cast_slice(&bases)),
            });
            placed.table_dirty = false;
        }
        let Some(buffers) = &placed.buffers else {
            return;
        };
        for rows in placed.moved.drain(..) {
            gpu.queue.write_buffer(
                &buffers.transforms,
                u64::from(rows.start) * 64,
                bytemuck::cast_slice(&placed.transforms[rows.start as usize..rows.end as usize]),
            );
        }
        if placed.table_dirty && !placed.pieces.is_empty() {
            let table = placed.piece_table();
            gpu.queue
                .write_buffer(&buffers.pieces, 0, bytemuck::cast_slice(&table));
            placed.table_dirty = false;
        }
        let sel = &mut placed.selected;
        if std::mem::take(&mut sel.dirty) {
            let usage = wgpu::BufferUsages::STORAGE;
            sel.buffers = Some(SelectedBuffers {
                rows: buffer_init(gpu, "selected rows", bytemuck::cast_slice(&sel.rows), usage),
                // One matrix at least: a storage binding cannot be empty.
                starts: buffer_init(
                    gpu,
                    "selected starts",
                    bytemuck::cast_slice(if sel.starts.is_empty() {
                        &[math::IDENTITY][..]
                    } else {
                        &sel.starts
                    }),
                    usage,
                ),
                marks: buffer_init(
                    gpu,
                    "selected marks",
                    bytemuck::cast_slice(&sel.marks),
                    usage,
                ),
            });
        }
    }
}

/// The arena's vertices, indices (rebased onto the shared vertices) and
/// thinning parts for `spans` (mesh, first index, first vertex, first part
/// or `NO_PARTS`, in increasing order), with the impostor quad's 0..6 at
/// `quad`. Each mesh fills its own slices, on all cores.
fn pack(
    spans: &[(Arc<Mesh>, u32, u32, u32)],
    vertex_count: u32,
    index_count: u32,
    part_count: u32,
    quad: Option<u32>,
) -> (Vec<super::GpuVertex>, Vec<u32>, Vec<[f32; 4]>) {
    use bytemuck::allocation::zeroed_vec;
    let mut vertices: Vec<super::GpuVertex> = zeroed_vec(vertex_count as usize);
    let mut indices: Vec<u32> = zeroed_vec(index_count as usize);
    // A storage binding cannot be empty.
    let mut parts: Vec<[f32; 4]> = zeroed_vec(part_count.max(1) as usize);
    if let Some(q) = quad {
        indices[q as usize..q as usize + 6].copy_from_slice(&[0, 1, 2, 3, 4, 5]);
    }
    // Split every array into each span's slices; spans run in order.
    let (mut v, mut i, mut p) = (&mut vertices[..], &mut indices[..], &mut parts[..]);
    let (mut v_at, mut i_at, mut p_at) = (0usize, 0usize, 0usize);
    let mut jobs = Vec::with_capacity(spans.len());
    // `len` items from `start` off the front of `rest`, which begins at `at`.
    fn take<'a, T>(
        rest: &mut &'a mut [T],
        at: &mut usize,
        start: usize,
        len: usize,
    ) -> &'a mut [T] {
        let (_, tail) = std::mem::take(rest).split_at_mut(start - *at);
        let (mine, tail) = tail.split_at_mut(len);
        *rest = tail;
        *at = start + len;
        mine
    }
    for (mesh, first, vertex, part) in spans {
        let (first, vertex, part) = (*first, *vertex, *part);
        let n = mesh.vertex_count();
        let mine_v = take(&mut v, &mut v_at, vertex as usize, n);
        let mine_i = take(&mut i, &mut i_at, first as usize, mesh.index_count());
        let mine_p = (part != NO_PARTS).then(|| take(&mut p, &mut p_at, part as usize, n));
        jobs.push((&**mesh, vertex, mine_v, mine_i, mine_p));
    }
    // Contiguous groups of about equal vertex counts, one per core.
    let threads = std::thread::available_parallelism().map_or(4, |n| n.get());
    let per = (vertex_count as usize).div_ceil(threads).max(1);
    let mut groups: Vec<Vec<_>> = vec![Vec::new()];
    let mut filled = 0;
    for job in jobs {
        if filled >= per {
            groups.push(Vec::new());
            filled = 0;
        }
        filled += job.2.len();
        groups.last_mut().expect("one group").push(job);
    }
    std::thread::scope(|scope| {
        for group in groups {
            scope.spawn(move || {
                for (mesh, base, v, i, p) in group {
                    // Compact meshes decode here, off the frame.
                    mesh.write_vertices(v);
                    mesh.write_indices(i, base);
                    if let (Some(p), Some(t)) = (p, &mesh.data().thinning) {
                        p.copy_from_slice(&t.parts);
                    }
                }
            });
        }
    });
    (vertices, indices, parts)
}

impl Arena {
    /// Packs every draw's mesh, sharing meshes several draws use, for
    /// `Upload` to send. `None` when the textures do not fit `cap` slots.
    fn pack(gpu: &Gpu, kit: &Kit, cap: u32, placed: &Placed) -> Option<Upload> {
        // Slot 0 is white, for meshes without a texture.
        let mut views: Vec<&wgpu::TextureView> = vec![&kit.dummy.white];
        let mut slot_of: HashMap<usize, u32> = HashMap::new();
        // Each distinct mesh's first index, first vertex and first part.
        let mut packed: HashMap<usize, (u32, u32, u32)> = HashMap::new();
        // Distinct meshes in packing order, then sized arrays filled in
        // parallel: packing 9M vertices on one core, growing the arrays as
        // it went, took a 330 ms frame on the continental island.
        let mut spans: Vec<(Arc<Mesh>, u32, u32, u32)> = Vec::new();
        let (mut vertex_count, mut index_count, mut part_count) = (0u32, 0u32, 0u32);
        // Per material, as visible entries index them: a thinning level's
        // buckets fill the same one.
        let mut draws = vec![GpuDraw::zeroed(); placed.materials as usize];
        let mut first_index = Vec::with_capacity(placed.draws.len());
        // Per material: centre and radius of the impostor's quad (zero for
        // meshes).
        let mut shapes: Vec<[f32; 4]> = vec![[0.; 4]; placed.materials as usize];
        let mut impostors = Vec::new();
        // Pieces sharing a mesh share its baked layer.
        let mut layer_of: HashMap<usize, u32> = HashMap::new();
        let mut mapped: Vec<(usize, &ResidentMesh)> = Vec::new();
        let mut quad: Option<u32> = None;
        // Materials filled: thinning buckets and pieces repeating a mesh
        // come back to them.
        let mut filled = vec![0u64; (placed.materials as usize).div_ceil(64)];
        for d in &placed.draws {
            let levels = placed.resident[d.piece as usize].as_ref()?;
            let m = d.material as usize;
            if d.impostor {
                let source = placed.first_draw[d.piece as usize];
                let mesh = &levels[0];
                let [lo, hi] = mesh.source.bounds();
                let centre: Vec3 = std::array::from_fn(|a| (lo[a] + hi[a]) * 0.5);
                let half: Vec3 = std::array::from_fn(|a| (hi[a] - lo[a]) * 0.5);
                let radius = math::length(half);
                // Mesh draws come first, so the source's range exists.
                let start = first_index[source as usize];
                let layer = *layer_of.entry(key(&mesh.source)).or_insert_with(|| {
                    impostors.push((d.material, start..start + mesh.count));
                    impostors.len() as u32 - 1
                });
                draws[m] = GpuDraw {
                    albedo: layer,
                    flags: 4,
                    gain: mesh.source.data().color_gain,
                    source: placed.draws[source as usize].material,
                    maps: 0,
                    tint: 0.,
                    vertex: 0,
                    parts: NO_PARTS,
                };
                first_index.push(0);
                shapes[m] = [centre[0], centre[1], centre[2], radius.max(1e-3)];
                // Impostor quads draw indexed too: one shared 0..6, so the
                // vertex index is the corner.
                let quad = *quad.get_or_insert_with(|| {
                    index_count += 6;
                    index_count - 6
                });
                *first_index.last_mut().expect("pushed above") = quad;
                continue;
            }
            let mesh = &levels[d.level as usize];
            let data = mesh.source.data();
            let albedo = mesh.albedo.as_ref().map_or(0, |t| {
                *slot_of.entry(key(t)).or_insert_with(|| {
                    views.push(&t.view);
                    views.len() as u32 - 1
                })
            });
            let (first, vertex, parts) = *packed.entry(key(&mesh.source)).or_insert_with(|| {
                let at = (index_count, vertex_count, part_count);
                let n = mesh.source.vertex_count() as u32;
                vertex_count += n;
                index_count += mesh.source.index_count() as u32;
                let parts = match data.thinning {
                    Some(_) => {
                        part_count += n;
                        at.2
                    }
                    None => NO_PARTS,
                };
                spans.push((mesh.source.clone(), at.0, at.1, parts));
                (at.0, at.1, parts)
            });
            first_index.push(first);
            if filled[m / 64] & 1 << (m % 64) != 0 {
                continue;
            }
            filled[m / 64] |= 1 << (m % 64);
            if mesh.maps.iter().any(Option::is_some) {
                mapped.push((m, &**mesh));
            }
            draws[m] = GpuDraw {
                albedo,
                flags: u32::from(mesh.source.has_normals()) | u32::from(data.ignore_alpha) << 1,
                gain: data.color_gain,
                source: 0,
                maps: 0,
                tint: data.terrain_tint,
                vertex,
                parts,
            };
        }
        // Filled on a thread: 9M vertices took a 56 ms frame on all cores.
        let packing =
            std::thread::spawn(move || pack(&spans, vertex_count, index_count, part_count, quad));
        // Normal and surface maps take the slots albedo leaves: without
        // them pieces still draw in one go, only flatter.
        let extra: HashSet<usize> = (mapped.iter())
            .flat_map(|(_, m)| m.maps.iter().flatten().map(key))
            .filter(|k| !slot_of.contains_key(k))
            .collect();
        if views.len() + extra.len() <= cap as usize {
            for &(d, mesh) in &mapped {
                let mut slots = [0; 2];
                for (slot, t) in slots.iter_mut().zip(&mesh.maps) {
                    let Some(t) = t else { continue };
                    *slot = *slot_of.entry(key(t)).or_insert_with(|| {
                        views.push(&t.view);
                        views.len() as u32 - 1
                    });
                }
                draws[d].maps = slots[0] | slots[1] << 16;
            }
        } else if !mapped.is_empty() {
            eprintln!(
                "{} placement textures with normal and surface maps exceed the {cap} the device binds; drawing without them",
                views.len() + extra.len()
            );
        }
        if views.len() > cap as usize {
            eprintln!(
                "{} placement textures exceed the {cap} the device binds; drawing pieces one by one",
                views.len()
            );
            return None;
        }
        views.resize(cap as usize, &kit.dummy.white);
        // Indices are an index buffer for drawing and storage for baking.
        let usage =
            wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::INDEX | wgpu::BufferUsages::COPY_DST;
        let sizes = [
            std::mem::size_of_val(draws.as_slice()),
            vertex_count as usize * std::mem::size_of::<super::GpuVertex>(),
            index_count as usize * 4,
            std::mem::size_of_val(shapes.as_slice()),
            part_count.max(1) as usize * 16,
        ];
        let buffers = std::array::from_fn(|k| {
            gpu.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some(UploadData::LABELS[k]),
                // Bindings want at least 16 bytes.
                size: (sizes[k] as u64).max(16),
                usage,
                mapped_at_creation: false,
            })
        });
        Some(Upload {
            buffers,
            data: UploadData {
                draws,
                vertices: Vec::new(),
                indices: Vec::new(),
                shapes,
                parts: Vec::new(),
            },
            packing: Some(packing),
            written: [0; 5],
            views: views.into_iter().cloned().collect(),
            first_index,
            impostors,
        })
    }
}

/// What `Arena::pack` laid out, as it uploads.
struct UploadData {
    draws: Vec<GpuDraw>,
    vertices: Vec<super::GpuVertex>,
    indices: Vec<u32>,
    shapes: Vec<[f32; 4]>,
    parts: Vec<[f32; 4]>,
}

impl UploadData {
    const LABELS: [&str; 5] = [
        "arena draws",
        "arena vertices",
        "arena indices",
        "arena shapes",
        "arena parts",
    ];

    fn bytes(&self) -> [&[u8]; 5] {
        [
            bytemuck::cast_slice(&self.draws),
            bytemuck::cast_slice(&self.vertices),
            bytemuck::cast_slice(&self.indices),
            bytemuck::cast_slice(&self.shapes),
            bytemuck::cast_slice(&self.parts),
        ]
    }
}

/// Vertices, indices and parts as `pack` fills them.
type Packed = (Vec<super::GpuVertex>, Vec<u32>, Vec<[f32; 4]>);

/// An arena packed on the CPU, its buffers filling a share of each frame:
/// the continental island's 390 MB in one go took a 100 ms frame. Nothing
/// draws from it until every byte is in.
pub(super) struct Upload {
    buffers: [wgpu::Buffer; 5],
    data: UploadData,
    /// The geometry still filling on its thread.
    packing: Option<std::thread::JoinHandle<Packed>>,
    written: [usize; 5],
    views: Vec<wgpu::TextureView>,
    first_index: Vec<u32>,
    impostors: Vec<(u32, Range<u32>)>,
}

/// Arena bytes a frame writes at least, past the frame's upload budget:
/// at its 8 MB the arena would take 50 frames, at this about a dozen of a
/// few milliseconds each.
const UPLOAD_CHUNK: usize = 32 * 1024 * 1024;

impl Upload {
    /// Writes what this frame allows; true once every buffer is full.
    fn step(&mut self, gpu: &Gpu, budget: &mut Budget) -> bool {
        let mut room = budget.room().max(UPLOAD_CHUNK);
        if let Some(packing) = self.packing.take_if(|p| p.is_finished()) {
            let (vertices, indices, parts) = packing.join().expect("arena packing");
            (self.data.vertices, self.data.indices, self.data.parts) = (vertices, indices, parts);
        }
        if self.packing.is_some() {
            return false;
        }
        for ((buffer, bytes), written) in self
            .buffers
            .iter()
            .zip(self.data.bytes())
            .zip(&mut self.written)
        {
            while *written < bytes.len() && room > 0 {
                // Write offsets and sizes keep wgpu's 4-byte alignment.
                let n = (bytes.len() - *written).min(room) & !3;
                if n == 0 {
                    return false;
                }
                gpu.queue
                    .write_buffer(buffer, *written as u64, &bytes[*written..*written + n]);
                *written += n;
                room -= n;
                budget.charge(n);
            }
        }
        (self.written.iter().zip(self.data.bytes())).all(|(w, b)| *w == b.len())
    }

    fn finish(self, gpu: &Gpu, layout: &wgpu::BindGroupLayout) -> Arena {
        let views: Vec<&wgpu::TextureView> = self.views.iter().collect();
        let b = &self.buffers;
        let group = gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("placement arena"),
            layout,
            entries: &[
                entry(0, b[0].as_entire_binding()),
                entry(1, b[1].as_entire_binding()),
                entry(2, b[2].as_entire_binding()),
                entry(3, wgpu::BindingResource::TextureViewArray(&views)),
                entry(4, b[3].as_entire_binding()),
                entry(5, b[4].as_entire_binding()),
            ],
        });
        let [_, _, indices, _, _] = self.buffers;
        Arena {
            group,
            indices,
            first_index: self.first_index,
            impostors: self.impostors,
        }
    }
}

/// One viewport's cull outputs: indirect arguments and visible rows for the
/// view's two culling phases and for the shadow map, and which placements
/// the view showed last frame.
pub(super) struct CullTargets {
    pub capacity: u32,
    pub draws: u32,
    pub args: wgpu::Buffer,
    pub visible: wgpu::Buffer,
    /// Placements the second phase found newly revealed (cull.wgsl).
    pub revealed_args: wgpu::Buffer,
    pub revealed_visible: wgpu::Buffer,
    pub shadow_args: wgpu::Buffer,
    pub shadow_visible: wgpu::Buffer,
    /// Per row, the share of a thinning mesh's parts it keeps, for the view
    /// and the shadow map (cull.wgsl writes, placed.wgsl reads).
    pub keep: wgpu::Buffer,
    pub shadow_keep: wgpu::Buffer,
    /// A bit per placement row: visible last frame.
    pub seen: wgpu::Buffer,
    /// What the view's phases drew, for `Drawn`: placements per level, then
    /// their triangles (cull.wgsl). 20 bytes read back instead of every
    /// draw's arguments.
    pub totals: wgpu::Buffer,
    /// `Placed::args_template` as of `template_version`, copied over the
    /// arguments each frame.
    pub template: wgpu::Buffer,
    pub template_version: Option<u64>,
}

impl CullTargets {
    pub fn new(gpu: &Gpu, capacity: u32, draws: u32) -> Self {
        use wgpu::BufferUsages as U;
        let buffer = |label, size: u64, usage| {
            gpu.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some(label),
                size: size.max(16),
                usage,
                mapped_at_creation: false,
            })
        };
        let args = u64::from(draws) * u64::from(ARGS) * 4;
        let rows = u64::from(capacity) * 4;
        let args_usage = U::INDIRECT | U::STORAGE | U::COPY_DST | U::COPY_SRC;
        Self {
            capacity,
            draws,
            args: buffer("editor cull args", args, args_usage),
            visible: buffer("editor cull visible", rows, U::STORAGE),
            revealed_args: buffer("editor revealed args", args, args_usage),
            revealed_visible: buffer("editor revealed visible", rows, U::STORAGE),
            shadow_args: buffer("editor shadow args", args, args_usage),
            shadow_visible: buffer("editor shadow visible", rows, U::STORAGE),
            // Rows never exceed the visible capacity.
            keep: buffer("editor thinning", rows, U::STORAGE),
            shadow_keep: buffer("editor shadow thinning", rows, U::STORAGE),
            // Rows never exceed the visible capacity; zero is "unseen".
            seen: buffer(
                "editor seen",
                u64::from(capacity.div_ceil(32)) * 4,
                U::STORAGE,
            ),
            template: buffer("editor cull template", args, U::COPY_SRC | U::COPY_DST),
            template_version: None,
            totals: buffer(
                "editor cull totals",
                32,
                U::STORAGE | U::COPY_SRC | U::COPY_DST,
            ),
        }
    }
}
