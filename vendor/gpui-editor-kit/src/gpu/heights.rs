// Terrain heights as an R32Float texture that meshes displace from, their
// slopes per sample that ground fragments shade from, their ranges per
// cell for the cull, and the rectangle edits that keep every device copy in
// step with the host's grid.
use super::LockAny as _;
use crate::heightfield::{Grid, Heightfield, Rect};
use anyhow::{Result, ensure};
use std::collections::VecDeque;
use std::sync::Mutex;

// Vulkan guarantees 4096 texels per 2D side; desktop drivers give 16384.
// Island grids are around 1025 samples, so stay inside the guarantee.
const MAX_SIDE: usize = 4096;
// Edits a device copy can fall behind by before it re-uploads everything.
// One frame of a fast drag queues a handful.
const LOG: usize = 64;
// More pending rects than this upload as their union, one copy.
const MERGE: usize = 8;
// Samples across a finest cell of the ranges the cull reads (`HeightMap::
// levels`): 4 m on a 0.5 m grid, about the smallest rows that bend. 1025²
// samples keep 22k cells, 177 KB.
pub(super) const CELL: usize = 8;

/// Heights a displaced mesh samples on the GPU, one texel per heightfield
/// sample. Share it through `Arc` in `MeshData::heights`. `update` copies a
/// dirty rectangle in; every device copy picks it up on its next frame, with
/// no new scene and no mesh rebuild.
///
/// Keeps a CPU copy of the heights (4 bytes per sample, 4 MB for 1025²) so a
/// device that first sees the map later, or fell behind, uploads from it.
pub struct HeightMap {
    grid: Grid,
    /// Per level of `State::cells`, its first cell and its cells across x
    /// and z: the finest has a cell per `CELL` samples each way, each
    /// coarser one per two by two of the one before, down to one.
    levels: Vec<(usize, [usize; 2])>,
    state: Mutex<State>,
}

struct State {
    heights: Vec<f32>,
    /// Lowest and highest height ever stored. Edits only widen it, so
    /// culling and shadow fitting stay conservative after a dig.
    range: [f32; 2],
    /// Lowest and highest height per cell of `HeightMap::levels`. A cell
    /// takes the samples on both its edges, so it bounds the bilinear
    /// ground anywhere over it, which placements that bend to the terrain
    /// sit on (cull.wgsl `ground_range`).
    cells: Vec<[f32; 2]>,
    /// Bumped by every edit; device copies remember the last one they have.
    serial: u64,
    log: VecDeque<(u64, Rect)>,
}

impl std::fmt::Debug for HeightMap {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HeightMap")
            .field("grid", &self.grid)
            .finish_non_exhaustive()
    }
}

impl HeightMap {
    pub fn new(field: &Heightfield) -> Result<Self> {
        let grid = field.grid;
        ensure!(
            (1..=MAX_SIDE).contains(&grid.width) && (1..=MAX_SIDE).contains(&grid.depth),
            "Height map is {}x{}; sides must be 1..={MAX_SIDE}",
            grid.width,
            grid.depth
        );
        ensure!(
            grid.spacing.is_finite()
                && grid.spacing > 0.
                && grid.origin.iter().all(|v| v.is_finite()),
            "Height map needs a positive spacing and a finite origin"
        );
        ensure!(
            field.heights.len() == grid.len(),
            "Height map has {} heights for {} samples",
            field.heights.len(),
            grid.len()
        );
        ensure!(
            field.heights.iter().all(|h| h.is_finite()),
            "Height map has a non-finite height"
        );
        let mut levels = Vec::new();
        let mut size = [(grid.width - 1) / CELL + 1, (grid.depth - 1) / CELL + 1];
        let mut first = 0;
        loop {
            levels.push((first, size));
            first += size[0] * size[1];
            if size == [1, 1] {
                break;
            }
            size = size.map(|n| (n - 1) / 2 + 1);
        }
        let map = Self {
            grid,
            levels,
            state: Mutex::new(State {
                heights: field.heights.clone(),
                range: field.range(),
                cells: vec![[0.; 2]; first],
                serial: 0,
                log: VecDeque::new(),
            }),
        };
        let mut state = map.state.lock_any();
        map.update_cells(&mut state, map.whole());
        drop(state);
        Ok(map)
    }

