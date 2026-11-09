// Draws instanced meshes and markers with GPUI's wgpu device, inside the
// window's frame. Geometry stays resident between camera moves; a new scene
// replaces the visible one only once every batch it needs is uploaded.
//
// A frame is up to three passes: the sun's shadow map, opaque geometry, then
// blended surfaces over a copy of the opaque depth. Markers go last.
//
// Linux and Windows: it rides on the `GpuCanvas` hook in vendor/zed (see PATCH.md).
mod control;
mod heights;
mod light;
mod offscreen;
mod placed;
#[cfg(test)]
mod tests;

pub use control::ControlMap;
use control::ResidentControl;
pub use heights::HeightMap;
pub use light::{Fog, Light, Occlusion, Sea, Shadows, Sky, Tone};
pub use offscreen::Offscreen;
pub use placed::{MAX_LEVELS, Piece};

use crate::camera::{Camera, Extent, Frustum};
use crate::heightfield::{Grid, Rect};
use crate::math::{self, Mat4, Vec3};
use crate::texture::{Block, Image, Texture};
use anyhow::{Result, bail, ensure};
use bytemuck::{Pod, Zeroable};
use gpui::{Bounds, Pixels, Point, ScaledPixels, Window, px};
use heights::ResidentHeights;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use wgpu::util::DeviceExt as _;

#[derive(Clone, Debug, Default)]
pub enum Material {
    /// Texture times vertex color, lit, with an alpha cutout at 0.5.
    #[default]
    Lit,
    /// Unlit vertex color, for authored helpers and debug geometry.
    Flat,
    /// Alpha-blended and lit, drawn after all opaque geometry without
    /// writing depth. Batches sort back to front by their bounds; triangles
    /// within one batch do not, so keep each blended surface simple.
    Blend(Blend),
    /// Terrain layers blended per texel, lit like `Lit`.
    Splat(Arc<Splat>),
}

/// Splats compare by identity: their images are too large to compare by value.
impl PartialEq for Material {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Lit, Self::Lit) | (Self::Flat, Self::Flat) => true,
            (Self::Blend(a), Self::Blend(b)) => a == b,
            (Self::Splat(a), Self::Splat(b)) => Arc::ptr_eq(a, b),
            _ => false,
        }
    }
}

/// A translucent surface such as water or glass.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Blend {
    /// 0xRRGGBB, multiplied with the texture and vertex color.
    pub color: u32,
    pub opacity: f32,
    /// World units of view depth behind the surface over which it fades in
    /// from clear, so shorelines soften. 0 disables.
    pub fade: f32,
    /// 0..1: how strongly grazing angles turn opaque and take the sky color.
    pub fresnel: f32,
    /// World units of view depth behind the surface at which it turns fully
    /// opaque, rising from `opacity` past `fade`: deep water hides its
    /// floor. 0 keeps `opacity` at every depth.
    pub depth: f32,
}

impl Default for Blend {
    fn default() -> Self {
        Self {
            color: 0xffffff,
            opacity: 0.5,
            fade: 0.,
            fresnel: 0.,
            depth: 0.,
        }
    }
}

/// Tiled terrain layers stacked through control images.
///
/// `indices` and `weights` come in matching pairs of RGBA8 control maps
/// sampled through the mesh UVs: each channel of an index texel names a layer, and
/// the same channel of the weight texel says how much of it covers. One pair
/// gives four layers per texel, two give eight. Layers stack in index
/// order: the lowest present is opaque, each higher one is laid over the
/// ones below at its weight, interpolated between texels (see
/// `sharpness`). A pixel blends at most 12 distinct layers. Layers tile in
/// world X and Z.
/// Control maps take rectangle edits (`ControlMap::write`), so painting
/// shows on the next frame without a new splat.
///
/// Layers share one texture array, so they must reach one size: the smallest
/// layer's. Larger compressed layers start at the mip of that size, so 512²
/// and 1024² layers share a 512² array; RGBA layers have no mips and must be
/// that size. The array keeps the mips every layer has from there down.
/// Compressed layers of one block format stay compressed on the GPU; mixing
/// formats, or mixing in RGBA, decodes them to RGBA band by band on upload.
///
/// GPU memory: `layers × width × height` times 4 bytes for RGBA, 1 for BC2,
/// BC3, BC5 and BC7, ½ for BC1 and BC4, plus a third for mips; then
/// `width × height × 4` per control image (4096² is 64 MB each, so two pairs
/// at that size cost 256 MB). 25 BC7 layers at 512² with mips are 8.7 MB.
/// Everything streams in under the upload budget.
#[derive(Clone, Debug, Default)]
pub struct Splat {
    /// Layer images, at most 256; see above for sizes.
    pub layers: Vec<Image>,
    /// World units one repeat of each layer covers.
    pub tiling: Vec<f32>,
    /// How each layer's image alpha (a height) cuts into its weight where it
    /// is laid over the layers below; larger is crisper. Empty or 0 lays a
    /// layer over at its plain weight.
    pub sharpness: Vec<f32>,
    /// Tangent-space normal maps (RGB, flat is 128,128,255; green along
    /// +Z), one per layer, or empty for none. They stack with the same
    /// alphas as the layers and bend the terrain normal before lighting.
    /// Same size rules as `layers`, in their own array.
    pub normals: Vec<Image>,
    pub indices: Vec<Arc<ControlMap>>,
    pub weights: Vec<Arc<ControlMap>>,
}

// Vulkan guarantees 256 array layers, and an index channel holds 0..=255,
// of which palettes keep 255 for "none" (palette.wgsl).
const MAX_LAYERS: usize = 255;

impl Splat {
    pub fn validate(&self) -> Result<()> {
        let count = self.layers.len();
        ensure!(
            (1..=MAX_LAYERS).contains(&count),
            "Splat needs 1 to {MAX_LAYERS} layers, has {count}"
        );
        for layer in &self.layers {
            layer.validate()?;
        }
        Layout::new(&self.layers)?;
        if !self.normals.is_empty() {
            ensure!(
                self.normals.len() == count,
                "Splat normals are empty or one per layer"
            );
            for normal in &self.normals {
                normal.validate()?;
            }
            Layout::new(&self.normals)?;
        }
        ensure!(
            self.tiling.len() == count && self.tiling.iter().all(|t| t.is_finite() && *t > 0.),
            "Splat needs one positive tiling per layer"
        );
        ensure!(
            self.sharpness.is_empty()
                || self.sharpness.len() == count
                    && self.sharpness.iter().all(|s| s.is_finite() && *s >= 0.),
            "Splat sharpness is empty or one non-negative value per layer"
        );
        ensure!(
            (1..=2).contains(&self.indices.len()) && self.weights.len() == self.indices.len(),
            "Splat needs one or two index/weight pairs"
        );
        let control = self.indices[0].size();
        for map in self.indices.iter().chain(&self.weights) {
            ensure!(map.size() == control, "Splat control maps differ in size");
        }
        Ok(())
    }
}

/// Mesh input. `uvs`, `colors` and `normals` are either empty or one per position.
#[derive(Clone, Debug)]
pub struct MeshData {
    pub positions: Vec<Vec3>,
    pub uvs: Vec<[f32; 2]>,
    /// 0xRRGGBB. Empty means white.
    pub colors: Vec<u32>,
    /// Unit normals. Empty shades each face flat from screen derivatives.
    pub normals: Vec<Vec3>,
    /// Triangle list.
    pub indices: Vec<u32>,
    /// Albedo for `Lit` and `Blend`. Compressed images keep their mips and
    /// sample trilinearly; `Arc<Texture>` converts with `.into()`.
    pub texture: Option<Image>,
    /// Draw and pick as if the texture's alpha were 1. For albedo whose alpha
    /// stores something else (gloss, height) and would otherwise cut holes.
    pub ignore_alpha: bool,
    /// Tangent-space normals for `Lit` meshes, on the same UVs as `texture`:
    /// X and Y in red and green (Y along +V, texel rows downward), Z
    /// rebuilt; alpha is gloss, 1 a mirror. The tangent frame comes from
    /// screen derivatives, so meshes need no tangents.
    pub normal_map: Option<Image>,
    /// Metalness in red and ambient occlusion in alpha (1 open), same UVs.
    /// Only a [`Tone`]d scene reads metalness.
    pub surface_map: Option<Image>,
    /// `Lit` meshes: how much the scene terrain's colors (times its
    /// `color_gain`) under each fragment multiply the albedo, 0 to 1, so
    /// rocks and plants take on the ground's tint. 0 off.
    pub terrain_tint: f32,
    /// Vertex colors are multiplied by this. Above 1 lets them brighten what
    /// they tint, e.g. a terrain tint multiplier stored as `color / gain`.
    pub color_gain: f32,
    pub material: Material,
    /// Darkens other surfaces when the scene light has shadows. Flat and
    /// blended materials never cast.
    pub casts_shadow: bool,
    /// Displace from these heights: each vertex goes through its instance
    /// matrix with y = 0, then takes the height at its world X/Z (bilinear
    /// between samples, exact on them) and a normal from central
    /// differences of neighbouring samples. Position y and `normals` are
    /// ignored. Empty `uvs` become grid-wide UVs from world X/Z, matching
    /// `Heightfield::tile`, so one flat grid mesh can serve every tile.
    /// Instance matrices should only move, scale and turn about Y.
    /// `Renderer::cast` skips displaced meshes; ray-cast the `Heightfield`.
    pub heights: Option<Arc<HeightMap>>,
    /// Displaced meshes: colors per height sample, multiplied in like
    /// vertex colors (which are then ignored), so recoloring a region
    /// writes texels instead of building meshes. One texel per sample.
    pub color_map: Option<Arc<ControlMap>>,
    /// Loose parts (foliage clumps) placements may draw fewer of with
    /// distance, smoothly; see [`Thinning`]. Placements in the arena only.
    pub thinning: Option<Thinning>,
}

/// How a placed mesh of many loose parts (a tree's leaf clumps) thins out
/// with distance, on the GPU and without popping. `indices` must list the
/// triangles in rank order, so a prefix holds the parts drawn: the cull
/// keeps `keep = clamp((ppm / full)², least, 1)` of them for a placement
/// `ppm` pixels per metre across, draws the shortest of eight prefixes
/// that holds them, and the vertex shader grows kept parts about their
/// centres by `sqrt(1 / keep)` (at most 2.5) so the canopy keeps its
/// coverage, shrinking those ranked just past `keep` to nothing.
#[derive(Clone, Debug)]
pub struct Thinning {
    /// Per vertex: its part's centre and rank, 0 for parts always drawn
    /// (trunks, boughs) up to 1; triangles of a part share them.
    pub parts: Vec<[f32; 4]>,
    /// Pixels per metre from which every part draws.
    pub full: f32,
    /// The least share of parts drawn, however small the placement.
    pub least: f32,
}

impl Default for MeshData {
    fn default() -> Self {
        Self {
            positions: Vec::new(),
            uvs: Vec::new(),
            colors: Vec::new(),
            normals: Vec::new(),
            indices: Vec::new(),
            texture: None,
            ignore_alpha: false,
            normal_map: None,
            surface_map: None,
            terrain_tint: 0.,
            color_gain: 1.,
            material: Material::default(),
            casts_shadow: true,
            heights: None,
            color_map: None,
            thinning: None,
        }
    }
}

/// Thinning parts with fewer triangles stay whole in `MeshData::simplified`.
const THIN_SIMPLIFY_TRIS: usize = 8;
/// How much a UV moved in a collapse counts against the shape error, as a
/// share of the mesh's extent per unit of UV.
const THIN_UV_WEIGHT: f32 = 32.;

impl MeshData {
    /// A coarser stand-in with about `keep` of the triangles, for distance
    /// levels of meshes that ship none: meshoptimizer's edge collapses,
    /// which keep UV seams, with the shape error allowed to grow as `keep`
    /// shrinks (1% of the mesh's extent at half, 6% at a tenth). `None`
    /// when that leaves more than 3/4 of the triangles: the mesh is already
    /// coarse, or seams pin it.
    ///
    /// A thinning mesh keeps every part and each vertex's rank, so the
    /// level thins as the full mesh does. Only parts of at least
    /// `THIN_SIMPLIFY_TRIS` triangles get coarser, and collapses that slide
    /// UVs weigh heavily.
    pub fn simplified(&self, keep: f32) -> Option<MeshData> {
        let bytes: &[u8] = bytemuck::cast_slice(&self.positions);
        let vertices = meshopt::VertexDataAdapter::new(bytes, 12, 0).ok()?;
        let target = ((self.indices.len() as f32 * keep) as usize / 3 * 3).max(3);
        let error = 0.07 * (1. - keep.clamp(0., 1.)).powi(2);
        let indices = match &self.thinning {
            Some(t) if self.uvs.len() == self.positions.len() => {
                let rank = |v: u32| t.parts[v as usize][3].to_bits();
                let mut size: HashMap<u32, usize> = HashMap::new();
                for tri in self.indices.as_chunks::<3>().0 {
                    *size.entry(rank(tri[0])).or_default() += 1;
                }
                // Cards of a few triangles (pine needles: 1-4) only shrink
                // when collapsed.
                let locks: Vec<bool> = (0..self.positions.len() as u32)
                    .map(|v| size.get(&rank(v)).is_none_or(|&n| n < THIN_SIMPLIFY_TRIS))
                    .collect();
                // Leaf clumps are cut out of their texture, and the normal
                // map's frame comes from UV derivatives: UVs weighed like
                // positions left a laurel's canopy darker and blotchy at
                // 300 px. At 32 it matches the full mesh.
                let uvs: Vec<f32> = self.uvs.iter().flatten().copied().collect();
                let mut indices = meshopt::simplify_with_attributes_and_locks(
                    &self.indices,
                    &vertices,
                    &uvs,
                    &[THIN_UV_WEIGHT; 2],
                    8,
                    &locks,
                    target,
                    error,
                    meshopt::SimplifyOptions::None,
                    None,
                );
                // Collapses still delete small clumps whole, and thinning
                // only grows the clumps it keeps by the share it drops
                // itself: canopies went sparse. Deleted clumps come back as
                // they were.
                let kept: HashSet<u32> = indices.iter().map(|&v| rank(v)).collect();
                for tri in self.indices.as_chunks::<3>().0 {
                    if !kept.contains(&rank(tri[0])) {
                        indices.extend(tri);
                    }
                }
                indices
            }
            _ => meshopt::simplify(
                &self.indices,
                &vertices,
                target,
                error,
                meshopt::SimplifyOptions::None,
                None,
            ),
        };
        if indices.is_empty() || indices.len() * 4 > self.indices.len() * 3 {
            return None;
        }
        // Keep only the vertices the coarser triangles use.
        let mut remap = vec![u32::MAX; self.positions.len()];
        let mut out = MeshData {
            positions: Vec::new(),
            uvs: Vec::new(),
            colors: Vec::new(),
            normals: Vec::new(),
            indices: Vec::with_capacity(indices.len()),
            ..self.clone()
        };
        for i in indices {
            let v = i as usize;
            if remap[v] == u32::MAX {
                remap[v] = out.positions.len() as u32;
                out.positions.push(self.positions[v]);
                out.uvs.extend(self.uvs.get(v));
                out.colors.extend(self.colors.get(v));
                out.normals.extend(self.normals.get(v));
            }
            out.indices.push(remap[v]);
        }
        if let (Some(t), Some(thin)) = (&self.thinning, &mut out.thinning) {
            let mut parts = vec![[0.; 4]; out.positions.len()];
            for (v, &r) in remap.iter().enumerate() {
                if r != u32::MAX {
                    parts[r as usize] = t.parts[v];
                }
            }
            // Collapses reorder triangles; prefix draws need rank order.
            let mut tris: Vec<[u32; 3]> = out.indices.as_chunks::<3>().0.to_vec();
            tris.sort_by(|a, b| parts[a[0] as usize][3].total_cmp(&parts[b[0] as usize][3]));
            out.indices = tris.into_iter().flatten().collect();
            thin.parts = parts;
        }
        Some(out)
    }
}

/// Immutable geometry. Share it through `Arc`: the renderer keys GPU
/// residency on the `Arc`, so the same `Arc` in a later scene costs nothing.
pub struct Mesh {
    data: MeshData,
    bounds: [Vec3; 2],
    counts: Counts,
    /// `compact`'s geometry: vertices as the arena stores them and
    /// indices, through meshopt's codecs.
    packed: Option<(Vec<u8>, Vec<u8>)>,
}

/// What draws need of a mesh's geometry without reading it.
struct Counts {
    vertices: u32,
    indices: u32,
    normals: bool,
    uvs: bool,
    /// A thinning mesh's index count up to each of its prefix buckets
    /// (`placed::THIN_BUCKETS`).
    thin: Option<Vec<u32>>,
}

impl Mesh {
    /// Validates indices and stream lengths. Mesh bytes usually come from
    /// game files; a bad index must fail here, not on the GPU.
    pub fn new(data: MeshData) -> Result<Self> {
        for texture in [&data.texture, &data.normal_map, &data.surface_map]
            .into_iter()
            .flatten()
        {
            texture.validate()?;
        }
        match &data.material {
            Material::Splat(splat) => splat.validate()?,
            Material::Blend(blend) => ensure!(
                [blend.opacity, blend.fade, blend.fresnel, blend.depth]
                    .iter()
                    .all(|v| v.is_finite()),
                "Blend material has a non-finite parameter"
            ),
            Material::Lit | Material::Flat => {}
        }
        let n = data.positions.len();
        ensure!(n <= u32::MAX as usize, "Mesh has too many vertices");
        if let Some(t) = &data.thinning {
            ensure!(
                t.parts.len() == n,
                "Mesh thinning has {} parts entries for {n} vertices",
                t.parts.len()
            );
            ensure!(
                t.full > 0. && (0.0..=1.).contains(&t.least),
                "Mesh thinning needs full > 0 and least in 0..=1"
            );
            ensure!(
                (data.indices.as_chunks::<3>().0)
                    .windows(2)
                    .all(|w| t.parts[w[0][0] as usize][3] <= t.parts[w[1][0] as usize][3]),
                "Mesh thinning needs triangles in rank order"
            );
        }
        ensure!(
            data.indices.len().is_multiple_of(3),
            "Mesh indices are not a triangle list"
        );
        ensure!(
            data.indices.iter().all(|&i| (i as usize) < n),
            "Mesh index out of range"
        );
        ensure!(
            data.uvs.is_empty() || data.uvs.len() == n,
            "Mesh has {} UVs for {n} vertices",
            data.uvs.len()
        );
        ensure!(
            data.colors.is_empty() || data.colors.len() == n,
            "Mesh has {} colors for {n} vertices",
            data.colors.len()
        );
        ensure!(
            data.normals.is_empty() || data.normals.len() == n,
            "Mesh has {} normals for {n} vertices",
            data.normals.len()
        );
        ensure!(
            data.positions
                .iter()
                .chain(&data.normals)
                .flatten()
                .all(|v| v.is_finite()),
            "Mesh has a non-finite position or normal"
        );
        let thin = data.thinning.as_ref().map(|t| {
            let ranks: Vec<f32> = (data.indices.as_chunks::<3>().0.iter())
                .map(|tri| t.parts[tri[0] as usize][3])
                .collect();
            (0..placed::THIN_BUCKETS)
                .map(|b| {
                    let upto = placed::bucket_end(t.least, b);
                    ranks.partition_point(|&r| r <= upto) as u32 * 3
                })
                .collect()
        });
        Ok(Self {
            bounds: math::bounds(data.positions.iter().copied()),
            counts: Counts {
                vertices: n as u32,
                indices: data.indices.len() as u32,
                normals: !data.normals.is_empty(),
                uvs: !data.uvs.is_empty(),
                thin,
            },
            data,
            packed: None,
        })
    }

    /// `new`, keeping the geometry compressed: for meshes a host keeps
    /// only to hand the renderer, which decodes them as it packs. An
    /// island's 3,300 model meshes held 266 MB as `MeshData`, 64 MB so.
    /// `data()` then has no positions, UVs, colors, normals or indices;
    /// thinning keeps its parts.
    pub fn compact(data: MeshData) -> Result<Self> {
        let mut mesh = Self::new(data)?;
        let d = &mut mesh.data;
        let mut encoded = (
            meshopt::encode_vertex_buffer(&vertices(d))?,
            meshopt::encode_index_buffer(&d.indices, d.positions.len())?,
        );
        // The encoders allocate their worst case, about the raw size.
        encoded.0.shrink_to_fit();
        encoded.1.shrink_to_fit();
        (d.positions, d.uvs, d.normals, d.colors, d.indices) =
            (Vec::new(), Vec::new(), Vec::new(), Vec::new(), Vec::new());
        mesh.packed = Some(encoded);
        Ok(mesh)
    }

    /// Materials, maps and flags; the geometry too unless `compact`.
    pub fn data(&self) -> &MeshData {
        &self.data
    }

    pub fn triangles(&self) -> usize {
        self.counts.indices as usize / 3
    }

    pub(crate) fn vertex_count(&self) -> usize {
        self.counts.vertices as usize
    }

    pub(crate) fn index_count(&self) -> usize {
        self.counts.indices as usize
    }

    pub(crate) fn has_normals(&self) -> bool {
        self.counts.normals
    }

    pub(crate) fn has_uvs(&self) -> bool {
        self.counts.uvs
    }

    /// Index counts of a thinning mesh's prefix buckets.
    pub(crate) fn thin_counts(&self) -> Option<&[u32]> {
        self.counts.thin.as_deref()
    }

    /// The vertices as shaders read them, into `out`.
    pub(crate) fn write_vertices(&self, out: &mut [GpuVertex]) {
        match &self.packed {
            Some((v, _)) => {
                let decoded: Vec<GpuVertex> =
                    meshopt::decode_vertex_buffer(v, out.len()).expect("encoded by compact");
                out.copy_from_slice(&decoded);
            }
            None => write_vertices(&self.data, out),
        }
    }

    /// The indices plus `base`, into `out`.
    pub(crate) fn write_indices(&self, out: &mut [u32], base: u32) {
        match &self.packed {
            Some((_, i)) => {
                let decoded: Vec<u32> =
                    meshopt::decode_index_buffer(i, out.len()).expect("encoded by compact");
                for (o, d) in out.iter_mut().zip(decoded) {
                    *o = d + base;
                }
            }
            None => {
                for (o, &d) in out.iter_mut().zip(&self.data.indices) {
                    *o = d + base;
                }
            }
        }
    }

    pub fn bounds(&self) -> [Vec3; 2] {
        self.bounds
    }
}

