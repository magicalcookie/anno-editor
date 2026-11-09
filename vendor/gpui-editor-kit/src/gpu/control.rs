// Splat control images (layer indices and weights) as RGBA8 textures that
// paint strokes edit by rectangle, kept in step on every device the same way
// height maps are, and the per-tile palettes the GPU builds from them for
// drawing (palette.wgsl).
use super::heights::{pending, record};
use crate::heightfield::Rect;
use anyhow::{Result, ensure};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

// Same bound as height maps: Vulkan guarantees 4096 texels per side.
const MAX_SIDE: usize = 4096;

/// An RGBA8 control image for a `Splat`. `write` replaces a rectangle of
/// texels; every device copy uploads just the changed rectangles on its
/// next frame, so painting shows at once without a new splat or mesh.
///
/// Keeps a CPU copy (4 bytes per texel, 16 MB at 2048²) for devices that
/// see the map later or fell behind.
pub struct ControlMap {
    width: usize,
    height: usize,
    state: Mutex<State>,
}

struct State {
    texels: Vec<u8>,
    serial: u64,
    log: VecDeque<(u64, Rect)>,
}

impl std::fmt::Debug for ControlMap {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ControlMap")
            .field("width", &self.width)
            .field("height", &self.height)
            .finish_non_exhaustive()
    }
}

impl ControlMap {
    /// `texels` are RGBA, row-major, `width × height × 4` bytes.
    pub fn new(width: usize, height: usize, texels: Vec<u8>) -> Result<Self> {
        ensure!(
            (1..=MAX_SIDE).contains(&width) && (1..=MAX_SIDE).contains(&height),
            "Control map is {width}x{height}; sides must be 1..={MAX_SIDE}"
        );
        ensure!(
            texels.len() == width * height * 4,
            "Control map has {} bytes for {width}x{height} RGBA texels",
            texels.len()
        );
        Ok(Self {
            width,
            height,
            state: Mutex::new(State {
                texels,
                serial: 0,
                log: VecDeque::new(),
            }),
        })
    }

    pub fn size(&self) -> [usize; 2] {
        [self.width, self.height]
    }

    /// A copy of every texel, RGBA rows.
    pub fn to_vec(&self) -> Vec<u8> {
        self.state.lock().unwrap().texels.clone()
    }

    /// Replaces the texels of `rect` (texel columns `x0..x1`, rows
    /// `z0..z1`, clipped to the map) with `texels`, RGBA rows of the
    /// unclipped rect. Repaint afterwards; the change shows next frame.
    pub fn write(&self, rect: Rect, texels: &[u8]) {
        let width = rect.x1.saturating_sub(rect.x0);
        assert_eq!(
            texels.len(),
            width * rect.z1.saturating_sub(rect.z0) * 4,
            "one RGBA texel per texel of the rect"
        );
        let clipped = Rect {
            x0: rect.x0,
            z0: rect.z0,
            x1: rect.x1.min(self.width),
            z1: rect.z1.min(self.height),
        };
        if clipped.is_empty() {
            return;
        }
        let mut state = self.state.lock().unwrap();
        let span = (clipped.x1 - clipped.x0) * 4;
        for z in clipped.z0..clipped.z1 {
            let from = (z - rect.z0) * width * 4;
            let to = (z * self.width + clipped.x0) * 4;
            state.texels[to..to + span].copy_from_slice(&texels[from..from + span]);
        }
        state.serial += 1;
        let serial = state.serial;
        record(&mut state.log, serial, clipped);
    }

    fn whole(&self) -> Rect {
        Rect {
            x0: 0,
            z0: 0,
            x1: self.width,
            z1: self.height,
        }
    }

    /// Writes the rects `texture` still lacks; returns the serial they
    /// bring it to and the texels they cover.
    fn upload(
        &self,
        queue: &wgpu::Queue,
        texture: &wgpu::Texture,
        since: Option<u64>,
    ) -> (u64, Option<Rect>) {
        let state = self.state.lock().unwrap();
        if since == Some(state.serial) {
            return (state.serial, None);
        }
        let mut covered: Option<Rect> = None;
        for rect in pending(&state.log, since, self.whole()) {
            covered = Some(covered.map_or(rect, |c| c.union(rect)));
            let width = rect.x1 - rect.x0;
            let mut packed = Vec::with_capacity(width * (rect.z1 - rect.z0) * 4);
            for z in rect.z0..rect.z1 {
                let row = (z * self.width + rect.x0) * 4;
                packed.extend_from_slice(&state.texels[row..row + width * 4]);
            }
            super::write_texture_rect(
                queue,
                texture,
                [rect.x0 as u32, rect.z0 as u32],
                [width as u32, (rect.z1 - rect.z0) as u32],
                4,
                &packed,
            );
        }
        (state.serial, covered)
    }
}

/// One device's copy of a `ControlMap`.
pub(super) struct ResidentControl {
    source: Arc<ControlMap>,
    texture: wgpu::Texture,
    pub view: wgpu::TextureView,
    synced: Mutex<u64>,
}