    /// Cells across x and z of the finest level, and how many levels.
    pub(super) fn cell_levels(&self) -> ([usize; 2], usize) {
        (self.levels[0].1, self.levels.len())
    }

    /// Per level, the cells whose samples meet `rect`'s: columns and rows.
    fn cells_touching(&self, rect: Rect) -> impl Iterator<Item = [std::ops::Range<usize>; 2]> + '_ {
        // Sample s lies in cell s / CELL and, on its edge, in the one before.
        let mut x = rect.x0.saturating_sub(1) / CELL..(rect.x1.max(1) - 1) / CELL + 1;
        let mut z = rect.z0.saturating_sub(1) / CELL..(rect.z1.max(1) - 1) / CELL + 1;
        self.levels
            .iter()
            .enumerate()
            .map(move |(l, &(_, [nx, nz]))| {
                if l > 0 {
                    x = x.start / 2..(x.end - 1) / 2 + 1;
                    z = z.start / 2..(z.end - 1) / 2 + 1;
                }
                [
                    x.start.min(nx)..x.end.min(nx),
                    z.start.min(nz)..z.end.min(nz),
                ]
            })
    }

    /// Brings the cells over `rect`'s samples up to the heights, finest
    /// level first.
    fn update_cells(&self, state: &mut State, rect: Rect) {
        let g = self.grid;
        for (l, [xs, zs]) in self.cells_touching(rect).enumerate() {
            let (first, [nx, _]) = self.levels[l];
            for cz in zs {
                for cx in xs.clone() {
                    let mut range = [f32::INFINITY, f32::NEG_INFINITY];
                    let mut take = |r: [f32; 2]| range = [range[0].min(r[0]), range[1].max(r[1])];
                    if l == 0 {
                        for z in cz * CELL..=((cz + 1) * CELL).min(g.depth - 1) {
                            let row = g.index(0, z);
                            let end = ((cx + 1) * CELL).min(g.width - 1);
                            for &h in &state.heights[row + cx * CELL..=row + end] {
                                take([h, h]);
                            }
                        }
                    } else {
                        let (below, [bx, bz]) = self.levels[l - 1];
                        for z in 2 * cz..(2 * cz + 2).min(bz) {
                            for x in 2 * cx..(2 * cx + 2).min(bx) {
                                take(state.cells[below + z * bx + x]);
                            }
                        }
                    }
                    state.cells[first + cz * nx + cx] = range;
                }
            }
        }
    }

    /// Writes the cells over `rect`'s samples into `buffer`, whole rows of
    /// each level.
    fn upload_cells(&self, queue: &wgpu::Queue, buffer: &wgpu::Buffer, state: &State, rect: Rect) {
        for (l, [_, zs]) in self.cells_touching(rect).enumerate() {
            let (first, [nx, _]) = self.levels[l];
            let cells = first + zs.start * nx..first + zs.end * nx;
            if cells.is_empty() {
                continue;
            }
            let bytes = bytemuck::cast_slice(&state.cells[cells.clone()]);
            queue.write_buffer(buffer, (cells.start * 8) as u64, bytes);
        }
    }

    pub fn grid(&self) -> Grid {
        self.grid
    }

    /// Copies `rect` (clipped to the grid) out of `heights`, the whole grid
    /// as `Heightfield::heights` holds it. Pass the `Rect` a sculpt dab
    /// returned. Repaint the viewport afterwards; the change shows on its
    /// next frame. Non-finite heights are stored as 0.
    pub fn update(&self, rect: Rect, heights: &[f32]) {
        assert_eq!(heights.len(), self.grid.len(), "one height per grid sample");
        let rect = Rect {
            x0: rect.x0,
            z0: rect.z0,
            x1: rect.x1.min(self.grid.width),
            z1: rect.z1.min(self.grid.depth),
        };
        if rect.is_empty() {
            return;
        }
        let mut state = self.state.lock_any();
        for z in rect.z0..rect.z1 {
            let row = self.grid.index(rect.x0, z)..self.grid.index(rect.x1, z);
            for (to, &from) in state.heights[row.clone()].iter_mut().zip(&heights[row]) {
                *to = if from.is_finite() { from } else { 0. };
            }
        }
        let [mut lo, mut hi] = state.range;
        for z in rect.z0..rect.z1 {
            let row = self.grid.index(rect.x0, z)..self.grid.index(rect.x1, z);
            for &h in &state.heights[row] {
                lo = lo.min(h);
                hi = hi.max(h);
            }
        }
        state.range = [lo, hi];
        self.update_cells(&mut state, rect);
        state.serial += 1;
        let serial = state.serial;
        record(&mut state.log, serial, rect);
    }

    /// Lowest and highest height per square of `cell` metres from the
    /// grid's origin, and its samples' mean normal Y (central differences,
    /// as vs_mesh lights them) taken no lower than `floor`, `side` squares
    /// a row in `out`, for the squares touching the samples of `rect`. A
    /// square reads the samples on and within its edges.
    pub(super) fn cell_ranges(
        &self,
        cell: f32,
        side: usize,
        out: &mut [[f32; 3]],
        rect: Rect,
        floor: f32,
    ) {
        let g = self.grid;
        let state = self.state.lock_any();
        let per = cell / g.spacing;
        let squares = |a: usize, b: usize| {
            let lo = ((a as f32 / per).floor() as usize).saturating_sub(1);
            let hi = ((b as f32 / per).ceil() as usize).min(side);
            lo..hi
        };
        for cz in squares(rect.z0, rect.z1) {
            for cx in squares(rect.x0, rect.x1) {
                let samples = |c: usize, n: usize| {
                    let a = ((c as f32 * per).floor() as usize).min(n - 1);
                    let b = (((c + 1) as f32 * per).ceil() as usize).min(n - 1);
                    a..=b
                };
                let mut range = [f32::INFINITY, f32::NEG_INFINITY, 0.];
                let mut count = 0.;
                let h = |x: usize, z: usize| {
                    state.heights[g.index(x.min(g.width - 1), z.min(g.depth - 1))]
                };
                for z in samples(cz, g.depth) {
                    for x in samples(cx, g.width) {
                        let at = h(x, z);
                        let dx = (h(x + 1, z) - h(x.saturating_sub(1), z)) / (2. * g.spacing);
                        let dz = (h(x, z + 1) - h(x, z.saturating_sub(1))) / (2. * g.spacing);
                        let ny = 1. / (1. + dx * dx + dz * dz).sqrt();
                        range = [range[0].min(at), range[1].max(at), range[2] + ny.max(floor)];
                        count += 1.;
                    }
                }
                range[2] /= count;
                out[cz * side + cx] = range;
            }
        }
    }

    /// World Y the heights have spanned since creation.
    pub(super) fn range(&self) -> [f32; 2] {
        self.state.lock_any().range
    }

    /// The edit serial, and the samples edited since `since` (the whole
    /// grid when the log no longer reaches back).
    pub(super) fn edits_since(&self, since: Option<u64>) -> (u64, Vec<Rect>) {
        let state = self.state.lock_any();
        if since == Some(state.serial) {
            return (state.serial, Vec::new());
        }
        (state.serial, pending(&state.log, since, self.whole()))
    }

    /// The share of `rect`'s samples (clipped to the grid) that face up
    /// less than `floor`, 0 to 255, rounded up so any is at least 1: their
    /// normal from central differences clamped at the grid's edge, as
    /// gpu.wgsl terrain_detail takes them. Between samples terrain_detail's
    /// slope is a bilinear blend of theirs, so no steeper than the
    /// steepest: at 0 the ground over `rect`'s cells faces up as much.
    /// `None` once the heights have moved past edit `serial`.
    pub(super) fn steep_share(&self, rect: Rect, floor: f32, serial: u64) -> Option<u8> {
        let g = self.grid;
        let state = self.state.lock_any();
        if state.serial != serial {
            return None;
        }
        let h = |x: usize, z: usize| state.heights[g.index(x.min(g.width - 1), z.min(g.depth - 1))];
        // n.y >= floor where |slope|² <= 1 / floor² - 1.
        let most = (1. / (floor * floor) - 1.) * (2. * g.spacing).powi(2);
        let (xs, zs) = (rect.x0..rect.x1.min(g.width), rect.z0..rect.z1.min(g.depth));
        let mut steep = 0;
        for z in zs.clone() {
            for x in xs.clone() {
                let dx = h(x + 1, z) - h(x.saturating_sub(1), z);
                let dz = h(x, z + 1) - h(x, z.saturating_sub(1));
                steep += usize::from(dx * dx + dz * dz > most);
            }
        }
        let all = (xs.len() * zs.len()).max(1);
        Some((steep * 255).div_ceil(all) as u8)
    }

    /// How far, in world Y, a patch of `tile` cells from sample (`x0`, `z0`)
    /// strays from the heights when drawn at each of `strides` (samples a
    /// cell): the largest gap at any sample between its height and the
    /// coarse cell's triangle over it. Cells split along the diagonal from
    /// their low corner as Terrain::patch's do. Past the grid's edge
    /// samples clamp, as gpu.wgsl height() does. `None` once the heights
    /// have moved past edit `serial`.
    pub(super) fn patch_errors<const N: usize>(
        &self,
        [x0, z0]: [usize; 2],
        tile: usize,
        strides: [usize; N],
        serial: u64,
    ) -> Option<[f32; N]> {
        let g = self.grid;
        let state = self.state.lock_any();
        if state.serial != serial {
            return None;
        }
        let h = |x: usize, z: usize| state.heights[g.index(x.min(g.width - 1), z.min(g.depth - 1))];
        Some(strides.map(|s| {
            let mut most = 0f32;
            for cz in (0..tile).step_by(s) {
                for cx in (0..tile).step_by(s) {
                    let (x, z) = (x0 + cx, z0 + cz);
                    let a = h(x, z);
                    let b = h(x + s, z);
                    let c = h(x + s, z + s);
                    let d = h(x, z + s);
                    for v in 0..=s {
                        for u in 0..=s {
                            let (fu, fv) = (u as f32 / s as f32, v as f32 / s as f32);
                            let flat = if u >= v {
                                a + fu * (b - a) + fv * (c - b)
                            } else {
                                a + fv * (d - a) + fu * (c - d)
                            };
                            most = most.max((h(x + u, z + v) - flat).abs());
                        }
                    }
                }
            }
            most
        }))
    }

    fn whole(&self) -> Rect {
        Rect {
            x0: 0,
            z0: 0,
            x1: self.grid.width,
            z1: self.grid.depth,
        }
    }

    /// Writes whole `rows` into `texture` and their slopes into `slopes`.
    fn upload_rows(
        &self,
        queue: &wgpu::Queue,
        texture: &wgpu::Texture,
        slopes: &wgpu::Texture,
        rows: std::ops::Range<usize>,
    ) {
        let state = self.state.lock_any();
        let width = self.grid.width;
        let heights = &state.heights[rows.start * width..rows.end * width];
        super::write_texture_rect(
            queue,
            texture,
            [0, rows.start as u32],
            [width as u32, rows.len() as u32],
            4,
            bytemuck::cast_slice(heights),
        );
        let rect = Rect {
            x0: 0,
            z0: rows.start,
            x1: width,
            z1: rows.end,
        };
        self.upload_slopes(queue, slopes, &state, rect);
    }

    /// Writes the slopes of `rect`'s samples into `slopes` (ResidentHeights::
    /// slopes), and of the ring round it, whose central differences read
    /// `rect`'s heights.
    fn upload_slopes(
        &self,
        queue: &wgpu::Queue,
        slopes: &wgpu::Texture,
        state: &State,
        rect: Rect,
    ) {
        let g = self.grid;
        let r = Rect {
            x0: rect.x0.saturating_sub(1),
            z0: rect.z0.saturating_sub(1),
            x1: (rect.x1 + 1).min(g.width),
            z1: (rect.z1 + 1).min(g.depth),
        };
        let h = |x: usize, z: usize| state.heights[g.index(x.min(g.width - 1), z.min(g.depth - 1))];
        let mut packed = Vec::with_capacity((r.x1 - r.x0) * (r.z1 - r.z0) * 4);
        for z in r.z0..r.z1 {
            for x in r.x0..r.x1 {
                let (x0, x1) = (h(x.saturating_sub(1), z), h(x + 1, z));
                let (z0, z1) = (h(x, z.saturating_sub(1)), h(x, z + 1));
                let at = h(x, z);
                packed.extend(
                    [
                        (x1 - x0) / (2. * g.spacing),
                        (z1 - z0) / (2. * g.spacing),
                        (x1 - 2. * at + x0) / g.spacing,
                        (z1 - 2. * at + z0) / g.spacing,
                    ]
                    .map(f16_bits),
                );
            }
        }
        super::write_texture_rect(
            queue,
            slopes,
            [r.x0 as u32, r.z0 as u32],
            [(r.x1 - r.x0) as u32, (r.z1 - r.z0) as u32],
            8,
            bytemuck::cast_slice(&packed),
        );
    }

    /// Writes the rects of the current heights `texture` and their cells
    /// `cells` still lack, and returns the serial they bring them to and
    /// the samples they cover. `since` is the serial they already have;
    /// `None` means nothing yet.
    fn upload(
        &self,
        queue: &wgpu::Queue,
        texture: &wgpu::Texture,
        slopes: &wgpu::Texture,
        cells: &wgpu::Buffer,
        since: Option<u64>,
    ) -> (u64, Option<Rect>) {
        let state = self.state.lock_any();
        if since == Some(state.serial) {
            return (state.serial, None);
        }
        let mut covered: Option<Rect> = None;
        for rect in pending(&state.log, since, self.whole()) {
            self.upload_cells(queue, cells, &state, rect);
            covered = Some(covered.map_or(rect, |c| c.union(rect)));
            let width = rect.x1 - rect.x0;
            let mut packed = Vec::with_capacity(width * (rect.z1 - rect.z0));
            for z in rect.z0..rect.z1 {
                let row = self.grid.index(rect.x0, z);
                packed.extend_from_slice(&state.heights[row..row + width]);
            }
            let data: &[u8] = bytemuck::cast_slice(&packed);
            super::write_texture_rect(
                queue,
                texture,
                [rect.x0 as u32, rect.z0 as u32],
                [width as u32, (rect.z1 - rect.z0) as u32],
                4,
                data,
            );
            self.upload_slopes(queue, slopes, &state, rect);
        }
        (state.serial, covered)
    }
}

