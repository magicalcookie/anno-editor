// 2070's ground: an island's `Layers` paint uploaded as stored and drawn by
// the ranked chunk splat of terrain.fx (layers.wgsl), on the kit's terrain
// geometry and pages. An editor hands it new paint through `LivePaint`; the
// mounted ground uploads it and names the changed area, so only the pages
// there bake again.
// Engine addresses here are Anno5.exe build 11514956 (Steam).
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use anno_formats::isd::terrain::{GroundTextures, Terrain};
use anno_formats::vfs::Install;
use anyhow::{Context, Result};
use gpui_editor_kit::{
    gpu::{self, ControlMap, HeightMap, StreamedTexture, terrain},
    heightfield::{Grid, Heightfield},
    texture::{Dds, Image},
};

/// Layer textures: the arrays' layer per registry id, and what goes into
/// them. Layers without a diffuse texture (grass and bush objects) aren't
/// drawn.
///
/// Ids 9, 11, 35 and 88 name `wiki_dummy_diff`, the developers' magenta
/// checker (`GroundTextures::placeholder`), and 45 shipped islands paint
/// with them. They are drawn like any layer because the game draws them:
/// its chunk layer list (0x140831130) swaps an id for its `MapToGUID` only
/// while its `MappingProfile` is above the TERRAIN setting, which is 2 at
/// the default High preset and at Very High (presets 0x1400C70C0), and no
/// id has a profile over 2. The editor shows what ships; the shell warns
/// when an island uses them.
pub struct LayerTextures {
    /// Metres one repeat of a layer's textures spans.
    pub scale: f32,
    /// By registry id, the arrays' layer, or `NO_LAYER`.
    layer_of: Vec<u8>,
    diffuse: Vec<Image>,
    normals: Vec<Image>,
}

const NO_LAYER: u8 = u8::MAX;
/// Planes a coverage atlas row holds; the largest island's 2.5k planes
/// make it 1088 texels wide and 680 high.
const COLUMNS: usize = 64;
/// Heights for corners only empty ocean chunks touch, which the file
/// leaves without: a sea floor well below any shore.
const EMPTY_FLOOR: f32 = -30.;

impl LayerTextures {
    /// One layer per id, each its diffuse and its normal map (tangent x
    /// and y, glow in blue, specular in alpha), all of one size.
    pub fn new(scale: f32, layers: Vec<(u32, Image, Image)>) -> Self {
        let mut out = Self {
            scale,
            layer_of: Vec::new(),
            diffuse: Vec::with_capacity(layers.len()),
            normals: Vec::with_capacity(layers.len()),
        };
        for (id, diffuse, normal) in layers {
            let id = id as usize;
            if out.layer_of.len() <= id {
                out.layer_of.resize(id + 1, NO_LAYER);
            }
            out.layer_of[id] = out.diffuse.len() as u8;
            out.diffuse.push(diffuse);
            out.normals.push(normal);
        }
        out
    }

    /// Every registry texture with a diffuse map, at most `max` texels a
    /// side, repeating every `TextureScale` metres.
    pub fn load(install: &Install, registry: &GroundTextures, max: u32) -> Result<Self> {
        Self::read(install, registry, registry.scale(), max)
    }

    fn read(install: &Install, registry: &GroundTextures, scale: f32, max: u32) -> Result<Self> {
        let read = |path: &str| -> Result<Image> {
            let path = anno_formats::model::texture_path(path);
            let bytes = install.read(&path)?;
            let header = Dds::parse(&bytes).with_context(|| path.clone())?;
            let mut c = header.compressed(&bytes).with_context(|| path.clone())?;
            c.shrink(max);
            Ok(Image::from(c))
        };
        let mut layers = Vec::new();
        for t in registry.textures() {
            let Some(diffuse) = &t.diffuse else {
                continue;
            };
            let normal = t
                .normal
                .as_deref()
                .context("ground texture without a normal map")?;
            layers.push((t.id, read(diffuse)?, read(normal)?));
        }
        Ok(Self::new(scale, layers))
    }

    /// The layer textures of the core island's 2070 paint registry, at
    /// most `max` texels a side.
    pub fn of(install: &Install, registry: &anno_core::island::Registry, max: u32) -> Result<Self> {
        Self::read(install, registry.textures(), registry.scale, max)
    }

    fn layer(&self, id: u32) -> Option<u8> {
        self.layer_of
            .get(id as usize)
            .copied()
            .filter(|&l| l != NO_LAYER)
    }
}

/// An island's paint as the GPU reads it: per chunk a span of entries in
/// draw rank order, base layer first, and every drawn layer's coverage
/// plane in a 17x17 cell of an R8 atlas. A chunk keeps its cells across
/// edits (`update`), so a mounted ground uploads only the cells an edit
/// changed.
#[derive(Clone)]
pub struct LayerPaint {
    chunks: [u32; 2],
    /// Per chunk, its first entry and how many.
    spans: Vec<[u32; 2]>,
    /// Per entry, the texture layer in the low byte, its atlas cell above.
    entries: Vec<u32>,
    coverage: Vec<u8>,
    rows: usize,
    /// Cells handed out, used or not, and those no entry uses: a chunk
    /// that lost layers gave them back for the next that gains some.
    cells: usize,
    free: Vec<u32>,
}

/// Copies `plane` into atlas cell `cell` of `coverage`.
fn blit(coverage: &mut [u8], cell: usize, plane: &[u8; Terrain::PLANE]) {
    let width = COLUMNS * Terrain::PLANE_SIDE;
    let (cx, cz) = (
        cell % COLUMNS * Terrain::PLANE_SIDE,
        cell / COLUMNS * Terrain::PLANE_SIDE,
    );
    for z in 0..Terrain::PLANE_SIDE {
        let at = (cz + z) * width + cx;
        coverage[at..at + Terrain::PLANE_SIDE]
            .copy_from_slice(&plane[z * Terrain::PLANE_SIDE..][..Terrain::PLANE_SIDE]);
    }
}

/// Whether atlas cell `cell` of `coverage` already holds `plane`.
fn same_plane(coverage: &[u8], cell: usize, plane: &[u8; Terrain::PLANE]) -> bool {
    let width = COLUMNS * Terrain::PLANE_SIDE;
    let (cx, cz) = (
        cell % COLUMNS * Terrain::PLANE_SIDE,
        cell / COLUMNS * Terrain::PLANE_SIDE,
    );
    (0..Terrain::PLANE_SIDE).all(|z| {
        let at = (cz + z) * width + cx;
        coverage[at..at + Terrain::PLANE_SIDE]
            == plane[z * Terrain::PLANE_SIDE..][..Terrain::PLANE_SIDE]
    })
}

impl LayerPaint {
    /// `terrain`'s paint, ranked by `registry`'s `SplatID`s. Layers the
    /// registry or `textures` lack, and scatter layers, are left out.
    pub fn read(terrain: &Terrain, registry: &GroundTextures, textures: &LayerTextures) -> Self {
        let (w, h) = terrain.size();
        Self::from_chunks([w, h], textures, |k| {
            (terrain.ids(k).iter().enumerate())
                .filter_map(|(i, &id)| Some((registry.rank(id)?, id, terrain.plane(k, i))))
                .collect()
        })
    }

    /// The core island's 2070 paint, ranked by its registry.
    pub fn of(paint: &anno_core::island::Layers, textures: &LayerTextures) -> Self {
        Self::from_chunks(paint.chunks(), textures, |k| Self::ranked(paint, k))
    }