/// Placements of one mesh. `ids` is empty or one per matrix; picking reports them.
#[derive(Clone, Debug, Default)]
pub struct Instances {
    pub matrices: Vec<Mat4>,
    pub ids: Vec<Option<u64>>,
}

#[derive(Clone)]
pub struct Batch {
    pub mesh: Arc<Mesh>,
    pub instances: Arc<Instances>,
}

impl Batch {
    pub fn new(mesh: Arc<Mesh>, instances: Arc<Instances>) -> Self {
        Self { mesh, instances }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Shape {
    #[default]
    Circle,
    Diamond,
    Square,
    Ring,
}

/// A screen-facing dot of constant pixel size, drawn above all geometry.
#[derive(Clone, Copy, Debug)]
pub struct Marker {
    pub position: Vec3,
    /// Radius in logical pixels.
    pub radius: f32,
    /// 0xRRGGBB.
    pub color: u32,
    pub shape: Shape,
}

/// A polyline over the scene at a constant width in logical pixels, drawn
/// above all geometry like markers: river courses, outlines, guides.
#[derive(Clone, Debug)]
pub struct Line {
    pub points: Vec<Vec3>,
    pub width: f32,
    /// 0xRRGGBBAA.
    pub color: u32,
}

#[derive(Clone, Default)]
pub struct Scene {
    pub batches: Vec<Batch>,
    pub lines: Arc<Vec<Line>>,
    /// Ground drawn as GPU-displaced patches; see [`Terrain`].
    pub terrain: Option<Arc<Terrain>>,
    pub markers: Arc<Vec<Marker>>,
    pub light: Light,
    /// Shade in linear light and tone map; `None` keeps display-space shading.
    pub tone: Option<Tone>,
    /// Behind everything; `None` leaves the target as it was.
    pub sky: Option<Sky>,
    pub occlusion: Option<Occlusion>,
}

/// Which instances [`Renderer::set_outline`] rings and lightly tints. Marks
/// go by what the opaque pass writes per pixel, so the band follows the drawn
/// silhouette: thin foliage gets an outline a bounding box could not give it.
#[derive(Clone, Debug, Default)]
pub struct Outline {
    /// Placement rows, as [`Hit::placement`] reports them.
    pub rows: Vec<u32>,
    /// Batch instance ids, as [`Hit::id`] reports them.
    pub ids: Vec<u64>,
    /// 0xRRGGBBAA.
    pub color: u32,
    /// Band width in pixels outside the silhouette.
    pub width: u32,
}

/// [`Outline`] as `outline.wgsl` reads it: a header, then one bit per
/// placement row, then one bit per batch pick handle. Bitsets so each of
/// the pass's taps is one load, whatever the selection's size.
fn outline_marks(outline: &Outline, pick_ids: &[Option<u64>]) -> Vec<u32> {
    let placed = outline
        .rows
        .iter()
        .max()
        .map_or(0, |&r| r as usize / 32 + 1);
    let mut ids = outline.ids.clone();
    ids.sort_unstable();
    let handles = if ids.is_empty() {
        0
    } else {
        pick_ids.len().div_ceil(32)
    };
    let mut marks = vec![0u32; 4 + placed + handles];
    marks[0] = placed as u32;
    marks[1] = outline.color;
    marks[2] = outline.width.max(1);
    for &row in &outline.rows {
        marks[4 + row as usize / 32] |= 1 << (row % 32);
    }
    if !ids.is_empty() {
        for (i, id) in pick_ids.iter().enumerate() {
            if id.is_some_and(|id| ids.binary_search(&id).is_ok()) {
                marks[4 + placed + i / 32] |= 1 << (i % 32);
            }
        }
    }
    marks
}

/// A heightfield drawn from its GPU height map as grid patches whose density
/// follows the screen: each frame every tile is culled and takes the
/// coarsest of five strides (1 to 16 samples) whose cells still cover a few
/// pixels, so distant ground costs a few triangles a tile. Skirts hide the
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
    /// `Lit` (with `texture`) or `Splat`; UVs run 0..1 across the grid.
    pub material: Material,
    pub texture: Option<Image>,
}

/// Each polyline's segments: two points, width and color packed in w.
fn segments(lines: &[Line]) -> Vec<[[f32; 4]; 2]> {
    (lines.iter())
        .flat_map(|l| {
            l.points.windows(2).map(|w| {
                let [a, b] = [w[0], w[1]];
                [
                    [a[0], a[1], a[2], l.width],
                    [b[0], b[1], b[2], f32::from_bits(l.color)],
                ]
            })
        })
        .collect()
}

/// Strides of the terrain's patch levels, in samples.
const TERRAIN_STRIDES: [u32; 5] = [1, 2, 4, 8, 16];
/// Cells of a terrain tile at stride 1. A tile is one patch instance.
const TERRAIN_TILE: u32 = 64;
/// Tiles take the coarsest stride whose cells cover at most this many
/// pixels: finer would spend triangles on sub-pixel detail.
const TERRAIN_CELL_PX: f32 = 3.;
/// The shadow map draws every tile at stride 4: shadows are soft anyway.
const TERRAIN_SHADOW_LEVEL: usize = 2;
/// Skirt vertices store `SKIRT - drop` in y (gpu.wgsl): far below anything
/// a displaced mesh means by y, which it otherwise ignores.
const SKIRT: f32 = -1e6;

impl Terrain {
    /// The patch for stride `stride`: a `cells`² grid of cells `stride`
    /// samples wide, with a skirt along each edge as deep as a few cells.
    fn patch(&self, cells: u32, stride: u32) -> MeshData {
        let spacing = self.heights.grid().spacing;
        let n = cells / stride;
        let step = spacing * stride as f32;
        let drop = (step * 4.).max(2.);
        let side = n + 1;
        let mut positions: Vec<Vec3> = (0..side * side)
            .map(|i| [(i % side) as f32 * step, 0., (i / side) as f32 * step])
            .collect();
        let at = |x: u32, z: u32| z * side + x;
        let mut indices = Vec::with_capacity((n * n * 6 + n * 24) as usize);
        for z in 0..n {
            for x in 0..n {
                let (a, b, c, d) = (at(x, z), at(x + 1, z), at(x + 1, z + 1), at(x, z + 1));
                indices.extend([a, b, c, a, c, d]);
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
/// batches from `first`, one per stride, without instances; each frame
/// places tiles on them.
#[derive(Clone)]
struct TerrainTiles {
    source: Arc<Terrain>,
    first: usize,
    /// South-west corner of each tile.
    origins: Vec<[f32; 2]>,
    /// Metres a tile spans.
    size: f32,
    /// Triangles of each level's patch.
    triangles: Vec<usize>,
}

impl TerrainTiles {
    /// The tiles a view shows as instance matrices, grouped by level, and
    /// each level's range of them. `rows` are the view's clip rows,
    /// `height` its height in pixels.
    fn place(
        &self,
        frustum: &Frustum,
        rows: &[[f64; 4]; 4],
        height: f32,
    ) -> (Vec<Mat4>, Vec<std::ops::Range<u32>>) {
        let [lo, hi] = self.source.heights.range();
        let spacing = f64::from(self.source.heights.grid().spacing);
        let w = rows[3];
        let scale = (rows[1][0].powi(2) + rows[1][1].powi(2) + rows[1][2].powi(2)).sqrt()
            * f64::from(height)
            * 0.5;
        let mut levels: Vec<Vec<Mat4>> = vec![Vec::new(); TERRAIN_STRIDES.len()];
        for o in &self.origins {
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
            levels[k].push(math::translate([o[0], 0., o[1]]));
        }
        let mut ranges = Vec::with_capacity(levels.len());
        let mut matrices = Vec::new();
        for level in levels {
            let start = matrices.len() as u32;
            matrices.extend(level);
            ranges.push(start..matrices.len() as u32);
        }
        (matrices, ranges)
    }
}

/// What is drawn under a pixel: a batch instance's id or a placement row,
/// and the surface point there.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Hit {
    pub id: Option<u64>,
    pub placement: Option<u32>,
    pub position: Vec3,
}

#[repr(C)]
#[derive(Clone, Copy, Default, Pod, Zeroable)]
pub(super) struct GpuVertex {
    position: [f32; 4],
    uv: [f32; 2],
    color: u32,
    normal: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct Globals {
    clip: [[f32; 4]; 4],
    /// Inverse of `clip`, for the sky's view rays.
    unclip: [[f32; 4]; 4],
    viewport: [f32; 4],
    eye: [f32; 4],
    anchor: [f32; 4],
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct Lights {
    shadow: [[f32; 4]; 4],
    sun: [f32; 4],
    sun_color: [f32; 4],
    sky: [f32; 4],
    ground: [f32; 4],
    fog: [f32; 4],
    fog_range: [f32; 4],
    /// Exposure (0: display-space shading), sun occlusion.
    tone: [f32; 4],
    /// Sky: zenith (w = 1 with a sky), horizon (w: sea level), sea color
    /// (w = 1 with a sea).
    zenith: [f32; 4],
    horizon: [f32; 4],
    sea: [f32; 4],
    /// Ground tint: the terrain grid's origin x and z, spacing, and the
    /// color gain (0: no terrain).
    tint: [f32; 4],
    /// Occlusion radius and strength.
    occlusion: [f32; 4],
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable, Default)]
struct Params {
    surface: [f32; 4],
    flags: [f32; 4],
    blend: [f32; 4],
    tone: [f32; 4],
    terrain: [f32; 4],
    /// x: terrain tint amount.
    extra: [f32; 4],
}

/// x: 1 when instances write pick handles; y: 1 for placed draws, which
/// read rows from the visible list at z. One per draw, at a dynamic offset.
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable, Default)]
struct DrawIds {
    ids: [u32; 4],
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct GpuMarker {
    position: [f32; 4],
    color: [f32; 4],
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct GpuLayer {
    scale: f32,
    sharpness: f32,
}

/// A batch with the bounds culling needs.
struct Prepared {
    batch: Batch,
    /// A terrain patch level: drawn per frame from tile lists, not from
    /// the batch's (empty) instances.
    terrain: bool,
    /// World bounds of every instance. Displaced meshes take their Y span
    /// from the height map on each use; see `bounds`.
    world: [Vec3; 2],
}

impl Prepared {
    fn new(batch: &Batch) -> Self {
        let mut mesh = batch.mesh.bounds();
        // Displaced vertices enter their matrix at y = 0.
        if batch.mesh.data().heights.is_some() {
            mesh[0][1] = 0.;
            mesh[1][1] = 0.;
        }
        let corners = math::corners(mesh);
        Self {
            world: math::bounds(
                (batch.instances.matrices.iter())
                    .flat_map(|m| corners.map(|p| math::transform(m, p))),
            ),
            batch: batch.clone(),
            terrain: false,
        }
    }

    /// A terrain patch level spanning the whole grid.
    fn terrain(mesh: Arc<Mesh>, grid: Grid) -> Self {
        let [w, d] = grid.size();
        let o = grid.origin;
        Self {
            world: [[o[0], 0., o[1]], [o[0] + w, 0., o[1] + d]],
            batch: Batch::new(mesh, Arc::new(Instances::default())),
            terrain: true,
        }
    }

    fn material(&self) -> &Material {
        &self.batch.mesh.data().material
    }

    fn casts_shadow(&self) -> bool {
        casts_shadow(self.batch.mesh.data())
    }

    fn heights(&self) -> Option<&Arc<HeightMap>> {
        self.batch.mesh.data().heights.as_ref()
    }

    fn bounds(&self) -> [Vec3; 2] {
        let mut bounds = self.world;
        if let Some(map) = self.heights() {
            [bounds[0][1], bounds[1][1]] = map.range();
        }
        bounds
    }
}

fn casts_shadow(data: &MeshData) -> bool {
    data.casts_shadow && matches!(data.material, Material::Lit | Material::Splat(_))
}

fn key<T: ?Sized>(arc: &Arc<T>) -> usize {
    Arc::as_ptr(arc) as *const () as usize
}

struct PreparedScene {
    generation: u64,
    batches: Vec<Prepared>,
    terrain: Option<TerrainTiles>,
    markers: Arc<Vec<Marker>>,
    lines: Arc<Vec<Line>>,
    light: Light,
    tone: Option<Tone>,
    sky: Option<Sky>,
    occlusion: Option<Occlusion>,
}

impl PreparedScene {
    fn bounds(&self, filter: impl Fn(&Prepared) -> bool) -> [Vec3; 2] {
        let boxes = self.batches.iter().filter(|p| filter(p));
        math::bounds(boxes.flat_map(|p| p.bounds()))
    }
}

#[derive(Clone, Copy, PartialEq)]
pub(crate) struct View {
    camera: Camera,
    extent: Extent,
    bounds: Bounds<Pixels>,
}

/// Renders an island's scene into one or more viewports. Create one, and a
/// [`Viewport`] per pane; viewports share its meshes, placements and shadow
/// map. All state sits behind one lock: public calls take it once and
/// nothing inside takes it again.
#[derive(Default)]
pub struct Renderer {
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    stats: Stats,
    /// Overlay lines set since the last draw, for the device to upload.
    overlay: Option<Vec<Line>>,
    /// The selection outline, drawn every frame until replaced, and how
    /// many times it was: its marks upload again only when it changes.
    outline: Option<Outline>,
    outline_version: u64,
    /// When the host started its frame (`Renderer::mark_frame`) and
    /// finished building it (`mark_built`).
    frame_mark: Option<std::time::Instant>,
    built_mark: Option<std::time::Instant>,
    /// The mark of the frame the last draw belonged to, and the slots
    /// drawn under it, one bit each. A draw under a new mark, or of a slot
    /// already drawn, starts a frame: GPUI repaints without the host
    /// building again (animation frames, hover), which reuse the mark.
    drawn_mark: Option<std::time::Instant>,
    drawn_slots: u64,
    /// Whether the host built the current frame (it marked it).
    frame_built: bool,
    /// When this frame's first viewport draw started.
    encode_start: Option<std::time::Instant>,
    frame_began: Option<std::time::Instant>,
    scene: Option<Arc<PreparedScene>>,
    assets: Assets,
    /// Created by the first draw: pipelines need the target format.
    device: Option<Device>,
    slots: Vec<Slot>,
}

/// One viewport's camera, targets and readbacks.
#[derive(Default)]
struct Slot {
    view: Option<View>,
    targets: Option<Targets>,
    pick_request: Option<Point<Pixels>>,
    picked: Option<Picked>,
    drawn: Drawn,
}

/// A pane drawing its [`Renderer`]'s scene with its own camera. Keep one
/// per pane for as long as the pane exists: GPUI frees a viewport's targets
/// when its canvas leaves the window.
pub struct Viewport {
    renderer: Arc<Renderer>,
    slot: usize,
}

/// A pick read back from the GPU: the cursor and view it was read at, the
/// handle under it and the depth there.
#[derive(Clone, Copy)]
struct Picked {
    cursor: Point<Pixels>,
    view: View,
    handle: u32,
    depth: f32,
}

/// Frame times, smoothed over about ten frames, in milliseconds.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Stats {
    /// Between consecutive frames: what the screen shows.
    pub frame_ms: f32,
    /// CPU time from the host's `mark_frame` to the end of the last
    /// viewport's draw: the sum of the next three.
    pub cpu_ms: f32,
    /// Building the window's elements (`mark_frame` to `mark_built`).
    pub build_ms: f32,
    /// GPUI's layout, paint and its encoding before ours.
    pub layout_ms: f32,
    /// Encoding the viewports.
    pub encode_ms: f32,
    /// GPU time of this renderer's passes, from timestamp queries (zero
    /// where the device has none).
    pub gpu_ms: f32,
    /// The last frame's timed passes by name, unsmoothed; unused slots
    /// have an empty name.
    pub passes: [(&'static str, f32); TIMED_PASSES as usize],
    /// The last frame from its first timed pass's start to its last one's
    /// end: `passes` plus the gaps and untimed work between them.
    pub gpu_span_ms: f32,
}

impl Stats {
    fn blend(old: f32, new: f32) -> f32 {
        if old == 0. {
            new
        } else {
            old * 0.9 + new * 0.1
        }
    }
}

/// What the last frame drew, for profiling.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Drawn {
    pub batches: usize,
    pub instances: usize,
    pub triangles: usize,
    /// Placements drawn and their triangles, per level (from the frame
    /// before: the counts come back from the GPU).
    pub placed: [usize; MAX_LEVELS],
    pub placed_triangles: usize,
}

/// What the placement table asks a visible-list entry to hold: rows and
/// materials share its 32 bits, else pieces draw one by one (placed.rs).
/// `draws` counts thinning buckets apart, for comparison.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PlacementCounts {
    pub rows: u32,
    pub materials: u32,
    pub draws: u32,
}

impl PlacementCounts {
    /// Bits a visible entry needs for its row and its material.
    pub fn bits(&self) -> u32 {
        let width = |n: u32| u32::BITS - n.saturating_sub(1).leading_zeros();
        u32::BITS - self.rows.leading_zeros() + width(self.materials)
    }
}

impl std::fmt::Debug for Renderer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("gpu::Renderer")
    }
}

impl std::fmt::Debug for Viewport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "gpu::Viewport({})", self.slot)
    }
}

impl Renderer {
    /// A new pane on this renderer's scene.
    pub fn viewport(self: &Arc<Self>) -> Arc<Viewport> {
        let mut state = self.state.lock().unwrap();
        state.slots.push(Slot::default());
        Arc::new(Viewport {
            renderer: self.clone(),
            slot: state.slots.len() - 1,
        })
    }

