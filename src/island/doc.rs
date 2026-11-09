// Owns the island document: heights, level-0 material maps in copy-on-write
// chunks, props, and the conversion to and from the game's island files.
use super::Item;
use anno_formats::{grids::Grids, objects::Object, session::SessionSettings, tint::TintMap};
use anno_formats::{
    island::{self, Island},
    vfs::Install,
};
use anyhow::{Context as _, Result, ensure};
use gpui_editor_kit::{
    heightfield::{Brush, Grid, Heightfield, Rect},
    math::{self, Mat4},
};
use std::sync::Arc;

#[derive(Clone, Debug)]
pub struct Prop {
    pub id: u64,
    pub matrix: Mat4,
    /// The stored record: mesh file index, tint color and flags ride along.
    pub source: island::Prop,
    /// Placed by Auto cliffs, which replaces its own pieces on a rerun.
    /// The game file has no room for it; project islands keep the list in
    /// their listing (`Listing::auto_cliffs`).
    pub auto_cliff: bool,
}

/// Material map level 0 in copy-on-write chunks. Islands carry 2560² alpha
/// words at 320 m (26 MB); whole-map undo snapshots would cost a gigabyte
/// over the history depth, so a stroke copies only the chunks it touches.
#[derive(Clone, PartialEq)]
pub struct Layer {
    width: usize,
    height: usize,
    chunks: Vec<Arc<Vec<u32>>>,
}

/// Chunk side in words.
const CHUNK: usize = 256;

/// A DX10 DDS header, which the material map words follow.
const DDS_HEADER: usize = 148;

impl Layer {
    pub(super) fn new(grid: &island::Grid<u32>) -> Self {
        let (cw, ch) = (grid.width.div_ceil(CHUNK), grid.height.div_ceil(CHUNK));
        let chunk = |i: usize| {
            let (x0, y0) = ((i % cw) * CHUNK, (i / cw) * CHUNK);
            let mut chunk = vec![0; CHUNK * CHUNK];
            for y in y0..(y0 + CHUNK).min(grid.height) {
                let row =
                    &grid.data[y * grid.width + x0..y * grid.width + (x0 + CHUNK).min(grid.width)];
                chunk[(y - y0) * CHUNK..][..row.len()].copy_from_slice(row);
            }
            Arc::new(chunk)
        };
        // Rows of chunks on all cores: one copied the continental island's
        // 144 MB alpha map in 70 ms.
        let threads = std::thread::available_parallelism().map_or(4, |n| n.get());
        let per = (cw * ch).div_ceil(threads).max(1);
        let chunks = std::thread::scope(|scope| {
            let parts: Vec<_> = (0..cw * ch)
                .step_by(per)
                .map(|start| {
                    let chunk = &chunk;
                    scope.spawn(move || {
                        (start..(start + per).min(cw * ch))
                            .map(chunk)
                            .collect::<Vec<_>>()
                    })
                })
                .collect();
            parts
                .into_iter()
                .flat_map(|p| p.join().expect("material chunks"))
                .collect()
        });
        Self {
            width: grid.width,
            height: grid.height,
            chunks,
        }
    }

    fn slot(&self, x: usize, y: usize) -> (usize, usize) {
        let cw = self.width.div_ceil(CHUNK);
        (
            (y / CHUNK) * cw + x / CHUNK,
            (y % CHUNK) * CHUNK + x % CHUNK,
        )
    }

    pub(super) fn size(&self) -> [usize; 2] {
        [self.width, self.height]
    }

    pub(super) fn get(&self, x: usize, y: usize) -> u32 {
        let (c, i) = self.slot(x, y);
        self.chunks[c][i]
    }

    pub(super) fn set(&mut self, x: usize, y: usize, v: u32) {
        let (c, i) = self.slot(x, y);
        if self.chunks[c][i] != v {
            Arc::make_mut(&mut self.chunks[c])[i] = v;
        }
    }