    // Chunk `k`'s registered layers as (rank, id, plane), in file order.
    //
    // TODO: the game's lower TERRAIN settings remap layers here
    // (0x140831130): an id whose `MappingProfile` is above the setting
    // becomes its `MapToGUID`, up to two hops, and planes landing on one id
    // add, saturating (0x140831420). 32767 names no entry, so those layers
    // are dropped. At Medium (1) the profile-2 ids go: 9, 21, 27 and 77
    // dropped; 29, 72 and 76 to 52; 61 to 5; 79 to 78; 81 to 86; 84 to 82;
    // 93 to 0; 108 to 15. At Low (0) the profile-1 ids go too: 41 and 74
    // dropped; 18 and 71 to 52; 15 (and so 108) to 10; 87 to 86. Not
    // modelled: the editor has no TERRAIN setting to key it on, and it
    // draws High.
    fn ranked(
        paint: &anno_core::island::Layers,
        k: usize,
    ) -> Vec<(u16, u32, &[u8; Terrain::PLANE])> {
        let registry = paint.registry();
        (paint.ids(k).iter().enumerate())
            .filter_map(|(l, &id)| Some((registry.rank(id)?, u32::from(id), paint.plane(k, l))))
            .collect()
    }

    /// Lays `paint`'s chunks that reach into `area` (island metres x0, z0,
    /// x1, z1) out again, in place. The others keep their entries and
    /// cells; a changed chunk reuses its own cells first, then free ones,
    /// then new ones past the end, and the atlas never shrinks. Pushes to
    /// `changed` each cell whose bytes the change rewrote. Paint of another
    /// chunk count is laid out afresh: then it returns false, and every
    /// cell counts as changed.
    pub fn update(
        &mut self,
        paint: &anno_core::island::Layers,
        textures: &LayerTextures,
        area: [f32; 4],
        changed: &mut Vec<u32>,
    ) -> bool {
        let [w, h] = paint.chunks();
        if self.chunks != [w as u32, h as u32] {
            *self = Self::of(paint, textures);
            return false;
        }
        let side = Terrain::CHUNK as f32;
        let touched = |k: usize| {
            let (x0, z0) = ((k % w) as f32 * side, (k / w) as f32 * side);
            x0 <= area[2] && area[0] <= x0 + side && z0 <= area[3] && area[1] <= z0 + side
        };
        let old = std::mem::take(&mut self.entries);
        let mut writes = Vec::new();
        for k in 0..w * h {
            let [first, n] = self.spans[k];
            let had = &old[first as usize..(first + n) as usize];
            let start = self.entries.len() as u32;
            if touched(k) {
                let mut chunk = Self::ranked(paint, k);
                chunk.sort_by_key(|&(rank, _, _)| rank);
                let mut own = had.iter().map(|e| e >> 8);
                for (_, id, plane) in chunk {
                    let Some(layer) = textures.layer(id) else {
                        continue;
                    };
                    let cell = own.next().or_else(|| self.free.pop()).unwrap_or_else(|| {
                        self.cells += 1;
                        self.cells as u32 - 1
                    });
                    self.entries.push(u32::from(layer) | cell << 8);
                    writes.push((cell, plane));
                }
                self.free.extend(own);
            } else {
                self.entries.extend_from_slice(had);
            }
            self.spans[k] = [start, self.entries.len() as u32 - start];
        }
        self.rows = self.cells.div_ceil(COLUMNS).max(self.rows);
        self.coverage.resize(
            COLUMNS * Terrain::PLANE_SIDE * self.rows * Terrain::PLANE_SIDE,
            0,
        );
        for (cell, plane) in writes {
            if !same_plane(&self.coverage, cell as usize, plane) {
                blit(&mut self.coverage, cell as usize, plane);
                changed.push(cell);
            }
        }
        true
    }

    /// Paint from each chunk's `(rank, id, plane)` layers, `chunks` across
    /// and down.
    pub fn from_chunks<'a>(
        chunks: [usize; 2],
        textures: &LayerTextures,
        mut layers: impl FnMut(usize) -> Vec<(u16, u32, &'a [u8; Terrain::PLANE])>,
    ) -> Self {
        let count = chunks[0] * chunks[1];
        let mut spans = Vec::with_capacity(count);
        let mut entries = Vec::new();
        let mut planes: Vec<&[u8; Terrain::PLANE]> = Vec::new();
        for k in 0..count {
            let mut chunk = layers(k);
            // Stable: equal ranks keep the file's order, as a stable sort
            // in the engine would.
            chunk.sort_by_key(|&(rank, _, _)| rank);
            let first = entries.len() as u32;
            for (_, id, plane) in chunk {
                let Some(layer) = textures.layer(id) else {
                    continue;
                };
                entries.push(u32::from(layer) | (planes.len() as u32) << 8);
                planes.push(plane);
            }
            spans.push([first, entries.len() as u32 - first]);
        }
        let rows = planes.len().div_ceil(COLUMNS).max(1);
        let mut coverage = vec![0u8; COLUMNS * Terrain::PLANE_SIDE * rows * Terrain::PLANE_SIDE];
        for (cell, plane) in planes.iter().enumerate() {
            blit(&mut coverage, cell, plane);
        }
        Self {
            chunks: chunks.map(|c| c as u32),
            spans,
            entries,
            coverage,
            rows,
            cells: planes.len(),
            free: Vec::new(),
        }
    }
}

/// The island's heights at its 1 m corners as the kit's terrain, its
/// corner at the world origin, and the ground that draws its paint.
pub fn island(
    terrain: &Terrain,
    paint: Arc<LayerPaint>,
    textures: Arc<LayerTextures>,
) -> Result<(gpu::Terrain, gpu::GroundDraw)> {
    let corners = terrain.corners();
    let (width, depth) = corners.size();
    let mut heights = Vec::with_capacity(width * depth);
    for z in 0..depth {
        for x in 0..width {
            heights.push(corners.get(x, z).unwrap_or(EMPTY_FLOOR));
        }
    }
    let grid = Grid {
        width,
        depth,
        spacing: 1.,
        origin: [0.; 2],
    };
    let ground = gpu::Terrain {
        heights: Arc::new(HeightMap::new(&Heightfield::new(grid, heights))?),
        colors: Arc::new(ControlMap::new(
            width,
            depth,
            [255; 4].repeat(width * depth),
        )?),
        color_gain: 1.,
        tints: false,
        material: gpu::Material::Lit,
        texture: None,
    };
    Ok((ground, draw(paint, textures, [0.; 2])))
}

/// The ground drawing `paint` with its corner at world X and Z `origin`.
pub fn draw(
    paint: Arc<LayerPaint>,
    textures: Arc<LayerTextures>,
    origin: [f32; 2],
) -> gpu::GroundDraw {
    draw_live(LivePaint::new(paint), textures, origin)
}

/// The ground drawing whatever paint `live` holds. Keep the returned
/// `GroundDraw` for the island's life: the kit keeps a mounted ground while
/// its factory stays the same, and a new one would remount and rebake every
/// page.
pub fn draw_live(
    live: Arc<LivePaint>,
    textures: Arc<LayerTextures>,
    origin: [f32; 2],
) -> gpu::GroundDraw {
    gpu::GroundDraw::Surface(Arc::new(move |init| {
        Box::new(LayersGround::new(
            init.device,
            live.clone(),
            &textures,
            origin,
        ))
    }))
}

/// Paint a mounted ground follows while an editor changes it. Each change
/// edits the paint in place and names the world area it touched and the
/// atlas cells it rewrote; the ground uploads those at its next prepare and
/// reports the areas since, so the kit rebakes only the pages under them.
pub struct LivePaint(Mutex<Live>);

