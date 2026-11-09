// Terrain heights as an R32Float texture that meshes displace from, and the
// rectangle edits that keep every device copy in step with the host's grid.
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

/// Heights a displaced mesh samples on the GPU, one texel per heightfield
/// sample. Share it through `Arc` in `MeshData::heights`. `update` copies a
/// dirty rectangle in; every device copy picks it up on its next frame, with
/// no new scene and no mesh rebuild.
///
/// Keeps a CPU copy of the heights (4 bytes per sample, 4 MB for 1025²) so a
/// device that first sees the map later, or fell behind, uploads from it.
pub struct HeightMap {
    grid: Grid,
    state: Mutex<State>,
}

struct State {
    heights: Vec<f32>,
    /// Lowest and highest height ever stored. Edits only widen it, so
    /// culling and shadow fitting stay conservative after a dig.
    range: [f32; 2],
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
        Ok(Self {
            grid,
            state: Mutex::new(State {
                heights: field.heights.clone(),
                range: field.range(),
                serial: 0,
                log: VecDeque::new(),
            }),
        })
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
        let mut state = self.state.lock().unwrap();
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
        state.serial += 1;
        let serial = state.serial;
        record(&mut state.log, serial, rect);
    }

    /// World Y the heights have spanned since creation.
    pub(super) fn range(&self) -> [f32; 2] {
        self.state.lock().unwrap().range
    }

    fn whole(&self) -> Rect {
        Rect {
            x0: 0,
            z0: 0,
            x1: self.grid.width,
            z1: self.grid.depth,
        }
    }

    /// Writes the rects of the current heights `texture` still lacks, and
    /// returns the serial they bring it to and the bytes written. `since`
    /// is the serial the texture already has; `None` means nothing yet.
    fn upload(
        &self,
        queue: &wgpu::Queue,
        texture: &wgpu::Texture,
        since: Option<u64>,
    ) -> (u64, usize) {
        let state = self.state.lock().unwrap();
        if since == Some(state.serial) {
            return (state.serial, 0);
        }
        let mut bytes = 0;
        for rect in pending(&state.log, since, self.whole()) {
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
            bytes += data.len();
        }
        (state.serial, bytes)
    }
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

/// One device's copy of a `HeightMap`.
pub(super) struct ResidentHeights {
    pub source: std::sync::Arc<HeightMap>,
    texture: wgpu::Texture,
    pub view: wgpu::TextureView,
    synced: Mutex<u64>,
}

impl ResidentHeights {
    /// Creates the texture and writes every height. Returns it with the
    /// bytes written.
    pub fn new(gpu: &super::Gpu, source: std::sync::Arc<HeightMap>) -> (Self, usize) {
        let grid = source.grid;
        let texture = gpu.device.create_texture(&wgpu::TextureDescriptor {
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
        });
        let view = texture.create_view(&Default::default());
        let (synced, bytes) = source.upload(&gpu.queue, &texture, None);
        let resident = Self {
            source,
            texture,
            view,
            synced: Mutex::new(synced),
        };
        (resident, bytes)
    }

    /// Writes edits made since the last sync. True if any were.
    pub fn sync(&self, queue: &wgpu::Queue) -> bool {
        let mut synced = self.synced.lock().unwrap();
        let (serial, _) = self.source.upload(queue, &self.texture, Some(*synced));
        let changed = serial != *synced;
        *synced = serial;
        changed
    }
}