    /// Replaces the batches, markers and light.
    pub fn set_scene(&self, scene: &Scene) {
        let mut batches: Vec<Prepared> = scene.batches.iter().map(Prepared::new).collect();
        let mut state = self.state.lock().unwrap();
        let generation = state.scene.as_ref().map_or(1, |s| s.generation + 1);
        // The same terrain keeps its patch meshes, and with them their GPU
        // copies.
        let previous = state.scene.as_ref().and_then(|s| {
            let t = s.terrain.as_ref()?;
            Some((
                t.clone(),
                &s.batches[t.first..t.first + TERRAIN_STRIDES.len()],
            ))
        });
        let terrain = scene.terrain.as_ref().map(|source| {
            let grid = source.heights.grid();
            let cells = grid.width.max(grid.depth).saturating_sub(1).max(1) as u32;
            // Tiles of 64 cells, or one tile when the grid does not split.
            let tile = if cells.is_multiple_of(TERRAIN_TILE) {
                TERRAIN_TILE
            } else {
                cells
            };
            let meshes: Vec<Arc<Mesh>> = match &previous {
                Some((t, levels)) if Arc::ptr_eq(&t.source, source) => {
                    levels.iter().map(|p| p.batch.mesh.clone()).collect()
                }
                _ => TERRAIN_STRIDES
                    .iter()
                    .map(|&s| {
                        let data = source.patch(tile, s.min(tile));
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
            TerrainTiles {
                source: source.clone(),
                first,
                origins,
                size,
                triangles,
            }
        });
        state.scene = Some(Arc::new(PreparedScene {
            generation,
            batches,
            terrain,
            markers: scene.markers.clone(),
            lines: scene.lines.clone(),
            light: scene.light,
            tone: scene.tone,
            sky: scene.sky,
            occlusion: scene.occlusion,
        }));
    }

    /// Replaces the model pieces placements draw. Clears the placements:
    /// rows name pieces by index.
    pub fn set_pieces(&self, pieces: Vec<Piece>) {
        self.state.lock().unwrap().assets.placed.set_pieces(pieces);
    }

    /// Replaces every placement: row i draws piece `piece_of[i]` with
    /// `transforms[i]`, culled and given a detail level on the GPU each
    /// frame. Picks report rows.
    pub fn set_placements(&self, piece_of: &[u32], transforms: &[Mat4]) {
        let mut state = self.state.lock().unwrap();
        state.assets.placed.set_placements(piece_of, transforms);
    }

    pub fn placement_counts(&self) -> PlacementCounts {
        self.state.lock().unwrap().assets.placed.counts()
    }

    /// Moves rows `first..` in place: a drag writes only its own rows.
    pub fn move_placements(&self, first: u32, transforms: &[Mat4]) {
        let mut state = self.state.lock().unwrap();
        state.assets.placed.move_placements(first, transforms);
    }

    /// Replaces the overlay lines: drawn like `Scene::lines` but set apart
    /// from the scene, so a brush ring or guide that follows the cursor
    /// changes a line buffer rather than the scene.
    pub fn set_overlay(&self, lines: Vec<Line>) {
        self.state.lock().unwrap().overlay = Some(lines);
    }

    /// Replaces the selection outline; `None` or empty marks draw nothing.
    /// Set apart from the scene, as the overlay is: selection changes often.
    pub fn set_outline(&self, outline: Option<Outline>) {
        let mut state = self.state.lock().unwrap();
        state.outline = outline;
        state.outline_version += 1;
    }

    /// Replaces the selected placements, kept on the GPU until replaced:
    /// `rows` are every row of the selected items, `marks` one marker per
    /// item (a row of it, and the item's origin in that row's space) drawn
    /// with `marker`'s radius, color and shape after the scene's markers.
    /// Rows must be in the current table (`set_placements` drops them).
    pub fn set_selection(&self, rows: Vec<u32>, marks: &[(u32, Vec3)], marker: &Marker) {
        let marks = (marks.iter())
            .map(|&(row, p)| [p[0], p[1], p[2], f32::from_bits(row)])
            .collect();
        let [r, g, b] = rgb(marker.color);
        let color = [r, g, b, marker.shape as u32 as f32];
        (self.state.lock().unwrap().assets.placed).set_selection(rows, marks, color, marker.radius);
    }

    /// Drags the selected rows on the GPU: while `Some`, each draws at
    /// `delta` times its transform from when the drag began, and its marker
    /// follows. The table is not touched; at `None` the rows go back to
    /// what it holds, so move them there (`move_placements`) first. A
    /// drag of 20k items costs the CPU one matrix a frame.
    pub fn drag_selection(&self, delta: Option<Mat4>) {
        self.state
            .lock()
            .unwrap()
            .assets
            .placed
            .drag_selection(delta);
    }

    /// Call when the host starts building a frame; CPU time counts from here.
    pub fn mark_frame(&self) {
        self.state.lock().unwrap().frame_mark = Some(std::time::Instant::now());
    }

    /// Call when the host has built its frame's elements.
    pub fn mark_built(&self) {
        self.state.lock().unwrap().built_mark = Some(std::time::Instant::now());
    }

    pub fn stats(&self) -> Stats {
        self.state.lock().unwrap().stats
    }

    /// Whether the latest scene or placement meshes are still uploading.
    pub fn pending(&self) -> bool {
        self.state.lock().unwrap().pending()
    }

    /// What the last offscreen frame drew (viewport 0).
    pub fn drawn(&self) -> Drawn {
        let state = self.state.lock().unwrap();
        state.slots.first().map(|s| s.drawn).unwrap_or_default()
    }

    /// Points viewport `slot` at a camera; offscreen rendering uses slot 0.
    pub(crate) fn aim(&self, slot: usize, view: View) {
        let mut state = self.state.lock().unwrap();
        if state.slots.len() <= slot {
            state.slots.resize_with(slot + 1, Slot::default);
        }
        state.slots[slot].view = Some(view);
    }

    /// See [`Viewport::pick`].
    pub(crate) fn pick(&self, slot: usize, cursor: Point<Pixels>) -> Option<Hit> {
        let mut state = self.state.lock().unwrap();
        let state = &mut *state;
        let s = state.slots.get_mut(slot)?;
        let view = s.view?;
        if !view.bounds.contains(&cursor) {
            return None;
        }
        s.pick_request = Some(cursor);
        let picked = s.picked?;
        if picked.cursor != cursor || picked.handle == 0 {
            return None;
        }
        // The point on the cursor's ray whose clip depth (z / w, reversed)
        // matches the depth read back.
        let v = picked.view;
        let ray = v.camera.ray(cursor, v.bounds, v.extent);
        let rows = v.camera.view_rows(v.bounds, v.extent, true);
        let row = |r: [f64; 4], p: [f64; 3]| r[0] * p[0] + r[1] * p[1] + r[2] * p[2];
        let depth = f64::from(picked.depth);
        let (z0, w0) = (
            row(rows[2], ray.origin) + rows[2][3],
            row(rows[3], ray.origin) + rows[3][3],
        );
        let (dz, dw) = (row(rows[2], ray.direction), row(rows[3], ray.direction));
        let t = (depth * w0 - z0) / (dz - depth * dw);
        let position = std::array::from_fn(|a| (ray.origin[a] + t * ray.direction[a]) as f32);
        let (id, placement) = if picked.handle & placed::PLACED != 0 {
            (None, Some(picked.handle & !placed::PLACED))
        } else {
            let ids = &state.device.as_ref()?.pick_ids;
            (*ids.get(picked.handle as usize - 1)?, None)
        };
        Some(Hit {
            id,
            placement,
            position,
        })
    }
}

impl State {
    fn pending(&self) -> bool {
        if self.assets.placed.pending() {
            return true;
        }
        let Some(generation) = self.scene.as_ref().map(|s| s.generation) else {
            return false;
        };
        self.device.as_ref().is_none_or(|d| {
            d.presented
                .as_ref()
                .is_none_or(|p| p.generation != generation)
        })
    }
}

impl Viewport {
    pub fn renderer(&self) -> &Arc<Renderer> {
        &self.renderer
    }

    /// Points this pane at `camera` without painting it: its canvas draws
    /// every frame GPUI draws, so a camera move only needs a frame, not a
    /// new paint of the pane (which keeps its last bounds).
    pub fn set_camera(&self, camera: Camera, extent: Extent) {
        let mut state = self.renderer.state.lock().unwrap();
        if let Some(view) = &mut state.slots[self.slot].view {
            view.camera = camera;
            view.extent = extent;
        }
    }

    /// Paints this pane into `bounds` with this camera.
    pub fn paint(
        self: &Arc<Self>,
        camera: Camera,
        extent: Extent,
        bounds: Bounds<Pixels>,
        window: &mut Window,
    ) {
        if bounds.size.width <= px(0.) || bounds.size.height <= px(0.) {
            return;
        }
        let keep_drawing = {
            let mut state = self.renderer.state.lock().unwrap();
            let pending = state.pending();
            let slot = &mut state.slots[self.slot];
            slot.view = Some(View {
                camera,
                extent,
                bounds,
            });
            // Uploads are spread across frames, and a pick needs the frame
            // that reads it back.
            let picking =
                slot.pick_request.is_some() && slot.picked.map(|p| p.cursor) != slot.pick_request;
            pending || picking
        };
        if keep_drawing {
            window.request_animation_frame();
        }
        window.paint_gpu_canvas(bounds, self.clone());
    }

    /// What is drawn under `cursor`. The GPU writes a pick handle per pixel
    /// and each frame reads back the one under the last asked-for cursor, so
    /// a new cursor answers `None` until the next frame has been drawn;
    /// hosts ask on hover, and clicks find the answer waiting.
    pub fn pick(&self, cursor: Point<Pixels>) -> Option<Hit> {
        self.renderer.pick(self.slot, cursor)
    }

    /// What this pane's last frame drew.
    pub fn drawn(&self) -> Drawn {
        let state = self.renderer.state.lock().unwrap();
        state.slots[self.slot].drawn
    }
}

impl gpui::GpuCanvas for Viewport {
    fn draw(&self, frame: &mut gpui::GpuCanvasFrame) {
        let gpu = Gpu {
            device: frame.device.clone(),
            queue: frame.queue.clone(),
        };
        self.renderer.draw(
            self.slot,
            &gpu,
            frame.encoder,
            frame.target,
            frame.format,
            frame.size,
            frame.bounds,
            frame.clip,
        );
    }
}

// Per-frame upload budget. Interactive frames matter more than finishing a
// big scene load one frame sooner. Textures stream in bands of rows under
// the same byte budget, so a 64 MB control image takes about eight frames.
const UPLOAD_BYTES: usize = 8 * 1024 * 1024;
// Level-of-detail bands and spatial cells make many small batches.
const UPLOAD_BATCHES: usize = 256;
const UPLOAD_MICROS: u128 = 3000;
// CPU-decoded bands (devices without BC sampling) stay small enough to fit
// the time budget: BC7 decodes at about 0.7 GB/s, so 1 MB of RGBA is ~1.5 ms.
const DECODE_BAND_BYTES: usize = 1024 * 1024;

/// One frame's upload allowance: 8 MB and 3 ms, counted from `new`. Until
/// something is charged it admits one item of any size, so a queue always
/// progresses.
pub struct Budget {
    started: std::time::Instant,
    bytes: usize,
    progressed: bool,
    limit: usize,
    micros: u128,
}

impl Default for Budget {
    fn default() -> Self {
        Self::new()
    }
}

impl Budget {
    /// Starts the default per-frame byte and time allowance.
    pub fn new() -> Self {
        Self {
            started: std::time::Instant::now(),
            bytes: 0,
            progressed: false,
            limit: UPLOAD_BYTES,
            micros: UPLOAD_MICROS,
        }
    }

    /// Bytes uploaded so far.
    pub fn used(&self) -> usize {
        self.bytes
    }

    /// Whether this frame should stop uploading.
    pub fn spent(&self) -> bool {
        self.progressed
            && (self.bytes >= self.limit || self.started.elapsed().as_micros() > self.micros)
    }

    /// Bytes left before `spent`; zero once over.
    pub fn room(&self) -> usize {
        self.limit.saturating_sub(self.bytes)
    }

    /// Counts `bytes` as uploaded this frame.
    pub fn charge(&mut self, bytes: usize) {
        self.bytes += bytes;
        self.progressed = true;
    }
}

/// How images share one device texture: the smallest image's size, the
/// level each image starts at to reach it, and how many levels all of them
/// have from there.
struct Layout {
    width: u32,
    height: u32,
    levels: u32,
    skips: Vec<u32>,
    /// The one block format, when every image is compressed with it.
    block: Option<Block>,
}

impl Layout {
    fn new(images: &[Image]) -> Result<Self> {
        let Some((width, height)) = images
            .iter()
            .map(Image::size)
            .min_by_key(|&(w, h)| u64::from(w) * u64::from(h))
        else {
            bail!("No images");
        };
        let mut levels = u32::MAX;
        let mut skips = Vec::with_capacity(images.len());
        for (i, image) in images.iter().enumerate() {
            let (w, h) = image.size();
            let Some(skip) = (0..image.levels())
                .find(|&level| ((w >> level).max(1), (h >> level).max(1)) == (width, height))
            else {
                bail!("Image {i} ({w}x{h}) has no {width}x{height} mip");
            };
            levels = levels.min((image.levels() - skip) as u32);
            skips.push(skip as u32);
        }
        let block = match images {
            [first, rest @ ..] => first
                .block()
                .filter(|&b| rest.iter().all(|i| i.block() == Some(b))),
            _ => None,
        };
        Ok(Self {
            width,
            height,
            levels,
            skips,
            block,
        })
    }
}

/// A wgpu device and its queue: GPUI's in a window, an offscreen target's
/// own in tests and thumbnails.
#[derive(Clone, Debug)]
pub(crate) struct Gpu {
    pub device: wgpu::Device,
    pub queue: wgpu::Queue,
}

/// Writes `data`, rows of `size[0]` texels of `bytes_per_texel`, into level
/// 0 of `texture` at `origin`.
pub(crate) fn write_texture_rect(
    queue: &wgpu::Queue,
    texture: &wgpu::Texture,
    origin: [u32; 2],
    size: [u32; 2],
    bytes_per_texel: u32,
    data: &[u8],
) {
    queue.write_texture(
        wgpu::TexelCopyTextureInfo {
            texture,
            mip_level: 0,
            origin: wgpu::Origin3d {
                x: origin[0],
                y: origin[1],
                z: 0,
            },
            aspect: wgpu::TextureAspect::All,
        },
        data,
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(size[0] * bytes_per_texel),
            rows_per_image: Some(size[1]),
        },
        wgpu::Extent3d {
            width: size[0],
            height: size[1],
            depth_or_array_layers: 1,
        },
    );
}

/// A buffer holding `bytes`, at least 16 long (empty bindings are invalid).
fn buffer_init(gpu: &Gpu, label: &str, bytes: &[u8], usage: wgpu::BufferUsages) -> wgpu::Buffer {
    let mut padded;
    let contents = if bytes.len() < 16 {
        padded = [0u8; 16].to_vec();
        padded[..bytes.len()].copy_from_slice(bytes);
        padded.as_slice()
    } else {
        bytes
    };
    gpu.device
        .create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some(label),
            contents,
            usage,
        })
}

/// GPU copies shared by a renderer's viewports, keyed by source `Arc`
/// pointer. Entries hold their source, so a key cannot be reused while
/// resident; dropping an entry frees its GPU memory once no frame uses it.
#[derive(Default)]
struct Assets {
    meshes: HashMap<usize, Arc<ResidentMesh>>,
    splats: HashMap<usize, Arc<ResidentSplat>>,
    textures: HashMap<usize, Arc<StreamedTexture>>,
    /// Splat layer arrays by their images: a splat rebuilt over the same
    /// layers (undoing paint) keeps the arrays uploaded.
    arrays: HashMap<Vec<usize>, Arc<StreamedTexture>>,
    heights: HashMap<usize, Arc<ResidentHeights>>,
    /// Displaced meshes' color maps.
    color_maps: HashMap<usize, Arc<ResidentControl>>,
    /// Bumped whenever a height map edit reaches the GPU, so the shadow
    /// map knows to redraw.
    height_edits: u64,
    /// Overrides the device's BC support, for tests of the decode path.
    bc: Option<bool>,
    /// `Some(false)` keeps placements on per-piece draws, for tests of
    /// that path on devices that could pack them.
    packed: Option<bool>,
    placed: placed::Placed,
    /// Scenes presented so far, and (that, `placed.version`) as of the last
    /// `collect`.
    presented: u64,
    collected: Option<(u64, u64)>,
}

fn image_key(image: &Image) -> usize {
    match image {
        Image::Rgba(t) => key(t),
        Image::Compressed(c) => key(c),
        Image::Releasable(r) => key(r),
    }
}

impl Assets {
    /// Drops entries nothing references any more. Meshes go first: they
    /// hold the splats and textures. References only go when a scene is
    /// presented or the placement table changes, so the sweep waits for
    /// one: walking every mesh's refcount each frame cost 0.4 ms on a 768 m
    /// island.
    fn collect(&mut self) {
        let version = (self.presented, self.placed.version);
        if self.collected == Some(version) {
            return;
        }
        self.collected = Some(version);
        let _span = crate::profile::span("gpu collect");
        self.meshes.retain(|_, m| Arc::strong_count(m) > 1);
        self.splats.retain(|_, s| Arc::strong_count(s) > 1);
        self.textures.retain(|_, t| Arc::strong_count(t) > 1);
        self.arrays.retain(|_, t| Arc::strong_count(t) > 1);
        self.heights.retain(|_, h| Arc::strong_count(h) > 1);
        self.color_maps.retain(|_, c| Arc::strong_count(c) > 1);
    }

    /// Writes height and splat control edits made since the last frame to
    /// their GPU copies. Not budgeted: a dab is a few KB to a MB and must
    /// show on the next frame.
    fn sync_heights(&mut self, gpu: &Gpu, kit: &Kit, encoder: &mut wgpu::CommandEncoder) {
        let queue = &gpu.queue;
        let mut changed = false;
        for heights in self.heights.values() {
            changed |= heights.sync(queue);
        }
        // Only heights move shadows; control edits just need the upload
        // and the palettes over them rebuilt.
        self.height_edits += u64::from(changed);
        for splat in self.splats.values() {
            let mut dirty = splat.dirty.lock().unwrap().take();
            for control in splat.indices.iter().chain(&splat.weights) {
                if let Some(rect) = control.sync(queue) {
                    dirty = Some(dirty.map_or(rect, |d| d.union(rect)));
                }
            }
            if let Some(rect) = dirty {
                let controls = [splat.indices.as_slice(), splat.weights.as_slice()];
                let layers = splat.layers.images.len() as u32;
                (splat.palette).build(gpu, encoder, &kit.palette, controls, layers, rect);
            }
        }
        for colors in self.color_maps.values() {
            colors.sync(queue);
        }
    }

    fn bc(&self, gpu: &Gpu) -> bool {
        self.bc.unwrap_or_else(|| bc_supported(&gpu.device))
    }

    fn texture(&mut self, gpu: &Gpu, source: &Image) -> Arc<StreamedTexture> {
        let bc = self.bc(gpu);
        self.textures
            .entry(image_key(source))
            .or_insert_with(|| {
                Arc::new(
                    StreamedTexture::new(&gpu.device, vec![source.clone()], false, bc)
                        .expect("validated mesh texture"),
                )
            })
            .clone()
    }

    fn splat(&mut self, gpu: &Gpu, source: &Arc<Splat>) -> Arc<ResidentSplat> {
        if let Some(splat) = self.splats.get(&key(source)) {
            return splat.clone();
        }
        let info = source
            .tiling
            .iter()
            .enumerate()
            .map(|(i, tiling)| GpuLayer {
                scale: 1. / tiling,
                sharpness: source.sharpness.get(i).copied().unwrap_or(0.),
            })
            .collect::<Vec<_>>();
        let bc = self.bc(gpu);
        let palette = control::Palette::new(gpu, source.indices[0].size());
        let whole = palette.whole();
        let mut array = |images: &[Image]| {
            let key = images.iter().map(image_key).collect();
            self.arrays
                .entry(key)
                .or_insert_with(|| {
                    Arc::new(
                        StreamedTexture::new(&gpu.device, images.to_vec(), true, bc)
                            .expect("validated splat layers"),
                    )
                })
                .clone()
        };
        let layers = array(&source.layers);
        let normals = (!source.normals.is_empty()).then(|| array(&source.normals));
        let splat = Arc::new(ResidentSplat {
            _source: source.clone(),
            layers,
            normals,
            layer_info: buffer_init(
                gpu,
                "splat layers",
                bytemuck::cast_slice(&info),
                wgpu::BufferUsages::STORAGE,
            ),
            indices: (source.indices.iter())
                .map(|m| ResidentControl::new(gpu, m.clone()))
                .collect(),
            weights: (source.weights.iter())
                .map(|m| ResidentControl::new(gpu, m.clone()))
                .collect(),
            palette,
            dirty: Mutex::new(Some(whole)),
        });
        self.splats.insert(key(source), splat.clone());
        splat
    }

    /// The GPU copy of `source` with its material bind group. Geometry
    /// uploads at once; textures are created empty and fill through
    /// `stream`.
    fn mesh(
        &mut self,
        gpu: &Gpu,
        kit: &Kit,
        source: &Arc<Mesh>,
        budget: &mut Budget,
    ) -> Arc<ResidentMesh> {
        self.mesh_with(gpu, kit, source, budget, true)
    }

    /// `mesh`, or without `geometry` one that binds only its material and
    /// textures: for placement pieces the arena draws, which hold their
    /// geometry once, in the arena. Such meshes key apart from full ones
    /// (`Arc` addresses are even), so a mesh can be both.
    fn mesh_with(
        &mut self,
        gpu: &Gpu,
        kit: &Kit,
        source: &Arc<Mesh>,
        budget: &mut Budget,
        geometry: bool,
    ) -> Arc<ResidentMesh> {
        let slot = key(source) | usize::from(!geometry);
        if let Some(mesh) = self.meshes.get(&slot) {
            return mesh.clone();
        }
        let data = source.data();
        let albedo = data
            .texture
            .as_ref()
            .filter(|_| matches!(data.material, Material::Lit | Material::Blend(_)))
            .map(|t| self.texture(gpu, t));
        let lit = matches!(data.material, Material::Lit);
        let mut map = |image: &Option<Image>| {
            (image.as_ref())
                .filter(|_| lit)
                .map(|t| self.texture(gpu, t))
        };
        let maps = [map(&data.normal_map), map(&data.surface_map)];
        let splat = match &data.material {
            Material::Splat(splat) => Some(self.splat(gpu, splat)),
            _ => None,
        };
        let heights = data.heights.as_ref().map(|map| {
            self.heights
                .entry(key(map))
                .or_insert_with(|| {
                    let (resident, bytes) = ResidentHeights::new(gpu, map.clone());
                    budget.bytes += bytes;
                    Arc::new(resident)
                })
                .clone()
        });
        let colors = data.color_map.as_ref().map(|map| {
            self.color_maps
                .entry(key(map))
                .or_insert_with(|| {
                    let [w, h] = map.size();
                    budget.charge(w * h * 4);
                    Arc::new(ResidentControl::new(gpu, map.clone()))
                })
                .clone()
        });
        let storage = wgpu::BufferUsages::STORAGE;
        let (vertex_buffer, index_buffer) = if geometry {
            let mut vertices = vec![GpuVertex::zeroed(); source.vertex_count()];
            source.write_vertices(&mut vertices);
            let mut indices = vec![0; source.index_count()];
            source.write_indices(&mut indices, 0);
            budget.charge(std::mem::size_of_val(vertices.as_slice()) + indices.len() * 4);
            (
                buffer_init(
                    gpu,
                    "mesh vertices",
                    bytemuck::cast_slice(&vertices),
                    storage,
                ),
                // An index buffer for drawing, storage for the bind group.
                buffer_init(
                    gpu,
                    "mesh indices",
                    bytemuck::cast_slice(&indices),
                    storage | wgpu::BufferUsages::INDEX,
                ),
            )
        } else {
            let empty = || buffer_init(gpu, "no geometry", &[], storage);
            (empty(), empty())
        };
        let mut params = Params {
            surface: [1.; 4],
            flags: [
                if source.has_normals() { 1. } else { 0. },
                0.,
                0.,
                if data.ignore_alpha { 1. } else { 0. },
            ],
            blend: [0.; 4],
            tone: [
                data.color_gain,
                0.,
                f32::from(u8::from(maps[0].is_some())),
                f32::from(u8::from(maps[1].is_some())),
            ],
            terrain: [0.; 4],
            extra: [data.terrain_tint, 0., 0., 0.],
        };
        if let Material::Blend(blend) = &data.material {
            let [r, g, b] = rgb(blend.color);
            params.surface = [r, g, b, blend.opacity];
            params.blend = [blend.fade, blend.fresnel, blend.depth, 0.];
        }
        if let Some(heights) = &heights {
            let grid = heights.source.grid();
            // w: 1 displaced, 2 displaced with UVs from world X/Z.
            let mode = if source.has_uvs() { 1. } else { 2. };
            params.terrain = [grid.origin[0], grid.origin[1], grid.spacing, mode];
            // w: colors from the color map.
            params.blend[3] = if colors.is_some() { 1. } else { 0. };
            // The vertex shader derives normals from the heights.
            params.flags[0] = 1.;
        }
        if let Some(splat) = &splat {
            params.flags[1] = splat.indices.len() as f32;
            let [w, h] = splat._source.indices[0].size();
            params.extra[1] = w as f32;
            params.extra[2] = h as f32;
            params.flags[2] = splat.layers.images.len() as f32;
            params.tone[1] = if splat.normals.is_some() { 1. } else { 0. };
        }
        let params = buffer_init(
            gpu,
            "mesh params",
            bytemuck::bytes_of(&params),
            wgpu::BufferUsages::UNIFORM,
        );
        let d = &kit.dummy;
        fn texture<'a>(
            t: Option<&'a wgpu::TextureView>,
            fallback: &'a wgpu::TextureView,
        ) -> wgpu::BindingResource<'a> {
            wgpu::BindingResource::TextureView(t.unwrap_or(fallback))
        }
        // Splats read their palettes, not the control maps.
        let palette = splat.as_ref().map(|s| &s.palette);
        let weights = |i: usize| palette.map(|p| &p.weights[i]);
        let ids = |i: usize| palette.map(|p| &p.ids[i]);
        let group = gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("mesh"),
            layout: &kit.mesh_layout,
            entries: &[
                entry(0, params.as_entire_binding()),
                entry(1, vertex_buffer.as_entire_binding()),
                entry(2, index_buffer.as_entire_binding()),
                entry(3, texture(albedo.as_ref().map(|t| &t.view), &d.white)),
                entry(
                    4,
                    splat.as_ref().map_or(d.buffer.as_entire_binding(), |s| {
                        s.layer_info.as_entire_binding()
                    }),
                ),
                entry(5, texture(splat.as_ref().map(|s| &s.layers.view), &d.array)),
                entry(6, texture(weights(0), &d.black)),
                entry(7, texture(weights(1), &d.black)),
                entry(8, texture(ids(0), &d.black)),
                entry(9, texture(ids(1), &d.black)),
                entry(10, texture(heights.as_ref().map(|h| &h.view), &d.heights)),
                entry(11, texture(colors.as_ref().map(|c| &c.view), &d.white)),
                entry(
                    12,
                    texture(
                        splat
                            .as_ref()
                            .and_then(|s| s.normals.as_ref())
                            .map(|n| &n.view),
                        &d.array,
                    ),
                ),
                entry(13, texture(maps[0].as_ref().map(|t| &t.view), &d.white)),
                entry(14, texture(maps[1].as_ref().map(|t| &t.view), &d.white)),
            ],
        });
        let mesh = Arc::new(ResidentMesh {
            source: source.clone(),
            count: source.index_count() as u32,
            indices: index_buffer.clone(),
            albedo,
            maps,
            splat,
            _heights: heights,
            colors,
            group,
        });
        self.meshes.insert(slot, mesh.clone());
        mesh
    }

    /// Streams `mesh`'s textures within the budget. True once all are complete.
    fn stream(&mut self, gpu: &Gpu, mesh: &ResidentMesh, budget: &mut Budget) -> bool {
        // Control maps upload whole when created; only layers stream.
        let splat = (mesh.splat.iter()).flat_map(|s| std::iter::once(&s.layers).chain(&s.normals));
        mesh.albedo
            .iter()
            .chain(mesh.maps.iter().flatten())
            .chain(splat)
            .all(|texture| texture.stream(&gpu.queue, budget))
    }
}

