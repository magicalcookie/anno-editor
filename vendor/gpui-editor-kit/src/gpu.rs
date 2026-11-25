// Draws instanced meshes and markers with GPUI's wgpu device, inside the
// window's frame. Geometry stays resident between camera moves; a new scene
// replaces the visible one only once every batch it needs is uploaded.
//
// A frame runs in fixed stages (`Stage`): the sun's shadow map, opaque
// geometry into an HDR target, ambient occlusion, water, the sky, blended
// surfaces over a copy of the opaque depth, one post pass that tone maps
// the HDR target onto the window's, then overlays (lines, markers, the
// selection outline) in display values. A scene plugs in drawing of its
// own (`Pass`, gpu/pass.rs) at the opaque, water, sky, blend and overlay
// stages, and any of those passes can cast into the shadow map
// (`Pass::shadow`); that is how a game brings its sea without the kit
// knowing the game. The kit's ground is such a pass too (`TerrainPass`,
// gpu/terrain.rs), listed first unless the scene asks for none
// (`GroundDraw::Data`). At the opaque and sky stages a game's pass can draw
// instead of the kit's ground or sky backdrop (`Pass::replaces`), while the
// scene's terrain and sky still supply the ground data and the light.
//
// Linux, Windows and macOS: each viewport records on a thread of its own into a
// texture GPUI draws in the same frame, through the texture handoff in
// vendor/zed (see PATCH.md and `Viewport`).
/// The kit's `fog` (gpu/frame.wgsl `kit_fog`), a literal so that
/// `FRAME_WGSL` can end with it.
macro_rules! kit_fog {
    () => {
        "fn fog(color: vec3<f32>, world: vec3<f32>) -> vec3<f32> { return kit_fog(color, world); }\n"
    };
}

mod bc;
mod control;
mod device;
mod exposure;
mod heights;
mod light;
mod offscreen;
mod pass;
mod placed;
mod residency;
pub mod terrain;
#[cfg(test)]
mod tests;
mod visbuf;

pub use control::{ControlMap, Kept, ResidentControl};
pub use device::{OnDevice, install, lose_window_device};
pub use heights::HeightMap;
pub use light::{
    AutoExposure, Curve, Environment, Falloff, Fog, Light, LocalLight, MAX_CASCADES,
    MAX_LOCAL_LIGHTS, Occlusion, Sea, Shadows, Sky, SkyMap, Spot, Tone,
};
pub use offscreen::{Busy, Offscreen, busy, join_at_exit};
pub use pass::{
    AfterOpaque, Caster, CloudShade, DEPTH_FORMAT, FRAME_WGSL, HDR_FORMAT, OPAQUE_TARGETS, Pass,
    Prepare, Record, SHADOW_FORMAT, SSR_WGSL, Ssr, Stage, ssr_module,
};
pub use placed::{
    Families, Family, FamilyId, Lighting, MAX_LEVELS, MAX_PAYLOAD, MAX_SLOTS, Piece, Pose, ViewMode,
};
pub use residency::{TEXTURE_RESIDENCY, floor_level};
use terrain::TerrainTiles;
pub use terrain::{PageCounts, Terrain, TerrainPass};

use crate::camera::{Camera, Extent, Frustum};
use crate::heightfield::{Grid, Rect};
use crate::math::{self, Mat4, Vec3};
use crate::texture::{Block, Image, Texture};
use anyhow::{Result, bail, ensure};
use bytemuck::{Pod, Zeroable};
use gpui::{Bounds, Pixels, Point, ScaledPixels, Window, point, px};
use heights::ResidentHeights;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, OnceLock};
use wgpu::util::DeviceExt as _;

#[derive(Clone, Debug, Default, PartialEq)]
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
    /// Lit like `Lit` and blended over what is already drawn, as Anno's
    /// mesh decals (`model_decal.fx`) blend over the ground and buildings:
    /// paving, dirt, stains. Placements and batches alike draw after every
    /// opaque mesh; never pickable and never casting shadows.
    Decal(Decal),
}

/// UVs from world X/Z (functions_pbr.fxh `GetWorldSpaceTilingTexCoord`):
/// `(x, -z) / scale`, or along the diagonals `(x + z, z - x) / scale`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct WorldTiling {
    pub scale: f32,
    pub diagonal: bool,
}

/// What the scene's lit surfaces take beside the light: how bright the
/// light they give off is, and numbers of the host's own for its families
/// and lighting.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Glow {
    /// Scales the light surfaces give off themselves: the kit family's and
    /// decals' [`MeshData::emissive`] and a ground's glow
    /// ([`terrain::Ground::glow`]). WGSL reads it as `lights.glow.w`.
    pub strength: f32,
    /// Opaque to the kit: a host family or lighting reads word `k` as
    /// `lights.host[k / 4][k % 4]`, the same for every pixel of the frame
    /// (an hour, a scene-wide colour, a light profile's constants).
    /// Changing them rewrites the uniform and builds no pipeline.
    pub host: [f32; HOST_WORDS],
}

/// Words of [`Glow::host`]: eight `vec4`s of the frame's uniform. A host
/// that lays out two sets of numbers (a model family's and its lighting's)
/// splits them between its crates; the kit has no say.
pub const HOST_WORDS: usize = 32;

impl Default for Glow {
    fn default() -> Self {
        Self {
            strength: 1.,
            host: [0.; HOST_WORDS],
        }
    }
}

/// Pictures the host's lighting reads that no material holds (a view
/// mode's moving light pattern): frame WGSL's `host_texture(uv, layer)`,
/// sampled repeating at level 0. The kit never reads them.
#[derive(Clone, Debug, PartialEq)]
pub struct HostTexture {
    pub width: u32,
    pub height: u32,
    pub layers: u32,
    /// RGBA8, as stored (no sRGB decode), layer after layer, rows from the
    /// top. The same `Arc` uploads once.
    pub texels: Arc<Vec<u8>>,
}

/// A planar mirror the scene is seen in, such as still water: each view
/// first draws the scene once more from its camera mirrored through the
/// horizontal plane at `level` (`Camera::mirrored`), opaque surfaces alone,
/// cut at the plane, and passes read it through frame WGSL's
/// `mirror_reflection(pixel)`: the mirror's colour as its opaque surfaces left
/// the HDR target (fog included), with alpha 1 where a surface drew and 0
/// where the mirror shows only sky.
/// One more opaque scene draw a view, every frame.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Reflection {
    pub level: f32,
}

/// The oblique near plane a mirror view clips at: depth runs `1 − k·(y −
/// level)/w`, so the plane itself is the near plane (depth 1), what lies
/// under it clips, and depth still falls with distance along every ray. `k`
/// keeps rays up to 72° off the view's axis inside the far end (depth 0).
const MIRROR_SLOPE: f64 = 0.3;

/// A mirror view's pixels per view pixel, each way. 2070's sea, the one
/// mirror, weighs it by pow(n.y, 64) and moves it by its waves: at half
/// size its frames stay within 71 dB of full size (at most 27 levels off,
/// on shore reflections) and the mirror costs 0.39 ms instead of 1.13 on
/// n_m02 at 5K.
const MIRROR_SCALE: f32 = 0.5;

/// `GpuDraw::tiling` and `Params::blend.y`: the scale, negative along the
/// diagonals, 0 off (gpu.wgsl `tiled`).
fn tiling_code(tiling: Option<WorldTiling>) -> f32 {
    tiling
        .filter(|t| t.scale.is_finite() && t.scale > 0.)
        .map_or(0., |t| if t.diagonal { -t.scale } else { t.scale })
}

/// A mesh decal's coverage and how far it moves toward the viewer.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Decal {
    /// Coverage is this times texture alpha, times the alpha map's
    /// smoothstepped green when there is one (`MeshData::alpha_map`).
    pub opacity: f32,
    /// World units the decal moves toward the viewer along the line of
    /// sight, so it wins over the surface it lies on (117's 0.01, or 0.2
    /// for `cHigherDepthBias`; 1800's 0.8 on the ground, 0.01 on models).
    pub bias: f32,
    /// The bias grows at grazing angles, over the cosine of the view from
    /// straight up (at most 10x), as 117's camera-angle bias does. 1800
    /// moves its decals the bias alone (`ApplyDepthBias`).
    pub grazing: bool,
    /// Lies on the ground and draws with it: right after the ground stage
    /// and before every model, so a model shows whole however near the
    /// ground it comes (Anno 1800's terrain decals, `decal_pbr.fx`).
    /// Otherwise it blends over everything opaque, as a decal on a model
    /// does.
    pub ground: bool,
    /// A projected decal's fade toward the sides of its box. The mesh must
    /// then be the box's unit grid: X and Z from -0.5 to 0.5.
    pub edge: Option<DecalEdge>,
}

impl Decal {
    /// The bias as the shaders take it (gpu.wgsl `decal_lift`): negative
    /// for a fixed move, positive for one that grows at grazing angles.
    fn lift(&self) -> f32 {
        if self.grazing { self.bias } else { -self.bias }
    }
}

/// How a projected decal fades toward its box's sides (`decal_pbr.fx`).
/// Both measure from the whole half-metre grid cells the box covers.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DecalEdge {
    /// Half the box across its own X and Z, in metres.
    pub extents: [f32; 2],
    pub fade: EdgeFade,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum EdgeFade {
    /// `cEnableBorderFadeout`: out over `distance` metres past the grid.
    /// With `height` (threshold, falloff) the decal frays instead where the
    /// height map in `MeshData::alpha_map`'s red is low.
    Grid {
        distance: f32,
        height: Option<[f32; 2]>,
    },
    /// `cExperimentalEnableAlphaBasedFade`: smoothly from `border` metres
    /// past the grid, over `fade` metres.
    Smooth { border: f32, fade: f32 },
}

impl DecalEdge {
    /// Mode (0 none, 1 grid, 2 grid with height map, 3 smooth), extents
    /// and the three parameters, for the shaders' `decal_edge`.
    fn code(edge: Option<&Self>) -> ([f32; 4], [f32; 4]) {
        let Some(e) = edge else {
            return ([0.; 4], [0.; 4]);
        };
        let [x, z] = e.extents;
        match e.fade {
            EdgeFade::Grid {
                distance,
                height: None,
            } => ([1., x, z, distance], [0.; 4]),
            EdgeFade::Grid {
                distance,
                height: Some([threshold, falloff]),
            } => ([2., x, z, distance], [threshold, falloff, 0., 0.]),
            EdgeFade::Smooth { border, fade } => ([3., x, z, border], [fade, 0., 0., 0.]),
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
    /// Adds its light to what is behind instead of covering it, unlit, as
    /// glow and fire particles draw. Its alpha scales how much it adds.
    pub additive: bool,
    /// Takes its colour as it is, unshaded, as particles without lighting
    /// draw. Additive surfaces are always unlit.
    pub unlit: bool,
}

impl Default for Blend {
    fn default() -> Self {
        Self {
            color: 0xffffff,
            opacity: 0.5,
            fade: 0.,
            fresnel: 0.,
            depth: 0.,
            additive: false,
            unlit: false,
        }
    }
}

/// Up to four tiling textures blended over a `Lit` placement by the
/// channels of a control map on its UVs, a generic splat: each channel
/// weighs its layer, sharpened by the layer albedo's alpha, the blend's
/// albedo replaces the mesh's (or is tinted by it) and its normal and gloss
/// replace the normal map's (or bend it further); the normal map's blue
/// stays. How sharp, what shows where no layer weighs in and how the tint
/// scales are the game's (`sharpness`, `alpha_floor`, `fallback`, `tint`).
#[derive(Clone, Debug)]
pub struct MeshSplat {
    /// Layer weights in red, green, blue and alpha.
    pub control: Image,
    /// One per control channel; a channel without a layer weighs nothing.
    pub layers: [Option<MeshSplatLayer>; 4],
    /// Each layer weighs in by its control channel raised to this times its
    /// albedo's alpha, so a layer's alpha sharpens its edge where layers
    /// meet: 1 with opaque layers blends the channels as they are.
    pub sharpness: f32,
    /// The least alpha `sharpness` multiplies, so a clear layer still
    /// blends.
    pub alpha_floor: f32,
    /// Linear albedo where no layer weighs in.
    pub fallback: [f32; 3],
    /// The albedo texture's red times this multiplies the blend; `None`,
    /// the blend is the albedo.
    pub tint: Option<f32>,
    /// The blend's normal bends the normal map's; off, it replaces it.
    /// Either way only meshes with a normal map take the blend's normal.
    pub mix_normals: bool,
    /// Layer UVs grow with the placement's average scale, so a layer keeps
    /// its size in the world however large the mesh is placed.
    pub scale_invariant: bool,
    /// Layers take their UVs from world X/Z, `(x, -z)` times their
    /// tiling, as [`WorldTiling`] does, so they run on across placements
    /// and pieces; the control map stays on the mesh's UVs. Their normals
    /// are then the world's, leaned by the surface's own normal.
    pub world_tiling: bool,
}

/// One [`MeshSplat`] layer.
#[derive(Clone, Debug)]
pub struct MeshSplatLayer {
    /// Albedo; its alpha sharpens the layer's edge against the others.
    pub albedo: Image,
    /// Tangent-space normal in red and green, gloss in alpha. Without one
    /// the layer is flat and matte.
    pub normal: Option<Image>,
    /// Repeats over the mesh's UV square.
    pub tiling: f32,
}

impl MeshSplat {
    fn validate(&self) -> Result<()> {
        self.control.validate()?;
        ensure!(
            self.layers.iter().any(Option::is_some),
            "Mesh splat has no layers"
        );
        for layer in self.layers.iter().flatten() {
            layer.albedo.validate()?;
            if let Some(n) = &layer.normal {
                n.validate()?;
            }
            ensure!(
                layer.tiling.is_finite() && layer.tiling > 0.,
                "Mesh splat layer tiling must be positive"
            );
        }
        let numbers = [self.sharpness, self.alpha_floor, self.tint.unwrap_or(0.)];
        ensure!(
            numbers.iter().chain(&self.fallback).all(|v| v.is_finite()),
            "Mesh splat has a non-finite parameter"
        );
        Ok(())
    }
}

/// Which specular terms a lit surface keeps, as bits the shaders read
/// (`Surface::specular`). Games leave terms out per shader: Anno's plants
/// and ground define `BRDF_DISABLE_ENVMAP_REFLECTION`,
/// `BRDF_DISABLE_SPEC_LIGHTING` or `BRDF_DISABLE_SPEC_ON_ZERO_GLOSS`,
/// because on a dark matte surface seen edge-on the sky's reflection
/// outshines the albedo and it reads sky grey.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Specular(pub u8);

impl Specular {
    pub const FULL: Self = Self(0);
    /// No reflection of the sky.
    pub const NO_SKY: Self = Self(1);
    /// No highlight from the sun.
    pub const NO_SUN: Self = Self(2);
    /// Both terms fade out as gloss falls from 0.25 to 0.
    pub const MATTE_FADES: Self = Self(4);
}

impl std::ops::BitOr for Specular {
    type Output = Self;
    fn bitor(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }
}

/// Mesh input. `uvs`, `colors` and `normals` are either empty or one per position.
#[derive(Clone, Debug)]
pub struct MeshData {
    pub positions: Vec<Vec3>,
    pub uvs: Vec<[f32; 2]>,
    /// 0xRRGGBB. Empty means white. On `Blend` meshes the top byte is the
    /// vertex's transparency, 0 opaque: a particle fading out.
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
    /// Where the texture's alpha cuts, as an `AlphaTest(smoothstep(0, 0.9,
    /// alpha), reference)`: for materials a game tests at a reference of
    /// its own. `None` cuts as foliage always has here, at half on the
    /// drawing path's own measure.
    pub alpha_ref: Option<f32>,
    /// Tangent-space normals for `Lit` meshes, on the same UVs as `texture`:
    /// X and Y in red and green (Y along +V, texel rows downward), Z
    /// rebuilt; alpha is gloss, 1 a mirror. The tangent frame comes from
    /// screen derivatives, so meshes need no tangents.
    pub normal_map: Option<Image>,
    /// Metalness in red and ambient occlusion in alpha (1 open), same UVs.
    /// Only a [`Tone`]d scene reads metalness.
    pub surface_map: Option<Image>,
    /// `Decal` meshes: coverage also takes the smoothstepped green of this
    /// map, on the mesh's UVs even under `world_tiling` (Anno's `cAlphaTex`).
    pub alpha_map: Option<Image>,
    /// `Lit` and `Decal` meshes: texture, normal and surface maps take
    /// their UVs from world X/Z instead of the mesh, so a ground tile's
    /// pattern runs on across neighbours (Anno's `cEnableWorldSpaceTiling`).
    pub world_tiling: Option<WorldTiling>,
    /// `Lit` meshes of the kit's family and `Decal` meshes: light the
    /// surface gives off itself, linear (it may pass 1). The normal map's
    /// blue, which its Z leaves free, is how far the albedo texel times
    /// this colour, times the scene's [`Glow::strength`], replaces the
    /// surface's own shading. Masks fade in below 0.35 as their square:
    /// block compression leaves faint noise in black. Without a normal map
    /// nothing glows. Impostors bake none of it.
    pub emissive: Option<[f32; 3]>,
    /// `Lit` placements: tiling textures blended by a control map over
    /// the albedo, normal map and gloss ([`MeshSplat`]).
    // TODO(alex): drawn by the arena only; a mesh drawn as a scene batch
    // (asset previews, an arena out of texture slots) shows its albedo.
    pub splat: Option<Arc<MeshSplat>>,
    /// `Lit` meshes: how much the scene terrain's colors (times its
    /// `color_gain`) under each fragment multiply the albedo, 0 to 1, so
    /// rocks and plants take on the ground's tint. 0 off.
    pub terrain_tint: f32,
    /// `Lit` meshes: which specular terms light them ([`Specular`]).
    pub specular: Specular,
    /// Vertex colors are multiplied by this. Above 1 lets them brighten what
    /// they tint, e.g. a terrain tint multiplier stored as `color / gain`.
    pub color_gain: f32,
    pub material: Material,
    /// `Lit` meshes: the surface family that shades the mesh, one of its
    /// renderer's ([`Renderer::with_families`]); the kit's own by default.
    /// Placements draw it from the arena. Its pieces get impostors
    /// only where the family bakes ([`Family::bake`]).
    pub family: FamilyId,
    /// A host family's own words for the mesh, read in WGSL with
    /// `family_word(material, k)`: at most [`MAX_PAYLOAD`]. The kit's
    /// family takes none (its glow is `emissive`), and a host family's mesh
    /// that brings words gets no glow from the kit.
    pub payload: Vec<u32>,
    /// A host family's own textures beside the four the kit samples (at
    /// most [`MAX_SLOTS`]): the family samples slot `k` with
    /// `family_texel(family_slot(material, k), uv, duv)`, from the arena's
    /// array or, for a kit mesh, the mesh's own bindings. Residency keeps
    /// the levels the mesh's UVs ask of each in the arena.
    pub slots: Vec<Image>,
    /// How many times slot `k` repeats across the mesh's UVs where the
    /// family samples it scaled, so residency holds the levels that asks
    /// for: 1 where this is shorter.
    pub slot_repeats: Vec<f32>,
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
    /// Placements only: bend each vertex to the scene terrain, see
    /// [`TerrainAdapt`]. The top byte of each vertex color is how rigid
    /// that vertex stays, 0 (follows fully) to 255.
    pub terrain_adapt: Option<TerrainAdapt>,
    /// Per vertex, four joints of a placement's [`Pose`] and then their
    /// weights (unorm8, normalised by their sum): the vertex follows their
    /// blend. Empty follows the pose's joint 0 whole. A mesh that skins
    /// thins at no distance. Placements in the arena only.
    pub skin: Vec<[u8; 8]>,
}

/// How a placed mesh's vertices follow the scene terrain (Anno's
/// `ADJUST_TO_TERRAIN_HEIGHT`). Only placements that allow it bend
/// ([`adapting`]); the rest draw rigid.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TerrainAdapt {
    /// Keep each vertex's height above the placement's origin, measured
    /// from the ground under the vertex: a foundation authored on flat
    /// ground drapes over a slope.
    Relative,
    /// Put every vertex on the ground.
    Absolute,
    /// Put every vertex on the ground with the ground's normal, whatever
    /// the placement allows: Anno's projected decals, laid over the
    /// terrain under their box (`decal_pbr.fx`).
    Drape,
}

/// `GpuDraw::flags` bits 3-4: 0 rigid, 1 relative,
/// 2 absolute, 3 drape (gpu.wgsl `adapt_to_terrain`).
pub(crate) fn adapt_code(adapt: Option<TerrainAdapt>) -> f32 {
    match adapt {
        None => 0.,
        Some(TerrainAdapt::Relative) => 1.,
        Some(TerrainAdapt::Absolute) => 2.,
        Some(TerrainAdapt::Drape) => 3.,
    }
}

/// `GpuDraw::flags` bits 24-31: the albedo alpha below which a texel is
/// cut, in 255ths, 0 for placed.wgsl's 0.5. The arena compares raw alpha,
/// so a reference on smoothstep(0, 0.9, alpha) is taken back through it:
/// 0.3 cuts below 0.327.
pub(crate) fn alpha_cut(reference: Option<f32>) -> u32 {
    let Some(r) = reference else { return 0 };
    // The smoothstep's inverse on 0..1.
    let x = 0.5 - ((1. - 2. * r.clamp(0., 1.)).asin() / 3.).sin();
    ((0.9 * x * 255.).round() as u32).clamp(1, 255)
}

/// `m` with its bottom row carrying terrain adaption for placed.wgsl:
/// whether the placement may bend to the ground (`TerrainAdapt` meshes do,
/// others ignore it), whether the whole piece rides the ground at its own
/// origin (`follow`, a sub-object placed relative to the terrain), and
/// `rise`, the piece origin's height above the placement's origin, from
/// which the shader finds the object's own height. Points still transform
/// as before as long as a reader takes only xyz of `m * p`: the row is not
/// a projective one, so `m * p`'s w is not the point's, and a product with
/// another matrix must carry the row across untouched (select.wgsl
/// `dragged`).
pub fn adapting(m: Mat4, allow: bool, follow: bool, rise: f32) -> Mat4 {
    let mut m = m;
    m[3] = f32::from(u8::from(allow));
    m[7] = rise;
    m[11] = f32::from(u8::from(follow));
    m
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
            alpha_ref: None,
            normal_map: None,
            surface_map: None,
            alpha_map: None,
            world_tiling: None,
            emissive: None,
            splat: None,
            terrain_tint: 0.,
            specular: Specular::FULL,
            color_gain: 1.,
            material: Material::default(),
            family: FamilyId::LIT,
            payload: Vec::new(),
            slots: Vec::new(),
            slot_repeats: Vec::new(),
            casts_shadow: true,
            heights: None,
            color_map: None,
            thinning: None,
            terrain_adapt: None,
            skin: Vec::new(),
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
        let (indices, _) = self.simplified_indices(keep)?;
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
            out.indices = in_rank_order(&out.indices, &parts);
            thin.parts = parts;
        }
        Some(out)
    }

    /// `simplified`'s triangles, over this mesh's own vertices, and how far
    /// they stray from the mesh's surface at most, in its own units
    /// (meshoptimizer's measure).
    fn simplified_indices(&self, keep: f32) -> Option<(Vec<u32>, f32)> {
        let bytes: &[u8] = bytemuck::cast_slice(&self.positions);
        let vertices = meshopt::VertexDataAdapter::new(bytes, 12, 0).ok()?;
        // Relative to the mesh's extent.
        let mut strays = 0f32;
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
                    Some(&mut strays),
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
                Some(&mut strays),
            ),
        };
        if indices.is_empty() || indices.len() * 4 > self.indices.len() * 3 {
            return None;
        }
        Some((indices, strays * meshopt::simplify_scale(&vertices)))
    }
}

/// `indices`' triangles sorted by their first vertex's thinning rank in
/// `parts`, stably: collapses reorder triangles, and prefix draws need rank
/// order.
fn in_rank_order(indices: &[u32], parts: &[[f32; 4]]) -> Vec<u32> {
    let mut tris: Vec<[u32; 3]> = indices.as_chunks::<3>().0.to_vec();
    tris.sort_by(|a, b| parts[a[0] as usize][3].total_cmp(&parts[b[0] as usize][3]));
    tris.into_iter().flatten().collect()
}

/// Reorders `indices` (over `data`'s vertices) for the post-transform
/// cache; a thinning mesh's within each part's run, keeping rank order.
fn order_triangles(data: &MeshData, indices: &mut [u32]) {
    let n = data.positions.len();
    match &data.thinning {
        Some(t) if t.parts.len() == n => {
            let rank = |tri: &[u32]| t.parts[tri[0] as usize][3].to_bits();
            // Each part numbered on its own: the optimizer sets up tables
            // as long as the vertex count it is given, and a laurel's
            // thousand clumps over the whole mesh's vertices made loading
            // quadratic (45% of the continental island's load CPU). The
            // optimizer only indexes by vertex, so the order is the same.
            let mut local = vec![u32::MAX; n];
            let mut back: Vec<u32> = Vec::new();
            let mut part: Vec<u32> = Vec::new();
            let mut start = 0;
            while start < indices.len() {
                let r = rank(&indices[start..]);
                let mut end = start + 3;
                while end < indices.len() && rank(&indices[end..]) == r {
                    end += 3;
                }
                part.clear();
                for &v in &indices[start..end] {
                    let l = &mut local[v as usize];
                    if *l == u32::MAX {
                        *l = back.len() as u32;
                        back.push(v);
                    }
                    part.push(*l);
                }
                meshopt::optimize_vertex_cache_in_place(&mut part, back.len());
                for (out, &l) in indices[start..end].iter_mut().zip(&part) {
                    *out = back[l as usize];
                }
                for &v in &back {
                    local[v as usize] = u32::MAX;
                }
                back.clear();
                start = end;
            }
        }
        Some(_) => {}
        None => meshopt::optimize_vertex_cache_in_place(indices, n),
    }
}

/// Reorders `data`'s triangles for the post-transform cache and its
/// vertices into first use, so each draw reads its vertices in order:
/// placements draw millions of triangles a frame, most of them twice
/// (depth, then shading). Unused vertices go. A thinning mesh keeps its
/// triangles in rank order, each part reordered within itself; ranking
/// had scattered its vertex reads across the whole buffer. Returns each
/// old vertex's new number (`u32::MAX` gone), empty when nothing moved.
fn order_for_gpu(data: &mut MeshData) -> Vec<u32> {
    let n = data.positions.len();
    if data.indices.is_empty() || data.indices.iter().any(|&i| i as usize >= n) {
        return Vec::new(); // `new` reports it.
    }
    let mut indices = std::mem::take(&mut data.indices);
    order_triangles(data, &mut indices);
    data.indices = indices;
    // Vertices numbered by first use (meshopt 0.7's optimize_vertex_fetch_remap
    // cuts its table to the vertices used, losing the rest of the map).
    let mut remap = vec![u32::MAX; n];
    let mut used = 0usize;
    for &i in &data.indices {
        if remap[i as usize] == u32::MAX {
            remap[i as usize] = used as u32;
            used += 1;
        }
    }
    fn moved<T: Copy + Default>(v: &mut Vec<T>, remap: &[u32], used: usize) {
        if v.len() != remap.len() {
            return;
        }
        let mut out = vec![T::default(); used];
        for (old, &new) in remap.iter().enumerate() {
            if (new as usize) < used {
                out[new as usize] = v[old];
            }
        }
        *v = out;
    }
    moved(&mut data.positions, &remap, used);
    moved(&mut data.uvs, &remap, used);
    moved(&mut data.colors, &remap, used);
    moved(&mut data.normals, &remap, used);
    moved(&mut data.skin, &remap, used);
    if let Some(t) = &mut data.thinning {
        moved(&mut t.parts, &remap, used);
    }
    for i in &mut data.indices {
        *i = remap[*i as usize];
    }
    remap
}

/// Immutable geometry. Share it through `Arc`: the renderer keys GPU
/// residency on the `Arc`, so the same `Arc` in a later scene costs nothing.
pub struct Mesh {
    data: MeshData,
    bounds: [Vec3; 2],
    counts: Counts,
    /// `compact`'s geometry: vertices as the arena stores them and
    /// indices, through meshopt's codecs. A level made by `compact_levels`
    /// keeps its indices only.
    packed: Option<(Vec<u8>, Vec<u8>)>,
    /// The mesh whose vertices a `compact_levels` level draws from.
    shares: Option<Arc<Mesh>>,
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
    /// The vertices each bucket's own triangles (those past the bucket
    /// before) reach, first and past the last: the arena numbers each
    /// bucket's indices apart (placed::Stream).
    thin_vertices: Option<Vec<[u32; 2]>>,
    /// `uv_density`: how fine a texture level a placement can sample.
    uv_density: f32,
    /// The grids the placement arena stores positions and UVs on.
    quant: Quant,
    /// A `compact_levels` level: how far it strays from the full mesh's
    /// surface, in the mesh's units.
    strays: Option<f32>,
}

/// The grids the placement arena stores a mesh's positions and UVs on, 16
/// bits a coordinate (`ArenaVertex`): per axis (x, y, z, u, v) the grid's
/// origin, and log2 of the step of positions (shared by x, y and z) and of
/// u and of v. Steps are powers of two: meshes of like size snap shared
/// corners alike, and the games' half floats (positions P4h, UVs T2h) land
/// on the grid but for the smallest values: a 40 m mesh's 1/1024 m grid
/// holds every half from 1/16 m up and the rest within half a millimetre,
/// UVs in 0..1 every half from 1/64 up and the rest within 2^-17.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct Quant {
    pub origin: [f32; 5],
    pub step: [i8; 3],
}

impl Quant {
    /// Grids holding `lo..=hi` per axis.
    fn new(lo: [f32; 5], hi: [f32; 5]) -> Self {
        let extent = |a: usize| if hi[a] >= lo[a] { hi[a] - lo[a] } else { 0. };
        let mut out = Self::default();
        for (k, axes) in [&[0usize, 1, 2][..], &[3], &[4]].into_iter().enumerate() {
            let widest = axes.iter().map(|&a| extent(a)).fold(0f32, f32::max);
            // The origins snap down by up to a step: 65534 steps span it.
            let mut e = (widest / 65534.).log2().ceil().clamp(-24., 100.) as i32;
            let origin = |a: usize, e: i32| {
                let lo = if hi[a] >= lo[a] { lo[a] } else { 0. };
                (lo / 2f32.powi(e)).floor() * 2f32.powi(e)
            };
            while axes
                .iter()
                .any(|&a| (hi[a] - origin(a, e)) / 2f32.powi(e) > 65535.)
            {
                e += 1;
            }
            for &a in axes {
                out.origin[a] = origin(a, e);
            }
            out.step[k] = e as i8;
        }
        out
    }

    /// `v` on axis `a`'s grid, in steps.
    fn snap(&self, a: usize, v: f32) -> u32 {
        let e = i32::from(self.step[[0, 0, 0, 1, 2][a]]);
        let q = ((v - self.origin[a]) / 2f32.powi(e)).round();
        if q.is_finite() {
            q.clamp(0., 65535.) as u32
        } else {
            0
        }
    }

    /// The steps of positions, u and v.
    pub fn steps(&self) -> [f32; 3] {
        self.step.map(|e| 2f32.powi(i32::from(e)))
    }

    /// `vertex` on these grids.
    fn pack(&self, vertex: &GpuVertex) -> ArenaVertex {
        let [x, y, z, _] = vertex.position;
        let [u, v] = vertex.uv;
        ArenaVertex {
            xy: self.snap(0, x) | self.snap(1, y) << 16,
            z: self.snap(2, z),
            uv: self.snap(3, u) | self.snap(4, v) << 16,
            color: vertex.color,
        }
    }
}

/// A placed mesh's vertex in the arena (placed.wgsl `ArenaVertex`): x, y,
/// z, u and v as 16-bit steps on the mesh's grids (`Quant`) and the color
/// as `GpuVertex` holds it, 16 bytes the shaders load at once; its
/// octahedral normal sits in an array of its own, which only shading
/// reads. 20 bytes against `GpuVertex`'s 32: the continental island's 4.3M
/// arena vertices took 131 MB. All 20 bytes in one record take five
/// loads, not one and one: the visibility resolve, which reads a dozen
/// vertices a pixel, took 0.03 ms more so on Albion at 5K.
#[repr(C)]
#[derive(Clone, Copy, Default, Pod, Zeroable)]
pub(super) struct ArenaVertex {
    xy: u32,
    z: u32,
    uv: u32,
    color: u32,
}

impl ArenaVertex {
    /// The position in steps of its grid, which x, y and z share.
    pub(super) fn grid(&self) -> Vec3 {
        [self.xy & 0xffff, self.xy >> 16, self.z & 0xffff].map(|q| q as f32)
    }
}

/// Area share of a mesh whose triangles may sample finer than its
/// `uv_density`: trims stretched along a texture strip, faces whose UVs fold
/// to a point. Leaving them out keeps far textures far coarser; their
/// pixels then sample a level coarser than every level would give.
const UV_DENSITY_SHARE: f32 = 0.01;

/// The texture span (UV units) a unit of the mesh's own space covers at
/// least, over all but `UV_DENSITY_SHARE` of its area, along the direction
/// the sampler picks its level by: per triangle the larger of its UV map's
/// smaller stretch and an eighth of its larger (8x anisotropic filtering
/// takes up to eight taps along the larger, `Kit::sampler`). A pixel
/// spanning `m` units then spans at least `density * m * side` texels of a
/// texture `side` texels across. 0 without UVs.
fn uv_density(data: &MeshData) -> f32 {
    if data.uvs.is_empty() {
        return 0.;
    }
    let (p, t) = (&data.positions, &data.uvs);
    // Each triangle's bound and area.
    let mut tris: Vec<(f32, f32)> = Vec::with_capacity(data.indices.len() / 3);
    for tri in data.indices.as_chunks::<3>().0 {
        let [a, b, c] = tri.map(|i| i as usize);
        let (e1, e2) = (math::sub(p[b], p[a]), math::sub(p[c], p[a]));
        let (d1, d2) = (math::sub(t[b], t[a]), math::sub(t[c], t[a]));
        // The UV map in the triangle's plane: e1 along x, e2 at (x2, y2).
        let x1 = math::length(e1);
        let area = math::length(math::cross(e1, e2));
        // Slivers draw no pixels to speak of.
        if x1 < 1e-6 || area < 1e-9 * x1 * x1 {
            continue;
        }
        let x2 = math::dot(e1, e2) / x1;
        let y2 = area / x1;
        // J = D · P⁻¹, P = [[x1, x2], [0, y2]] the edges' plane coordinates.
        let j = [
            [d1[0] / x1, (d2[0] - d1[0] * x2 / x1) / y2],
            [d1[1] / x1, (d2[1] - d1[1] * x2 / x1) / y2],
        ];
        let sum = j[0][0] * j[0][0] + j[0][1] * j[0][1] + j[1][0] * j[1][0] + j[1][1] * j[1][1];
        let det = j[0][0] * j[1][1] - j[0][1] * j[1][0];
        let larger = ((sum + (sum * sum - 4. * det * det).max(0.).sqrt()) * 0.5).sqrt();
        let smaller = if larger > 0. { det.abs() / larger } else { 0. };
        tris.push((smaller.max(larger / 8.), area));
    }
    tris.sort_unstable_by(|a, b| a.0.total_cmp(&b.0));
    let total: f32 = tris.iter().map(|t| t.1).sum();
    let mut covered = 0.;
    for (bound, area) in tris {
        covered += area;
        if covered > UV_DENSITY_SHARE * total {
            return bound;
        }
    }
    0.
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
        if let Some(splat) = &data.splat {
            splat.validate()?;
        }
        match &data.material {
            Material::Blend(blend) => ensure!(
                [blend.opacity, blend.fade, blend.fresnel, blend.depth]
                    .iter()
                    .all(|v| v.is_finite()),
                "Blend material has a non-finite parameter"
            ),
            Material::Decal(decal) => {
                let (a, b) = DecalEdge::code(decal.edge.as_ref());
                ensure!(
                    [decal.opacity, decal.bias]
                        .iter()
                        .chain(&a)
                        .chain(&b)
                        .all(|v| v.is_finite()),
                    "Decal material has a non-finite parameter"
                )
            }
            Material::Lit | Material::Flat => {}
        }
        ensure!(
            data.payload.len() <= MAX_PAYLOAD,
            "Mesh payload of {} words: at most {MAX_PAYLOAD}",
            data.payload.len()
        );
        ensure!(
            data.payload.is_empty() || data.family != FamilyId::LIT,
            "Mesh payload on the kit's own family, which reads none"
        );
        ensure!(
            data.slots.len() <= MAX_SLOTS,
            "Mesh with {} family slots: at most {MAX_SLOTS}",
            data.slots.len()
        );
        ensure!(
            data.slots.is_empty() || data.family != FamilyId::LIT,
            "Mesh family slots on the kit's own family, which samples none"
        );
        for texture in &data.slots {
            texture.validate()?;
        }
        ensure!(
            data.slot_repeats.len() <= data.slots.len()
                && data.slot_repeats.iter().all(|r| r.is_finite() && *r > 0.),
            "Mesh slot repeats must be positive, one per slot at most"
        );
        if let Some(color) = data.emissive {
            ensure!(
                color.iter().all(|c| c.is_finite()),
                "Mesh emissive colour is not finite"
            );
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
            data.skin.is_empty() || data.skin.len() == n,
            "Mesh has {} skin entries for {n} vertices",
            data.skin.len()
        );
        ensure!(
            data.skin.is_empty() || data.thinning.is_none(),
            "Mesh both skins and thins"
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
        let thin_vertices = thin.as_ref().map(|ends: &Vec<u32>| {
            let mut start = 0;
            (ends.iter())
                .map(|&end| {
                    let own = &data.indices[start as usize..end as usize];
                    start = end;
                    match (own.iter().min(), own.iter().max()) {
                        (Some(&lo), Some(&hi)) => [lo, hi + 1],
                        _ => [0, 0],
                    }
                })
                .collect()
        });
        let bounds = math::bounds(data.positions.iter().copied());
        let (mut lo, mut hi) = ([0f32; 5], [0f32; 5]);
        lo[..3].copy_from_slice(&bounds[0]);
        hi[..3].copy_from_slice(&bounds[1]);
        let finite = data.uvs.iter().filter(|t| t.iter().all(|v| v.is_finite()));
        let uvs = math::bounds(finite.map(|t| [t[0], t[1], 0.]));
        lo[3..].copy_from_slice(&uvs[0][..2]);
        hi[3..].copy_from_slice(&uvs[1][..2]);
        Ok(Self {
            bounds,
            counts: Counts {
                vertices: n as u32,
                indices: data.indices.len() as u32,
                normals: !data.normals.is_empty(),
                uvs: !data.uvs.is_empty(),
                thin,
                thin_vertices,
                uv_density: uv_density(&data),
                quant: Quant::new(lo, hi),
                strays: None,
            },
            data,
            packed: None,
            shares: None,
        })
    }

    /// `new`, keeping the geometry compressed: for meshes a host keeps
    /// only to hand the renderer, which decodes them as it packs. An
    /// island's 3,300 model meshes held 266 MB as `MeshData`, 64 MB so.
    /// `data()` then has no positions, UVs, colors, normals or indices;
    /// thinning keeps its parts.
    ///
    /// Triangles and vertices are reordered for the GPU's vertex caches
    /// (`order_for_gpu`).
    pub fn compact(mut data: MeshData) -> Result<Self> {
        order_for_gpu(&mut data);
        Self::compact_ordered(data)
    }

    /// `compact`, and coarser levels simplified to each share of its
    /// triangles in `keeps` (`MeshData::simplified`), up to the first that
    /// does not simplify. The levels keep only their triangles and draw
    /// from this mesh's vertices: separate, simplified levels held 1.7M of
    /// the continental island's 4.3M placement vertices.
    pub fn compact_levels(
        mut data: MeshData,
        keeps: &[f32],
    ) -> Result<(Arc<Self>, Vec<Arc<Self>>)> {
        // Simplified in the mesh's own order, as `simplified` would, then
        // onto the vertices as the full mesh orders them.
        let simplified: Vec<(Vec<u32>, f32)> = (keeps.iter())
            .map_while(|&keep| data.simplified_indices(keep))
            .collect();
        let remap = order_for_gpu(&mut data);
        let mut levels = Vec::new();
        for (mut indices, strays) in simplified {
            if !remap.is_empty() {
                indices.iter_mut().for_each(|i| *i = remap[*i as usize]);
            }
            if let Some(t) = &data.thinning {
                indices = in_rank_order(&indices, &t.parts);
            }
            order_triangles(&data, &mut indices);
            // Counted over the full mesh's vertices, as it draws.
            let mut level = Self::new(MeshData {
                indices,
                ..data.clone()
            })?;
            level.counts.strays = Some(strays);
            levels.push(level);
        }
        let full = Arc::new(Self::compact_ordered(data)?);
        let levels = (levels.into_iter())
            .map(|mut level| {
                let d = &mut level.data;
                let mut indices = meshopt::encode_index_buffer(&d.indices, d.positions.len())?;
                indices.shrink_to_fit();
                (d.positions, d.uvs, d.normals, d.colors, d.indices) =
                    (Vec::new(), Vec::new(), Vec::new(), Vec::new(), Vec::new());
                // The full mesh holds the parts and the skin.
                if let Some(t) = &mut d.thinning {
                    t.parts = Vec::new();
                }
                d.skin = Vec::new();
                level.packed = Some((Vec::new(), indices));
                level.shares = Some(full.clone());
                Ok(Arc::new(level))
            })
            .collect::<Result<_>>()?;
        Ok((full, levels))
    }

    /// `compact` of a mesh already in `order_for_gpu`'s order.
    fn compact_ordered(data: MeshData) -> Result<Self> {
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

    /// A full copy, with geometry decoded back out of a `compact` mesh
    /// (normals as the shaders see them, through octahedral snorm16): for
    /// hosts that build new meshes from loaded ones, such as a model bent
    /// along a curve. Vertex colors come back white where none were set,
    /// UVs and normals only if the mesh had them; triangles may start at
    /// another corner (meshopt's index codec), winding kept.
    pub fn geometry(&self) -> MeshData {
        if let (Some(full), Some((_, i))) = (&self.shares, &self.packed) {
            let k = self.counts.indices as usize;
            return MeshData {
                indices: meshopt::decode_index_buffer(i, k).expect("encoded by compact_levels"),
                ..full.geometry()
            };
        }
        let Some((v, i)) = &self.packed else {
            return self.data.clone();
        };
        let (n, k) = (self.counts.vertices as usize, self.counts.indices as usize);
        let vertices: Vec<GpuVertex> =
            meshopt::decode_vertex_buffer(v, n).expect("encoded by compact");
        let mut data = self.data.clone();
        data.positions = vertices
            .iter()
            .map(|v| [v.position[0], v.position[1], v.position[2]])
            .collect();
        data.colors = vertices.iter().map(|v| v.color).collect();
        if self.counts.uvs {
            data.uvs = vertices.iter().map(|v| v.uv).collect();
        }
        if self.counts.normals {
            data.normals = vertices.iter().map(|v| unoctahedral(v.normal)).collect();
        }
        data.indices = meshopt::decode_index_buffer(i, k).expect("encoded by compact");
        data
    }

    pub fn triangles(&self) -> usize {
        self.counts.indices as usize / 3
    }

    /// For a level from `compact_levels`, how far its surface strays from
    /// the full mesh's at most, in the mesh's own units; `None` for other
    /// meshes.
    pub fn strays(&self) -> Option<f32> {
        self.counts.strays
    }

    pub(crate) fn vertex_count(&self) -> usize {
        self.counts.vertices as usize
    }

    pub(crate) fn index_count(&self) -> usize {
        self.counts.indices as usize
    }

    pub(crate) fn uv_density(&self) -> f32 {
        self.counts.uv_density
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

    /// Per prefix bucket, the vertices its own triangles reach (`Counts`).
    pub(crate) fn thin_vertices(&self) -> Option<&[[u32; 2]]> {
        self.counts.thin_vertices.as_deref()
    }

    /// The mesh whose vertices this level draws from (`compact_levels`).
    pub(crate) fn shares(&self) -> Option<&Arc<Mesh>> {
        self.shares.as_ref()
    }

    /// The vertices as shaders read them, into `out`.
    pub(crate) fn write_vertices(&self, out: &mut [GpuVertex]) {
        if let Some(full) = &self.shares {
            return full.write_vertices(out);
        }
        match &self.packed {
            Some((v, _)) => {
                let decoded: Vec<GpuVertex> =
                    meshopt::decode_vertex_buffer(v, out.len()).expect("encoded by compact");
                out.copy_from_slice(&decoded);
            }
            None => write_vertices(&self.data, out),
        }
    }

    pub(crate) fn quant(&self) -> &Quant {
        &self.counts.quant
    }

    /// The vertices as the placement arena stores them, and their
    /// normals, into `out` and `normals`.
    pub(crate) fn write_arena_vertices(&self, out: &mut [ArenaVertex], normals: &mut [u32]) {
        let mut vertices = vec![GpuVertex::zeroed(); out.len()];
        self.write_vertices(&mut vertices);
        for ((o, n), v) in out.iter_mut().zip(normals).zip(&vertices) {
            *o = self.counts.quant.pack(v);
            *n = v.normal;
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
#[derive(Clone, Debug, PartialEq)]
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
    /// Lights of the scene's own beside the sun, the first
    /// [`MAX_LOCAL_LIGHTS`] of them. None costs nothing.
    pub local_lights: Arc<Vec<LocalLight>>,
    /// Shade in linear light and tone map; `None` keeps display-space shading.
    pub tone: Option<Tone>,
    /// Behind everything; `None` leaves the target as it was.
    pub sky: Option<Sky>,
    pub occlusion: Option<Occlusion>,
    pub ground_tint: Option<GroundTint>,
    /// Shade over the sun's light, as clouds cast it.
    pub cloud_shadows: Option<CloudShadows>,
    /// The emissive light's strength, and the host's own numbers.
    pub glow: Glow,
    /// What surfaces reflect; `None` reflects the sky.
    pub environment: Option<Environment>,
    /// Which of the renderer's family set's view modes the scene is seen
    /// through: 0 the plain lighting, `k` its `Lighting::modes[k - 1]`.
    /// A mode reads the clock (frame WGSL's `lights.view.y`), so a view in
    /// one keeps drawing.
    pub view_mode: u32,
    /// The host lighting's own pictures.
    pub host_texture: Option<HostTexture>,
    /// A mirror view of the scene for passes to reflect (`Reflection`);
    /// `None` draws none, and costs nothing.
    pub reflection: Option<Reflection>,
    /// Drawing of the scene's own, in list order within each stage.
    pub passes: Vec<Arc<dyn Pass>>,
    /// Whether the kit's [`TerrainPass`] draws [`Scene::terrain`].
    pub ground: GroundDraw,
}

/// Who draws a scene's [`Scene::terrain`].
#[derive(Clone, Default)]
pub enum GroundDraw {
    /// The kit's [`TerrainPass`] in the terrain's own material, listed
    /// before [`Scene::passes`]; a ready Opaque pass that replaces still
    /// takes its place. No ground is mounted, so nothing is paged.
    #[default]
    Kit,
    /// Nothing of the kit's: the terrain only supplies the heights and
    /// colors placements drape on and take their tint from, and the
    /// scene's own passes draw the ground.
    Data,
    /// Shade the kit's patches with a device-local ground. The terrain material
    /// stays visible until the replacement has prepared its bindings. The
    /// factory brings what its ground draws from (a host's data rides in
    /// it); the kit keeps one mounted while the factory and terrain stay
    /// the same `Arc`s, so make it once per ground, not per scene.
    Surface(terrain::GroundFactory),
}

impl std::fmt::Debug for GroundDraw {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Kit => "Kit",
            Self::Data => "Data",
            Self::Surface(_) => "Surface",
        })
    }
}

impl PartialEq for GroundDraw {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Kit, Self::Kit) | (Self::Data, Self::Data) => true,
            (Self::Surface(a), Self::Surface(b)) => Arc::ptr_eq(a, b),
            _ => false,
        }
    }
}
impl Eq for GroundDraw {}

/// What the kit's own passes prepare with beyond [`Prepare`]'s public
/// handles: this view's culling and draw-id slots, the kit's device and
/// the prepared scene.
pub(crate) struct KitPrepare<'a> {
    gpu: &'a Gpu,
    device: &'a Device,
    ids: &'a wgpu::Buffer,
    targets: &'a mut Targets,
    slot: &'a mut dyn FnMut([u32; 4]) -> u32,
    scene: &'a PreparedScene,
    frustum: &'a Frustum,
    rows: &'a [[f64; 4]; 4],
    /// The view's height in pixels.
    height: f32,
}

impl<'a> pass::Prepare<'a> {
    /// The encoder, and timestamp writes for one render pass recorded with
    /// it under `label` when the frame is timed: the pass's GPU time then
    /// shows in [`Renderer::stats`] among the kit's.
    pub fn timed(
        &mut self,
        label: &'static str,
    ) -> (
        &mut wgpu::CommandEncoder,
        Option<wgpu::RenderPassTimestampWrites<'_>>,
    ) {
        let writes = (self.kit.as_mut())
            .and_then(|k| k.targets.timing.as_mut())
            .and_then(|t| t.render(label));
        (&mut *self.encoder, writes)
    }

    /// Runs `work` on the encoder between two timestamps under `label`
    /// when the frame is timed, for GPU work outside a render pass (compute
    /// passes, copies): its time then shows among the kit's passes.
    pub fn timed_work<R>(
        &mut self,
        label: &'static str,
        work: impl FnOnce(&mut wgpu::CommandEncoder) -> R,
    ) -> R {
        let timing = (self.kit.as_mut()).and_then(|k| k.targets.timing.as_mut());
        Timing::around(timing, self.device, self.encoder, label, work)
    }
}

/// What the kit's own passes record with beyond [`Record`]'s.
#[derive(Clone, Copy)]
pub(crate) struct KitRecord<'a> {
    gpu: &'a Gpu,
    device: &'a Device,
    ids: &'a wgpu::Buffer,
    scene: &'a PreparedScene,
    /// While drawing a view's shadow cascade: its clip rows and world units
    /// per texel, for the ground to cull and pick its detail by.
    cascade: Option<([[f64; 4]; 4], f32)>,
    /// While a shadow map draws only part of itself again: clip rows
    /// narrowed to that part, which casters outside need not draw into.
    shadow_part: Option<[[f64; 4]; 4]>,
    /// With ground pages to draw the ground in two (TerrainPass): per
    /// terrain tile, a bit when they cover all of it. None when the view
    /// has no pages, or when its depth was restored (`Reuse::Geometry`),
    /// where the second draw could not tell the first's pixels by depth.
    paged: Option<&'a [u64]>,
}

/// A tint over the ground, as a map over world X and Z: texels filtered as
/// stored, then decoded per pixel into a multiplier by the renderer's
/// lighting ([`Lighting::tint`]; the kit's reads RGB as it is), so a ground
/// may store it encoded. Meshes with a terrain tint amount take it in place
/// of the terrain's color map; a ground reads it as `ground_tint(world)` or
/// `ground_tint_texel(world)`.
#[derive(Clone, Debug)]
pub struct GroundTint {
    pub map: Arc<ControlMap>,
    /// World X and Z of the map's corner, and the metres it spans.
    pub origin: [f32; 2],
    pub size: [f32; 2],
}

/// A shade over the sun's light, as moving clouds cast it: the alpha of
/// `map` laid over world X and Z in two scrolling layers, averaged, then
/// shaped into the light left (1 - (1 + (noise - 1) × contrast), raised to
/// `power`, both clamped to 0..1). Where the shadow map also shades, the
/// darker of the two wins.
#[derive(Clone, Debug)]
pub struct CloudShadows {
    pub map: Arc<ControlMap>,
    pub layers: [CloudLayer; 2],
    pub contrast: f32,
    pub power: f32,
}

/// `clock`'s value while held (f32 bits; NaN runs it), for images that
/// must match between runs: gpu_bench's reference frames, whose clouds
/// otherwise drift with wall time.
pub static HELD_CLOCK: std::sync::atomic::AtomicU32 =
    std::sync::atomic::AtomicU32::new(0x7fc0_0000);

/// Seconds on the clock plugged passes and cloud shadows animate by
/// (`Prepare::time`), from its first reading. One for the process, so
/// what is baked off the GPU (a sky's drifting clouds) can match it.
pub fn clock() -> f32 {
    let held = f32::from_bits(HELD_CLOCK.load(std::sync::atomic::Ordering::Relaxed));
    if !held.is_nan() {
        return held;
    }
    static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    START
        .get_or_init(std::time::Instant::now)
        .elapsed()
        .as_secs_f32()
}

/// One layer of [`CloudShadows`]: map coordinates are world X and Z times
/// `scale`, less `offset` and `velocity` times the renderer's clock.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct CloudLayer {
    pub scale: f32,
    pub offset: [f32; 2],
    pub velocity: [f32; 2],
}

impl CloudShadows {
    /// Whether its layers move, so the view keeps drawing.
    fn moving(&self) -> bool {
        self.layers.iter().any(|l| l.velocity != [0.; 2])
    }
}

/// What a renderer leaves out or draws cheaper than its scene asks: an
/// app's graphics settings, apart from the scene so they hold across
/// scenes ([`Renderer::set_detail`]). The default leaves nothing out.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Detail {
    /// Placements whose box spans fewer of the screen's device pixels
    /// than this, across its widest extent at its nearest point, are not
    /// drawn in the views nor picked (0 draws them all). Device pixels, as
    /// a game counts them at the screen's resolution: 13 is 6.5 of a
    /// pane's logical pixels at scale 2, whatever the render scale. Shadow
    /// maps still draw them: their shadows are as small.
    pub min_pixels: f32,
    pub cloud_shadows: CloudMotion,
    /// While nothing but animation keeps a pane drawing (passes on the
    /// clock, drifting cloud shadows), at most this many frames a second;
    /// `None` follows the display. Input, uploads and picks draw at once.
    pub idle_fps: Option<u32>,
    /// How far from a perspective view's eye placements with an impostor
    /// (foliage) draw; `None` draws them at any distance. Orthographic
    /// views draw everything.
    pub reach: Option<Reach>,
    /// Shades a mounted ground live at every pixel instead of from its
    /// baked pages: slower, for checking what pages bake against the
    /// ground itself.
    pub live_ground: bool,
}

/// Distances from a view's eye, in world units, at which placements with
/// an impostor (foliage, `Piece::impostor`) turn to it and then stop
/// drawing, as games drop vegetation by view distance. Other placements
/// are left alone. The view's shadow cascades cut the same way from the
/// same eye, so nothing past the reach casts into them; the scene's one map
/// over everything is drawn once for every eye and keeps all its casters.
///
/// Over the last `thin` share of `end` placements drop out one by one, each
/// at its own distance hashed from where it stands, so the edge reads as
/// the forest thinning rather than a line. It is all decided per placement
/// in the cull: nothing costs per pixel.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Reach {
    /// From here on the impostor draws, however large the placement is on
    /// screen.
    pub impostor: f32,
    /// Past this nothing draws.
    pub end: f32,
    /// The share of `end`, 0 to 1, over which placements drop out.
    pub thin: f32,
}

impl Detail {
    /// What the cull's lists depend on, for keeping them.
    fn cull_key(&self) -> [u32; 4] {
        let r = self.reach.map_or([0.; 3], |r| [r.impostor, r.end, r.thin]);
        [
            self.min_pixels.to_bits(),
            r[0].to_bits(),
            r[1].to_bits(),
            r[2].to_bits(),
        ]
    }
}

/// How [`CloudShadows`] and a pass's [`CloudShade`] move over the ground.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CloudMotion {
    /// With the clock, every frame: a still view shades its ground and
    /// models again each frame and keeps only their geometry.
    #[default]
    Smooth,
    /// In steps of [`CLOUD_STEP`] seconds: between steps a still view
    /// keeps its opaque layers whole.
    Stepped,
    /// None drawn: the ground takes the sun unshaded by clouds.
    Off,
}

/// Seconds [`CloudMotion::Stepped`] cloud shadows hold still between
/// moves. Their edges are soft: on gpu_bench's views a whole second of
/// drift moved pixels by 0.03 to 0.15 levels of 255 on average, so a
/// quarter second's step does not show (branch gpu-still-cloudstep).
pub const CLOUD_STEP: f32 = 0.25;

impl CloudMotion {
    /// The clock cloud shadows take at `time`; `None` draws none.
    pub fn time(self, time: f32) -> Option<f32> {
        match self {
            Self::Smooth => Some(time),
            Self::Stepped => Some((time / CLOUD_STEP).floor() * CLOUD_STEP),
            Self::Off => None,
        }
    }
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
/// outline.wgsl's header words: placed words, color, width, unused, the
/// scene's corner and scale in the target (Globals::display), unused.
const MARKS_HEADER: usize = 8;

/// outline.wgsl's TILE: the side of the `ids` square one tile texel covers.
const OUTLINE_TILE: u32 = 8;
const OUTLINE_TILES: wgpu::TextureFormat = wgpu::TextureFormat::R8Uint;

fn outline_marks(outline: &Outline, pick_ids: &[Option<u64>], display: [f32; 3]) -> Vec<u32> {
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
    let mut marks = vec![0u32; MARKS_HEADER + placed + handles];
    marks[0] = placed as u32;
    marks[1] = outline.color;
    marks[2] = outline.width.max(1);
    marks[4..7].copy_from_slice(&display.map(f32::to_bits));
    for &row in &outline.rows {
        marks[MARKS_HEADER + row as usize / 32] |= 1 << (row % 32);
    }
    if !ids.is_empty() {
        for (i, id) in pick_ids.iter().enumerate() {
            if id.is_some_and(|id| ids.binary_search(&id).is_ok()) {
                marks[MARKS_HEADER + placed + i / 32] |= 1 << (i % 32);
            }
        }
    }
    marks
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
#[derive(Clone, Copy, Pod, Zeroable, PartialEq)]
struct Globals {
    clip: [[f32; 4]; 4],
    /// Inverse of `clip`, for the sky's view rays.
    unclip: [[f32; 4]; 4],
    viewport: [f32; 4],
    eye: [f32; 4],
    anchor: [f32; 4],
    /// The scene's corner in the target (pixels) and its size over the
    /// pane's (`Renderer::set_render_scale`): the post stage reads the
    /// pane's pixel `p` at `xy + (p - xy) * z`.
    display: [f32; 4],
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable, PartialEq)]
struct Lights {
    shadow: [[f32; 4]; 4],
    sun: [f32; 4],
    sun_color: [f32; 4],
    sky: [f32; 4],
    ground: [f32; 4],
    fog: [f32; 4],
    fog_range: [f32; 4],
    /// Exposure (0: display-space shading), sun occlusion, diffuse ambient
    /// factor.
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
    /// The presented terrain's grid for terrain-adapting placements
    /// (`t_ground_heights`): origin x and z, spacing, 1 when there is one.
    terrain: [f32; 4],
    /// Diffuse sky light per channel (`light::SkyMap::irradiance`): the
    /// sky map's, or the hemisphere of `sky` and `ground`.
    ambient: [[[f32; 4]; 4]; 3],
    /// The sun disc's radiance; w = 1 with a sky map in `t_sky`.
    disc: [f32; 4],
    /// `GroundTint` in `t_tint`: its corner's world X and Z and 1 / its
    /// size; zero without one.
    ground_tint: [f32; 4],
    /// `Tone::curve`: 0 ACES and its lift edge, lift power and scale, or 1
    /// shoulder and its max linear, white and gamma.
    curve: [f32; 4],
    /// [`CloudShadows`]: the layers' scales, contrast, power (0: none).
    clouds: [f32; 4],
    /// Each layer's offset at this frame's clock.
    cloud_offsets: [f32; 4],
    /// The view's shadow cascades (`Shadows::cascades`), world to their
    /// clip, nearest first.
    cascades: [[[f32; 4]; 4]; light::MAX_CASCADES],
    /// Per cascade: world units per texel, depth bias in its depth units,
    /// filter radius in its UV units.
    cascade: [[f32; 4]; light::MAX_CASCADES],
    /// x: cascades in use.
    cascade_info: [f32; 4],
    /// `Occlusion`'s small and large scale weights and its pixel cap and
    /// floor.
    occlusion_scale: [f32; 4],
    /// Per cascade: the square the view wants of it in its drawn map's
    /// clip, centre x, y and half side.
    cascade_want: [[f32; 4]; light::MAX_CASCADES],
    /// Baked ground pages (`terrain::GroundPages`): the quadtree's corner
    /// world X and Z, metres its finest page spans, and the least normal Y
    /// that takes them when the view has them (0 when not).
    pages: [f32; 4],
    /// A pass's [`CloudShade`] in `t_cloud_shade`: its corner's world X
    /// and Z, 1 / its size, 1 when there is one.
    cloud_shade: [f32; 4],
    /// w: [`Glow::strength`]; xyz spare.
    glow: [f32; 4],
    /// [`Glow::host`].
    host: [[f32; 4]; HOST_WORDS / 4],
    /// w: 1 with an [`Environment`] in `t_environment`.
    environment: [f32; 4],
    /// x: [`Scene::view_mode`]; y: the renderer's clock in a view mode, 0
    /// in the plain one; w: 1 with a [`HostTexture`] in `t_host`.
    view: [f32; 4],
    /// x: the scene's own lights in use ([`Scene::local_lights`]).
    local: [f32; 4],
    /// Three words per local light (`LocalLight::words`).
    locals: [[[f32; 4]; 3]; MAX_LOCAL_LIGHTS],
    /// x: the scene's height in target pixels, which a mirror's rows flip
    /// about; y: 1 with a mirror in `t_mirror`; z: its level; w: its
    /// pixels per target pixel (`MIRROR_SCALE`).
    reflection: [f32; 4],
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable, Default)]
struct Params {
    surface: [f32; 4],
    flags: [f32; 4],
    blend: [f32; 4],
    tone: [f32; 4],
    terrain: [f32; 4],
    /// x: terrain tint amount; y: terrain adaption (`adapt_code`); z: a
    /// decal's bias toward the viewer.
    extra: [f32; 4],
    /// A projected decal's edge (`DecalEdge::code`).
    edge: [f32; 4],
    edge2: [f32; 4],
    /// `MeshData::emissive`, w 1 when the normal map's blue glows.
    glow: [f32; 4],
    /// x: the reference albedo alpha is cut at (`MeshData::alpha_ref`).
    alpha: [f32; 4],
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
    /// Relief: displacement floor, ceil, and 1 when it adds (Relief::ranges).
    floor: f32,
    ceil: f32,
    add: f32,
    _pad: f32,
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
    data.casts_shadow && matches!(data.material, Material::Lit)
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
    local_lights: Arc<Vec<LocalLight>>,
    tone: Option<Tone>,
    /// `Lights::ambient`.
    ambient: [[[f32; 4]; 4]; 3],
    sky: Option<Sky>,
    occlusion: Option<Occlusion>,
    ground_tint: Option<GroundTint>,
    cloud_shadows: Option<CloudShadows>,
    glow: Glow,
    environment: Option<Environment>,
    view_mode: u32,
    host_texture: Option<HostTexture>,
    reflection: Option<Reflection>,
    /// The kit's terrain pass when the scene lists it, as `passes[0]`.
    ground: Option<Arc<TerrainPass>>,
    ground_draw: GroundDraw,
    passes: Vec<Arc<dyn Pass>>,
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
/// map. All state sits behind one lock, which a pane's thread holds while
/// it records: public calls take it once and nothing inside takes it again.
/// The host's calls never wait for a frame. Writes queue (`Inputs`) and the
/// next frame applies them before it records; reads take the state only
/// while it is free, else what the last frame answered (`Answers`).
#[derive(Default)]
pub struct Renderer {
    state: Mutex<State>,
    /// Live [`ShadowHold`]s: outside the lock, so a hold can drop anywhere.
    shadow_holds: Arc<std::sync::atomic::AtomicU32>,
    /// The viewports' panes, for `changed`. Outside the lock, which their
    /// threads hold while they record.
    panes: Mutex<Vec<std::sync::Weak<Pane>>>,
    /// Nanoseconds other threads (the UI's) waited for the lock while a
    /// pane's thread held it (`take_blocked`).
    blocked: std::sync::atomic::AtomicU64,
    host: Mutex<Host>,
    /// Slots handed out, to panes and their mirrors: a new pane takes its
    /// own without the state's lock, and `State::slots` grows to it.
    slots: std::sync::atomic::AtomicUsize,
    /// The family set, fixed at creation (`with_families`).
    families: Arc<Families>,
}

/// What the host and the panes' threads pass each other without the
/// state's lock, which a frame holds while it records. Lock order: the
/// state, then this.
#[derive(Default)]
struct Host {
    inputs: Inputs,
    answers: Answers,
}

/// What the host set and the state has yet to take, kept apart from
/// `State` so a write never waits for a pane's thread to finish
/// recording: the host's latest values and which of them are new
/// (`unread`), and the placement writes in the order made. Whoever locks
/// the state next takes them (`Renderer::lock`), which is the next frame's
/// first step.
#[derive(Default)]
struct Inputs {
    render_scale: Option<f32>,
    detail: Detail,
    overlay: Vec<Line>,
    outline: Option<Option<Outline>>,
    frame_mark: Option<std::time::Instant>,
    built_mark: Option<std::time::Instant>,
    /// The latest scene set, kept after the state takes it: the next
    /// `set_scene` numbers itself and keeps terrain patches from it.
    scene: Option<Arc<PreparedScene>>,
    /// `INPUT_*` bits.
    unread: u8,
    /// Placement writes in the order made (`queue_placed`).
    placed: Vec<PlacedWrite>,
    /// The pieces and rows the placement table holds once `placed` is
    /// applied. Writes are checked against them as the host makes them,
    /// so its mistake panics in its own call and not on a pane's thread,
    /// where a panic reads as a lost device.
    pieces: u32,
    rows: u32,
    /// Picks asked while a frame held the state: slot and cursor.
    picks: Vec<(usize, Point<Pixels>)>,
}

const INPUT_SCALE: u8 = 1;
const INPUT_DETAIL: u8 = 2;
const INPUT_OVERLAY: u8 = 4;
const INPUT_MARKS: u8 = 8;
const INPUT_SCENE: u8 = 16;

/// Placement writes queued before the host's next one applies them itself,
/// waiting for the frame. Frames take every write each time, and the
/// common runs fold (`queue_placed`): a drag's moves into one, a load's
/// tables into the latest. A drop queues one per run of moved rows.
const QUEUED_WRITES: usize = 1024;

/// A placement write as the host made it (`Renderer::set_pieces` and the
/// rest), queued for the next frame.
enum PlacedWrite {
    Pieces(Vec<Piece>),
    /// The whole table, with its bounds when the host worked them out.
    Rows(Vec<u32>, Vec<Mat4>, Option<[Vec3; 2]>),
    Moves(u32, Vec<Mat4>),
    Words(u32, Vec<u32>),
    Selection {
        rows: Vec<u32>,
        marks: Vec<[f32; 4]>,
        color: [f32; 4],
        radius: f32,
    },
    Drag(Option<Mat4>),
}

impl PlacedWrite {
    fn apply(self, placed: &mut placed::Placed) {
        match self {
            Self::Pieces(pieces) => placed.set_pieces(pieces),
            Self::Rows(piece_of, transforms, Some(bounds)) => {
                placed.set_rows(piece_of, transforms, bounds)
            }
            Self::Rows(piece_of, transforms, None) => placed.set_placements(&piece_of, &transforms),
            Self::Moves(first, transforms) => placed.move_placements(first, &transforms),
            Self::Words(first, words) => placed.set_words(first, &words),
            Self::Selection {
                rows,
                marks,
                color,
                radius,
            } => placed.set_selection(rows, marks, color, radius),
            Self::Drag(delta) => placed.drag_selection(delta),
        }
    }

    /// Rows in the table this write leaves (`Pieces` empties it).
    fn count(&self) -> Option<usize> {
        match self {
            Self::Pieces(_) => Some(0),
            Self::Rows(piece_of, ..) => Some(piece_of.len()),
            _ => None,
        }
    }
}

impl Inputs {
    /// Scene or placement writes the state has yet to take: what a frame
    /// would still upload.
    fn queued(&self) -> bool {
        self.unread & INPUT_SCENE != 0 || !self.placed.is_empty()
    }

    /// Queues `write` after the writes not yet taken, dropping those it
    /// replaces where that leaves the table as applying them all would:
    ///
    /// - a new piece list or table, the tables and row writes before it,
    ///   unless a selection or drag queued among them reads the table. A
    ///   table drops an earlier one only if no smaller: a table clears a
    ///   selection reaching past it, so a smaller one between must stay.
    /// - a selection, one right before it; a drag, one right before it
    ///   that also starts, moves or ends (`Placed::drag_selection`).
    ///
    /// True when the queue is past `QUEUED_WRITES`.
    fn queue_placed(&mut self, write: PlacedWrite) -> bool {
        use PlacedWrite as W;
        let reads = (self.placed.iter()).any(|w| matches!(w, W::Selection { .. } | W::Drag(_)));
        match &write {
            W::Pieces(_) | W::Rows(..) if !reads => {
                let pieces = matches!(write, W::Pieces(_));
                let count = write.count().unwrap_or(0);
                self.placed.retain(|w| match w {
                    W::Pieces(_) => !pieces,
                    W::Rows(..) => !pieces && w.count().is_some_and(|c| c < count),
                    _ => false,
                });
            }
            W::Selection { .. } if matches!(self.placed.last(), Some(W::Selection { .. })) => {
                self.placed.pop();
            }
            W::Drag(d) if matches!(self.placed.last(), Some(W::Drag(e)) if e.is_some() == d.is_some()) =>
            {
                self.placed.pop();
            }
            _ => {}
        }
        self.placed.push(write);
        self.placed.len() > QUEUED_WRITES
    }

    /// Checks `write` against the table the queue leaves, as applying it
    /// would (placed.rs), and notes the table it leaves in turn.
    fn note(&mut self, write: &PlacedWrite) {
        let rows = self.rows as usize;
        match write {
            PlacedWrite::Pieces(pieces) => {
                self.pieces = pieces.len() as u32;
                self.rows = 0;
            }
            PlacedWrite::Rows(piece_of, transforms, _) => {
                assert_eq!(piece_of.len(), transforms.len());
                assert!(
                    piece_of.iter().all(|&p| p < self.pieces),
                    "rows name pieces past the piece list"
                );
                self.rows = piece_of.len() as u32;
            }
            PlacedWrite::Moves(first, n) => {
                assert!(
                    *first as usize + n.len() <= rows,
                    "rows past the placement table"
                )
            }
            PlacedWrite::Words(first, n) => {
                assert!(
                    *first as usize + n.len() <= rows,
                    "rows past the placement table"
                )
            }
            PlacedWrite::Selection { rows: selected, .. } => assert!(
                selected.iter().all(|&r| (r as usize) < rows),
                "selected rows past the table"
            ),
            PlacedWrite::Drag(_) => {}
        }
    }

    /// Everything the state has yet to take, leaving the latest values
    /// for the host to compare against.
    fn take(&mut self) -> Taken {
        let unread = std::mem::take(&mut self.unread);
        let new = |bit: u8| unread & bit != 0;
        Taken {
            render_scale: new(INPUT_SCALE).then_some(self.render_scale),
            detail: new(INPUT_DETAIL).then_some(self.detail),
            overlay: new(INPUT_OVERLAY).then(|| self.overlay.clone()),
            marks: new(INPUT_MARKS).then_some((self.frame_mark, self.built_mark)),
            outline: self.outline.take(),
            scene: new(INPUT_SCENE).then(|| self.scene.clone()).flatten(),
            placed: std::mem::take(&mut self.placed),
            picks: std::mem::take(&mut self.picks),
        }
    }
}

/// What `Renderer::lock` took from `Inputs`, applied to the state after
/// the host's lock is let go: a host's write never waits on a frame
/// applying the last ones.
struct Taken {
    render_scale: Option<Option<f32>>,
    detail: Option<Detail>,
    overlay: Option<Vec<Line>>,
    marks: Option<(Option<std::time::Instant>, Option<std::time::Instant>)>,
    outline: Option<Option<Outline>>,
    scene: Option<Arc<PreparedScene>>,
    placed: Vec<PlacedWrite>,
    picks: Vec<(usize, Point<Pixels>)>,
}

impl Taken {
    fn apply(self, state: &mut State) {
        if let Some(scale) = self.render_scale {
            state.render_scale = scale;
        }
        if let Some(detail) = self.detail {
            state.detail = detail;
        }
        if let Some(overlay) = self.overlay {
            state.overlay = overlay;
            state.overlay_version += 1;
        }
        if let Some((frame, built)) = self.marks {
            state.frame_mark = frame;
            state.built_mark = built;
        }
        if let Some(outline) = self.outline {
            state.outline = outline;
            state.outline_version += 1;
        }
        if let Some(scene) = self.scene {
            state.scene = Some(scene);
        }
        // Rows refer to pieces and the selection to rows: in order.
        for write in self.placed {
            write.apply(&mut state.assets.placed);
        }
        // As `Renderer::pick` asks: a cursor off the view is never read
        // back, and its frames would go on asking.
        for (slot, cursor) in self.picks {
            if let Some(s) = state.slots.get_mut(slot)
                && s.view.is_some_and(|v| v.bounds.contains(&cursor))
            {
                s.pick_request = Some(cursor);
            }
        }
    }
}

/// What the host reads while a pane's thread holds the state, as the last
/// frame left it (`Renderer::answer`).
#[derive(Default)]
struct Answers {
    stats: Stats,
    /// `State::pending`, and set as soon as a frame takes scene or
    /// placement writes, until a frame answers after them.
    pending: bool,
    settled: bool,
    waiting: Vec<&'static str>,
    /// Per slot: what its last frame drew, and its pick (`State::hit`).
    drawn: Vec<Drawn>,
    picks: Vec<Option<(Point<Pixels>, Option<Hit>)>>,
}

std::thread_local! {
    /// This thread is a pane's (`Renderer::produce`): its own waits for
    /// the lock are not the UI's.
    static PRODUCING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Locks state a frame holds while it calls wgpu, as a panic left it. wgpu
/// panics on a lost device (`create_buffer_init` maps a buffer that is
/// gone), which unwinds the frame (`Renderer::produce`) with such a lock
/// held. A plain `lock().unwrap()` then failed every later frame and the
/// host's next setter on the poison, and the view never drew again on the
/// new device. What the state keeps per device is made again there
/// (`OnDevice`), and the rest a frame reads or replaces whole.
pub trait LockAny<T> {
    fn lock_any(&self) -> std::sync::MutexGuard<'_, T>;
}

impl<T> LockAny<T> for Mutex<T> {
    fn lock_any(&self) -> std::sync::MutexGuard<'_, T> {
        self.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// Marks this thread as one that records views, as a pane's thread does:
/// its waits for the renderer's lock stop counting as the UI's
/// ([`Renderer::take_blocked`]). For benchmarks that record offscreen
/// frames on a thread of their own, as a window does.
#[doc(hidden)]
pub fn mark_view_thread() {
    PRODUCING.set(true);
}

/// While alive, ground edits leave the shadow maps as they are
/// ([`Renderer::hold_shadows`]); dropping it draws them again where the
/// ground moved meanwhile.
pub struct ShadowHold(Arc<std::sync::atomic::AtomicU32>);

impl Drop for ShadowHold {
    fn drop(&mut self) {
        self.0.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
    }
}

/// Frees the renderer's GPU memory on a thread of its own: thousands of
/// textures and buffers, 23 to 62 ms of the UI thread as an island closed
/// (wherever the last reference went, often a frame's canvas). wgpu frees
/// from any thread. Once the process is exiting the GPU must not be
/// touched (`busy`): the state leaks instead.
impl Drop for Renderer {
    fn drop(&mut self) {
        // Writes no frame took go too: the latest scene and pieces.
        let state = (
            std::mem::take(self.state.get_mut().unwrap_or_else(|e| e.into_inner())),
            std::mem::take(self.host.get_mut().unwrap_or_else(|e| e.into_inner())),
        );
        let Some(busy) = busy() else {
            std::mem::forget(state);
            return;
        };
        *FREEING.0.lock().unwrap_or_else(|e| e.into_inner()) += 1;
        let free = move || {
            drop(state);
            drop(busy);
            let mut freeing = FREEING.0.lock().unwrap_or_else(|e| e.into_inner());
            *freeing -= 1;
            FREEING.1.notify_all();
        };
        let spawned = std::thread::Builder::new()
            .name("kit renderer free".into())
            .spawn(free);
        // Without a thread the closure, and the state with it, went here.
        if let Ok(thread) = spawned {
            join_at_exit(thread);
        }
    }
}

/// Renderers still freeing on their threads.
static FREEING: (Mutex<usize>, std::sync::Condvar) = (Mutex::new(0), std::sync::Condvar::new());

/// Waits until every renderer dropped so far has freed its GPU memory: for
/// tests and benchmarks that measure the device after closing one.
pub fn wait_for_freed() {
    let mut freeing = FREEING.0.lock().unwrap_or_else(|e| e.into_inner());
    while *freeing > 0 {
        freeing = FREEING.1.wait(freeing).unwrap_or_else(|e| e.into_inner());
    }
}

#[derive(Default)]
struct State {
    stats: Stats,
    /// The scene's pixels per pane pixel, at most 1 (`set_render_scale`).
    render_scale: Option<f32>,
    detail: Detail,
    /// The overlay lines, and how many times they were set: the device
    /// uploads them as they change, and a new device again.
    overlay: Vec<Line>,
    overlay_version: u64,
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
    /// Swapchain wait before this frame's commands. Subtracted from layout.
    surface_wait: std::time::Duration,
    /// When `stats`' VRAM figures were last read (`note_vram`).
    vram_read: Option<std::time::Instant>,
    frame_began: Option<std::time::Instant>,
    scene: Option<Arc<PreparedScene>>,
    assets: Assets,
    /// Created by the first draw: pipelines need the target format.
    device: Option<Device>,
    /// A window's device while a thread builds it (`Renderer::device_ready`).
    building: Option<(wgpu::TextureFormat, std::sync::mpsc::Receiver<Device>)>,
    /// The wgpu device every GPU copy here was made on (`move_to`).
    on: Option<wgpu::Device>,
    /// The copies were dropped for a new device: nothing warmed it for
    /// this renderer's passes, so the device's build warms them too.
    moved: bool,
    slots: Vec<Slot>,
    /// A scene has been drawn with nothing left to upload: frames since
    /// keep to the interactive budget (`OPENING_BYTES`).
    opened: bool,
}

/// One viewport's camera, targets and readbacks.
#[derive(Default)]
struct Slot {
    view: Option<View>,
    targets: Option<Targets>,
    pick_request: Option<Point<Pixels>>,
    picked: Option<Picked>,
    drawn: Drawn,
    times: SlotTimes,
    /// What this view's culls last asked of the arena's textures.
    wants: residency::Wants,
    /// This view's mirror (`Reflection`): the slot that draws it.
    mirror: Option<usize>,
    /// A mirror's slot: opaque surfaces only, never shown or picked.
    hidden: bool,
}

/// One viewport's GPU times, smoothed per readback. [`Renderer::stats`]
/// sums the viewports drawn lately; one shared average flipped between
/// the views' different pass sets with every readback.
struct SlotTimes {
    /// By name, in first-seen order. A name stays once seen, so rows do not
    /// come and go; a pass that sits frames out decays toward zero.
    avg: [(&'static str, f32); TIMED_PASSES as usize],
    total: f32,
    span: f32,
    /// The last readback: a view not drawn for a while stops counting.
    at: Option<std::time::Instant>,
}

// By hand: std derives Default for arrays of at most 32.
impl Default for SlotTimes {
    fn default() -> Self {
        Self {
            avg: [("", 0.); TIMED_PASSES as usize],
            total: 0.,
            span: 0.,
            at: None,
        }
    }
}

impl SlotTimes {
    fn add(&mut self, passes: &[(&'static str, f32)], span: f32) {
        let sum =
            |name: &str| -> f32 { (passes.iter()).filter(|p| p.0 == name).map(|p| p.1).sum() };
        for (name, ms) in self.avg.iter_mut().filter(|(n, _)| !n.is_empty()) {
            *ms = Stats::blend(*ms, sum(name));
        }
        for &(name, _) in passes.iter().filter(|(p, _)| !p.is_empty()) {
            if !self.avg.iter().any(|(a, _)| *a == name)
                && let Some(free) = self.avg.iter_mut().find(|(a, _)| a.is_empty())
            {
                *free = (name, sum(name));
            }
        }
        self.total = Stats::blend(self.total, passes.iter().map(|p| p.1).sum());
        self.span = Stats::blend(self.span, span);
        self.at = Some(std::time::Instant::now());
    }
}

/// A pane drawing its [`Renderer`]'s scene with its own camera. Keep one
/// per pane for as long as the pane exists. Its targets live in the
/// renderer's slot for it and go with the renderer.
///
/// The pane records and submits on a thread of its own, into a texture
/// GPUI draws in the same frame (`gpui::TextureHandoff`): each window
/// frame asks for a frame as it starts, and the thread records it while
/// the window lays out. The UI thread only waits where the thread is still
/// recording when GPUI is about to draw the texture, and not past
/// `HANDOFF_DEADLINE`.
pub struct Viewport {
    renderer: Arc<Renderer>,
    slot: usize,
    pane: Arc<Pane>,
    thread: Mutex<Option<std::thread::JoinHandle<()>>>,
    /// A present-only follow-up frame is already queued. Animation and
    /// uploads need another draw, not another layout of the window.
    armed: std::sync::atomic::AtomicBool,
    /// Frames GPUI had drawn the texture in when the present chain last
    /// asked for one: no new draw since means the pane left the window
    /// (`arm_present`).
    drawn_at_present: std::sync::atomic::AtomicU64,
}

/// How long after a frame asks for the 3D view GPUI waits for it at most;
/// then it draws the last complete one, and asks for another frame to show
/// the late one. Moving frames record in 0.4 to 3 ms (pirate and
/// continental, 1440p and 5K, 2026-11-20) and are submitted before GPUI
/// gets to the texture; an island's opening frames record in up to 40 to
/// 85 ms, which this keeps off the UI thread. 10 ms leaves a 60 Hz frame
/// room for GPUI's own work.
const HANDOFF_DEADLINE: std::time::Duration = std::time::Duration::from_millis(10);

/// What a pane's UI side and its thread share. The thread holds the
/// renderer's lock while it records, so the UI's per-frame calls (paint,
/// a camera move, the present chain) go through here instead.
struct Pane {
    handoff: Arc<gpui::TextureHandoff>,
    shared: Mutex<Shared>,
    /// The device GPUI handed the thread can't draw 3D (`supported`), and
    /// whether the window was redrawn once for its host to say so.
    unsupported: std::sync::atomic::AtomicBool,
    told: std::sync::atomic::AtomicBool,
}

#[derive(Default)]
struct Shared {
    /// What the pane shows, and its bounds in device pixels.
    view: Option<(View, Bounds<ScaledPixels>)>,
    /// The view the thread took for the newest frame it started, and that
    /// frame's number: a paint that changes it after asks again.
    taken: Option<(View, u64)>,
    /// When the thread took the view for its last few frames.
    started: [(u64, Option<std::time::Instant>); 4],
    /// The newest frame the thread answered, and what the renderer wanted
    /// after it.
    answered: u64,
    wants: Wants,
    clock: Clock,
}

impl Shared {
    /// Times a window frame that drew the pane (`Clock::frame`). Reports
    /// are taken as the next frame starts or the camera next moves,
    /// whichever comes first, so a move is never weighed against a frame
    /// drawn before it.
    fn note(&mut self, shown: Option<Composite>) {
        let Some(shown) = shown else { return };
        let recorded = (self.started.iter())
            .find(|(f, _)| *f == shown.drew && *f > 0)
            .and_then(|(_, at)| *at);
        self.clock.frame(shown.index, recorded, shown);
    }
}

/// What a pane's renderer wants after its last frame, for the present
/// chain: read once a frame by the UI thread without the renderer's lock.
#[derive(Clone, Copy, Default)]
struct Wants {
    /// Uploads, a pick or animation: the frames after should draw too.
    more: bool,
    /// `more`, or texture levels waiting out their hold.
    frame: bool,
    /// Animation (or held levels) alone, at `Detail::idle_fps`: the rate
    /// and when the renderer's last frame began (`Viewport::idle_wait`).
    idle: Option<(u32, std::time::Instant)>,
}

/// What a pane's window frames cost the UI thread and how soon camera
/// moves reached the screen, for benchmarks (`Viewport::take_timing`).
#[derive(Clone, Debug, Default)]
pub struct PaneTiming {
    /// Per window frame that drew the pane: milliseconds its UI thread
    /// spent on the 3D view (waiting for the pane's thread).
    pub ui_ms: Vec<f32>,
    /// Camera moves by the window frame that showed them: the first frame
    /// after the move, one later, two later, three or more later.
    pub moves: [u32; 4],
    /// Moves a later one replaced before any frame showed them.
    pub overtaken: u32,
    /// Per shown move: milliseconds from the move to the end of the UI
    /// thread's wait on the frame that showed it.
    pub move_ms: Vec<f32>,
    /// Per window frame: milliseconds the swapchain held the UI thread
    /// before the frame could draw (display pacing, not work).
    pub surface_ms: Vec<f32>,
    /// Window frames that drew an older 3D frame than they asked for, the
    /// pane's thread having missed `HANDOFF_DEADLINE`.
    pub late: u32,
}

/// `PaneTiming` as it gathers, and the latest camera move not yet shown:
/// when it came and how many window frames had drawn the pane by then.
#[derive(Default)]
struct Clock {
    timing: PaneTiming,
    moved: Option<(std::time::Instant, u64)>,
}

// Benchmarks take the samples about once a second; this bounds a pane no
// one takes them from.
const TIMING_SAMPLES: usize = 4096;

impl Clock {
    fn camera(&mut self, from: Option<Camera>, to: Camera, frames: u64) {
        if from.is_some_and(|c| c != to) {
            let overtaken = self.moved.replace((std::time::Instant::now(), frames));
            self.timing.overtaken += u32::from(overtaken.is_some());
        }
    }

    /// Window frame `index` (counted from 1) drew the pane at `shown`,
    /// with the camera as it was at `recorded` (moves after it are not in
    /// it; `None`: no 3D frame yet). Its UI thread spent `ui` on the pane.
    fn frame(&mut self, index: u64, recorded: Option<std::time::Instant>, shown: Composite) {
        let t = &mut self.timing;
        if t.ui_ms.len() < TIMING_SAMPLES {
            t.ui_ms.push(shown.waited.as_secs_f32() * 1000.);
            t.surface_ms.push(shown.surface_wait.as_secs_f32() * 1000.);
        }
        t.late += u32::from(shown.drew < shown.asked);
        if let Some((at, frames)) = self.moved.filter(|m| recorded.is_some_and(|r| m.0 <= r)) {
            self.moved = None;
            let later = index.saturating_sub(frames + 1).min(3) as usize;
            t.moves[later] += 1;
            if t.move_ms.len() < TIMING_SAMPLES {
                t.move_ms
                    .push(shown.at.saturating_duration_since(at).as_secs_f32() * 1000.);
            }
        }
    }
}

type Composite = gpui::Composite;

/// The texture a pane's thread draws into, the pane's size rounded up to
/// 64 pixels (as the renderer's targets are) so dragging a dock edge does
/// not make a new one every frame.
struct Output {
    view: wgpu::TextureView,
    size: [u32; 2],
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
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Stats {
    /// Between consecutive frames: what the screen shows.
    pub frame_ms: f32,
    /// CPU time from the host's `mark_frame` to the end of the last
    /// viewport's draw: the sum of the next three.
    pub cpu_ms: f32,
    /// Building the window's elements (`mark_frame` to `mark_built`).
    pub build_ms: f32,
    /// From `mark_built` to the viewports' recording. Their threads start
    /// as the window frame does, before it builds, so this is usually zero:
    /// layout and paint run beside the recording (`Viewport`).
    pub layout_ms: f32,
    /// Recording the viewports, on their threads.
    pub encode_ms: f32,
    /// GPU time of this renderer's passes over every view drawn lately,
    /// from timestamp queries (zero where the device has none).
    pub gpu_ms: f32,
    /// The last read view's timed passes by name, unsmoothed; unused slots
    /// have an empty name.
    pub passes: [(&'static str, f32); TIMED_PASSES as usize],
    /// Every view's passes summed by name (the shadow cascades share one)
    /// and smoothed like `gpu_ms`, in a fixed order, for display. A pass
    /// that sits frames out (a shadow redraw) decays toward zero instead of
    /// flickering in and out.
    pub pass_avg: [(&'static str, f32); TIMED_PASSES as usize],
    /// `gpu_span_ms` smoothed and summed like `pass_avg`.
    pub span_avg_ms: f32,
    /// The view's mirror (`Reflection`): its last drawn frame's passes
    /// summed, read a frame late, and zero while a still view keeps it.
    /// Its passes stay out of `passes`.
    pub mirror_ms: f32,
    /// The last frame from its first timed pass's start to its last one's
    /// end: `passes` plus the gaps and untimed work between them.
    pub gpu_span_ms: f32,
    /// The window device's memory in bytes, every process-side allocation
    /// on it (GPUI's included): what is in use, and what the allocator has
    /// reserved from the driver for it. Zero where the backend has no
    /// report (`wgpu::Device::generate_allocator_report`).
    pub vram_used: u64,
    pub vram_reserved: u64,
}

impl Default for Stats {
    fn default() -> Self {
        Self {
            frame_ms: 0.,
            cpu_ms: 0.,
            build_ms: 0.,
            layout_ms: 0.,
            encode_ms: 0.,
            gpu_ms: 0.,
            passes: [("", 0.); TIMED_PASSES as usize],
            pass_avg: [("", 0.); TIMED_PASSES as usize],
            span_avg_ms: 0.,
            mirror_ms: 0.,
            gpu_span_ms: 0.,
            vram_used: 0,
            vram_reserved: 0,
        }
    }
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
    /// Whether the frame drew the shadow map again, and how many of the
    /// view's cascades it drew.
    pub shadow: bool,
    pub cascades: usize,
    /// Ground pages the view picked, how many were baked, and how many it
    /// baked this frame.
    pub pages: PageCounts,
    /// How much of its opaque layers the frame kept from the one before.
    pub reuse: Reuse,
    /// Indirect draws the arena's multi-draws recorded, by pass: shadow
    /// maps, opaque (flat meshes and impostors), visibility buffer, decals.
    /// Each counts every draw in its ranges, whether the cull listed
    /// anything in it or not.
    pub arena_draws: [usize; 4],
}

/// Which pass a multi-draw of the arena counts in (`Drawn::arena_draws`).
#[derive(Clone, Copy)]
enum ArenaPass {
    Shadow,
    Opaque,
    Visibility,
    Decals,
}

/// What a frame kept of its view's opaque layers ([`Drawn::reuse`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Reuse {
    /// Drew them: shadow, opaque and occlusion stages as usual.
    #[default]
    Nothing,
    /// Kept depth, pick handles, the placements' culling and the occlusion
    /// estimate, and shaded the opaque scene again over the depth: the
    /// lights changed (clouds drifting over the ground) but not the view.
    Geometry,
    /// Kept the shaded layers as well: only the stages after the occlusion
    /// were drawn.
    Whole,
}

/// What the placement table asks a visible-list entry to hold: rows and
/// materials share its 32 bits, else placements don't draw (placed.rs).
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
        let slot = (self.slots).fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let pane = Arc::new(Pane {
            handoff: Arc::new(gpui::TextureHandoff::new(HANDOFF_DEADLINE)),
            shared: Mutex::default(),
            unsupported: Default::default(),
            told: Default::default(),
        });
        let mut panes = self.panes.lock_any();
        panes.retain(|p| p.strong_count() > 0);
        panes.push(Arc::downgrade(&pane));
        Arc::new(Viewport {
            renderer: self.clone(),
            slot,
            pane,
            thread: Mutex::new(None),
            armed: std::sync::atomic::AtomicBool::new(false),
            drawn_at_present: std::sync::atomic::AtomicU64::new(0),
        })
    }

    /// The renderer's state, with what the host set since (`Inputs`)
    /// applied. Counts how long another thread waited for it while a
    /// pane's thread recorded (`take_blocked`). The host's own calls use it
    /// only where waiting is their contract (tests' and benchmarks' reads),
    /// or past `QUEUED_WRITES`.
    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        let mut state = match self.state.try_lock() {
            Ok(state) => state,
            Err(std::sync::TryLockError::WouldBlock) => {
                let at = std::time::Instant::now();
                // Poisoned only by a frame that failed on a lost device
                // (`produce`); the next device's frame makes its copies anew.
                let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
                if !PRODUCING.get() {
                    let waited = at.elapsed().as_nanos() as u64;
                    (self.blocked).fetch_add(waited, std::sync::atomic::Ordering::Relaxed);
                }
                state
            }
            Err(std::sync::TryLockError::Poisoned(e)) => e.into_inner(),
        };
        let taken = {
            let mut host = self.host.lock_any();
            // Until a frame answers after them, the writes taken here are
            // still to upload, whatever the last frame answered.
            if host.inputs.queued() {
                host.answers.pending = true;
                host.answers.settled = false;
            }
            host.inputs.take()
        };
        taken.apply(&mut state);
        state
    }

    /// A read that never waits for a pane's frame: `now` with the state
    /// while it is free, else `last` with what the last frame answered.
    /// Either way with the host's side, whose writes the state may not
    /// have taken yet. Takes nothing from `Inputs`: applying a table is a
    /// frame's work, not a read's.
    fn peek<T>(&self, now: impl FnOnce(&State, &Host) -> T, last: impl FnOnce(&Host) -> T) -> T {
        let state = match self.state.try_lock() {
            Ok(state) => state,
            Err(std::sync::TryLockError::Poisoned(e)) => e.into_inner(),
            Err(std::sync::TryLockError::WouldBlock) => return last(&self.host.lock_any()),
        };
        now(&state, &self.host.lock_any())
    }

    /// Notes what the host reads while a pane's thread holds the state
    /// (`Answers`), as `state` stands after a frame.
    fn answer(&self, state: &State) {
        let stats = state.stats();
        let (pending, settled, waiting) = (state.pending(), state.settled(), state.waiting_on());
        let drawn = state.slots.iter().map(|s| s.drawn).collect();
        let picks = (0..state.slots.len())
            .map(|i| {
                let cursor = state.slots[i].pick_request?;
                Some((cursor, state.hit(i, cursor)))
            })
            .collect();
        let mut host = self.host.lock_any();
        // Writes queued since this frame took its own are still to come.
        let queued = host.inputs.queued();
        host.answers = Answers {
            stats,
            pending: pending || queued,
            settled: settled && !queued,
            waiting,
            drawn,
            picks,
        };
    }

    /// Queues a placement write for the next frame (`Inputs::note`). Past
    /// `QUEUED_WRITES` this call applies the queue itself, waiting for the
    /// frame.
    fn write_placed(&self, write: PlacedWrite) {
        let full = {
            let inputs = &mut self.host.lock_any().inputs;
            inputs.note(&write);
            inputs.queue_placed(write)
        };
        if full {
            drop(self.lock());
        }
        self.changed();
    }

    /// How long threads other than the panes' waited for the renderer's
    /// lock since the last take: the UI thread's share of the recording,
    /// for benchmarks.
    pub fn take_blocked(&self) -> std::time::Duration {
        let ns = self.blocked.swap(0, std::sync::atomic::Ordering::Relaxed);
        std::time::Duration::from_nanos(ns)
    }

    /// Something the views draw changed. A pane whose thread already took
    /// this window frame draws it again, so a change the host makes while
    /// the window lays out still shows in this frame, as it did when panes
    /// recorded at the frame's end. Changes made before the frame (input)
    /// cost nothing more. Call with the state unlocked.
    fn changed(&self) {
        let panes = self.panes.lock_any().clone();
        for pane in panes.iter().filter_map(std::sync::Weak::upgrade) {
            // Between frames (input) the thread's last frame is drawn
            // already, and the next frame's ask takes the change.
            let Some(asked) = pane.handoff.unread() else {
                continue;
            };
            let taken = pane.shared.lock_any().taken;
            if taken.is_some_and(|(_, frame)| frame == asked) {
                pane.handoff.ask();
            }
        }
    }

    /// Replaces the batches, markers and light.
    pub fn set_scene(&self, scene: &Scene) {
        let mut batches: Vec<Prepared> = scene.batches.iter().map(Prepared::new).collect();
        // Built against the latest scene set, drawn yet or not, outside
        // every lock; numbered as it is queued.
        let latest = self.host.lock_any().inputs.scene.clone();
        // The same terrain keeps its patch meshes, and with them their GPU
        // copies.
        let terrain = scene
            .terrain
            .as_ref()
            .map(|source| TerrainTiles::new(source, latest.as_deref(), &mut batches));
        drop(latest);
        // The kit's ground goes first among the passes, so it records
        // right after the kit's meshes and placements, as it always has.
        let ground = (terrain.is_some() && !matches!(scene.ground, GroundDraw::Data))
            .then(|| Arc::new(TerrainPass::default()));
        let passes = (ground.iter())
            .map(|g| g.clone() as Arc<dyn Pass>)
            .chain(scene.passes.iter().cloned())
            .collect();
        let mut prepared = PreparedScene {
            generation: 0,
            batches,
            terrain,
            markers: scene.markers.clone(),
            lines: scene.lines.clone(),
            light: scene.light,
            local_lights: scene.local_lights.clone(),
            tone: scene.tone,
            // The sky's light, projected once per scene rather than per
            // frame (4096 map samples).
            ambient: match scene.sky.as_ref() {
                Some(Sky {
                    ambient: Some(m), ..
                }) => *m,
                Some(Sky {
                    radiance: Some(m), ..
                }) => m.irradiance(),
                _ => light::hemisphere(scene.light.sky, scene.light.ground),
            },
            sky: scene.sky.clone(),
            occlusion: scene.occlusion,
            ground_tint: scene.ground_tint.clone(),
            cloud_shadows: scene.cloud_shadows.clone(),
            glow: scene.glow,
            environment: scene.environment.clone(),
            view_mode: scene.view_mode,
            host_texture: scene.host_texture.clone(),
            reflection: scene.reflection,
            ground,
            ground_draw: scene.ground.clone(),
            passes,
        };
        let replaced = {
            let mut host = self.host.lock_any();
            let inputs = &mut host.inputs;
            prepared.generation = inputs.scene.as_ref().map_or(1, |s| s.generation + 1);
            inputs.unread |= INPUT_SCENE;
            inputs.scene.replace(Arc::new(prepared))
        };
        // A scene no frame took may hold the last references to its
        // meshes: CPU memory only, uploads happen as a frame draws.
        drop(replaced);
        self.changed();
    }

    /// A renderer whose lit placements shade with `families`: meshes name
    /// theirs by [`Families::id`] (`MeshData::family`). `default()` draws
    /// with the kit's own family alone. The set is the renderer's for
    /// good: draws are ordered and pipelines chosen by it.
    pub fn with_families(families: Arc<Families>) -> Self {
        let mut renderer = Self::default();
        renderer.families = families.clone();
        renderer.state.lock_any().assets.placed.families = families;
        renderer
    }

    /// Keeps the shadow maps as they are while the ground takes a run of
    /// edits, such as a brush stroke's dabs: the ground itself still draws
    /// each edit at once. Once every hold has dropped, the next frame draws
    /// the maps again over the union of where the ground moved, or whole
    /// past `SHADOW_PARTIAL_MAX`. Anything else that moves shadows (the
    /// sun, placements, a new scene, a fit the ground's bounds changed)
    /// draws them at once, held or not.
    pub fn hold_shadows(&self) -> ShadowHold {
        (self.shadow_holds).fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        ShadowHold(self.shadow_holds.clone())
    }

    /// The family set this renderer draws with ([`Renderer::with_families`]):
    /// a second renderer of the same pieces takes it too.
    pub fn families(&self) -> Arc<Families> {
        self.families.clone()
    }

    /// Replaces the model pieces placements draw. Clears the placements:
    /// rows name pieces by index.
    pub fn set_pieces(&self, pieces: Vec<Piece>) {
        placed::check_pieces(&pieces, &self.families);
        self.write_placed(PlacedWrite::Pieces(pieces));
    }

    /// Replaces every placement: row i draws piece `piece_of[i]` with
    /// `transforms[i]`, culled and given a detail level on the GPU each
    /// frame. Picks report rows.
    pub fn set_placements(&self, piece_of: &[u32], transforms: &[Mat4]) {
        // The bounds go through every row's matrix: the frame works them
        // out as it applies the table.
        let write = PlacedWrite::Rows(piece_of.to_vec(), transforms.to_vec(), None);
        self.write_placed(write);
    }

    /// `set_placements` with rows built elsewhere and their world bounds:
    /// every piece's mesh bounds through every row's matrix, which a host
    /// can work out on a thread of its own instead of the frame that
    /// applies the table (`set_placements`).
    pub fn set_placements_bounded(
        &self,
        piece_of: Vec<u32>,
        transforms: Vec<Mat4>,
        bounds: [Vec3; 2],
    ) {
        self.write_placed(PlacedWrite::Rows(piece_of, transforms, Some(bounds)));
    }

    /// Waits for a pane's frame and applies the writes queued: for tests.
    pub fn placement_counts(&self) -> PlacementCounts {
        self.lock().assets.placed.counts()
    }

    /// Moves rows `first..` in place: a drag writes only its own rows.
    pub fn move_placements(&self, first: u32, transforms: &[Mat4]) {
        self.write_placed(PlacedWrite::Moves(first, transforms.to_vec()));
    }

    /// Sets rows `first..`'s instance words: one `u32` a row that the kit
    /// carries untouched to the rows' surface families (`LitPixel.word` in
    /// placed.wgsl) and reads nothing of, for what varies per placement
    /// rather than per mesh. Every row's is 0 until set, and again after
    /// `set_placements`. Only families drawn from the arena see them yet.
    pub fn set_instance_words(&self, first: u32, words: &[u32]) {
        self.write_placed(PlacedWrite::Words(first, words.to_vec()));
    }

    /// Replaces the overlay lines: drawn like `Scene::lines` but set apart
    /// from the scene, so a brush ring or guide that follows the cursor
    /// changes a line buffer rather than the scene.
    pub fn set_overlay(&self, lines: Vec<Line>) {
        {
            let inputs = &mut self.host.lock_any().inputs;
            // Hosts set it as they lay out, mostly to the same lines; a
            // change then has the views' threads record the frame again
            // (`changed`).
            if inputs.overlay == lines {
                return;
            }
            inputs.overlay = lines;
            inputs.unread |= INPUT_OVERLAY;
        }
        self.changed();
    }

    /// Draws the scene at `scale` of each pane's pixels each way (0.25 to
    /// 1), and the post stage stretches it over the pane; lines, markers and
    /// the outline stay at full size. Fewer pixels shade: at 0.75 under
    /// three fifths as many. Detail levels still follow the pane's own size.
    pub fn set_render_scale(&self, scale: f32) {
        {
            let inputs = &mut self.host.lock_any().inputs;
            let scale = Some(scale.clamp(0.25, 1.));
            if inputs.render_scale == scale {
                return;
            }
            inputs.render_scale = scale;
            inputs.unread |= INPUT_SCALE;
        }
        self.changed();
    }

    /// What the views leave out or draw cheaper from the next frame on.
    pub fn set_detail(&self, detail: Detail) {
        {
            let inputs = &mut self.host.lock_any().inputs;
            if inputs.detail == detail {
                return;
            }
            inputs.detail = detail;
            inputs.unread |= INPUT_DETAIL;
        }
        self.changed();
    }

    pub fn detail(&self) -> Detail {
        self.host.lock_any().inputs.detail
    }

    /// Replaces the selection outline; `None` or empty marks draw nothing.
    /// Set apart from the scene, as the overlay is: selection changes often.
    pub fn set_outline(&self, outline: Option<Outline>) {
        self.host.lock_any().inputs.outline = Some(outline);
        self.changed();
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
        self.write_placed(PlacedWrite::Selection {
            rows,
            marks,
            color,
            radius: marker.radius,
        });
    }

    /// Drags the selected rows on the GPU: while `Some`, each draws at
    /// `delta` times its transform from when the drag began, and its marker
    /// follows. The table is not touched; at `None` the rows go back to
    /// what it holds, so move them there (`move_placements`) first. A
    /// drag of 20k items costs the CPU one matrix a frame.
    pub fn drag_selection(&self, delta: Option<Mat4>) {
        self.write_placed(PlacedWrite::Drag(delta));
    }

    /// Call when the host starts building a frame; CPU time counts from here.
    pub fn mark_frame(&self) {
        let inputs = &mut self.host.lock_any().inputs;
        inputs.frame_mark = Some(std::time::Instant::now());
        inputs.unread |= INPUT_MARKS;
    }

    /// Call when the host has built its frame's elements.
    pub fn mark_built(&self) {
        let inputs = &mut self.host.lock_any().inputs;
        inputs.built_mark = Some(std::time::Instant::now());
        inputs.unread |= INPUT_MARKS;
    }

    /// While a pane's thread records, the last frame's: readouts ask as
    /// the window lays out.
    pub fn stats(&self) -> Stats {
        self.peek(|state, _| state.stats(), |host| host.answers.stats)
    }

    /// Whether the latest scene or placement meshes are still uploading,
    /// or set and not yet taken by a frame. While a pane's thread records,
    /// as its last frame left them.
    pub fn pending(&self) -> bool {
        self.peek(
            |state, host| host.inputs.queued() || state.pending(),
            |host| host.inputs.queued() || host.answers.pending,
        )
    }

    /// Whether the latest scene is not on screen yet. A host that replaces
    /// the scene every frame (animation playback) waits on this: a new
    /// scene starts its uploads over, so on a slow device one each frame
    /// would never be drawn. Narrower than [`Renderer::pending`], which
    /// texture levels and shadow cascades also hold.
    pub fn scene_waiting(&self) -> bool {
        let state = self.lock();
        let generation = state.scene.as_ref().map(|s| s.generation);
        generation.is_some_and(|g| {
            (state.device.as_ref())
                .is_none_or(|d| d.presented.as_ref().is_none_or(|p| p.generation != g))
        })
    }

    /// The latest scene's plugged passes, to warm without holding the
    /// renderer's lock (`Pass::warm` compiles).
    pub(crate) fn passes(&self) -> Vec<Arc<dyn Pass>> {
        let host = self.host.lock_any();
        (host.inputs.scene.as_ref()).map_or_else(Vec::new, |s| s.passes.clone())
    }

    /// Whether a still view draws the same from here on: nothing uploading
    /// or still to come (`pending`), and no texture level held past what
    /// the views ask, which residency lets go on the wall clock two to four
    /// seconds later. For tests that compare frames byte for byte; ground
    /// pages bake on their own budget (`Drawn::pages`).
    pub fn settled(&self) -> bool {
        self.peek(
            |state, host| !host.inputs.queued() && state.settled(),
            |host| !host.inputs.queued() && host.answers.settled,
        )
    }

    /// What keeps `pending` true, by name: for benchmarks that time how a
    /// load reaches the screen.
    pub fn waiting_on(&self) -> Vec<&'static str> {
        let (mut out, queued) = self.peek(
            |state, host| (state.waiting_on(), host.inputs.queued()),
            |host| (host.answers.waiting.clone(), host.inputs.queued()),
        );
        if queued {
            out.push("writes no frame took yet");
        }
        out
    }

    /// Uploads still running, or a pass on the clock (waves). Either one
    /// keeps presenting; the host can refresh a readout without laying the
    /// window out again.
    ///
    /// While a pane's thread records, the panes' answers after their last
    /// frames: readouts ask as the window lays out.
    pub fn wants_frames(&self) -> bool {
        let state = match self.state.try_lock() {
            Ok(state) => state,
            Err(std::sync::TryLockError::WouldBlock) => {
                let panes = self.panes.lock_any().clone();
                return self.host.lock_any().inputs.queued()
                    || (panes.iter().filter_map(std::sync::Weak::upgrade))
                        .any(|p| p.shared.lock_any().wants.frame);
            }
            Err(std::sync::TryLockError::Poisoned(e)) => e.into_inner(),
        };
        let queued = self.host.lock_any().inputs.queued();
        queued || state.pending() || state.animating() || state.assets.placed.residency.due
    }

    fn note_surface_wait(&self, wait: std::time::Duration) {
        self.lock().surface_wait = wait;
    }

    /// Reads the device's memory into `stats`, at most once a second: the
    /// report lists every allocation by name, too much to build per frame.
    /// The lock is not held while it builds.
    pub(crate) fn note_vram(&self, device: &wgpu::Device) {
        {
            let mut state = self.lock();
            if state
                .vram_read
                .is_some_and(|at| at.elapsed().as_secs_f32() < 1.)
            {
                return;
            }
            state.vram_read = Some(std::time::Instant::now());
        }
        let Some(report) = device.generate_allocator_report() else {
            return;
        };
        let mut state = self.lock();
        state.stats.vram_used = report.total_allocated_bytes;
        state.stats.vram_reserved = report.total_reserved_bytes;
    }

    /// Whether a window's device is there to draw with, else starts building
    /// it on a thread. Compiling every pipeline takes ~0.2 s with the
    /// driver's shader cache warm and over a second cold, which stalled the
    /// whole window on the first frame of a view; the view stays empty for
    /// those frames instead. Offscreen frames build in `draw`, since they
    /// must draw when asked.
    fn device_ready(&self, gpu: &Gpu, format: wgpu::TextureFormat) -> bool {
        use std::sync::mpsc::TryRecvError;
        let mut state = self.lock();
        state.move_to(&gpu.device);
        // A changed format (rare: the window moved to another display)
        // rebuilds in `draw`, which also drops what the old device made.
        if state.device.is_some() {
            return true;
        }
        match state.building.take() {
            Some((built_for, rx)) if built_for == format => match rx.try_recv() {
                Ok(device) => {
                    state.device = Some(device);
                    state.moved = false;
                    true
                }
                Err(TryRecvError::Empty) => {
                    state.building = Some((built_for, rx));
                    false
                }
                // The build panicked; `draw` builds on the spot, so the
                // panic shows where it happens.
                Err(TryRecvError::Disconnected) => true,
            },
            _ => {
                offscreen::guard_exit();
                // Exiting: nothing more to draw, and a compile must not
                // start under the driver's teardown.
                let Some(busy) = busy() else { return false };
                let (tx, rx) = std::sync::mpsc::sync_channel(1);
                let gpu = gpu.clone();
                let families = state.assets.placed.families.clone();
                // A new device after a loss: the host's warm-up ran on the
                // old one, and the passes compiling as they prepare would
                // hold up the UI thread.
                let passes = match &state.scene {
                    Some(scene) if state.moved => scene.passes.clone(),
                    _ => Vec::new(),
                };
                let spawned =
                    std::thread::Builder::new()
                        .name("kit device".into())
                        .spawn(move || {
                            let device = Device::new(&gpu, format, &families);
                            let warm = Warm {
                                device: &gpu.device,
                                queue: &gpu.queue,
                                frame_layout: &device.kit.frame_layout,
                                kit: &device.kit,
                                format,
                            };
                            for pass in &passes {
                                pass.warm(&warm);
                            }
                            let _ = tx.send(device);
                            drop(busy);
                        });
                match spawned {
                    Ok(_) => {
                        state.building = Some((format, rx));
                        false
                    }
                    Err(_) => true,
                }
            }
        }
    }

    /// What the last offscreen frame drew (viewport 0).
    pub fn drawn(&self) -> Drawn {
        self.drawn_by(0)
    }

    fn drawn_by(&self, slot: usize) -> Drawn {
        self.peek(
            |state, _| state.slots.get(slot).map(|s| s.drawn),
            |host| host.answers.drawn.get(slot).copied(),
        )
        .unwrap_or_default()
    }

    /// Points viewport `slot` at a camera; offscreen rendering uses slot 0.
    pub(crate) fn aim(&self, slot: usize, view: View) {
        (self.slots).fetch_max(slot + 1, std::sync::atomic::Ordering::Relaxed);
        let mut state = self.lock();
        if state.slots.len() <= slot {
            state.slots.resize_with(slot + 1, Slot::default);
        }
        state.slots[slot].view = Some(view);
    }

    /// See [`Viewport::pick`]. While a pane's thread records, the cursor
    /// is asked of the next frame and the answer is the last frame's.
    pub(crate) fn pick(&self, slot: usize, cursor: Point<Pixels>) -> Option<Hit> {
        let mut state = match self.state.try_lock() {
            Ok(state) => state,
            Err(std::sync::TryLockError::Poisoned(e)) => e.into_inner(),
            Err(std::sync::TryLockError::WouldBlock) => {
                let mut host = self.host.lock_any();
                // One per slot: the latest cursor is the one asked.
                let picks = &mut host.inputs.picks;
                picks.retain(|(s, _)| *s != slot);
                picks.push((slot, cursor));
                let answer = host.answers.picks.get(slot).copied().flatten();
                return answer.filter(|(at, _)| *at == cursor)?.1;
            }
        };
        let s = state.slots.get_mut(slot)?;
        let view = s.view?;
        if !view.bounds.contains(&cursor) {
            return None;
        }
        s.pick_request = Some(cursor);
        state.hit(slot, cursor)
    }
}

impl State {
    /// What slot `slot`'s last pick read back under `cursor`, if it read
    /// back there.
    fn hit(&self, slot: usize, cursor: Point<Pixels>) -> Option<Hit> {
        let picked = self.slots.get(slot)?.picked?;
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
            let ids = &self.device.as_ref()?.pick_ids;
            (*ids.get(picked.handle as usize - 1)?, None)
        };
        Some(Hit {
            id,
            placement,
            position,
        })
    }

    /// The smoothed frame times, with every view drawn lately summed in
    /// (`Renderer::stats`).
    fn stats(&self) -> Stats {
        let mut stats = self.stats;
        // 0.5 s: a closed or hidden pane drops out of the totals.
        let live = (self.slots.iter().map(|s| &s.times))
            .filter(|t| t.at.is_some_and(|at| at.elapsed().as_secs_f32() < 0.5));
        let (mut total, mut span) = (0., 0.);
        let mut avg = [("", 0.); TIMED_PASSES as usize];
        for t in live {
            total += t.total;
            span += t.span;
            for &(name, ms) in t.avg.iter().filter(|(n, _)| !n.is_empty()) {
                if let Some(row) = avg.iter_mut().find(|(a, _)| a.is_empty() || *a == name) {
                    *row = (name, row.1 + ms);
                }
            }
        }
        stats.gpu_ms = total;
        stats.span_avg_ms = span;
        stats.pass_avg = avg;
        stats
    }

    /// See [`Renderer::settled`].
    fn settled(&self) -> bool {
        !self.pending() && !self.assets.placed.residency.due
    }

    /// See [`Renderer::waiting_on`].
    fn waiting_on(&self) -> Vec<&'static str> {
        let placed = &self.assets.placed;
        let mut out = Vec::new();
        let mut add = |on: bool, name| {
            if on {
                out.push(name);
            }
        };
        add(placed.resident.iter().any(Option::is_none), "placed meshes");
        add(!placed.unstreamed.is_empty(), "placed texels");
        add(placed.upload_pending(), "arena upload");
        add(placed.placements_pending(), "placements");
        add(self.building.is_some(), "device build");
        if let Some(d) = &self.device {
            // `passes_loading` holds the ground surface's state too.
            let ground =
                (d.surface.as_ref()).map(|s| (s.uploading, s.ground.bind_group().is_none()));
            add(ground.is_some_and(|g| g.0), "ground surface upload");
            add(ground.is_some_and(|g| g.1), "ground bind group");
            add(
                d.passes_loading && ground.is_none_or(|g| g == (false, false)),
                "plugged passes",
            );
            add(d.controls_filling(), "controls");
        }
        add(placed.residency.wanted > 0, "texture levels");
        let generation = self.scene.as_ref().map(|s| s.generation);
        add(
            generation.is_some_and(|g| {
                (self.device.as_ref())
                    .is_none_or(|d| d.presented.as_ref().is_none_or(|p| p.generation != g))
            }),
            "scene",
        );
        if out.is_empty() && self.texels_pending() {
            out.push("culls");
        }
        if self.cascades_relaxed() {
            out.push("shadow cascades");
        }
        out
    }

    /// Drops every GPU copy made on another device than `device`, as after
    /// a device loss, when wgpu rejects all of them. What they were made
    /// from stays: the scene, the placements and their textures (read
    /// again where released), the heights and the overlays, so the frames
    /// that follow make them again on `device`, streaming in as an
    /// island's opening does. Passes drop theirs as they prepare
    /// (`OnDevice`).
    fn move_to(&mut self, device: &wgpu::Device) {
        if self.on.as_ref() == Some(device) {
            return;
        }
        if self.on.replace(device.clone()).is_none() {
            return;
        }
        // A build under way is for the old device; its thread finishes
        // and its result goes nowhere.
        self.device = None;
        self.building = None;
        self.moved = true;
        let assets = &mut self.assets;
        assets.meshes.clear();
        assets.textures.clear();
        assets.heights.clear();
        assets.color_maps.clear();
        assets.collected = None;
        assets.writes += 1;
        let placed = &mut assets.placed;
        placed.release();
        placed.residency.restart();
        placed.version += 1;
        for slot in &mut self.slots {
            slot.targets = None;
            slot.picked = None;
            slot.wants = Default::default();
        }
        self.opened = false;
    }

    /// Whether the scene moves by itself: a plugged pass or a view mode on
    /// the clock, or posed placements.
    fn animating(&self) -> bool {
        let clouds = self.detail.cloud_shadows != CloudMotion::Off;
        self.assets.placed.posing()
            || (self.scene.as_ref()).is_some_and(|s| {
                s.view_mode != 0
                    || s.passes.iter().any(|p| p.animating())
                    || clouds && s.cloud_shadows.as_ref().is_some_and(CloudShadows::moving)
            })
    }

    /// `uploading`, or texture levels the views asked for still to come
    /// (residency.rs): a view drawn last frame whose camera moved since
    /// its culls last answered, answers not yet taken, or levels filling.
    /// Hosts draw on while it holds; frames keep their layers regardless,
    /// as a level arriving bumps `Assets::writes`.
    fn pending(&self) -> bool {
        self.uploading() || self.texels_pending() || self.cascades_relaxed()
    }

    /// A view drawn last frame keeps a cascade coarser than it wants
    /// (`Cascades::relaxed`).
    fn cascades_relaxed(&self) -> bool {
        (self.slots.iter().enumerate())
            .filter(|(i, _)| self.drawn_slots & 1 << (*i).min(63) != 0)
            .any(|(_, s)| {
                (s.targets.as_ref()).is_some_and(|t| t.cascades.as_ref().is_some_and(|c| c.relaxed))
            })
    }

    fn texels_pending(&self) -> bool {
        let placed = &self.assets.placed;
        // No rows: no cull runs to answer the views, and nothing samples.
        let Some(arena) = placed.arena.as_ref().filter(|_| placed.count > 0) else {
            return false;
        };
        let drawn = |(i, _): &(usize, &Slot)| self.drawn_slots & 1 << (*i).min(63) != 0;
        placed.residency.wanted > 0
            || (self.slots.iter().enumerate().filter(drawn))
                .any(|(_, s)| s.wants.fresh || !s.wants.answers(arena.id, s.view))
    }

    /// Uploads or a scene still coming: what a frame draws now is not what
    /// the next will.
    fn uploading(&self) -> bool {
        if self.assets.placed.pending()
            || self.building.is_some()
            || self
                .device
                .as_ref()
                .is_some_and(|d| d.passes_loading || d.controls_filling())
        {
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

    /// The last frame this pane drew, to keep on screen after the pane and
    /// its renderer are gone: one view-sized texture, not the scene.
    /// `None` before the first frame.
    pub fn still(&self) -> Option<Still> {
        let handoff = &self.pane.handoff;
        handoff.has_frame().then(|| Still(handoff.clone()))
    }

    /// Whether the pane's thread has handed over a frame, so a paint now
    /// shows the scene rather than nothing.
    pub fn has_frame(&self) -> bool {
        self.pane.handoff.has_frame()
    }

    /// The window's GPU can't draw 3D ([`supported`]): the pane stays
    /// empty, and its host should say so there. Known once the pane's
    /// thread has seen the device, after its first paint; the window then
    /// draws again so the host can.
    pub fn unsupported(&self) -> bool {
        (self.pane.unsupported).load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Points this pane at `camera` without painting it: the next window
    /// frame asks the pane's thread for a frame with it, so a camera move
    /// only needs a frame, not a new paint of the pane (which keeps its
    /// last bounds).
    pub fn set_camera(&self, camera: Camera, extent: Extent) {
        let handoff = &self.pane.handoff;
        let (drawn, shown) = (handoff.drawn(), handoff.take_composite());
        let mut shared = self.pane.shared.lock_any();
        shared.note(shown);
        let Shared { view, clock, .. } = &mut *shared;
        if let Some((view, _)) = view {
            clock.camera(Some(view.camera), camera, drawn);
            view.camera = camera;
            view.extent = extent;
        }
    }

    /// Takes what was timed since the last take.
    pub fn take_timing(&self) -> PaneTiming {
        std::mem::take(&mut self.pane.shared.lock_any().clock.timing)
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
        self.start();
        let view = View {
            camera,
            extent,
            bounds,
        };
        let handoff = &self.pane.handoff;
        let (keep_drawing, again) = {
            let (drawn, shown) = (handoff.drawn(), handoff.take_composite());
            let mut shared = self.pane.shared.lock_any();
            shared.note(shown);
            let old = shared
                .view
                .replace((view, bounds.scale(window.scale_factor())));
            (shared.clock).camera(old.map(|(v, _)| v.camera), camera, drawn);
            // The thread already took this frame's view before layout moved
            // the pane or the camera: it draws the frame again.
            let asked = handoff.asked();
            let again = (shared.taken).is_some_and(|(v, frame)| frame == asked && v != view);
            // Uploads are spread across frames, and a pick needs the frame
            // that reads it back; a pane yet to draw has its device to
            // build. The thread's answer to this frame comes after.
            (shared.wants.more || shared.answered == 0, again)
        };
        window.paint_gpu_texture(bounds, handoff.clone());
        if again {
            handoff.ask();
        }
        if keep_drawing {
            // Not request_animation_frame: that notifies the view being
            // painted, which owns this canvas, so every wave frame rebuilt
            // and laid out that view. A present draws this pane again;
            // the clock the waves read moves on its own.
            self.arm_present(window);
        }
    }

    /// Starts the pane's thread, once: it records each frame a window asks
    /// for, until the pane is dropped.
    fn start(&self) {
        let mut thread = self.thread.lock_any();
        if thread.is_some() {
            return;
        }
        let (renderer, pane, slot) = (self.renderer.clone(), self.pane.clone(), self.slot);
        *thread = std::thread::Builder::new()
            .name(format!("kit view {slot}"))
            .spawn(move || renderer.produce(slot, &pane))
            .ok();
    }

    fn picking(slot: &Slot) -> bool {
        slot.pick_request.is_some() && slot.picked.map(|p| p.cursor) != slot.pick_request
    }

    fn wants_frame(&self) -> bool {
        let shared = self.pane.shared.lock_any();
        // A frame the thread has yet to answer may start uploads.
        shared.wants.frame || shared.answered < self.pane.handoff.asked()
    }

    /// Presents this frame, and looks at the frames after it while this
    /// pane still has something to draw and its canvas is still in the
    /// window. One chain at a time: a second paint before it runs only
    /// repeats the present.
    ///
    /// The chain holds the pane weakly and ends at the first frame that
    /// did not draw its canvas after it asked for one. Waves animate for
    /// good, so a chain holding it kept every island the editor had opened
    /// presenting, and kept its renderer and all its video memory, after
    /// the island was closed.
    fn arm_present(self: &Arc<Self>, window: &mut Window) {
        use std::sync::atomic::Ordering;
        self.present(window);
        if self
            .armed
            .compare_exchange(false, true, Ordering::Relaxed, Ordering::Relaxed)
            .is_err()
        {
            return;
        }
        self.follow(window, true);
    }

    /// Presents the frame GPUI is about to finish, and notes how many
    /// frames had drawn this pane before it.
    fn present(&self, window: &mut Window) {
        (self.drawn_at_present).store(
            self.pane.handoff.drawn(),
            std::sync::atomic::Ordering::Relaxed,
        );
        window.request_present();
    }

    /// Looks at the next frame: presents it when this pane wants one and
    /// `Detail::idle_fps` allows it, else waits on a timer until it does,
    /// without presenting. Input repaints the window (and so this pane) on
    /// its own meanwhile; the chain stays armed so those paints do not
    /// start a second one. `presented`: the chain asked for the frame
    /// before, which drew this pane unless it left the window.
    ///
    /// A timer that asked for its present as it fired started the next
    /// wait in the frame of that present, before it drew, counted from the
    /// frame before: two presents for every interval, 42 to 46 a second
    /// at 30.
    fn follow(self: &Arc<Self>, window: &mut Window, presented: bool) {
        use std::sync::atomic::Ordering;
        let pane = Arc::downgrade(self);
        window.on_next_frame(move |window, cx| {
            let Some(viewport) = pane.upgrade() else {
                return;
            };
            if viewport.unsupported() && !viewport.pane.told.swap(true, Ordering::Relaxed) {
                window.refresh();
            }
            let drawn = viewport.pane.handoff.drawn();
            let left = presented && drawn == viewport.drawn_at_present.load(Ordering::Relaxed);
            if left || !viewport.wants_frame() {
                viewport.armed.store(false, Ordering::Relaxed);
                return;
            }
            let Some(wait) = viewport.idle_wait() else {
                viewport.present(window);
                viewport.follow(window, true);
                return;
            };
            drop(viewport);
            window
                .spawn(cx, async move |cx| {
                    cx.background_executor().timer(wait).await;
                    let _ = cx.update(|window, _| {
                        if let Some(viewport) = pane.upgrade() {
                            viewport.follow(window, false);
                        }
                    });
                })
                .detach();
        });
    }

    /// How long to hold the next frame back under `Detail::idle_fps`:
    /// `None` when something besides animation wants it (uploads, a pick)
    /// or it is due. Counted from the renderer's last frame, whatever drew
    /// it, so other presents (input, the host's readouts) push it back.
    fn idle_wait(&self) -> Option<std::time::Duration> {
        let (fps, began) = self.pane.shared.lock_any().wants.idle?;
        let interval = std::time::Duration::from_secs_f32(1. / fps as f32);
        let wait = (began + interval).saturating_duration_since(std::time::Instant::now());
        // Frames start on the display's beat, a little after the one that
        // set `frame_began`: within an eighth of the interval is due, or
        // 30 a second on a 60 Hz display waited out a third vsync, 20.
        (wait > interval / 8).then_some(wait)
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
        self.renderer.drawn_by(self.slot)
    }
}

/// A pane's last frame (`Viewport::still`). Its thread has ended or will
/// when the pane drops, so painting draws that frame and asks for nothing.
/// Texels map one to a device pixel from the bounds' origin: a resized pane
/// crops it rather than stretching.
#[derive(Clone)]
pub struct Still(Arc<gpui::TextureHandoff>);

impl Still {
    pub fn paint(&self, bounds: Bounds<Pixels>, window: &mut Window) {
        window.paint_gpu_texture(bounds, self.0.clone());
    }
}

impl Drop for Viewport {
    /// Ends the pane's thread. It may be recording; exit joins it rather
    /// than the UI thread waiting here.
    fn drop(&mut self) {
        self.pane.handoff.close();
        if let Some(thread) = self
            .thread
            .get_mut()
            .unwrap_or_else(|e| e.into_inner())
            .take()
        {
            join_at_exit(thread);
        }
    }
}

impl Renderer {
    /// A pane's thread: records each frame a window asks for into the
    /// pane's texture and submits it, until the pane is dropped.
    fn produce(&self, slot: usize, pane: &Pane) {
        PRODUCING.set(true);
        let mut output = OnDevice::<Output>::default();
        // A device a frame failed on: the next frames there answer without
        // recording, until GPUI hands over a new one.
        let mut failed: Option<wgpu::Device> = None;
        while let Some((frame, gpu)) = pane.handoff.wait() {
            // Exiting: a compile or submit must not run under the driver's
            // teardown.
            let Some(_busy) = busy() else { break };
            let unsupported = !supported(&gpu.device);
            (pane.unsupported).store(unsupported, std::sync::atomic::Ordering::Relaxed);
            offscreen::guard_exit();
            let shown = pane.handoff.take_composite();
            let started = std::time::Instant::now();
            let view = {
                let mut shared = pane.shared.lock_any();
                let shared = &mut *shared;
                shared.note(shown);
                shared.started.rotate_right(1);
                shared.started[0] = (frame, Some(started));
                shared.taken = shared.view.map(|(v, _)| (v, frame));
                shared.view
            };
            // A device lost while this thread records (another thread
            // destroyed it, or the driver reset) fails wgpu calls that
            // panic, as `create_buffer_init`'s mapping does. GPUI recovers
            // onto a new device on its next frame, as it did when this ran
            // inside GPUI's frame, where a lost device failed first at the
            // swapchain; here it can come at any point of the recording.
            let drew = if failed.as_ref() == Some(&gpu.device) {
                None
            } else {
                let frame = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    let (view, scaled) = view?;
                    self.produce_frame(slot, &gpu, &mut output, view, scaled, shown)
                }));
                frame.unwrap_or_else(|_| {
                    eprintln!(
                        "kit view {slot}: a frame failed, as on a lost device; no more frames on it"
                    );
                    failed = Some(gpu.device.clone());
                    None
                })
            };
            pane.handoff.submit(frame, || {
                let (commands, texture) = drew?;
                gpu.queue.submit([commands]);
                texture
            });
            let wants = self.wants(slot);
            let mut shared = pane.shared.lock_any();
            shared.answered = frame;
            shared.wants = wants;
        }
    }

    /// Records one frame of `slot` with `view` into `output`, made on
    /// `gpu`'s device at the pane's size. The commands to submit, and the
    /// texture to show unless nothing was drawn (no scene yet).
    fn produce_frame(
        &self,
        slot: usize,
        gpu: &gpui::HandoffGpu,
        output: &mut OnDevice<Output>,
        view: View,
        scaled: Bounds<ScaledPixels>,
        shown: Option<Composite>,
    ) -> Option<(wgpu::CommandBuffer, Option<gpui::HandoffTexture>)> {
        let kit = Gpu {
            device: gpu.device.clone(),
            queue: gpu.queue.clone(),
        };
        device::window_frame(&kit);
        self.note_vram(&gpu.device);
        if let Some(shown) = shown {
            self.note_surface_wait(shown.surface_wait);
        }
        self.aim(slot, view);
        if !self.device_ready(&kit, VIEW_FORMAT) {
            return None;
        }
        let sp = |v: ScaledPixels| f64::from(v) as f32;
        let size = [scaled.size.width, scaled.size.height].map(|v| sp(v).ceil().max(1.) as u32);
        let room = size.map(|v| v.next_multiple_of(64));
        if (output.get(&gpu.device)).is_none_or(|o| o.size != room) {
            let texture = gpu.device.create_texture(&wgpu::TextureDescriptor {
                label: Some("kit view"),
                size: wgpu::Extent3d {
                    width: room[0],
                    height: room[1],
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: VIEW_FORMAT,
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT
                    | wgpu::TextureUsages::TEXTURE_BINDING,
                view_formats: &[],
            });
            let out = Output {
                view: texture.create_view(&Default::default()),
                size: room,
            };
            output.set(&gpu.device, out);
        }
        let out = output.get(&gpu.device).expect("made above");
        let mut encoder = gpu.device.create_command_encoder(&Default::default());
        // The texture is the pane's own, kept across frames, and the post
        // pass blends onto it by coverage: where nothing draws (a model
        // view has no sky) the last frame would stay, smeared as the
        // camera turns. Each frame starts clear: the post pass clears it,
        // or this when nothing drew.
        let pane = Bounds::new(point(ScaledPixels(0.), ScaledPixels(0.)), scaled.size);
        let drew = self.draw(
            slot,
            &kit,
            &mut encoder,
            &out.view,
            VIEW_FORMAT,
            room,
            pane,
            pane,
            true,
            true,
        );
        if !drew {
            clear_target(&mut encoder, "kit view clear", &out.view);
        }
        let texture = drew.then(|| gpui::HandoffTexture {
            device: gpu.device.clone(),
            view: out.view.clone(),
            size,
        });
        Some((encoder.finish(), texture))
    }

    /// What `slot` wants after the frame just recorded (`Wants`).
    fn wants(&self, slot: usize) -> Wants {
        let state = self.lock();
        let picking = (state.slots.get(slot)).is_some_and(Viewport::picking);
        let pending = state.pending();
        let animating = state.animating();
        let due = state.assets.placed.residency.due;
        let idle = (state.detail.idle_fps.filter(|&f| f > 0))
            .filter(|_| !pending && !picking && (animating || due))
            .zip(state.frame_began);
        self.answer(&state);
        Wants {
            more: pending || picking || animating,
            frame: pending || picking || animating || due,
            idle,
        }
    }
}

/// `build`'s value for `device` under `key`, built by the first caller and
/// shared after, for as long as the process runs: for a plugged pass's
/// pipelines, the same for every island it draws. The driver compiled the
/// 1800 sea's anew at every island opened, 0.7 s. Layouts made again from
/// the same descriptors fit pipelines built against the first ones, since
/// wgpu pools bind group layouts.
pub fn per_device<T: Clone + Send + Sync + 'static>(
    device: &wgpu::Device,
    key: &'static str,
    build: impl FnOnce() -> T,
) -> T {
    let cell = {
        let mut built = PER_DEVICE.lock_any();
        match built.iter().find(|(d, k, _)| d == device && *k == key) {
            Some((_, _, cell)) => cell.clone(),
            None => {
                let cell = PerDeviceCell::default();
                built.push((device.clone(), key, cell.clone()));
                cell
            }
        }
    };
    let value = cell.get_or_init(|| Box::new(build()));
    (value.downcast_ref::<T>())
        .unwrap_or_else(|| panic!("per_device: {key} holds another type"))
        .clone()
}

type PerDeviceCell = Arc<OnceLock<Box<dyn std::any::Any + Send + Sync>>>;
static PER_DEVICE: Mutex<Vec<(wgpu::Device, &'static str, PerDeviceCell)>> = Mutex::new(Vec::new());

/// Drops what the process-wide caches built on `device`, once it is lost:
/// its pipelines are useless, and the entries held the device for as long
/// as the process ran. Whoever still holds a value keeps it; asking on the
/// new device builds there.
pub(crate) fn forget(device: &wgpu::Device) {
    (PER_DEVICE.lock().unwrap_or_else(|e| e.into_inner())).retain(|(d, ..)| d != device);
    (PLACING.lock().unwrap_or_else(|e| e.into_inner())).retain(|(d, ..)| d != device);
    (PIPELINES.lock().unwrap_or_else(|e| e.into_inner())).retain(|(d, ..)| d != device);
    terrain::forget(device);
}

/// What a host's own passes build their pipelines against, for
/// [`warm_passes`]: the device, its queue, and the frame bind group every
/// plugged pass's pipelines take first (`Prepare::frame_layout`).
pub struct Warm<'a> {
    pub device: &'a wgpu::Device,
    pub queue: &'a wgpu::Queue,
    pub frame_layout: &'a wgpu::BindGroupLayout,
    /// What `Warm::families` and `Warm::ground` build against.
    kit: &'a Kit,
    format: wgpu::TextureFormat,
}

impl Warm<'_> {
    /// Builds the pipelines that draw placements with `families`, on this
    /// thread. A renderer made [`Renderer::with_families`] builds them on
    /// its first frame otherwise.
    pub fn families(&self, families: &Arc<Families>) {
        let gpu = Gpu {
            device: self.device.clone(),
            queue: self.queue.clone(),
        };
        Placing::shared(&gpu, self.kit, families);
    }
}

/// A host's own pipelines for [`warm_passes`]: builds its passes'
/// pipelines, on the thread it is called on.
pub type WarmPasses = Arc<dyn Fn(&Warm<'_>) + Send + Sync>;

/// What every window device builds after the kit's pipelines.
static WARM_PASSES: Mutex<Option<WarmPasses>> = Mutex::new(None);

/// Has each device GPUI makes for its windows build `passes` after the
/// renderer's pipelines, on a thread of their own, before any viewport
/// draws: with the driver's shader cache cold the build takes seconds,
/// which an island opening then does not wait for. The device there is now
/// builds them too. Replaces what an earlier call registered. Needs
/// [`install`], which has the window devices start the kit's own build.
pub fn warm_passes(passes: WarmPasses) {
    *WARM_PASSES.lock().unwrap_or_else(|e| e.into_inner()) = Some(passes.clone());
    if let Some(gpu) = device::window_gpu() {
        let (cell, _) = Pipelines::cell(&gpu.device, VIEW_FORMAT);
        build_warm(gpu, cell, Some(passes));
    }
}

/// Starts building the renderer's pipelines and the host's passes on a
/// window device GPUI just made (`device::install`'s provider).
fn warm(gpu: Gpu) {
    let (cell, new) = Pipelines::cell(&gpu.device, VIEW_FORMAT);
    if !new {
        return;
    }
    let passes = WARM_PASSES
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    build_warm(gpu, cell, passes);
}

/// The build on its thread. A second build for the same device waits on
/// `cell` for the first and builds only the passes.
fn build_warm(gpu: Gpu, cell: Arc<OnceLock<Pipelines>>, passes: Option<WarmPasses>) {
    // Exit waits for the build: a compile cut off by the driver's teardown
    // crashes the process.
    let Some(busy) = busy() else { return };
    let _ = std::thread::Builder::new()
        .name("kit pipelines".into())
        .spawn(move || {
            let built = cell.get_or_init(|| Pipelines::build(&gpu, VIEW_FORMAT));
            if let Some(passes) = passes {
                passes(&Warm {
                    device: &gpu.device,
                    queue: &gpu.queue,
                    frame_layout: &built.kit.frame_layout,
                    kit: &built.kit,
                    format: VIEW_FORMAT,
                });
            }
            drop(busy);
        });
}

/// The format views draw in, whatever the window's: GPUI draws the texture
/// into its own target (`TextureHandoff`). Offscreen targets take it too,
/// so a window device warms the pipelines its views use and tests draw as
/// the app does.
pub(crate) const VIEW_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;

// Per-frame upload budget. Interactive frames matter more than finishing a
// big scene load one frame sooner. Textures stream in bands of rows under
// the same byte budget, so a 64 MB control image takes about eight frames.
const UPLOAD_BYTES: usize = 8 * 1024 * 1024;
// Level-of-detail bands and spatial cells make many small batches.
const UPLOAD_BATCHES: usize = 256;
const UPLOAD_MICROS: u128 = 3000;
// Until a renderer has shown its scene whole once, an island's opening view
// wants 300 MB of texture levels alone: window frames upload eight times
// the bytes, and every upload of the frame shares one deadline, 8 ms from
// the frame's start, which keeps the frame within a 60 Hz display's 16.7.
// The continental island took 64 frames at 60 Hz to complete; with a
// deadline per upload of four times the 3 ms, frames ran to 115 ms.
const OPENING_BYTES: usize = 8;
const OPENING_MICROS: u128 = 8000;

/// Whether opening renderers get the larger budget. Off, they upload as
/// interactive frames do: for timing loads against it.
pub static OPENING_UPLOADS: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(true);

std::thread_local! {
    /// When the opening draw on this thread began (`State::opened`): its
    /// budgets are the larger ones, against that one deadline.
    static OPENING: std::cell::Cell<Option<std::time::Instant>> =
        const { std::cell::Cell::new(None) };
}

/// Marks this thread's draw as opening until dropped.
struct Opening;

impl Opening {
    fn begin(opening: bool) -> Self {
        let on = opening && OPENING_UPLOADS.load(std::sync::atomic::Ordering::Relaxed);
        OPENING.set(on.then(std::time::Instant::now));
        Self
    }
}

impl Drop for Opening {
    fn drop(&mut self) {
        OPENING.set(None);
    }
}
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
    /// Starts the default per-frame byte and time allowance: a window's
    /// frame, which the user waits on.
    pub fn new() -> Self {
        match OPENING.get() {
            Some(began) => Self {
                started: began,
                bytes: 0,
                progressed: false,
                limit: UPLOAD_BYTES * OPENING_BYTES,
                micros: OPENING_MICROS,
            },
            None => Self {
                started: std::time::Instant::now(),
                bytes: 0,
                progressed: false,
                limit: UPLOAD_BYTES,
                micros: UPLOAD_MICROS,
            },
        }
    }

    /// The byte allowance alone, for frames nobody waits on (offscreen
    /// thumbnails and tests): on a loaded machine the clock ran out after
    /// one or two items, so how far a frame got depended on the load.
    pub fn untimed() -> Self {
        // The plain allowance while opening too: tests count the frames a
        // large upload spreads over.
        Self {
            micros: u128::MAX,
            limit: UPLOAD_BYTES,
            ..Self::new()
        }
    }

    /// A window frame's allowance when `timed`, else [`Budget::untimed`].
    pub fn frame(timed: bool) -> Self {
        if timed { Self::new() } else { Self::untimed() }
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

    /// Bytes left before `spent`, zero once over, and at most an
    /// interactive frame's allowance at once: an opening frame's 64 MB as
    /// one texture band was one write of 10 to 60 ms past its deadline,
    /// which is checked between pieces.
    pub fn room(&self) -> usize {
        self.limit.saturating_sub(self.bytes).min(UPLOAD_BYTES)
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

/// IEEE half-float bits of `v`, rounded to nearest; overflow saturates to
/// infinity and values below the smallest normal flush to zero, which a
/// sky's radiance never comes near.
fn half(v: f32) -> u16 {
    let bits = v.to_bits();
    let sign = ((bits >> 16) & 0x8000) as u16;
    let exp = ((bits >> 23) & 0xff) as i32 - 127 + 15;
    if v.is_nan() {
        return sign | 0x7e00;
    }
    if exp <= 0 {
        return sign;
    }
    if exp >= 31 {
        return sign | 0x7c00;
    }
    let mantissa = bits & 0x7f_ffff;
    // Round half up on the 13 dropped bits; a carry out of the mantissa
    // bumps the exponent, which is the right result.
    let h = ((exp as u32) << 10 | mantissa >> 13) + ((mantissa >> 12) & 1);
    sign | (h.min(0x7c00) as u16)
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
    textures: HashMap<usize, Arc<StreamedTexture>>,
    heights: HashMap<usize, Arc<ResidentHeights>>,
    /// Displaced meshes' color maps.
    color_maps: HashMap<usize, Arc<ResidentControl>>,
    /// Bumped whenever a height map edit reaches the GPU, so the shadow
    /// map knows to redraw.
    height_edits: u64,
    /// Where those edits landed: `(height_edits, the map's key, samples)`,
    /// the last `terrain::MOVES_KEPT`, so a shadow map can redraw just there
    /// (`Device::ground_moved`).
    height_moves: terrain::HeightMoves,
    /// Height edits update page selection as well as baked texels.
    ground_edits: Vec<terrain::GroundEdit>,
    /// Bumped by every upload that changes what a view's opaque layers
    /// draw with in place (edits to heights, paint, tints and cloud maps,
    /// a new scene, the impostor bake), so views kept still draw them
    /// again (`LayerKey`).
    writes: u64,
    /// Overrides the device's BC support, for tests of the decode path.
    bc: Option<bool>,
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
    /// hold the textures and maps. References only go when a scene is
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
        self.textures.retain(|_, t| Arc::strong_count(t) > 1);
        self.heights.retain(|_, h| Arc::strong_count(h) > 1);
        self.color_maps.retain(|_, c| Arc::strong_count(c) > 1);
    }

    /// Height and colour dabs land on the next frame.
    fn sync_heights(&mut self, frame: &mut terrain::GroundPrepare) {
        let gpu = frame.gpu;
        let queue = &gpu.queue;
        let mut changed = false;
        let serial = self.height_edits + 1;
        for (&map, heights) in &self.heights {
            if let Some(rect) = heights.sync(queue) {
                changed = true;
                (self.ground_edits).push(terrain::GroundEdit::Heights(rect));
                self.height_moves.push(serial, map, rect);
            }
        }
        // The mounted ground tracks its own displaced shadow separately.
        self.height_edits += u64::from(changed);
        let mut colored = false;
        for colors in self.color_maps.values() {
            colored |= colors.sync(queue).is_some();
        }
        self.writes += u64::from(changed || colored);
    }

    fn bc(&self, gpu: &Gpu) -> bool {
        self.bc.unwrap_or_else(|| bc_supported(&gpu.device))
    }

    /// The GPU copy of a mesh's `source` texture. `managed` ones are the
    /// arena's, which keep only the levels views sample (residency.rs);
    /// they key apart (`Arc` addresses are even), so a texture both a
    /// placement and a scene batch draw is uploaded once each way.
    fn texture(&mut self, gpu: &Gpu, source: &Image, managed: bool) -> Arc<StreamedTexture> {
        let bc = self.bc(gpu);
        self.textures
            .entry(image_key(source) | usize::from(managed))
            .or_insert_with(|| {
                let texture = if managed {
                    StreamedTexture::managed(&gpu.device, source.clone(), bc)
                } else {
                    StreamedTexture::new(&gpu.device, vec![source.clone()], false, bc)
                };
                Arc::new(texture.expect("validated mesh texture"))
            })
            .clone()
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
            .filter(|_| {
                matches!(
                    data.material,
                    Material::Lit | Material::Blend(_) | Material::Decal(_)
                )
            })
            .map(|t| self.texture(gpu, t, !geometry));
        let lit = matches!(data.material, Material::Lit | Material::Decal(_));
        let mut map = |image: &Option<Image>| {
            (image.as_ref())
                .filter(|_| lit)
                .map(|t| self.texture(gpu, t, !geometry))
        };
        let maps = [map(&data.normal_map), map(&data.surface_map)];
        let alpha = (data.alpha_map.as_ref())
            .filter(|_| matches!(data.material, Material::Decal(_)))
            .map(|t| self.texture(gpu, t, !geometry));
        // A family's slots, for the arena's array or, for the kit's meshes,
        // their own bindings (pieces.wgsl).
        let slots: Vec<_> = (data.slots.iter())
            .filter(|_| matches!(data.material, Material::Lit))
            .map(|t| self.texture(gpu, t, !geometry))
            .collect();
        // Only the arena draws a mesh's splat.
        let layers = match &data.splat {
            Some(s) if !geometry && matches!(data.material, Material::Lit) => {
                let mut layers = ResidentLayers {
                    control: self.texture(gpu, &s.control, false),
                    albedo: Default::default(),
                    normal: Default::default(),
                };
                for (k, layer) in s.layers.iter().enumerate() {
                    let Some(layer) = layer else { continue };
                    layers.albedo[k] = Some(self.texture(gpu, &layer.albedo, false));
                    layers.normal[k] = (layer.normal.as_ref()).map(|n| self.texture(gpu, n, false));
                }
                Some(Box::new(layers))
            }
            _ => None,
        };
        let heights = data.heights.as_ref().map(|map| {
            self.heights
                .entry(key(map))
                .or_insert_with(|| Arc::new(ResidentHeights::new(gpu, map.clone())))
                .clone()
        });
        let colors = data.color_map.as_ref().map(|map| {
            self.color_maps
                .entry(key(map))
                .or_insert_with(|| {
                    // `stream` fills it and charges what it writes.
                    Arc::new(ResidentControl::new(
                        &gpu.device,
                        "mesh color map",
                        map.clone(),
                        Kept::Rgba,
                    ))
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
                f32::from(data.specular.0),
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
            edge: [0.; 4],
            edge2: [0.; 4],
            glow: data.emissive.map_or([0.; 4], |[r, g, b]| {
                [r, g, b, f32::from(u8::from(maps[0].is_some()))]
            }),
            alpha: [data.alpha_ref.unwrap_or(0.5), 0., 0., 0.],
        };
        if let Material::Blend(blend) = &data.material {
            let [r, g, b] = rgb(blend.color);
            params.surface = [r, g, b, blend.opacity];
            // w: 0 lit, 1 additive, 2 unlit.
            let mode = if blend.additive {
                1.
            } else if blend.unlit {
                2.
            } else {
                0.
            };
            params.blend = [blend.fade, blend.fresnel, blend.depth, mode];
            // vs_mesh hands the vertex color's top byte on as transparency.
            params.extra[3] = 1.;
        }
        if let Material::Decal(decal) = &data.material {
            // Decals blend nothing behind them: blend.x says an alpha map
            // is bound.
            params.surface[3] = decal.opacity;
            params.blend[0] = f32::from(u8::from(alpha.is_some()));
            (params.edge, params.edge2) = DecalEdge::code(decal.edge.as_ref());
        }
        if lit {
            params.blend[1] = tiling_code(data.world_tiling);
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
        let params = buffer_init(
            gpu,
            "mesh params",
            bytemuck::bytes_of(&params),
            wgpu::BufferUsages::UNIFORM,
        );
        // pieces.wgsl PieceFamily: the family, its slot count, then the
        // payload, in a vec4-aligned array.
        let family = {
            let mut words = [0u32; 4 + MAX_PAYLOAD];
            if matches!(data.material, Material::Lit) {
                words[0] = u32::from(data.family.0);
                words[1] = slots.len() as u32;
                words[4..4 + data.payload.len()].copy_from_slice(&data.payload);
            }
            buffer_init(
                gpu,
                "mesh family",
                bytemuck::cast_slice(&words),
                wgpu::BufferUsages::UNIFORM,
            )
        };
        let d = &kit.dummy;
        fn texture<'a>(
            t: Option<&'a wgpu::TextureView>,
            fallback: &'a wgpu::TextureView,
        ) -> wgpu::BindingResource<'a> {
            wgpu::BindingResource::TextureView(t.unwrap_or(fallback))
        }
        // Meshes the arena draws never draw with this group: binding their
        // textures would hold device textures residency lets go.
        let bound =
            |t: &Option<Arc<StreamedTexture>>| t.as_ref().filter(|_| geometry).map(|t| t.view());
        let [albedo_view, normal_view, surface_view, alpha_view] =
            [&albedo, &maps[0], &maps[1], &alpha].map(bound);
        let entries = [
            entry(0, params.as_entire_binding()),
            entry(1, vertex_buffer.as_entire_binding()),
            entry(2, index_buffer.as_entire_binding()),
            entry(3, texture(albedo_view.as_ref(), &d.white)),
            entry(10, texture(heights.as_ref().map(|h| &h.view), &d.heights)),
            entry(11, texture(colors.as_ref().map(|c| &c.view), &d.white)),
            entry(
                12,
                texture(heights.as_ref().map(|h| &h.slopes_view), &d.white),
            ),
            entry(13, texture(normal_view.as_ref(), &d.white)),
            entry(14, texture(surface_view.as_ref(), &d.white)),
            entry(15, texture(alpha_view.as_ref(), &d.white)),
            entry(23, family.as_entire_binding()),
        ];
        let slot_views: Vec<Option<wgpu::TextureView>> = slots
            .iter()
            .map(|t| Some(t).filter(|_| geometry).map(|t| t.view()))
            .collect();
        let entries: Vec<wgpu::BindGroupEntry<'_>> = entries
            .into_iter()
            .chain((0..MAX_SLOTS).map(|k| {
                let view = slot_views.get(k).and_then(Option::as_ref);
                entry(24 + k as u32, texture(view, &d.white))
            }))
            .collect();
        let group = gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("mesh"),
            layout: &kit.mesh_layout,
            entries: &entries,
        });
        let patch = heights.as_ref().map(|_| {
            gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("terrain patch"),
                layout: &kit.patch_layout,
                entries: &entries
                    .iter()
                    .filter(|e| terrain::patch_binding(e.binding))
                    .cloned()
                    .collect::<Vec<_>>(),
            })
        });
        let mesh = Arc::new(ResidentMesh {
            source: source.clone(),
            count: source.index_count() as u32,
            indices: index_buffer.clone(),
            albedo,
            maps,
            alpha,
            slots,
            layers,
            heights,
            colors,
            patch,
            group,
        });
        self.meshes.insert(slot, mesh.clone());
        mesh
    }

    /// Streams `mesh`'s textures within the budget. True once all are complete.
    fn stream(&mut self, gpu: &Gpu, mesh: &ResidentMesh, budget: &mut Budget) -> bool {
        // Colour and height maps first: the mesh draws only once they are
        // full.
        if (mesh.colors.as_ref()).is_some_and(|c| !c.fill(&gpu.queue, budget)) {
            return false;
        }
        if mesh
            .heights
            .as_ref()
            .is_some_and(|h| !h.fill(&gpu.queue, budget))
        {
            return false;
        }
        let layers = (mesh.layers.iter()).flat_map(|l| {
            std::iter::once(&l.control)
                .chain(l.albedo.iter().flatten())
                .chain(l.normal.iter().flatten())
        });
        (mesh.albedo.iter())
            .chain(mesh.maps.iter().flatten())
            .chain(&mesh.alpha)
            .chain(&mesh.slots)
            .chain(layers)
            .all(|texture| texture.stream(&gpu.queue, budget))
    }
}

/// All of `texture` for a copy: depth copies must be whole textures.
fn whole(texture: &wgpu::Texture) -> wgpu::TexelCopyTextureInfo<'_> {
    wgpu::TexelCopyTextureInfo {
        texture,
        mip_level: 0,
        origin: wgpu::Origin3d::ZERO,
        aspect: wgpu::TextureAspect::All,
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
    /// `MeshData::alpha_map` of a decal.
    alpha: Option<Arc<StreamedTexture>>,
    /// `MeshData::slots`, for the arena.
    slots: Vec<Arc<StreamedTexture>>,
    /// `MeshData::splat`, for the arena.
    layers: Option<Box<ResidentLayers>>,
    /// Held so `Assets::collect` keeps the height and color maps the bind
    /// group reads; the frame group binds the presented terrain's heights
    /// for terrain-adapting placements.
    heights: Option<Arc<ResidentHeights>>,
    colors: Option<Arc<ResidentControl>>,
    patch: Option<wgpu::BindGroup>,
    /// Material, geometry and textures (bind group 1). Holds its buffers.
    group: wgpu::BindGroup,
}

/// A [`MeshSplat`]'s textures, every level kept: residency sizes a slot by
/// the mesh's UVs, which tiling layers outrun, and the control map is
/// sampled with them. The control map, then per channel its layer's
/// albedo and normal map.
struct ResidentLayers {
    control: Arc<StreamedTexture>,
    albedo: [Option<Arc<StreamedTexture>>; 4],
    normal: [Option<Arc<StreamedTexture>>; 4],
}

impl ResidentMesh {
    /// Draws `instances` of the mesh with the pass's bound pipeline and
    /// groups.
    fn draw(&self, pass: &mut wgpu::RenderPass<'_>, instances: std::ops::Range<u32>) {
        pass.set_index_buffer(self.indices.slice(..), wgpu::IndexFormat::Uint32);
        pass.draw_indexed(0..self.count, 0, instances);
    }

    /// `draw` of the triangles at `indices` alone.
    fn draw_indices(
        &self,
        pass: &mut wgpu::RenderPass<'_>,
        indices: std::ops::Range<u32>,
        instances: std::ops::Range<u32>,
    ) {
        pass.set_index_buffer(self.indices.slice(..), wgpu::IndexFormat::Uint32);
        pass.draw_indexed(indices, 0, instances);
    }
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

/// What drawing placements from one arena needs (placed.rs): texture
/// arrays indexed per fragment. Its indirect draws start at instance and
/// vertex zero (`placed::ARGS`), so it needs no `INDIRECT_FIRST_INSTANCE`.
/// The kit's device asks for them where offered (gpu/device.rs).
pub(crate) const ARENA_FEATURES: wgpu::Features = wgpu::Features::TEXTURE_BINDING_ARRAY
    .union(wgpu::Features::SAMPLED_TEXTURE_AND_STORAGE_BUFFER_ARRAY_NON_UNIFORM_INDEXING);

/// The arena's texture slots on `device`: 2048 at most, which a 768 m
/// island's pieces use a few hundred of.
fn arena_slots(device: &wgpu::Device) -> u32 {
    device
        .limits()
        .max_binding_array_elements_per_shader_stage
        .min(2048)
}

/// Whether the renderer draws 3D on `device`: placements reach the screen
/// only through the arena and the visibility buffer, which need texture
/// arrays indexed per fragment and the storage buffers the resolve binds.
/// Without them a viewport draws nothing (GL, and the oldest Vulkan
/// drivers), and its host should say so where the scene would be.
pub fn supported(device: &wgpu::Device) -> bool {
    let limits = device.limits();
    device.features().contains(ARENA_FEATURES)
        && arena_slots(device) >= 256
        && limits.max_storage_buffers_per_shader_stage >= visbuf::RESOLVE_STORAGE_BUFFERS
        // WebGPU's floors, 8 and 32: every adapter wgpu reports meets them.
        && limits.max_color_attachments >= visbuf::REVEALED_ATTACHMENTS
        && limits.max_color_attachment_bytes_per_sample >= visbuf::REVEALED_BYTES
}

/// Whether the device samples BC textures: the kit's device asks for the
/// feature where the adapter has it (gpu/device.rs).
pub fn bc_supported(device: &wgpu::Device) -> bool {
    device
        .features()
        .contains(wgpu::Features::TEXTURE_COMPRESSION_BC)
}

/// A device texture filled from its images a band of rows at a time, every
/// mip of a layer before the next layer, within a frame's `Budget`. Draw
/// with it only once `stream` reports it complete. Placement textures in
/// the arena keep only the levels views sample (`StreamedTexture::managed`,
/// residency.rs): their device texture is made again, smaller or larger,
/// as the camera moves, so callers bind `view()` afresh rather than keep it.
pub struct StreamedTexture {
    images: Vec<Image>,
    layout: Layout,
    /// Compressed images upload decoded to RGBA, band by band: the device
    /// cannot sample BC, or the images mix formats.
    decode: bool,
    format: wgpu::TextureFormat,
    array: bool,
    live: Mutex<Live>,
}

/// What the device holds of a `StreamedTexture`.
struct Live {
    texture: wgpu::Texture,
    view: wgpu::TextureView,
    /// The layout level the texture's level 0 holds: 0 unless residency
    /// left finer levels out.
    base: u32,
    /// Next layer, level of `texture` and band row of the first fill. A
    /// row is one texel row of an RGBA image, or one row of 4x4 blocks of
    /// a compressed one.
    progress: (u32, u32, u32),
    /// A placement texture's residency; `None` keeps every level.
    managed: Option<residency::Managed>,
}

impl StreamedTexture {
    /// Fails unless `images` can share one texture array (`new` with
    /// `array`): one size from some mip on, and RGBA images at it.
    pub fn check(images: &[Image]) -> Result<()> {
        Layout::new(images).map(drop)
    }

    /// An empty texture for validated `images` that fit one `Layout`, with a
    /// `D2Array` view when `array`. Without `bc` compressed images decode
    /// to RGBA band by band.
    pub fn new(device: &wgpu::Device, images: Vec<Image>, array: bool, bc: bool) -> Result<Self> {
        Self::made(device, images, array, bc, false)
    }

    /// A placement texture for the arena: starts with its coarse levels
    /// only and holds what the views' culls ask for (residency.rs). One
    /// compressed image with mips; others keep every level as `new` does.
    pub(crate) fn managed(device: &wgpu::Device, image: Image, bc: bool) -> Result<Self> {
        Self::made(device, vec![image], false, bc, true)
    }

    fn made(
        device: &wgpu::Device,
        images: Vec<Image>,
        array: bool,
        bc: bool,
        managed: bool,
    ) -> Result<Self> {
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
        // Said once each: without BC every compressed texture takes four to
        // eight times its size; a few shipped ones are not whole blocks (a
        // 94x94 placement texture on the pirate island, 47 KB as RGBA).
        if layout.block.is_some() && !bc {
            static WARNED: std::sync::Once = std::sync::Once::new();
            WARNED.call_once(|| {
                eprintln!("no BC texture support here; decoding compressed textures to RGBA");
            });
        } else if layout.block.is_some() && !aligned {
            static WARNED: std::sync::Once = std::sync::Once::new();
            let (w, h) = (layout.width, layout.height);
            WARNED.call_once(|| {
                eprintln!("decoding a {w}x{h} compressed texture to RGBA: not whole 4x4 blocks");
            });
        }
        // Unorm, not sRGB: game albedo is displayed as stored, matching how
        // GPUI composites its own images.
        let format = block.map_or(wgpu::TextureFormat::Rgba8Unorm, block_format);
        let managed = (managed && !array && layout.levels > 1)
            .then(|| residency::Managed::new(&layout, block.is_some()));
        let base = managed.as_ref().map_or(0, |m| m.coarsest);
        let shape = TextureShape {
            layout: &layout,
            layers: images.len() as u32,
            format,
            array,
            managed: managed.is_some(),
        };
        let (texture, view) = shape.create(device, base);
        Ok(Self {
            images,
            decode: block.is_none(),
            format,
            array,
            live: Mutex::new(Live {
                texture,
                view,
                base,
                progress: (0, 0, 0),
                managed,
            }),
            layout,
        })
    }

    fn shape(&self) -> TextureShape<'_> {
        TextureShape {
            layout: &self.layout,
            layers: self.images.len() as u32,
            format: self.format,
            array: self.array,
            managed: true,
        }
    }

    /// View to bind once `stream` reports completion. A managed texture's
    /// changes as its levels come and go: bind it afresh.
    pub fn view(&self) -> wgpu::TextureView {
        self.live.lock_any().view.clone()
    }

    /// Uploads bands until done or the budget is spent. True once complete.
    pub fn stream(&self, queue: &wgpu::Queue, budget: &mut Budget) -> bool {
        let mut live = self.live.lock_any();
        let Live {
            texture,
            base,
            progress,
            managed,
            ..
        } = &mut *live;
        while let Some(image) = self.images.get(progress.0 as usize) {
            if budget.spent() {
                return false;
            }
            let (layer, level, row) = *progress;
            let held;
            let data = match image {
                Image::Rgba(t) => t.rgba.as_slice(),
                Image::Compressed(c) => {
                    c.mips[(self.layout.skips[layer as usize] + *base + level) as usize].as_slice()
                }
                Image::Releasable(r) => {
                    let at = (self.layout.skips[layer as usize] + *base + level) as usize;
                    // A placement texture fills from its kept coarse levels
                    // without reading the rest.
                    match r.tail_level(at).filter(|_| managed.is_some()) {
                        Some(data) => data,
                        None => {
                            held = r.texels();
                            let Some(c) = &held else {
                                // Gone from disk since: the layer stays empty.
                                *progress = (layer + 1, 0, 0);
                                continue;
                            };
                            c.mips[at].as_slice()
                        }
                    }
                }
            };
            let (count, rows) = self.band(
                queue,
                texture,
                level,
                layer,
                *base + level,
                row,
                data,
                budget,
            );
            *progress = if row + count < rows {
                (layer, level, row + count)
            } else if *base + level + 1 < self.layout.levels {
                (layer, level + 1, 0)
            } else {
                // The layer is on the GPU: its texels may go, but for a
                // managed texture's, which residency lets go once views
                // stop asking for more of it.
                if let Image::Releasable(r) = image
                    && managed.is_none()
                {
                    r.release();
                    RELEASED.store(true, std::sync::atomic::Ordering::Relaxed);
                }
                (layer + 1, 0, 0)
            };
        }
        true
    }

    /// Band rows of layout level `level`: texel rows, or rows of 4x4 blocks.
    fn rows(&self, level: u32) -> u32 {
        let height = (self.layout.height >> level).max(1);
        if self.images[0].block().is_some() {
            height.div_ceil(4)
        } else {
            height
        }
    }

    /// Writes as many band rows of layout level `level` of `layer`, from
    /// band row `row`, as the budget has room for (one at least), into
    /// `texture`'s level `into`; `data` is that level's texels. Returns
    /// the rows written and the level's rows.
    #[allow(clippy::too_many_arguments)]
    fn band(
        &self,
        queue: &wgpu::Queue,
        texture: &wgpu::Texture,
        into: u32,
        layer: u32,
        level: u32,
        row: u32,
        data: &[u8],
        budget: &mut Budget,
    ) -> (u32, u32) {
        let image = &self.images[layer as usize];
        let width = (self.layout.width >> level).max(1);
        let height = (self.layout.height >> level).max(1);
        // Texel rows per band row, and source bytes per band row.
        let (texels, source_row) = match image.block() {
            None => (1, width as usize * 4),
            Some(block) => (4, block.size(width, 1)),
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
                texture,
                mip_level: into,
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
        (count, rows)
    }
}

/// What a `StreamedTexture`'s device texture is made of, at any base level.
struct TextureShape<'a> {
    layout: &'a Layout,
    layers: u32,
    format: wgpu::TextureFormat,
    array: bool,
    managed: bool,
}

impl TextureShape<'_> {
    /// A device texture holding layout levels `base..` (both sides halved
    /// `base` times), empty. Managed ones are also copied from, as their
    /// levels move between textures.
    fn create(&self, device: &wgpu::Device, base: u32) -> (wgpu::Texture, wgpu::TextureView) {
        let mut usage = wgpu::TextureUsages::COPY_DST | wgpu::TextureUsages::TEXTURE_BINDING;
        if self.managed {
            usage |= wgpu::TextureUsages::COPY_SRC;
        }
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some(if self.managed {
                "editor placed texture"
            } else {
                "editor texture"
            }),
            size: wgpu::Extent3d {
                width: (self.layout.width >> base).max(1),
                height: (self.layout.height >> base).max(1),
                depth_or_array_layers: self.layers,
            },
            mip_level_count: self.layout.levels - base,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: self.format,
            usage,
            view_formats: &[],
        });
        let view = texture.create_view(&wgpu::TextureViewDescriptor {
            dimension: Some(if self.array {
                wgpu::TextureViewDimension::D2Array
            } else {
                wgpu::TextureViewDimension::D2
            }),
            ..Default::default()
        });
        (texture, view)
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

/// A pass that only clears `view` to transparent.
fn clear_target(encoder: &mut wgpu::CommandEncoder, label: &str, view: &wgpu::TextureView) {
    encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
        label: Some(label),
        color_attachments: &[Some(wgpu::RenderPassColorAttachment {
            view,
            resolve_target: None,
            ops: wgpu::Operations {
                load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                store: wgpu::StoreOp::Store,
            },
            depth_slice: None,
        })],
        depth_stencil_attachment: None,
        timestamp_writes: None,
        occlusion_query_set: None,
        multiview_mask: None,
    });
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
    /// Kept beside its view for tests to read back.
    #[cfg_attr(not(test), allow(dead_code))]
    texture: wgpu::Texture,
    view: wgpu::TextureView,
    resolution: u32,
    /// The scene generation, height edits and placement version drawn, and
    /// each casting pass's place in the scene with its caster version.
    drawn: Option<ShadowVersion>,
    /// The fit it was drawn with: a partial redraw keeps the rest.
    fit: Option<light::ShadowFit>,
    held: Option<HeldShadow>,
}

type ShadowVersion = (u64, u64, u64, Vec<(usize, u64)>);

/// Ground moves a map skipped under a `ShadowHold`: the version it has
/// accounted for, and the world box they moved, which it draws once the
/// hold drops. Taken frame by frame, so a long stroke outlasting the
/// ground's move logs (`MOVES_KEPT`) still draws in part.
#[derive(Clone)]
struct HeldShadow {
    at: ShadowVersion,
    region: [Vec3; 2],
}

/// A view's shadow cascades (`Shadows::cascades`), a layer each, drawn again
/// when its camera or a caster moved.
struct Cascades {
    #[cfg_attr(not(test), allow(dead_code))]
    texture: wgpu::Texture,
    /// Every layer, as `t_cascades` samples them.
    view: wgpu::TextureView,
    layers: Vec<wgpu::TextureView>,
    resolution: u32,
    /// Per layer, the fit and casters it was last drawn with.
    drawn: Vec<Option<(light::ShadowFit, ShadowVersion)>>,
    /// Per layer, what it skipped under a `ShadowHold`.
    held: Vec<Option<HeldShadow>>,
    /// The view's clip rows last frame: whether its camera moves.
    rows: Option<[[f64; 4]; 4]>,
    /// Some layer is kept a size step coarser than wanted, as a moving
    /// view allows: the view wants another frame, at rest, to draw it.
    relaxed: bool,
    /// `Detail::reach` the layers were culled under.
    reach: Option<Reach>,
}

/// Shadow maps and cascades: drawn, then sampled; tests also read them back.
const SHADOW_USAGE: wgpu::TextureUsages = wgpu::TextureUsages::RENDER_ATTACHMENT
    .union(wgpu::TextureUsages::TEXTURE_BINDING)
    .union(if cfg!(test) {
        wgpu::TextureUsages::COPY_SRC
    } else {
        wgpu::TextureUsages::empty()
    });

/// Cascades are drawn this much wider than the view needs, with as many
/// more texels, so they keep serving while it moves a little: each cost
/// 0.7 ms at 5K and all three were redrawn on every moving frame. One
/// quarter octave, the step fits round their size to: the drawn map then
/// has the wanted one's texel size and grid, and shades the same.
const CASCADE_MARGIN: f64 = 1.189_207_115;

/// Bindings for slots a draw does not use.
#[derive(Clone)]
struct Dummies {
    buffer: wgpu::Buffer,
    /// Stands in for storage a pass may write and nothing reads: `buffer`
    /// is bound read-only beside it, and one buffer cannot be both in a
    /// dispatch.
    written: wgpu::Buffer,
    white: wgpu::TextureView,
    black: wgpu::TextureView,
    array: wgpu::TextureView,
    heights: wgpu::TextureView,
    page_index: wgpu::TextureView,
    depth: wgpu::TextureView,
    cascades: wgpu::TextureView,
    /// A black cube, for `t_environment`.
    cube: wgpu::TextureView,
}

/// Bind group layouts, samplers and dummies: what pipelines and meshes are
/// built against, the same for every target format.
#[derive(Clone)]
struct Kit {
    frame_layout: wgpu::BindGroupLayout,
    mesh_layout: wgpu::BindGroupLayout,
    patch_layout: wgpu::BindGroupLayout,
    draw_layout: wgpu::BindGroupLayout,
    cull_layout: wgpu::BindGroupLayout,
    /// Placement transforms and visible entries for placed.wgsl.
    placed_layout: wgpu::BindGroupLayout,
    /// Depth pyramid steps (hzb.wgsl): from the depth target, and level to
    /// level.
    hzb_depth_layout: wgpu::BindGroupLayout,
    hzb_down_layout: wgpu::BindGroupLayout,
    /// The placement arena's layout and texture slots, where the device
    /// draws 3D at all ([`supported`]).
    arena: Option<(wgpu::BindGroupLayout, u32)>,
    /// Baked impostor views (placed.wgsl group 1).
    atlas_layout: wgpu::BindGroupLayout,
    /// The raw occlusion texture, for the pass that blurs and applies it.
    occlusion_layout: wgpu::BindGroupLayout,
    /// The HDR target and the view's exposure, for the post pass.
    post_layout: wgpu::BindGroupLayout,
    /// Reads the depth under the pick cursor (`PICK_DEPTH_WGSL`).
    pick_depth: (wgpu::BindGroupLayout, wgpu::ComputePipeline),
    sampler: wgpu::Sampler,
    /// Clamped, for ground pages.
    pages: wgpu::Sampler,
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

/// The depth under the pick cursor, for its readback: depth textures copy
/// only whole, so the pick target holds the handle alone and this reads
/// the depth beside it. `picked` holds the pixel in, its depth's bits out.
const PICK_DEPTH_WGSL: &str = "
@group(0) @binding(0) var t_depth: texture_depth_2d;
@group(0) @binding(1) var<storage, read_write> picked: array<u32, 2>;

@compute @workgroup_size(1)
fn cs_pick_depth() {
    let at = vec2<i32>(i32(picked[0]), i32(picked[1]));
    picked[1] = bitcast<u32>(textureLoad(t_depth, at, 0));
}
";

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
        let sheets = wgpu::TextureViewDimension::D2Array;
        let cap = arena_slots(d);
        let arenas = supported(d);
        if !arenas {
            // Once a process: offscreen tests make a device each.
            static LOGGED: std::sync::Once = std::sync::Once::new();
            LOGGED.call_once(|| {
                eprintln!(
                    "this GPU can't draw the 3D view: it lacks binding arrays with non-uniform indexing"
                )
            });
        }
        // The visibility buffer's compute resolve shades placements as the
        // fragment stages do: whatever shading reads is bound for compute too.
        let shading = if arenas { S::COMPUTE } else { S::NONE };
        let frag = S::FRAGMENT | shading;
        let frame_layout = d.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("editor frame"),
            entries: &[
                layout_entry(0, both | shading, uniform(false)),
                layout_entry(1, both | shading, uniform(false)),
                // Both: relief displaces terrain vertices by the ground.
                layout_entry(
                    2,
                    both | shading,
                    wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                ),
                layout_entry(3, frag, depth_texture()),
                layout_entry(
                    4,
                    frag,
                    wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Comparison),
                ),
                layout_entry(5, frag, depth_texture()),
                layout_entry(6, both | shading, storage(true)),
                layout_entry(7, S::VERTEX, storage(true)),
                layout_entry(8, frag, sampled(false, d2)),
                // R32Float is not filterable everywhere; heights are loaded,
                // also by impostors lit by the ground's normal.
                layout_entry(9, both | shading, sampled(false, d2)),
                layout_entry(10, frag, sampled(true, d2)),
                layout_entry(11, frag, sampled(true, d2)),
                layout_entry(12, frag, sampled(true, d2)),
                layout_entry(
                    13,
                    frag,
                    wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Depth,
                        view_dimension: wgpu::TextureViewDimension::D2Array,
                        multisampled: false,
                    },
                ),
                layout_entry(
                    14,
                    frag,
                    wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Uint,
                        view_dimension: d2,
                        multisampled: false,
                    },
                ),
                // Ground pages' sheets (terrain.rs PAGE_SHEET_SIDE).
                layout_entry(15, frag, sampled(true, sheets)),
                layout_entry(16, frag, sampled(true, sheets)),
                layout_entry(
                    17,
                    frag,
                    wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                ),
                layout_entry(18, frag, sampled(true, d2)),
                layout_entry(19, frag, sampled(false, d2)),
                layout_entry(20, frag, sampled(true, sheets)),
                layout_entry(21, frag, sampled(true, wgpu::TextureViewDimension::Cube)),
                layout_entry(22, frag, sampled(true, sheets)),
                layout_entry(23, frag, sampled(true, d2)),
            ],
        });
        let mesh_entries = [
            layout_entry(0, both, uniform(false)),
            layout_entry(1, both, storage(true)),
            layout_entry(2, both, storage(true)),
            layout_entry(3, S::FRAGMENT, sampled(true, d2)),
            // R32Float is not filterable everywhere; heights are loaded,
            // by fs_page_bake too.
            layout_entry(10, both, sampled(false, d2)),
            // The color map: vs_mesh loads it, ground fragments filter it.
            layout_entry(11, both, sampled(true, d2)),
            // The heights' slopes (heights.rs ResidentHeights::slopes).
            layout_entry(12, S::FRAGMENT, sampled(true, d2)),
            layout_entry(13, S::FRAGMENT, sampled(true, d2)),
            layout_entry(14, S::FRAGMENT, sampled(true, d2)),
            layout_entry(15, S::FRAGMENT, sampled(true, d2)),
            // The kit's meshes: the surface family, its payload and slots
            // (pieces.wgsl).
            layout_entry(23, S::FRAGMENT, uniform(false)),
            layout_entry(24, S::FRAGMENT, sampled(true, d2)),
            layout_entry(25, S::FRAGMENT, sampled(true, d2)),
            layout_entry(26, S::FRAGMENT, sampled(true, d2)),
            layout_entry(27, S::FRAGMENT, sampled(true, d2)),
            layout_entry(28, S::FRAGMENT, sampled(true, d2)),
            layout_entry(29, S::FRAGMENT, sampled(true, d2)),
            layout_entry(30, S::FRAGMENT, sampled(true, d2)),
            layout_entry(31, S::FRAGMENT, sampled(true, d2)),
        ];
        let mesh_layout = d.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("editor mesh"),
            entries: &mesh_entries,
        });
        let patch_layout = d.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("terrain patch"),
            entries: &mesh_entries
                .iter()
                .copied()
                .filter(|e| terrain::patch_binding(e.binding))
                .collect::<Vec<_>>(),
        });
        let draw_layout = d.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("editor draw"),
            entries: &[
                layout_entry(0, S::VERTEX, uniform(true)),
                layout_entry(1, S::VERTEX, storage(true)),
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
                // The arena's texel table and the view's wants of it
                // (residency.rs).
                layout_entry(13, S::COMPUTE, storage(true)),
                layout_entry(14, S::COMPUTE, storage(false)),
                // The presented ground's cell ranges (heights.rs).
                layout_entry(15, S::COMPUTE, storage(true)),
                // The posed placements' tiles (visbuf.rs `posed_tiles`).
                layout_entry(16, S::COMPUTE, storage(false)),
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
        // The visibility buffer's resolve rebuilds placed vertices per
        // pixel: per-vertex arrays are read in fragments too.
        let vertices = both | shading;
        let placed_layout = d.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("editor placed"),
            entries: &[
                // Fragments too: a splat's layers scale with the placement.
                layout_entry(0, vertices | frag, storage(true)),
                layout_entry(1, S::VERTEX, storage(true)),
                layout_entry(2, vertices, storage(true)),
                // The instance words: what families read, in fragments.
                layout_entry(3, frag, storage(true)),
                // Per draw slot, its list and where that starts.
                layout_entry(4, S::VERTEX, storage(true)),
            ],
        });
        let arena = arenas.then(|| {
            let entries = [
                layout_entry(0, both | shading, storage(true)),
                layout_entry(1, vertices, storage(true)),
                layout_entry(2, vertices, storage(true)),
                wgpu::BindGroupLayoutEntry {
                    binding: 3,
                    visibility: frag,
                    ty: sampled(true, d2),
                    count: std::num::NonZeroU32::new(cap),
                },
                // Shapes hold a family's slots and payload, which its
                // coverage reads in the resolve too.
                layout_entry(4, both | shading, storage(true)),
                layout_entry(5, vertices, storage(true)),
                layout_entry(7, vertices, storage(true)),
                // Which draw and vertex each index value names.
                layout_entry(8, vertices, storage(true)),
                // Which triangles each vertex starts (placed.rs spread_firsts).
                layout_entry(6, frag, storage(true)),
            ];
            let layout = d.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("editor placement arena"),
                entries: &entries,
            });
            (layout, cap)
        });
        let pick_depth = {
            let layout = d.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("editor pick depth"),
                entries: &[
                    layout_entry(0, S::COMPUTE, depth_texture()),
                    layout_entry(1, S::COMPUTE, storage(false)),
                ],
            });
            let module = d.create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some("editor pick depth"),
                source: wgpu::ShaderSource::Wgsl(PICK_DEPTH_WGSL.into()),
            });
            let pipeline = d.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
                label: Some("editor pick depth"),
                layout: Some(&d.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                    label: Some("editor pick depth"),
                    bind_group_layouts: &[Some(&layout)],
                    immediate_size: 0,
                })),
                module: &module,
                entry_point: Some("cs_pick_depth"),
                compilation_options: Default::default(),
                cache: None,
            });
            (layout, pipeline)
        };
        let array = wgpu::TextureViewDimension::D2Array;
        // 1: the posed placements' depth, for the apply inside the water
        // pass (fs_occlusion_apply_late); a stand-in for the rest.
        let occlusion_layout = d.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("editor occlusion"),
            entries: &[
                layout_entry(0, S::FRAGMENT, sampled(false, d2)),
                layout_entry(1, S::FRAGMENT, sampled(false, d2)),
            ],
        });
        let post_layout = d.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("editor post"),
            entries: &[
                layout_entry(0, S::FRAGMENT, sampled(false, d2)),
                layout_entry(1, S::FRAGMENT, storage(true)),
            ],
        });
        let atlas_layout = d.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("editor impostor atlas"),
            entries: &[
                // The vertex stage reads its size, for the tile's.
                layout_entry(0, S::VERTEX_FRAGMENT, sampled(true, array)),
                layout_entry(1, S::FRAGMENT, sampled(true, array)),
                layout_entry(2, S::FRAGMENT, sampled(true, array)),
                // What each view covers, which sizes the quads.
                layout_entry(
                    3,
                    S::VERTEX,
                    wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Uint,
                        view_dimension: d2,
                        multisampled: false,
                    },
                ),
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
        // Ground pages: as the albedo sampler, but clamped to each page.
        let pages = d.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("editor ground pages"),
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
            t.stream(&gpu.queue, &mut Budget::untimed());
            t.view()
        };
        let (heights, heights_view) = texture_2d(
            gpu,
            "dummy heights",
            [1, 1],
            wgpu::TextureFormat::R32Float,
            wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        );
        write_texture_rect(&gpu.queue, &heights, [0, 0], [1, 1], 4, &[0; 4]);
        // No ground pages: every entry unbaked.
        let (page_index, page_index_view) = texture_2d(
            gpu,
            "dummy page index",
            [1, 1],
            wgpu::TextureFormat::R32Uint,
            wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        );
        write_texture_rect(&gpu.queue, &page_index, [0, 0], [1, 1], 4, &[0; 4]);
        // Black, never read: `environment` reads the sky without a cube.
        let cube = gpu
            .device
            .create_texture(&wgpu::TextureDescriptor {
                label: Some("dummy environment"),
                size: wgpu::Extent3d {
                    width: 1,
                    height: 1,
                    depth_or_array_layers: 6,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Rgba8Unorm,
                usage: wgpu::TextureUsages::TEXTURE_BINDING,
                view_formats: &[],
            })
            .create_view(&wgpu::TextureViewDescriptor {
                dimension: Some(wgpu::TextureViewDimension::Cube),
                ..Default::default()
            });
        let (_, depth) = texture_2d(
            gpu,
            "dummy depth",
            [1, 1],
            wgpu::TextureFormat::Depth32Float,
            wgpu::TextureUsages::TEXTURE_BINDING,
        );
        let cascades = gpu
            .device
            .create_texture(&wgpu::TextureDescriptor {
                label: Some("dummy cascades"),
                size: wgpu::Extent3d {
                    width: 1,
                    height: 1,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Depth32Float,
                usage: wgpu::TextureUsages::TEXTURE_BINDING,
                view_formats: &[],
            })
            .create_view(&wgpu::TextureViewDescriptor {
                dimension: Some(wgpu::TextureViewDimension::D2Array),
                ..Default::default()
            });
        let dummy = Dummies {
            // At least one of whatever it stands in for: the cull's
            // texel records are 80 bytes.
            buffer: buffer_init(
                gpu,
                "dummy",
                bytemuck::cast_slice(&[math::IDENTITY; 2]),
                wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::UNIFORM,
            ),
            written: buffer_init(
                gpu,
                "dummy written",
                bytemuck::cast_slice(&[math::IDENTITY; 2]),
                wgpu::BufferUsages::STORAGE,
            ),
            white: solid([255; 4], false),
            black: solid([0; 4], false),
            array: solid([255; 4], true),
            heights: heights_view,
            page_index: page_index_view,
            depth,
            cascades,
            cube,
        };
        Self {
            frame_layout,
            mesh_layout,
            patch_layout,
            draw_layout,
            cull_layout,
            placed_layout,
            hzb_depth_layout,
            hzb_down_layout,
            arena,
            atlas_layout,
            occlusion_layout,
            post_layout,
            pick_depth,
            sampler,
            pages,
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
    /// The view's first phase: placements seen last frame.
    Seen,
    /// The view's second phase: placements the depth pyramid shows that
    /// the first phase skipped.
    Revealed,
}

#[derive(Clone, Copy, PartialEq)]
enum Phase {
    Shadow,
    Opaque,
    Blend,
    /// Markers: blended, above everything.
    Overlay,
    /// The sky: opaque, only where nothing opaque drew (depth still clear).
    Sky,
    /// Depth alone into the opaque pass's targets (cutouts still cut), for
    /// surfaces whose shading follows with `Shaded`.
    Prepass,
    /// Opaque shading where a `Prepass` left this very surface: equal
    /// depth, no depth writes. Each pixel shades once however many
    /// leaves overlap there.
    Shaded,
    /// Opaque over an earlier draw of the same surface: strictly nearer,
    /// so what that draw shaded is not shaded again.
    Over,
    /// Decals on the ground, in the opaque pass between the ground and the
    /// models (`gpu::Decal::ground`): blended into the color, no depth
    /// written.
    Decal,
}

/// GPU state the viewports share: pipelines, the presented scene and the
/// shadow map.
struct Device {
    format: wgpu::TextureFormat,
    kit: Kit,
    /// Pipelines over the placement arena, with it (`Placing::arena`).
    arena: Option<[wgpu::RenderPipeline; 12]>,
    /// The visibility buffer's, with the arena (visbuf.rs).
    visibility: Option<visbuf::Visibility>,
    lit: wgpu::RenderPipeline,
    flat: wgpu::RenderPipeline,
    /// Compresses ground pages and impostor views as they bake.
    bc: bc::Encoder,
    blend: wgpu::RenderPipeline,
    add: wgpu::RenderPipeline,
    /// Decal batches: over everything opaque, and on the ground
    /// (`Phase::Decal`).
    decal: wgpu::RenderPipeline,
    ground_decal: wgpu::RenderPipeline,
    marker: wgpu::RenderPipeline,
    sky: wgpu::RenderPipeline,
    /// The kit's far sea (fs_sea), at the Water stage.
    sea: wgpu::RenderPipeline,
    /// Halving depth for it, estimating occlusion, blurring it into the
    /// darkening factor, and applying that: in a pass of its own, or (4)
    /// inside the water and blend pass.
    occlusion: [wgpu::RenderPipeline; 5],
    /// The HDR target tone mapped onto the viewport's target.
    post: wgpu::RenderPipeline,
    /// The selection outline (outline.wgsl): the layouts of its marks
    /// group and of the tiles group the band adds, then its pipelines, the
    /// tile pass and the band.
    outline: (
        wgpu::BindGroupLayout,
        wgpu::BindGroupLayout,
        [wgpu::RenderPipeline; 2],
    ),
    /// kept.wgsl's layout, then its pipelines: the kept depth into the
    /// opaque pass's targets, depth with the HDR color on their own, and
    /// the HDR color into the kept copy.
    kept: (wgpu::BindGroupLayout, [wgpu::RenderPipeline; 3]),
    shadow: wgpu::RenderPipeline,
    /// cull.wgsl: the cull, then the decal lists put in row order.
    cull: [wgpu::ComputePipeline; 2],
    /// select.wgsl: its layout, then moving rows and writing their markers.
    select: (wgpu::BindGroupLayout, [wgpu::ComputePipeline; 2]),
    /// The outline's marks as of `State::outline_version`.
    /// The outline's marks, by its version and the scene's place in the
    /// target they map pixels with.
    outline_marks: Option<((u64, [u32; 3]), wgpu::Buffer)>,
    /// The scene's markers, for `markers` to hold the selection's after.
    scene_markers: Vec<GpuMarker>,
    /// What `markers` was made for: the scene generation and selection
    /// markers.
    markers_for: Option<(u64, usize)>,
    /// Depth pyramid from the depth target, and level to level.
    hzb: [wgpu::ComputePipeline; 2],
    exposure: exposure::Pipelines,
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
    /// The `State::overlay_version` uploaded.
    overlay_for: u64,
    line: wgpu::RenderPipeline,
    /// The presented sky's radiance map as `t_sky` samples it, with the
    /// map's Arc so an unchanged sky is not uploaded again. Each holds its
    /// source: compared by address alone, a source freed and another
    /// allocated in its place would keep the old texels.
    sky_map: Option<(Arc<SkyMap>, wgpu::Texture, wgpu::TextureView)>,
    /// The scene's environment cube as `t_environment` samples it, with its
    /// texels' `Arc`.
    environment: Option<(Arc<Vec<[f32; 3]>>, wgpu::Texture, wgpu::TextureView)>,
    /// The scene's host texture as `t_host` samples it, with its texels.
    host_texture: Option<(Arc<Vec<u8>>, wgpu::Texture, wgpu::TextureView)>,
    /// The mirror the view being drawn reflects (`Reflection`): its slot's
    /// HDR target, bound as `t_mirror` while the view draws and unset
    /// while the mirror itself draws into it.
    reflection: Option<wgpu::TextureView>,
    /// The presented scene's `GroundTint` map, keyed by its Arc; painted
    /// texels sync every frame.
    ground_tint: Option<(usize, ResidentControl)>,
    /// The scene's cloud shadow map, by its source.
    cloud_map: Option<(usize, ResidentControl)>,
    /// The view being drawn's [`CloudShade`], from its passes' prepare:
    /// views draw one after another.
    cloud_shade: Option<CloudShade>,
    /// The renderer's `Detail`, copied in as each draw starts.
    detail: Detail,
    /// The presented terrain's baked ground pages, which every view picks
    /// from.
    ground_pages: Option<terrain::GroundPages>,
    surface: Option<terrain::MountedGround>,
    ground_program: Option<Arc<terrain::GroundProgram>>,
    /// Ground programs this device built on the render thread rather than
    /// found warmed (`Warm::ground`).
    ground_builds: u32,
    /// Shadow maps and cascades drawn again only where the ground moved.
    partial_shadows: u32,
    /// A `ShadowHold` lives, as of this draw.
    shadows_held: bool,
    /// This frame's `Drawn::arena_draws` so far, by `ArenaPass`.
    arena_drawn: std::cell::Cell<[usize; 4]>,
    /// A plugged pass sat the last frame out while loading.
    passes_loading: bool,
    /// The family set's `shade` and `fog` (`Families::scene`), which ground
    /// programs end with.
    scene: String,
}

/// What a viewport reads back from the GPU, a frame or two later.
enum ReadKind {
    /// The pick handle and depth under `cursor`, as `view` drew them.
    Pick { cursor: Point<Pixels>, view: View },
    /// Placement counts per draw, from the view's indirect arguments.
    Counts,
    /// The culls' wants of arena `arena`'s `slots` textures, for each of
    /// `sections` (a bit each) in order.
    Wants {
        arena: u64,
        slots: usize,
        sections: u32,
        view: View,
    },
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
pub(crate) struct Timing {
    set: wgpu::QuerySet,
    resolve: wgpu::Buffer,
    /// Queries written this frame.
    used: u32,
    /// Each timed pass's name this frame, in query order.
    labels: [&'static str; TIMED_PASSES as usize],
    /// The part of a split pass (`Timing::mark`) still open: its end slot.
    open: Option<u32>,
}

/// gpu_bench's BENCH_SPLIT: stamp inside the opaque pass between what it
/// draws (`Timing::mark`), on devices that can. Off in the app: the stamps
/// are not free, and only a profile reads them.
pub static SPLIT_TIMING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Whether views at rest keep their opaque layers ([`Reuse`]). Off, every
/// frame draws them whole: for comparing images and times against it.
pub static KEEP_LAYERS: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);

/// Whether ground with pages draws what they shade and the rest apart
/// (`TerrainPass`). Off, one draw shades both: the same pixels, for
/// comparing images and times against it.
pub static SPLIT_GROUND: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);

/// Whether terrain tiles across a view's edge draw only their chunks in
/// view (`TerrainPass`). Off, every tile draws whole: the same pixels, for
/// comparing images and times against it.
pub static CHUNK_GROUND: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);

/// Room for this many timed passes per frame: two culls, shadow cull and
/// map, a cull and map per cascade, opaque, revealed, occlusion estimate
/// and apply, water, blend, post and overlay, the depth copy, and a few of
/// the plugged passes' own, and the parts SPLIT_TIMING splits passes
/// into: Albion's view split takes 34 or more. Parts past it go untimed.
const TIMED_PASSES: u32 = 48;

impl Timing {
    /// Runs `work` between two encoder timestamps under `label` when
    /// `timing` has room and the device stamps inside encoders.
    fn around<R>(
        timing: Option<&mut Timing>,
        device: &wgpu::Device,
        encoder: &mut wgpu::CommandEncoder,
        label: &'static str,
        work: impl FnOnce(&mut wgpu::CommandEncoder) -> R,
    ) -> R {
        let inside = (device.features()).contains(wgpu::Features::TIMESTAMP_QUERY_INSIDE_ENCODERS);
        let Some(timing) = timing.filter(|_| inside) else {
            return work(encoder);
        };
        let Some((begin, end)) = timing.pass(label) else {
            return work(encoder);
        };
        encoder.write_timestamp(&timing.set, begin);
        let out = work(encoder);
        encoder.write_timestamp(&timing.set, end);
        out
    }

    /// Begin and end slots for one more pass, while there is room.
    fn pass(&mut self, label: &'static str) -> Option<(u32, u32)> {
        (self.used + 2 <= TIMED_PASSES * 2).then(|| {
            self.labels[self.used as usize / 2] = label;
            self.used += 2;
            (self.used - 2, self.used - 1)
        })
    }

    /// Closes the open part of a split pass and opens `label`, or with
    /// None closes it: per-draw times inside one render pass. Off unless
    /// SPLIT_TIMING is on and the device stamps inside passes.
    fn split(
        timing: Option<&mut Timing>,
        pass: &mut wgpu::RenderPass<'_>,
        label: Option<&'static str>,
    ) {
        if let Some(t) = timing {
            t.mark(pass, label);
        }
    }

    fn mark(&mut self, pass: &mut wgpu::RenderPass<'_>, label: Option<&'static str>) {
        if !SPLIT_TIMING.load(std::sync::atomic::Ordering::Relaxed) {
            return;
        }
        if let Some(end) = self.open.take() {
            pass.write_timestamp(&self.set, end);
        }
        if let Some((begin, end)) = label.and_then(|l| self.pass(l)) {
            pass.write_timestamp(&self.set, begin);
            self.open = Some(end);
        }
    }

    pub(crate) fn render(
        &mut self,
        label: &'static str,
    ) -> Option<wgpu::RenderPassTimestampWrites<'_>> {
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
    /// The scene's color before the post stage (`HDR_FORMAT`).
    hdr: (wgpu::Texture, wgpu::TextureView),
    depth: (wgpu::Texture, wgpu::TextureView),
    /// Opaque depth as the blend and occlusion passes sample it.
    depth_copy: (wgpu::Texture, wgpu::TextureView),
    /// With posed placements, `depth_copy` keeps the rest alone, for a
    /// view at rest to draw them over again; this copy, made each frame
    /// once they are drawn, is what passes after the opaque stage read.
    /// Made on first use.
    posed_depth: Option<(wgpu::Texture, wgpu::TextureView)>,
    /// Raw ambient occlusion, before its blur.
    occlusion: (wgpu::Texture, wgpu::TextureView),
    /// Half-size depth for the occlusion estimate's steps.
    occlusion_depth: (wgpu::Texture, wgpu::TextureView),
    /// The blurred occlusion as the factor the color is multiplied by, at
    /// the same half size, beside the depth it was taken at.
    occlusion_blur: (wgpu::Texture, wgpu::TextureView),
    /// Pick handle and depth bits per pixel, written by the opaque pass.
    ids: (wgpu::Texture, wgpu::TextureView),
    /// One texel per OUTLINE_TILE square of `ids`: whether it holds a
    /// pixel of the selection. Made on the first frame with an outline.
    outline_tiles: Option<(wgpu::Texture, wgpu::TextureView)>,
    /// The visibility buffer (visbuf.wgsl), made with the first frame that
    /// draws placements.
    vis: Option<(wgpu::Texture, wgpu::TextureView)>,
    /// The visibility buffer's depth as it drew it, for its resolve after
    /// the opaque pass (visbuf.rs).
    vis_depth: Option<(wgpu::Texture, wgpu::TextureView)>,
    /// The posed placements' buffer and its depth (visbuf.rs draw_posed).
    posed_vis: Option<[(wgpu::Texture, wgpu::TextureView); 2]>,
    /// The screen tiles posed placements may cover, which the view's culls
    /// mark and the posed resolve keeps to (visbuf.rs POSED_TILE).
    posed_tiles: wgpu::Buffer,
    /// The opaque pass's linear color, RGB9E5 in 4 bytes, which the ocean
    /// refracts: the frame's own target is display values and cannot be
    /// read back.
    scene: (wgpu::Texture, wgpu::TextureView),
    cull: Option<placed::CullTargets>,
    readbacks: Vec<Readback>,
    timing: Option<Timing>,
    /// Per-draw handle slots and how many fit.
    draw_ids: Option<(wgpu::Buffer, usize)>,
    hzb: Hzb,
    /// What the post pass exposes by (Tone::auto).
    exposure: exposure::Exposure,
    /// This view's terrain tile matrices, grown as needed.
    terrain: Option<wgpu::Buffer>,
    cascades: Option<Cascades>,
    /// The ground pages this view picked (terrain.rs).
    pages: Option<terrain::PageIndex>,
    /// What of the frame's opaque layers these targets still hold.
    layers: Layers,
}

/// What a view's depth, pick handles, placement culling and occlusion
/// estimate were drawn from: the same key draws them the same, so a frame
/// with the key the targets hold keeps them ([`Reuse::Geometry`]).
/// Everything the opaque stage draws by is either here or bumps
/// `Assets::writes`; a missing input would show a stale frame, so when in
/// doubt a frame has no key and draws everything.
#[derive(Clone, PartialEq)]
struct GeometryKey {
    globals: Globals,
    scissor: [u32; 4],
    /// The presented scene, `Assets::writes` and `Placed::version`.
    versions: [u64; 3],
    /// The ready Opaque passes, by pointer, and their `Pass::version`s.
    passes: Vec<(usize, u64)>,
    /// `Lights::occlusion` and `occlusion_scale`: the estimate's.
    occlusion: [[f32; 4]; 2],
    /// `Detail::cull_key`, which the cull's lists were made under.
    detail: [u32; 4],
}

/// [`GeometryKey`] and what the opaque and occlusion stages shade with:
/// equal keys leave the same HDR target behind ([`Reuse::Whole`]).
#[derive(Clone, PartialEq)]
struct LayerKey {
    geometry: GeometryKey,
    /// The lights as the shaders read them: sun, the shadow maps' and
    /// cascades' fits, cloud shadows at this frame's clock, ground pages.
    lights: Lights,
    /// The ready passes that cast into the shadow maps, and their versions.
    casters: Vec<(usize, u64)>,
    /// The cloud shade's version: redrawn in place, its square unmoved,
    /// it changes what `lights` alone would not.
    shade: Option<u64>,
    /// Whether a Blend-stage pass applies the occlusion: then the kept HDR
    /// target holds the scene without it.
    occlusion_later: bool,
}

/// A view's opaque layers kept between frames. A camera at rest draws the
/// same layers frame after frame, while water, sky, haze, exposure and
/// overlays still move; this keeps the layers instead. Moving views pay
/// nothing for it: the copy of the shaded layer is taken only on the
/// second frame in a row with one key.
#[derive(Default)]
struct Layers {
    /// What `depth_copy`, `ids`, the cull's lists and `occlusion` hold:
    /// the last frame that drew them all.
    geometry: Option<GeometryKey>,
    /// The HDR target as it stood after the occlusion stage, and the key
    /// it was shaded under; cleared when a frame shades another.
    shaded: Option<LayerKey>,
    hdr: Option<(wgpu::Texture, wgpu::TextureView)>,
    /// With posed placements, the pick handles taken with `hdr`: those
    /// draw theirs over the kept ones every frame, and picks and the
    /// selection outline would find them where they no longer are.
    ids: Option<(wgpu::Texture, wgpu::TextureView)>,
    /// The previous frame's key.
    last: Option<LayerKey>,
}

impl Layers {
    /// What a frame under these keys can keep. `None` keys keep nothing.
    fn reuse(&self, geometry: Option<&GeometryKey>, key: Option<&LayerKey>) -> Reuse {
        if key.is_some() && self.shaded.as_ref() == key {
            Reuse::Whole
        } else if geometry.is_some() && self.geometry.as_ref() == geometry {
            Reuse::Geometry
        } else {
            Reuse::Nothing
        }
    }
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
                open: None,
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
        // The visibility buffer's compute resolve writes the opaque targets
        // as storage (visbuf.rs).
        let stored = if kit.arena.is_some() {
            U::STORAGE_BINDING
        } else {
            U::empty()
        };
        Self {
            exposure: exposure::Exposure::new(gpu),
            size,
            hdr: texture_2d(
                gpu,
                "editor hdr",
                size,
                HDR_FORMAT,
                U::RENDER_ATTACHMENT | U::TEXTURE_BINDING | stored,
            ),
            hzb,
            depth,
            // An attachment for the visibility resolve's depth test over
            // kept geometry (visbuf.rs).
            depth_copy: texture_2d(
                gpu,
                "editor depth copy",
                size,
                wgpu::TextureFormat::Depth32Float,
                U::COPY_DST | U::TEXTURE_BINDING | U::RENDER_ATTACHMENT,
            ),
            posed_depth: None,
            // Half resolution: the estimate is blurred over 8x8 pixels anyway,
            // and at full size it cost 0.3 ms of a 1080p frame. Each texel
            // keeps its depth beside the estimate, from which the blur's
            // same-surface test takes its world point again: the point
            // itself took twice the memory (36 MB at 5K).
            occlusion: texture_2d(
                gpu,
                "editor occlusion",
                size.map(|s| s.div_ceil(2)),
                wgpu::TextureFormat::Rg32Float,
                U::RENDER_ATTACHMENT | U::TEXTURE_BINDING,
            ),
            // The depth the estimate steps through: every other pixel's,
            // which spares its scattered steps half the cache lines (0.39
            // to 0.24 ms on Albion at 5K). Not the depth pyramid's base:
            // that keeps the farthest of four and is built before the cull
            // reveals what came into view.
            occlusion_depth: texture_2d(
                gpu,
                "editor occlusion depth",
                size.map(|s| s.div_ceil(2)),
                wgpu::TextureFormat::R32Float,
                U::RENDER_ATTACHMENT | U::TEXTURE_BINDING,
            ),
            // Depth in alpha as a half float: reversed perspective depth is
            // near / distance with near a 200th of the camera's distance,
            // so it keeps the 2% the upsample tells surfaces apart by out
            // to thousands of times that distance. Rgba32Float made the blur
            // and apply 0.05 and 0.06 ms slower on Albion at 5K.
            occlusion_blur: texture_2d(
                gpu,
                "editor occlusion blur",
                size.map(|s| s.div_ceil(2)),
                wgpu::TextureFormat::Rgba16Float,
                U::RENDER_ATTACHMENT | U::TEXTURE_BINDING,
            ),
            outline_tiles: None,
            ids: texture_2d(
                gpu,
                "editor pick ids",
                size,
                wgpu::TextureFormat::R32Uint,
                // Sampled by the selection outline; restored from a kept
                // copy under posed placements (`Layers::ids`).
                U::RENDER_ATTACHMENT | U::COPY_SRC | U::COPY_DST | U::TEXTURE_BINDING | stored,
            ),
            scene: texture_2d(
                gpu,
                "editor linear scene",
                size,
                wgpu::TextureFormat::R32Uint,
                U::RENDER_ATTACHMENT | U::TEXTURE_BINDING | stored,
            ),
            vis: None,
            vis_depth: None,
            posed_vis: None,
            posed_tiles: gpu.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("editor posed tiles"),
                size: visbuf::POSED_WORDS * 4,
                usage: wgpu::BufferUsages::STORAGE
                    | wgpu::BufferUsages::UNIFORM
                    | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            }),
            cull: None,
            readbacks: Vec::new(),
            timing,
            draw_ids: None,
            terrain: None,
            cascades: None,
            pages: None,
            layers: Layers::default(),
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

/// `octahedral` undone, as `gpu.wgsl`'s `octahedral` decodes it.
fn unoctahedral(v: u32) -> Vec3 {
    let snorm = |h: u32| ((h & 0xffff) as u16 as i16 as f32 / 32767.).max(-1.);
    let (x, y) = (snorm(v), snorm(v >> 16));
    let mut n = [x, y, 1. - x.abs() - y.abs()];
    let t = (-n[2]).max(0.);
    n[0] += if n[0] >= 0. { -t } else { t };
    n[1] += if n[1] >= 0. { -t } else { t };
    math::normalize(n).unwrap_or([0., 1., 0.])
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

/// Where the physical waves centre: where the view's centre ray meets the
/// sea, as the game does for a camera above it looking down; else the
/// camera's anchor.
/// What a plugged pass records with, made where each stage records: the
/// stages between borrow the targets whole.
fn record<'a>(gpu: &'a Gpu, scene: &'a wgpu::TextureView) -> pass::Record<'a> {
    pass::Record {
        device: &gpu.device,
        queue: &gpu.queue,
        scene,
        occlusion: false,
        kit: None,
    }
}

fn physical_focus(globals: &Globals) -> [f32; 2] {
    let (eye, anchor) = (globals.eye, globals.anchor);
    let ray = [anchor[0] - eye[0], anchor[1] - eye[1], anchor[2] - eye[2]];
    let length = (ray[0] * ray[0] + ray[1] * ray[1] + ray[2] * ray[2]).sqrt();
    if eye[3] > 0.5 && eye[1] > 5. && length > 0. && ray[1] / length < -0.26 {
        let t = -eye[1] / ray[1];
        return [eye[0] + ray[0] * t, eye[2] + ray[2] * t];
    }
    [anchor[0], anchor[2]]
}

fn rgb(color: u32) -> [f32; 3] {
    [16, 8, 0].map(|s| ((color >> s) & 255) as f32 / 255.)
}

impl Device {
    /// Whether the ground tint or cloud map is still filling (`update`):
    /// frames keep coming until they bind.
    fn controls_filling(&self) -> bool {
        (self.ground_tint.iter().chain(&self.cloud_map)).any(|(_, c)| !c.ready())
    }

    /// A device's GPU state for `format` and `families`, its pipelines
    /// shared with every other renderer drawing on that device
    /// (`Pipelines::shared`, `Placing::shared`).
    fn new(gpu: &Gpu, format: wgpu::TextureFormat, families: &Arc<placed::Families>) -> Self {
        // Off the render thread, as the rest: the first partial shadow
        // redraw compiling it took 8 ms of a paint dab's frame.
        shadow_clear(&gpu.device);
        let Pipelines {
            format,
            kit,
            hzb,
            bc,
            marker,
            occlusion,
            post,
            outline,
            kept,
            cull,
            select,
            line,
            exposure,
        } = Pipelines::shared(gpu, format);
        let Placing {
            arena,
            visibility,
            shading:
                Shading {
                    lit,
                    flat,
                    blend,
                    add,
                    decal,
                    ground_decal,
                    sky,
                    sea,
                    shadow,
                },
        } = Placing::shared(gpu, &kit, families);
        Self {
            exposure,
            format,
            kit,
            arena,
            visibility,
            hzb,
            lit,
            flat,
            bc,
            blend,
            add,
            decal,
            ground_decal,
            marker,
            sky,
            sea,
            occlusion,
            post,
            outline,
            kept,
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
            overlay_for: 0,
            line,
            sky_map: None,
            environment: None,
            host_texture: None,
            reflection: None,
            ground_tint: None,
            cloud_map: None,
            cloud_shade: None,
            detail: Detail::default(),
            ground_pages: None,
            surface: None,
            ground_program: None,
            ground_builds: 0,
            partial_shadows: 0,
            shadows_held: false,
            arena_drawn: Default::default(),
            passes_loading: false,
            scene: families.scene(),
        }
    }
}

/// The pipelines that draw placements from the arena. Their shaders end
/// with a family set's sources and dispatchers (`placed::Families`), so
/// they are built per device and family set, where `Pipelines` are per
/// device and format: a second set compiles these alone. None of them
/// draws into the window's format.
#[derive(Clone)]
struct Placing {
    /// Over the placement arena, with it: 0 flat meshes, 1 shadows, 2
    /// impostor shadows, 3 impostor baking, 4 decals, 5 and 6 the depth and
    /// shaded halves of impostors, 7 the shadows of meshes nothing cuts
    /// out, 8 decals on the ground; 9 to 11 those of 0, 5 and 6 with the
    /// visibility buffer's targets beside, for the revealed pass
    /// (`Device::draw_revealed`). Lit meshes draw through `visibility`.
    arena: Option<[wgpu::RenderPipeline; 12]>,
    /// The visibility buffer's, with the arena (visbuf.rs).
    visibility: Option<visbuf::Visibility>,
    shading: Shading,
}

/// The piece-by-piece pipelines, over the families' pieces module
/// (`placed::Module::Pieces`), which ends in their dispatchers and the
/// scene's `shade` and `fog`: meshes lit through their families, blended
/// and as decals, the shadows they cast through their families' coverage,
/// the kit's ground, its sky backdrop and its far sea.
#[derive(Clone)]
struct Shading {
    lit: wgpu::RenderPipeline,
    /// With the family set's lighting too: a view mode's `unlit` turns
    /// flat colours (placed.rs `ViewMode::unlit`).
    flat: wgpu::RenderPipeline,
    blend: wgpu::RenderPipeline,
    /// `Blend::additive` meshes: the blend pipeline adding its output.
    add: wgpu::RenderPipeline,
    decal: wgpu::RenderPipeline,
    ground_decal: wgpu::RenderPipeline,
    sky: wgpu::RenderPipeline,
    sea: wgpu::RenderPipeline,
    shadow: wgpu::RenderPipeline,
}

static PLACING: Mutex<Vec<(wgpu::Device, Arc<placed::Families>, Arc<OnceLock<Placing>>)>> =
    Mutex::new(Vec::new());

impl Placing {
    /// The pipelines for `gpu`'s device and `families`, built by the first
    /// to ask; others asking meanwhile wait for that build. Sets compare
    /// by what they hold, so two registries of the same families share.
    /// The device and the set are held until the device is lost
    /// (`forget`), as `Pipelines::cell` holds its devices: a host has a
    /// handful of sets.
    fn shared(gpu: &Gpu, kit: &Kit, families: &Arc<placed::Families>) -> Self {
        let cell = {
            let mut built = PLACING.lock_any();
            let found = built
                .iter()
                .find(|(d, f, _)| *d == gpu.device && (Arc::ptr_eq(f, families) || f == families));
            match found {
                Some((_, _, cell)) => cell.clone(),
                None => {
                    let cell = Arc::<OnceLock<Placing>>::default();
                    built.push((gpu.device.clone(), families.clone(), cell.clone()));
                    cell
                }
            }
        };
        cell.get_or_init(|| Self::build(gpu, kit, families)).clone()
    }

    fn build(gpu: &Gpu, kit: &Kit, families: &placed::Families) -> Self {
        let d = &gpu.device;
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
        // Compiled only with the arena, whose texture array it declares.
        let arena_shader = kit.arena.as_ref().map(|_| {
            d.create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some("editor placement arena"),
                source: wgpu::ShaderSource::Wgsl(families.module(placed::Module::Arena).into()),
            })
        });
        // The arena's other layouts: impostors read the atlas, bakes read
        // meshes from the arena and nothing per placement.
        let impostors = kit.arena.as_ref().map(|(arena, _)| {
            d.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("editor impostors"),
                bind_group_layouts: &[
                    Some(&kit.frame_layout),
                    Some(&kit.atlas_layout),
                    Some(&kit.placed_layout),
                    Some(arena),
                ],
                immediate_size: 0,
            })
        });
        let baking = kit.arena.as_ref().map(|(arena, _)| {
            d.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("editor impostor bake"),
                // Group 2 for what `lit`'s surface names and the bake
                // leaves out (fs_bake): bound to stand-ins.
                bind_group_layouts: &[
                    Some(&kit.frame_layout),
                    None,
                    Some(&kit.placed_layout),
                    Some(arena),
                ],
                immediate_size: 0,
            })
        });
        // No overlays here, so the window's format never matters.
        let build = |name: &str,
                     module: &wgpu::ShaderModule,
                     layout: &wgpu::PipelineLayout,
                     vertex: &str,
                     fragment: &str,
                     pass: Phase| {
            mesh_pipeline(d, HDR_FORMAT, name, module, layout, vertex, fragment, pass)
        };
        let build = &build;
        // Opaque phases blend nothing.
        let revealed = |name: &str, module, layout, vertex: &str, fragment: &str, pass| {
            mesh_pipeline_blending(
                d, HDR_FORMAT, name, module, layout, vertex, fragment, pass, None, true,
            )
        };
        let revealed = &revealed;
        // Every pipeline on a thread of its own, as `Pipelines::build`.
        std::thread::scope(|s| {
            let arena = (arena_shader.as_ref())
                .zip(arena_layout.as_ref())
                .zip(impostors.as_ref().zip(baking.as_ref()))
                .map(|((module, layout), (impostors, baking))| {
                    let placed = |name, vertex, fragment, pass| {
                        s.spawn(move || build(name, module, layout, vertex, fragment, pass))
                    };
                    let impostor = |name, vertex, fragment, pass| {
                        s.spawn(move || build(name, module, impostors, vertex, fragment, pass))
                    };
                    let placed_revealed = |name, vertex, fragment, pass| {
                        s.spawn(move || revealed(name, module, layout, vertex, fragment, pass))
                    };
                    let impostor_revealed = |name, vertex, fragment, pass| {
                        s.spawn(move || revealed(name, module, impostors, vertex, fragment, pass))
                    };
                    let bake = s.spawn(move || {
                        // Color, octahedral model-space normals, and depth with
                        // gloss per view; nearer is smaller z.
                        let target = |format| {
                            Some(wgpu::ColorTargetState {
                                format,
                                blend: None,
                                write_mask: wgpu::ColorWrites::ALL,
                            })
                        };
                        d.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                            label: Some("editor impostor bake"),
                            layout: Some(baking),
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
                        })
                    });
                    [
                        placed(
                            "editor placed flat",
                            "vs_placed",
                            "fs_placed_flat",
                            Phase::Opaque,
                        ),
                        // Depth and the cutout alone.
                        placed(
                            "editor placed shadow",
                            "vs_placed_depth",
                            "fs_placed_depth",
                            Phase::Shadow,
                        ),
                        impostor(
                            "editor impostor shadows",
                            "vs_impostor_depth",
                            "fs_impostor_shadow",
                            Phase::Shadow,
                        ),
                        bake,
                        placed(
                            "editor placed decals",
                            "vs_placed",
                            "fs_placed_decal",
                            Phase::Blend,
                        ),
                        impostor(
                            "editor impostor depth",
                            "vs_impostor_depth",
                            "fs_impostor_shadow",
                            Phase::Prepass,
                        ),
                        impostor(
                            "editor impostors shaded",
                            "vs_impostor",
                            "fs_impostor_shaded",
                            Phase::Shaded,
                        ),
                        // Depth of what nothing cuts out (Ranges::lit_opaque).
                        placed(
                            "editor placed opaque shadow",
                            "vs_placed_position",
                            "",
                            Phase::Shadow,
                        ),
                        placed(
                            "editor placed ground decals",
                            "vs_placed",
                            "fs_placed_decal",
                            Phase::Decal,
                        ),
                        placed_revealed(
                            "editor placed flat, revealed",
                            "vs_placed",
                            "fs_placed_flat",
                            Phase::Opaque,
                        ),
                        impostor_revealed(
                            "editor impostor depth, revealed",
                            "vs_impostor_depth",
                            "fs_impostor_shadow",
                            Phase::Prepass,
                        ),
                        impostor_revealed(
                            "editor impostors shaded, revealed",
                            "vs_impostor",
                            "fs_impostor_shaded",
                            Phase::Shaded,
                        ),
                    ]
                });
            let visibility = s.spawn(|| visbuf::Visibility::new(d, kit, families));
            let shading = Shading::build(d, kit, families);
            Self {
                arena: arena.map(|a| a.map(|h| h.join().expect("pipeline build"))),
                visibility: visibility.join().expect("pipeline build"),
                shading,
            }
        })
    }
}

impl Shading {
    fn build(d: &wgpu::Device, kit: &Kit, families: &placed::Families) -> Self {
        let shader = d.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("editor shading"),
            source: wgpu::ShaderSource::Wgsl(families.module(placed::Module::Pieces).into()),
        });
        let layout = |groups: &[&wgpu::BindGroupLayout]| {
            let groups: Vec<Option<&wgpu::BindGroupLayout>> =
                groups.iter().map(|g| Some(*g)).collect();
            d.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("editor shading"),
                bind_group_layouts: &groups,
                immediate_size: 0,
            })
        };
        let meshes = layout(&[&kit.frame_layout, &kit.mesh_layout, &kit.draw_layout]);
        let sky = layout(&[&kit.frame_layout]);
        let (shader, meshes, sky) = (&shader, &meshes, &sky);
        // None draws into the window's format.
        std::thread::scope(|s| {
            let build = |name, layout, vertex, fragment, pass| {
                s.spawn(move || {
                    mesh_pipeline(d, HDR_FORMAT, name, shader, layout, vertex, fragment, pass)
                })
            };
            let join = |h: std::thread::ScopedJoinHandle<'_, wgpu::RenderPipeline>| {
                h.join().expect("pipeline build")
            };
            let lit = build("editor lit", meshes, "vs_mesh", "fs_lit", Phase::Opaque);
            let flat = build("editor flat", meshes, "vs_mesh", "fs_flat", Phase::Opaque);
            let blend = build("editor blend", meshes, "vs_mesh", "fs_blend", Phase::Blend);
            // fs_blend writes premultiplied light and a coverage for an
            // additive mesh; both add.
            let adding = wgpu::BlendComponent {
                src_factor: wgpu::BlendFactor::One,
                dst_factor: wgpu::BlendFactor::One,
                operation: wgpu::BlendOperation::Add,
            };
            let add = s.spawn(move || {
                mesh_pipeline_blending(
                    d,
                    HDR_FORMAT,
                    "editor blend add",
                    shader,
                    meshes,
                    "vs_mesh",
                    "fs_blend",
                    Phase::Blend,
                    Some(wgpu::BlendState {
                        color: adding,
                        alpha: adding,
                    }),
                    false,
                )
            });
            let decal = build("editor decal", meshes, "vs_mesh", "fs_decal", Phase::Blend);
            let ground_decal = build(
                "editor ground decal",
                meshes,
                "vs_mesh",
                "fs_decal",
                Phase::Decal,
            );
            let sea = build("editor sea", sky, "vs_sky", "fs_sea", Phase::Sky);
            let sky = build("editor sky", sky, "vs_sky", "fs_sky", Phase::Sky);
            let shadow = build(
                "editor shadow",
                meshes,
                "vs_mesh",
                "fs_piece_shadow",
                Phase::Shadow,
            );
            Self {
                lit: join(lit),
                flat: join(flat),
                blend: join(blend),
                add: join(add),
                decal: join(decal),
                ground_decal: join(ground_decal),
                sky: join(sky),
                sea: join(sea),
                shadow: join(shadow),
            }
        })
    }
}

static PIPELINES: Mutex<Vec<(wgpu::Device, wgpu::TextureFormat, Arc<OnceLock<Pipelines>>)>> =
    Mutex::new(Vec::new());

/// What a `Device` draws with and builds against that never changes: the
/// kit and every pipeline, for one target format. Compiling them took 0.1
/// s with the driver's shader cache warm and 3 s cold, and every island
/// opened made a renderer of its own that compiled them again.
#[derive(Clone)]
struct Pipelines {
    format: wgpu::TextureFormat,
    kit: Kit,
    hzb: [wgpu::ComputePipeline; 2],
    bc: bc::Encoder,
    marker: wgpu::RenderPipeline,
    occlusion: [wgpu::RenderPipeline; 5],
    post: wgpu::RenderPipeline,
    outline: (
        wgpu::BindGroupLayout,
        wgpu::BindGroupLayout,
        [wgpu::RenderPipeline; 2],
    ),
    kept: (wgpu::BindGroupLayout, [wgpu::RenderPipeline; 3]),
    cull: [wgpu::ComputePipeline; 2],
    select: (wgpu::BindGroupLayout, [wgpu::ComputePipeline; 2]),
    line: wgpu::RenderPipeline,
    exposure: exposure::Pipelines,
}

/// A pipeline for one of the frame's phases over `module`: `pass` picks
/// its targets, blending and depth test. `format` is the window's, which
/// overlays alone draw into.
#[allow(clippy::too_many_arguments)]
fn mesh_pipeline(
    d: &wgpu::Device,
    format: wgpu::TextureFormat,
    name: &str,
    module: &wgpu::ShaderModule,
    layout: &wgpu::PipelineLayout,
    vertex: &str,
    fragment: &str,
    pass: Phase,
) -> wgpu::RenderPipeline {
    let blend = matches!(pass, Phase::Blend | Phase::Overlay | Phase::Decal)
        .then_some(wgpu::BlendState::ALPHA_BLENDING);
    mesh_pipeline_blending(
        d, format, name, module, layout, vertex, fragment, pass, blend, false,
    )
}

/// [`mesh_pipeline`] with the scene target's blending given, not the pass's;
/// `revealed` with the visibility buffer's targets after the opaque ones,
/// unwritten, for the revealed pass (`Device::draw_revealed`).
#[allow(clippy::too_many_arguments)]
fn mesh_pipeline_blending(
    d: &wgpu::Device,
    format: wgpu::TextureFormat,
    name: &str,
    module: &wgpu::ShaderModule,
    layout: &wgpu::PipelineLayout,
    vertex: &str,
    fragment: &str,
    pass: Phase,
    blend: Option<wgpu::BlendState>,
    revealed: bool,
) -> wgpu::RenderPipeline {
    let writes = if pass == Phase::Prepass {
        wgpu::ColorWrites::empty()
    } else {
        wgpu::ColorWrites::ALL
    };
    // The opaque pass also writes pick handles and linear color, but
    // decals drawn in it blend into the color alone: the ground under them
    // keeps its handle and the linear color the ocean refracts.
    // Overlays draw display values after the post stage; everything
    // else draws the scene into the HDR target.
    let beside = if pass == Phase::Decal {
        wgpu::ColorWrites::empty()
    } else {
        writes
    };
    let unwritten = |format| {
        Some(wgpu::ColorTargetState {
            format,
            blend: None,
            write_mask: wgpu::ColorWrites::empty(),
        })
    };
    let [vis, vis_depth] = visbuf::VIS_TARGETS;
    let colors = [
        Some(wgpu::ColorTargetState {
            format: if pass == Phase::Overlay {
                format
            } else {
                HDR_FORMAT
            },
            blend,
            write_mask: writes,
        }),
        Some(wgpu::ColorTargetState {
            format: wgpu::TextureFormat::R32Uint,
            blend: None,
            write_mask: beside,
        }),
        Some(wgpu::ColorTargetState {
            format: wgpu::TextureFormat::R32Uint,
            blend: None,
            write_mask: beside,
        }),
        unwritten(vis),
        unwritten(vis_depth),
    ];
    let opaque = if revealed { 5 } else { 3 };
    let targets: &[Option<wgpu::ColorTargetState>] = match pass {
        Phase::Shadow => &[],
        Phase::Opaque | Phase::Prepass | Phase::Shaded | Phase::Over | Phase::Decal => {
            &colors[..opaque]
        }
        Phase::Blend | Phase::Overlay | Phase::Sky => &colors[..1],
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
        // "" for none: depth alone, into no color target.
        fragment: (!fragment.is_empty()).then_some(wgpu::FragmentState {
            module,
            entry_point: Some(fragment),
            targets,
            compilation_options: Default::default(),
        }),
        primitive: wgpu::PrimitiveState::default(),
        // Overlays draw after the post stage, over no depth.
        depth_stencil: (pass != Phase::Overlay).then(|| wgpu::DepthStencilState {
            format: if pass == Phase::Shadow {
                SHADOW_FORMAT
            } else {
                wgpu::TextureFormat::Depth32Float
            },
            depth_write_enabled: Some(matches!(
                pass,
                Phase::Shadow | Phase::Opaque | Phase::Prepass | Phase::Over
            )),
            depth_compare: Some(match pass {
                Phase::Shadow => wgpu::CompareFunction::LessEqual,
                Phase::Opaque | Phase::Blend | Phase::Sky | Phase::Prepass | Phase::Decal => {
                    wgpu::CompareFunction::GreaterEqual
                }
                Phase::Shaded => wgpu::CompareFunction::Equal,
                Phase::Over => wgpu::CompareFunction::Greater,
                Phase::Overlay => wgpu::CompareFunction::Always,
            }),
            stencil: Default::default(),
            bias: if pass == Phase::Shadow {
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
}

impl Pipelines {
    /// The pipelines for `gpu`'s device and `format`, built by the first
    /// to ask; others asking meanwhile wait for that build.
    fn shared(gpu: &Gpu, format: wgpu::TextureFormat) -> Self {
        let (cell, _) = Self::cell(&gpu.device, format);
        cell.get_or_init(|| Self::build(gpu, format)).clone()
    }

    /// The build slot for a device and format, and whether it is new. The
    /// device is held until it is lost (`forget`): a window's device lives
    /// as long as the process otherwise.
    fn cell(device: &wgpu::Device, format: wgpu::TextureFormat) -> (Arc<OnceLock<Self>>, bool) {
        let mut built = PIPELINES.lock_any();
        if let Some((_, _, cell)) = built.iter().find(|(d, f, _)| d == device && *f == format) {
            return (cell.clone(), false);
        }
        let cell = Arc::<OnceLock<Pipelines>>::default();
        built.push((device.clone(), format, cell.clone()));
        (cell, true)
    }

    fn build(gpu: &Gpu, format: wgpu::TextureFormat) -> Self {
        let kit = Kit::new(gpu);
        let d = &gpu.device;
        let shader = d.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("editor"),
            // The kit's lighting: these pipelines shade nothing (`Shading`).
            source: wgpu::ShaderSource::Wgsl(
                [
                    pass::FRAME_PRELUDE,
                    include_str!("gpu.wgsl"),
                    &placed::Families::kit().scene(),
                ]
                .concat()
                .into(),
            ),
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
        let build = |name: &str,
                     module: &wgpu::ShaderModule,
                     layout: &wgpu::PipelineLayout,
                     vertex: &str,
                     fragment: &str,
                     pass: Phase| {
            mesh_pipeline(d, format, name, module, layout, vertex, fragment, pass)
        };
        let occlusion_layout = layout(&[&kit.frame_layout, &kit.occlusion_layout]);
        let post_layout = layout(&[&kit.frame_layout, &kit.post_layout]);
        let pipeline = |name: &str, vertex: &str, fragment: &str, pass: Phase| {
            let layout = if matches!(pass, Phase::Overlay | Phase::Sky) {
                &overlay
            } else {
                &meshes
            };
            build(name, &shader, layout, vertex, fragment, pass)
        };
        // Occlusion: estimate and blur at half size, then bring it up and
        // multiply it over the color. None tests depth; all read a copy.
        // `over_depth`: in a pass with the opaque depth attachment, which it
        // leaves as it is.
        let fullscreen = |name: &str,
                          layout: &wgpu::PipelineLayout,
                          fragment: &str,
                          target,
                          over_depth: bool| {
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
                depth_stencil: over_depth.then(|| wgpu::DepthStencilState {
                    format: wgpu::TextureFormat::Depth32Float,
                    depth_write_enabled: Some(false),
                    depth_compare: Some(wgpu::CompareFunction::Always),
                    stencil: Default::default(),
                    bias: Default::default(),
                }),
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
        // Every pipeline on a thread of its own: the driver compiles each
        // apart, and one after another they took 3 s with its shader cache
        // cold, 0.1 s warm.
        let (pipeline, fullscreen, kit) = (&pipeline, &fullscreen, &kit);
        let overlay = &overlay;
        let (occlusion_layout, post_layout) = (&occlusion_layout, &post_layout);
        std::thread::scope(|s| {
            let mesh = |name, vertex, fragment, pass| {
                s.spawn(move || pipeline(name, vertex, fragment, pass))
            };
            let marker = mesh("editor marker", "vs_marker", "fs_marker", Phase::Overlay);
            let line = mesh("editor line", "vs_line", "fs_line", Phase::Overlay);
            let screen = |name, layout, fragment, target| {
                s.spawn(move || fullscreen(name, layout, fragment, target, false))
            };
            let applied = wgpu::ColorTargetState {
                format: HDR_FORMAT,
                blend: Some(wgpu::BlendState {
                    color: multiply,
                    alpha: wgpu::BlendComponent::REPLACE,
                }),
                write_mask: wgpu::ColorWrites::COLOR,
            };
            let occlusion = [
                screen(
                    "editor occlusion depth",
                    overlay,
                    "fs_occlusion_depth",
                    wgpu::ColorTargetState {
                        format: wgpu::TextureFormat::R32Float,
                        blend: None,
                        write_mask: wgpu::ColorWrites::ALL,
                    },
                ),
                screen(
                    "editor occlusion",
                    occlusion_layout,
                    "fs_occlusion",
                    wgpu::ColorTargetState {
                        format: wgpu::TextureFormat::Rg32Float,
                        blend: None,
                        write_mask: wgpu::ColorWrites::ALL,
                    },
                ),
                screen(
                    "editor occlusion blur",
                    occlusion_layout,
                    "fs_occlusion_blur",
                    wgpu::ColorTargetState {
                        format: wgpu::TextureFormat::Rgba16Float,
                        blend: None,
                        write_mask: wgpu::ColorWrites::ALL,
                    },
                ),
                screen(
                    "editor occlusion apply",
                    occlusion_layout,
                    "fs_occlusion_apply",
                    applied.clone(),
                ),
                s.spawn(move || {
                    let name = "editor occlusion apply, late";
                    fullscreen(
                        name,
                        occlusion_layout,
                        "fs_occlusion_apply_late",
                        applied,
                        true,
                    )
                }),
            ];
            // Coverage-weighted HDR over whatever the target held: see fs_post.
            let post = screen(
                "editor post",
                post_layout,
                "fs_post",
                wgpu::ColorTargetState {
                    format,
                    blend: Some(wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                },
            );
            let outline = s.spawn(|| {
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
                let tiles = d.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                    label: Some("editor outline tiles"),
                    entries: &[layout_entry(
                        0,
                        wgpu::ShaderStages::FRAGMENT,
                        wgpu::BindingType::Texture {
                            sample_type: wgpu::TextureSampleType::Uint,
                            view_dimension: wgpu::TextureViewDimension::D2,
                            multisampled: false,
                        },
                    )],
                });
                let module = d.create_shader_module(wgpu::ShaderModuleDescriptor {
                    label: Some("editor outline"),
                    source: wgpu::ShaderSource::Wgsl(include_str!("gpu/outline.wgsl").into()),
                });
                let pipeline = |label, groups: &[&wgpu::BindGroupLayout], entry, target| {
                    d.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                        label: Some(label),
                        layout: Some(&layout(groups)),
                        vertex: wgpu::VertexState {
                            module: &module,
                            entry_point: Some("vs_outline"),
                            buffers: &[],
                            compilation_options: Default::default(),
                        },
                        fragment: Some(wgpu::FragmentState {
                            module: &module,
                            entry_point: Some(entry),
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
                let tile_pass = pipeline(
                    "editor outline tiles",
                    &[&group],
                    "fs_tiles",
                    wgpu::ColorTargetState {
                        format: OUTLINE_TILES,
                        blend: None,
                        write_mask: wgpu::ColorWrites::ALL,
                    },
                );
                let band = pipeline(
                    "editor outline",
                    &[&group, &tiles],
                    "fs_outline",
                    wgpu::ColorTargetState {
                        format,
                        blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                        write_mask: wgpu::ColorWrites::COLOR,
                    },
                );
                (group, tiles, [tile_pass, band])
            });
            let kept = s.spawn(|| {
                let group = d.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                    label: Some("editor kept layers"),
                    entries: &[
                        layout_entry(0, wgpu::ShaderStages::FRAGMENT, depth_texture()),
                        layout_entry(
                            1,
                            wgpu::ShaderStages::FRAGMENT,
                            sampled(false, wgpu::TextureViewDimension::D2),
                        ),
                    ],
                });
                let module = d.create_shader_module(wgpu::ShaderModuleDescriptor {
                    label: Some("editor kept layers"),
                    source: wgpu::ShaderSource::Wgsl(include_str!("gpu/kept.wgsl").into()),
                });
                let pipeline_layout = layout(&[&group]);
                // Every pixel's depth as written, whatever was there.
                let depth = wgpu::DepthStencilState {
                    format: DEPTH_FORMAT,
                    depth_write_enabled: Some(true),
                    depth_compare: Some(wgpu::CompareFunction::Always),
                    stencil: Default::default(),
                    bias: Default::default(),
                };
                let pipeline = |entry: &str, targets: &[Option<wgpu::ColorTargetState>], depth| {
                    d.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                        label: Some("editor kept layers"),
                        layout: Some(&pipeline_layout),
                        vertex: wgpu::VertexState {
                            module: &module,
                            entry_point: Some("vs_kept"),
                            buffers: &[],
                            compilation_options: Default::default(),
                        },
                        fragment: Some(wgpu::FragmentState {
                            module: &module,
                            entry_point: Some(entry),
                            targets,
                            compilation_options: Default::default(),
                        }),
                        primitive: wgpu::PrimitiveState::default(),
                        depth_stencil: depth,
                        multisample: Default::default(),
                        multiview_mask: None,
                        cache: None,
                    })
                };
                let untouched = OPAQUE_TARGETS.map(|format| {
                    Some(wgpu::ColorTargetState {
                        format,
                        blend: None,
                        write_mask: wgpu::ColorWrites::empty(),
                    })
                });
                let hdr = [Some(wgpu::ColorTargetState {
                    format: HDR_FORMAT,
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })];
                let pipelines = [
                    pipeline("fs_kept_depth", &untouched, Some(depth.clone())),
                    pipeline("fs_kept_whole", &hdr, Some(depth)),
                    pipeline("fs_kept_color", &hdr, None),
                ];
                (group, pipelines)
            });
            let cull = s.spawn(|| {
                let cull_shader = d.create_shader_module(wgpu::ShaderModuleDescriptor {
                    label: Some("editor cull"),
                    source: wgpu::ShaderSource::Wgsl(include_str!("gpu/cull.wgsl").into()),
                });
                let layout = layout(&[&kit.cull_layout]);
                ["cs_cull", "cs_order_decals"].map(|entry| {
                    d.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
                        label: Some("editor cull"),
                        layout: Some(&layout),
                        module: &cull_shader,
                        entry_point: Some(entry),
                        compilation_options: Default::default(),
                        cache: None,
                    })
                })
            });
            let select = s.spawn(|| {
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
            });
            let hzb = s.spawn(|| {
                let hzb_shader = d.create_shader_module(wgpu::ShaderModuleDescriptor {
                    label: Some("editor depth pyramid"),
                    source: wgpu::ShaderSource::Wgsl(include_str!("gpu/hzb.wgsl").into()),
                });
                [
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
                })
            });
            // The kit's own families' pipelines beside these: what a
            // renderer draws placements with unless a host brought a set.
            let placing = s.spawn(|| Placing::shared(gpu, kit, &placed::Families::kit()));
            let exposure = s.spawn(|| exposure::Pipelines::new(d));
            let bc = s.spawn(|| bc::Encoder::new(d));
            fn join<T>(h: std::thread::ScopedJoinHandle<'_, T>) -> T {
                h.join().expect("pipeline build")
            }
            join(placing);
            Self {
                exposure: join(exposure),
                format,
                kit: kit.clone(),
                hzb: join(hzb),
                bc: join(bc),
                marker: join(marker),
                occlusion: occlusion.map(join),
                post: join(post),
                outline: join(outline),
                kept: join(kept),
                cull: join(cull),
                select: join(select),
                line: join(line),
            }
        })
    }
}

impl Device {
    /// Uploads `map` as `t_sky` unless it is already there: half floats,
    /// which filter everywhere (32-bit floats need a feature).
    fn upload_sky(&mut self, gpu: &Gpu, map: Option<&Arc<SkyMap>>) {
        let Some(map) = map else {
            self.sky_map = None;
            return;
        };
        if self
            .sky_map
            .as_ref()
            .is_some_and(|s| Arc::ptr_eq(&s.0, map))
        {
            return;
        }
        let (texture, view) = texture_2d(
            gpu,
            "editor sky",
            [map.width, map.height],
            wgpu::TextureFormat::Rgba16Float,
            wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        );
        let texels: Vec<u16> = (map.texels.iter())
            .flat_map(|t| [t[0], t[1], t[2], 1.].map(half))
            .collect();
        write_texture_rect(
            &gpu.queue,
            &texture,
            [0, 0],
            [map.width, map.height],
            8,
            bytemuck::cast_slice(&texels),
        );
        self.sky_map = Some((map.clone(), texture, view));
    }

    /// Uploads `cube` as `t_environment` unless it is already there, in
    /// half floats.
    fn upload_environment(&mut self, gpu: &Gpu, cube: Option<&Environment>) {
        let Some(cube) = cube.filter(|c| c.side > 0) else {
            self.environment = None;
            return;
        };
        if (self.environment.as_ref()).is_some_and(|e| Arc::ptr_eq(&e.0, &cube.texels)) {
            return;
        }
        let side = cube.side;
        let texture = gpu.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("editor environment"),
            size: wgpu::Extent3d {
                width: side,
                height: side,
                depth_or_array_layers: 6,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba16Float,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        let face = (side * side) as usize;
        for f in 0..6 {
            let texels: Vec<u16> = (cube.texels.iter().skip(f * face).take(face))
                .flat_map(|t| [t[0], t[1], t[2], 1.].map(half))
                .collect();
            if texels.len() < face * 4 {
                // A short cube: what it lacks stays black.
                break;
            }
            gpu.queue.write_texture(
                wgpu::TexelCopyTextureInfo {
                    texture: &texture,
                    mip_level: 0,
                    origin: wgpu::Origin3d {
                        x: 0,
                        y: 0,
                        z: f as u32,
                    },
                    aspect: wgpu::TextureAspect::All,
                },
                bytemuck::cast_slice(&texels),
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(side * 8),
                    rows_per_image: Some(side),
                },
                wgpu::Extent3d {
                    width: side,
                    height: side,
                    depth_or_array_layers: 1,
                },
            );
        }
        let view = texture.create_view(&wgpu::TextureViewDescriptor {
            dimension: Some(wgpu::TextureViewDimension::Cube),
            ..Default::default()
        });
        self.environment = Some((cube.texels.clone(), texture, view));
    }

    /// Uploads `host` as `t_host` unless it is already there.
    fn upload_host_texture(&mut self, gpu: &Gpu, host: Option<&HostTexture>) {
        let Some(host) = host.filter(|h| h.width * h.height * h.layers > 0) else {
            self.host_texture = None;
            return;
        };
        if (self.host_texture.as_ref()).is_some_and(|h| Arc::ptr_eq(&h.0, &host.texels)) {
            return;
        }
        let size = wgpu::Extent3d {
            width: host.width,
            height: host.height,
            depth_or_array_layers: host.layers,
        };
        let texture = gpu.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("editor host texture"),
            size,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        // A short one keeps what it lacks black.
        let whole = (host.width * host.height * host.layers * 4) as usize;
        if host.texels.len() >= whole {
            gpu.queue.write_texture(
                texture.as_image_copy(),
                &host.texels[..whole],
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(host.width * 4),
                    rows_per_image: Some(host.height),
                },
                size,
            );
        }
        let view = texture.create_view(&wgpu::TextureViewDescriptor {
            dimension: Some(wgpu::TextureViewDimension::D2Array),
            ..Default::default()
        });
        self.host_texture = Some((host.texels.clone(), texture, view));
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
    fn update(
        &mut self,
        gpu: &Gpu,
        scene: &Arc<PreparedScene>,
        assets: &mut Assets,
        budget: &mut Budget,
    ) {
        // A new tint or cloud map fills a band a frame, unbound until full;
        // then tint strokes land in it between scenes.
        let span = crate::profile::span("gpu scene: control maps");
        if let Some((_, map)) = &self.cloud_map
            && map.fill(&gpu.queue, budget)
            && map.sync(&gpu.queue).is_some()
        {
            assets.writes += 1;
        }
        let tinted = (self.ground_tint.as_ref())
            .filter(|(_, map)| map.fill(&gpu.queue, budget))
            .and_then(|(_, map)| map.sync(&gpu.queue));
        assets.writes += u64::from(tinted.is_some());
        if let Some(rect) = tinted
            && let Some(tint) = self.presented.as_ref().and_then(|p| p.ground_tint.as_ref())
            && let Some(pages) = &mut self.ground_pages
        {
            let map = &self.ground_tint.as_ref().expect("synced above").1;
            // Texels are filtered: one more each way.
            let [w, h] = map.source.size().map(|v| v as f32);
            let at = |x: usize, z: usize, pad: f32, a: usize, n: f32| {
                tint.origin[a] + (if a == 0 { x } else { z } as f32 + pad) / n * tint.size[a]
            };
            pages.invalidate(terrain::GroundEdit::World([
                at(rect.x0, rect.z0, -1., 0, w),
                at(rect.x0, rect.z0, -1., 1, h),
                at(rect.x1, rect.z1, 1., 0, w),
                at(rect.x1, rect.z1, 1., 1, h),
            ]));
        }
        drop(span);
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
        let mut added = 0;
        let _span = crate::profile::span("gpu scene: meshes");
        {
            let meshes = &mut self.loading.as_mut().expect("loading").1;
            while meshes.len() < scene.batches.len() {
                if budget.spent() || added >= UPLOAD_BATCHES {
                    return;
                }
                let batch = &scene.batches[meshes.len()].batch;
                let mesh = assets.mesh(gpu, &self.kit, &batch.mesh, budget);
                if !assets.stream(gpu, &mesh, budget) {
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
        self.mount_ground(gpu, scene);
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
        self.upload_sky(gpu, scene.sky.as_ref().and_then(|s| s.radiance.as_ref()));
        self.upload_environment(gpu, scene.environment.as_ref());
        self.upload_host_texture(gpu, scene.host_texture.as_ref());
        self.ground_tint = scene
            .ground_tint
            .as_ref()
            .map(|t| match self.ground_tint.take() {
                Some(same) if same.0 == key(&t.map) => same,
                _ => (
                    key(&t.map),
                    ResidentControl::new(&gpu.device, "ground tint", t.map.clone(), Kept::Bc1),
                ),
            });
        self.cloud_map = scene
            .cloud_shadows
            .as_ref()
            .map(|c| match self.cloud_map.take() {
                Some(same) if same.0 == key(&c.map) => same,
                _ => (
                    key(&c.map),
                    ResidentControl::new(
                        &gpu.device,
                        "cloud shadow map",
                        c.map.clone(),
                        Kept::Rgba,
                    ),
                ),
            });
        self.presented = Some(scene.clone());
        // The old scene's meshes are free to go.
        assets.presented += 1;
        assets.writes += 1;
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
        let (Some(placed_buffers), Some(buffers)) = (&placed.buffers, &sel.buffers) else {
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
        // No markers at all, the scene's or the selection's: a drag still
        // moves its rows, and cs_marks, which writes them, does not run.
        let markers = self.markers.as_ref().map_or(&d.written, |m| &m.0);
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

    /// Bind group 0 for one pass: its camera, the lights, the shadow map and
    /// the view's cascades to sample (dummies while drawing them), the
    /// opaque depth (a dummy outside the blend pass) and the view's ground
    /// page index (the opaque pass's).
    fn frame_group(
        &self,
        gpu: &Gpu,
        globals: &Globals,
        lights: &Lights,
        shadow: Option<(&wgpu::TextureView, &wgpu::TextureView)>,
        depth: Option<&wgpu::TextureView>,
        pages: Option<&wgpu::TextureView>,
        // The blurred occlusion, at Water and Blend when a Blend-stage pass
        // applies it (frame.wgsl occlusion_factor).
        occlusion: Option<&wgpu::TextureView>,
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
        // Its heights, which terrain-adapting placements bend to.
        let ground = (self.presented.as_ref())
            .and_then(|s| s.terrain.as_ref())
            .and_then(|t| self.resident.get(t.first))
            .and_then(|draw| draw.mesh.heights.as_ref())
            .map_or(&d.heights, |h| &h.view);
        gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("editor frame"),
            layout: &self.kit.frame_layout,
            entries: &[
                entry(0, globals.as_entire_binding()),
                entry(1, lights.as_entire_binding()),
                entry(2, wgpu::BindingResource::Sampler(&self.kit.sampler)),
                entry(
                    3,
                    wgpu::BindingResource::TextureView(shadow.map_or(&d.depth, |s| s.0)),
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
                entry(9, wgpu::BindingResource::TextureView(ground)),
                entry(
                    10,
                    wgpu::BindingResource::TextureView(
                        self.sky_map.as_ref().map_or(&d.black, |s| &s.2),
                    ),
                ),
                entry(
                    11,
                    wgpu::BindingResource::TextureView(
                        (self.ground_tint.as_ref())
                            .filter(|t| t.1.ready())
                            .map_or(&d.white, |t| &t.1.view),
                    ),
                ),
                entry(
                    12,
                    wgpu::BindingResource::TextureView(
                        (self.cloud_map.as_ref())
                            .filter(|t| t.1.ready())
                            .map_or(&d.white, |t| &t.1.view),
                    ),
                ),
                entry(
                    13,
                    wgpu::BindingResource::TextureView(shadow.map_or(&d.cascades, |s| s.1)),
                ),
                entry(
                    14,
                    wgpu::BindingResource::TextureView(pages.unwrap_or(&d.page_index)),
                ),
                entry(
                    15,
                    wgpu::BindingResource::TextureView(
                        (self.ground_pages.as_ref()).map_or(&d.array, |p| &p.albedo_view),
                    ),
                ),
                entry(
                    16,
                    wgpu::BindingResource::TextureView(
                        (self.ground_pages.as_ref())
                            .and_then(|p| p.normal_view())
                            .unwrap_or(&d.array),
                    ),
                ),
                entry(17, wgpu::BindingResource::Sampler(&self.kit.pages)),
                entry(
                    18,
                    wgpu::BindingResource::TextureView(
                        self.cloud_shade.as_ref().map_or(&d.white, |s| &s.view),
                    ),
                ),
                // A stand-in that reads as no occlusion: white, its depth
                // 1 nowhere near a drawn pixel's, so the factor is the
                // nearest texel's, white.
                entry(
                    19,
                    wgpu::BindingResource::TextureView(occlusion.unwrap_or(&d.white)),
                ),
                // Read only by a ground that declares glow (ground_glows()).
                entry(
                    20,
                    wgpu::BindingResource::TextureView(
                        (self.ground_pages.as_ref())
                            .and_then(|p| p.glow_view())
                            .unwrap_or(&d.array),
                    ),
                ),
                entry(
                    21,
                    wgpu::BindingResource::TextureView(
                        self.environment.as_ref().map_or(&d.cube, |e| &e.2),
                    ),
                ),
                entry(
                    22,
                    wgpu::BindingResource::TextureView(
                        self.host_texture.as_ref().map_or(&d.array, |h| &h.2),
                    ),
                ),
                entry(
                    23,
                    wgpu::BindingResource::TextureView(
                        self.reflection.as_ref().unwrap_or(&d.black),
                    ),
                ),
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
            ],
        })
    }

    /// Resets this frame's indirect arguments (every draw at zero
    /// instances), growing the cull outputs for the placements as needed.
    /// The view's cull lists, made or grown to fit the placements; the
    /// shadow maps' culls use them too.
    fn prepare_cull(&self, gpu: &Gpu, placed: &placed::Placed, view: &mut Targets) {
        let draws = placed.draws.len() as u32;
        if view.cull.as_ref().is_none_or(|t| {
            t.capacity < placed.capacity || t.rows < placed.count || t.draws != draws
        }) {
            // Room to grow, so scattering a few props keeps the buffers: an
            // eighth more. The next power of two left the continental
            // island's 1.25M rows 0.85M more, 17 MB over the row lists.
            let grown = |n: u32| (n + n / 8).next_multiple_of(1024).max(1024);
            let (capacity, rows) = (grown(placed.capacity), grown(placed.count));
            view.cull = Some(placed::CullTargets::new(gpu, capacity, rows, draws));
        }
    }

    /// Starts the view's cull lists over for this frame's culls: totals
    /// cleared, arguments from the template. Not on frames that keep the
    /// lists (`Reuse`).
    fn reset_cull(
        &self,
        gpu: &Gpu,
        encoder: &mut wgpu::CommandEncoder,
        placed: &placed::Placed,
        view: &mut Targets,
    ) {
        let draws = placed.indirect_draws();
        encoder.clear_buffer(&view.posed_tiles, 0, None);
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
    /// world to the render target, for the occlusion test. `eye` is the
    /// view's, which `Detail::reach` measures from; `None` reaches anywhere.
    /// `screen` is the device pixels per pixel of `height`, which
    /// `Detail::min_pixels` counts in.
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
        [height, screen]: [f32; 2],
        planes: [[f64; 4]; 6],
        wants: Option<(u32, f32)>,
        eye: Option<[f32; 3]>,
    ) {
        let (Some(buffers), Some(cull)) = (&placed.buffers, &targets.cull) else {
            return;
        };
        // The section of the view's wants this cull answers in, started
        // over: only with the arena, whose slots they name.
        let texels = wants.map_or(1., |(_, f)| f);
        let wants = wants
            .zip(placed.arena.as_ref())
            .map(|((section, _), arena)| {
                let bytes = u64::from(residency::SLOTS) * 4;
                // The second phase adds to what the first asked this frame.
                if phase != CullPhase::Revealed {
                    encoder.clear_buffer(&cull.wants, u64::from(section) * bytes, Some(bytes));
                }
                (section, arena)
            });
        let (args, visible, keep) = match phase {
            CullPhase::Shadow => (&cull.shadow_args, &cull.shadow_visible, &cull.shadow_keep),
            CullPhase::Seen => (&cull.args, &cull.visible, &cull.keep),
            CullPhase::Revealed => (&cull.revealed_args, &cull.revealed_visible, &cull.keep),
        };
        let f = |v: [f64; 4]| v.map(|x| x as f32);
        let hzb = &targets.hzb;
        // Decals blend in their list's order: the view's first phase lists
        // them all, then puts them in row order (cull.wgsl).
        let decals = match phase {
            CullPhase::Shadow => 0..0,
            _ => placed.ranges.decals.clone(),
        };
        let orders = phase == CullPhase::Seen && !decals.is_empty();
        let reach = self.detail.reach.zip(eye);
        // The ground rows that bend to the terrain sit on, as the frame
        // group binds it.
        let ground = (self.presented.as_ref())
            .and_then(|s| s.terrain.as_ref())
            .and_then(|t| self.resident.get(t.first))
            .and_then(|draw| draw.mesh.heights.as_ref())
            .filter(|h| h.serial().is_some());
        let globals = placed::CullGlobals {
            rows: rows.map(f),
            planes: planes.map(f),
            // z: the view's phases count what they draw for `Drawn`.
            // y: the views' least size in pixels of `height`
            // (`Detail::min_pixels`, in device pixels). w: target pixels
            // per pixel of `height`.
            info: [
                height,
                if phase == CullPhase::Shadow {
                    0.
                } else {
                    self.detail.min_pixels / screen.max(1e-3)
                },
                if phase == CullPhase::Shadow { 0. } else { 1. },
                texels,
            ],
            counts: [
                placed.count,
                if placed.packs() { placed.row_bits() } else { 0 },
                match phase {
                    CullPhase::Shadow => 0,
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
            lit: [
                0,
                wants.map_or(0, |(section, _)| section + 1),
                decals.start,
                decals.end,
            ],
            eye: reach.map_or([0.; 4], |(_, e)| [e[0], e[1], e[2], 1.]),
            reach: reach.map_or([0.; 4], |(r, _)| {
                [r.impostor.max(0.), r.end.max(0.), r.thin.clamp(0., 1.), 0.]
            }),
            ground: ground.map_or([0.; 4], |h| {
                let g = h.source.grid();
                [
                    g.origin[0],
                    g.origin[1],
                    1. / (g.spacing * heights::CELL as f32),
                    1.,
                ]
            }),
            cells: ground.map_or([0; 4], |h| {
                let ([x, z], levels) = h.source.cell_levels();
                [x as u32, z as u32, levels as u32, 0]
            }),
            posed: [
                placed.ranges.posed.start,
                placed.ranges.posed.end,
                if phase == CullPhase::Shadow {
                    0
                } else {
                    targets.size[0].div_ceil(visbuf::POSED_TILE)
                },
                0,
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
                entry(4, buffers.lists.as_entire_binding()),
                entry(5, args.as_entire_binding()),
                entry(6, visible.as_entire_binding()),
                entry(7, wgpu::BindingResource::TextureView(&hzb.view)),
                entry(8, cull.seen.as_entire_binding()),
                entry(9, cull.totals.as_entire_binding()),
                entry(10, keep.as_entire_binding()),
                entry(
                    13,
                    wants.map_or(self.kit.dummy.buffer.as_entire_binding(), |(_, a)| {
                        a.texels.as_entire_binding()
                    }),
                ),
                entry(14, cull.wants.as_entire_binding()),
                entry(
                    15,
                    ground.map_or(self.kit.dummy.buffer.as_entire_binding(), |h| {
                        h.cells.as_entire_binding()
                    }),
                ),
                entry(16, targets.posed_tiles.as_entire_binding()),
            ],
        });
        if let Some((section, arena)) = wants {
            let cull = targets.cull.as_mut().expect("cull targets");
            if cull.wants_arena != arena.id {
                cull.wants_arena = arena.id;
                cull.written = 0;
            }
            cull.written |= 1 << section;
        }
        let timestamp_writes = targets.timing.as_mut().and_then(|t| t.compute("cull"));
        let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
            label: Some(match phase {
                CullPhase::Shadow => "editor shadow cull",
                CullPhase::Revealed => "editor occlusion cull",
                _ => "editor cull",
            }),
            timestamp_writes,
        });
        pass.set_pipeline(&self.cull[0]);
        pass.set_bind_group(0, &group, &[]);
        pass.dispatch_workgroups(placed.count.div_ceil(64), 1, 1);
        if orders {
            pass.set_pipeline(&self.cull[1]);
            pass.dispatch_workgroups(decals.len() as u32, 1, 1);
        }
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

    /// Bind group 2 of placed.wgsl: placement transforms and words, and the
    /// visible entries of the view or the shadow map.
    fn arena_draws(
        &self,
        gpu: &Gpu,
        rows: &placed::PlacedBuffers,
        visible: &wgpu::Buffer,
        keep: &wgpu::Buffer,
    ) -> wgpu::BindGroup {
        gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("editor placed"),
            layout: &self.kit.placed_layout,
            entries: &[
                entry(0, rows.transforms.as_entire_binding()),
                entry(1, visible.as_entire_binding()),
                entry(2, keep.as_entire_binding()),
                entry(3, rows.words.as_entire_binding()),
                entry(4, rows.lists.as_entire_binding()),
            ],
        })
    }

    /// Bakes every impostor's views into a new atlas, once the arena holds
    /// their meshes: one pass per impostor, its mesh drawn once per view,
    /// then its mips. Layers bake in batches into staging arrays, which
    /// are encoded into BC3 and BC5 layers where `bc`, copied as they are
    /// otherwise.
    fn bake_impostors(
        &self,
        gpu: &Gpu,
        encoder: &mut wgpu::CommandEncoder,
        placed: &mut placed::Placed,
        bc: bool,
    ) {
        use wgpu::TextureFormat as F;
        let (Some(pipelines), Some(arena), None) = (&self.arena, &placed.arena, &placed.atlas)
        else {
            return;
        };
        // Views bake the textures as they are: wait for every texel, and
        // for the levels the bake samples (`Arena::floors`).
        if !placed.unstreamed.is_empty() || placed.residency.floors_met != Some(arena.id) {
            return;
        }
        if arena.impostors.is_empty() {
            return;
        }
        let layers = arena.impostors.len() as u32;
        let tile = placed::impostor_tile(layers);
        let side = placed::IMPOSTOR_VIEWS * tile;
        let views = placed::IMPOSTOR_VIEWS * placed::IMPOSTOR_VIEWS;
        // Three halvings at most (tiles are multiples of 8): each mip's
        // tiles still start on whole texels, and its sides on whole blocks.
        let mips = tile.trailing_zeros().min(3) + 1;
        let layered = |label, format, layers, usage| {
            gpu.device.create_texture(&wgpu::TextureDescriptor {
                label: Some(label),
                size: wgpu::Extent3d {
                    width: side,
                    height: side,
                    depth_or_array_layers: layers,
                },
                mip_level_count: mips,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format,
                usage,
                view_formats: &[],
            })
        };
        // Color and coverage; octahedral normal; depth and gloss.
        let baked = [F::Rgba8Unorm, F::Rg8Unorm, F::Rg8Unorm];
        let kept = if bc {
            [F::Bc3RgbaUnorm, F::Bc5RgUnorm, F::Bc5RgUnorm]
        } else {
            baked
        };
        let usage = wgpu::TextureUsages::COPY_DST | wgpu::TextureUsages::TEXTURE_BINDING;
        let atlas = [
            layered("impostor albedo", kept[0], layers, usage),
            layered("impostor normals", kept[1], layers, usage),
            layered("impostor depth and gloss", kept[2], layers, usage),
        ];
        // Batches of at most 32 MB of staging.
        let per_layer = u64::from(side * side) * 8 * 4 / 3;
        let batch = ((32 << 20) / per_layer).clamp(1, u64::from(layers)) as u32;
        let usage = wgpu::TextureUsages::RENDER_ATTACHMENT
            | wgpu::TextureUsages::TEXTURE_BINDING
            | wgpu::TextureUsages::COPY_SRC;
        let staging = baked.map(|format| layered("impostor staging", format, batch, usage));
        // Per layer and view, the texels the view covers (cs_bounds).
        let bounds = gpu.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("impostor bounds"),
            size: wgpu::Extent3d {
                width: views,
                height: layers,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: F::Rgba8Uint,
            usage: wgpu::TextureUsages::STORAGE_BINDING | wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        });
        let bounds_view = bounds.create_view(&Default::default());
        // Cleared and discarded by each bake pass: memoryless on tile GPUs.
        let (_, depth) = texture_2d(
            gpu,
            "impostor depth",
            [side; 2],
            wgpu::TextureFormat::Depth32Float,
            wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TRANSIENT_ATTACHMENT,
        );
        // Only the albedo sampler is read.
        let frame = self.frame_group(
            gpu,
            &Globals::zeroed(),
            &Lights::zeroed(),
            None,
            None,
            None,
            None,
        );
        let layer_view = |texture: &wgpu::Texture, layer, mip| {
            texture.create_view(&wgpu::TextureViewDescriptor {
                dimension: Some(wgpu::TextureViewDimension::D2),
                base_array_layer: layer,
                array_layer_count: Some(1),
                base_mip_level: mip,
                mip_level_count: Some(1),
                ..Default::default()
            })
        };
        let array = |texture: &wgpu::Texture| {
            texture.create_view(&wgpu::TextureViewDescriptor {
                dimension: Some(wgpu::TextureViewDimension::D2Array),
                ..Default::default()
            })
        };
        let targets = |layer, mip| staging.each_ref().map(|t| layer_view(t, layer, mip));
        fn attachments(
            views: &[wgpu::TextureView; 3],
        ) -> [Option<wgpu::RenderPassColorAttachment<'_>>; 3] {
            views.each_ref().map(|view| {
                Some(wgpu::RenderPassColorAttachment {
                    view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                        store: wgpu::StoreOp::Store,
                    },
                    depth_slice: None,
                })
            })
        }
        let (mip_pipeline, bounds_pipeline) = self.impostor_mips(gpu);
        // The encoder's jobs per staged layer and mip, and their blocks.
        // Every texel is fitted, empty ones too, which drawing weighs out:
        // filtering still reads their black at a crown's edge, and with
        // blocks fitted to the leaves alone a continental view's forests
        // changed 1% of their pixels (0.2% as it is).
        let jobs: Vec<bc::Job> = (0..batch)
            .flat_map(|layer| (0..mips).map(move |mip| (layer, mip)))
            .flat_map(|(layer, mip)| {
                [bc::Kind::Rgba, bc::Kind::Rg, bc::Kind::Rg].map(|kind| bc::Job {
                    origin: [0, 0],
                    layer,
                    mip,
                    blocks: [(side >> mip) / 4; 2],
                    kind,
                })
            })
            .collect();
        let blocks = bc.then(|| {
            gpu.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("impostor blocks"),
                size: jobs.iter().map(bc::Job::bytes).sum(),
                usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC,
                mapped_at_creation: false,
            })
        });
        let sources = staging.each_ref().map(array);
        let rows = {
            let d = &self.kit.dummy.buffer;
            gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("editor impostor bake rows"),
                layout: &self.kit.placed_layout,
                entries: &[0, 1, 2, 3, 4].map(|b| entry(b, d.as_entire_binding())),
            })
        };
        for first in (0..layers).step_by(batch as usize) {
            let count = batch.min(layers - first);
            for staged in 0..count {
                let (draw, range) = &arena.impostors[(first + staged) as usize];
                let into = targets(staged, 0);
                let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("editor impostor bake"),
                    color_attachments: &attachments(&into),
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
                pass.set_pipeline(&pipelines[3]);
                pass.set_bind_group(0, &frame, &[]);
                pass.set_bind_group(2, &rows, &[]);
                pass.set_bind_group(3, &arena.group, &[]);
                pass.draw(range.clone(), draw * views..(draw + 1) * views);
            }
            // What each view covers, before the next batch bakes over it.
            {
                let info = buffer_init(
                    gpu,
                    "impostor bounds batch",
                    bytemuck::bytes_of(&[first, tile, 0, 0]),
                    wgpu::BufferUsages::UNIFORM,
                );
                let group = gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some("editor impostor bounds"),
                    layout: &bounds_pipeline.0,
                    entries: &[
                        entry(0, wgpu::BindingResource::TextureView(&sources[0])),
                        entry(1, wgpu::BindingResource::TextureView(&bounds_view)),
                        entry(2, info.as_entire_binding()),
                    ],
                });
                let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
                    label: Some("editor impostor bounds"),
                    timestamp_writes: None,
                });
                pass.set_pipeline(&bounds_pipeline.1);
                pass.set_bind_group(1, &group, &[]);
                pass.dispatch_workgroups(placed::IMPOSTOR_VIEWS, placed::IMPOSTOR_VIEWS, count);
            }
            // Mips: far trees read them instead of aliasing across the tile.
            for staged in 0..count {
                for mip in 1..mips {
                    let above = targets(staged, mip - 1);
                    let group = gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
                        label: Some("editor impostor mip"),
                        layout: &mip_pipeline.0,
                        entries: &[0, 1, 2].map(|b| {
                            entry(b, wgpu::BindingResource::TextureView(&above[b as usize]))
                        }),
                    });
                    let into = targets(staged, mip);
                    let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                        label: Some("editor impostor mip"),
                        color_attachments: &attachments(&into),
                        depth_stencil_attachment: None,
                        timestamp_writes: None,
                        occlusion_query_set: None,
                        multiview_mask: None,
                    });
                    pass.set_pipeline(&mip_pipeline.1);
                    pass.set_bind_group(0, &group, &[]);
                    pass.draw(0..3, 0..1);
                }
            }
            let Some(blocks) = &blocks else {
                for (from, to) in staging.iter().zip(&atlas) {
                    for mip in 0..mips {
                        encoder.copy_texture_to_texture(
                            wgpu::TexelCopyTextureInfo {
                                texture: from,
                                mip_level: mip,
                                origin: wgpu::Origin3d::ZERO,
                                aspect: wgpu::TextureAspect::All,
                            },
                            wgpu::TexelCopyTextureInfo {
                                texture: to,
                                mip_level: mip,
                                origin: wgpu::Origin3d {
                                    x: 0,
                                    y: 0,
                                    z: first,
                                },
                                aspect: wgpu::TextureAspect::All,
                            },
                            wgpu::Extent3d {
                                width: side >> mip,
                                height: side >> mip,
                                depth_or_array_layers: count,
                            },
                        );
                    }
                }
                continue;
            };
            let mut at = 0;
            for (k, (source, to)) in sources.iter().zip(&atlas).enumerate() {
                let mine: Vec<bc::Job> = (jobs.iter().skip(k).step_by(3))
                    .filter(|j| j.layer < count)
                    .copied()
                    .collect();
                let offsets = self
                    .bc
                    .encode(&gpu.device, encoder, source, &mine, blocks, at, None);
                for (job, &offset) in mine.iter().zip(&offsets) {
                    job.copy(
                        encoder,
                        blocks,
                        offset,
                        to,
                        job.mip,
                        [0, 0],
                        first + job.layer,
                    );
                }
                at += mine.iter().map(bc::Job::bytes).sum::<u64>();
            }
        }
        let views = atlas.each_ref().map(array);
        let group = gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("editor impostor atlas"),
            layout: &self.kit.atlas_layout,
            entries: &[
                entry(0, wgpu::BindingResource::TextureView(&views[0])),
                entry(1, wgpu::BindingResource::TextureView(&views[1])),
                entry(2, wgpu::BindingResource::TextureView(&views[2])),
                entry(3, wgpu::BindingResource::TextureView(&bounds_view)),
            ],
        });
        placed.set_atlas(placed::Atlas { group });
    }

    /// impostor_mips.wgsl: the mips' bind group layout and pipeline, and
    /// cs_bounds' (its group 1).
    fn impostor_mips(
        &self,
        gpu: &Gpu,
    ) -> (
        (wgpu::BindGroupLayout, wgpu::RenderPipeline),
        (wgpu::BindGroupLayout, wgpu::ComputePipeline),
    ) {
        let module = gpu
            .device
            .create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some("editor impostor mips"),
                source: wgpu::ShaderSource::Wgsl(include_str!("gpu/impostor_mips.wgsl").into()),
            });
        let layout = gpu
            .device
            .create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("editor impostor mips"),
                entries: &[0, 1, 2].map(|b| {
                    layout_entry(
                        b,
                        wgpu::ShaderStages::FRAGMENT,
                        sampled(true, wgpu::TextureViewDimension::D2),
                    )
                }),
            });
        let target = |format| {
            Some(wgpu::ColorTargetState {
                format,
                blend: None,
                write_mask: wgpu::ColorWrites::ALL,
            })
        };
        let pipeline =
            gpu.device
                .create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                    label: Some("editor impostor mips"),
                    layout: Some(&gpu.device.create_pipeline_layout(
                        &wgpu::PipelineLayoutDescriptor {
                            label: Some("editor impostor mips"),
                            bind_group_layouts: &[Some(&layout)],
                            immediate_size: 0,
                        },
                    )),
                    vertex: wgpu::VertexState {
                        module: &module,
                        entry_point: Some("vs_mip"),
                        buffers: &[],
                        compilation_options: Default::default(),
                    },
                    fragment: Some(wgpu::FragmentState {
                        module: &module,
                        entry_point: Some("fs_mip"),
                        targets: &[
                            target(wgpu::TextureFormat::Rgba8Unorm),
                            target(wgpu::TextureFormat::Rg8Unorm),
                            target(wgpu::TextureFormat::Rg8Unorm),
                        ],
                        compilation_options: Default::default(),
                    }),
                    primitive: wgpu::PrimitiveState::default(),
                    depth_stencil: None,
                    multisample: Default::default(),
                    multiview_mask: None,
                    cache: None,
                });
        let bounds_layout = gpu
            .device
            .create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("editor impostor bounds"),
                entries: &[
                    layout_entry(
                        0,
                        wgpu::ShaderStages::COMPUTE,
                        sampled(false, wgpu::TextureViewDimension::D2Array),
                    ),
                    layout_entry(
                        1,
                        wgpu::ShaderStages::COMPUTE,
                        wgpu::BindingType::StorageTexture {
                            access: wgpu::StorageTextureAccess::WriteOnly,
                            format: wgpu::TextureFormat::Rgba8Uint,
                            view_dimension: wgpu::TextureViewDimension::D2,
                        },
                    ),
                    layout_entry(2, wgpu::ShaderStages::COMPUTE, uniform(false)),
                ],
            });
        let bounds =
            gpu.device
                .create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
                    label: Some("editor impostor bounds"),
                    layout: Some(&gpu.device.create_pipeline_layout(
                        &wgpu::PipelineLayoutDescriptor {
                            label: Some("editor impostor bounds"),
                            bind_group_layouts: &[None, Some(&bounds_layout)],
                            immediate_size: 0,
                        },
                    )),
                    module: &module,
                    entry_point: Some("cs_bounds"),
                    compilation_options: Default::default(),
                    cache: None,
                });
        ((layout, pipeline), (bounds_layout, bounds))
    }

    /// The placements the visibility buffer leaves to the opaque pass: flat
    /// meshes, and impostors, their depth first when `prepass` (not over
    /// kept depth, which holds it). Impostors keep their depth right before
    /// their shading: crowns far off overlap many times over, and drawn
    /// ahead of the ground and the other placements, it hung the RTX 4080
    /// SUPER's driver on 1800 islands, as an earlier impostor prepass was
    /// reported to.
    fn draw_arena_opaque(
        &self,
        pass: &mut wgpu::RenderPass<'_>,
        placed: &placed::Placed,
        draws: &wgpu::BindGroup,
        args: &wgpu::Buffer,
        prepass: bool,
        revealed: bool,
        mut timing: Option<&mut Timing>,
    ) {
        let mut mark = |pass: &mut wgpu::RenderPass<'_>, label| {
            if let Some(t) = timing.as_deref_mut() {
                t.mark(pass, label);
            }
        };
        let (Some(pipelines), Some(arena)) = (&self.arena, &placed.arena) else {
            return;
        };
        let [flat, impostor_depth, impostors] =
            if revealed { [9, 10, 11] } else { [0, 5, 6] }.map(|i| &pipelines[i]);
        let ranges = &arena.ranges;
        mark(pass, Some("placed flat"));
        self.draw_arena(
            ArenaPass::Opaque,
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
            if prepass {
                mark(pass, Some("impostor depth"));
                self.draw_arena(
                    ArenaPass::Opaque,
                    pass,
                    impostor_depth,
                    arena,
                    draws,
                    args,
                    impostor,
                );
            }
            mark(pass, Some("impostors"));
            self.draw_arena(
                ArenaPass::Opaque,
                pass,
                impostors,
                arena,
                draws,
                args,
                impostor,
            );
        }
        mark(pass, None);
    }

    /// Every arena draw in `ranges` (`placed::Arena::lists`) with
    /// `pipeline`, one multi-draw each: draws the cull left empty cost the
    /// GPU little, and the CPU encodes a handful of commands instead of
    /// one per mesh.
    #[allow(clippy::too_many_arguments)]
    fn draw_arena(
        &self,
        counted: ArenaPass,
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
        let mut drawn = self.arena_drawn.get();
        drawn[counted as usize] += ranges.iter().map(|r| r.len()).sum::<usize>();
        self.arena_drawn.set(drawn);
        pass.set_pipeline(pipeline);
        pass.set_bind_group(2, draws, &[]);
        pass.set_bind_group(3, &arena.group, &[]);
        pass.set_index_buffer(arena.indices.slice(..), wgpu::IndexFormat::Uint32);
        let stride = u64::from(placed::ARGS) * 4;
        for r in ranges.iter().filter(|r| !r.is_empty()) {
            pass.multi_draw_indexed_indirect(args, u64::from(r.start) * stride, r.end - r.start);
        }
    }

    /// The scene's light, and the shadow map drawn again if what casts or
    /// receives shadows changed since it was last drawn; true when it was,
    /// and then how many cascades were.
    /// The view's cascades (clip `view_rows`) are drawn again when they or
    /// what casts moved.
    /// `casters` are the ready passes that cast (the kit's terrain among
    /// them), by their place in the scene's passes.
    #[allow(clippy::too_many_arguments)]
    fn lights(
        &mut self,
        gpu: &Gpu,
        encoder: &mut wgpu::CommandEncoder,
        scene: &PreparedScene,
        globals: Globals,
        placed: &placed::Placed,
        height_edits: u64,
        height_moves: &terrain::HeightMoves,
        targets: &mut Targets,
        ids: &wgpu::Buffer,
        slot: &mut impl FnMut([u32; 4]) -> u32,
        casters: &[(usize, &Arc<dyn Pass>, Caster)],
        view_rows: &[[f64; 4]; 4],
    ) -> (Lights, bool, usize) {
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
            tone: scene.tone.map_or([0.; 4], |t| {
                [t.exposure.max(1e-6), t.sun_occlusion, t.ambient.max(0.), 0.]
            }),
            zenith: [0.; 4],
            horizon: [0.; 4],
            sea: [0.; 4],
            tint: [0.; 4],
            occlusion: scene.occlusion.map_or([0.; 4], |o| {
                [o.radius.max(1e-3), o.strength, o.bias, o.power.max(0.)]
            }),
            terrain: [0.; 4],
            ambient: scene.ambient,
            disc: [0.; 4],
            ground_tint: scene.ground_tint.as_ref().map_or([0.; 4], |t| {
                [
                    t.origin[0],
                    t.origin[1],
                    1. / t.size[0].max(1e-6),
                    1. / t.size[1].max(1e-6),
                ]
            }),
            curve: match scene.tone.map(|t| t.curve).unwrap_or_default() {
                light::Curve::Aces {
                    lift_edge,
                    lift_power,
                    scale,
                } => [0., lift_edge, lift_power, scale],
                light::Curve::Shoulder {
                    gamma,
                    max_linear,
                    white,
                } => [1., max_linear, white, gamma],
            },
            clouds: [0.; 4],
            cloud_offsets: [0.; 4],
            cascades: [[[0.; 4]; 4]; light::MAX_CASCADES],
            cascade: [[0.; 4]; light::MAX_CASCADES],
            cascade_info: [0.; 4],
            cascade_want: [[0., 0., 1., 0.]; light::MAX_CASCADES],
            pages: [0.; 4],
            cloud_shade: [0.; 4],
            glow: [0., 0., 0., scene.glow.strength],
            host: bytemuck::cast(scene.glow.host),
            environment: [0., 0., 0., f32::from(u8::from(scene.environment.is_some()))],
            view: [
                scene.view_mode as f32,
                // A view mode's light may move (a pattern drifting): it reads the
                // clock, so the plain view's lights stay as they were.
                if scene.view_mode != 0 { clock() } else { 0. },
                0.,
                f32::from(u8::from(scene.host_texture.is_some())),
            ],
            local: [0.; 4],
            locals: [[[0.; 4]; 3]; MAX_LOCAL_LIGHTS],
            // Filled per view, once its mirror drew (`Renderer::draw`).
            reflection: [0.; 4],
            occlusion_scale: scene.occlusion.map_or([0.; 4], |o| {
                [
                    o.small_scale,
                    o.large_scale,
                    o.max_pixels.max(1.),
                    o.min_pixels.min(o.max_pixels),
                ]
            }),
        };
        let local = &scene.local_lights[..scene.local_lights.len().min(MAX_LOCAL_LIGHTS)];
        lights.local[0] = local.len() as f32;
        for (words, l) in lights.locals.iter_mut().zip(local) {
            *words = l.words();
        }
        if let Some(c) = &scene.cloud_shadows
            && let Some(time) = self.detail.cloud_shadows.time(clock())
        {
            let [a, b] = c.layers;
            let at = |l: CloudLayer| {
                [
                    l.offset[0] + l.velocity[0] * time,
                    l.offset[1] + l.velocity[1] * time,
                ]
            };
            let (oa, ob) = (at(a), at(b));
            lights.clouds = [a.scale, b.scale, c.contrast, c.power.max(1e-6)];
            lights.cloud_offsets = [oa[0], oa[1], ob[0], ob[1]];
        }
        if let Some(t) = scene.terrain.as_ref().filter(|t| t.source.tints) {
            let grid = t.source.heights.grid();
            let o = grid.origin;
            lights.tint = [o[0], o[1], grid.spacing, t.source.color_gain];
        }
        if let Some(t) = scene.terrain.as_ref() {
            let grid = t.source.heights.grid();
            lights.terrain = [grid.origin[0], grid.origin[1], grid.spacing, 1.];
        }
        if let Some(sky) = scene.sky.as_ref() {
            let ([r, g, b], [hr, hg, hb]) = (sky.zenith, sky.horizon);
            lights.zenith = [r, g, b, 1.];
            lights.horizon = [hr, hg, hb, sky.sea.map_or(0., |s| s.level)];
            if let Some(sea) = sky.sea {
                lights.sea = [sea.color[0], sea.color[1], sea.color[2], 1.];
            }
            if sky.radiance.is_some() {
                let [r, g, b] = sky.sun_disc;
                lights.disc = [r, g, b, 1.];
            }
        }
        if let Some(fog) = light.fog {
            lights.fog = [fog.color[0], fog.color[1], fog.color[2], 1.];
            lights.fog_range[..2].copy_from_slice(&[fog.start, fog.end]);
        }
        let Some(shadows) = light.shadows else {
            return (lights, false, 0);
        };
        let resolution = shadows.resolution.clamp(
            *light::SHADOW_RESOLUTION.start(),
            (*light::SHADOW_RESOLUTION.end()).min(gpu.device.limits().max_texture_dimension_2d),
        );
        // Placements and plugged casters join the kit's batches' bounds.
        // As boxes: a scene of placements alone has an empty batch box.
        let with_placed = |b: [Vec3; 2]| {
            let b = if placed.count == 0 {
                b
            } else {
                math::union(b, placed.bounds)
            };
            (casters.iter()).fold(b, |b, (.., c)| math::union(b, c.bounds))
        };
        let caster_bounds = with_placed(scene.bounds(Prepared::casts_shadow));
        // Opaque receivers only: a sea plane reaching far past the land
        // would stretch every texel (3x for an editor's sea around an
        // island), and small casters' shadows would wash out. Water outside
        // the map is lit.
        let receivers = with_placed(scene.bounds(|p| !matches!(p.material(), Material::Blend(_))));
        if (0..3).any(|a| receivers[0][a] > receivers[1][a]) {
            return (lights, false, 0);
        }
        let fit = light::fit(
            sun,
            caster_bounds,
            &math::corners(receivers),
            receivers,
            resolution,
            1.,
        );
        let Some(fit) = fit else {
            return (lights, false, 0);
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
            let (texture, view) = texture_2d(
                gpu,
                "editor shadow map",
                [resolution; 2],
                SHADOW_FORMAT,
                SHADOW_USAGE,
            );
            self.shadow_map = Some(ShadowMap {
                texture,
                view,
                resolution,
                drawn: None,
                fit: None,
                held: None,
            });
        }
        let version: ShadowVersion = (
            scene.generation,
            height_edits,
            placed.version,
            casters.iter().map(|(i, _, c)| (*i, c.version)).collect(),
        );
        // The view's cascades, nearer than the scene's map and finer.
        let count = (shadows.cascades as usize).min(light::MAX_CASCADES);
        let size = shadows.cascade_resolution.clamp(
            *light::SHADOW_RESOLUTION.start(),
            (*light::SHADOW_RESOLUTION.end()).min(gpu.device.limits().max_texture_dimension_2d),
        );
        // What the view needs of each cascade, and the wider fit drawn.
        let cascades = |margin, size| {
            light::cascades(
                sun,
                caster_bounds,
                receivers,
                view_rows,
                count,
                shadows.reach,
                size,
                margin,
            )
        };
        // Memory: drawn² × 4 bytes a layer, per viewport; 24 MB at 2048.
        let drawn_size = (f64::from(size) * CASCADE_MARGIN).round() as u32;
        let (wants, fits) = (cascades(1., size), cascades(CASCADE_MARGIN, drawn_size));
        let mut redrawn = false;
        let mut draw = ShadowDraw {
            gpu,
            encoder,
            scene,
            globals: Globals {
                // The sun is the camera here: impostors face it, so a
                // tree's shadow is its silhouette from the sun. Facing the
                // view's eye, they turned edge-on to the sun whenever the
                // view looked across it, and their shadows vanished.
                eye: [sun[0], sun[1], sun[2], 0.],
                ..globals
            },
            view_eye: (globals.eye[3] > 0.5)
                .then(|| [globals.eye[0], globals.eye[1], globals.eye[2]]),
            lights: &lights,
            placed,
            targets,
            ids,
            slot,
            casters,
        };
        let map = self.shadow_map.as_ref().expect("created above");
        if map.drawn.as_ref() != Some(&version) {
            let view = map.view.clone();
            let (drawn, same_fit) = (map.drawn.clone(), map.fit == Some(fit));
            let mut held = self.shadow_map.as_mut().expect("created above").held.take();
            let due = self.shadow_due(
                &mut held,
                drawn.as_ref(),
                same_fit,
                &version,
                (scene, casters, placed, height_moves),
            );
            if let Some(region) = due {
                self.partial_shadows += u32::from(self.draw_shadow(
                    &mut draw,
                    &fit,
                    resolution,
                    &view,
                    None,
                    1,
                    region,
                    "shadow map",
                ));
            }
            let map = self.shadow_map.as_mut().expect("created above");
            map.held = held;
            if due.is_some() {
                map.drawn = Some(version.clone());
                map.fit = Some(fit);
                redrawn = true;
            }
        }

        if fits.is_empty() {
            draw.targets.cascades = None;
            return (lights, redrawn, 0);
        }
        let cascades = &mut draw.targets.cascades;
        if cascades
            .as_ref()
            .is_none_or(|c| c.resolution != drawn_size || c.layers.len() < fits.len())
        {
            let texture = gpu.device.create_texture(&wgpu::TextureDescriptor {
                label: Some("editor shadow cascades"),
                size: wgpu::Extent3d {
                    width: drawn_size,
                    height: drawn_size,
                    depth_or_array_layers: count as u32,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: SHADOW_FORMAT,
                usage: SHADOW_USAGE,
                view_formats: &[],
            });
            let layer = |k: u32| {
                texture.create_view(&wgpu::TextureViewDescriptor {
                    dimension: Some(wgpu::TextureViewDimension::D2),
                    base_array_layer: k,
                    array_layer_count: Some(1),
                    ..Default::default()
                })
            };
            *cascades = Some(Cascades {
                texture: texture.clone(),
                view: texture.create_view(&wgpu::TextureViewDescriptor {
                    dimension: Some(wgpu::TextureViewDimension::D2Array),
                    ..Default::default()
                }),
                layers: (0..count as u32).map(layer).collect(),
                resolution: drawn_size,
                drawn: Vec::new(),
                held: Vec::new(),
                rows: None,
                relaxed: false,
                reach: None,
            });
        }
        let c = cascades.as_mut().expect("created above");
        c.drawn.resize(fits.len(), None);
        c.held.resize(fits.len(), None);
        let layers = c.layers.clone();
        // A moving view keeps a map a size step coarser than it wants:
        // fits round to quarter octaves, and a view turning to and fro
        // over a step redrew a cascade in one frame of seven (pirate
        // island). At rest each is drawn again as wanted.
        let moving = c.rows.is_some_and(|r| r != *view_rows);
        c.rows = Some(*view_rows);
        let coarser = if moving {
            CASCADE_MARGIN as f32 * 1.01
        } else {
            1.01
        };
        // Stale: the drawn map no longer holds what the view needs, or the
        // casters changed. A new scene, an edit or a different count redraws
        // every stale layer; the camera moving alone redraws one a frame,
        // nearest first, while the others shade what they still hold and
        // the shader falls through to the next map for the rest.
        // A new reach culls other casters: like an edit, every layer.
        let reached = c.reach != self.detail.reach;
        c.reach = self.detail.reach;
        let kept = |k: usize| {
            (c.drawn[k].as_ref()).is_some_and(|(f, v)| {
                // Texels as fine as wanted, give or take rounding (or a
                // step coarser while moving).
                !reached
                    && *v == version
                    && wants.len() == fits.len()
                    && f.covers(&wants[k], coarser)
            })
        };
        let stale: Vec<usize> = (0..fits.len()).filter(|&k| !kept(k)).collect();
        let edited = reached
            || (c.drawn.iter()).any(|d| d.as_ref().is_none_or(|(_, v)| *v != version))
            || wants.len() != fits.len();
        let redraw = if edited {
            &stale[..]
        } else {
            &stale[..stale.len().min(1)]
        };
        let mut cascaded = 0;
        for &k in redraw {
            let texel = Some(fits[k].texel);
            let c = draw.targets.cascades.as_mut().expect("created above");
            let drawn = c.drawn[k].as_ref().map(|(f, v)| (*f == fits[k], v.clone()));
            let mut held = c.held[k].take();
            let due = self.shadow_due(
                &mut held,
                drawn.as_ref().map(|(_, v)| v),
                drawn.as_ref().is_some_and(|(same, _)| *same),
                &version,
                (scene, casters, placed, height_moves),
            );
            c.held[k] = held;
            let Some(region) = due else {
                continue;
            };
            cascaded += 1;
            self.partial_shadows += u32::from(self.draw_shadow(
                &mut draw,
                &fits[k],
                drawn_size,
                &layers[k],
                texel,
                2 + k as u32,
                region,
                "shadow cascade",
            ));
            let c = draw.targets.cascades.as_mut().expect("created above");
            c.drawn[k] = Some((fits[k], version.clone()));
        }
        let c = draw.targets.cascades.as_mut().expect("created above");
        c.relaxed = (c.drawn.iter().zip(&wants))
            .any(|(d, w)| d.as_ref().is_some_and(|(f, _)| !f.covers(w, 1.01)));
        let c = &*c;
        let drawn: Vec<light::ShadowFit> = c.drawn.iter().flatten().map(|(f, _)| *f).collect();
        // The shader shades with the maps as drawn, and hands over between
        // cascades where the wanted squares end, as a fresh fit's would.
        for (k, f) in drawn.iter().enumerate() {
            if let Some(want) = wants.get(k) {
                lights.cascade_want[k] = f.square_of(want);
            }
            lights.cascades[k] = light::matrix(f.rows);
            let radius = shadows.softness.max(0.) / drawn_size as f32;
            lights.cascade[k] = [f.texel, f.texel * 0.5 / f.depth, radius, 0.];
        }
        lights.cascade_info[0] = drawn.len() as f32;
        (lights, redrawn, cascaded)
    }

    /// Whether a map drawn at `drawn` (with the fit it needs now when
    /// `same_fit`) draws for `version`, and over which world box when only
    /// the ground moved since (`ground_moved`): `Some(None)` draws it
    /// whole, `None` skips it under a `ShadowHold`, adding the ground's
    /// moves to `held`. Once the hold drops, the region drawn is what was
    /// held plus the moves since.
    fn shadow_due(
        &self,
        held: &mut Option<HeldShadow>,
        drawn: Option<&ShadowVersion>,
        same_fit: bool,
        version: &ShadowVersion,
        (scene, casters, placed, height_moves): (
            &PreparedScene,
            &[(usize, &Arc<dyn Pass>, Caster)],
            &placed::Placed,
            &terrain::HeightMoves,
        ),
    ) -> Option<Option<[Vec3; 2]>> {
        let moved = |from: &ShadowVersion| {
            self.ground_moved(scene, casters, from, version, placed, height_moves)
        };
        let region = match (held.take(), drawn) {
            _ if !same_fit => None,
            (Some(h), _) if h.at == *version => Some(h.region),
            (Some(h), _) => moved(&h.at).map(|m| math::union(h.region, m)),
            (None, Some(from)) => moved(from),
            (None, None) => None,
        };
        if self.shadows_held
            && let Some(region) = region
        {
            *held = Some(HeldShadow {
                at: version.clone(),
                region,
            });
            return None;
        }
        Some(region)
    }

    /// Draws every caster depth-only into `view` from the sun along `fit`:
    /// the kit's batches, the placements culled and levelled for the map's
    /// texels, then the plugged casters. `cascade` is a view cascade's
    /// world units per texel; the scene's map has none. With a `region`
    /// the map keeps what it holds outside that world box's texels and
    /// draws only the casters that reach them again; true when it did.
    #[allow(clippy::too_many_arguments)]
    fn draw_shadow(
        &self,
        d: &mut ShadowDraw<'_, '_, impl FnMut([u32; 4]) -> u32>,
        fit: &light::ShadowFit,
        resolution: u32,
        view: &wgpu::TextureView,
        cascade: Option<f32>,
        wants: u32,
        region: Option<[Vec3; 2]>,
        label: &'static str,
    ) -> bool {
        let gpu = d.gpu;
        let matrix = light::matrix(fit.rows);
        let part = region.and_then(|r| shadow_scissor(&fit.rows, resolution, r));
        if part.is_some_and(|(rect, _)| rect[2] == 0 || rect[3] == 0) {
            return true;
        }
        // Casters are culled to the texels drawn; levels still follow the
        // whole map's projection, so what is drawn matches a whole redraw.
        let frustum = Frustum::from_rows(part.as_ref().map_or(&fit.rows, |(_, rows)| rows));
        // Each map culls into the same lists: start them from the template.
        if let Some(cull) = d.targets.cull.as_ref() {
            let bytes = u64::from(d.placed.indirect_draws()) * u64::from(placed::ARGS) * 4;
            if bytes > 0 {
                d.encoder
                    .copy_buffer_to_buffer(&cull.template, 0, &cull.shadow_args, 0, bytes);
            }
        }
        // Casters take the level a shadow texel can show, from the map's
        // own projection: one texel is a pixel.
        self.cull(
            gpu,
            d.encoder,
            d.placed,
            d.targets,
            CullPhase::Shadow,
            &fit.rows,
            matrix,
            [resolution as f32, 1.],
            frustum.planes(),
            Some((wants, 1.)),
            // Cascades follow the view's eye; the scene's map serves every
            // eye and keeps all its casters.
            cascade.and(d.view_eye),
        );
        let frame = self.frame_group(
            gpu,
            &Globals {
                clip: matrix,
                ..d.globals
            },
            d.lights,
            None,
            None,
            None,
            None,
        );
        let placed = d.placed;
        let batches = self.draw_group(gpu, d.ids, self.instances.as_ref());
        let timestamp_writes = d.targets.timing.as_mut().and_then(|t| t.render(label));
        let mut pass = d.encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some(label),
            color_attachments: &[],
            depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                view,
                depth_ops: Some(wgpu::Operations {
                    load: if part.is_some() {
                        wgpu::LoadOp::Load
                    } else {
                        wgpu::LoadOp::Clear(1.)
                    },
                    store: wgpu::StoreOp::Store,
                }),
                stencil_ops: None,
            }),
            timestamp_writes,
            occlusion_query_set: None,
            multiview_mask: None,
        });
        if let Some(([x, y, w, h], _)) = part {
            // The scissor holds for every draw after, the casters' too.
            pass.set_scissor_rect(x, y, w, h);
            pass.set_pipeline(&shadow_clear(&gpu.device));
            pass.draw(0..3, 0..1);
        }
        pass.set_pipeline(&self.shadow);
        pass.set_bind_group(0, &frame, &[]);
        for (prepared, draw) in d.scene.batches.iter().zip(&self.resident) {
            if prepared.terrain || !prepared.casts_shadow() || frustum.excludes(prepared.bounds()) {
                continue;
            }
            pass.set_bind_group(1, &draw.mesh.group, &[]);
            pass.set_bind_group(2, &batches, &[(d.slot)([0; 4])]);
            draw.mesh
                .draw(&mut pass, draw.first..draw.first + draw.count);
        }
        if let (
            Some([_, shadow, impostors, _, _, _, _, shadow_opaque, ..]),
            Some(arena),
            Some(buffers),
            Some(cull),
        ) = (&self.arena, &placed.arena, &placed.buffers, &d.targets.cull)
        {
            let draws = self.arena_draws(gpu, buffers, &cull.shadow_visible, &cull.shadow_keep);
            // Casters nothing cuts out without a fragment stage: depth
            // alone, the rasteriser's fastest.
            let r = &arena.ranges;
            let args = &cull.shadow_args;
            // Posed casters as they stand while the map is drawn: a
            // view at rest keeps its maps, and their shadows hold still.
            let opaque = [r.lit_opaque[0].clone(), r.posed_whole[0].clone()];
            self.draw_arena(
                ArenaPass::Shadow,
                &mut pass,
                shadow_opaque,
                arena,
                &draws,
                args,
                &opaque,
            );
            let ranges = [
                r.lit_cutout[0].clone(),
                r.flat_casters.clone(),
                r.posed_cutout[0].clone(),
            ];
            self.draw_arena(
                ArenaPass::Shadow,
                &mut pass,
                shadow,
                arena,
                &draws,
                args,
                &ranges,
            );
            if let Some(atlas) = &placed.atlas {
                pass.set_bind_group(1, &atlas.group, &[]);
                let ranges = std::slice::from_ref(&arena.ranges.impostors);
                self.draw_arena(
                    ArenaPass::Shadow,
                    &mut pass,
                    impostors,
                    arena,
                    &draws,
                    args,
                    ranges,
                );
            }
        }
        let kit = KitRecord {
            gpu,
            device: self,
            ids: d.ids,
            scene: d.scene,
            cascade: cascade.map(|texel| (fit.rows, texel)),
            shadow_part: part.map(|(_, rows)| rows),
            paged: None,
        };
        for (_, p, _) in d.casters {
            pass.set_bind_group(0, &frame, &[]);
            let frame = pass::Record {
                kit: Some(kit),
                ..record(gpu, &d.targets.scene.1)
            };
            p.record_shadow(&frame, &mut pass);
        }
        part.is_some()
    }
}

/// Texels a partial shadow redraw adds round its region: rounding, and
/// triangles whose texel centres fall just inside.
const SHADOW_PAD: f64 = 2.;
/// A partial redraw over more of a map than this draws it whole: the
/// casters it would skip are few, and the clear costs a pass of its own.
const SHADOW_PARTIAL_MAX: f64 = 0.5;

/// The texels of a shadow map (clip `rows`, `resolution`²) under world box
/// `region`, padded, as a scissor rect (x, y, width, height; empty when the
/// box misses the map), and the rows narrowed to that rect for culling.
/// None when it covers most of the map, which then draws whole.
fn shadow_scissor(
    rows: &[[f64; 4]; 4],
    resolution: u32,
    region: [Vec3; 2],
) -> Option<([u32; 4], [[f64; 4]; 4])> {
    let (mut lo, mut hi) = ([f64::MAX; 2], [f64::MIN; 2]);
    for c in math::corners(region) {
        let at = |r: &[f64; 4]| r[0] * c[0] as f64 + r[1] * c[1] as f64 + r[2] * c[2] as f64 + r[3];
        let w = at(&rows[3]);
        for a in 0..2 {
            let v = at(&rows[a]) / w;
            lo[a] = lo[a].min(v);
            hi[a] = hi[a].max(v);
        }
    }
    let res = f64::from(resolution);
    // Clip x runs left to right across the texels, clip y up while rows run
    // down.
    let x0 = ((lo[0] * 0.5 + 0.5) * res - SHADOW_PAD)
        .floor()
        .clamp(0., res);
    let x1 = ((hi[0] * 0.5 + 0.5) * res + SHADOW_PAD)
        .ceil()
        .clamp(0., res);
    let y0 = ((0.5 - hi[1] * 0.5) * res - SHADOW_PAD)
        .floor()
        .clamp(0., res);
    let y1 = ((0.5 - lo[1] * 0.5) * res + SHADOW_PAD)
        .ceil()
        .clamp(0., res);
    if x1 <= x0 || y1 <= y0 {
        return Some(([0; 4], *rows));
    }
    if (x1 - x0) * (y1 - y0) > res * res * SHADOW_PARTIAL_MAX {
        return None;
    }
    // Clip x' = (2x - (a + b) w) / (b - a) spans the rect's [a, b] as x
    // spans the map, so the frustum's planes bound the rect.
    let narrow = |r: [f64; 4], a: f64, b: f64| -> [f64; 4] {
        std::array::from_fn(|i| (2. * r[i] - (a + b) * rows[3][i]) / (b - a))
    };
    let mut narrowed = *rows;
    narrowed[0] = narrow(rows[0], x0 / res * 2. - 1., x1 / res * 2. - 1.);
    narrowed[1] = narrow(rows[1], 1. - y1 / res * 2., 1. - y0 / res * 2.);
    let rect = [x0, y0, x1 - x0, y1 - y0].map(|v| v as u32);
    Some((rect, narrowed))
}

/// Depth 1 over the scissored texels of a shadow map, as its clear writes
/// everywhere: what a partial redraw starts from.
fn shadow_clear(d: &wgpu::Device) -> wgpu::RenderPipeline {
    per_device(d, "editor shadow clear", || {
        let module = d.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("editor shadow clear"),
            source: wgpu::ShaderSource::Wgsl(
                "@vertex fn vs(@builtin(vertex_index) i: u32) -> @builtin(position) vec4<f32> {
                    let p = vec2<f32>(f32((i << 1u) & 2u), f32(i & 2u));
                    return vec4<f32>(p * 2.0 - 1.0, 1.0, 1.0);
                }"
                .into(),
            ),
        });
        let layout = d.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("editor shadow clear"),
            bind_group_layouts: &[],
            immediate_size: 0,
        });
        d.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("editor shadow clear"),
            layout: Some(&layout),
            vertex: wgpu::VertexState {
                module: &module,
                entry_point: Some("vs"),
                buffers: &[],
                compilation_options: Default::default(),
            },
            fragment: None,
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: Some(wgpu::DepthStencilState {
                format: SHADOW_FORMAT,
                depth_write_enabled: Some(true),
                depth_compare: Some(wgpu::CompareFunction::Always),
                stencil: Default::default(),
                bias: Default::default(),
            }),
            multisample: Default::default(),
            multiview_mask: None,
            cache: None,
        })
    })
}

/// What every shadow map of a frame draws with (`Device::draw_shadow`).
struct ShadowDraw<'a, 'e, F> {
    gpu: &'a Gpu,
    encoder: &'e mut wgpu::CommandEncoder,
    scene: &'a PreparedScene,
    /// The sun's camera but for the clip matrix, which each map sets.
    globals: Globals,
    /// The view's own eye, which `Detail::reach` measures from.
    view_eye: Option<[f32; 3]>,
    lights: &'a Lights,
    placed: &'a placed::Placed,
    targets: &'e mut Targets,
    ids: &'a wgpu::Buffer,
    slot: &'e mut F,
    casters: &'a [(usize, &'a Arc<dyn Pass>, Caster)],
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
    // frames wait for theirs, until each has its answer: on the offscreen
    // device other threads share, a mapping's callback can run on the
    // thread whose poll found it done, just after this poll returned.
    // Bounded, so a lost device cannot hold a thread for good.
    if wait {
        let settled = || (targets.readbacks.iter()).all(|r| r.state.load(Ordering::Acquire) != 1);
        let started = std::time::Instant::now();
        while !settled() && started.elapsed() < std::time::Duration::from_secs(10) {
            let _ = gpu.device.poll(wgpu::PollType::Wait {
                submission_index: None,
                timeout: Some(std::time::Duration::from_secs(1)),
            });
            if !settled() {
                std::thread::yield_now();
            }
        }
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
                ReadKind::Wants {
                    arena,
                    slots,
                    sections,
                    view,
                } => slot.wants.take(arena, slots, sections, view, words),
                ReadKind::Counts => {
                    let drawn = &mut slot.drawn;
                    drawn.placed = std::array::from_fn(|l| words[l] as usize);
                    drawn.placed_triangles = words[MAX_LEVELS] as usize;
                }
                ReadKind::Times(labels) => {
                    let stamps: &[u64] = bytemuck::cast_slice(&data[..]);
                    let mut passes = [("", 0.); TIMED_PASSES as usize];
                    for ((p, label), out) in stamps.chunks(2).zip(labels).zip(&mut passes) {
                        if p[0] != 0 && p[1] >= p[0] {
                            let ms = (p[1] - p[0]) as f32 * period / 1e6;
                            *out = (label, ms);
                        }
                    }
                    let valid = || stamps.chunks(2).filter(|p| p[0] != 0 && p[1] >= p[0]);
                    let span = match (valid().map(|p| p[0]).min(), valid().map(|p| p[1]).max()) {
                        (Some(first), Some(last)) => (last - first) as f32 * period / 1e6,
                        _ => stats.gpu_span_ms,
                    };
                    if slot.hidden {
                        stats.mirror_ms = passes.iter().map(|p| p.1).sum();
                    } else {
                        stats.passes = passes;
                        stats.gpu_span_ms = span;
                    }
                    slot.times.add(&passes, span);
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
        let mut state = self.lock();
        let State { slots, stats, .. } = &mut *state;
        if let Some(s) = slots.get_mut(slot) {
            take_readbacks(gpu, s, stats, true);
        }
        self.answer(&state);
    }

    /// Records view `slot` into `encoder`, drawing into `target` (`size`
    /// pixels) at `bounds` within `clip`, after its mirror when the scene
    /// has one (`Reflection`): the mirror's slot draws the same bounds from
    /// the mirrored camera first, and the view binds what it drew. False
    /// when nothing was drawn (no scene shown yet), so `target` holds
    /// nothing new. `clear`: nothing in `target` is to be kept, so the post
    /// pass clears all of it instead of loading it; when nothing was drawn
    /// the caller still clears it itself.
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
        timed: bool,
        clear: bool,
    ) -> bool {
        let mirror = {
            let mut state = self.lock();
            let reflection = state.scene.as_ref().and_then(|s| s.reflection);
            let view = state.slots.get(slot).and_then(|s| s.view);
            match (reflection, view) {
                (Some(r), Some(view)) if view.camera.is_perspective() => {
                    let mirror = match state.slots[slot].mirror {
                        Some(m) => m,
                        None => {
                            // From the panes' count: a pane made later
                            // takes the next one (`Renderer::viewport`).
                            let m = (self.slots).fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            if state.slots.len() <= m {
                                state.slots.resize_with(m + 1, Slot::default);
                            }
                            state.slots[m].hidden = true;
                            state.slots[slot].mirror = Some(m);
                            m
                        }
                    };
                    state.slots[mirror].view = Some(View {
                        camera: view.camera.mirrored(r.level, view.extent),
                        ..view
                    });
                    Some(mirror)
                }
                // Orthographic views have no eye to mirror.
                _ => None,
            }
        };
        if let Some(mirror) = mirror {
            // A mirror never reaches the post pass: it leaves `target` alone.
            self.draw_slot(
                mirror, gpu, encoder, target, format, size, bounds, clip, timed, false,
            );
        }
        self.draw_slot(
            slot, gpu, encoder, target, format, size, bounds, clip, timed, clear,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn draw_slot(
        &self,
        slot: usize,
        gpu: &Gpu,
        encoder: &mut wgpu::CommandEncoder,
        target: &wgpu::TextureView,
        format: wgpu::TextureFormat,
        size: [u32; 2],
        bounds: Bounds<ScaledPixels>,
        clip: Bounds<ScaledPixels>,
        timed: bool,
        clear: bool,
    ) -> bool {
        let _span = crate::profile::span("gpu draw (cpu encode)");
        let mut state = self.lock();
        state.move_to(&gpu.device);
        if !state.opened
            && (state.device.as_ref()).is_some_and(|d| d.presented.is_some())
            && !state.pending()
        {
            state.opened = true;
        }
        let _opening = Opening::begin(!state.opened);
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
        // Uploads still coming as the frame starts: what it draws is not
        // what the next frame will, so it keeps no layers.
        let unsettled = state.uploading();
        let State {
            scene,
            assets,
            device,
            slots,
            stats,
            overlay,
            overlay_version,
            outline,
            outline_version,
            render_scale,
            detail,
            ..
        } = &mut *state;
        let detail = *detail;
        let outline_version = *outline_version;
        let mirror_scale = if slots.get(slot).is_some_and(|s| s.hidden) {
            MIRROR_SCALE
        } else {
            1.
        };
        let render_scale = render_scale.unwrap_or(1.) * mirror_scale;
        if let Some(s) = slots.get_mut(slot) {
            let _span = crate::profile::span("gpu readbacks");
            take_readbacks(gpu, s, stats, false);
        }
        // The frame's encoding; returns early, false, when there is nothing
        // to draw.
        let drew = (|| {
            // Once a frame, the levels the views' culls asked for last.
            let placed = &mut assets.placed;
            if new_frame
                && let (Some(device), Some(arena)) = (device.as_ref(), placed.arena.as_mut())
            {
                let _span = crate::profile::span("gpu residency");
                let stepped = placed.residency.step(
                    gpu,
                    encoder,
                    arena.id,
                    &arena.textures,
                    &arena.floors,
                    slots.iter_mut().map(|s| &mut s.wants),
                    &mut Budget::frame(timed),
                );
                if stepped.rebind {
                    arena.bind(gpu, &device.kit);
                }
                // Kept layers drew without the level; shadow maps redraw
                // only when they had asked for it.
                assets.writes += u64::from(stepped.grew);
                placed.version += u64::from(stepped.shadows);
            }
            // The mirror this view reflects, drawn just before it.
            let mirrored = (slots.get(slot))
                .and_then(|s| slots.get(s.mirror?))
                .and_then(|m| m.targets.as_ref())
                .map(|t| t.hdr.1.clone());
            let Some(slot) = slots.get_mut(slot) else {
                return false;
            };
            let Some(view) = slot.view else {
                return false;
            };
            let hidden = slot.hidden;
            // The scene draws into targets of its own, the pane's size at the
            // render scale, from their corner; the post stage stretches that
            // over the pane in the window's target, and overlays draw over
            // the pane itself. Window-sized targets cost every full-target
            // step (clears, the depth copy, the pyramid's first level) the
            // window's pixels: 5120x2880 for a 3660x2624 pane at 5K.
            let (pane, pane_clip, window) = (bounds, clip, size);
            let sp = |v: ScaledPixels| f64::from(v) as f32;
            let shrink = |b: Bounds<ScaledPixels>| {
                let o = bounds.origin;
                let at =
                    |v: ScaledPixels, o: ScaledPixels| ScaledPixels((sp(v) - sp(o)) * render_scale);
                Bounds::new(
                    point(at(b.origin.x, o.x), at(b.origin.y, o.y)),
                    gpui::size(
                        ScaledPixels(sp(b.size.width) * render_scale),
                        ScaledPixels(sp(b.size.height) * render_scale),
                    ),
                )
            };
            let (bounds, clip) = (shrink(bounds), shrink(clip));
            // Rounded up to 64 pixels, so dragging a dock edge does not
            // reallocate the targets every frame.
            let size = [bounds.size.width, bounds.size.height]
                .map(|v| (sp(v).ceil().max(1.) as u32).next_multiple_of(64));
            if device.as_ref().is_some_and(|d| d.format != format) {
                // Meshes hold bind groups made against the old device's kit.
                *device = None;
                assets.meshes.clear();
                assets.placed.release();
            }
            let device =
                device.get_or_insert_with(|| Device::new(gpu, format, &assets.placed.families));
            // No 3D without the arena: placements reach the screen only
            // through it. The host says so in the pane (`supported`).
            if device.kit.arena.is_none() {
                return false;
            }
            device.detail = detail;
            device.shadows_held = self.shadow_holds.load(std::sync::atomic::Ordering::Relaxed) > 0;
            // A mirror can't read the target it draws into.
            device.reflection = if hidden { None } else { mirrored };
            if slot.targets.as_ref().is_some_and(|t| t.size != size) {
                slot.targets = None;
            }
            let targets = slot
                .targets
                .get_or_insert_with(|| Targets::new(gpu, &device.kit, size));
            let mut budget = Budget::frame(timed);
            // Before anything shows, the ground mounts with the first scene
            // and streams its layers ahead of the meshes, as the kit's own
            // splat did with its batch. Mounted once the scene presented,
            // its layers' texels waited in memory through the placements'
            // texture reads: 71 MB more at the open's peak on 1800's
            // moderate island. Later scenes mount theirs as they present.
            if let Some(scene) = scene
                && device.presented.is_none()
            {
                device.mount_ground(gpu, scene);
            }
            let refreshed = device.prepare_ground(&mut terrain::GroundPrepare {
                gpu,
                encoder,
                budget: &mut budget,
            });
            assets.writes += u64::from(refreshed);
            if let Some(scene) = scene {
                let _span = crate::profile::span("gpu scene update");
                device.update(gpu, scene, assets, &mut budget);
            }
            if device.overlay_for != *overlay_version {
                device.overlay_segments = segments(overlay);
                device.upload_lines(gpu);
                device.overlay_for = *overlay_version;
            }
            {
                let _span = crate::profile::span("gpu sync");
                let mut frame = terrain::GroundPrepare {
                    gpu,
                    encoder,
                    budget: &mut budget,
                };
                assets.sync_heights(&mut frame);
                for edit in assets.ground_edits.drain(..) {
                    if let Some(pages) = &mut device.ground_pages {
                        pages.invalidate(edit);
                    }
                }
                assets.sync_placed(gpu, &device.kit, &mut Budget::frame(timed));
                device.select(gpu, encoder, &assets.placed);
                let _span = crate::profile::span("gpu sync: impostor bake");
                let unbaked = assets.placed.atlas.is_none();
                let bc = assets.bc(gpu);
                device.bake_impostors(gpu, encoder, &mut assets.placed, bc);
                assets.writes += u64::from(unbaked && assets.placed.atlas.is_some());
            }
            if !assets.placed.pending()
                && RELEASED.swap(false, std::sync::atomic::Ordering::Relaxed)
            {
                trim_heap();
            }
            let height_edits = assets.height_edits;
            let height_moves = &assets.height_moves;
            let writes = assets.writes;
            let placed = &assets.placed;
            let Some(scene) = device.presented.clone() else {
                return false;
            };
            let camera = view.camera;
            let factor = (f64::from(bounds.size.width) as f32) / f32::from(view.bounds.size.width);
            let rows = camera.view_rows(view.bounds, view.extent, true);
            let anchor = camera.anchor(view.extent);
            // A mirror clips at its plane: depth is `w − k·(y − level)`
            // over `w`, so the plane is the near plane (`MIRROR_SLOPE`).
            // Shadow cascades and ground pages fit the view's own rows.
            let (view_rows, rows) = match (hidden, scene.reflection) {
                (true, Some(r)) => {
                    let mut clipped = rows;
                    let (w, k) = (rows[3], MIRROR_SLOPE);
                    clipped[2] = [w[0], w[1] - k, w[2], w[3] + k * f64::from(r.level)];
                    (rows, clipped)
                }
                _ => (rows, rows),
            };
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
                        view_rows[2][3] as f32
                    } else {
                        camera.depth_span(view.extent)
                    },
                ],
                display: [sp(pane.origin.x), sp(pane.origin.y), render_scale, 0.],
            };
            // What the post stage and overlays draw with: the pane in the
            // window's target.
            let pane_globals = {
                let clip = crate::camera::to_target(rows, pane, window);
                Globals {
                    clip,
                    unclip: math::inverse(&bytemuck::cast(clip)).map_or(clip, bytemuck::cast),
                    viewport: [
                        window[0] as f32,
                        window[1] as f32,
                        sp(pane.size.width) / f32::from(view.bounds.size.width),
                        globals.viewport[3],
                    ],
                    ..globals
                }
            };
            let [ux, uy, uw, uh] = scissor(pane_clip, window);
            // The shadow map, each cascade, opaque and blend passes each
            // draw the scene once. Placements draw from the arena and take
            // no slots.
            let maps = 3 + light::MAX_CASCADES;
            let capacity = maps * device.resident.len() + 1;
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
            // Device pixels per logical one, for Detail::min_pixels.
            let screen = sp(pane.size.height) / height.max(1.);
            let view_eye = camera.eye(view.bounds, view.extent);
            // Placements draw once their arena is up: until then their
            // pieces hold no geometry of their own (placed.rs).
            let placing = placed.count > 0 && placed.buffers.is_some() && placed.arena.is_some();
            // Posed placements play by the clock passes animate by, and
            // draw apart from the rest (`draw_posed`).
            if let Some(arena) = &placed.arena {
                arena.set_clock(gpu, clock());
            }
            let posed = placing
                && placed
                    .arena
                    .as_ref()
                    .is_some_and(|a| !a.ranges.posed.is_empty());
            let [sx, sy, sw, sh] = scissor(clip, size);
            // The view's cull, early as the GPU likes it, unless the frame
            // may keep the lists (`Reuse`): its view and versions are the
            // kept geometry's. What the rest of the key holds is known only
            // once passes and shadows are ready, so a frame that then draws
            // after all culls late, and one that culled draws whole.
            let dragging = placed.selected.drag.is_some() && !placed.selected.rows.is_empty();
            let pending =
                unsettled || placed.pending() || device.passes_loading || device.controls_filling();
            let keeping = KEEP_LAYERS.load(std::sync::atomic::Ordering::Relaxed);
            let generation = device.presented.as_ref().map_or(0, |p| p.generation);
            let may_keep = keeping
                && !pending
                && !dragging
                && targets.layers.geometry.as_ref().is_some_and(|g| {
                    g.globals == globals
                        && g.scissor == [sx, sy, sw, sh]
                        && g.versions == [generation, writes, placed.version]
                        && g.detail == device.detail.cull_key()
                });
            let culled = placing && !may_keep;
            if placing {
                device.prepare_cull(gpu, placed, targets);
            }
            // Occlusion culls in two phases around the opaque pass: the
            // first draws what the view saw last frame.
            if culled {
                device.reset_cull(gpu, encoder, placed, targets);
                device.cull(
                    gpu,
                    encoder,
                    placed,
                    targets,
                    CullPhase::Seen,
                    &rows,
                    globals.clip,
                    [height, screen],
                    frustum.planes(),
                    // In the pixels the scene draws at. With occlusion the
                    // first phase answers for decals, the second for the
                    // rest (cull.wgsl), into one section.
                    Some((0, factor)),
                    view_eye,
                );
            }
            // The scene's own passes, readied before the shadow map and any
            // of them records, the opaque ones included; those still
            // loading sit this frame out.
            // The kit's ground, when listed, is `passes[0]`, readied last:
            // a ready Opaque pass that replaces takes its place.
            let kit_ground = usize::from(scene.ground.is_some());
            let ready: Vec<&Arc<dyn Pass>> = {
                let (loading, ready) = {
                    let mut frame = pass::Prepare {
                        device: &gpu.device,
                        queue: &gpu.queue,
                        encoder: &mut *encoder,
                        frame_layout: &device.kit.frame_layout,
                        time: clock(),
                        cloud_time: device.detail.cloud_shadows.time(clock()),
                        focus: physical_focus(&globals),
                        eye: (globals.eye[3] > 0.5)
                            .then(|| [globals.eye[0], globals.eye[1], globals.eye[2]]),
                        unclip: globals.unclip,
                        size,
                        timed,
                        kit: Some(KitPrepare {
                            gpu,
                            device,
                            ids: &ids,
                            targets: &mut *targets,
                            slot: &mut push,
                            scene: &scene,
                            frustum: &frustum,
                            rows: &rows,
                            height,
                        }),
                    };
                    // A mirror draws opaque surfaces alone: its passes'
                    // other stages are left to the view's own frame.
                    let open = (scene.passes[kit_ground..].iter()).filter(|p| match p.stage() {
                        Stage::Opaque => true,
                        Stage::Water | Stage::Sky | Stage::Blend | Stage::Overlay => !hidden,
                        _ => false,
                    });
                    let (mut ready, loading): (Vec<_>, Vec<_>) = open.partition(|p| {
                        let _span = crate::profile::span(match p.stage() {
                            Stage::Water => "gpu pass prepare: water",
                            Stage::Sky => "gpu pass prepare: sky",
                            _ => "gpu pass prepare: other",
                        });
                        p.prepare(&mut frame)
                    });
                    let replaced =
                        (ready.iter()).any(|p| p.stage() == Stage::Opaque && p.replaces());
                    if let Some(ground) = scene.passes[..kit_ground].first()
                        && !replaced
                        && ground.prepare(&mut frame)
                    {
                        ready.insert(0, ground);
                    }
                    (!loading.is_empty(), ready)
                };
                device.passes_loading = loading
                    || device
                        .surface
                        .as_ref()
                        .is_some_and(|s| s.uploading || s.ground.bind_group().is_none());
                ready
            };
            let plugged = |stage: Stage| ready.iter().filter(move |p| p.stage() == stage);
            device.cloud_shade = (ready.iter())
                .filter(|_| device.detail.cloud_shadows != CloudMotion::Off)
                .find_map(|p| p.cloud_shade());
            let casters: Vec<(usize, &Arc<dyn Pass>, Caster)> = (scene.passes.iter())
                .enumerate()
                .filter(|(_, p)| ready.iter().any(|r| Arc::ptr_eq(r, p)))
                .filter_map(|(i, p)| Some((i, p, p.shadow()?)))
                .collect();
            let (lights, redrawn, cascaded) = {
                let _span = crate::profile::span("gpu lights + shadow pass");
                device.lights(
                    gpu,
                    encoder,
                    &scene,
                    globals,
                    placed,
                    height_edits,
                    height_moves,
                    targets,
                    &ids,
                    &mut push,
                    &casters,
                    &view_rows,
                )
            };
            slot.drawn.shadow = redrawn;
            slot.drawn.cascades = cascaded;
            let mut lights = lights;
            if let (false, Some(r), Some(_)) = (hidden, scene.reflection, &device.reflection) {
                lights.reflection = [sp(bounds.size.height), 1., r.level, MIRROR_SCALE];
            }
            // The kit's far sea, unless a ready Water pass draws its own
            // instead: the backdrop then fills under the horizon (fs_sky).
            let sea = lights.sea[3] > 0.5 && !plugged(Stage::Water).any(|p| p.replaces());
            if !sea {
                lights.sea[3] = 0.;
            }
            if let Some(s) = &device.cloud_shade {
                lights.cloud_shade = [s.origin[0], s.origin[1], 1. / s.size.max(1e-6), 1.];
            }
            // The ground's pages, when the kit's ground draws: baked into
            // before the opaque pass reads them.
            let ground_drawn = scene.ground.is_some()
                && ready
                    .first()
                    .is_some_and(|p| Arc::ptr_eq(p, &scene.passes[0]));
            if ground_drawn {
                let (pages, counts) = device.ground_pages(
                    gpu,
                    encoder,
                    targets,
                    &scene,
                    &frustum,
                    &view_rows,
                    // Pages match the pixels the scene draws at.
                    height * factor,
                    &globals,
                    &lights,
                    &ids,
                    &mut push,
                    assets.bc(gpu),
                );
                lights.pages = pages;
                slot.drawn.pages = counts;
                // The pool is the views': a page baked for one can take
                // the place of one another view's kept layers show.
                assets.writes += u64::from(counts.new > 0);
            }
            let cascades = targets.cascades.as_ref().map(|c| c.view.clone());
            let shadow = (lights.sun[3] > 0.)
                .then_some(device.shadow_map.as_ref())
                .flatten()
                .map(|m| {
                    (
                        &m.view,
                        cascades.as_ref().unwrap_or(&device.kit.dummy.cascades),
                    )
                });
            let visible = scene
                .batches
                .iter()
                .zip(&device.resident)
                .filter(|(p, _)| !p.terrain && !frustum.excludes(p.bounds()))
                .collect::<Vec<_>>();
            let mut blended = visible
                .iter()
                .filter_map(|&(p, draw)| match p.material() {
                    Material::Blend(b) => Some((p, draw, b.additive)),
                    _ => None,
                })
                .map(|(p, draw, additive)| {
                    let bounds = p.bounds();
                    let center = std::array::from_fn(|a| (bounds[0][a] + bounds[1][a]) * 0.5);
                    (camera.depth(center, view.extent), draw, additive)
                })
                .collect::<Vec<_>>();
            // Farthest first.
            blended.sort_by(|a, b| a.0.total_cmp(&b.0));
            // Mesh decals as batches, over models: a model view's ground
            // tiles and a building's ground decal, with no placement to draw
            // them. Those on the ground drew in the opaque pass. Walked where
            // drawn, not collected: a Vec a frame for what is most often
            // none.
            let decal_batches = || {
                (visible.iter())
                    .filter(|(p, _)| matches!(p.material(), Material::Decal(d) if !d.ground))
                    .map(|&(_, draw)| draw)
            };
            {
                let drawn = &mut slot.drawn;
                drawn.batches = visible.len();
                drawn.instances = visible.iter().map(|(_, d)| d.count as usize).sum();
                drawn.triangles = (visible.iter())
                    .map(|(_, d)| d.mesh.count as usize / 3 * d.count as usize)
                    .sum();
                if let Some(ground) = &scene.ground
                    && ready
                        .first()
                        .is_some_and(|p| Arc::ptr_eq(p, &scene.passes[0]))
                {
                    drawn.triangles += ground.triangles();
                }
            }
            let markers = device.markers.as_ref().map_or(0, |m| m.1);
            if sw == 0 || sh == 0 {
                return false;
            }
            // The blend and occlusion passes sample a copy of the opaque
            // depth, which also holds it for frames that keep their layers.
            let water = sea || plugged(Stage::Water).next().is_some();
            let blend_passes = plugged(Stage::Blend).next().is_some();
            let copies = scene.occlusion.is_some() || !blended.is_empty() || water || blend_passes;
            // What this frame's opaque layers draw from, and so what of the
            // last frames' it can keep (`Layers`). No key when something
            // is still arriving or moving on its own: uploads, a pass still
            // loading or without a version, a drag of the selection, ground
            // pages baking.
            let pending = pending || device.passes_loading;
            let passes: Option<Vec<(usize, u64)>> = plugged(Stage::Opaque)
                .map(|p| Some((key(p), p.version()?)))
                .collect();
            let geometry = passes
                // A mirror restores nothing from copies: kept, it is
                // simply not drawn again (below).
                .filter(|_| keeping && (copies || hidden) && !pending && !dragging)
                .filter(|_| assets.writes == writes)
                .map(|passes| GeometryKey {
                    globals,
                    scissor: [sx, sy, sw, sh],
                    versions: [scene.generation, writes, placed.version],
                    passes,
                    occlusion: [lights.occlusion, lights.occlusion_scale],
                    detail: device.detail.cull_key(),
                });
            // A Blend-stage pass that applies the occlusion itself, over
            // every pixel as it draws, spares the kit's full-screen pass
            // for it (Pass::applies_occlusion): it needs two-source
            // blending to multiply a color over the target besides its own.
            let occlusion_later = scene.occlusion.is_some()
                && (gpu.device.features()).contains(wgpu::Features::DUAL_SOURCE_BLENDING)
                && plugged(Stage::Blend).any(|p| p.applies_occlusion());
            // A shadow map drawn this frame holds what no key names.
            let key = geometry
                .clone()
                .filter(|_| !redrawn && cascaded == 0)
                .map(|geometry| LayerKey {
                    geometry,
                    lights,
                    casters: casters.iter().map(|(i, _, c)| (*i, c.version)).collect(),
                    shade: device.cloud_shade.as_ref().map(|s| s.version),
                    occlusion_later,
                });
            let reuse = match (culled, hidden) {
                (true, _) => Reuse::Nothing,
                // A mirror keeps no depth copy to shade kept geometry
                // over: it draws again unless nothing changed at all.
                (false, true) => match targets.layers.reuse(geometry.as_ref(), key.as_ref()) {
                    Reuse::Whole => Reuse::Whole,
                    _ => Reuse::Nothing,
                },
                (false, false) => targets.layers.reuse(geometry.as_ref(), key.as_ref()),
            };
            // A mirror under the key it last drew under still holds that
            // frame in its HDR target, which only its view reads: nothing
            // to draw.
            if hidden && reuse == Reuse::Whole {
                slot.drawn.reuse = reuse;
                stats.mirror_ms = 0.;
                gpu.queue.write_buffer(&ids, 0, &slots_.bytes);
                return false;
            }
            // A key seen twice in a row: the view rests, keep its shading.
            let keep = reuse != Reuse::Whole && key.is_some() && targets.layers.last == key;
            slot.drawn.reuse = reuse;
            if placing && !culled && reuse == Reuse::Nothing {
                device.reset_cull(gpu, encoder, placed, targets);
                device.cull(
                    gpu,
                    encoder,
                    placed,
                    targets,
                    CullPhase::Seen,
                    &rows,
                    globals.clip,
                    [height, screen],
                    frustum.planes(),
                    // In the pixels the scene draws at. With occlusion the
                    // first phase answers for decals, the second for the
                    // rest (cull.wgsl), into one section.
                    Some((0, factor)),
                    view_eye,
                );
            }
            let page_index = (lights.pages[3] > 0.)
                .then(|| targets.pages.as_ref().map(|p| p.view.clone()))
                .flatten();
            let opaque_frame = device.frame_group(
                gpu,
                &globals,
                &lights,
                shadow,
                None,
                page_index.as_ref(),
                None,
            );
            let batches = device.draw_group(gpu, &ids, device.instances.as_ref());
            // Kept layers: the opaque depth back from its copy, which the
            // water wrote over, and when the whole of them is kept the HDR
            // target as the occlusion stage left it (kept.wgsl).
            let kept_group = (reuse != Reuse::Nothing).then(|| {
                let hdr = (targets.layers.hdr.as_ref()).map_or(&device.kit.dummy.black, |h| &h.1);
                gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some("editor kept layers"),
                    layout: &device.kept.0,
                    entries: &[
                        entry(0, wgpu::BindingResource::TextureView(&targets.depth_copy.1)),
                        entry(1, wgpu::BindingResource::TextureView(hdr)),
                    ],
                })
            });
            // Without posed placements, which draw over them first, the
            // water and blend pass restores them as its first draw
            // (`restore_late`): a tile GPU would store both targets here and
            // load them back there. Nothing between reads them: the pick
            // reads the depth's copy instead.
            let restore_late = reuse == Reuse::Whole && !posed;
            if reuse == Reuse::Whole
                && !restore_late
                && let Some(group) = &kept_group
            {
                let timestamp_writes = targets
                    .timing
                    .as_mut()
                    .and_then(|t| t.render("kept layers"));
                let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("editor kept layers"),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: &targets.hdr.1,
                        resolve_target: None,
                        ops: wgpu::Operations {
                            load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                            store: wgpu::StoreOp::Store,
                        },
                        depth_slice: None,
                    })],
                    depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                        view: &targets.depth.1,
                        depth_ops: Some(wgpu::Operations {
                            load: wgpu::LoadOp::Clear(0.),
                            store: wgpu::StoreOp::Store,
                        }),
                        stencil_ops: None,
                    }),
                    timestamp_writes,
                    occlusion_query_set: None,
                    multiview_mask: None,
                });
                pass.set_pipeline(&device.kept.1[1]);
                pass.set_bind_group(0, group, &[]);
                pass.draw(0..3, 0..1);
            }
            if reuse == Reuse::Whole
                && posed
                && let Some((kept, _)) = &targets.layers.ids
            {
                encoder.copy_texture_to_texture(whole(kept), whole(&targets.ids.0), kept.size());
            }
            // Placements' bind group 2 for the cull's first phase, and its
            // indirect arguments.
            let seen_draws = |targets: &Targets| {
                let (buffers, cull) = (placed.buffers.as_ref()?, targets.cull.as_ref()?);
                let group = device.arena_draws(gpu, buffers, &cull.visible, &cull.keep);
                Some((group, cull.args.clone()))
            };
            // The visibility buffer starts the frame's depth; its resolve
            // shades the placements after the opaque pass has drawn the
            // ground and the decals on it, as the game draws models after
            // them. Kept geometry keeps the buffer and resolves it again.
            let mut resolving = None;
            if placing
                && reuse != Reuse::Whole
                && let Some((draws, args)) = seen_draws(targets)
            {
                let scissor = [sx, sy, sw, sh];
                let kept = reuse == Reuse::Geometry;
                if !kept {
                    device.draw_visibility(
                        gpu,
                        encoder,
                        targets,
                        placed,
                        &draws,
                        &opaque_frame,
                        &args,
                        scissor,
                    );
                }
                resolving = Some((draws, scissor));
            }
            // Over kept depth (`Reuse::Geometry`) the same surfaces shade as
            // over a fresh one: every opaque pipeline tests GreaterEqual or
            // Equal, so each pixel's nearest surface, the last of equals,
            // writes, and the depth prepasses are skipped.
            //
            // In the game's order (Anno1800.exe build 16367353 sub_141C389B0 registers its
            // frame: the models' depth in the Z pre-pass, then Terrain,
            // Decals, Streets, Decals, Models): the placements' depth (the
            // visibility buffer's, above), the ground, the decals on it, then
            // the models (its resolve, below). Decals write no depth and
            // models shade after them, so a model just over the ground shows
            // whole however far a decal moves toward the eye. The ground
            // also still skips what the placements' depth covers.
            if reuse != Reuse::Whole {
                let timestamp_writes = targets.timing.as_mut().and_then(|t| t.render("opaque"));
                let cleared = Some(wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                    store: wgpu::StoreOp::Store,
                });
                let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("editor viewport"),
                    // Alpha 0 is "nothing drew here": the post stage leaves
                    // the viewport's target as it was there.
                    color_attachments: &[&targets.hdr.1, &targets.ids.1, &targets.scene.1].map(
                        |view| {
                            Some(wgpu::RenderPassColorAttachment {
                                view,
                                resolve_target: None,
                                ops: cleared.unwrap(),
                                depth_slice: None,
                            })
                        },
                    ),
                    depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                        view: &targets.depth.1,
                        depth_ops: Some(wgpu::Operations {
                            // Reversed depth: 0 is the far plane. The
                            // visibility buffer drew the placements' already.
                            load: if resolving.is_some() && reuse == Reuse::Nothing {
                                wgpu::LoadOp::Load
                            } else {
                                wgpu::LoadOp::Clear(0.)
                            },
                            store: wgpu::StoreOp::Store,
                        }),
                        stencil_ops: None,
                    }),
                    timestamp_writes,
                    occlusion_query_set: None,
                    multiview_mask: None,
                });
                // Kept depth first, over the whole target as copied.
                if reuse == Reuse::Geometry
                    && let Some(group) = &kept_group
                {
                    if let Some(t) = targets.timing.as_mut() {
                        t.mark(&mut pass, Some("depth restore"));
                    }
                    pass.set_pipeline(&device.kept.1[0]);
                    pass.set_bind_group(0, group, &[]);
                    pass.draw(0..3, 0..1);
                }
                pass.set_scissor_rect(sx, sy, sw, sh);
                pass.set_bind_group(0, &opaque_frame, &[]);
                let arena_draws = (placed.buffers.as_ref())
                    .zip(targets.cull.as_ref())
                    .filter(|_| placing)
                    .map(|(buffers, cull)| {
                        let draws = device.arena_draws(gpu, buffers, &cull.visible, &cull.keep);
                        (draws, cull.args.clone())
                    });
                // The ground (the kit's terrain pass, listed first, or a
                // game's) and other opaque passes.
                let paged = (lights.pages[3] > 0. && reuse != Reuse::Geometry)
                    .then(|| targets.pages.as_ref().map(|p| p.tiles.clone()))
                    .flatten();
                let kit = KitRecord {
                    gpu,
                    device,
                    ids: &ids,
                    scene: &scene,
                    cascade: None,
                    shadow_part: None,
                    paged: paged.as_deref(),
                };
                for (i, p) in plugged(Stage::Opaque).enumerate() {
                    if let Some(t) = targets.timing.as_mut() {
                        t.mark(
                            &mut pass,
                            Some(if i == 0 { "ground" } else { "opaque passes" }),
                        );
                    }
                    pass.set_bind_group(0, &opaque_frame, &[]);
                    let frame = pass::Record {
                        kit: Some(kit),
                        ..record(gpu, &targets.scene.1)
                    };
                    p.record(&frame, &mut pass);
                }
                // Decals on the ground, in row order (cull.wgsl).
                if let Some(t) = targets.timing.as_mut() {
                    t.mark(&mut pass, Some("ground decals"));
                }
                pass.set_bind_group(0, &opaque_frame, &[]);
                let ground_decal = |m: &Material| matches!(m, Material::Decal(d) if d.ground);
                if (visible.iter()).any(|(p, _)| ground_decal(p.material())) {
                    pass.set_pipeline(&device.ground_decal);
                    for (_, draw) in visible.iter().filter(|(p, _)| ground_decal(p.material())) {
                        pass.set_bind_group(1, &draw.mesh.group, &[]);
                        let offset = push([0, 0, 0, 0]);
                        pass.set_bind_group(2, &batches, &[offset]);
                        draw.mesh
                            .draw(&mut pass, draw.first..draw.first + draw.count);
                    }
                }
                if let (Some(pipelines), Some(arena), Some((draws, args))) =
                    (&device.arena, &placed.arena, &arena_draws)
                {
                    device.draw_arena(
                        ArenaPass::Decals,
                        &mut pass,
                        &pipelines[8],
                        arena,
                        draws,
                        args,
                        std::slice::from_ref(&arena.ranges.ground_decals),
                    );
                }
                // Then the models: the kit's meshes, and placements.
                if let Some(t) = targets.timing.as_mut() {
                    t.mark(&mut pass, Some("meshes"));
                }
                let opaque = |m: &Material| match m {
                    Material::Lit => Some(0),
                    Material::Flat => Some(1),
                    Material::Blend(_) | Material::Decal(_) => None,
                };
                for (index, pipeline) in [&device.lit, &device.flat].into_iter().enumerate() {
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
                }
                if let Some((draws, args)) = &arena_draws {
                    let timing = targets.timing.as_mut();
                    let prepass = reuse == Reuse::Nothing;
                    device
                        .draw_arena_opaque(&mut pass, placed, draws, args, prepass, false, timing);
                }
                if let Some(t) = targets.timing.as_mut() {
                    t.mark(&mut pass, None);
                }
            }
            // Second phase: what the first phase's depth shows of the rest,
            // in one pass (`draw_revealed`); kept geometry kept it.
            if let Some((_, scissor)) = &resolving
                && reuse == Reuse::Nothing
            {
                device.build_hzb(encoder, targets);
                device.cull(
                    gpu,
                    encoder,
                    placed,
                    targets,
                    CullPhase::Revealed,
                    &rows,
                    globals.clip,
                    [height, screen],
                    frustum.planes(),
                    Some((0, factor)),
                    view_eye,
                );
                if let (Some(buffers), Some(cull)) = (&placed.buffers, &targets.cull) {
                    let draws =
                        device.arena_draws(gpu, buffers, &cull.revealed_visible, &cull.keep);
                    let args = cull.revealed_args.clone();
                    device.draw_revealed(
                        gpu,
                        encoder,
                        targets,
                        placed,
                        &draws,
                        &opaque_frame,
                        &args,
                        *scissor,
                    );
                }
            }
            if let Some((draws, scissor)) = resolving {
                device.resolve(
                    gpu,
                    encoder,
                    targets,
                    placed,
                    &draws,
                    &opaque_frame,
                    scissor,
                );
            }
            // A mirror ends with its opaque surfaces: the view's own frame
            // fogs, waters, skies and shows them. What it drew is kept under
            // its keys: its cull lists, and the frame itself. Its posed
            // placements with them: a mirror at rest shows them still.
            if hidden {
                if posed {
                    device.draw_posed(
                        gpu,
                        encoder,
                        placed,
                        targets,
                        &opaque_frame,
                        [sx, sy, sw, sh],
                    );
                }
                gpu.queue.write_buffer(&ids, 0, &slots_.bytes);
                let layers = &mut targets.layers;
                if reuse == Reuse::Nothing {
                    layers.geometry = geometry;
                }
                layers.shaded = key;
                return false;
            }
            if placing && let Some(cull) = &targets.cull {
                // Both phases' counts, summed on the GPU as they cull.
                let totals = cull.totals.clone();
                let bytes = (MAX_LEVELS as u64 + 1) * 4;
                let buffer = targets.readback(gpu, bytes, ReadKind::Counts).clone();
                encoder.copy_buffer_to_buffer(&totals, 0, &buffer, 0, bytes);
            }
            // What this frame's culls asked of the arena's textures, by
            // section, for residency (residency.rs).
            if let (Some(cull), Some(arena)) = (targets.cull.as_mut(), &placed.arena)
                && cull.written != 0
                && cull.wants_arena == arena.id
            {
                let sections = std::mem::take(&mut cull.written);
                let wants = cull.wants.clone();
                let slots = arena.textures.len() as u64;
                let kind = ReadKind::Wants {
                    arena: arena.id,
                    slots: slots as usize,
                    sections,
                    view,
                };
                let bytes = u64::from(sections.count_ones()) * slots * 4;
                let buffer = targets.readback(gpu, bytes, kind).clone();
                let written = (0..residency::SECTIONS).filter(|s| sections & 1 << s != 0);
                for (k, section) in written.enumerate() {
                    let from = u64::from(section) * u64::from(residency::SLOTS) * 4;
                    encoder.copy_buffer_to_buffer(
                        &wants,
                        from,
                        &buffer,
                        k as u64 * slots * 4,
                        slots * 4,
                    );
                }
            }
            let lines = device.lines.as_ref().map_or(0, |l| l.1);
            // A plugged backdrop draws instead of the kit's.
            let sky = scene.sky.is_some() && !plugged(Stage::Sky).any(|p| p.replaces());
            // The blend and occlusion passes sample a copy of the opaque
            // depth (the blend pass also tests against the original; one
            // texture cannot be both). Kept layers' copy already holds it.
            if copies && reuse == Reuse::Nothing {
                let (depth, copy) = (&targets.depth.0, &targets.depth_copy.0);
                Timing::around(
                    targets.timing.as_mut(),
                    &gpu.device,
                    encoder,
                    "depth copy",
                    |encoder| {
                        encoder.copy_texture_to_texture(whole(depth), whole(copy), depth.size())
                    },
                );
            }
            // Passes' own work over the opaque depth, at sizes of their own,
            // before the stages that composite it; with posed placements,
            // once those have drawn (below).
            let after_opaque = |encoder: &mut wgpu::CommandEncoder,
                                targets: &mut Targets,
                                depth: &wgpu::TextureView| {
                let frame =
                    device.frame_group(gpu, &globals, &lights, shadow, Some(depth), None, None);
                for p in &ready {
                    p.after_opaque(&mut pass::AfterOpaque {
                        device: &gpu.device,
                        queue: &gpu.queue,
                        encoder: &mut *encoder,
                        frame: &frame,
                        size,
                        viewport: [sx, sy, sw, sh],
                        timing: targets.timing.as_mut(),
                    });
                }
            };
            if copies && !posed {
                let depth = targets.depth_copy.1.clone();
                after_opaque(encoder, targets, &depth);
            }
            // The half-size depth and the estimate read depth alone, which
            // kept layers keep; the blur weighs the estimate by the sun's
            // share of the light, shadows and clouds, so it and the apply
            // shade again with the opaque scene. The kit's apply is the
            // water and blend pass's first draw (`late_apply`, its group 0),
            // which a tile GPU spares a load and store of the HDR target, but
            // in a pass of its own here when the layers are kept after it:
            // the kept copy holds the occlusion, which `Reuse::Whole` frames
            // then leave out.
            let mut late_apply = None;
            if scene.occlusion.is_some() && reuse != Reuse::Whole {
                let frame = device.frame_group(
                    gpu,
                    &globals,
                    &lights,
                    shadow,
                    Some(&targets.depth_copy.1),
                    None,
                    None,
                );
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
                // The passes' timestamp slots up front: the writes borrow the
                // query set for as long as the passes live.
                let estimated = reuse == Reuse::Nothing;
                let slots = (targets.timing.as_mut()).map_or((None, None, None, None), |t| {
                    let (depth, estimate) = if estimated {
                        (t.pass("occlusion depth"), t.pass("occlusion"))
                    } else {
                        (None, None)
                    };
                    (
                        depth,
                        estimate,
                        t.pass("occlusion blur"),
                        (!occlusion_later && keep)
                            .then(|| t.pass("occlusion apply"))
                            .flatten(),
                    )
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
                let half = {
                    let [w, h] = size.map(|s| s.div_ceil(2));
                    let (x, y) = ((sx / 2).min(w), (sy / 2).min(h));
                    [x, y, sw.div_ceil(2).min(w - x), sh.div_ceil(2).min(h - y)]
                };
                let pass = |encoder: &mut wgpu::CommandEncoder,
                            label,
                            view,
                            load,
                            timestamp_writes,
                            [x, y, w, h]: [u32; 4]| {
                    let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                        label: Some(label),
                        color_attachments: &[color(view, load)],
                        depth_stencil_attachment: None,
                        timestamp_writes,
                        occlusion_query_set: None,
                        multiview_mask: None,
                    });
                    pass.set_scissor_rect(x, y, w, h);
                    pass.set_bind_group(0, &frame, &[]);
                    // Each pass ends before the encoder records the next.
                    pass.forget_lifetime()
                };
                let group = |view| {
                    gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
                        label: Some("editor occlusion"),
                        layout: &device.kit.occlusion_layout,
                        entries: &[
                            entry(0, wgpu::BindingResource::TextureView(view)),
                            entry(
                                1,
                                wgpu::BindingResource::TextureView(&device.kit.dummy.black),
                            ),
                        ],
                    })
                };
                // Depth alone: kept geometry keeps them.
                if estimated {
                    {
                        let mut halve = pass(
                            encoder,
                            "editor occlusion depth",
                            &targets.occlusion_depth.1,
                            wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                            writes(slots.0),
                            half,
                        );
                        halve.set_pipeline(&device.occlusion[0]);
                        halve.draw(0..3, 0..1);
                    }
                    // Below 0: no surface (fs_occlusion).
                    let white = wgpu::LoadOp::Clear(wgpu::Color {
                        r: -1.,
                        g: 0.,
                        b: 0.,
                        a: 0.,
                    });
                    let depth = group(&targets.occlusion_depth.1);
                    let mut estimate = pass(
                        encoder,
                        "editor occlusion",
                        &targets.occlusion.1,
                        white,
                        writes(slots.1),
                        half,
                    );
                    estimate.set_pipeline(&device.occlusion[1]);
                    estimate.set_bind_group(1, &depth, &[]);
                    estimate.draw(0..3, 0..1);
                }
                let (raw, blurred) = (
                    group(&targets.occlusion.1),
                    group(&targets.occlusion_blur.1),
                );
                {
                    // Unblurred texels (outside the scissor) read as nothing
                    // drawn: depth 0.
                    let open = wgpu::LoadOp::Clear(wgpu::Color {
                        r: 1.,
                        g: 1.,
                        b: 1.,
                        a: 0.,
                    });
                    let mut blur = pass(
                        encoder,
                        "editor occlusion blur",
                        &targets.occlusion_blur.1,
                        open,
                        writes(slots.2),
                        half,
                    );
                    blur.set_pipeline(&device.occlusion[2]);
                    blur.set_bind_group(1, &raw, &[]);
                    blur.draw(0..3, 0..1);
                }
                // Else the Blend stage's pass multiplies it in.
                if !occlusion_later && !keep {
                    late_apply = Some(frame.clone());
                } else if !occlusion_later {
                    let mut apply = pass(
                        encoder,
                        "editor occlusion apply",
                        &targets.hdr.1,
                        wgpu::LoadOp::Load,
                        writes(slots.3),
                        [sx, sy, sw, sh],
                    );
                    apply.set_pipeline(&device.occlusion[3]);
                    apply.set_bind_group(1, &blurred, &[]);
                    apply.draw(0..3, 0..1);
                }
            }
            // The second frame in a row under one key: keep the HDR target
            // as the opaque and occlusion stages leave it.
            if keep {
                let size = targets.size;
                let layers = &mut targets.layers;
                let kept = &layers
                    .hdr
                    .get_or_insert_with(|| {
                        use wgpu::TextureUsages as U;
                        let usage = U::RENDER_ATTACHMENT | U::TEXTURE_BINDING;
                        texture_2d(gpu, "editor kept hdr", size, HDR_FORMAT, usage)
                    })
                    .1;
                layers.shaded = key.clone();
                let group = gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some("editor keep layers"),
                    layout: &device.kept.0,
                    entries: &[
                        entry(0, wgpu::BindingResource::TextureView(&targets.depth_copy.1)),
                        entry(1, wgpu::BindingResource::TextureView(&targets.hdr.1)),
                    ],
                });
                let timestamp_writes = targets
                    .timing
                    .as_mut()
                    .and_then(|t| t.render("keep layers"));
                let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("editor keep layers"),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: kept,
                        resolve_target: None,
                        ops: wgpu::Operations {
                            load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                            store: wgpu::StoreOp::Store,
                        },
                        depth_slice: None,
                    })],
                    depth_stencil_attachment: None,
                    timestamp_writes,
                    occlusion_query_set: None,
                    multiview_mask: None,
                });
                pass.set_pipeline(&device.kept.1[2]);
                pass.set_bind_group(0, &group, &[]);
                pass.draw(0..3, 0..1);
            }
            if keep && posed {
                let size = targets.size;
                let kept = targets.layers.ids.get_or_insert_with(|| {
                    use wgpu::TextureUsages as U;
                    let format = wgpu::TextureFormat::R32Uint;
                    texture_2d(
                        gpu,
                        "editor kept pick ids",
                        size,
                        format,
                        U::COPY_DST | U::COPY_SRC,
                    )
                });
                let ids = &targets.ids.0;
                encoder.copy_texture_to_texture(whole(ids), whole(&kept.0), ids.size());
            }
            {
                let layers = &mut targets.layers;
                if reuse == Reuse::Nothing {
                    layers.geometry = geometry;
                }
                // Shading another key leaves the kept copy stale.
                if reuse != Reuse::Whole && layers.shaded != key {
                    layers.shaded = None;
                }
                layers.last = key;
            }
            // Posed placements over the layers kept or drawn, every frame:
            // the occlusion stage and the kept copies hold the rest alone,
            // so a view at rest redraws only these. Posed surfaces take no
            // occlusion of their own. What composites the opaque scene
            // after (water, haze, blended surfaces) reads a copy of the
            // depth with them in.
            let scene_depth = if posed {
                device.draw_posed(
                    gpu,
                    encoder,
                    placed,
                    targets,
                    &opaque_frame,
                    [sx, sy, sw, sh],
                );
                if copies {
                    let size = targets.size;
                    let posed_depth = targets.posed_depth.get_or_insert_with(|| {
                        use wgpu::TextureUsages as U;
                        let usage = U::COPY_DST | U::TEXTURE_BINDING;
                        let format = wgpu::TextureFormat::Depth32Float;
                        texture_2d(gpu, "editor posed depth copy", size, format, usage)
                    });
                    let (depth, copy) = (&targets.depth.0, &posed_depth.0);
                    let view = posed_depth.1.clone();
                    Timing::around(
                        targets.timing.as_mut(),
                        &gpu.device,
                        encoder,
                        "posed depth copy",
                        |encoder| {
                            encoder.copy_texture_to_texture(whole(depth), whole(copy), depth.size())
                        },
                    );
                    after_opaque(encoder, targets, &view);
                    view
                } else {
                    targets.depth_copy.1.clone()
                }
            } else {
                targets.depth_copy.1.clone()
            };
            // The pick handle under the asked-for cursor comes back a frame
            // or two later. After posed placements, before water.
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
                    // The depth beside the handle: kept frames restored
                    // the opaque depth before the opaque stage.
                    let cursor = buffer_init(
                        gpu,
                        "editor pick cursor",
                        bytemuck::cast_slice(&[x as u32, y as u32]),
                        wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC,
                    );
                    let (layout, pipeline) = &device.kit.pick_depth;
                    let depth = if restore_late {
                        &targets.depth_copy.1
                    } else {
                        &targets.depth.1
                    };
                    let group = gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
                        label: Some("editor pick depth"),
                        layout,
                        entries: &[
                            entry(0, wgpu::BindingResource::TextureView(depth)),
                            entry(1, cursor.as_entire_binding()),
                        ],
                    });
                    {
                        let mut pass = encoder.begin_compute_pass(&Default::default());
                        pass.set_pipeline(pipeline);
                        pass.set_bind_group(0, &group, &[]);
                        pass.dispatch_workgroups(1, 1, 1);
                    }
                    encoder.copy_buffer_to_buffer(&cursor, 4, &buffer, 4, 4);
                }
            }
            // Water, then the Blend stage's sky, decals and blended surfaces, in
            // one render pass over the HDR target and depth: water under the sky
            // and blended surfaces, which test against the depth it writes, over
            // the occlusion, which the sea's own shading replaces. Both read depth
            // through `scene_depth`'s copy, never the attachment, so nothing needs
            // the pass boundary between them; a tile GPU (Apple's) stores and
            // reloads both targets at every boundary, and low-end desktop parts pay
            // the flush.
            let decals = placing && placed.ranges.decals.end > placed.ranges.ground_decals.end;
            let sky_passes = plugged(Stage::Sky).next().is_some();
            let decals_drawn = decals || decal_batches().next().is_some();
            let blending = sky || !blended.is_empty() || decals_drawn || sky_passes || blend_passes;
            if water || blending || late_apply.is_some() || restore_late {
                // The occlusion the Blend stage will apply, which water divides out
                // (frame.wgsl occlusion_factor).
                let occlusion = occlusion_later.then_some(&targets.occlusion_blur.1);
                let water_frame = water.then(|| {
                    device.frame_group(
                        gpu,
                        &globals,
                        &lights,
                        shadow,
                        Some(&scene_depth),
                        None,
                        occlusion,
                    )
                });
                let blend_frame = blending.then(|| {
                    let depth = (!blended.is_empty() || blend_passes).then_some(&scene_depth);
                    device.frame_group(gpu, &globals, &lights, shadow, depth, None, occlusion)
                });
                let label = match (water, blending) {
                    (true, true) => "water + blend",
                    (true, false) => "water",
                    (false, true) => "blend",
                    _ if restore_late => "kept layers",
                    _ => "occlusion apply",
                };
                let late_apply = late_apply.map(|frame| {
                    let posed_depth = (targets.posed_vis.as_ref())
                        .filter(|_| posed)
                        .map_or(&device.kit.dummy.black, |[_, depth]| &depth.1);
                    let group = gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
                        label: Some("editor occlusion"),
                        layout: &device.kit.occlusion_layout,
                        entries: &[
                            entry(
                                0,
                                wgpu::BindingResource::TextureView(&targets.occlusion_blur.1),
                            ),
                            entry(1, wgpu::BindingResource::TextureView(posed_depth)),
                        ],
                    });
                    (frame, group)
                });
                let timestamp_writes = targets.timing.as_mut().and_then(|t| t.render(label));
                let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("editor water and blend"),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: &targets.hdr.1,
                        resolve_target: None,
                        ops: wgpu::Operations {
                            load: if restore_late {
                                wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT)
                            } else {
                                wgpu::LoadOp::Load
                            },
                            store: wgpu::StoreOp::Store,
                        },
                        depth_slice: None,
                    })],
                    depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                        view: &targets.depth.1,
                        depth_ops: Some(wgpu::Operations {
                            load: if restore_late {
                                wgpu::LoadOp::Clear(0.)
                            } else {
                                wgpu::LoadOp::Load
                            },
                            // Nothing after this pass reads it: post and overlays
                            // take no depth, the pick and the copies the next
                            // frame keeps (`depth_copy`, `posed_depth`) were made
                            // before it, and every frame clears or restores it.
                            store: wgpu::StoreOp::Discard,
                        }),
                        stencil_ops: None,
                    }),
                    timestamp_writes,
                    occlusion_query_set: None,
                    multiview_mask: None,
                });
                // Over the whole target, as the kept layers pass drew them.
                if restore_late && let Some(group) = &kept_group {
                    Timing::split(targets.timing.as_mut(), &mut pass, Some("kept layers"));
                    pass.set_pipeline(&device.kept.1[1]);
                    pass.set_bind_group(0, group, &[]);
                    pass.draw(0..3, 0..1);
                    Timing::split(targets.timing.as_mut(), &mut pass, None);
                }
                pass.set_scissor_rect(sx, sy, sw, sh);
                if let Some((frame, group)) = &late_apply {
                    Timing::split(targets.timing.as_mut(), &mut pass, Some("occlusion apply"));
                    pass.set_pipeline(&device.occlusion[4]);
                    pass.set_bind_group(0, frame, &[]);
                    pass.set_bind_group(1, group, &[]);
                    pass.draw(0..3, 0..1);
                    Timing::split(targets.timing.as_mut(), &mut pass, None);
                }
                if let Some(frame) = &water_frame {
                    // Where nothing opaque drew, before the game's water, which
                    // draws over it where it reaches.
                    if sea {
                        Timing::split(targets.timing.as_mut(), &mut pass, Some("kit sea"));
                        pass.set_bind_group(0, frame, &[]);
                        pass.set_pipeline(&device.sea);
                        pass.draw(0..3, 0..1);
                    }
                    for (i, p) in plugged(Stage::Water).enumerate() {
                        let label = ["water 0", "water 1", "water 2"].get(i).copied();
                        Timing::split(targets.timing.as_mut(), &mut pass, label);
                        pass.set_bind_group(0, frame, &[]);
                        let frame = pass::Record {
                            occlusion: occlusion_later,
                            ..record(gpu, &targets.scene.1)
                        };
                        p.record(&frame, &mut pass);
                    }
                    Timing::split(targets.timing.as_mut(), &mut pass, None);
                }
                if let Some(blend_frame) = &blend_frame {
                    pass.set_bind_group(0, blend_frame, &[]);
                    if sky {
                        Timing::split(targets.timing.as_mut(), &mut pass, Some("kit sky"));
                        pass.set_pipeline(&device.sky);
                        pass.draw(0..3, 0..1);
                    }
                    for (i, p) in plugged(Stage::Sky).enumerate() {
                        let label = ["sky 0", "sky 1", "sky 2"].get(i).copied();
                        Timing::split(targets.timing.as_mut(), &mut pass, label);
                        pass.set_bind_group(0, blend_frame, &[]);
                        p.record(&record(gpu, &targets.scene.1), &mut pass);
                    }
                    // Decals on models, over everything opaque, under water and
                    // glass, from the first cull phase's list, which holds them
                    // all in row order (cull.wgsl).
                    if decals_drawn {
                        Timing::split(targets.timing.as_mut(), &mut pass, Some("decals"));
                    }
                    if decals && let Some(cull) = &targets.cull {
                        pass.set_bind_group(0, blend_frame, &[]);
                        let arena = device.arena.as_ref().zip(placed.arena.as_ref());
                        if let (Some((pipelines, arena)), Some(buffers)) = (arena, &placed.buffers)
                        {
                            let draws = device.arena_draws(gpu, buffers, &cull.visible, &cull.keep);
                            let r = &arena.ranges;
                            device.draw_arena(
                                ArenaPass::Decals,
                                &mut pass,
                                &pipelines[4],
                                arena,
                                &draws,
                                &cull.args,
                                &[r.ground_decals.end..r.decals.end],
                            );
                        }
                    }
                    if decal_batches().next().is_some() {
                        pass.set_bind_group(0, blend_frame, &[]);
                        pass.set_pipeline(&device.decal);
                        for draw in decal_batches() {
                            pass.set_bind_group(1, &draw.mesh.group, &[]);
                            let offset = push([0; 4]);
                            pass.set_bind_group(2, &batches, &[offset]);
                            draw.mesh
                                .draw(&mut pass, draw.first..draw.first + draw.count);
                        }
                    }
                    if !blended.is_empty() {
                        Timing::split(targets.timing.as_mut(), &mut pass, Some("blended"));
                        let mut adding = None;
                        for &(_, draw, additive) in &blended {
                            if adding != Some(additive) {
                                pass.set_pipeline(if additive {
                                    &device.add
                                } else {
                                    &device.blend
                                });
                                adding = Some(additive);
                            }
                            pass.set_bind_group(1, &draw.mesh.group, &[]);
                            let offset = push([0; 4]);
                            pass.set_bind_group(2, &batches, &[offset]);
                            draw.mesh
                                .draw(&mut pass, draw.first..draw.first + draw.count);
                        }
                    }
                    for (i, p) in plugged(Stage::Blend).enumerate() {
                        let label = ["blend 0", "blend 1", "blend 2", "blend 3"].get(i).copied();
                        Timing::split(targets.timing.as_mut(), &mut pass, label);
                        pass.set_bind_group(0, blend_frame, &[]);
                        let frame = pass::Record {
                            occlusion: occlusion_later,
                            ..record(gpu, &targets.scene.1)
                        };
                        p.record(&frame, &mut pass);
                    }
                    Timing::split(targets.timing.as_mut(), &mut pass, None);
                }
            }
            // Overlays in display values: the selection outline, then lines
            // (the selection box among them) and markers over it, in the post
            // pass: a tile GPU would store the view and load it back for a
            // pass of their own. The outline's tiles read pick handles alone,
            // so they are made before it.
            let outline = (outline.as_ref()).filter(|o| !o.rows.is_empty() || !o.ids.is_empty());
            let overlay_passes = plugged(Stage::Overlay).next().is_some();
            let overlaying = outline.is_some() || markers > 0 || lines > 0 || overlay_passes;
            let outline_group = outline.map(|outline| {
                // Batch ids name pick handles, which a new scene
                // renumbers; rows alone stay until the outline changes.
                let display = [
                    pane_globals.display[0],
                    pane_globals.display[1],
                    render_scale,
                ];
                let key = (outline_version, display.map(f32::to_bits));
                if !outline.ids.is_empty()
                    || device.outline_marks.as_ref().map(|m| m.0) != Some(key)
                {
                    let marks = outline_marks(outline, &device.pick_ids, display);
                    let buffer = gpu
                        .device
                        .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                            label: Some("editor outline marks"),
                            contents: bytemuck::cast_slice(&marks),
                            usage: wgpu::BufferUsages::STORAGE,
                        });
                    device.outline_marks = Some((key, buffer));
                }
                let buffer = &device.outline_marks.as_ref().expect("outline marks").1;
                let marks = gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some("editor outline"),
                    layout: &device.outline.0,
                    entries: &[
                        entry(0, wgpu::BindingResource::TextureView(&targets.ids.1)),
                        entry(1, buffer.as_entire_binding()),
                    ],
                });
                // The band tests up to 15x15 taps a pixel over the whole
                // pane, which cost ~5 ms at 1300x1000 with anything
                // selected. The tiles let it skip pixels with no marked
                // pixel in reach for a few loads, at about one load a pixel.
                let ids = targets.ids.0.size();
                let tiles = targets.outline_tiles.get_or_insert_with(|| {
                    texture_2d(
                        gpu,
                        "editor outline tiles",
                        [ids.width, ids.height].map(|s| s.div_ceil(OUTLINE_TILE)),
                        OUTLINE_TILES,
                        wgpu::TextureUsages::RENDER_ATTACHMENT
                            | wgpu::TextureUsages::TEXTURE_BINDING,
                    )
                });
                let timestamp_writes = targets
                    .timing
                    .as_mut()
                    .and_then(|t| t.render("outline tiles"));
                let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("editor outline tiles"),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: &tiles.1,
                        resolve_target: None,
                        ops: wgpu::Operations {
                            load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                            store: wgpu::StoreOp::Store,
                        },
                        depth_slice: None,
                    })],
                    depth_stencil_attachment: None,
                    timestamp_writes,
                    occlusion_query_set: None,
                    multiview_mask: None,
                });
                pass.set_pipeline(&device.outline.2[0]);
                pass.set_bind_group(0, &marks, &[]);
                pass.draw(0..3, 0..1);
                drop(pass);
                let tiles = gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some("editor outline tiles"),
                    layout: &device.outline.1,
                    entries: &[entry(0, wgpu::BindingResource::TextureView(&tiles.1))],
                });
                (marks, tiles)
            });
            // The one tone map: HDR to the viewport's target, over what the
            // target held where nothing drew; measured first when it exposes
            // itself.
            {
                let auto = scene.tone.and_then(|t| t.auto);
                let timestamp_writes =
                    auto.and_then(|_| targets.timing.as_mut()?.compute("exposure"));
                targets.exposure.record(
                    gpu,
                    &device.exposure,
                    encoder,
                    &targets.hdr.1,
                    [sx, sy, sw, sh],
                    auto.as_ref(),
                    timestamp_writes,
                );
                let frame =
                    device.frame_group(gpu, &pane_globals, &lights, shadow, None, None, None);
                let group = gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some("editor post"),
                    layout: &device.kit.post_layout,
                    entries: &[
                        entry(0, wgpu::BindingResource::TextureView(&targets.hdr.1)),
                        entry(1, targets.exposure.value.as_entire_binding()),
                    ],
                });
                let timestamp_writes = targets.timing.as_mut().and_then(|t| t.render("post"));
                let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("editor post"),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: target,
                        resolve_target: None,
                        ops: wgpu::Operations {
                            // It blends by coverage onto clear, which its own
                            // clear gives without a pass of its own: a tile GPU
                            // would store a cleared target and load it back here.
                            // The clear spans the whole target, past the scissor.
                            load: if clear {
                                wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT)
                            } else {
                                wgpu::LoadOp::Load
                            },
                            store: wgpu::StoreOp::Store,
                        },
                        depth_slice: None,
                    })],
                    depth_stencil_attachment: None,
                    timestamp_writes,
                    occlusion_query_set: None,
                    multiview_mask: None,
                });
                pass.set_scissor_rect(ux, uy, uw, uh);
                pass.set_pipeline(&device.post);
                pass.set_bind_group(0, &frame, &[]);
                pass.set_bind_group(1, &group, &[]);
                pass.draw(0..3, 0..1);
                if overlaying {
                    Timing::split(targets.timing.as_mut(), &mut pass, Some("overlay"));
                    if let Some((marks, tiles)) = &outline_group {
                        pass.set_pipeline(&device.outline.2[1]);
                        pass.set_bind_group(0, marks, &[]);
                        pass.set_bind_group(1, tiles, &[]);
                        pass.draw(0..3, 0..1);
                    }
                    pass.set_bind_group(0, &frame, &[]);
                    if lines > 0 {
                        pass.set_pipeline(&device.line);
                        pass.draw(0..6, 0..lines);
                    }
                    if markers > 0 {
                        pass.set_pipeline(&device.marker);
                        pass.draw(0..6, 0..markers);
                    }
                    for p in plugged(Stage::Overlay) {
                        pass.set_bind_group(0, &frame, &[]);
                        p.record(&record(gpu, &targets.scene.1), &mut pass);
                    }
                    Timing::split(targets.timing.as_mut(), &mut pass, None);
                }
            }
            drop(push);
            // Lands before this encoder runs: queue writes precede the
            // next submission's command buffers.
            gpu.queue.write_buffer(&ids, 0, &slots_.bytes);
            true
        })();
        if let (Some(device), Some(s)) = (device.as_ref(), slots.get_mut(slot)) {
            s.drawn.arena_draws = device.arena_drawn.take();
        }
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
            // The first viewport draw is after the swapchain hands over a
            // drawable. That wait is FIFO pacing, not layout.
            let surface_wait = state.surface_wait;
            let st = &mut state.stats;
            if built_now {
                let build = ms(built.saturating_duration_since(started));
                let layout =
                    (ms(encode.saturating_duration_since(built)) - ms(surface_wait)).max(0.);
                st.build_ms = Stats::blend(st.build_ms, build);
                st.layout_ms = Stats::blend(st.layout_ms, layout);
            }
            let encoding = ms(now.saturating_duration_since(encode));
            st.encode_ms = Stats::blend(st.encode_ms, encoding);
            st.cpu_ms = st.build_ms + st.layout_ms + st.encode_ms;
        }
        drew
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