    /// Copies a clipped rectangle out for the format's paint routine, a
    /// chunk-row run at a time.
    pub(super) fn region(&self, x0: usize, y0: usize, x1: usize, y1: usize) -> island::Grid<u32> {
        let (x1, y1) = (x1.min(self.width), y1.min(self.height));
        let mut data = Vec::with_capacity((x1 - x0) * (y1 - y0));
        for y in y0..y1 {
            let mut x = x0;
            while x < x1 {
                let (c, i) = self.slot(x, y);
                let run = (CHUNK - x % CHUNK).min(x1 - x);
                data.extend_from_slice(&self.chunks[c][i..i + run]);
                x += run;
            }
        }
        island::Grid {
            width: x1 - x0,
            height: y1 - y0,
            data,
        }
    }

    /// Writes `grid` at (x0, y0), copying only chunks whose words change.
    /// Returns whether any did.
    pub(super) fn write(&mut self, x0: usize, y0: usize, grid: &island::Grid<u32>) -> bool {
        let mut changed = false;
        for y in 0..grid.height {
            let row = &grid.data[y * grid.width..(y + 1) * grid.width];
            let mut x = 0;
            while x < grid.width {
                let (c, i) = self.slot(x0 + x, y0 + y);
                let run = (CHUNK - (x0 + x) % CHUNK).min(grid.width - x);
                let new = &row[x..x + run];
                if self.chunks[c][i..i + run] != *new {
                    Arc::make_mut(&mut self.chunks[c])[i..i + run].copy_from_slice(new);
                    changed = true;
                }
                x += run;
            }
        }
        changed
    }

    pub(super) fn grid(&self) -> island::Grid<u32> {
        self.region(0, 0, self.width, self.height)
    }
}

impl crate::terrain::MaterialWords for Layer {
    fn size(&self) -> [usize; 2] {
        [self.width, self.height]
    }

    fn word(&self, x: usize, y: usize) -> u32 {
        self.get(x, y)
    }
}

/// Everything an undo step restores. Heights are metres. Materials are the
/// game's level-0 maps when the island ships them: one id block per 2x2
/// alpha texels, `per_metre` alpha texels per metre.
#[derive(Clone)]
pub struct Doc {
    /// Copy-on-write (`Arc::make_mut` to edit): undo steps that leave the
    /// heights alone share them, and a stroke copies them once.
    pub field: Arc<Heightfield>,
    /// Heights last used to synchronize gameplay grids. Undo keeps this
    /// snapshot with the grids, so manual painting survives later saves.
    /// After a sync it is `field` itself, shared.
    pub(super) land_heights: Arc<Heightfield>,
    pub materials: Option<Materials>,
    pub meshes: Arc<Vec<String>>,
    /// Props and objects are copy-on-write (`Arc::make_mut` to edit): every
    /// undo step copied the continental island's 45k props and 4k objects,
    /// 9 MB a step, 290 MB over the history's depth.
    pub props: Arc<Vec<Prop>>,
    /// Placed gamedata objects (buildings, nature, editor markers). The
    /// decoded area trees stay with the editor; undo snapshots hold only this.
    pub objects: Arc<Vec<Object>>,
    /// Gameplay grids. Copy-on-write: undo steps share untouched layers.
    pub grids: Option<Arc<Grids>>,
    /// Working tint level (see `tint::WORK_LEVEL`), copy-on-write.
    pub tint: Option<Arc<TintMap>>,
    /// Ambient and vegetation prop set (gamedata `SessionSettings`).
    pub session: SessionSettings,
    /// In an undo snapshot: its heights as a patch against the step undo
    /// restores it from, `field` then holding none (`patch_heights`).
    pub(super) heights_patch: Option<Arc<HeightsPatch>>,
}

/// An undo step's heights as the samples that differ from its neighbour
/// in the history. Whole heights made every sculpt step 9 MB on the
/// continental island, 290 MB over the history's depth; a stroke's patch
/// is its footprint.
pub(super) struct HeightsPatch {
    rect: Rect,
    /// The step's heights in `rect`, row by row.
    pub(super) values: Vec<f32>,
    /// The land sync's heights were these heights.
    land: bool,
}