struct Live {
    /// Held here alone, so a change edits it without a copy.
    paint: Arc<LayerPaint>,
    serial: u64,
    /// (serial, world XZ area) of the latest changes, oldest first.
    edits: VecDeque<(u64, [f32; 4])>,
    /// (serial, atlas cell) each change rewrote, oldest first, at most
    /// `CELLS_KEPT`.
    cells: VecDeque<(u64, u32)>,
    /// The newest serial whose cells the log no longer holds whole: a
    /// ground that uploaded before it sends the whole atlas.
    whole: u64,
    /// Coverage bytes the mounted ground has sent to the GPU.
    uploaded: u64,
}

/// Changes a ground can still name one by one: more dabs than a stroke
/// lays between two frames. Asked about older ones, it names the island.
const EDITS_KEPT: usize = 256;

/// Rewritten cells a ground can still upload one by one: a dab rewrites a
/// few dozen (28 on a shipped island's 7 m dab), so about a hundred dabs.
/// Past it the ground sends the whole atlas once.
const CELLS_KEPT: usize = 4096;

impl LivePaint {
    pub fn new(paint: Arc<LayerPaint>) -> Arc<Self> {
        Arc::new(Self(Mutex::new(Live {
            paint,
            serial: 0,
            edits: VecDeque::new(),
            cells: VecDeque::new(),
            whole: 0,
            uploaded: 0,
        })))
    }

    /// Hands the ground `paint`, changed within `area` (island metres), laid
    /// out in place over the paint it holds ([`LayerPaint::update`]): the
    /// ground then uploads only the cells the change rewrote.
    pub fn update(
        &self,
        paint: &anno_core::island::Layers,
        textures: &LayerTextures,
        area: [f32; 4],
    ) {
        let mut live = self.0.lock().unwrap();
        let mut changed = Vec::new();
        let kept = Arc::make_mut(&mut live.paint).update(paint, textures, area, &mut changed);
        let serial = live.next(area);
        if !kept {
            live.cells.clear();
            live.whole = serial;
        }
        for cell in changed {
            if live.cells.len() == CELLS_KEPT
                && let Some((dropped, _)) = live.cells.pop_front()
            {
                live.whole = live.whole.max(dropped);
            }
            live.cells.push_back((serial, cell));
        }
    }

    /// Coverage bytes the mounted ground has uploaded so far.
    pub fn uploaded(&self) -> u64 {
        self.0.lock().unwrap().uploaded
    }

    /// Hands the ground `paint`, changed from the last within `area`, world
    /// X and Z `[x0, z0, x1, z1]` relative to the ground's origin. It
    /// uploads the whole atlas.
    pub fn set(&self, paint: Arc<LayerPaint>, area: [f32; 4]) {
        let mut live = self.0.lock().unwrap();
        live.paint = paint;
        let serial = live.next(area);
        live.cells.clear();
        live.whole = serial;
    }

    fn current(&self) -> Arc<LayerPaint> {
        self.0.lock().unwrap().paint.clone()
    }
}

impl Live {
    /// The next change's serial, its area logged.
    fn next(&mut self, area: [f32; 4]) -> u64 {
        self.serial += 1;
        if self.edits.len() == EDITS_KEPT {
            self.edits.pop_front();
        }
        self.edits.push_back((self.serial, area));
        self.serial
    }
}

/// What a ground last sent to the GPU: the change it holds, the atlas's
/// rows, the span and entry lists' lengths, and the chunks across and down.
struct Uploaded {
    serial: u64,
    rows: usize,
    lists: [usize; 2],
    chunks: [u32; 2],
}

struct LayersGround {
    layout: wgpu::BindGroupLayout,
    group: wgpu::BindGroup,
    live: Arc<LivePaint>,
    /// What the GPU holds, `None` before the first upload.
    uploaded: Option<Uploaded>,
    origin: [f32; 2],
    info: wgpu::Buffer,
    spans: wgpu::Buffer,
    entries: wgpu::Buffer,
    coverage: wgpu::Texture,
    diffuse: StreamedTexture,
    normals: StreamedTexture,
    streamed: bool,
}

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct LayersInfo {
    origin: [f32; 2],
    chunks: [u32; 2],
    scale: f32,
    columns: u32,
    pad: [u32; 2],
}

fn layout(d: &wgpu::Device) -> wgpu::BindGroupLayout {
    gpu::per_device(d, "anno-render layers ground", || {
        use wgpu::ShaderStages as S;
        let stages = S::VERTEX_FRAGMENT;
        let buffer = |ty| wgpu::BindingType::Buffer {
            ty,
            has_dynamic_offset: false,
            min_binding_size: None,
        };
        let texture = |filterable, dimension| wgpu::BindingType::Texture {
            sample_type: wgpu::TextureSampleType::Float { filterable },
            view_dimension: dimension,
            multisampled: false,
        };
        let storage = buffer(wgpu::BufferBindingType::Storage { read_only: true });
        let types = [
            buffer(wgpu::BufferBindingType::Uniform),
            storage,
            storage,
            texture(false, wgpu::TextureViewDimension::D2),
            texture(true, wgpu::TextureViewDimension::D2Array),
            texture(true, wgpu::TextureViewDimension::D2Array),
        ];
        let entries: Vec<_> = (types.into_iter().enumerate())
            .map(|(binding, ty)| wgpu::BindGroupLayoutEntry {
                binding: binding as u32,
                visibility: stages,
                ty,
                count: None,
            })
            .collect();
        d.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("2070 ground"),
            entries: &entries,
        })
    })
}

