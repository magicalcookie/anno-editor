// RGBA8 control maps that paint strokes edit by rectangle (a ground's
// layer indices and weights, tint and masks), and the device copies of
// those drawn as they are, kept in step on every device the same way height
// maps are.
use super::LockAny as _;
use super::heights::{pending, record};
use crate::heightfield::Rect;
use anyhow::{Result, ensure};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

// Same bound as height maps: Vulkan guarantees 4096 texels per side.
const MAX_SIDE: usize = 4096;

/// An RGBA8 control image a ground or mesh draws from (paint, tint, masks).
/// `write` replaces a rectangle of texels; every device copy uploads just
/// the changed rectangles on its next frame, so painting shows at once
/// without a new ground or mesh.
///
/// Keeps a CPU copy (4 bytes per texel, 16 MB at 2048²) for devices that
/// see the map later or fell behind, or, made `drawn`, has the host draw
/// any rectangle of it from its own data when read: an island ground's
/// controls are a view of its material maps, which hold them already.
pub struct ControlMap {
    width: usize,
    height: usize,
    state: Mutex<State>,
}

struct State {
    texels: Texels,
    serial: u64,
    log: VecDeque<(u64, Rect)>,
}

enum Texels {
    Held(Vec<u8>),
    /// RGBA rows of the rectangle asked for.
    Drawn(Arc<dyn Fn(Rect) -> Vec<u8> + Send + Sync>),
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
                texels: Texels::Held(texels),
                serial: 0,
                log: VecDeque::new(),
            }),
        })
    }

    /// A map whose texels `draw` gives, RGBA rows of the rectangle asked
    /// for, from data the host keeps; [`ControlMap::touch`] says where they
    /// changed.
    pub fn drawn(
        width: usize,
        height: usize,
        draw: impl Fn(Rect) -> Vec<u8> + Send + Sync + 'static,
    ) -> Result<Self> {
        ensure!(
            (1..=MAX_SIDE).contains(&width) && (1..=MAX_SIDE).contains(&height),
            "Control map is {width}x{height}; sides must be 1..={MAX_SIDE}"
        );
        Ok(Self {
            width,
            height,
            state: Mutex::new(State {
                texels: Texels::Drawn(Arc::new(draw)),
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
        self.rows(self.whole())
    }

    /// RGBA rows of `rect` (clipped to the map).
    pub fn rows(&self, rect: Rect) -> Vec<u8> {
        self.with_rows(rect, <[u8]>::to_vec)
    }

    /// `with` given the RGBA rows of `rect` (clipped to the map): held
    /// rows the map's full width are lent, not copied; a drawn map draws
    /// them outside its lock.
    fn with_rows<R>(&self, rect: Rect, with: impl FnOnce(&[u8]) -> R) -> R {
        let rect = Rect {
            x0: rect.x0.min(self.width),
            z0: rect.z0.min(self.height),
            x1: rect.x1.min(self.width),
            z1: rect.z1.min(self.height),
        };
        let draw = {
            let state = self.state.lock_any();
            match &state.texels {
                Texels::Held(all) if rect.x0 == 0 && rect.x1 == self.width => {
                    return with(&all[rect.z0 * self.width * 4..rect.z1 * self.width * 4]);
                }
                Texels::Held(all) => {
                    let mut out = Vec::with_capacity((rect.x1 - rect.x0) * (rect.z1 - rect.z0) * 4);
                    for z in rect.z0..rect.z1 {
                        let row = (z * self.width + rect.x0) * 4;
                        out.extend_from_slice(&all[row..row + (rect.x1 - rect.x0) * 4]);
                    }
                    drop(state);
                    return with(&out);
                }
                Texels::Drawn(draw) => draw.clone(),
            }
        };
        with(&draw(rect))
    }

    /// Notes that what a drawn map draws changed under `rect`, as after a
    /// `write`: device copies and what is built from it there catch up.
    pub fn touch(&self, rect: Rect) {
        let clipped = Rect {
            x0: rect.x0,
            z0: rect.z0,
            x1: rect.x1.min(self.width),
            z1: rect.z1.min(self.height),
        };
        if clipped.is_empty() {
            return;
        }
        let mut state = self.state.lock_any();
        state.serial += 1;
        let serial = state.serial;
        record(&mut state.log, serial, clipped);
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
        let mut state = self.state.lock_any();
        let Texels::Held(all) = &mut state.texels else {
            panic!("a drawn control map is written by its host: touch it");
        };
        let span = (clipped.x1 - clipped.x0) * 4;
        for z in clipped.z0..clipped.z1 {
            let from = (z - rect.z0) * width * 4;
            let to = (z * self.width + clipped.x0) * 4;
            all[to..to + span].copy_from_slice(&texels[from..from + span]);
        }
        state.serial += 1;
        let serial = state.serial;
        record(&mut state.log, serial, clipped);
    }

    /// The serial now and the rectangles written since `since` (the whole
    /// map for `None`, or when those edits fell out of the log).
    /// The edit serial now and the texels edited since `since`, all of
    /// them for `None` or a serial older than the kept log.
    pub fn edits_since(&self, since: Option<u64>) -> (u64, Vec<Rect>) {
        let state = self.state.lock_any();
        if since == Some(state.serial) {
            return (state.serial, Vec::new());
        }
        (state.serial, pending(&state.log, since, self.whole()))
    }

    fn whole(&self) -> Rect {
        Rect {
            x0: 0,
            z0: 0,
            x1: self.width,
            z1: self.height,
        }
    }

    /// Writes whole `rows` into `texture`, kept as `kept`.
    fn upload_rows(
        &self,
        queue: &wgpu::Queue,
        texture: &wgpu::Texture,
        rows: std::ops::Range<usize>,
        kept: Kept,
    ) {
        let rect = Rect {
            x0: 0,
            z0: rows.start,
            x1: self.width,
            z1: rows.end,
        };
        self.with_rows(rect, |texels| {
            self.write_device(texels, queue, texture, rect, kept);
        });
    }

    /// Writes the rects `texture` still lacks; returns the serial they
    /// bring it to and the texels they cover.
    fn upload(
        &self,
        queue: &wgpu::Queue,
        texture: &wgpu::Texture,
        since: Option<u64>,
        kept: Kept,
    ) -> (u64, Option<Rect>) {
        let (serial, rects) = {
            let state = self.state.lock_any();
            if since == Some(state.serial) {
                return (state.serial, None);
            }
            (state.serial, pending(&state.log, since, self.whole()))
        };
        let mut covered: Option<Rect> = None;
        for rect in rects {
            covered = Some(covered.map_or(rect, |c| c.union(rect)));
            // Blocks are written whole: the map's sides are whole blocks.
            let rect = if kept == Kept::Bc1 {
                Rect {
                    x0: rect.x0 / 4 * 4,
                    z0: rect.z0 / 4 * 4,
                    x1: rect.x1.next_multiple_of(4),
                    z1: rect.z1.next_multiple_of(4),
                }
            } else {
                rect
            };
            self.with_rows(rect, |texels| {
                self.write_device(texels, queue, texture, rect, kept);
            });
        }
        (serial, covered)
    }

    /// Packs `texels`, the RGBA rows of `rect`, as `kept` and writes them
    /// into `texture`.
    fn write_device(
        &self,
        texels: &[u8],
        queue: &wgpu::Queue,
        texture: &wgpu::Texture,
        rect: Rect,
        kept: Kept,
    ) {
        let (width, height) = (rect.x1 - rect.x0, rect.z1 - rect.z0);
        let row = |z: usize| &texels[(z - rect.z0) * width * 4..(z - rect.z0 + 1) * width * 4];
        // RGBA goes as handed over, without a copy; byte by byte through
        // iterators an 8 MB band took the UI thread 40 ms.
        let packed: std::borrow::Cow<[u8]>;
        let bytes_per_row = match kept {
            Kept::Rgba => {
                packed = std::borrow::Cow::Borrowed(texels);
                width * 4
            }
            // A plain loop: rayon's pool is busy with the ground bake while
            // an island opens, and a task per row waited 19 ms for 2 M
            // texels a loop takes 2 ms for.
            Kept::Red => {
                let mut out = Vec::with_capacity(width * height);
                for z in rect.z0..rect.z1 {
                    out.extend(row(z).as_chunks::<4>().0.iter().map(|t| t[0]));
                }
                packed = std::borrow::Cow::Owned(out);
                width
            }
            Kept::Bc1 => {
                use rayon::prelude::*;
                let blocks = width / 4;
                let mut encoded = vec![0u8; blocks * 8 * (height / 4)];
                encoded
                    .par_chunks_mut(blocks * 8)
                    .enumerate()
                    .for_each(|(by, out)| {
                        let rows: [&[u8]; 4] = std::array::from_fn(|y| row(rect.z0 + by * 4 + y));
                        for (bx, block) in out.chunks_exact_mut(8).enumerate() {
                            let texels: [[u8; 4]; 16] = std::array::from_fn(|i| {
                                let at = (bx * 4 + i % 4) * 4;
                                rows[i / 4][at..at + 4].try_into().unwrap()
                            });
                            block.copy_from_slice(&crate::texture::encode_bc1(&texels));
                        }
                    });
                packed = std::borrow::Cow::Owned(encoded);
                blocks * 8
            }
        };
        queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture,
                mip_level: 0,
                origin: wgpu::Origin3d {
                    x: rect.x0 as u32,
                    y: rect.z0 as u32,
                    z: 0,
                },
                aspect: wgpu::TextureAspect::All,
            },
            &packed,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(bytes_per_row as u32),
                rows_per_image: None,
            },
            wgpu::Extent3d {
                width: width as u32,
                height: height as u32,
                depth_or_array_layers: 1,
            },
        );
    }
}