#[derive(Clone)]
pub struct Materials {
    pub ids: Layer,
    pub alphas: Layer,
    pub per_metre: f32,
    /// Shipped DDS bytes per level, (matid, matalpha); saves keep headers.
    /// Level 0 keeps only its header and what follows its words
    /// (`templates` puts `original`'s words back): they were 180 MB on the
    /// continental island.
    templates: Arc<Vec<(Vec<u8>, Vec<u8>)>>,
    original: (Layer, Layer),
}

/// Material level 0 as decoded grids: ids and alphas.
type Level0 = (island::Grid<u32>, island::Grid<u32>);

impl Materials {
    fn embedded(island: &Island) -> Self {
        let ids = Layer::new(island.material_ids());
        let alphas = Layer::new(island.material_alphas());
        Self {
            original: (ids.clone(), alphas.clone()),
            ids,
            alphas,
            per_metre: island.material_alphas().width as f32 / island.world_size()[0] as f32,
            templates: Arc::new(Vec::new()),
        }
    }

    /// The `matid.dds` / `matalpha.dds` levels next to the island archive:
    /// 0..3 on most islands, fewer on the 64 m deco ones. Saves write back
    /// as many levels as shipped.
    /// With them, level 0 as the plain grids it decoded (ids, alphas) when
    /// they come from files: the island attaches them and the ground bakes
    /// from them, where each copy back out of the chunks cost 50 ms.
    pub(super) fn load_level0(
        install: &Install,
        path: &str,
        island: &Island,
    ) -> Result<(Self, Option<Level0>)> {
        if !install.game().profile().material_files {
            return Ok((Self::embedded(island), None));
        }
        let base = island_data_dir(path)?;
        let read = |level: usize, name: &str| {
            install.read(&format!("{base}/materialmaps/{level}/{name}.dds"))
        };
        let mut templates = vec![(read(0, "matid")?, read(0, "matalpha")?)];
        for level in 1..4 {
            match (read(level, "matid"), read(level, "matalpha")) {
                (Ok(ids), Ok(alphas)) => templates.push((ids, alphas)),
                _ => break,
            }
        }
        let words = (
            island::material_words(&templates[0].0)?,
            island::material_words(&templates[0].1)?,
        );
        let (ids, alphas) = (Layer::new(&words.0), Layer::new(&words.1));
        let frame = |dds: &mut Vec<u8>, grid: &island::Grid<u32>| {
            dds.drain(DDS_HEADER..DDS_HEADER + grid.data.len() * 4);
            dds.shrink_to_fit();
        };
        frame(&mut templates[0].0, &words.0);
        frame(&mut templates[0].1, &words.1);
        Ok((
            Self {
                original: (ids.clone(), alphas.clone()),
                ids,
                alphas,
                per_metre: 0.,
                templates: Arc::new(templates),
            },
            Some(words),
        ))
    }

    /// The shipped files of every level, level 0 rebuilt from its frame.
    fn templates(&self) -> Vec<(Vec<u8>, Vec<u8>)> {
        let mut out = (*self.templates).clone();
        let unframe = |frame: &mut Vec<u8>, layer: &Layer| {
            let words = layer.grid().data;
            let bytes = words.iter().flat_map(|w| w.to_le_bytes());
            frame.splice(DDS_HEADER..DDS_HEADER, bytes);
        };
        if let Some((ids, alphas)) = out.first_mut() {
            unframe(ids, &self.original.0);
            unframe(alphas, &self.original.1);
        }
        out
    }

    fn texel(&self, p: [f32; 2]) -> (usize, usize) {
        let clamp = |v: f32, n: usize| ((v * self.per_metre).max(0.) as usize).min(n - 1);
        (
            clamp(p[0], self.alphas.width),
            clamp(p[1], self.alphas.height),
        )
    }

    /// The id with the most weight at a world position.
    pub(super) fn dominant(&self, p: [f32; 2]) -> u8 {
        let (ids, weights) = self.slots(p);
        (0..6).max_by_key(|&k| weights[k]).map_or(0, |k| ids[k])
    }