impl LayersGround {
    fn new(
        d: &wgpu::Device,
        live: Arc<LivePaint>,
        textures: &LayerTextures,
        origin: [f32; 2],
    ) -> Self {
        let layout = layout(d);
        let paint = live.current();
        let info = LayersInfo {
            origin,
            chunks: paint.chunks,
            scale: textures.scale,
            columns: COLUMNS as u32,
            pad: [0; 2],
        };
        use wgpu::util::DeviceExt;
        let info = d.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("2070 ground info"),
            contents: bytemuck::bytes_of(&info),
            usage: wgpu::BufferUsages::UNIFORM,
        });
        let (spans, entries, coverage) = Self::paint_resources(d, &paint);
        let bc = gpu::bc_supported(d);
        let array = |images: &[Image]| {
            StreamedTexture::new(d, images.to_vec(), true, bc).expect("ground layers of one size")
        };
        let (diffuse, normals) = (array(&textures.diffuse), array(&textures.normals));
        let group = Self::group(
            d, &layout, &info, &spans, &entries, &coverage, &diffuse, &normals,
        );
        Self {
            layout,
            group,
            live,
            uploaded: None,
            origin,
            info,
            spans,
            entries,
            coverage,
            diffuse,
            normals,
            streamed: false,
        }
    }

    // The span and entry buffers, filled, and the coverage atlas, empty,
    // sized for `paint`.
    fn paint_resources(
        d: &wgpu::Device,
        paint: &LayerPaint,
    ) -> (wgpu::Buffer, wgpu::Buffer, wgpu::Texture) {
        let (spans, entries) = Self::lists(d, paint);
        let coverage = d.create_texture(&wgpu::TextureDescriptor {
            label: Some("2070 ground coverage"),
            size: wgpu::Extent3d {
                width: (COLUMNS * Terrain::PLANE_SIDE) as u32,
                height: (paint.rows * Terrain::PLANE_SIDE) as u32,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::R8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        (spans, entries, coverage)
    }

    // The span and entry buffers, filled from `paint`.
    fn lists(d: &wgpu::Device, paint: &LayerPaint) -> (wgpu::Buffer, wgpu::Buffer) {
        use wgpu::util::DeviceExt;
        let buffer = |label, contents: &[u8]| {
            d.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some(label),
                contents,
                usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
            })
        };
        let spans = buffer("2070 ground spans", bytemuck::cast_slice(&paint.spans));
        // A storage binding can't be empty.
        let entries = buffer(
            "2070 ground entries",
            bytemuck::cast_slice(if paint.entries.is_empty() {
                &[0]
            } else {
                &paint.entries
            }),
        );
        (spans, entries)
    }

    // New span, entry and coverage resources sized for `paint`, and the
    // group binding them.
    fn resize(&mut self, d: &wgpu::Device, paint: &LayerPaint) {
        (self.spans, self.entries, self.coverage) = Self::paint_resources(d, paint);
        self.group = Self::group(
            d,
            &self.layout,
            &self.info,
            &self.spans,
            &self.entries,
            &self.coverage,
            &self.diffuse,
            &self.normals,
        );
    }

    // Uploads `paint`'s span and entry lists, a few KB, made again when
    // their lengths changed from `had`'s, then the coverage `cells`. The
    // atlas keeps its size, so its other cells stay valid. Gives the
    // coverage bytes sent.
    fn upload_cells(
        &mut self,
        frame: &mut terrain::GroundPrepare<'_>,
        had: &Uploaded,
        paint: &LayerPaint,
        cells: impl Iterator<Item = u32>,
    ) -> usize {
        if had.lists == [paint.spans.len(), paint.entries.len()] {
            let queue = frame.queue();
            queue.write_buffer(&self.spans, 0, bytemuck::cast_slice(&paint.spans));
            if !paint.entries.is_empty() {
                queue.write_buffer(&self.entries, 0, bytemuck::cast_slice(&paint.entries));
            }
        } else {
            let d = frame.device();
            (self.spans, self.entries) = Self::lists(d, paint);
            self.group = Self::group(
                d,
                &self.layout,
                &self.info,
                &self.spans,
                &self.entries,
                &self.coverage,
                &self.diffuse,
                &self.normals,
            );
        }
        let width = COLUMNS * Terrain::PLANE_SIDE;
        let mut sent = 0;
        for cell in cells {
            let cell = cell as usize;
            let (cx, cz) = (
                cell % COLUMNS * Terrain::PLANE_SIDE,
                cell / COLUMNS * Terrain::PLANE_SIDE,
            );
            frame.queue().write_texture(
                wgpu::TexelCopyTextureInfo {
                    texture: &self.coverage,
                    mip_level: 0,
                    origin: wgpu::Origin3d {
                        x: cx as u32,
                        y: cz as u32,
                        z: 0,
                    },
                    aspect: wgpu::TextureAspect::All,
                },
                &paint.coverage[cz * width + cx..],
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(width as u32),
                    rows_per_image: None,
                },
                wgpu::Extent3d {
                    width: Terrain::PLANE_SIDE as u32,
                    height: Terrain::PLANE_SIDE as u32,
                    depth_or_array_layers: 1,
                },
            );
            sent += Terrain::PLANE;
        }
        sent
    }

    #[allow(clippy::too_many_arguments)]
    fn group(
        d: &wgpu::Device,
        layout: &wgpu::BindGroupLayout,
        info: &wgpu::Buffer,
        spans: &wgpu::Buffer,
        entries: &wgpu::Buffer,
        coverage: &wgpu::Texture,
        diffuse: &StreamedTexture,
        normals: &StreamedTexture,
    ) -> wgpu::BindGroup {
        let entry = |binding, resource| wgpu::BindGroupEntry { binding, resource };
        let coverage = coverage.create_view(&Default::default());
        let (diffuse, normals) = (diffuse.view(), normals.view());
        d.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("2070 ground"),
            layout,
            entries: &[
                entry(0, info.as_entire_binding()),
                entry(1, spans.as_entire_binding()),
                entry(2, entries.as_entire_binding()),
                entry(3, wgpu::BindingResource::TextureView(&coverage)),
                entry(4, wgpu::BindingResource::TextureView(&diffuse)),
                entry(5, wgpu::BindingResource::TextureView(&normals)),
            ],
        })
    }
}

/// Builds the ground's programs ahead of the first 2070 island
/// (`gpu::warm_passes`), under `families`, the set its islands draw with. No
/// relief, and normals read at every distance (`normal_footprint`).
pub fn warm(gpu: &gpu::Warm<'_>, families: &gpu::Families) {
    let variants = [(false, Some(1e6))];
    gpu.ground(
        families,
        include_str!("layers.wgsl"),
        &layout(gpu.device),
        true,
        gpu::Specular::FULL,
        &variants,
    );
}