fn entry(binding: u32, resource: wgpu::BindingResource<'_>) -> wgpu::BindGroupEntry<'_> {
    wgpu::BindGroupEntry { binding, resource }
}

struct ResidentMesh {
    source: Arc<Mesh>,
    count: u32,
    /// Drawn indexed (vs_mesh takes the vertex index as the vertex), so
    /// the post-transform cache shades shared vertices once. Empty for
    /// meshes the arena draws.
    indices: wgpu::Buffer,
    albedo: Option<Arc<StreamedTexture>>,
    /// `MeshData::normal_map` and `surface_map`.
    maps: [Option<Arc<StreamedTexture>>; 2],
    splat: Option<Arc<ResidentSplat>>,
    /// Held so `Assets::collect` keeps the height and color maps the bind
    /// group reads.
    _heights: Option<Arc<ResidentHeights>>,
    colors: Option<Arc<ResidentControl>>,
    /// Material, geometry and textures (bind group 1). Holds its buffers.
    group: wgpu::BindGroup,
}

impl ResidentMesh {
    /// Draws `instances` of the mesh with the pass's bound pipeline and
    /// groups.
    fn draw(&self, pass: &mut wgpu::RenderPass<'_>, instances: std::ops::Range<u32>) {
        pass.set_index_buffer(self.indices.slice(..), wgpu::IndexFormat::Uint32);
        pass.draw_indexed(0..self.count, 0, instances);
    }
}

struct ResidentSplat {
    _source: Arc<Splat>,
    layers: Arc<StreamedTexture>,
    normals: Option<Arc<StreamedTexture>>,
    layer_info: wgpu::Buffer,
    indices: Vec<ResidentControl>,
    weights: Vec<ResidentControl>,
    /// What fs_splat reads, rebuilt where the controls change.
    palette: control::Palette,
    /// Control texels whose palette tiles are still to build (all of them
    /// at first).
    dirty: Mutex<Option<Rect>>,
}

/// The device format for a block. Sampled as stored (UNORM) even when the
/// file says sRGB: the renderer shades in display space, as RGBA textures do.
fn block_format(block: Block) -> wgpu::TextureFormat {
    match block {
        Block::Bc1 => wgpu::TextureFormat::Bc1RgbaUnorm,
        Block::Bc2 => wgpu::TextureFormat::Bc2RgbaUnorm,
        Block::Bc3 => wgpu::TextureFormat::Bc3RgbaUnorm,
        Block::Bc4 => wgpu::TextureFormat::Bc4RUnorm,
        Block::Bc5 => wgpu::TextureFormat::Bc5RgUnorm,
        Block::Bc7 => wgpu::TextureFormat::Bc7RgbaUnorm,
    }
}

/// What drawing placements from one arena needs (placed.rs): indirect
/// draws that start at a first instance, and texture arrays indexed per
/// fragment. GPUI's device and `Offscreen` ask for them where offered.
pub(crate) const ARENA_FEATURES: wgpu::Features = wgpu::Features::INDIRECT_FIRST_INSTANCE
    .union(wgpu::Features::TEXTURE_BINDING_ARRAY)
    .union(wgpu::Features::SAMPLED_TEXTURE_AND_STORAGE_BUFFER_ARRAY_NON_UNIFORM_INDEXING);

/// Whether the device samples BC textures: GPUI's device asks for the
/// feature where the adapter has it (vendor/zed/PATCH.md).
pub fn bc_supported(device: &wgpu::Device) -> bool {
    device
        .features()
        .contains(wgpu::Features::TEXTURE_COMPRESSION_BC)
}

/// A device texture filled from its images a band of rows at a time, every
/// mip of a layer before the next layer, within a frame's `Budget`. Draw
/// with it only once `stream` reports it complete.
pub struct StreamedTexture {
    images: Vec<Image>,
    layout: Layout,
    /// Compressed images upload decoded to RGBA, band by band: the device
    /// cannot sample BC, or the images mix formats.
    decode: bool,
    texture: wgpu::Texture,
    view: wgpu::TextureView,
    /// Next layer, level and band row. A row is one texel row of an RGBA
    /// image, or one row of 4x4 blocks of a compressed one.
    progress: Mutex<(u32, u32, u32)>,
}

impl StreamedTexture {
    /// An empty texture for validated `images` that fit one `Layout`, with a
    /// `D2Array` view when `array`. Without `bc` compressed images decode
    /// to RGBA band by band.
    pub fn new(device: &wgpu::Device, images: Vec<Image>, array: bool, bc: bool) -> Result<Self> {
        for image in &images {
            image.validate()?;
        }
        let layout = Layout::new(&images)?;
        let limits = device.limits();
        anyhow::ensure!(
            array || images.len() == 1,
            "A 2D texture needs exactly one image"
        );
        anyhow::ensure!(
            images.len() <= limits.max_texture_array_layers as usize
                && layout.width <= limits.max_texture_dimension_2d
                && layout.height <= limits.max_texture_dimension_2d,
            "Texture exceeds device limits"
        );
        let bc = bc && bc_supported(device);
        // wgpu wants block-aligned base sizes; odd ones decode.
        let aligned = layout.width.is_multiple_of(4) && layout.height.is_multiple_of(4);
        let block = layout.block.filter(|_| bc && aligned);
        if layout.block.is_some() && block.is_none() {
            static WARNED: std::sync::Once = std::sync::Once::new();
            WARNED.call_once(|| {
                eprintln!("no BC texture support here; decoding compressed textures to RGBA");
            });
        }
        // Unorm, not sRGB: game albedo is displayed as stored, matching how
        // GPUI composites its own images.
        let format = block.map_or(wgpu::TextureFormat::Rgba8Unorm, block_format);
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("editor texture"),
            size: wgpu::Extent3d {
                width: layout.width,
                height: layout.height,
                depth_or_array_layers: images.len() as u32,
            },
            mip_level_count: layout.levels,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::COPY_DST | wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        });
        let view = texture.create_view(&wgpu::TextureViewDescriptor {
            dimension: Some(if array {
                wgpu::TextureViewDimension::D2Array
            } else {
                wgpu::TextureViewDimension::D2
            }),
            ..Default::default()
        });
        Ok(Self {
            images,
            layout,
            decode: block.is_none(),
            texture,
            view,
            progress: Mutex::new((0, 0, 0)),
        })
    }

    /// View to bind once `stream` reports completion.
    pub fn view(&self) -> &wgpu::TextureView {
        &self.view
    }

    /// Uploads bands until done or the budget is spent. True once complete.
    pub fn stream(&self, queue: &wgpu::Queue, budget: &mut Budget) -> bool {
        let mut progress = self.progress.lock().unwrap();
        while let Some(image) = self.images.get(progress.0 as usize) {
            if budget.spent() {
                return false;
            }
            let (layer, level, row) = *progress;
            let width = (self.layout.width >> level).max(1);
            let height = (self.layout.height >> level).max(1);
            // Texel rows per band row, source bytes per band row, and the level.
            let mip = (self.layout.skips[layer as usize] + level) as usize;
            let held;
            let (texels, source_row, data) = match image {
                Image::Rgba(t) => (1, width as usize * 4, t.rgba.as_slice()),
                Image::Compressed(c) => (4, c.block.size(width, 1), c.mips[mip].as_slice()),
                Image::Releasable(r) => {
                    held = r.texels();
                    let Some(c) = &held else {
                        // Gone from disk since: the layer stays empty.
                        *progress = (layer + 1, 0, 0);
                        continue;
                    };
                    (4, c.block.size(width, 1), c.mips[mip].as_slice())
                }
            };
            let decode = self.decode && image.block().is_some();
            let (upload_row, room) = if decode {
                (
                    width as usize * 4 * texels as usize,
                    DECODE_BAND_BYTES.min(budget.room()),
                )
            } else {
                (source_row, budget.room())
            };
            let rows = height.div_ceil(texels);
            let count = (room / upload_row).clamp(1, (rows - row) as usize) as u32;
            let band = &data[row as usize * source_row..(row + count) as usize * source_row];
            let band_height = (count * texels).min(height - row * texels);
            let decoded;
            let (bytes, bytes_per_row, extent) = match image.block() {
                Some(block) if decode => {
                    decoded = Texture::decode(block, width, band_height, band)
                        .expect("validated mip")
                        .rgba;
                    (decoded.as_slice(), width * 4, [width, band_height])
                }
                // Blocks cover whole 4x4 texels, also past a small mip's edge.
                Some(_) => (band, source_row as u32, [width.div_ceil(4) * 4, count * 4]),
                None => (band, source_row as u32, [width, band_height]),
            };
            queue.write_texture(
                wgpu::TexelCopyTextureInfo {
                    texture: &self.texture,
                    mip_level: level,
                    origin: wgpu::Origin3d {
                        x: 0,
                        y: row * texels,
                        z: layer,
                    },
                    aspect: wgpu::TextureAspect::All,
                },
                bytes,
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(bytes_per_row),
                    rows_per_image: None,
                },
                wgpu::Extent3d {
                    width: extent[0],
                    height: extent[1],
                    depth_or_array_layers: 1,
                },
            );
            budget.charge(bytes.len());
            *progress = if row + count < rows {
                (layer, level, row + count)
            } else if level + 1 < self.layout.levels {
                (layer, level + 1, 0)
            } else {
                // The layer is on the GPU: its texels may go.
                if let Image::Releasable(r) = image {
                    r.release();
                    RELEASED.store(true, std::sync::atomic::Ordering::Relaxed);
                }
                (layer + 1, 0, 0)
            };
        }
        true
    }
}

/// Releasable texels were let go since the heap was last trimmed.
static RELEASED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Hands freed heap back to the system, on a thread. glibc keeps what its
/// per-thread arenas free: after the continental island's 418 MB of model
/// texels went, the process stayed as large. The renderer trims once its
/// uploads let texels go; hosts call this after edits free much.
pub fn trim_heap() {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    std::thread::spawn(|| {
        // SAFETY: malloc_trim only releases free memory; it is thread-safe.
        unsafe { libc::malloc_trim(0) };
    });
}

/// One batch of the presented scene: its mesh, and its instances' place in
/// the scene's instance buffer.
struct Draw {
    mesh: Arc<ResidentMesh>,
    first: u32,
    count: u32,
    /// Whether its instances write pick handles (they have ids).
    pickable: bool,
}

fn texture_2d(
    gpu: &Gpu,
    label: &str,
    size: [u32; 2],
    format: wgpu::TextureFormat,
    usage: wgpu::TextureUsages,
) -> (wgpu::Texture, wgpu::TextureView) {
    let texture = gpu.device.create_texture(&wgpu::TextureDescriptor {
        label: Some(label),
        size: wgpu::Extent3d {
            width: size[0].max(1),
            height: size[1].max(1),
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage,
        view_formats: &[],
    });
    let view = texture.create_view(&Default::default());
    (texture, view)
}

/// A sun shadow map over the whole scene, not the view: it is drawn again
/// only when what casts or receives shadows changes, never for a camera
/// move.
struct ShadowMap {
    view: wgpu::TextureView,
    resolution: u32,
    /// The scene generation, height edits and placement version drawn.
    drawn: Option<(u64, u64, u64)>,
}

/// Bindings for slots a draw does not use.
struct Dummies {
    buffer: wgpu::Buffer,
    white: wgpu::TextureView,
    black: wgpu::TextureView,
    array: wgpu::TextureView,
    heights: wgpu::TextureView,
    depth: wgpu::TextureView,
}

/// Bind group layouts, samplers and dummies: what pipelines and meshes are
/// built against, the same for every target format.
struct Kit {
    frame_layout: wgpu::BindGroupLayout,
    mesh_layout: wgpu::BindGroupLayout,
    draw_layout: wgpu::BindGroupLayout,
    cull_layout: wgpu::BindGroupLayout,
    /// Placement transforms and visible entries for placed.wgsl.
    placed_layout: wgpu::BindGroupLayout,
    /// Depth pyramid steps (hzb.wgsl): from the depth target, and level to
    /// level.
    hzb_depth_layout: wgpu::BindGroupLayout,
    hzb_down_layout: wgpu::BindGroupLayout,
    /// The placement arena's layout and texture slots, where the device
    /// has multi-draw first instances and non-uniformly indexed texture
    /// arrays (placed.rs).
    arena: Option<(wgpu::BindGroupLayout, u32)>,
    /// Baked impostor views (placed.wgsl group 1).
    atlas_layout: wgpu::BindGroupLayout,
    /// The raw occlusion texture, for the pass that blurs and applies it.
    occlusion_layout: wgpu::BindGroupLayout,
    /// Builds splat palettes from control maps (palette.wgsl).
    palette: (wgpu::BindGroupLayout, wgpu::ComputePipeline),
    sampler: wgpu::Sampler,
    compare: wgpu::Sampler,
    dummy: Dummies,
    /// Dynamic uniform offsets step by this.
    align: u64,
}

fn layout_entry(
    binding: u32,
    visibility: wgpu::ShaderStages,
    ty: wgpu::BindingType,
) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility,
        ty,
        count: None,
    }
}

fn uniform(dynamic: bool) -> wgpu::BindingType {
    wgpu::BindingType::Buffer {
        ty: wgpu::BufferBindingType::Uniform,
        has_dynamic_offset: dynamic,
        min_binding_size: None,
    }
}

fn storage(read_only: bool) -> wgpu::BindingType {
    wgpu::BindingType::Buffer {
        ty: wgpu::BufferBindingType::Storage { read_only },
        has_dynamic_offset: false,
        min_binding_size: None,
    }
}

fn sampled(filterable: bool, dimension: wgpu::TextureViewDimension) -> wgpu::BindingType {
    wgpu::BindingType::Texture {
        sample_type: wgpu::TextureSampleType::Float { filterable },
        view_dimension: dimension,
        multisampled: false,
    }
}

fn depth_texture() -> wgpu::BindingType {
    wgpu::BindingType::Texture {
        sample_type: wgpu::TextureSampleType::Depth,
        view_dimension: wgpu::TextureViewDimension::D2,
        multisampled: false,
    }
}

impl Kit {
    fn new(gpu: &Gpu) -> Self {
        use wgpu::ShaderStages as S;
        let d = &gpu.device;
        let both = S::VERTEX_FRAGMENT;
        let d2 = wgpu::TextureViewDimension::D2;
        let frame_layout = d.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("editor frame"),
            entries: &[
                layout_entry(0, both, uniform(false)),
                layout_entry(1, both, uniform(false)),
                layout_entry(
                    2,
                    S::FRAGMENT,
                    wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                ),
                layout_entry(3, S::FRAGMENT, depth_texture()),
                layout_entry(
                    4,
                    S::FRAGMENT,
                    wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Comparison),
                ),
                layout_entry(5, S::FRAGMENT, depth_texture()),
                layout_entry(6, both, storage(true)),
                layout_entry(7, S::VERTEX, storage(true)),
                layout_entry(8, S::FRAGMENT, sampled(false, d2)),
            ],
        });
        let mesh_layout = d.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("editor mesh"),
            entries: &[
                layout_entry(0, both, uniform(false)),
                layout_entry(1, both, storage(true)),
                layout_entry(2, both, storage(true)),
                layout_entry(3, S::FRAGMENT, sampled(true, d2)),
                layout_entry(4, S::FRAGMENT, storage(true)),
                layout_entry(
                    5,
                    S::FRAGMENT,
                    sampled(true, wgpu::TextureViewDimension::D2Array),
                ),
                layout_entry(6, S::FRAGMENT, sampled(true, d2)),
                layout_entry(7, S::FRAGMENT, sampled(true, d2)),
                layout_entry(8, S::FRAGMENT, sampled(true, d2)),
                layout_entry(9, S::FRAGMENT, sampled(true, d2)),
                // R32Float is not filterable everywhere; heights are loaded.
                layout_entry(10, S::VERTEX, sampled(false, d2)),
                layout_entry(11, S::VERTEX, sampled(false, d2)),
                layout_entry(
                    12,
                    S::FRAGMENT,
                    sampled(true, wgpu::TextureViewDimension::D2Array),
                ),
                layout_entry(13, S::FRAGMENT, sampled(true, d2)),
                layout_entry(14, S::FRAGMENT, sampled(true, d2)),
            ],
        });
        let draw_layout = d.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("editor draw"),
            entries: &[
                layout_entry(0, S::VERTEX, uniform(true)),
                layout_entry(1, S::VERTEX, storage(true)),
                layout_entry(2, S::VERTEX, storage(true)),
            ],
        });
        let cull_layout = d.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("editor cull"),
            entries: &[
                layout_entry(0, S::COMPUTE, uniform(false)),
                layout_entry(1, S::COMPUTE, storage(true)),
                layout_entry(2, S::COMPUTE, storage(true)),
                layout_entry(3, S::COMPUTE, storage(true)),
                layout_entry(4, S::COMPUTE, storage(true)),
                layout_entry(5, S::COMPUTE, storage(false)),
                layout_entry(6, S::COMPUTE, storage(false)),
                layout_entry(7, S::COMPUTE, sampled(false, d2)),
                layout_entry(8, S::COMPUTE, storage(false)),
                layout_entry(9, S::COMPUTE, storage(false)),
                layout_entry(10, S::COMPUTE, storage(false)),
            ],
        });
        let pyramid_out = wgpu::BindingType::StorageTexture {
            access: wgpu::StorageTextureAccess::WriteOnly,
            format: wgpu::TextureFormat::R32Float,
            view_dimension: d2,
        };
        let hzb_depth_layout = d.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("editor depth pyramid base"),
            entries: &[
                layout_entry(0, S::COMPUTE, depth_texture()),
                layout_entry(1, S::COMPUTE, pyramid_out),
            ],
        });
        let hzb_down_layout = d.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("editor depth pyramid level"),
            entries: &[
                layout_entry(1, S::COMPUTE, pyramid_out),
                layout_entry(2, S::COMPUTE, sampled(false, d2)),
            ],
        });
        let placed_layout = d.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("editor placed"),
            entries: &[
                layout_entry(0, S::VERTEX, storage(true)),
                layout_entry(1, S::VERTEX, storage(true)),
                layout_entry(2, S::VERTEX, storage(true)),
            ],
        });
        // 2048 albedo slots: a 768 m island's pieces use a few hundred.
        let cap = d
            .limits()
            .max_binding_array_elements_per_shader_stage
            .min(2048);
        let arena = (d.features().contains(ARENA_FEATURES) && cap >= 256).then(|| {
            let layout = d.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("editor placement arena"),
                entries: &[
                    layout_entry(0, both, storage(true)),
                    layout_entry(1, S::VERTEX, storage(true)),
                    layout_entry(2, S::VERTEX, storage(true)),
                    wgpu::BindGroupLayoutEntry {
                        binding: 3,
                        visibility: S::FRAGMENT,
                        ty: sampled(true, d2),
                        count: std::num::NonZeroU32::new(cap),
                    },
                    layout_entry(4, both, storage(true)),
                    layout_entry(5, S::VERTEX, storage(true)),
                ],
            });
            (layout, cap)
        });
        let palette = {
            let written = wgpu::BindingType::StorageTexture {
                access: wgpu::StorageTextureAccess::WriteOnly,
                format: wgpu::TextureFormat::Rgba8Unorm,
                view_dimension: d2,
            };
            let mut entries = vec![layout_entry(0, S::COMPUTE, uniform(false))];
            entries.extend((1..=4).map(|b| layout_entry(b, S::COMPUTE, sampled(false, d2))));
            entries.extend((5..=8).map(|b| layout_entry(b, S::COMPUTE, written)));
            let layout = d.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("splat palette"),
                entries: &entries,
            });
            let module = d.create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some("splat palette"),
                source: wgpu::ShaderSource::Wgsl(include_str!("gpu/palette.wgsl").into()),
            });
            let pipeline = d.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
                label: Some("splat palette"),
                layout: Some(&d.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                    label: Some("splat palette"),
                    bind_group_layouts: &[Some(&layout)],
                    immediate_size: 0,
                })),
                module: &module,
                entry_point: Some("cs_palette"),
                compilation_options: Default::default(),
                cache: None,
            });
            (layout, pipeline)
        };
        let array = wgpu::TextureViewDimension::D2Array;
        let occlusion_layout = d.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("editor occlusion"),
            entries: &[layout_entry(0, S::FRAGMENT, sampled(false, d2))],
        });
        let atlas_layout = d.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("editor impostor atlas"),
            entries: &[
                layout_entry(0, S::FRAGMENT, sampled(true, array)),
                layout_entry(1, S::FRAGMENT, sampled(true, array)),
            ],
        });
        // Trilinear and anisotropic: the views are often grazing.
        let sampler = d.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("editor albedo"),
            address_mode_u: wgpu::AddressMode::Repeat,
            address_mode_v: wgpu::AddressMode::Repeat,
            address_mode_w: wgpu::AddressMode::Repeat,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            mipmap_filter: wgpu::MipmapFilterMode::Linear,
            anisotropy_clamp: 8,
            ..Default::default()
        });
        let compare = d.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("editor shadow"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            compare: Some(wgpu::CompareFunction::LessEqual),
            ..Default::default()
        });
        let solid = |rgba: [u8; 4], array: bool| {
            let t =
                StreamedTexture::new(&gpu.device, vec![Texture::solid(rgba).into()], array, false)
                    .expect("valid fallback texture");
            t.stream(&gpu.queue, &mut Budget::new());
            t.view
        };
        let (heights, heights_view) = texture_2d(
            gpu,
            "dummy heights",
            [1, 1],
            wgpu::TextureFormat::R32Float,
            wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        );
        write_texture_rect(&gpu.queue, &heights, [0, 0], [1, 1], 4, &[0; 4]);
        let (_, depth) = texture_2d(
            gpu,
            "dummy depth",
            [1, 1],
            wgpu::TextureFormat::Depth32Float,
            wgpu::TextureUsages::TEXTURE_BINDING,
        );
        let dummy = Dummies {
            buffer: buffer_init(
                gpu,
                "dummy",
                bytemuck::cast_slice(&[math::IDENTITY]),
                wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::UNIFORM,
            ),
            white: solid([255; 4], false),
            black: solid([0; 4], false),
            array: solid([255; 4], true),
            heights: heights_view,
            depth,
        };
        Self {
            frame_layout,
            mesh_layout,
            draw_layout,
            cull_layout,
            placed_layout,
            hzb_depth_layout,
            hzb_down_layout,
            arena,
            atlas_layout,
            occlusion_layout,
            palette,
            sampler,
            compare,
            dummy,
            align: u64::from(d.limits().min_uniform_buffer_offset_alignment).max(16),
        }
    }
}