    /// The id that shows most at a world position once the game has laid
    /// the slots over each other; without the set, the heaviest.
    pub(super) fn shown(&self, p: [f32; 2], palette: Option<&crate::terrain::Palette>) -> u8 {
        let Some(palette) = palette else {
            return self.dominant(p);
        };
        let (ids, weights) = self.slots(p);
        let shown = palette.visible(ids, weights);
        (0..6)
            .max_by(|&a, &b| shown[a].total_cmp(&shown[b]))
            .map_or(0, |k| ids[k])
    }

    /// The six (id, 5-bit weight) slots of the texel at a world position.
    pub(super) fn slots(&self, p: [f32; 2]) -> ([u8; 6], [u8; 6]) {
        let (x, y) = self.texel(p);
        (
            island::unpack_slots(self.ids.get(x / 2, y / 2)),
            island::unpack_slots(self.alphas.get(x, y)),
        )
    }

    /// Paints one dab. The format's routine runs on a block-aligned copy of
    /// the footprint so chunk copies stay limited to what the brush touches.
    pub(super) fn paint(&mut self, center: [f32; 2], brush: &Brush, id: u8) -> bool {
        let r = brush.radius * self.per_metre;
        let (cx, cy) = (center[0] * self.per_metre, center[1] * self.per_metre);
        let even = |v: f32| ((v.max(0.) as usize) / 2) * 2;
        let (x0, y0) = (even(cx - r), even(cy - r));
        let (x1, y1) = (
            ((cx + r).ceil().max(0.) as usize + 2) & !1,
            ((cy + r).ceil().max(0.) as usize + 2) & !1,
        );
        let (x1, y1) = (x1.min(self.alphas.width), y1.min(self.alphas.height));
        if x0 >= x1 || y0 >= y1 {
            return false;
        }
        // Rows of id blocks are independent (each block owns its 2x2
        // texels), so bands of them paint in parallel. A 21 m brush on an
        // 8-texel/m map is 113k texels; one core took 4 ms.
        let paint_band = |by0: usize, by1: usize| {
            let (ty0, ty1) = (by0 * 2, (by1 * 2).min(y1));
            let mut ids = self.ids.region(x0 / 2, by0, x1.div_ceil(2), by1);
            let mut alphas = self.alphas.region(x0, ty0, x1, ty1);
            for y in 0..alphas.height {
                for x in 0..alphas.width {
                    let wx = (x0 + x) as f32 + 0.5;
                    let wy = (ty0 + y) as f32 + 0.5;
                    let weight = brush.weight((wx - cx).hypot(wy - cy) / self.per_metre);
                    if weight > 0. {
                        island::paint_material(
                            &mut ids,
                            &mut alphas,
                            x,
                            y,
                            id,
                            brush.strength.min(1.) * weight,
                        );
                    }
                }
            }
            (by0, ty0, ids, alphas)
        };
        let (by0, by1) = (y0 / 2, y1.div_ceil(2));
        let threads = std::thread::available_parallelism().map_or(4, |n| n.get());
        let texels = (x1 - x0) * (y1 - y0);
        let bands = if texels < 16_384 {
            1
        } else {
            threads.min(by1 - by0)
        };
        let step = (by1 - by0).div_ceil(bands.max(1));
        let painted: Vec<_> = std::thread::scope(|scope| {
            let workers: Vec<_> = (by0..by1)
                .step_by(step.max(1))
                .map(|b| {
                    let end = (b + step).min(by1);
                    scope.spawn(move || paint_band(b, end))
                })
                .collect();
            workers
                .into_iter()
                .map(|w| w.join().expect("paint band"))
                .collect()
        });
        let mut changed = false;
        for (by, ty, ids, alphas) in painted {
            changed |= self.ids.write(x0 / 2, by, &ids);
            changed |= self.alphas.write(x0, ty, &alphas);
        }
        changed
    }
}

/// `data/.../<name>/<name>.a7m` -> `data/.../<name>/_gamedata/<name>`.
pub fn island_data_dir(path: &str) -> Result<String> {
    let (dir, file) = path.rsplit_once('/').context("island path has a folder")?;
    let name = file.rsplit_once('.').map_or(file, |(n, _)| n);
    Ok(format!("{dir}/_gamedata/{name}"))
}