impl terrain::Ground for LayersGround {
    fn wgsl(&self) -> &str {
        include_str!("layers.wgsl")
    }
    fn layout(&self) -> &wgpu::BindGroupLayout {
        &self.layout
    }
    fn bind_group(&self) -> Option<&wgpu::BindGroup> {
        (self.uploaded.is_some() && self.streamed).then_some(&self.group)
    }
    fn prepare(&mut self, frame: &mut terrain::GroundPrepare<'_>) -> bool {
        // Held through the upload: `write_texture` copies the cells out at
        // once, and an editor's next change waits that long.
        let shared = self.live.clone();
        let mut guard = shared.0.lock().unwrap();
        let live = &mut *guard;
        if self
            .uploaded
            .as_ref()
            .is_none_or(|u| u.serial != live.serial)
        {
            let paint = &*live.paint;
            let sent = match self.uploaded.take() {
                // The same atlas, and the log holds every cell rewritten
                // since: those cells, each once.
                Some(had) if had.rows == paint.rows && live.whole <= had.serial => {
                    let mut cells: Vec<u32> = (live.cells.iter())
                        .filter(|c| c.0 > had.serial)
                        .map(|c| c.1)
                        .collect();
                    cells.sort_unstable();
                    cells.dedup();
                    self.upload_cells(frame, &had, paint, cells.into_iter())
                }
                held => {
                    match held {
                        // The lists; the whole atlas follows.
                        Some(had) if had.rows == paint.rows => {
                            self.upload_cells(frame, &had, paint, std::iter::empty());
                        }
                        // A larger atlas brings its own buffers, sized for it.
                        Some(_) => self.resize(frame.device(), paint),
                        None => {}
                    }
                    let size = self.coverage.size();
                    frame.queue().write_texture(
                        self.coverage.as_image_copy(),
                        &paint.coverage,
                        wgpu::TexelCopyBufferLayout {
                            offset: 0,
                            bytes_per_row: Some(size.width),
                            rows_per_image: None,
                        },
                        size,
                    );
                    paint.coverage.len()
                }
            };
            frame.budget.charge(sent);
            live.uploaded += sent as u64;
            self.uploaded = Some(Uploaded {
                serial: live.serial,
                rows: paint.rows,
                lists: [paint.spans.len(), paint.entries.len()],
                chunks: paint.chunks,
            });
        }
        drop(guard);
        if !self.streamed {
            let queue = frame.queue().clone();
            self.streamed = self.diffuse.stream(&queue, frame.budget)
                && self.normals.stream(&queue, frame.budget);
        }
        self.streamed
    }
    fn edits(&self, since: u64, out: &mut Vec<[f32; 4]>) -> u64 {
        let Some(Uploaded { serial, chunks, .. }) = &self.uploaded else {
            return since;
        };
        let live = self.live.0.lock().unwrap();
        let [x, z] = self.origin;
        let reported = (live.edits.iter()).filter(|e| e.0 > since && e.0 <= *serial);
        // Changes older than those kept: the whole island.
        if live.edits.front().is_none_or(|e| e.0 > since + 1) && *serial > since {
            let [w, h] = chunks.map(|c| (c as usize * Terrain::CHUNK) as f32);
            out.push([x, z, x + w, z + h]);
        } else {
            out.extend(reported.map(|&(_, a)| [x + a[0], z + a[1], x + a[2], z + a[3]]));
        }
        *serial
    }
    // Every layer reads its normal map, near and far.
    fn normal_footprint(&self) -> Option<f32> {
        Some(1e6)
    }
    fn glow(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui_editor_kit::{
        camera::{Camera, Extent},
        gpu::{Light, Offscreen, Renderer, Scene},
        texture::Texture,
    };

    const SIDE: usize = Terrain::PLANE_SIDE;

    fn solid(rgba: [u8; 4]) -> Image {
        Arc::new(Texture::from_rgba(8, 8, rgba.repeat(64)).unwrap()).into()
    }

    /// Flat layers of one colour each; `glow` is the normal map's blue.
    fn textures(layers: &[(u32, [u8; 3], u8)]) -> LayerTextures {
        let layers = (layers.iter())
            .map(|&(id, [r, g, b], glow)| (id, solid([r, g, b, 255]), solid([128, 128, glow, 0])))
            .collect();
        LayerTextures::new(4., layers)
    }

    fn plane(f: impl Fn(usize, usize) -> u8) -> [u8; Terrain::PLANE] {
        std::array::from_fn(|i| f(i % SIDE, i / SIDE))
    }

    /// `paint` on flat ground, `chunks` across and one down, through a
    /// top-down view under a uniform sky, and the colour at each world
    /// X and Z of `at`.
    fn draw_at(
        chunks: usize,
        paint: LayerPaint,
        textures: LayerTextures,
        light: f32,
        at: &[[f32; 2]],
    ) -> Vec<[u8; 3]> {
        let ground = draw(Arc::new(paint), Arc::new(textures), [0.; 2]);
        frames(chunks, ground, light, at, &[]).remove(0)
    }

    /// `ground` drawn as `draw_at` draws, once, then again after each of
    /// `steps`, through the same renderer and scene: a frame each.
    fn frames(
        chunks: usize,
        ground: gpu::GroundDraw,
        light: f32,
        at: &[[f32; 2]],
        steps: &[&dyn Fn()],
    ) -> Vec<Vec<[u8; 3]>> {
        let (width, depth) = (chunks * Terrain::CHUNK + 1, Terrain::CHUNK + 1);
        let grid = Grid {
            width,
            depth,
            spacing: 1.,
            origin: [0.; 2],
        };
        let terrain = gpu::Terrain {
            heights: Arc::new(
                HeightMap::new(&Heightfield::new(grid, vec![0.; width * depth])).unwrap(),
            ),
            colors: Arc::new(
                ControlMap::new(width, depth, [255; 4].repeat(width * depth)).unwrap(),
            ),
            color_gain: 1.,
            tints: false,
            material: gpu::Material::Lit,
            texture: None,
        };
        let scene = Scene {
            terrain: Some(Arc::new(terrain)),
            ground,
            light: Light {
                sun_color: [0.; 3],
                sky: [light; 3],
                ground: [light; 3],
                ..Default::default()
            },
            ..Default::default()
        };
        let (w, h) = (320, 160);
        let mut target = Offscreen::new(w, h).unwrap();
        let renderer = Renderer::default();
        renderer.set_scene(&scene);
        let camera = Camera::default();
        let world = Extent {
            center: [
                (chunks * Terrain::CHUNK) as f32 / 2.,
                0.,
                Terrain::CHUNK as f32 / 2.,
            ],
            size: (chunks * Terrain::CHUNK) as f32,
        };
        let mut out = Vec::new();
        let first: &dyn Fn() = &|| {};
        for step in std::iter::once(&first).chain(steps) {
            step();
            let pixels = target.render(&renderer, camera, world).unwrap();
            assert!(
                renderer.drawn().pages.baked > 0,
                "{:?}",
                renderer.drawn().pages
            );
            out.push(
                at.iter()
                    .map(|&[x, z]| {
                        let p = camera.project([x, 0., z], target.bounds(), world);
                        let (px, py) = (f32::from(p.x) as usize, f32::from(p.y) as usize);
                        let i = (py * w as usize + px) * 4;
                        [pixels[i], pixels[i + 1], pixels[i + 2]]
                    })
                    .collect(),
            );
        }
        out
    }

    /// terrain.fx's BlendSplat with the diffuse alpha at 0.5.
    fn alpha(coverage: f32) -> f32 {
        (5. * (0.5 - 1.) + 6. * coverage).clamp(0., 1.)
    }

    fn near(actual: [u8; 3], expected: [f32; 3], what: &str) {
        // Pages hold BC3 albedo, a few levels off (3 on a flat colour, more
        // across a blend). 12 is under 0.08 of a weight between these
        // colours, 160 apart; plain coverage instead of BlendSplat's cut is
        // about 40 off where these points sample.
        let off = (0..3)
            .map(|c| (f32::from(actual[c]) - expected[c]).abs())
            .fold(0., f32::max);
        assert!(off <= 12., "{what}: {actual:?}, expected {expected:?}");
    }

    const A: [u8; 3] = [200, 40, 40];
    const B: [u8; 3] = [40, 200, 40];
    const C: [u8; 3] = [40, 40, 200];

    // Paint handed to a mounted ground through `LivePaint` draws on the
    // next frame, through the same renderer and ground: a layer laid over
    // chunk 1 shows there, and chunk 0, outside the change, stays.
    #[test]
    fn live_paint_redraws_where_it_changed() {
        let tex = || textures(&[(1, A, 0), (2, B, 0)]);
        let (base, full) = (plane(|_, _| 255), plane(|_, _| 255));
        let bare = LayerPaint::from_chunks([2, 1], &tex(), |_| vec![(10, 1, &base)]);
        let laid = LayerPaint::from_chunks([2, 1], &tex(), |k| {
            let mut layers = vec![(10, 1, &base)];
            if k == 1 {
                layers.push((20, 2, &full));
            }
            layers
        });
        let live = LivePaint::new(Arc::new(bare));
        let ground = draw_live(live.clone(), Arc::new(tex()), [0.; 2]);
        let laid = std::cell::RefCell::new(Some(laid));
        let set = || {
            let paint = laid.borrow_mut().take().unwrap();
            live.set(Arc::new(paint), [16., 0., 32., 16.]);
        };
        let got = frames(2, ground, 1., &[[8., 8.], [24., 8.]], &[&set]);
        near(got[0][1], A.map(f32::from), "before");
        near(got[1][0], A.map(f32::from), "outside the change");
        near(got[1][1], B.map(f32::from), "the laid layer");
    }

    // A dab handed over through `LivePaint::update` uploads only the
    // coverage of the chunk it changed: the one plane it added, 289 bytes,
    // where the first upload sent the whole atlas. The frame after it
    // matches, at the middle of every metre, the same paint uploaded whole
    // to a fresh ground.
    #[test]
    fn a_dab_uploads_only_its_chunks_cells_and_draws_as_a_full_upload() {
        use anno_core::island::{Layers, Registry};
        let xml: String = [(0, 0), (1, 5), (2, 9)]
            .map(|(id, rank)| {
                format!(
                    "<Texture><GUID>{id}</GUID><SplatID>{rank}</SplatID>\
                     <DiffuseTexture>t{id}</DiffuseTexture></Texture>"
                )
            })
            .concat();
        let registry = Registry::new(GroundTextures::parse(&xml).unwrap(), 4.).unwrap();
        let mut before = Layers::new([4, 1], Arc::new(registry));
        for k in 0..4 {
            let planes = [plane(|_, _| 255), plane(|_, _| 140)].concat();
            before.set(k, &[0, 1], &planes).unwrap();
        }
        // Id 2 dabbed in chunk 1's middle, which no other chunk shares.
        let mut after = before.clone();
        for z in 5..=11 {
            for x in 21..=27 {
                after.paint(x, z, 2, 1.).unwrap();
            }
        }
        let tex = || textures(&[(0, A, 0), (1, B, 0), (2, C, 0)]);
        let atlas = LayerPaint::of(&before, &tex()).coverage.len() as u64;
        let textures = Arc::new(tex());
        let live = LivePaint::new(Arc::new(LayerPaint::of(&before, &textures)));
        let ground = draw_live(live.clone(), textures.clone(), [0.; 2]);
        let at: Vec<[f32; 2]> = (0..16 * 64)
            .map(|i| [(i % 64) as f32 + 0.5, (i / 64) as f32 + 0.5])
            .collect();
        let first = std::cell::Cell::new(0);
        let dab = || {
            first.set(live.uploaded());
            live.update(&after, &textures, [21., 5., 27., 11.]);
        };
        let got = frames(4, ground, 1., &at, &[&dab]);
        let dabbed = live.uploaded() - first.get();
        eprintln!("first upload {} bytes, the dab {dabbed}", first.get());
        assert_eq!(first.get(), atlas);
        assert_eq!(dabbed, Terrain::PLANE as u64);
        let whole = draw_at(4, LayerPaint::of(&after, &tex()), tex(), 1., &at);
        assert!(got[1] != got[0], "the dab shows");
        for ((p, a), b) in at.iter().zip(&got[1]).zip(&whole) {
            assert_eq!(a, b, "at {p:?}");
        }
    }

    // Laid out again, a chunk that gains a layer takes a free cell before a
    // new one, and keeps the cells it had; the others keep theirs.
    #[test]
    fn a_relaid_chunk_keeps_its_cells_and_reuses_freed_ones() {
        use anno_core::island::{Layers, Registry};
        let xml: String = [(0, 0), (1, 5), (2, 9)]
            .map(|(id, rank)| {
                format!(
                    "<Texture><GUID>{id}</GUID><SplatID>{rank}</SplatID>\
                     <DiffuseTexture>t{id}</DiffuseTexture></Texture>"
                )
            })
            .concat();
        let registry = Arc::new(Registry::new(GroundTextures::parse(&xml).unwrap(), 4.).unwrap());
        let tex = textures(&[(0, A, 0), (1, B, 0), (2, C, 0)]);
        let full = [plane(|_, _| 255), plane(|_, _| 140)].concat();
        let mut two = Layers::new([2, 1], registry.clone());
        two.set(0, &[0, 1], &full).unwrap();
        two.set(1, &[0, 1], &full).unwrap();
        let mut one = two.clone();
        one.set(0, &[0], &full[..Terrain::PLANE]).unwrap();
        let mut three = one.clone();
        three
            .set(
                1,
                &[0, 1, 2],
                &[full.as_slice(), &plane(|_, _| 200)].concat(),
            )
            .unwrap();
        let cells = |p: &LayerPaint, k: usize| -> Vec<u32> {
            let [first, n] = p.spans[k];
            p.entries[first as usize..(first + n) as usize]
                .iter()
                .map(|e| e >> 8)
                .collect()
        };
        let a = LayerPaint::of(&two, &tex);
        let mut b = a.clone();
        assert!(b.update(&one, &tex, [0., 0., 15., 16.], &mut Vec::new()));
        assert_eq!(cells(&b, 0), [0]);
        assert_eq!(cells(&b, 1), cells(&a, 1));
        let mut c = b.clone();
        assert!(c.update(&three, &tex, [17., 0., 32., 16.], &mut Vec::new()));
        assert_eq!(cells(&c, 0), [0]);
        assert_eq!(
            cells(&c, 1),
            [2, 3, 1],
            "its own two, then chunk 0's freed one"
        );
        assert_eq!((c.cells, c.rows), (4, 1));
    }

    #[test]
    fn the_base_layer_is_opaque_at_zero_coverage() {
        let empty = plane(|_, _| 0);
        let full = plane(|_, _| 255);
        // Chunk 0: the base's plane all zero, nothing over it. Chunk 1: a
        // full layer over the same empty base.
        let paint = LayerPaint::from_chunks([2, 1], &textures(&[(1, A, 0), (2, B, 0)]), |k| {
            let mut layers = vec![(10, 1, &empty)];
            if k == 1 {
                layers.push((20, 2, &full));
            }
            layers
        });
        let tex = textures(&[(1, A, 0), (2, B, 0)]);
        let got = draw_at(2, paint, tex, 1., &[[8., 8.], [24., 8.]]);
        near(got[0], A.map(f32::from), "bare base");
        near(got[1], B.map(f32::from), "full layer over it");
    }

    #[test]
    fn layer_weights_match_on_both_sides_of_a_chunk_seam() {
        // Neighbours with different layer sets agree on their shared edge:
        // B fades out toward it in chunk 0, C in from it in chunk 1, both 0
        // on it, as a missing plane reads. The ramps are gentle where
        // sampled: a pixel's 5 cm off its point moves the cut little.
        fn b_at(x: usize) -> u8 {
            if x >= 16 {
                0
            } else {
                (110 + 10 * (15 - x)).min(255) as u8
            }
        }
        fn c_at(x: usize) -> u8 {
            if x == 0 {
                0
            } else {
                (110 + 10 * (x - 1)).min(255) as u8
            }
        }
        let base = plane(|_, _| 255);
        let b = plane(|x, _| b_at(x));
        let c = plane(|x, _| c_at(x));
        let layers = [(1, A, 0), (2, B, 0), (3, C, 0)];
        let paint = LayerPaint::from_chunks([2, 1], &textures(&layers), |k| match k {
            0 => vec![(20, 2, &b), (10, 1, &base)],
            _ => vec![(10, 1, &base), (30, 3, &c)],
        });
        let at = [
            [13.5, 8.5],
            [14.5, 8.5],
            [15.5, 8.5],
            [16.5, 8.5],
            [17.5, 8.5],
            [18.5, 8.5],
        ];
        let got = draw_at(2, paint, textures(&layers), 1., &at);
        for (&[x, _], got) in at.iter().zip(got) {
            // Coverage between the metre vertices on either side, by hand.
            let (over, local, value): (_, f32, fn(usize) -> u8) = if x < 16. {
                (B, x, b_at)
            } else {
                (C, x - 16., c_at)
            };
            let v = local.floor() as usize;
            let f = local - v as f32;
            let cover = (f32::from(value(v)) * (1. - f) + f32::from(value(v + 1)) * f) / 255.;
            let a = alpha(cover);
            let expected = [0, 1, 2].map(|i| f32::from(A[i]) * (1. - a) + f32::from(over[i]) * a);
            near(got, expected, &format!("x {x}"));
        }
    }

    #[test]
    fn glow_comes_from_the_normal_maps_blue() {
        let full = plane(|_, _| 255);
        let layers = [(1, A, 0), (2, B, 255)];
        let paint = LayerPaint::from_chunks([2, 1], &textures(&layers), |k| {
            let mut l = vec![(10, 1, &full)];
            if k == 1 {
                l.push((20, 2, &full));
            }
            l
        });
        // Dim light: the glowing layer shows its own colour at full
        // strength, the matte base a fifth of its.
        let got = draw_at(2, paint, textures(&layers), 0.2, &[[8., 8.], [24., 8.]]);
        assert!(got[0][0] < 100, "matte base {:?}", got[0]);
        near(got[1], B.map(f32::from), "glowing layer");
    }

    // Needs Anno 2070 (ANNO2070_DIR).
    fn install() -> Option<(Install, GroundTextures, String)> {
        let dir = anno_formats::vfs::install_dir("ANNO2070_DIR")?;
        // What ground textures and islands need of anno2070's archive list.
        let install = Install::open(&dir, &["data", "patch", "addon", "exedata"]).unwrap();
        let xml = install
            .read("data/config/engine/groundtextures.xml")
            .unwrap();
        let xml = String::from_utf8_lossy(&xml).into_owned();
        Some((install, GroundTextures::parse(&xml).unwrap(), xml))
    }

    /// A colour of its own per registry id.
    fn colour(id: u32) -> [u8; 3] {
        [id * 67 % 200 + 30, id * 131 % 200 + 30, id * 29 % 200 + 30].map(|v| v as u8)
    }

    /// The chunk's colour layers at vertex `v` composited in `order`'s
    /// sort, base first, by hand from the stored planes.
    fn composite(layers: &[(u16, u32, u8)], order: impl Fn(&(u16, u32, u8)) -> u32) -> [f32; 3] {
        let mut sorted = layers.to_vec();
        sorted.sort_by_key(|l| order(l));
        let mut out = [0f32; 3];
        for (i, &(_, id, cov)) in sorted.iter().enumerate() {
            let a = if i == 0 {
                1.
            } else {
                alpha(f32::from(cov) / 255.)
            };
            for c in 0..3 {
                out[c] = out[c] * (1. - a) + f32::from(colour(id)[c]) * a;
            }
        }
        out
    }

    #[test]
    fn beach_draws_under_rock_by_splat_id_where_guids_say_otherwise() {
        let Some((install, registry, _)) = install() else {
            return;
        };
        const BEACH_03: u32 = 83;
        // mountain_00, _01, _03 and _06: GUIDs below beach_03's, ranks above.
        let rocks = [74, 80, 81, 82];
        assert!(
            rocks
                .iter()
                .all(|&r| r < BEACH_03 && registry.rank(r) > registry.rank(BEACH_03))
        );
        let mut paths: Vec<String> = (install.search(".isd", 10_000).into_iter())
            .map(|id| install.file_path(id).to_owned())
            .filter(|p| p.starts_with("data/levels/islands/normal/") && p.ends_with(".isd"))
            .collect();
        paths.sort();
        // A vertex where beach_03 and a rock both cover fully and every
        // layer holds the same coverage on the vertices around it, so the
        // pixel over it reads the vertex's own values.
        let mut found = None;
        'files: for path in &paths {
            let bytes = install.read(path).unwrap();
            let doc = anno_formats::isd::Doc::parse(&bytes).unwrap();
            let terrain = Terrain::read(&doc, path).unwrap();
            let (w, h) = terrain.size();
            for k in 0..w * h {
                let ids = terrain.ids(k);
                let Some(beach) = ids.iter().position(|&i| i == BEACH_03) else {
                    continue;
                };
                for z in 1..SIDE - 1 {
                    for x in 1..SIDE - 1 {
                        let at = |i: usize, dx: usize, dz: usize| {
                            terrain.plane(k, i)[(z + dz - 1) * SIDE + x + dx - 1]
                        };
                        let flat = (0..ids.len())
                            .all(|i| (0..9).all(|n| at(i, n % 3, n / 3) == at(i, 1, 1)));
                        let rock =
                            (0..ids.len()).find(|&i| rocks.contains(&ids[i]) && at(i, 1, 1) >= 160);
                        if flat && at(beach, 1, 1) >= 160 && rock.is_some() {
                            let layers: Vec<(u16, u32, u8)> = (ids.iter().enumerate())
                                .filter(|&(_, &id)| {
                                    registry
                                        .textures()
                                        .iter()
                                        .any(|t| t.id == id && t.diffuse.is_some())
                                })
                                .map(|(i, &id)| (registry.rank(id).unwrap(), id, at(i, 1, 1)))
                                .collect();
                            let (cx, cz) = (k % w * Terrain::CHUNK + x, k / w * Terrain::CHUNK + z);
                            let height = terrain.corners().get(cx, cz).unwrap();
                            found = Some((
                                path.clone(),
                                bytes.clone(),
                                layers,
                                [cx as f32, height, cz as f32],
                            ));
                            break 'files;
                        }
                    }
                }
            }
        }
        let (path, bytes, layers, at) = found.expect("a coast vertex with beach_03 over rock");
        let by_rank = composite(&layers, |l| u32::from(l.0));
        let by_guid = composite(&layers, |l| l.1);
        assert!(
            (0..3).any(|c| (by_rank[c] - by_guid[c]).abs() > 40.),
            "{path}: the orders agree at {at:?}"
        );
        let doc = anno_formats::isd::Doc::parse(&bytes).unwrap();
        let terrain = Terrain::read(&doc, &path).unwrap();
        let ids: Vec<(u32, [u8; 3], u8)> = (registry.textures().iter())
            .filter(|t| t.diffuse.is_some())
            .map(|t| (t.id, colour(t.id), 0))
            .collect();
        let paint = Arc::new(LayerPaint::read(&terrain, &registry, &textures(&ids)));
        let (ground, draw) = island(&terrain, paint, Arc::new(textures(&ids))).unwrap();
        let scene = Scene {
            terrain: Some(Arc::new(ground)),
            ground: draw,
            light: Light {
                sun_color: [0.; 3],
                sky: [1.; 3],
                ground: [1.; 3],
                ..Default::default()
            },
            ..Default::default()
        };
        let mut target = Offscreen::new(160, 160).unwrap();
        let renderer = Renderer::default();
        renderer.set_scene(&scene);
        let camera = Camera::default();
        let world = Extent {
            center: at,
            size: 4.,
        };
        let pixels = target.render(&renderer, camera, world).unwrap();
        let p = camera.project(at, target.bounds(), world);
        let i = ((f32::from(p.y) as usize) * 160 + f32::from(p.x) as usize) * 4;
        let got = [pixels[i], pixels[i + 1], pixels[i + 2]];
        near(
            got,
            by_rank,
            &format!("{path} at {at:?}, GUID order gives {by_guid:?}"),
        );
    }