/// Which lists a cull pass fills (cull.wgsl).
#[derive(Clone, Copy, PartialEq)]
enum CullPhase {
    /// The shadow map's lists, without occlusion.
    Shadow,
    /// The view's, without occlusion (placements drawn one by one).
    All,
    /// The view's first phase: placements seen last frame.
    Seen,
    /// The view's second phase: placements the depth pyramid shows that
    /// the first phase skipped.
    Revealed,
}

#[derive(Clone, Copy, PartialEq)]
enum Pass {
    Shadow,
    Opaque,
    Blend,
    /// Markers: blended, above everything.
    Overlay,
    /// The sky: opaque, only where nothing opaque drew (depth still clear).
    Sky,
}

/// GPU state the viewports share: pipelines, the presented scene and the
/// shadow map.
struct Device {
    format: wgpu::TextureFormat,
    kit: Kit,
    /// Pipelines over the placement arena, with it: lit, flat and shadow
    /// meshes, impostors and their shadows, and impostor baking.
    arena: Option<[wgpu::RenderPipeline; 6]>,
    lit: wgpu::RenderPipeline,
    flat: wgpu::RenderPipeline,
    splat: wgpu::RenderPipeline,
    blend: wgpu::RenderPipeline,
    marker: wgpu::RenderPipeline,
    sky: wgpu::RenderPipeline,
    /// Estimating occlusion, and blurring and applying it.
    occlusion: [wgpu::RenderPipeline; 2],
    /// The selection outline (outline.wgsl) and its bind group layout.
    outline: (wgpu::BindGroupLayout, wgpu::RenderPipeline),
    shadow: wgpu::RenderPipeline,
    cull: wgpu::ComputePipeline,
    /// select.wgsl: its layout, then moving rows and writing their markers.
    select: (wgpu::BindGroupLayout, [wgpu::ComputePipeline; 2]),
    /// The outline's marks as of `State::outline_version`.
    outline_marks: Option<(u64, wgpu::Buffer)>,
    /// The scene's markers, for `markers` to hold the selection's after.
    scene_markers: Vec<GpuMarker>,
    /// What `markers` was made for: the scene generation and selection
    /// markers.
    markers_for: Option<(u64, usize)>,
    /// Depth pyramid from the depth target, and level to level.
    hzb: [wgpu::ComputePipeline; 2],
    shadow_map: Option<ShadowMap>,
    /// The presented scene's draws, in batch order.
    resident: Vec<Draw>,
    /// Every batch instance of the presented scene, batch after batch.
    instances: Option<wgpu::Buffer>,
    /// Batch instance ids by pick handle - 1, in `instances` order.
    pick_ids: Vec<Option<u64>>,
    /// The scene being uploaded, and its meshes so far.
    loading: Option<(u64, Vec<Arc<ResidentMesh>>)>,
    /// The mesh whose textures are mid-upload. No draw references it yet;
    /// holding it stops `Assets::collect` dropping a half-filled texture
    /// between frames and restarting the upload forever.
    streaming: Option<Arc<ResidentMesh>>,
    presented: Option<Arc<PreparedScene>>,
    markers: Option<(wgpu::Buffer, u32)>,
    /// Line segments: two points with the width and color packed in w;
    /// the scene's, then the overlay's, uploaded together whenever either
    /// changes.
    lines: Option<(wgpu::Buffer, u32)>,
    scene_segments: Vec<[[f32; 4]; 2]>,
    overlay_segments: Vec<[[f32; 4]; 2]>,
    line: wgpu::RenderPipeline,
}

/// What a viewport reads back from the GPU, a frame or two later.
enum ReadKind {
    /// The pick handle and depth under `cursor`, as `view` drew them.
    Pick { cursor: Point<Pixels>, view: View },
    /// Placement counts per draw, from the view's indirect arguments.
    Counts,
    /// Begin and end timestamps of the frame's passes, and their names.
    Times(Vec<&'static str>),
}

/// A host-readable copy recorded into a frame. It is mapped at the
/// viewport's next draw, when GPUI has submitted the frame (offscreen right
/// after its submit), and read when the mapping lands.
struct Readback {
    buffer: wgpu::Buffer,
    kind: ReadKind,
    /// 0 recorded, 1 mapping, 2 mapped, 3 failed.
    state: Arc<std::sync::atomic::AtomicU8>,
}

/// Timestamp queries for this viewport's passes.
struct Timing {
    set: wgpu::QuerySet,
    resolve: wgpu::Buffer,
    /// Queries written this frame.
    used: u32,
    /// Each timed pass's name this frame, in query order.
    labels: [&'static str; TIMED_PASSES as usize],
}

/// Room for this many timed passes per frame: two culls, shadow cull and
/// map, opaque, revealed, occlusion estimate and apply, blend.
const TIMED_PASSES: u32 = 12;

impl Timing {
    /// Begin and end slots for one more pass, while there is room.
    fn pass(&mut self, label: &'static str) -> Option<(u32, u32)> {
        (self.used + 2 <= TIMED_PASSES * 2).then(|| {
            self.labels[self.used as usize / 2] = label;
            self.used += 2;
            (self.used - 2, self.used - 1)
        })
    }

    fn render(&mut self, label: &'static str) -> Option<wgpu::RenderPassTimestampWrites<'_>> {
        let (begin, end) = self.pass(label)?;
        Some(wgpu::RenderPassTimestampWrites {
            query_set: &self.set,
            beginning_of_pass_write_index: Some(begin),
            end_of_pass_write_index: Some(end),
        })
    }

    fn compute(&mut self, label: &'static str) -> Option<wgpu::ComputePassTimestampWrites<'_>> {
        let (begin, end) = self.pass(label)?;
        Some(wgpu::ComputePassTimestampWrites {
            query_set: &self.set,
            beginning_of_pass_write_index: Some(begin),
            end_of_pass_write_index: Some(end),
        })
    }
}

/// One viewport's render targets, cull outputs and readbacks.
struct Targets {
    size: [u32; 2],
    depth: (wgpu::Texture, wgpu::TextureView),
    /// Opaque depth as the blend and occlusion passes sample it.
    depth_copy: (wgpu::Texture, wgpu::TextureView),
    /// Raw ambient occlusion, before its blur.
    occlusion: (wgpu::Texture, wgpu::TextureView),
    /// Pick handle and depth bits per pixel, written by the opaque pass.
    ids: (wgpu::Texture, wgpu::TextureView),
    cull: Option<placed::CullTargets>,
    readbacks: Vec<Readback>,
    timing: Option<Timing>,
    /// Per-draw handle slots and how many fit.
    draw_ids: Option<(wgpu::Buffer, usize)>,
    hzb: Hzb,
    /// This view's terrain tile matrices, grown as needed.
    terrain: Option<wgpu::Buffer>,
}

/// A view's depth pyramid for occlusion culling (hzb.wgsl): level 0 is half
/// the depth target, rounded up.
struct Hzb {
    /// Every level, as the cull pass reads it.
    view: wgpu::TextureView,
    size: [u32; 2],
    /// One bind group per level: from the depth target, then from the level
    /// above. Built with the targets; they only change with the size.
    steps: Vec<(wgpu::BindGroup, [u32; 2])>,
}

impl Hzb {
    fn new(gpu: &Gpu, kit: &Kit, depth: &wgpu::TextureView, target: [u32; 2]) -> Self {
        let size = target.map(|v| v.div_ceil(2).max(1));
        let levels = 32 - size[0].max(size[1]).leading_zeros();
        let texture = gpu.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("editor depth pyramid"),
            size: wgpu::Extent3d {
                width: size[0],
                height: size[1],
                depth_or_array_layers: 1,
            },
            mip_level_count: levels,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::R32Float,
            usage: wgpu::TextureUsages::STORAGE_BINDING | wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        });
        let level = |l| {
            texture.create_view(&wgpu::TextureViewDescriptor {
                base_mip_level: l,
                mip_level_count: Some(1),
                ..Default::default()
            })
        };
        let steps = (0..levels)
            .map(|l| {
                let out = level(l);
                let group = if l == 0 {
                    gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
                        label: Some("editor depth pyramid base"),
                        layout: &kit.hzb_depth_layout,
                        entries: &[
                            entry(0, wgpu::BindingResource::TextureView(depth)),
                            entry(1, wgpu::BindingResource::TextureView(&out)),
                        ],
                    })
                } else {
                    gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
                        label: Some("editor depth pyramid level"),
                        layout: &kit.hzb_down_layout,
                        entries: &[
                            entry(1, wgpu::BindingResource::TextureView(&out)),
                            entry(2, wgpu::BindingResource::TextureView(&level(l - 1))),
                        ],
                    })
                };
                (group, size.map(|v| (v >> l).max(1)))
            })
            .collect();
        Self {
            view: texture.create_view(&Default::default()),
            size,
            steps,
        }
    }

    fn levels(&self) -> u32 {
        self.steps.len() as u32
    }
}

impl Targets {
    fn new(gpu: &Gpu, kit: &Kit, size: [u32; 2]) -> Self {
        use wgpu::TextureUsages as U;
        let timing = gpu
            .device
            .features()
            .contains(wgpu::Features::TIMESTAMP_QUERY)
            .then(|| Timing {
                set: gpu.device.create_query_set(&wgpu::QuerySetDescriptor {
                    label: Some("editor timing"),
                    ty: wgpu::QueryType::Timestamp,
                    count: TIMED_PASSES * 2,
                }),
                resolve: gpu.device.create_buffer(&wgpu::BufferDescriptor {
                    label: Some("editor timing resolve"),
                    size: u64::from(TIMED_PASSES) * 16,
                    usage: wgpu::BufferUsages::QUERY_RESOLVE | wgpu::BufferUsages::COPY_SRC,
                    mapped_at_creation: false,
                }),
                used: 0,
                labels: [""; TIMED_PASSES as usize],
            });
        let depth = texture_2d(
            gpu,
            "editor depth",
            size,
            wgpu::TextureFormat::Depth32Float,
            // Sampled by the depth pyramid.
            U::RENDER_ATTACHMENT | U::COPY_SRC | U::TEXTURE_BINDING,
        );
        let hzb = Hzb::new(gpu, kit, &depth.1, size);
        Self {
            size,
            hzb,
            depth,
            depth_copy: texture_2d(
                gpu,
                "editor depth copy",
                size,
                wgpu::TextureFormat::Depth32Float,
                U::COPY_DST | U::TEXTURE_BINDING,
            ),
            // Half resolution: the estimate is blurred over 8x8 pixels anyway,
            // and at full size it cost 0.3 ms of a 1080p frame. Each texel
            // keeps its world point beside the estimate, so the blur's
            // same-surface test reads it instead of unprojecting depth.
            occlusion: texture_2d(
                gpu,
                "editor occlusion",
                size.map(|s| s.div_ceil(2)),
                wgpu::TextureFormat::Rgba32Float,
                U::RENDER_ATTACHMENT | U::TEXTURE_BINDING,
            ),
            ids: texture_2d(
                gpu,
                "editor pick ids",
                size,
                wgpu::TextureFormat::Rg32Uint,
                // Sampled by the selection outline.
                U::RENDER_ATTACHMENT | U::COPY_SRC | U::TEXTURE_BINDING,
            ),
            cull: None,
            readbacks: Vec::new(),
            timing,
            draw_ids: None,
            terrain: None,
        }
    }

    /// A buffer the frame copies `bytes` into for reading back as `kind`.
    fn readback(&mut self, gpu: &Gpu, bytes: u64, kind: ReadKind) -> &wgpu::Buffer {
        let buffer = gpu.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("editor readback"),
            size: bytes.max(16),
            usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        self.readbacks.push(Readback {
            buffer,
            kind,
            state: Default::default(),
        });
        &self.readbacks.last().expect("just pushed").buffer
    }
}

/// Octahedral unit vector in two snorm16 halves, x low.
fn octahedral(n: Vec3) -> u32 {
    let n = math::normalize(n).unwrap_or([0., 1., 0.]);
    let l1 = n[0].abs() + n[1].abs() + n[2].abs();
    let (mut x, mut y) = (n[0] / l1, n[1] / l1);
    if n[2] < 0. {
        // Fold the lower hemisphere over the diamond's edges. `signum` of
        // zero must be positive here, as the shader's decode assumes.
        let sign = |v: f32| if v >= 0. { 1. } else { -1. };
        (x, y) = ((1. - y.abs()) * sign(x), (1. - x.abs()) * sign(y));
    }
    let snorm = |v: f32| u32::from((v.clamp(-1., 1.) * 32767.).round() as i16 as u16);
    snorm(x) | snorm(y) << 16
}

fn vertices(data: &MeshData) -> Vec<GpuVertex> {
    let mut out = vec![GpuVertex::zeroed(); data.positions.len()];
    write_vertices(data, &mut out);
    out
}

/// `data`'s vertices as the shaders read them, into `out` (one per position).
fn write_vertices(data: &MeshData, out: &mut [GpuVertex]) {
    for (i, (p, v)) in data.positions.iter().zip(out).enumerate() {
        *v = GpuVertex {
            position: [p[0], p[1], p[2], 1.],
            uv: data.uvs.get(i).copied().unwrap_or_default(),
            color: data.colors.get(i).copied().unwrap_or(0xffffff),
            normal: data.normals.get(i).map_or(0, |n| octahedral(*n)),
        };
    }
}

fn rgb(color: u32) -> [f32; 3] {
    [16, 8, 0].map(|s| ((color >> s) & 255) as f32 / 255.)
}