/// What a device copy of a control map keeps.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kept {
    /// RGBA8, as stored.
    Rgba,
    /// Red alone (R8), for maps shaders read no more of.
    Red,
    /// RGB as BC1, an eighth of RGBA8, for color maps the game itself
    /// ships as DXT1 (the ground tint). Only on devices with BC and for
    /// sides of whole blocks; others keep RGBA8.
    Bc1,
}

/// One device's copy of a `ControlMap`. It fills in bands of rows within
/// each frame's upload budget (`fill`): a 4096² ground tint written whole
/// held an 1800 island's first frame for 33-41 ms, its ground's controls
/// for 30 ms more. Nothing may sample it until `ready`.
pub struct ResidentControl {
    pub source: Arc<ControlMap>,
    texture: wgpu::Texture,
    pub view: wgpu::TextureView,
    kept: Kept,
    synced: Mutex<Fill>,
}

#[derive(Clone, Copy)]
enum Fill {
    /// Rows before `next` are written, as of edit serial `from` or later.
    Rows { next: usize, from: u64 },
    /// Every texel, as of this edit serial.
    Synced(u64),
}

impl ResidentControl {
    /// Creates the texture, empty: `fill` writes it. `label` names it in
    /// the device's memory report.
    pub fn new(device: &wgpu::Device, label: &str, source: Arc<ControlMap>, kept: Kept) -> Self {
        let blocks = source.width.is_multiple_of(4) && source.height.is_multiple_of(4);
        let kept = match kept {
            Kept::Bc1 if !(blocks && super::bc_supported(device)) => Kept::Rgba,
            k => k,
        };
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some(label),
            size: wgpu::Extent3d {
                width: source.width as u32,
                height: source.height as u32,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: match kept {
                Kept::Rgba => wgpu::TextureFormat::Rgba8Unorm,
                Kept::Red => wgpu::TextureFormat::R8Unorm,
                Kept::Bc1 => wgpu::TextureFormat::Bc1RgbaUnorm,
            },
            usage: wgpu::TextureUsages::COPY_DST | wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        });
        let view = texture.create_view(&Default::default());
        let from = source.state.lock_any().serial;
        Self {
            source,
            texture,
            view,
            kept,
            synced: Mutex::new(Fill::Rows { next: 0, from }),
        }
    }

    /// Writes rows while `budget` allows, at least one band a call; true
    /// once every texel is in. Edits made meanwhile follow at the end.
    pub fn fill(&self, queue: &wgpu::Queue, budget: &mut super::Budget) -> bool {
        let mut synced = self.synced.lock_any();
        let Fill::Rows { mut next, from } = *synced else {
            return true;
        };
        let (width, height) = (self.source.width, self.source.height);
        // Bands are charged as RGBA: encoding them costs more than their
        // bytes, and BC1 bands keep to a decoded band's size
        // (DECODE_BAND_BYTES) and whole blocks.
        let row = width * 4;
        let (room, step) = match self.kept {
            Kept::Bc1 => (super::DECODE_BAND_BYTES, 4),
            _ => (usize::MAX, 1),
        };
        while next < height {
            if budget.spent() {
                *synced = Fill::Rows { next, from };
                return false;
            }
            let rows = (budget.room().min(room) / row / step * step).clamp(step, height - next);
            self.source
                .upload_rows(queue, &self.texture, next..next + rows, self.kept);
            budget.charge(rows * row);
            next += rows;
        }
        // Rows written before an edit missed it: its log rewrites them.
        let (serial, _) = self
            .source
            .upload(queue, &self.texture, Some(from), self.kept);
        *synced = Fill::Synced(serial);
        true
    }

    /// Whether every texel is in: until then nothing samples it.
    pub fn ready(&self) -> bool {
        matches!(*self.synced.lock_any(), Fill::Synced(_))
    }

    /// Writes edits made since the last sync; the texels they cover. While
    /// still filling, nothing: `fill` takes them in at its end.
    pub fn sync(&self, queue: &wgpu::Queue) -> Option<Rect> {
        let mut synced = self.synced.lock_any();
        let Fill::Synced(since) = *synced else {
            return None;
        };
        let (serial, covered) = self
            .source
            .upload(queue, &self.texture, Some(since), self.kept);
        *synced = Fill::Synced(serial);
        covered
    }
}