/// `v` as IEEE half-precision bits, rounded to nearest even.
fn f16_bits(v: f32) -> u16 {
    let b = v.to_bits();
    let sign = (b >> 16 & 0x8000) as u16;
    let exp = (b >> 23 & 0xff) as i32;
    let man = b & 0x7f_ffff;
    if exp == 0xff {
        return sign | 0x7c00 | u16::from(man != 0) << 9;
    }
    let e = exp - 127 + 15;
    if e >= 0x1f {
        return sign | 0x7c00;
    }
    // Normal halves drop 13 mantissa bits; subnormal ones the implicit bit
    // shifted down as well.
    let (m, shift) = if e > 0 {
        (man, 13)
    } else if e >= -10 {
        (man | 0x80_0000, 14 - e)
    } else {
        return sign;
    };
    let kept = m >> shift;
    let rest = m & ((1 << shift) - 1);
    let half = 1 << (shift - 1);
    let up = u32::from(rest > half || (rest == half && kept & 1 == 1));
    // A carry out of the mantissa steps the exponent, up to infinity.
    sign | (((e.max(0) as u32) << 10 | kept) + up) as u16
}

/// Appends an edit to a map's log, dropping the oldest past `LOG`.
pub(super) fn record(log: &mut VecDeque<(u64, Rect)>, serial: u64, rect: Rect) {
    log.push_back((serial, rect));
    if log.len() > LOG {
        log.pop_front();
    }
}