impl Device {
    fn new(gpu: &Gpu, format: wgpu::TextureFormat) -> Self {
        let kit = Kit::new(gpu);
        let d = &gpu.device;
        let shader = d.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("editor"),
            source: wgpu::ShaderSource::Wgsl(include_str!("gpu.wgsl").into()),
        });
        let layout = |groups: &[&wgpu::BindGroupLayout]| {
            let groups: Vec<Option<&wgpu::BindGroupLayout>> =
                groups.iter().map(|g| Some(*g)).collect();
            d.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("editor"),
                bind_group_layouts: &groups,
                immediate_size: 0,
            })
        };
        let meshes = layout(&[&kit.frame_layout, &kit.mesh_layout, &kit.draw_layout]);
        let overlay = layout(&[&kit.frame_layout]);
        let arena_layout = kit.arena.as_ref().map(|(arena, _)| {
            d.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("editor placement arena"),
                bind_group_layouts: &[
                    Some(&kit.frame_layout),
                    None,
                    Some(&kit.placed_layout),
                    Some(arena),
                ],
                immediate_size: 0,
            })
        });
        // Compiled only with the arena: it declares a texture array, whose
        // enable directive must open the source.
        let arena_shader = kit.arena.as_ref().map(|_| {
            d.create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some("editor placement arena"),
                source: wgpu::ShaderSource::Wgsl(
                    [
                        "enable wgpu_binding_array;\n",
                        include_str!("gpu.wgsl"),
                        include_str!("gpu/placed.wgsl"),
                    ]
                    .concat()
                    .into(),
                ),
            })
        });
        let build = |name: &str,
                     module: &wgpu::ShaderModule,
                     layout: &wgpu::PipelineLayout,
                     vertex: &str,
                     fragment: &str,
                     pass: Pass| {
            let blend = matches!(pass, Pass::Blend | Pass::Overlay)
                .then_some(wgpu::BlendState::ALPHA_BLENDING);
            // The opaque pass also writes pick handles.
            let colors = [
                Some(wgpu::ColorTargetState {
                    format,
                    blend,
                    write_mask: wgpu::ColorWrites::ALL,
                }),
                Some(wgpu::ColorTargetState {
                    format: wgpu::TextureFormat::Rg32Uint,
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                }),
            ];
            let targets: &[Option<wgpu::ColorTargetState>] = match pass {
                Pass::Shadow => &[],
                Pass::Opaque => &colors,
                Pass::Blend | Pass::Overlay | Pass::Sky => &colors[..1],
            };
            d.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some(name),
                layout: Some(layout),
                vertex: wgpu::VertexState {
                    module,
                    entry_point: Some(vertex),
                    buffers: &[],
                    compilation_options: Default::default(),
                },
                fragment: Some(wgpu::FragmentState {
                    module,
                    entry_point: Some(fragment),
                    targets,
                    compilation_options: Default::default(),
                }),
                primitive: wgpu::PrimitiveState::default(),
                depth_stencil: Some(wgpu::DepthStencilState {
                    format: wgpu::TextureFormat::Depth32Float,
                    depth_write_enabled: Some(matches!(pass, Pass::Shadow | Pass::Opaque)),
                    depth_compare: Some(match pass {
                        Pass::Shadow => wgpu::CompareFunction::LessEqual,
                        Pass::Opaque | Pass::Blend | Pass::Sky => {
                            wgpu::CompareFunction::GreaterEqual
                        }
                        Pass::Overlay => wgpu::CompareFunction::Always,
                    }),
                    stencil: Default::default(),
                    bias: if pass == Pass::Shadow {
                        // Slope bias for surfaces nearly edge-on to the sun;
                        // the receiver's normal offset handles the rest.
                        wgpu::DepthBiasState {
                            constant: 0,
                            slope_scale: 1.5,
                            clamp: 0.,
                        }
                    } else {
                        Default::default()
                    },
                }),
                multisample: Default::default(),
                multiview_mask: None,
                cache: None,
            })
        };
        let pipeline = |name: &str, vertex: &str, fragment: &str, pass: Pass| {
            let layout = if matches!(pass, Pass::Overlay | Pass::Sky) {
                &overlay
            } else {
                &meshes
            };
            build(name, &shader, layout, vertex, fragment, pass)
        };
        let arena = arena_shader
            .as_ref()
            .zip(arena_layout.as_ref())
            .zip(kit.arena.as_ref())
            .map(|((module, layout), (arena, _))| {
                let impostors = d.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                    label: Some("editor impostors"),
                    bind_group_layouts: &[
                        Some(&kit.frame_layout),
                        Some(&kit.atlas_layout),
                        Some(&kit.placed_layout),
                        Some(arena),
                    ],
                    immediate_size: 0,
                });
                let [lit, flat, shadow] = [
                    ("editor placed lit", "fs_placed_lit", Pass::Opaque),
                    ("editor placed flat", "fs_placed_flat", Pass::Opaque),
                    ("editor placed shadow", "fs_placed_shadow", Pass::Shadow),
                ]
                .map(|(name, fragment, pass)| {
                    build(name, module, layout, "vs_placed", fragment, pass)
                });
                let [impostor, impostor_shadow] = [
                    ("editor impostors", "fs_impostor", Pass::Opaque),
                    (
                        "editor impostor shadows",
                        "fs_impostor_shadow",
                        Pass::Shadow,
                    ),
                ]
                .map(|(name, fragment, pass)| {
                    build(name, module, &impostors, "vs_impostor", fragment, pass)
                });
                // Color and octahedral model-space normals per view; nearer
                // is smaller z.
                let target = |format| {
                    Some(wgpu::ColorTargetState {
                        format,
                        blend: None,
                        write_mask: wgpu::ColorWrites::ALL,
                    })
                };
                // Baking reads meshes from the arena and nothing per placement.
                let baking = d.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                    label: Some("editor impostor bake"),
                    bind_group_layouts: &[Some(&kit.frame_layout), None, None, Some(arena)],
                    immediate_size: 0,
                });
                let bake = d.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                    label: Some("editor impostor bake"),
                    layout: Some(&baking),
                    vertex: wgpu::VertexState {
                        module,
                        entry_point: Some("vs_bake"),
                        buffers: &[],
                        compilation_options: Default::default(),
                    },
                    fragment: Some(wgpu::FragmentState {
                        module,
                        entry_point: Some("fs_bake"),
                        targets: &[
                            target(wgpu::TextureFormat::Rgba8Unorm),
                            target(wgpu::TextureFormat::Rg8Unorm),
                        ],
                        compilation_options: Default::default(),
                    }),
                    primitive: wgpu::PrimitiveState::default(),
                    depth_stencil: Some(wgpu::DepthStencilState {
                        format: wgpu::TextureFormat::Depth32Float,
                        depth_write_enabled: Some(true),
                        depth_compare: Some(wgpu::CompareFunction::Less),
                        stencil: Default::default(),
                        bias: Default::default(),
                    }),
                    multisample: Default::default(),
                    multiview_mask: None,
                    cache: None,
                });
                [lit, flat, shadow, impostor, impostor_shadow, bake]
            });
        let lit = pipeline("editor lit", "vs_mesh", "fs_lit", Pass::Opaque);
        let flat = pipeline("editor flat", "vs_mesh", "fs_flat", Pass::Opaque);
        let splat = pipeline("editor splat", "vs_mesh", "fs_splat", Pass::Opaque);
        let blend = pipeline("editor blend", "vs_mesh", "fs_blend", Pass::Blend);
        let marker = pipeline("editor marker", "vs_marker", "fs_marker", Pass::Overlay);
        let line = pipeline("editor line", "vs_line", "fs_line", Pass::Overlay);
        let sky = pipeline("editor sky", "vs_sky", "fs_sky", Pass::Sky);
        // Occlusion: estimate into an R8 target, then blur it and multiply
        // it over the color. Neither tests depth; both read a copy.
        let fullscreen = |name: &str, layout: &wgpu::PipelineLayout, fragment: &str, target| {
            d.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some(name),
                layout: Some(layout),
                vertex: wgpu::VertexState {
                    module: &shader,
                    entry_point: Some("vs_sky"),
                    buffers: &[],
                    compilation_options: Default::default(),
                },
                fragment: Some(wgpu::FragmentState {
                    module: &shader,
                    entry_point: Some(fragment),
                    targets: &[Some(target)],
                    compilation_options: Default::default(),
                }),
                primitive: wgpu::PrimitiveState::default(),
                depth_stencil: None,
                multisample: Default::default(),
                multiview_mask: None,
                cache: None,
            })
        };
        let multiply = wgpu::BlendComponent {
            src_factor: wgpu::BlendFactor::Zero,
            dst_factor: wgpu::BlendFactor::Src,
            operation: wgpu::BlendOperation::Add,
        };
        let occlusion = [
            fullscreen(
                "editor occlusion",
                &overlay,
                "fs_occlusion",
                wgpu::ColorTargetState {
                    format: wgpu::TextureFormat::Rgba32Float,
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                },
            ),
            fullscreen(
                "editor occlusion apply",
                &layout(&[&kit.frame_layout, &kit.occlusion_layout]),
                "fs_occlusion_apply",
                wgpu::ColorTargetState {
                    format,
                    blend: Some(wgpu::BlendState {
                        color: multiply,
                        alpha: wgpu::BlendComponent::REPLACE,
                    }),
                    write_mask: wgpu::ColorWrites::COLOR,
                },
            ),
        ];
        let shadow = pipeline("editor shadow", "vs_mesh", "fs_shadow", Pass::Shadow);
        let outline = {
            let group = d.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("editor outline"),
                entries: &[
                    layout_entry(
                        0,
                        wgpu::ShaderStages::FRAGMENT,
                        wgpu::BindingType::Texture {
                            sample_type: wgpu::TextureSampleType::Uint,
                            view_dimension: wgpu::TextureViewDimension::D2,
                            multisampled: false,
                        },
                    ),
                    layout_entry(1, wgpu::ShaderStages::FRAGMENT, storage(true)),
                ],
            });
            let module = d.create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some("editor outline"),
                source: wgpu::ShaderSource::Wgsl(include_str!("gpu/outline.wgsl").into()),
            });
            let pipeline = d.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some("editor outline"),
                layout: Some(&layout(&[&group])),
                vertex: wgpu::VertexState {
                    module: &module,
                    entry_point: Some("vs_outline"),
                    buffers: &[],
                    compilation_options: Default::default(),
                },
                fragment: Some(wgpu::FragmentState {
                    module: &module,
                    entry_point: Some("fs_outline"),
                    targets: &[Some(wgpu::ColorTargetState {
                        format,
                        blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                        write_mask: wgpu::ColorWrites::COLOR,
                    })],
                    compilation_options: Default::default(),
                }),
                primitive: wgpu::PrimitiveState::default(),
                depth_stencil: None,
                multisample: Default::default(),
                multiview_mask: None,
                cache: None,
            });
            (group, pipeline)
        };
        let cull_shader = d.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("editor cull"),
            source: wgpu::ShaderSource::Wgsl(include_str!("gpu/cull.wgsl").into()),
        });
        let cull = d.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
            label: Some("editor cull"),
            layout: Some(&layout(&[&kit.cull_layout])),
            module: &cull_shader,
            entry_point: Some("cs_cull"),
            compilation_options: Default::default(),
            cache: None,
        });
        let select = {
            use wgpu::ShaderStages as S;
            let select_layout = d.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("editor select"),
                entries: &[
                    layout_entry(0, S::COMPUTE, uniform(false)),
                    layout_entry(1, S::COMPUTE, storage(true)),
                    layout_entry(2, S::COMPUTE, storage(true)),
                    layout_entry(3, S::COMPUTE, storage(false)),
                    layout_entry(4, S::COMPUTE, storage(true)),
                    layout_entry(5, S::COMPUTE, storage(false)),
                ],
            });
            let module = d.create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some("editor select"),
                source: wgpu::ShaderSource::Wgsl(include_str!("gpu/select.wgsl").into()),
            });
            let pipeline = |entry: &str| {
                d.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
                    label: Some("editor select"),
                    layout: Some(&layout(&[&select_layout])),
                    module: &module,
                    entry_point: Some(entry),
                    compilation_options: Default::default(),
                    cache: None,
                })
            };
            let pipelines = [pipeline("cs_move"), pipeline("cs_marks")];
            (select_layout, pipelines)
        };
        let hzb_shader = d.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("editor depth pyramid"),
            source: wgpu::ShaderSource::Wgsl(include_str!("gpu/hzb.wgsl").into()),
        });
        let hzb = [
            ("cs_hzb_depth", &kit.hzb_depth_layout),
            ("cs_hzb_down", &kit.hzb_down_layout),
        ]
        .map(|(entry_point, group)| {
            d.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
                label: Some("editor depth pyramid"),
                layout: Some(&layout(&[group])),
                module: &hzb_shader,
                entry_point: Some(entry_point),
                compilation_options: Default::default(),
                cache: None,
            })
        });
        Self {
            format,
            kit,
            arena,
            hzb,
            lit,
            flat,
            splat,
            blend,
            marker,
            sky,
            occlusion,
            outline,
            shadow,
            cull,
            select,
            outline_marks: None,
            scene_markers: Vec::new(),
            markers_for: None,
            shadow_map: None,
            resident: Vec::new(),
            instances: None,
            pick_ids: Vec::new(),
            loading: None,
            streaming: None,
            presented: None,
            markers: None,
            lines: None,
            scene_segments: Vec::new(),
            overlay_segments: Vec::new(),
            line,
        }
    }

    fn upload_lines(&mut self, gpu: &Gpu) {
        let all: Vec<[[f32; 4]; 2]> =
            [&self.scene_segments[..], &self.overlay_segments[..]].concat();
        self.lines = (!all.is_empty()).then(|| {
            let buffer = buffer_init(
                gpu,
                "lines",
                bytemuck::cast_slice(&all),
                wgpu::BufferUsages::STORAGE,
            );
            (buffer, all.len() as u32)
        });
    }

    /// Uploads what `scene` still needs, within the frame budget, and
    /// presents it once complete. Until then the previous scene stays
    /// visible. A presented scene's instances go up as one buffer.
    fn update(&mut self, gpu: &Gpu, scene: &Arc<PreparedScene>, assets: &mut Assets) {
        if self
            .presented
            .as_ref()
            .is_some_and(|p| p.generation == scene.generation)
        {
            return;
        }
        if self
            .loading
            .as_ref()
            .is_none_or(|l| l.0 != scene.generation)
        {
            self.loading = Some((scene.generation, Vec::with_capacity(scene.batches.len())));
        }
        let mut budget = Budget::new();
        let mut added = 0;
        {
            let meshes = &mut self.loading.as_mut().expect("loading").1;
            while meshes.len() < scene.batches.len() {
                if budget.spent() || added >= UPLOAD_BATCHES {
                    return;
                }
                let batch = &scene.batches[meshes.len()].batch;
                let mesh = assets.mesh(gpu, &self.kit, &batch.mesh, &mut budget);
                if !assets.stream(gpu, &mesh, &mut budget) {
                    self.streaming = Some(mesh);
                    return;
                }
                meshes.push(mesh);
                added += 1;
            }
        }
        self.streaming = None;
        let (_, meshes) = self.loading.take().expect("loading");
        let total: usize = (scene.batches.iter())
            .map(|p| p.batch.instances.matrices.len())
            .sum();
        let mut matrices: Vec<Mat4> = Vec::with_capacity(total);
        self.pick_ids.clear();
        self.resident.clear();
        for (prepared, mesh) in scene.batches.iter().zip(meshes) {
            let instances = &prepared.batch.instances;
            self.resident.push(Draw {
                mesh,
                first: matrices.len() as u32,
                count: instances.matrices.len() as u32,
                pickable: !instances.ids.is_empty(),
            });
            matrices.extend_from_slice(&instances.matrices);
            let ids =
                (0..instances.matrices.len()).map(|i| instances.ids.get(i).copied().flatten());
            self.pick_ids.extend(ids);
        }
        self.instances = (!matrices.is_empty()).then(|| {
            buffer_init(
                gpu,
                "scene instances",
                bytemuck::cast_slice(&matrices),
                wgpu::BufferUsages::STORAGE,
            )
        });
        self.scene_markers = (scene.markers.iter())
            .map(|m| GpuMarker {
                position: [m.position[0], m.position[1], m.position[2], m.radius],
                color: {
                    let [r, g, b] = rgb(m.color);
                    [r, g, b, m.shape as u32 as f32]
                },
            })
            .collect();
        self.markers_for = None;
        self.scene_segments = segments(&scene.lines);
        self.upload_lines(gpu);
        self.presented = Some(scene.clone());
        // The old scene's meshes are free to go.
        assets.presented += 1;
    }

    /// The scene's markers and room for the selection's after them, then
    /// select.wgsl: the dragged rows' transforms and the selection's
    /// markers. Every viewport's draw runs it; it writes the same values
    /// each time.
    fn select(&mut self, gpu: &Gpu, encoder: &mut wgpu::CommandEncoder, placed: &placed::Placed) {
        let sel = &placed.selected;
        let generation = self.presented.as_ref().map_or(0, |p| p.generation);
        let marks = if placed.buffers.is_some() {
            sel.marks.len()
        } else {
            0
        };
        if self.markers_for != Some((generation, marks)) {
            self.markers_for = Some((generation, marks));
            let total = self.scene_markers.len() + marks;
            self.markers = (total > 0).then(|| {
                let mut all = self.scene_markers.clone();
                all.resize(total, GpuMarker::zeroed());
                let usage = wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST;
                let buffer = buffer_init(gpu, "markers", bytemuck::cast_slice(&all), usage);
                (buffer, total as u32)
            });
        }
        let drag = sel.drag.filter(|_| !sel.rows.is_empty());
        let (Some(placed_buffers), Some(buffers), Some((markers, _))) =
            (&placed.buffers, &sel.buffers, &self.markers)
        else {
            return;
        };
        if drag.is_none() && marks == 0 {
            return;
        }
        #[repr(C)]
        #[derive(Clone, Copy, Pod, Zeroable)]
        struct Params {
            delta: Mat4,
            color: [f32; 4],
            rows: u32,
            marks: u32,
            base: u32,
            radius: f32,
        }
        let rows = drag.map_or(0, |_| sel.rows.len() as u32);
        let params = Params {
            delta: drag.unwrap_or(math::IDENTITY),
            color: sel.color,
            rows,
            marks: marks as u32,
            base: self.scene_markers.len() as u32,
            radius: sel.radius,
        };
        let params = buffer_init(
            gpu,
            "select params",
            bytemuck::bytes_of(&params),
            wgpu::BufferUsages::UNIFORM,
        );
        let d = &self.kit.dummy;
        // A storage binding cannot be empty.
        let rows_buffer = if sel.rows.is_empty() {
            &d.buffer
        } else {
            &buffers.rows
        };
        let marks_buffer = if marks == 0 {
            &d.buffer
        } else {
            &buffers.marks
        };
        let group = gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("editor select"),
            layout: &self.select.0,
            entries: &[
                entry(0, params.as_entire_binding()),
                entry(1, rows_buffer.as_entire_binding()),
                entry(2, buffers.starts.as_entire_binding()),
                entry(3, placed_buffers.transforms.as_entire_binding()),
                entry(4, marks_buffer.as_entire_binding()),
                entry(5, markers.as_entire_binding()),
            ],
        });
        let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
            label: Some("editor select"),
            timestamp_writes: None,
        });
        pass.set_bind_group(0, &group, &[]);
        if rows > 0 {
            pass.set_pipeline(&self.select.1[0]);
            pass.dispatch_workgroups(rows.div_ceil(64), 1, 1);
        }
        if marks > 0 {
            pass.set_pipeline(&self.select.1[1]);
            pass.dispatch_workgroups((marks as u32).div_ceil(64), 1, 1);
        }
    }

    /// Bind group 0 for one pass: its camera, the lights, the shadow map to
    /// sample (a dummy while drawing it) and the opaque depth (a dummy
    /// outside the blend pass).
    fn frame_group(
        &self,
        gpu: &Gpu,
        globals: &Globals,
        lights: &Lights,
        shadow: Option<&wgpu::TextureView>,
        depth: Option<&wgpu::TextureView>,
    ) -> wgpu::BindGroup {
        let u = wgpu::BufferUsages::UNIFORM;
        let globals = buffer_init(gpu, "globals", bytemuck::bytes_of(globals), u);
        let lights = buffer_init(gpu, "lights", bytemuck::bytes_of(lights), u);
        let d = &self.kit.dummy;
        let markers = self.markers.as_ref().map_or(&d.buffer, |m| &m.0);
        // The presented terrain's color map, for models tinted by the ground.
        let tint = (self.presented.as_ref())
            .and_then(|s| s.terrain.as_ref())
            .and_then(|t| self.resident.get(t.first))
            .and_then(|draw| draw.mesh.colors.as_ref())
            .map_or(&d.white, |c| &c.view);
        gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("editor frame"),
            layout: &self.kit.frame_layout,
            entries: &[
                entry(0, globals.as_entire_binding()),
                entry(1, lights.as_entire_binding()),
                entry(2, wgpu::BindingResource::Sampler(&self.kit.sampler)),
                entry(
                    3,
                    wgpu::BindingResource::TextureView(shadow.unwrap_or(&d.depth)),
                ),
                entry(4, wgpu::BindingResource::Sampler(&self.kit.compare)),
                entry(
                    5,
                    wgpu::BindingResource::TextureView(depth.unwrap_or(&d.depth)),
                ),
                entry(6, markers.as_entire_binding()),
                entry(
                    7,
                    self.lines
                        .as_ref()
                        .map_or(&d.buffer, |l| &l.0)
                        .as_entire_binding(),
                ),
                entry(8, wgpu::BindingResource::TextureView(tint)),
            ],
        })
    }

    /// Bind group 2: per-draw handles at dynamic offsets into `ids`, the
    /// instance matrices and the visible rows.
    fn draw_group(
        &self,
        gpu: &Gpu,
        ids: &wgpu::Buffer,
        instances: Option<&wgpu::Buffer>,
        visible: Option<&wgpu::Buffer>,
    ) -> wgpu::BindGroup {
        let d = &self.kit.dummy.buffer;
        gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("editor draw"),
            layout: &self.kit.draw_layout,
            entries: &[
                entry(
                    0,
                    wgpu::BindingResource::Buffer(wgpu::BufferBinding {
                        buffer: ids,
                        offset: 0,
                        size: std::num::NonZeroU64::new(std::mem::size_of::<DrawIds>() as u64),
                    }),
                ),
                entry(1, instances.unwrap_or(d).as_entire_binding()),
                entry(2, visible.unwrap_or(d).as_entire_binding()),
            ],
        })
    }

    /// Resets this frame's indirect arguments (every draw at zero
    /// instances), growing the cull outputs for the placements as needed.
    fn prepare_cull(
        &self,
        gpu: &Gpu,
        encoder: &mut wgpu::CommandEncoder,
        placed: &placed::Placed,
        view: &mut Targets,
    ) {
        let draws = placed.draws.len() as u32;
        if view
            .cull
            .as_ref()
            .is_none_or(|t| t.capacity < placed.capacity || t.draws != draws)
        {
            // Room to grow, so scattering a few props keeps the buffers.
            let capacity = placed.capacity.next_power_of_two().max(1024);
            view.cull = Some(placed::CullTargets::new(gpu, capacity, draws));
        }
        let targets = view.cull.as_mut().expect("cull targets");
        encoder.clear_buffer(&targets.totals, 0, None);
        // The template changes with the pieces and placements, which bump
        // the version; every frame the GPU copies it over the arguments.
        if targets.template_version != Some(placed.version) {
            let template = placed.args_template();
            gpu.queue
                .write_buffer(&targets.template, 0, bytemuck::cast_slice(&template));
            targets.template_version = Some(placed.version);
        }
        let bytes = u64::from(draws) * u64::from(placed::ARGS) * 4;
        for args in [&targets.args, &targets.revealed_args, &targets.shadow_args] {
            if bytes > 0 {
                encoder.copy_buffer_to_buffer(&targets.template, 0, args, 0, bytes);
            }
        }
    }

    /// Culls every placement against `planes` and picks its level from
    /// `rows`, into the lists `phase` writes (see `CullPhase`). `clip` maps
    /// world to the render target, for the occlusion test.
    #[allow(clippy::too_many_arguments)]
    fn cull(
        &self,
        gpu: &Gpu,
        encoder: &mut wgpu::CommandEncoder,
        placed: &placed::Placed,
        targets: &mut Targets,
        phase: CullPhase,
        rows: &[[f64; 4]; 4],
        clip: [[f32; 4]; 4],
        height: f32,
        planes: [[f64; 4]; 6],
    ) {
        let (Some(buffers), Some(cull)) = (&placed.buffers, &targets.cull) else {
            return;
        };
        let (args, visible, keep) = match phase {
            CullPhase::Shadow => (&cull.shadow_args, &cull.shadow_visible, &cull.shadow_keep),
            CullPhase::All | CullPhase::Seen => (&cull.args, &cull.visible, &cull.keep),
            CullPhase::Revealed => (&cull.revealed_args, &cull.revealed_visible, &cull.keep),
        };
        let f = |v: [f64; 4]| v.map(|x| x as f32);
        let hzb = &targets.hzb;
        let globals = placed::CullGlobals {
            rows: rows.map(f),
            planes: planes.map(f),
            // z: the view's phases count what they draw for `Drawn`.
            info: [
                height,
                0.,
                if phase == CullPhase::Shadow { 0. } else { 1. },
                0.,
            ],
            counts: [
                placed.count,
                if placed.packs() { placed.row_bits() } else { 0 },
                match phase {
                    CullPhase::Shadow | CullPhase::All => 0,
                    CullPhase::Seen => 1,
                    CullPhase::Revealed => 2,
                },
                hzb.levels(),
            ],
            clip,
            hzb: [
                targets.size[0] as f32,
                targets.size[1] as f32,
                hzb.size[0] as f32,
                hzb.size[1] as f32,
            ],
        };
        let globals = buffer_init(
            gpu,
            "cull",
            bytemuck::bytes_of(&globals),
            wgpu::BufferUsages::UNIFORM,
        );
        let group = gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("editor cull"),
            layout: &self.kit.cull_layout,
            entries: &[
                entry(0, globals.as_entire_binding()),
                entry(1, buffers.transforms.as_entire_binding()),
                entry(2, buffers.piece_of.as_entire_binding()),
                entry(3, buffers.pieces.as_entire_binding()),
                entry(4, buffers.bases.as_entire_binding()),
                entry(5, args.as_entire_binding()),
                entry(6, visible.as_entire_binding()),
                entry(7, wgpu::BindingResource::TextureView(&hzb.view)),
                entry(8, cull.seen.as_entire_binding()),
                entry(9, cull.totals.as_entire_binding()),
                entry(10, keep.as_entire_binding()),
            ],
        });
        let timestamp_writes = targets.timing.as_mut().and_then(|t| t.compute("cull"));
        let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
            label: Some(match phase {
                CullPhase::Shadow => "editor shadow cull",
                CullPhase::Revealed => "editor occlusion cull",
                _ => "editor cull",
            }),
            timestamp_writes,
        });
        pass.set_pipeline(&self.cull);
        pass.set_bind_group(0, &group, &[]);
        pass.dispatch_workgroups(placed.count.div_ceil(64), 1, 1);
    }

    /// Builds the view's depth pyramid from its depth target, level by level.
    fn build_hzb(&self, encoder: &mut wgpu::CommandEncoder, targets: &mut Targets) {
        let timestamp_writes = targets
            .timing
            .as_mut()
            .and_then(|t| t.compute("depth pyramid"));
        let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
            label: Some("editor depth pyramid"),
            timestamp_writes,
        });
        for (l, (group, size)) in targets.hzb.steps.iter().enumerate() {
            pass.set_pipeline(&self.hzb[usize::from(l > 0)]);
            pass.set_bind_group(0, group, &[]);
            pass.dispatch_workgroups(size[0].div_ceil(8), size[1].div_ceil(8), 1);
        }
    }

    /// Records the placed draws using pipeline `index` (0 lit, 1 flat; all
    /// casters for the shadow map). `slot` hands out per-draw handle slots.
    #[allow(clippy::too_many_arguments)]
    fn draw_placed(
        &self,
        pass: &mut wgpu::RenderPass<'_>,
        placed: &placed::Placed,
        cull: &placed::CullTargets,
        group: &wgpu::BindGroup,
        shadow: bool,
        index: Option<usize>,
        slot: &mut impl FnMut([u32; 4]) -> u32,
    ) {
        let args = if shadow {
            &cull.shadow_args
        } else {
            &cull.args
        };
        // Pieces without geometry wait for the arena.
        if placed.lightweight {
            return;
        }
        for (d, draw) in placed.draws.iter().enumerate() {
            // Impostors draw only from the arena.
            if draw.impostor {
                continue;
            }
            let Some(levels) = &placed.resident[draw.piece as usize] else {
                continue;
            };
            let mesh = &levels[draw.level as usize];
            let data = mesh.source.data();
            if shadow && !casts_shadow(data) {
                continue;
            }
            let pipeline = match data.material {
                Material::Lit => Some(0),
                Material::Flat => Some(1),
                _ => None,
            };
            if !shadow && pipeline != index {
                continue;
            }
            pass.set_bind_group(1, &mesh.group, &[]);
            pass.set_bind_group(2, group, &[slot([1, 1, draw.base, placed.row_mask()])]);
            pass.set_index_buffer(mesh.indices.slice(..), wgpu::IndexFormat::Uint32);
            pass.draw_indexed_indirect(args, d as u64 * u64::from(placed::ARGS) * 4);
        }
    }

    /// Bind group 2 of placed.wgsl: placement transforms and the visible
    /// entries of the view or the shadow map.
    fn arena_draws(
        &self,
        gpu: &Gpu,
        transforms: &wgpu::Buffer,
        visible: &wgpu::Buffer,
        keep: &wgpu::Buffer,
    ) -> wgpu::BindGroup {
        gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("editor placed"),
            layout: &self.kit.placed_layout,
            entries: &[
                entry(0, transforms.as_entire_binding()),
                entry(1, visible.as_entire_binding()),
                entry(2, keep.as_entire_binding()),
            ],
        })
    }

    /// Bakes every impostor's views into a new atlas, once the arena holds
    /// their meshes: one pass per impostor, its mesh drawn once per view.
    fn bake_impostors(
        &self,
        gpu: &Gpu,
        encoder: &mut wgpu::CommandEncoder,
        placed: &mut placed::Placed,
    ) {
        let (Some(pipelines), Some(arena), None) = (&self.arena, &placed.arena, &placed.atlas)
        else {
            return;
        };
        // Views bake the textures as they are: wait for every texel.
        if !placed.unstreamed.is_empty() {
            return;
        }
        if arena.impostors.is_empty() {
            return;
        }
        let side = placed::IMPOSTOR_VIEWS * placed::IMPOSTOR_TILE;
        let layers = arena.impostors.len() as u32;
        let views = placed::IMPOSTOR_VIEWS * placed::IMPOSTOR_VIEWS;
        let layered = |label, format| {
            gpu.device.create_texture(&wgpu::TextureDescriptor {
                label: Some(label),
                size: wgpu::Extent3d {
                    width: side,
                    height: side,
                    depth_or_array_layers: layers,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format,
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT
                    | wgpu::TextureUsages::TEXTURE_BINDING,
                view_formats: &[],
            })
        };
        let albedo = layered("impostor albedo", wgpu::TextureFormat::Rgba8Unorm);
        let normal = layered("impostor normals", wgpu::TextureFormat::Rg8Unorm);
        let (_, depth) = texture_2d(
            gpu,
            "impostor depth",
            [side; 2],
            wgpu::TextureFormat::Depth32Float,
            wgpu::TextureUsages::RENDER_ATTACHMENT,
        );
        // Only the albedo sampler is read.
        let frame = self.frame_group(gpu, &Globals::zeroed(), &Lights::zeroed(), None, None);
        let layer_view = |texture: &wgpu::Texture, layer| {
            texture.create_view(&wgpu::TextureViewDescriptor {
                dimension: Some(wgpu::TextureViewDimension::D2),
                base_array_layer: layer,
                array_layer_count: Some(1),
                ..Default::default()
            })
        };
        let clear = wgpu::Operations {
            load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
            store: wgpu::StoreOp::Store,
        };
        for (layer, (draw, range)) in arena.impostors.iter().enumerate() {
            let (a, n) = (
                layer_view(&albedo, layer as u32),
                layer_view(&normal, layer as u32),
            );
            let target = |view| {
                Some(wgpu::RenderPassColorAttachment {
                    view,
                    resolve_target: None,
                    ops: clear,
                    depth_slice: None,
                })
            };
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("editor impostor bake"),
                color_attachments: &[target(&a), target(&n)],
                depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                    view: &depth,
                    depth_ops: Some(wgpu::Operations {
                        load: wgpu::LoadOp::Clear(1.),
                        store: wgpu::StoreOp::Discard,
                    }),
                    stencil_ops: None,
                }),
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            pass.set_pipeline(&pipelines[5]);
            pass.set_bind_group(0, &frame, &[]);
            pass.set_bind_group(3, &arena.group, &[]);
            pass.draw(range.clone(), draw * views..(draw + 1) * views);
        }
        let array = |texture: &wgpu::Texture| {
            texture.create_view(&wgpu::TextureViewDescriptor {
                dimension: Some(wgpu::TextureViewDimension::D2Array),
                ..Default::default()
            })
        };
        let group = gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("editor impostor atlas"),
            layout: &self.kit.atlas_layout,
            entries: &[
                entry(0, wgpu::BindingResource::TextureView(&array(&albedo))),
                entry(1, wgpu::BindingResource::TextureView(&array(&normal))),
            ],
        });
        placed.set_atlas(placed::Atlas { group });
    }

    /// The opaque pass's placements from the arena: lit and flat meshes,
    /// then impostors once baked. `draws` and `args` pick the cull phase.
    fn draw_arena_opaque(
        &self,
        pass: &mut wgpu::RenderPass<'_>,
        placed: &placed::Placed,
        draws: &wgpu::BindGroup,
        args: &wgpu::Buffer,
    ) {
        let (Some([lit, flat, _, impostors, _, _]), Some(arena)) = (&self.arena, &placed.arena)
        else {
            return;
        };
        let ranges = &placed.ranges;
        Self::draw_arena(
            pass,
            lit,
            arena,
            draws,
            args,
            std::slice::from_ref(&ranges.lit),
        );
        Self::draw_arena(
            pass,
            flat,
            arena,
            draws,
            args,
            std::slice::from_ref(&ranges.flat),
        );
        if let Some(atlas) = &placed.atlas {
            pass.set_bind_group(1, &atlas.group, &[]);
            let impostor = std::slice::from_ref(&ranges.impostors);
            Self::draw_arena(pass, impostors, arena, draws, args, impostor);
        }
    }

    /// Every placed draw in `ranges` with `pipeline`, one multi-draw each:
    /// empty draws cost the GPU next to nothing, and the CPU encodes a
    /// handful of commands instead of one per (piece, level).
    fn draw_arena(
        pass: &mut wgpu::RenderPass<'_>,
        pipeline: &wgpu::RenderPipeline,
        arena: &placed::Arena,
        draws: &wgpu::BindGroup,
        args: &wgpu::Buffer,
        ranges: &[std::ops::Range<u32>],
    ) {
        if ranges.iter().all(|r| r.is_empty()) {
            return;
        }
        pass.set_pipeline(pipeline);
        pass.set_bind_group(2, draws, &[]);
        pass.set_bind_group(3, &arena.group, &[]);
        pass.set_index_buffer(arena.indices.slice(..), wgpu::IndexFormat::Uint32);
        let stride = u64::from(placed::ARGS) * 4;
        for r in ranges.iter().filter(|r| !r.is_empty()) {
            pass.multi_draw_indexed_indirect(args, u64::from(r.start) * stride, r.len() as u32);
        }
    }

    /// The scene's light, and the shadow map drawn again if what casts or
    /// receives shadows changed since it was last drawn.
    #[allow(clippy::too_many_arguments)]
    fn lights(
        &mut self,
        gpu: &Gpu,
        encoder: &mut wgpu::CommandEncoder,
        scene: &PreparedScene,
        globals: Globals,
        placed: &placed::Placed,
        height_edits: u64,
        targets: &mut Targets,
        ids: &wgpu::Buffer,
        slot: &mut impl FnMut([u32; 4]) -> u32,
    ) -> Lights {
        let light = scene.light;
        let sun = math::normalize(light.sun).unwrap_or([0., 1., 0.]);
        let [r, g, b] = light.sun_color;
        let mut lights = Lights {
            shadow: bytemuck::cast(math::IDENTITY),
            sun: [sun[0], sun[1], sun[2], 0.],
            sun_color: [r, g, b, 0.],
            sky: [light.sky[0], light.sky[1], light.sky[2], 1.],
            ground: [light.ground[0], light.ground[1], light.ground[2], 0.],
            fog: [0.; 4],
            fog_range: [0.; 4],
            tone: scene
                .tone
                .map_or([0.; 4], |t| [t.exposure.max(1e-6), t.sun_occlusion, 0., 0.]),
            zenith: [0.; 4],
            horizon: [0.; 4],
            sea: [0.; 4],
            tint: [0.; 4],
            occlusion: scene
                .occlusion
                .map_or([0.; 4], |o| [o.radius.max(1e-3), o.strength, 0., 0.]),
        };
        if let Some(t) = scene.terrain.as_ref().filter(|t| t.source.tints) {
            let grid = t.source.heights.grid();
            let o = grid.origin;
            lights.tint = [o[0], o[1], grid.spacing, t.source.color_gain];
        }
        if let Some(sky) = scene.sky {
            let ([r, g, b], [hr, hg, hb]) = (sky.zenith, sky.horizon);
            lights.zenith = [r, g, b, 1.];
            lights.horizon = [hr, hg, hb, sky.sea.map_or(0., |s| s.level)];
            if let Some(sea) = sky.sea {
                lights.sea = [sea.color[0], sea.color[1], sea.color[2], 1.];
            }
        }
        if let Some(fog) = light.fog {
            lights.fog = [fog.color[0], fog.color[1], fog.color[2], 1.];
            lights.fog_range[..2].copy_from_slice(&[fog.start, fog.end]);
        }
        let Some(shadows) = light.shadows else {
            return lights;
        };
        let resolution = shadows.resolution.clamp(
            *light::SHADOW_RESOLUTION.start(),
            (*light::SHADOW_RESOLUTION.end()).min(gpu.device.limits().max_texture_dimension_2d),
        );
        let with_placed = |b: [Vec3; 2]| {
            if placed.count == 0 {
                b
            } else {
                math::bounds(b.into_iter().chain(placed.bounds))
            }
        };
        let fit = light::fit(
            sun,
            with_placed(scene.bounds(Prepared::casts_shadow)),
            // Opaque receivers only: a sea plane reaching far past the land
            // would stretch every texel (3x for an editor's sea around an
            // island), and small casters' shadows would wash out. Water
            // outside the map is lit.
            with_placed(scene.bounds(|p| !matches!(p.material(), Material::Blend(_)))),
            resolution,
        );
        let Some(fit) = fit else {
            return lights;
        };
        let matrix = light::matrix(fit.rows);
        lights.shadow = matrix;
        lights.sun[3] = 1.;
        lights.sun_color[3] = shadows.softness.max(0.);
        lights.sky[3] = resolution as f32;
        lights.fog_range[2] = fit.texel;
        // Half a texel of depth, as a fraction of the map's depth range.
        lights.fog_range[3] = fit.texel * 0.5 / fit.depth;

        if self
            .shadow_map
            .as_ref()
            .is_none_or(|m| m.resolution != resolution)
        {
            // Memory: resolution² × 4 bytes, shared by the viewports.
            let (_, view) = texture_2d(
                gpu,
                "editor shadow map",
                [resolution; 2],
                wgpu::TextureFormat::Depth32Float,
                wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
            );
            self.shadow_map = Some(ShadowMap {
                view,
                resolution,
                drawn: None,
            });
        }
        let version = (scene.generation, height_edits, placed.version);
        if self
            .shadow_map
            .as_ref()
            .is_some_and(|m| m.drawn == Some(version))
        {
            return lights;
        }
        let frustum = Frustum::from_rows(&fit.rows);
        // Casters take the level a shadow texel can show, from the map's
        // own projection: one texel is a pixel.
        self.cull(
            gpu,
            encoder,
            placed,
            targets,
            CullPhase::Shadow,
            &fit.rows,
            matrix,
            resolution as f32,
            frustum.planes(),
        );
        let frame = self.frame_group(
            gpu,
            &Globals {
                clip: matrix,
                ..globals
            },
            &lights,
            None,
            None,
        );
        let batches = self.draw_group(gpu, ids, self.instances.as_ref(), None);
        let placed_group = (placed.buffers.as_ref())
            .zip(targets.cull.as_ref())
            .map(|(b, c)| self.draw_group(gpu, ids, Some(&b.transforms), Some(&c.shadow_visible)));
        let map = self.shadow_map.as_ref().expect("created above");
        let timestamp_writes = targets.timing.as_mut().and_then(|t| t.render("shadow map"));
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("editor shadow map"),
            color_attachments: &[],
            depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                view: &map.view,
                depth_ops: Some(wgpu::Operations {
                    load: wgpu::LoadOp::Clear(1.),
                    store: wgpu::StoreOp::Store,
                }),
                stencil_ops: None,
            }),
            timestamp_writes,
            occlusion_query_set: None,
            multiview_mask: None,
        });
        pass.set_pipeline(&self.shadow);
        pass.set_bind_group(0, &frame, &[]);
        for (prepared, draw) in scene.batches.iter().zip(&self.resident) {
            if prepared.terrain || !prepared.casts_shadow() || frustum.excludes(prepared.bounds()) {
                continue;
            }
            pass.set_bind_group(1, &draw.mesh.group, &[]);
            pass.set_bind_group(2, &batches, &[slot([0; 4])]);
            draw.mesh
                .draw(&mut pass, draw.first..draw.first + draw.count);
        }
        // Every terrain tile at one coarse stride; redrawn only on change.
        if let Some(t) = &scene.terrain
            && casts_shadow(scene.batches[t.first].batch.mesh.data())
        {
            let tiles: Vec<Mat4> = t
                .origins
                .iter()
                .map(|o| math::translate([o[0], 0., o[1]]))
                .collect();
            let buffer = buffer_init(
                gpu,
                "terrain shadow tiles",
                bytemuck::cast_slice(&tiles),
                wgpu::BufferUsages::STORAGE,
            );
            let group = self.draw_group(gpu, ids, Some(&buffer), None);
            let draw = &self.resident[t.first + TERRAIN_SHADOW_LEVEL];
            pass.set_bind_group(1, &draw.mesh.group, &[]);
            pass.set_bind_group(2, &group, &[slot([0; 4])]);
            draw.mesh.draw(&mut pass, 0..tiles.len() as u32);
        }
        match (&self.arena, &placed.arena, &placed.buffers, &targets.cull) {
            (Some([_, _, shadow, _, impostors, _]), Some(arena), Some(buffers), Some(cull)) => {
                let draws = self.arena_draws(
                    gpu,
                    &buffers.transforms,
                    &cull.shadow_visible,
                    &cull.shadow_keep,
                );
                let ranges = [
                    placed.ranges.lit_casters.clone(),
                    placed.ranges.flat_casters.clone(),
                ];
                let args = &cull.shadow_args;
                Self::draw_arena(&mut pass, shadow, arena, &draws, args, &ranges);
                if let Some(atlas) = &placed.atlas {
                    pass.set_bind_group(1, &atlas.group, &[]);
                    let ranges = std::slice::from_ref(&placed.ranges.impostors);
                    Self::draw_arena(&mut pass, impostors, arena, &draws, args, ranges);
                }
            }
            _ => {
                if let (Some(group), Some(cull)) = (&placed_group, &targets.cull) {
                    self.draw_placed(&mut pass, placed, cull, group, true, None, slot);
                }
            }
        }
        drop(pass);
        self.shadow_map.as_mut().expect("created above").drawn = Some(version);
        lights
    }
}