    // Needs Anno 2070: on n_m08 a 3 m dab of earth where four chunks meet
    // changes only cells of those chunks once laid out by `update`: 28
    // cells, 8 KB, that a mounted ground uploads, where each change sent
    // the whole 307 KB atlas before. Recorded 2026-11-19.
    #[test]
    fn a_dab_on_a_shipped_island_uploads_a_few_cells() {
        let Some((install, _, xml)) = install() else {
            return;
        };
        let path = "data/levels/islands/normal/n_m08.isd";
        let doc = anno_formats::isd::Doc::parse(&install.read(path).unwrap()).unwrap();
        let terrain = Terrain::read(&doc, path).unwrap();
        let ground = GroundTextures::parse(&xml).unwrap();
        let earth = (ground.textures().iter())
            .find(|t| t.name.contains("earth") && t.diffuse.is_some())
            .map(|t| t.id as u8)
            .unwrap();
        let registry = Arc::new(anno_core::island::Registry::new(ground, 4.).unwrap());
        let textures = LayerTextures::load(&install, registry.textures(), 16).unwrap();
        let before = anno_core::island::Layers::read(&terrain, registry).unwrap();
        let [w, h] = before.chunks();
        let (cx, cz) = (w / 2 * Terrain::CHUNK, h / 2 * Terrain::CHUNK);
        let mut after = before.clone();
        for z in cz - 3..=cz + 3 {
            for x in cx - 3..=cx + 3 {
                let d = ((x as f32 - cx as f32).hypot(z as f32 - cz as f32) / 3.).min(1.);
                let _ = after.paint(x, z, earth, 1. - d);
            }
        }
        let old = LayerPaint::of(&before, &textures);
        let area = [cx, cz, cx, cz].map(|v| v as f32);
        let mut new = old.clone();
        let mut changed = Vec::new();
        assert!(new.update(
            &after,
            &textures,
            [area[0] - 3., area[1] - 3., area[2] + 3., area[3] + 3.],
            &mut changed,
        ));
        assert_eq!(old.rows, new.rows);
        // The cells it names are exactly those whose bytes differ.
        let width = COLUMNS * Terrain::PLANE_SIDE;
        let differs = |c: usize| {
            let (x0, z0) = (
                c % COLUMNS * Terrain::PLANE_SIDE,
                c / COLUMNS * Terrain::PLANE_SIDE,
            );
            (0..Terrain::PLANE_SIDE).any(|z| {
                let at = (z0 + z) * width + x0;
                old.coverage[at..at + Terrain::PLANE_SIDE]
                    != new.coverage[at..at + Terrain::PLANE_SIDE]
            })
        };
        let diffs: Vec<u32> = (0..new.rows * COLUMNS)
            .filter(|&c| differs(c))
            .map(|c| c as u32)
            .collect();
        changed.sort_unstable();
        assert_eq!(changed, diffs);
        let cells = changed.len();
        eprintln!(
            "n_m08: a dab uploads {} bytes ({cells} cells) of the {}-byte atlas",
            cells * Terrain::PLANE,
            new.coverage.len()
        );
        assert_eq!(cells, 28);
        // The same pixels as the paint laid out afresh: each chunk's
        // layers hold the same planes in the same order.
        let fresh = LayerPaint::of(&after, &textures);
        for k in 0..w * h {
            let planes = |p: &LayerPaint| -> Vec<(u32, Vec<u8>)> {
                let [first, n] = p.spans[k];
                (p.entries[first as usize..(first + n) as usize].iter())
                    .map(|&e| {
                        let cell = (e >> 8) as usize;
                        let mut plane = vec![0; Terrain::PLANE];
                        let (x0, z0) = (
                            cell % COLUMNS * Terrain::PLANE_SIDE,
                            cell / COLUMNS * Terrain::PLANE_SIDE,
                        );
                        for z in 0..Terrain::PLANE_SIDE {
                            let at = (z0 + z) * COLUMNS * Terrain::PLANE_SIDE + x0;
                            plane[z * Terrain::PLANE_SIDE..][..Terrain::PLANE_SIDE]
                                .copy_from_slice(&p.coverage[at..at + Terrain::PLANE_SIDE]);
                        }
                        (e & 255, plane)
                    })
                    .collect()
            };
            assert_eq!(planes(&new), planes(&fresh), "chunk {k}");
        }
    }