/// The rectangles a device copy at serial `since` still needs: the logged
/// edits after it, or `whole` when it has nothing yet or fell out of the
/// log; merged into their union when there are many.
pub(super) fn pending(log: &VecDeque<(u64, Rect)>, since: Option<u64>, whole: Rect) -> Vec<Rect> {
    let mut rects = match since {
        // Missed edits that fell out of the log: start over.
        Some(s) if log.front().is_some_and(|&(first, _)| first <= s + 1) => log
            .iter()
            .filter(|&&(serial, _)| serial > s)
            .map(|&(_, rect)| rect)
            .collect(),
        _ => vec![whole],
    };
    if rects.len() > MERGE {
        rects = vec![rects.iter().fold(Rect::default(), |a, &b| a.union(b))];
    }
    rects
}

/// A grid's heights texture, one R32Float texel per sample.
fn texture_descriptor(grid: Grid) -> wgpu::TextureDescriptor<'static> {
    wgpu::TextureDescriptor {
        label: Some("editor heights"),
        size: wgpu::Extent3d {
            width: grid.width as u32,
            height: grid.depth as u32,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::R32Float,
        usage: wgpu::TextureUsages::COPY_DST | wgpu::TextureUsages::TEXTURE_BINDING,
        view_formats: &[],
    }
}