/// Maps readbacks whose frame has been submitted, and applies those that
/// have landed. `wait` blocks for them (offscreen, after its submit).
fn take_readbacks(gpu: &Gpu, slot: &mut Slot, stats: &mut Stats, wait: bool) {
    use std::sync::atomic::Ordering;
    let Some(targets) = slot.targets.as_mut() else {
        return;
    };
    for r in &targets.readbacks {
        if r.state.load(Ordering::Acquire) == 0 {
            r.state.store(1, Ordering::Release);
            let state = r.state.clone();
            r.buffer
                .slice(..)
                .map_async(wgpu::MapMode::Read, move |result| {
                    state.store(if result.is_ok() { 2 } else { 3 }, Ordering::Release);
                });
        }
    }
    // A window's mappings land as GPUI submits its frames, which maintains
    // the device; polling here as well cost 0.2 ms a frame. Offscreen
    // frames wait for theirs.
    if wait {
        let _ = gpu.device.poll(wgpu::PollType::wait_indefinitely());
    }
    let period = gpu.queue.get_timestamp_period();
    let mut keep = Vec::with_capacity(targets.readbacks.len());
    for r in targets.readbacks.drain(..) {
        match r.state.load(Ordering::Acquire) {
            2 => {}
            3 => continue,
            _ => {
                keep.push(r);
                continue;
            }
        }
        {
            let Ok(data) = r.buffer.slice(..).get_mapped_range() else {
                continue;
            };
            let words: &[u32] = bytemuck::cast_slice(&data[..data.len() / 4 * 4]);
            match r.kind {
                ReadKind::Pick { cursor, view } => {
                    slot.picked = Some(Picked {
                        cursor,
                        view,
                        handle: words[0],
                        depth: f32::from_bits(words[1]),
                    });
                }
                ReadKind::Counts => {
                    let drawn = &mut slot.drawn;
                    drawn.placed = std::array::from_fn(|l| words[l] as usize);
                    drawn.placed_triangles = words[MAX_LEVELS] as usize;
                }
                ReadKind::Times(labels) => {
                    let stamps: &[u64] = bytemuck::cast_slice(&data[..]);
                    let mut total = 0.;
                    let mut passes = [("", 0.); TIMED_PASSES as usize];
                    for ((p, label), out) in stamps.chunks(2).zip(labels).zip(&mut passes) {
                        if p[0] != 0 && p[1] >= p[0] {
                            let ms = (p[1] - p[0]) as f32 * period / 1e6;
                            *out = (label, ms);
                            total += ms;
                        }
                    }
                    stats.gpu_ms = Stats::blend(stats.gpu_ms, total);
                    stats.passes = passes;
                    let valid = || stamps.chunks(2).filter(|p| p[0] != 0 && p[1] >= p[0]);
                    if let (Some(first), Some(last)) =
                        (valid().map(|p| p[0]).min(), valid().map(|p| p[1]).max())
                    {
                        stats.gpu_span_ms = (last - first) as f32 * period / 1e6;
                    }
                }
            }
        }
        r.buffer.unmap();
    }
    targets.readbacks = keep;
}

fn scissor(clip: Bounds<ScaledPixels>, size: [u32; 2]) -> [u32; 4] {
    let x = ((f64::from(clip.origin.x) as f32).max(0.) as u32).min(size[0]);
    let y = ((f64::from(clip.origin.y) as f32).max(0.) as u32).min(size[1]);
    let w = ((f64::from(clip.size.width) as f32).max(0.) as u32).min(size[0] - x);
    let h = ((f64::from(clip.size.height) as f32).max(0.) as u32).min(size[1] - y);
    [x, y, w, h]
}

/// Per-draw handle slots for one frame, at dynamic uniform offsets into one
/// buffer the frame writes once it knows them all.
struct DrawSlots {
    bytes: Vec<u8>,
    align: usize,
    capacity: usize,
}

impl DrawSlots {
    fn push(&mut self, ids: [u32; 4]) -> u32 {
        let offset = self.bytes.len();
        // Fails loud: `Renderer::draw` sizes the buffer from the draw count.
        assert!(offset < self.capacity * self.align, "draw slots overflow");
        self.bytes
            .extend_from_slice(bytemuck::bytes_of(&DrawIds { ids }));
        self.bytes.resize(offset + self.align, 0);
        offset as u32
    }
}

impl Renderer {
    /// Maps and reads `slot`'s readbacks now: offscreen frames wait for
    /// their submission, so the answers are there at once.
    pub(crate) fn flush_readbacks(&self, gpu: &Gpu, slot: usize) {
        let mut state = self.state.lock().unwrap();
        let State { slots, stats, .. } = &mut *state;
        if let Some(s) = slots.get_mut(slot) {
            take_readbacks(gpu, s, stats, true);
        }
    }