    // Needs Anno 2070: every layer any shipped island paints resolves to a
    // texture the install holds and reaches the GPU, scatter layers aside
    // (they place grass objects). Nothing falls back or drops on the way.
    // The developers' magenta stand-in (ids 9, 11, 35, 88) counts as
    // resolved, since the game draws it too; it's listed apart.
    #[test]
    fn every_layer_a_shipped_island_paints_draws_a_shipped_texture() {
        let Some((install, registry, _)) = install() else {
            return;
        };
        // Reads every registry texture and fails on one the install lacks.
        let textures = LayerTextures::load(&install, &registry, 4).unwrap();
        let mut islands = install.search(".isd", 100_000);
        islands.retain(|&f| install.file_path(f).ends_with(".isd"));
        assert!(islands.len() >= 384, "{} islands", islands.len());
        let mut failures = Vec::new();
        // Per placeholder id, the islands and chunk planes painting it.
        let mut placeholders = std::collections::BTreeMap::<u32, (usize, usize)>::new();
        for &file in &islands {
            let path = install.file_path(file).to_owned();
            let doc = anno_formats::isd::Doc::parse(&install.read_file(file).unwrap()).unwrap();
            let terrain = Terrain::read(&doc, &path).unwrap();
            let paint = LayerPaint::read(&terrain, &registry, &textures);
            let (w, h) = terrain.size();
            let mut seen = std::collections::BTreeSet::new();
            for k in 0..w * h {
                let mut drawn = 0;
                for &id in terrain.ids(k) {
                    if registry.rank(id).is_none() {
                        failures.push(format!("{path} chunk {k}: id {id} isn't registered"));
                    } else if registry.scatter(id) {
                        continue;
                    } else if textures.layer(id).is_none() {
                        failures.push(format!("{path} chunk {k}: id {id} has no texture"));
                    } else {
                        drawn += 1;
                        if registry.placeholder(id) {
                            placeholders.entry(id).or_default().1 += 1;
                            seen.insert(id);
                        }
                    }
                }
                let uploaded = paint.spans[k][1] as usize;
                if uploaded != drawn {
                    failures.push(format!(
                        "{path} chunk {k}: {uploaded} of {drawn} layers uploaded"
                    ));
                }
            }
            for id in seen {
                placeholders.entry(id).or_default().0 += 1;
            }
        }
        assert!(failures.is_empty(), "{}", failures.join("\n"));
        for (id, (n, planes)) in &placeholders {
            eprintln!("placeholder id {id}: {n} islands, {planes} chunk planes");
        }
        // The four stand-in ids the shipped registry has, all painted.
        let ids: Vec<u32> = placeholders.keys().copied().collect();
        assert_eq!(ids, [9, 11, 35, 88]);
        let all: Vec<u32> = (registry.textures().iter())
            .map(|t| t.id)
            .filter(|&id| registry.placeholder(id))
            .collect();
        assert_eq!(all, [9, 88, 11, 35], "in file order");
    }