impl Doc {
    pub(super) fn from_island(island: &Island, materials: Option<Materials>) -> Result<Self> {
        let heights = island.heights();
        let [size_x, _] = island.world_size();
        ensure!(
            heights.width > 1 && heights.height > 1,
            "island has no height grid"
        );
        let grid = Grid {
            width: heights.width,
            depth: heights.height,
            spacing: size_x as f32 / (heights.width - 1) as f32,
            origin: [0., 0.],
        };
        let mut field = Heightfield::new(grid, heights.data.clone());
        // rd3d stores i16 in 1/64 m.
        field.limits = [
            f32::from(i16::MIN) / island::HEIGHT_UNITS_PER_METRE,
            f32::from(i16::MAX) / island::HEIGHT_UNITS_PER_METRE,
        ];
        let materials = materials.map(|mut m| {
            m.per_metre = m.alphas.width as f32 / size_x as f32;
            m
        });
        let props = island
            .props()
            .iter()
            .enumerate()
            .map(|(i, p)| Prop {
                id: i as u64 + 1,
                matrix: trs(p.position, p.rotation, p.scale),
                source: p.clone(),
                auto_cliff: false,
            })
            .collect();
        let field = Arc::new(field);
        Ok(Self {
            land_heights: field.clone(),
            field,
            materials,
            meshes: Arc::new(island.prop_files().to_vec()),
            props: Arc::new(props),
            objects: Arc::default(),
            grids: None,
            tint: None,
            session: SessionSettings::read(island.gamedata())?,
            heights_patch: None,
        })
    }

    /// Stores this snapshot's heights as a patch against `next`, the
    /// heights of the step undo or redo restores it from.
    pub(super) fn patch_heights(&mut self, next: &Arc<Heightfield>) {
        if self.heights_patch.is_some() || self.field.grid != next.grid {
            return;
        }
        let g = self.field.grid;
        let rect =
            super::changed_samples(g, &self.field.heights, &next.heights).unwrap_or_default();
        let mut values = Vec::with_capacity((rect.x1 - rect.x0) * (rect.z1 - rect.z0));
        for z in rect.z0..rect.z1 {
            values.extend_from_slice(
                &self.field.heights[z * g.width + rect.x0..z * g.width + rect.x1],
            );
        }
        let land = Arc::ptr_eq(&self.land_heights, &self.field);
        let empty = Arc::new(Heightfield {
            grid: g,
            heights: Vec::new(),
            limits: self.field.limits,
        });
        if land {
            self.land_heights = empty.clone();
        }
        self.field = empty;
        self.heights_patch = Some(Arc::new(HeightsPatch { rect, values, land }));
    }

    /// A patched snapshot's heights back from `next`'s.
    pub(super) fn unpatch_heights(&mut self, next: &Heightfield) {
        let Some(patch) = self.heights_patch.take() else {
            return;
        };
        let mut field = next.clone();
        let (r, w) = (patch.rect, field.grid.width);
        if r.x1 > r.x0 {
            for (z, row) in (r.z0..r.z1).zip(patch.values.chunks(r.x1 - r.x0)) {
                field.heights[z * w + r.x0..z * w + r.x1].copy_from_slice(row);
            }
        }
        let field = Arc::new(field);
        if patch.land {
            self.land_heights = field.clone();
        }
        self.field = field;
    }