    /// Records viewport `slot` into GPUI's frame encoder, drawing into
    /// `target` at `bounds` within `clip`.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn draw(
        &self,
        slot: usize,
        gpu: &Gpu,
        encoder: &mut wgpu::CommandEncoder,
        target: &wgpu::TextureView,
        format: wgpu::TextureFormat,
        size: [u32; 2],
        bounds: Bounds<ScaledPixels>,
        clip: Bounds<ScaledPixels>,
    ) {
        let _span = crate::profile::span("gpu draw (cpu encode)");
        let mut state = self.state.lock().unwrap();
        let now = std::time::Instant::now();
        // A frame starts at the first draw after the host marked a new one;
        // without marks (offscreen) every draw of slot 0 starts one.
        let bit = 1u64 << slot.min(63);
        let fresh = state.frame_mark.is_some() && state.drawn_mark != state.frame_mark;
        let new_frame = match state.frame_mark {
            Some(_) => fresh || state.drawn_slots & bit != 0,
            None => slot == 0,
        };
        if new_frame {
            state.drawn_mark = state.frame_mark;
            state.drawn_slots = 0;
            // Build and layout only count when the host built this frame.
            state.frame_built = fresh;
            state.encode_start = Some(now);
            if let Some(last) = state.frame_began.replace(now) {
                let ms = (now - last).as_secs_f32() * 1000.;
                state.stats.frame_ms = Stats::blend(state.stats.frame_ms, ms);
            }
            state.assets.collect();
        }
        let State {
            scene,
            assets,
            device,
            slots,
            stats,
            overlay,
            outline,
            outline_version,
            ..
        } = &mut *state;
        let outline_version = *outline_version;
        if let Some(s) = slots.get_mut(slot) {
            let _span = crate::profile::span("gpu readbacks");
            take_readbacks(gpu, s, stats, false);
        }
        // The frame's encoding; returns early when there is nothing to draw.
        (|| {
            let Some(slot) = slots.get_mut(slot) else {
                return;
            };
            let Some(view) = slot.view else {
                return;
            };
            if device.as_ref().is_some_and(|d| d.format != format) {
                // Meshes hold bind groups made against the old device's kit.
                *device = None;
                assets.meshes.clear();
                assets.placed.release();
            }
            let device = device.get_or_insert_with(|| Device::new(gpu, format));
            if slot.targets.as_ref().is_some_and(|t| t.size != size) {
                slot.targets = None;
            }
            let targets = slot
                .targets
                .get_or_insert_with(|| Targets::new(gpu, &device.kit, size));
            if let Some(scene) = scene {
                let _span = crate::profile::span("gpu scene update");
                device.update(gpu, scene, assets);
            }
            if let Some(lines) = overlay.take() {
                device.overlay_segments = segments(&lines);
                device.upload_lines(gpu);
            }
            {
                let _span = crate::profile::span("gpu sync");
                assets.sync_heights(gpu, &device.kit, encoder);
                assets.sync_placed(gpu, &device.kit, &mut Budget::new());
                device.select(gpu, encoder, &assets.placed);
                let _span = crate::profile::span("gpu sync: impostor bake");
                device.bake_impostors(gpu, encoder, &mut assets.placed);
            }
            if !assets.placed.pending()
                && RELEASED.swap(false, std::sync::atomic::Ordering::Relaxed)
            {
                trim_heap();
            }
            let height_edits = assets.height_edits;
            let placed = &assets.placed;
            let Some(scene) = device.presented.clone() else {
                return;
            };
            let camera = view.camera;
            let factor = (f64::from(bounds.size.width) as f32) / f32::from(view.bounds.size.width);
            let rows = camera.view_rows(view.bounds, view.extent, true);
            let anchor = camera.anchor(view.extent);
            let to_target = crate::camera::to_target(rows, bounds, size);
            let unclip =
                math::inverse(&bytemuck::cast(to_target)).map_or(to_target, bytemuck::cast);
            let globals = Globals {
                clip: to_target,
                unclip,
                // w: row bits of a packed visible entry (placed.wgsl).
                viewport: [
                    size[0] as f32,
                    size[1] as f32,
                    factor,
                    if placed.packs() {
                        placed.row_bits() as f32
                    } else {
                        0.
                    },
                ],
                eye: match camera.eye(view.bounds, view.extent) {
                    Some(eye) => [eye[0], eye[1], eye[2], 1.],
                    None => {
                        let t = camera.toward();
                        [t[0], t[1], t[2], 0.]
                    }
                },
                anchor: [
                    anchor[0],
                    anchor[1],
                    anchor[2],
                    if camera.is_perspective() {
                        // Reversed perspective depth is `near / w`: the z row is
                        // the near distance alone.
                        rows[2][3] as f32
                    } else {
                        camera.depth_span(view.extent)
                    },
                ],
            };
            // Shadow, opaque and blend passes each draw the scene once and
            // the placements once per pipeline at most.
            let capacity = 3 * (device.resident.len() + 3 * placed.draws.len()) + 1;
            let align = device.kit.align as usize;
            if targets.draw_ids.as_ref().is_none_or(|(_, c)| *c < capacity) {
                let capacity = capacity.next_power_of_two();
                let buffer = gpu.device.create_buffer(&wgpu::BufferDescriptor {
                    label: Some("editor draw ids"),
                    size: (capacity * align) as u64,
                    usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                    mapped_at_creation: false,
                });
                targets.draw_ids = Some((buffer, capacity));
            }
            let (ids, capacity) = targets.draw_ids.clone().expect("created above");
            let mut slots_ = DrawSlots {
                bytes: Vec::with_capacity(capacity * align),
                align,
                capacity,
            };
            let mut push = |v: [u32; 4]| slots_.push(v);

            let frustum = Frustum::from_rows(&rows);
            let height = f32::from(view.bounds.size.height);
            let placing = placed.count > 0 && placed.buffers.is_some();
            // Occlusion culls in two phases around the opaque pass; it needs
            // the arena, which draws each phase's lists in a few calls.
            let occluding = placing && device.arena.is_some() && placed.arena.is_some();
            if placing {
                device.prepare_cull(gpu, encoder, placed, targets);
                let phase = if occluding {
                    CullPhase::Seen
                } else {
                    CullPhase::All
                };
                device.cull(
                    gpu,
                    encoder,
                    placed,
                    targets,
                    phase,
                    &rows,
                    globals.clip,
                    height,
                    frustum.planes(),
                );
            }
            let lights = {
                let _span = crate::profile::span("gpu lights + shadow pass");
                device.lights(
                    gpu,
                    encoder,
                    &scene,
                    globals,
                    placed,
                    height_edits,
                    targets,
                    &ids,
                    &mut push,
                )
            };
            let shadow = (lights.sun[3] > 0.)
                .then_some(device.shadow_map.as_ref())
                .flatten()
                .map(|m| &m.view);
            let visible = scene
                .batches
                .iter()
                .zip(&device.resident)
                .filter(|(p, _)| !p.terrain && !frustum.excludes(p.bounds()))
                .collect::<Vec<_>>();
            let terrain = scene.terrain.as_ref().map(|t| {
                let (matrices, ranges) = t.place(&frustum, &rows, height);
                (t, matrices, ranges)
            });
            let mut blended = visible
                .iter()
                .filter(|(p, _)| matches!(p.material(), Material::Blend(_)))
                .map(|&(p, draw)| {
                    let bounds = p.bounds();
                    let center = std::array::from_fn(|a| (bounds[0][a] + bounds[1][a]) * 0.5);
                    (camera.depth(center, view.extent), draw)
                })
                .collect::<Vec<_>>();
            // Farthest first.
            blended.sort_by(|a, b| a.0.total_cmp(&b.0));
            {
                let drawn = &mut slot.drawn;
                drawn.batches = visible.len();
                drawn.instances = visible.iter().map(|(_, d)| d.count as usize).sum();
                drawn.triangles = (visible.iter())
                    .map(|(_, d)| d.mesh.count as usize / 3 * d.count as usize)
                    .sum();
                if let Some((t, _, ranges)) = &terrain {
                    drawn.triangles += (ranges.iter().zip(&t.triangles))
                        .map(|(r, &n)| r.len() * n)
                        .sum::<usize>();
                }
            }
            let markers = device.markers.as_ref().map_or(0, |m| m.1);
            let [sx, sy, sw, sh] = scissor(clip, size);
            if sw == 0 || sh == 0 {
                return;
            }
            let opaque_frame = device.frame_group(gpu, &globals, &lights, shadow, None);
            let batches = device.draw_group(gpu, &ids, device.instances.as_ref(), None);
            // This view's terrain tiles, written where the frame reads them.
            let terrain_group =
                terrain
                    .as_ref()
                    .filter(|(_, m, _)| !m.is_empty())
                    .map(|(_, matrices, _)| {
                        let bytes: &[u8] = bytemuck::cast_slice(matrices);
                        if targets
                            .terrain
                            .as_ref()
                            .is_none_or(|b| b.size() < bytes.len() as u64)
                        {
                            targets.terrain =
                                Some(gpu.device.create_buffer(&wgpu::BufferDescriptor {
                                    label: Some("editor terrain tiles"),
                                    size: (bytes.len() as u64).next_power_of_two(),
                                    usage: wgpu::BufferUsages::STORAGE
                                        | wgpu::BufferUsages::COPY_DST,
                                    mapped_at_creation: false,
                                }));
                        }
                        let buffer = targets.terrain.as_ref().expect("created above");
                        gpu.queue.write_buffer(buffer, 0, bytes);
                        device.draw_group(gpu, &ids, Some(buffer), None)
                    });
            let placed_group = (placed.buffers.as_ref())
                .zip(targets.cull.as_ref())
                .filter(|_| placing)
                .map(|(b, c)| device.draw_group(gpu, &ids, Some(&b.transforms), Some(&c.visible)));
            {
                let timestamp_writes = targets.timing.as_mut().and_then(|t| t.render("opaque"));
                let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("editor viewport"),
                    color_attachments: &[
                        Some(wgpu::RenderPassColorAttachment {
                            view: target,
                            resolve_target: None,
                            ops: wgpu::Operations {
                                load: wgpu::LoadOp::Load,
                                store: wgpu::StoreOp::Store,
                            },
                            depth_slice: None,
                        }),
                        Some(wgpu::RenderPassColorAttachment {
                            view: &targets.ids.1,
                            resolve_target: None,
                            ops: wgpu::Operations {
                                load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                                store: wgpu::StoreOp::Store,
                            },
                            depth_slice: None,
                        }),
                    ],
                    depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                        view: &targets.depth.1,
                        depth_ops: Some(wgpu::Operations {
                            // Reversed depth: 0 is the far plane.
                            load: wgpu::LoadOp::Clear(0.),
                            store: wgpu::StoreOp::Store,
                        }),
                        stencil_ops: None,
                    }),
                    timestamp_writes,
                    occlusion_query_set: None,
                    multiview_mask: None,
                });
                pass.set_scissor_rect(sx, sy, sw, sh);
                pass.set_bind_group(0, &opaque_frame, &[]);
                let opaque = |m: &Material| match m {
                    Material::Lit => Some(0),
                    Material::Flat => Some(1),
                    Material::Splat(_) => Some(2),
                    Material::Blend(_) => None,
                };
                for (index, pipeline) in [&device.lit, &device.flat, &device.splat]
                    .into_iter()
                    .enumerate()
                {
                    pass.set_pipeline(pipeline);
                    for (_, draw) in visible
                        .iter()
                        .filter(|(p, _)| opaque(p.material()) == Some(index))
                    {
                        pass.set_bind_group(1, &draw.mesh.group, &[]);
                        let offset = push([u32::from(draw.pickable), 0, 0, 0]);
                        pass.set_bind_group(2, &batches, &[offset]);
                        draw.mesh
                            .draw(&mut pass, draw.first..draw.first + draw.count);
                    }
                    if let (Some(group), Some(cull)) = (&placed_group, &targets.cull)
                        && placed.arena.is_none()
                    {
                        device.draw_placed(
                            &mut pass,
                            placed,
                            cull,
                            group,
                            false,
                            Some(index),
                            &mut push,
                        );
                    }
                }
                if let (Some(buffers), Some(cull), true) = (&placed.buffers, &targets.cull, placing)
                {
                    let draws =
                        device.arena_draws(gpu, &buffers.transforms, &cull.visible, &cull.keep);
                    device.draw_arena_opaque(&mut pass, placed, &draws, &cull.args);
                }
                // Terrain last: its splat shading is the costliest per pixel,
                // and drawn after props and rocks the depth test skips what
                // they cover.
                if let (Some((t, _, ranges)), Some(group)) = (&terrain, &terrain_group)
                    && let Some(index) = opaque(&t.source.material)
                {
                    pass.set_pipeline([&device.lit, &device.flat, &device.splat][index]);
                    for (k, r) in ranges.iter().enumerate().filter(|(_, r)| !r.is_empty()) {
                        let draw = &device.resident[t.first + k];
                        pass.set_bind_group(1, &draw.mesh.group, &[]);
                        let offset = push([0; 4]);
                        pass.set_bind_group(2, group, &[offset]);
                        draw.mesh.draw(&mut pass, r.clone());
                    }
                }
            }
            // Second phase: what the first phase's depth shows of the rest.
            if occluding {
                device.build_hzb(encoder, targets);
                device.cull(
                    gpu,
                    encoder,
                    placed,
                    targets,
                    CullPhase::Revealed,
                    &rows,
                    globals.clip,
                    height,
                    frustum.planes(),
                );
                if let (Some(buffers), Some(cull)) = (&placed.buffers, &targets.cull) {
                    let draws = device.arena_draws(
                        gpu,
                        &buffers.transforms,
                        &cull.revealed_visible,
                        &cull.keep,
                    );
                    let load = |view| {
                        Some(wgpu::RenderPassColorAttachment {
                            view,
                            resolve_target: None,
                            ops: wgpu::Operations {
                                load: wgpu::LoadOp::Load,
                                store: wgpu::StoreOp::Store,
                            },
                            depth_slice: None,
                        })
                    };
                    let timestamp_writes =
                        targets.timing.as_mut().and_then(|t| t.render("revealed"));
                    let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                        label: Some("editor viewport, revealed"),
                        color_attachments: &[load(target), load(&targets.ids.1)],
                        depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                            view: &targets.depth.1,
                            depth_ops: Some(wgpu::Operations {
                                load: wgpu::LoadOp::Load,
                                store: wgpu::StoreOp::Store,
                            }),
                            stencil_ops: None,
                        }),
                        timestamp_writes,
                        occlusion_query_set: None,
                        multiview_mask: None,
                    });
                    pass.set_scissor_rect(sx, sy, sw, sh);
                    pass.set_bind_group(0, &opaque_frame, &[]);
                    device.draw_arena_opaque(&mut pass, placed, &draws, &cull.revealed_args);
                }
            }
            // The pick handle under the asked-for cursor, and this frame's
            // placement counts, come back a frame or two later.
            if let Some(cursor) = slot.pick_request {
                let x = f64::from(bounds.origin.x) as f32
                    + f32::from(cursor.x - view.bounds.origin.x) * factor;
                let y = f64::from(bounds.origin.y) as f32
                    + f32::from(cursor.y - view.bounds.origin.y) * factor;
                if x >= 0. && y >= 0. && (x as u32) < size[0] && (y as u32) < size[1] {
                    let buffer = targets
                        .readback(gpu, 8, ReadKind::Pick { cursor, view })
                        .clone();
                    let texel = |texture, aspect| wgpu::TexelCopyTextureInfo {
                        texture,
                        mip_level: 0,
                        origin: wgpu::Origin3d {
                            x: x as u32,
                            y: y as u32,
                            z: 0,
                        },
                        aspect,
                    };
                    let one = wgpu::Extent3d {
                        width: 1,
                        height: 1,
                        depth_or_array_layers: 1,
                    };
                    encoder.copy_texture_to_buffer(
                        texel(&targets.ids.0, wgpu::TextureAspect::All),
                        wgpu::TexelCopyBufferInfo {
                            buffer: &buffer,
                            layout: Default::default(),
                        },
                        one,
                    );
                }
            }
            if placing && let Some(cull) = &targets.cull {
                // Both phases' counts, summed on the GPU as they cull.
                let totals = cull.totals.clone();
                let bytes = (MAX_LEVELS as u64 + 1) * 4;
                let buffer = targets.readback(gpu, bytes, ReadKind::Counts).clone();
                encoder.copy_buffer_to_buffer(&totals, 0, &buffer, 0, bytes);
            }
            let lines = device.lines.as_ref().map_or(0, |l| l.1);
            let sky = scene.sky.is_some();
            // The blend and occlusion passes sample a copy of the opaque
            // depth (the blend pass also tests against the original; one
            // texture cannot be both). Depth copies must be whole textures.
            if scene.occlusion.is_some() || !blended.is_empty() {
                let whole = |texture| wgpu::TexelCopyTextureInfo {
                    texture,
                    mip_level: 0,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::All,
                };
                encoder.copy_texture_to_texture(
                    whole(&targets.depth.0),
                    whole(&targets.depth_copy.0),
                    targets.depth.0.size(),
                );
            }
            if scene.occlusion.is_some() {
                let frame =
                    device.frame_group(gpu, &globals, &lights, shadow, Some(&targets.depth_copy.1));
                let color = |view, load| {
                    Some(wgpu::RenderPassColorAttachment {
                        view,
                        resolve_target: None,
                        ops: wgpu::Operations {
                            load,
                            store: wgpu::StoreOp::Store,
                        },
                        depth_slice: None,
                    })
                };
                // Both passes' timestamp slots up front: the writes borrow the
                // query set for as long as the passes live.
                let slots = (targets.timing.as_mut()).map_or((None, None), |t| {
                    (t.pass("occlusion"), t.pass("occlusion apply"))
                });
                let set = targets.timing.as_ref().map(|t| &t.set);
                let writes = |slots: Option<(u32, u32)>| {
                    set.zip(slots).map(|(query_set, (begin, end))| {
                        wgpu::RenderPassTimestampWrites {
                            query_set,
                            beginning_of_pass_write_index: Some(begin),
                            end_of_pass_write_index: Some(end),
                        }
                    })
                };
                let pass = |encoder: &mut wgpu::CommandEncoder,
                            label,
                            view,
                            load,
                            timestamp_writes,
                            half: bool| {
                    let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                        label: Some(label),
                        color_attachments: &[color(view, load)],
                        depth_stencil_attachment: None,
                        timestamp_writes,
                        occlusion_query_set: None,
                        multiview_mask: None,
                    });
                    if half {
                        let [w, h] = size.map(|s| s.div_ceil(2));
                        let (x, y) = ((sx / 2).min(w), (sy / 2).min(h));
                        pass.set_scissor_rect(
                            x,
                            y,
                            sw.div_ceil(2).min(w - x),
                            sh.div_ceil(2).min(h - y),
                        );
                    } else {
                        pass.set_scissor_rect(sx, sy, sw, sh);
                    }
                    pass.set_bind_group(0, &frame, &[]);
                    // Each pass ends before the encoder records the next.
                    pass.forget_lifetime()
                };
                {
                    // w < 0: no surface (fs_occlusion).
                    let white = wgpu::LoadOp::Clear(wgpu::Color {
                        r: 0.,
                        g: 0.,
                        b: 0.,
                        a: -1.,
                    });
                    let mut estimate = pass(
                        encoder,
                        "editor occlusion",
                        &targets.occlusion.1,
                        white,
                        writes(slots.0),
                        true,
                    );
                    estimate.set_pipeline(&device.occlusion[0]);
                    estimate.draw(0..3, 0..1);
                }
                let group = gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some("editor occlusion"),
                    layout: &device.kit.occlusion_layout,
                    entries: &[entry(
                        0,
                        wgpu::BindingResource::TextureView(&targets.occlusion.1),
                    )],
                });
                let mut apply = pass(
                    encoder,
                    "editor occlusion apply",
                    target,
                    wgpu::LoadOp::Load,
                    writes(slots.1),
                    false,
                );
                apply.set_pipeline(&device.occlusion[1]);
                apply.set_bind_group(1, &group, &[]);
                apply.draw(0..3, 0..1);
            }
            // Before the blend pass, so lines (the selection box among them)
            // and water stay on top of it.
            if let Some(outline) =
                (outline.as_ref()).filter(|o| !o.rows.is_empty() || !o.ids.is_empty())
            {
                // Batch ids name pick handles, which a new scene renumbers;
                // rows alone stay until the outline changes.
                if !outline.ids.is_empty()
                    || device.outline_marks.as_ref().map(|m| m.0) != Some(outline_version)
                {
                    let marks = outline_marks(outline, &device.pick_ids);
                    let buffer = gpu
                        .device
                        .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                            label: Some("editor outline marks"),
                            contents: bytemuck::cast_slice(&marks),
                            usage: wgpu::BufferUsages::STORAGE,
                        });
                    device.outline_marks = Some((outline_version, buffer));
                }
                let buffer = &device.outline_marks.as_ref().expect("outline marks").1;
                let group = gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some("editor outline"),
                    layout: &device.outline.0,
                    entries: &[
                        entry(0, wgpu::BindingResource::TextureView(&targets.ids.1)),
                        entry(1, buffer.as_entire_binding()),
                    ],
                });
                let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("editor outline"),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: target,
                        resolve_target: None,
                        ops: wgpu::Operations {
                            load: wgpu::LoadOp::Load,
                            store: wgpu::StoreOp::Store,
                        },
                        depth_slice: None,
                    })],
                    depth_stencil_attachment: None,
                    timestamp_writes: None,
                    occlusion_query_set: None,
                    multiview_mask: None,
                });
                pass.set_scissor_rect(sx, sy, sw, sh);
                pass.set_pipeline(&device.outline.1);
                pass.set_bind_group(0, &group, &[]);
                pass.draw(0..3, 0..1);
            }
            if sky || !blended.is_empty() || markers > 0 || lines > 0 {
                let depth = (!blended.is_empty()).then_some(&targets.depth_copy.1);
                let blend_frame = device.frame_group(gpu, &globals, &lights, shadow, depth);
                let timestamp_writes = targets.timing.as_mut().and_then(|t| t.render("blend"));
                let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("editor blend"),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: target,
                        resolve_target: None,
                        ops: wgpu::Operations {
                            load: wgpu::LoadOp::Load,
                            store: wgpu::StoreOp::Store,
                        },
                        depth_slice: None,
                    })],
                    depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                        view: &targets.depth.1,
                        depth_ops: Some(wgpu::Operations {
                            load: wgpu::LoadOp::Load,
                            store: wgpu::StoreOp::Discard,
                        }),
                        stencil_ops: None,
                    }),
                    timestamp_writes,
                    occlusion_query_set: None,
                    multiview_mask: None,
                });
                pass.set_scissor_rect(sx, sy, sw, sh);
                pass.set_bind_group(0, &blend_frame, &[]);
                if sky {
                    pass.set_pipeline(&device.sky);
                    pass.draw(0..3, 0..1);
                }
                if !blended.is_empty() {
                    pass.set_pipeline(&device.blend);
                    for (_, draw) in &blended {
                        pass.set_bind_group(1, &draw.mesh.group, &[]);
                        let offset = push([0; 4]);
                        pass.set_bind_group(2, &batches, &[offset]);
                        draw.mesh
                            .draw(&mut pass, draw.first..draw.first + draw.count);
                    }
                }
                if lines > 0 {
                    pass.set_pipeline(&device.line);
                    pass.draw(0..6, 0..lines);
                }
                if markers > 0 {
                    pass.set_pipeline(&device.marker);
                    pass.draw(0..6, 0..markers);
                }
            }
            drop(push);
            // Lands before this encoder runs: queue writes precede the
            // next submission's command buffers.
            gpu.queue.write_buffer(&ids, 0, &slots_.bytes);
        })();
        if let Some(targets) = slots.get_mut(slot).and_then(|s| s.targets.as_mut())
            && let Some(timing) = targets.timing.as_mut()
            && timing.used > 0
        {
            let used = std::mem::take(&mut timing.used);
            encoder.resolve_query_set(&timing.set, 0..used, &timing.resolve, 0);
            let resolve = timing.resolve.clone();
            let labels = timing.labels[..used as usize / 2].to_vec();
            let buffer = targets
                .readback(gpu, u64::from(used) * 8, ReadKind::Times(labels))
                .clone();
            encoder.copy_buffer_to_buffer(&resolve, 0, &buffer, 0, u64::from(used) * 8);
        }
        state.drawn_slots |= bit;
        if let (Some(started), Some(built), Some(encode)) =
            (state.frame_mark, state.built_mark, state.encode_start)
        {
            let ms = |d: std::time::Duration| d.as_secs_f32() * 1000.;
            let now = std::time::Instant::now();
            let built_now = state.frame_built;
            let st = &mut state.stats;
            if built_now {
                let build = ms(built.saturating_duration_since(started));
                let layout = ms(encode.saturating_duration_since(built));
                st.build_ms = Stats::blend(st.build_ms, build);
                st.layout_ms = Stats::blend(st.layout_ms, layout);
            }
            let encoding = ms(now.saturating_duration_since(encode));
            st.encode_ms = Stats::blend(st.encode_ms, encoding);
            st.cpu_ms = st.build_ms + st.layout_ms + st.encode_ms;
        }
    }
}

/// A unit cube centered on the origin, colored per face. Handy for tests,
/// placeholders, and objects whose model failed to load.
pub fn cube(color: u32, material: Material) -> Mesh {
    let mut data = MeshData {
        material,
        ..Default::default()
    };
    for axis in 0..3 {
        for side in [-0.5f32, 0.5] {
            let (u, v) = ((axis + 1) % 3, (axis + 2) % 3);
            let base = data.positions.len() as u32;
            for (a, b) in [(-0.5, -0.5), (0.5, -0.5), (0.5, 0.5), (-0.5, 0.5)] {
                let mut p = [0.; 3];
                p[axis] = side;
                p[u] = a;
                p[v] = b;
                data.positions.push(p);
                data.uvs.push([a + 0.5, b + 0.5]);
                // Slightly darker faces on the negative sides read as depth
                // even in the unlit material.
                let shade = if side < 0. { 0.8 } else { 1. };
                data.colors.push(
                    [16, 8, 0]
                        .map(|s| (((color >> s) & 255) as f32 * shade) as u32)
                        .into_iter()
                        .fold(0, |c, v| c << 8 | v),
                );
            }
            data.indices.extend([0, 1, 2, 0, 2, 3].map(|i| base + i));
        }
    }
    Mesh::new(data).expect("valid cube")
}