/// One device's copy of a `HeightMap`. Like a control map's it fills in
/// bands of rows within each frame's upload budget (`fill`): an 1800
/// island's heights written whole held its first frame 12-22 ms. Nothing
/// may sample it until `fill` says it is full.
pub(super) struct ResidentHeights {
    pub source: std::sync::Arc<HeightMap>,
    texture: wgpu::Texture,
    pub view: wgpu::TextureView,
    /// Per sample, Rgba16Float: its slope along X and Z (central
    /// differences a sample each way, over metres) and its second
    /// differences along X and Z (over the spacing). Every one is linear in
    /// the heights, so filtering them between samples gives what the
    /// bilinear heights give there: gpu.wgsl terrain_detail shades from one
    /// filtered fetch instead of twelve height loads (1800's moderate ground
    /// 1.57 to 1.47 ms at 5K). Half floats: slopes keep three digits, far
    /// finer than the shading resolves.
    slopes: wgpu::Texture,
    pub slopes_view: wgpu::TextureView,
    /// `HeightMap`'s cell ranges, two floats a cell, in step with `view`.
    pub cells: wgpu::Buffer,
    synced: Mutex<Fill>,
}

#[derive(Clone, Copy)]
enum Fill {
    /// Rows before `next` are written, as of edit serial `from` or later.
    Rows { next: usize, from: u64 },
    /// Every height, as of this edit serial.
    Synced(u64),
}