    #[test]
    fn a_shipped_island_draws_through_the_read_only_ground() {
        let Some((install, registry, _)) = install() else {
            return;
        };
        let path = "data/levels/islands/normal/n_m08.isd";
        let doc = anno_formats::isd::Doc::parse(&install.read(path).unwrap()).unwrap();
        let terrain = Terrain::read(&doc, path).unwrap();
        let textures = Arc::new(LayerTextures::load(&install, &registry, 256).unwrap());
        assert_eq!(textures.scale, 4.);
        let paint = Arc::new(LayerPaint::read(&terrain, &registry, &textures));
        let (ground, draw) = island(&terrain, paint, textures).unwrap();
        let (w, h) = terrain.size();
        let size = (w.max(h) * Terrain::CHUNK) as f32;
        let scene = Scene {
            terrain: Some(Arc::new(ground)),
            ground: draw,
            light: Light::default(),
            ..Default::default()
        };
        let mut target = Offscreen::new(320, 320).unwrap();
        let renderer = Renderer::default();
        renderer.set_scene(&scene);
        let world = Extent {
            center: [size / 2., 0., size / 2.],
            size,
        };
        let pixels = target.render(&renderer, Camera::default(), world).unwrap();
        let pages = renderer.drawn().pages;
        assert!(pages.baked > 0 && pages.baked == pages.wanted, "{pages:?}");
        // Ground, not one flat colour: many distinct pixels.
        let mut seen: Vec<[u8; 3]> = pixels.chunks(4).map(|p| [p[0], p[1], p[2]]).collect();
        seen.sort();
        seen.dedup();
        assert!(seen.len() > 1000, "{} colours", seen.len());
    }
}