impl ResidentControl {
    /// Creates the texture and writes every texel.
    pub fn new(gpu: &super::Gpu, source: Arc<ControlMap>) -> Self {
        let texture = gpu.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("splat control"),
            size: wgpu::Extent3d {
                width: source.width as u32,
                height: source.height as u32,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::COPY_DST | wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        });
        let view = texture.create_view(&Default::default());
        let (synced, _) = source.upload(&gpu.queue, &texture, None);
        Self {
            source,
            texture,
            view,
            synced: Mutex::new(synced),
        }
    }

    /// Writes edits made since the last sync; the texels they cover.
    pub fn sync(&self, queue: &wgpu::Queue) -> Option<Rect> {
        let mut synced = self.synced.lock().unwrap();
        let (serial, covered) = self.source.upload(queue, &self.texture, Some(*synced));
        *synced = serial;
        covered
    }
}

/// Control texels per palette tile side; palette.wgsl `TILE`. Small, so
/// a tile seldom holds more than its eight layers: at 64 texels (24 m on a
/// 768 m island) 4.5% of an overview's pixels changed against merging every
/// layer per pixel, at 8 0.3%. 2048² control maps make 256² tiles, 52 MB of
/// weights with their borders.
pub(super) const PALETTE_TILE: usize = 8;
const PALETTE_SIDE: u32 = PALETTE_TILE as u32 + 2;

/// One device's palettes for a splat: per tile its eight heaviest layers'
/// ids (two RGBA8 texels) and their weights over the tile and a border
/// texel (two RGBA8 atlases of `PALETTE_SIDE`² regions). fs_splat reads
/// these instead of the control maps. Layers past a tile's eight drop
/// there.
pub(super) struct Palette {
    pub weights: [wgpu::TextureView; 2],
    pub ids: [wgpu::TextureView; 2],
    tiles: [u32; 2],
}

impl Palette {
    pub fn new(gpu: &super::Gpu, control: [usize; 2]) -> Self {
        let tiles = control.map(|c| c.div_ceil(PALETTE_TILE).max(1) as u32);
        let texture = |label, size: [u32; 2]| {
            gpu.device.create_texture(&wgpu::TextureDescriptor {
                label: Some(label),
                size: wgpu::Extent3d {
                    width: size[0],
                    height: size[1],
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Rgba8Unorm,
                usage: wgpu::TextureUsages::STORAGE_BINDING | wgpu::TextureUsages::TEXTURE_BINDING,
                view_formats: &[],
            })
        };
        let atlas = tiles.map(|t| t * PALETTE_SIDE);
        let textures = [
            texture("splat palette weights", atlas),
            texture("splat palette weights", atlas),
            texture("splat palette ids", tiles),
            texture("splat palette ids", tiles),
        ];
        let view = |t: &wgpu::Texture| t.create_view(&Default::default());
        Self {
            weights: [view(&textures[0]), view(&textures[1])],
            ids: [view(&textures[2]), view(&textures[3])],
            tiles,
        }
    }

    /// Rebuilds the tiles `rect` (control texels) reaches, border included,
    /// from `indices` and `weights` (one or two of each), in `encoder`.
    pub fn build(
        &self,
        gpu: &super::Gpu,
        encoder: &mut wgpu::CommandEncoder,
        pipeline: &(wgpu::BindGroupLayout, wgpu::ComputePipeline),
        controls: [&[ResidentControl]; 2],
        layers: u32,
        rect: Rect,
    ) {
        let tile = PALETTE_TILE;
        // A texel also sits in the borders of the tiles next to it.
        let first = [
            rect.x0.saturating_sub(1) / tile,
            rect.z0.saturating_sub(1) / tile,
        ];
        let end = [(rect.x1 + 1).div_ceil(tile), (rect.z1 + 1).div_ceil(tile)];
        let end = [
            end[0].min(self.tiles[0] as usize),
            end[1].min(self.tiles[1] as usize),
        ];
        if end[0] <= first[0] || end[1] <= first[1] {
            return;
        }
        let [indices, weights] = controls;
        let globals: [u32; 4] = [
            first[0] as u32,
            first[1] as u32,
            indices.len() as u32,
            layers,
        ];
        let globals = super::buffer_init(
            gpu,
            "splat palette",
            bytemuck::cast_slice(&globals),
            wgpu::BufferUsages::UNIFORM,
        );
        fn control(m: &[ResidentControl], i: usize) -> &wgpu::TextureView {
            &m[i.min(m.len() - 1)].view
        }
        let views = [
            control(indices, 0),
            control(indices, 1),
            control(weights, 0),
            control(weights, 1),
        ];
        let outputs = [
            &self.weights[0],
            &self.weights[1],
            &self.ids[0],
            &self.ids[1],
        ];
        let mut entries = vec![super::entry(0, globals.as_entire_binding())];
        for (i, v) in views.into_iter().chain(outputs).enumerate() {
            entries.push(super::entry(
                i as u32 + 1,
                wgpu::BindingResource::TextureView(v),
            ));
        }
        let group = gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("splat palette"),
            layout: &pipeline.0,
            entries: &entries,
        });
        let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
            label: Some("splat palette"),
            timestamp_writes: None,
        });
        pass.set_pipeline(&pipeline.1);
        pass.set_bind_group(0, &group, &[]);
        pass.dispatch_workgroups((end[0] - first[0]) as u32, (end[1] - first[1]) as u32, 1);
    }

    pub fn whole(&self) -> Rect {
        Rect {
            x0: 0,
            z0: 0,
            x1: self.tiles[0] as usize * PALETTE_TILE,
            z1: self.tiles[1] as usize * PALETTE_TILE,
        }
    }
}