    /// Every prop and object with its world position.
    pub fn items(&self) -> impl Iterator<Item = (Item, [f32; 3])> + '_ {
        let props = self
            .props
            .iter()
            .map(|p| (Item::Prop(p.id), math::translation(&p.matrix)));
        let objects = self
            .objects
            .iter()
            .map(|o| (Item::Object(o.id), o.position));
        props.chain(objects)
    }

    /// The prop with `id`. Props stay sorted by id (ids are handed out
    /// in increasing order and appended), so this is a binary search; a
    /// miss scans, in case some path ever breaks the order.
    pub fn prop(&self, id: u64) -> Option<&Prop> {
        match self.props.binary_search_by_key(&id, |p| p.id) {
            Ok(i) => Some(&self.props[i]),
            Err(_) => self.props.iter().find(|p| p.id == id),
        }
    }

    pub fn prop_mut(&mut self, id: u64) -> Option<&mut Prop> {
        match self.props.binary_search_by_key(&id, |p| p.id) {
            Ok(i) => Some(&mut Arc::make_mut(&mut self.props)[i]),
            Err(_) => Arc::make_mut(&mut self.props)
                .iter_mut()
                .find(|p| p.id == id),
        }
    }

    pub fn matrix(&self, item: Item) -> Option<Mat4> {
        match item {
            Item::Prop(id) => self.prop(id).map(|p| p.matrix),
            Item::Object(id) => self
                .objects
                .iter()
                .find(|o| o.id == id)
                .map(|o| trs(o.position, o.rotation, o.scale)),
        }
    }

    pub fn set_matrix(&mut self, item: Item, m: Mat4) {
        match item {
            Item::Prop(id) => {
                if let Some(p) = self.prop_mut(id) {
                    p.matrix = m;
                }
            }
            Item::Object(id) => {
                if let Some(o) = Arc::make_mut(&mut self.objects)
                    .iter_mut()
                    .find(|o| o.id == id)
                {
                    let (t, q, s) = decompose(&m);
                    // Curves move with their object; their first point is
                    // the position, and heights stay on the waterline.
                    let delta = [t[0] - o.position[0], t[2] - o.position[2]];
                    for p in o.path.iter_mut().flatten() {
                        p.p[0] += delta[0];
                        p.p[2] += delta[1];
                    }
                    o.position = t;
                    o.rotation = q;
                    o.scale = s;
                    // Shipped yaw-only objects store q = (0, -sin(d/2), 0,
                    // cos(d/2)), which turns local X to (cos d, 0, sin d).
                    o.direction = m[2].atan2(m[0]);
                }
            }
        }
    }

    /// Writes heights, materials and props into the island model. Returns
    /// the material DDS files to stage, by level.
    pub(super) fn write_into(&self, island: &mut Island) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        island.heights_mut().data.clone_from(&self.field.heights);
        // Props index `meshes`; models added from the library are new to the
        // island's file list, which appends them.
        let props: Vec<island::Prop> = self
            .props
            .iter()
            .map(|p| {
                let (position, rotation, scale) = decompose(&p.matrix);
                let file = self
                    .meshes
                    .get(p.source.file as usize)
                    .map_or(p.source.file, |path| island.prop_file_index(path));
                island::Prop {
                    position,
                    rotation,
                    scale,
                    file,
                    ..p.source.clone()
                }
            })
            .collect();
        *island.props_mut() = props;
        self.session.write(island.gamedata_mut())?;
        let Some(m) = &self.materials else {
            return Ok(Vec::new());
        };
        if m.templates.is_empty() {
            *island.material_ids_mut() = m.ids.grid();
            *island.material_alphas_mut() = m.alphas.grid();
            return Ok(Vec::new());
        }
        // Undoing a paint edit restores the original mip chain exactly,
        // rather than replacing it with our approximate downsampling.
        if m.ids == m.original.0 && m.alphas == m.original.1 {
            return Ok(m.templates());
        }
        // Attached here rather than on load, where it held 360 MB with its
        // copy: the shipped layer, which saves compare against, then the
        // edits over it.
        if island.full_material_ids().is_none() {
            island.attach_full_material_map(m.original.0.grid(), m.original.1.grid())?;
        }
        *island.full_material_ids_mut().expect("attached above") = m.ids.grid();
        *island.full_material_alphas_mut().expect("attached above") = m.alphas.grid();
        let levels = island
            .sync_material_levels()
            .context("material level 0 is attached")?;
        levels
            .iter()
            .zip(m.templates())
            .map(|((ids, alphas), (tid, talpha))| {
                Ok((
                    island::material_dds(&tid, ids)?,
                    island::material_dds(&talpha, alphas)?,
                ))
            })
            .collect()
    }
}