impl ResidentHeights {
    /// Creates the texture, empty: `fill` writes it.
    pub fn new(gpu: &super::Gpu, source: std::sync::Arc<HeightMap>) -> Self {
        let grid = source.grid;
        let texture = gpu.device.create_texture(&texture_descriptor(grid));
        let view = texture.create_view(&Default::default());
        let slopes = gpu.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("editor height slopes"),
            format: wgpu::TextureFormat::Rgba16Float,
            ..texture_descriptor(grid)
        });
        let slopes_view = slopes.create_view(&Default::default());
        let from = source.state.lock_any().serial;
        let cells = gpu.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("editor height cells"),
            size: (source.state.lock_any().cells.len() * 8) as u64,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::STORAGE,
            mapped_at_creation: false,
        });
        Self {
            source,
            texture,
            view,
            slopes,
            slopes_view,
            cells,
            synced: Mutex::new(Fill::Rows { next: 0, from }),
        }
    }

    /// Writes rows while `budget` allows, at least one band a call; true
    /// once every height is in. Edits made meanwhile follow at the end.
    pub fn fill(&self, queue: &wgpu::Queue, budget: &mut super::Budget) -> bool {
        let mut synced = self.synced.lock_any();
        let Fill::Rows { mut next, from } = *synced else {
            return true;
        };
        let (width, depth) = (self.source.grid.width, self.source.grid.depth);
        let row = width * 4;
        while next < depth {
            if budget.spent() {
                *synced = Fill::Rows { next, from };
                return false;
            }
            let rows = (budget.room() / row).clamp(1, depth - next);
            self.source
                .upload_rows(queue, &self.texture, &self.slopes, next..next + rows);
            // The slopes write twice the heights' bytes.
            budget.charge(rows * row * 3);
            next += rows;
        }
        // Small: written whole once the heights are.
        let whole = self.source.whole();
        self.source
            .upload_cells(queue, &self.cells, &self.source.state.lock_any(), whole);
        // Rows written before an edit missed it: its log rewrites them.
        let (serial, _) =
            (self.source).upload(queue, &self.texture, &self.slopes, &self.cells, Some(from));
        *synced = Fill::Synced(serial);
        true
    }

    /// The edit serial the texture holds every height as of; none while
    /// still filling.
    pub fn serial(&self) -> Option<u64> {
        match *self.synced.lock_any() {
            Fill::Synced(serial) => Some(serial),
            Fill::Rows { .. } => None,
        }
    }

    /// Writes edits made since the last sync; the samples they cover.
    /// While still filling, nothing: `fill` takes them in at its end.
    pub fn sync(&self, queue: &wgpu::Queue) -> Option<Rect> {
        let mut synced = self.synced.lock_any();
        let Fill::Synced(since) = *synced else {
            return None;
        };
        let (serial, covered) =
            (self.source).upload(queue, &self.texture, &self.slopes, &self.cells, Some(since));
        *synced = Fill::Synced(serial);
        covered
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(heights: impl Fn(usize, usize) -> f32) -> HeightMap {
        let grid = Grid {
            width: 17,
            depth: 17,
            spacing: 0.5,
            origin: [0., 0.],
        };
        let heights = (0..17 * 17).map(|i| heights(i % 17, i / 17)).collect();
        HeightMap::new(&Heightfield::new(grid, heights)).unwrap()
    }

    #[test]
    fn halves_round_to_nearest_even() {
        assert_eq!(f16_bits(1.), 0x3c00);
        assert_eq!(f16_bits(-2.), 0xc000);
        assert_eq!(f16_bits(0.1), 0x2e66);
        assert_eq!(f16_bits(65504.), 0x7bff);
        assert_eq!(f16_bits(1e6), 0x7c00);
        // The smallest subnormal, and halfway below it to even zero.
        assert_eq!(f16_bits(2f32.powi(-24)), 0x0001);
        assert_eq!(f16_bits(2f32.powi(-25)), 0x0000);
        // 1 + 2^-11 lies halfway between 1 and the next half: even, 1.
        assert_eq!(f16_bits(1. + 2f32.powi(-11)), 0x3c00);
        assert_eq!(f16_bits(1. + 3. * 2f32.powi(-11)), 0x3c02);
    }

    #[test]
    fn a_plane_draws_at_every_stride_without_error() {
        let m = map(|x, z| 0.3 * x as f32 - 0.7 * z as f32);
        let errors = m.patch_errors([0, 0], 16, [2, 4, 8, 16], 0).unwrap();
        assert!(errors.iter().all(|&e| e < 1e-4), "{errors:?}");
    }

    #[test]
    fn a_bump_strays_by_its_height_where_the_stride_skips_it() {
        // On sample (5, 6): odd, so every stride above one skips it.
        let m = map(|x, z| if (x, z) == (5, 6) { 2. } else { 0. });
        let errors = m.patch_errors([0, 0], 16, [2, 4], 0).unwrap();
        assert_eq!(errors, [2., 2.]);
        // On (4, 4), strides of 2 and 4 keep it as a vertex, but their
        // triangles round it lean toward it over the samples beside it,
        // half and three quarters of the way up; 8 skips it.
        let m = map(|x, z| if (x, z) == (4, 4) { 2. } else { 0. });
        let errors = m.patch_errors([0, 0], 16, [2, 4, 8], 0).unwrap();
        assert_eq!(errors, [1., 1.5, 2.]);
        // Edited since: unknown.
        assert!(m.patch_errors([0, 0], 16, [2], 1).is_none());
    }
}