/// Column-major TRS matrix from a stored (x, y, z, w) quaternion.
pub(super) fn trs(t: [f32; 3], q: [f32; 4], s: [f32; 3]) -> Mat4 {
    let [x, y, z, w] = q;
    let r = [
        [
            1. - 2. * (y * y + z * z),
            2. * (x * y + z * w),
            2. * (x * z - y * w),
        ],
        [
            2. * (x * y - z * w),
            1. - 2. * (x * x + z * z),
            2. * (y * z + x * w),
        ],
        [
            2. * (x * z + y * w),
            2. * (y * z - x * w),
            1. - 2. * (x * x + y * y),
        ],
    ];
    [
        r[0][0] * s[0],
        r[0][1] * s[0],
        r[0][2] * s[0],
        0.,
        r[1][0] * s[1],
        r[1][1] * s[1],
        r[1][2] * s[1],
        0.,
        r[2][0] * s[2],
        r[2][1] * s[2],
        r[2][2] * s[2],
        0.,
        t[0],
        t[1],
        t[2],
        1.,
    ]
}

/// Inverse of [`trs`] for matrices without shear.
pub(super) fn decompose(m: &Mat4) -> ([f32; 3], [f32; 4], [f32; 3]) {
    let col = |i: usize| [m[i * 4], m[i * 4 + 1], m[i * 4 + 2]];
    let s = [0, 1, 2].map(|i| math::length(col(i)).max(1e-8));
    let r = [0, 1, 2].map(|i| math::scale(col(i), 1. / s[i]));
    // Standard rotation-matrix-to-quaternion; r[c][row].
    let (m00, m11, m22) = (r[0][0], r[1][1], r[2][2]);
    let trace = m00 + m11 + m22;
    let q = if trace > 0. {
        let k = 0.5 / (trace + 1.).sqrt();
        [
            (r[1][2] - r[2][1]) * k,
            (r[2][0] - r[0][2]) * k,
            (r[0][1] - r[1][0]) * k,
            0.25 / k,
        ]
    } else if m00 > m11 && m00 > m22 {
        let k = 2. * (1. + m00 - m11 - m22).sqrt();
        [
            0.25 * k,
            (r[1][0] + r[0][1]) / k,
            (r[2][0] + r[0][2]) / k,
            (r[1][2] - r[2][1]) / k,
        ]
    } else if m11 > m22 {
        let k = 2. * (1. + m11 - m00 - m22).sqrt();
        [
            (r[1][0] + r[0][1]) / k,
            0.25 * k,
            (r[2][1] + r[1][2]) / k,
            (r[2][0] - r[0][2]) / k,
        ]
    } else {
        let k = 2. * (1. + m22 - m00 - m11).sqrt();
        [
            (r[2][0] + r[0][2]) / k,
            (r[2][1] + r[1][2]) / k,
            0.25 * k,
            (r[0][1] - r[1][0]) / k,
        ]
    };
    ([m[12], m[13], m[14]], q, s)
}

#[cfg(test)]
mod tests {
    use super::{Doc, Layer, decompose, island, island_data_dir, trs};
    use anno_formats::{island::Island, rda};
    use gpui_editor_kit::heightfield::{Brush, Sculpt};
    use std::sync::Arc;

    #[test]
    fn prop_transforms_survive_matrix_roundtrip() {
        let h = std::f32::consts::FRAC_1_SQRT_2;
        for q in [[0., 0., 0., 1.], [0., h, 0., h], [0.1, 0.7, -0.2, 0.67]] {
            let n = q.iter().map(|v: &f32| v * v).sum::<f32>().sqrt();
            let q = q.map(|v| v / n);
            let (t, r, s) = decompose(&trs([1., 2., 3.], q, [2., 0.5, 1.]));
            assert_eq!(t, [1., 2., 3.]);
            assert!(
                s.iter()
                    .zip([2., 0.5, 1.])
                    .all(|(a, b)| (a - b).abs() < 1e-5),
                "{s:?}"
            );
            // q and -q are the same rotation.
            let dot: f32 = r.iter().zip(q).map(|(a, b)| a * b).sum();
            assert!((dot.abs() - 1.).abs() < 1e-5, "{r:?} vs {q:?}");
        }
        assert_eq!(
            island_data_dir("data/a/isl/isl.a7m").unwrap(),
            "data/a/isl/_gamedata/isl"
        );
    }

    // Needs the game: saves hand back the shipped material files exactly
    // while unedited, though level 0 keeps only its frame; an edit
    // attaches the shipped map as the original, the edit over it.
    #[test]
    fn material_levels_save_exactly_and_attach_on_edit() {
        use anno_formats::vfs::Install;
        use gpui_editor_kit::heightfield::Brush;
        let Some(dir) = std::env::var_os("ANNO117_DIR") else {
            eprintln!("skipping: ANNO117_DIR unset");
            return;
        };
        let install = Install::open(dir).unwrap();
        let path = "data/base/provinces/roman/islands/3rdparty/roman_island_3rdparty_pirate_01/roman_island_3rdparty_pirate_01.a7m";
        let editor = super::super::IslandEditor::load(&install, path).unwrap();
        let base = island_data_dir(path).unwrap();
        let shipped = |level: usize, name: &str| {
            install
                .read(&format!("{base}/materialmaps/{level}/{name}.dds"))
                .unwrap()
        };
        let mut source = editor.source.clone();
        let levels = editor.doc.write_into(&mut source).unwrap();
        assert!(!levels.is_empty());
        for (level, (ids, alphas)) in levels.iter().enumerate() {
            assert!(*ids == shipped(level, "matid"), "matid {level}");
            assert!(*alphas == shipped(level, "matalpha"), "matalpha {level}");
        }
        assert!(source.full_material_ids().is_none());

        let mut doc = editor.doc.clone();
        let m = doc.materials.as_mut().unwrap();
        let brush = Brush {
            radius: 6.,
            ..Brush::default()
        };
        assert!(m.paint([60., 60.], &brush, 3));
        let mut source = editor.source.clone();
        let levels = doc.write_into(&mut source).unwrap();
        assert!(levels[0].1 != shipped(0, "matalpha"));
        assert!(*source.full_material_alphas().unwrap() == doc.materials.unwrap().alphas.grid());
    }

    // Load an extracted island, sculpt and move a prop, write it, read it back.
    #[test]
    fn edits_survive_a_save_roundtrip() {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../extract/provinces_roman/data/base/provinces/roman/islands/3rdparty/",
            "roman_island_3rdparty_pirate_01/roman_island_3rdparty_pirate_01.a7m"
        );
        let Ok(archive) = rda::Archive::open(path) else {
            eprintln!("skipping: {path} missing");
            return;
        };
        let member = |n: &str| archive.read_path(n).unwrap();
        let mut source = Island::parse(&member("gamedata.data"), &member("rd3d.data")).unwrap();
        let mut doc = Doc::from_island(&source, None).unwrap();
        let before = doc.field.height([160., 160.]);
        let brush = Brush {
            radius: 10.,
            strength: 3.,
            ..Brush::default()
        };
        Arc::make_mut(&mut doc.field).sculpt([160., 160.], &brush, Sculpt::Raise);
        Arc::make_mut(&mut doc.props)[0].matrix[12] += 5.;
        let moved = doc.props[0].matrix[12];
        assert!(
            doc.write_into(&mut source).unwrap().is_empty(),
            "no material maps attached"
        );
        let (gamedata, rd3d) = source.to_members().unwrap();
        let reread = Doc::from_island(&Island::parse(&gamedata, &rd3d).unwrap(), None).unwrap();
        let after = reread.field.height([160., 160.]);
        assert!((after - before - 3.).abs() < 0.05, "{before} -> {after}");
        assert!((reread.props[0].matrix[12] - moved).abs() < 1e-3);
        assert_eq!(reread.props.len(), doc.props.len());
    }

    #[test]
    fn layer_chunks_copy_on_write() {
        let grid = island::Grid {
            width: 300,
            height: 260,
            data: (0..300 * 260).collect(),
        };
        let mut a = Layer::new(&grid);
        let b = a.clone();
        a.set(299, 259, 7);
        assert_eq!(a.get(299, 259), 7);
        assert_eq!(b.get(299, 259), 299 + 259 * 300);
        assert!(
            Arc::ptr_eq(&a.chunks[0], &b.chunks[0]),
            "untouched chunk is shared"
        );
        assert_eq!(b.grid(), grid);
    }
}
