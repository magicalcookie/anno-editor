// GPU-driven drawing of placed model pieces. The host hands over a table of
// placements (piece index, transform) once per structural change and moves
// rows in place; nothing here is rebuilt per frame on the CPU. Each frame a
// compute pass (cull.wgsl) culls every placement and picks its detail
// level, filling per-(piece, level) visible lists and indirect arguments
// that the opaque and shadow passes draw from. Where the device allows it,
// every resident piece level's geometry, material and texture also sit in
// one shared arena (placed.wgsl), so each pipeline draws all placements with
// one multi-draw; otherwise each (piece, level) is its own indirect draw.
// Also owns the surface families lit placements shade with (`Families`):
// the registry, its checks and the dispatchers it generates.
use super::{
    Assets, Budget, Gpu, Kit, Material, ResidentMesh, StreamedTexture, buffer_init, entry, key,
};
use crate::math::{self, Mat4, Vec3};
use anyhow::{Result, anyhow, ensure};
use bytemuck::{Pod, Zeroable};
use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::sync::Arc;
use wgpu::naga::{self, valid::Validator};

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
    /// How the piece moves by itself. Lit pieces without thinning or an
    /// impostor only. Posed placements draw after the opaque layers a view
    /// at rest keeps, every frame, over them.
    pub pose: Option<Arc<Pose>>,
}

/// A placed piece's own motion: a looped clip of joint matrices the vertex
/// stage blends by [`clock`](super::clock), for a skinned mesh's joints
/// (`MeshData::skin`) or for one joint the whole mesh follows (a part that
/// turns or swings, or hangs on a joint that moves). Each frame holds
/// `joints` 3x4 matrices, rows first, from the mesh's own space to where a
/// vertex bound to the joint lies in it at that moment. Frames are evenly
/// spaced over `period`, the last blending back into the first.
#[derive(Clone, Debug)]
pub struct Pose {
    /// `joints` matrices a frame, frame after frame.
    pub frames: Vec<[[f32; 4]; 3]>,
    pub joints: u32,
    /// Seconds one loop takes.
    pub period: f32,
    /// Each placement starts at its own point of the loop, picked from
    /// where `origin` lands in the world, so neighbours do not move in
    /// step; else every placement plays from the clock's zero.
    pub phased: bool,
    /// Where the placed object's origin lies in the mesh's space: the
    /// pieces of one object share it, so they keep in step.
    pub origin: Vec3,
    /// A box in the mesh's space every frame keeps the mesh in, which the
    /// cull and the shadow fit take in place of the mesh's own.
    pub bounds: [Vec3; 2],
}

pub const MAX_LEVELS: usize = 4;

/// Impostors bake an 8x8 grid of views over the upper hemisphere: one layer
/// per distinct mesh, with mips, color and coverage (BC3), the normal (BC5)
/// and depth with gloss (BC5) in three bytes a texel; eight where the
/// device lacks BC.
pub const IMPOSTOR_VIEWS: u32 = 8;
/// Texels every impostor layer may take together, mips included: tiles are
/// as large as this allows, up to `IMPOSTOR_TILE` px. The Albion view's
/// island bakes 60 layers at 64 px (63 MB); a 768 m island's 659 layers get
/// 24 px (97 MB; as RGBA8 pairs they took 259 MB, and with 40 px tiles and
/// no mips 405 MB).
/// TODO: bake only the meshes a view draws as impostors, from the cull's
/// counts, so a large island's trees get Albion's tiles too.
const IMPOSTOR_TEXELS: u64 = 33 << 20;
const IMPOSTOR_TILE: u32 = 64;

/// Pixels per side of each impostor view for `layers` layers: the largest
/// multiple of 8 within `IMPOSTOR_TEXELS`, at least 16.
pub(super) fn impostor_tile(layers: u32) -> u32 {
    let views = u64::from(IMPOSTOR_VIEWS * IMPOSTOR_VIEWS);
    // A third more for mips.
    let per_texel = u64::from(layers.max(1)) * views * 4 / 3;
    let side = ((IMPOSTOR_TEXELS / per_texel) as f64).sqrt() as u32;
    (side / 8 * 8).clamp(16, IMPOSTOR_TILE)
}

/// Words per draw in the indirect arguments: indexed draws' five (index
/// count, instances, first index, base vertex, first instance). Base
/// vertex and first instance are always zero: wgpu on D3D12 drops both
/// from shaders' vertex and instance index unless its indirect-draw checks
/// run, so the arena's shaders find a draw's vertices and instances from
/// its indices instead (`Stream`). Draws without the arena read the first
/// four as a plain draw's, all but the counts zero.
pub(super) const ARGS: u32 = 5;

/// Index values per page, log 2: each page of the arena's index values
/// names a draw and the vertex its first value is (placed.wgsl `p_pages`).
const PAGE_BITS: u32 = 6;

/// A draw slot's list (`Placed::lists`, cull.wgsl `NO_LIST`) when no
/// arena draw takes its placements: a thinning bucket without triangles
/// of its own.
const NO_LIST: u32 = u32::MAX;

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
    /// Bit 0 has normals, bit 1 ignores texture alpha, bit 2 impostor,
    /// bits 3-4 terrain adaption (`adapt_code`), bit 5 decal, bit 8 the
    /// normal map's blue glows (`MeshData::emissive`), bit 11
    /// `SLOTS_FLAG`, bits 12-14 `MeshData::specular`, bits 16-23 a lit mesh's surface family (`FamilyId`),
    /// bits 24-31 where its albedo's alpha cuts (`alpha_cut`).
    flags: u32,
    gain: f32,
    /// Impostors (flag bit 2): the material of the full-detail level they
    /// bake. `albedo` is then their atlas layer. Lit meshes: where their
    /// `GpuSplat` starts in the shapes array, 0 without one.
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
    /// Decals: the alpha map's slot (0, white, without), coverage and bias
    /// toward the viewer (`Material::Decal`).
    alpha: u32,
    opacity: f32,
    bias: f32,
    /// `tiling_code` of `MeshData::world_tiling`.
    tiling: f32,
    /// A projected decal's edge (`DecalEdge::code`).
    edge: [f32; 4],
    edge2: [f32; 2],
    /// The mesh's vertex grids (`Quant`): origins, and the steps of
    /// positions, u and v.
    origin: [f32; 5],
    step: [f32; 3],
    /// The mesh's first index (`pack`), whose values name arena vertices
    /// through the pages (placed.wgsl `arena_at`).
    index: u32,
    /// Where its triangle-start table begins in the visibility buffer's
    /// firsts (`Span::table`).
    firsts: u32,
    /// Where the mesh's family payload starts in the shapes array, eight
    /// words a shape (placed.wgsl `family_word`); 0 without one. `lit`'s
    /// and a decal's is a `GpuEmissive` where flags bit 8 says it glows.
    payload: u32,
}

/// `lit`'s and decals' payload, one `Shape`: the colour their glow takes
/// (placed.wgsl `lit_glow`), for the meshes that glow alone.
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct GpuEmissive {
    /// `MeshData::emissive`, w unused.
    color: [f32; 4],
    _rest: [u32; 4],
}

/// A lit mesh's splat (`MeshSplat`) as placed.wgsl's `splatted` reads it:
/// two `Shape`s long, after every material's shape, where the mesh's
/// `GpuDraw::source` says.
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct GpuSplat {
    /// Each channel's layer's repeats over the mesh's UVs.
    tiling: [f32; 4],
    /// The control map's slot.
    control: u32,
    /// Bit 0 tints by the albedo, bit 1 mixes normals, bit 2 scales the
    /// layers' UVs with the placement, bit 3 takes them from the world;
    /// bits 8-11 the channels with a layer.
    flags: u32,
    /// Each channel's albedo slot, two to a word, the lower channel low.
    albedo: [u32; 2],
    /// Each channel's normal map slot, the same way; 0 none.
    normal: [u32; 2],
    /// `MeshSplat::sharpness`, `alpha_floor`, `fallback` and `tint` (with
    /// flags bit 0).
    sharpness: f32,
    alpha_floor: f32,
    fallback: [f32; 3],
    tint: f32,
}

/// Words a mesh's family payload holds at most (`MeshData::payload`).
// Frozen with MAX_SLOTS once a six-slot family with a
// payload drew from the arena and mesh by mesh (gpu tests
// `an_extra_slot_family_draws_every_slot_on_every_path`). 16 is two arena
// shapes and the kit mesh's 80-byte uniform (pieces.wgsl PieceFamily).
// Raising it grows that uniform in every mesh's bind group; change both
// together.
pub const MAX_PAYLOAD: usize = 16;

/// Textures a mesh brings its family at most (`MeshData::slots`): one shape
/// of the payload, two to a word.
// Frozen with MAX_PAYLOAD. The survey's largest families (2070's ripples
// and parallax) need 5-7. For the kit's meshes each slot is a fixed binding
// of every mesh's bind group (bindings 24-31, pieces.wgsl), and the arena's
// residency walks them all per draw (cull.wgsl `want_texels`), so a
// ninth costs every mesh, not only the family that uses it.
pub const MAX_SLOTS: usize = 8;

/// `GpuDraw::flags` bit set when the payload opens with a shape of the
/// mesh's family slots (placed.wgsl `family_slot`).
const SLOTS_FLAG: u32 = 1 << 11;

/// `GpuDraw::parts` of a mesh that does not thin (placed.wgsl).
const NO_PARTS: u32 = u32::MAX;

/// A surface family's place in its [`Families`]: what a `Lit` mesh names
/// (`MeshData::family`) and `GpuDraw::flags` carries in bits 16-23.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct FamilyId(pub(super) u8);

impl FamilyId {
    /// The kit's own family: texture times vertex colour, lit, cut out at
    /// half alpha, with the maps, emissive and splat a mesh brings.
    pub const LIT: Self = Self(0);
}

/// What a lit placement's pixels are made of, as WGSL of the host's own:
/// appended to every module that draws from the arena, it defines, for a
/// family named `ore`,
///
/// ```wgsl
/// fn ore_coverage(material: GpuDraw, c: Cover) -> f32
/// fn ore_surface(material: GpuDraw, p: LitPixel, t: LitTexels) -> Lit
/// fn ore_light(material: GpuDraw, p: LitPixel, s: Lit) -> vec3<f32>
/// fn ore_vertex(material: GpuDraw, v: FamilyVertex) -> vec3<f32>
/// ```
///
/// (placed.wgsl says what each is called for), `light` only with
/// [`Family::light`] and `vertex` only with [`Family::vertex`]. The source may call anything all of those modules
/// hold. Three rules, checked when the family joins a registry
/// ([`Families::new`]):
///
/// - Every name it declares starts with `<name>_`, so families cannot
///   clash with each other or with a later kit.
/// - None of its functions takes a screen derivative, samples a texture at
///   an implicit level or discards. The visibility buffer's resolve shades
///   a pixel away from its triangle, so a derivative there is not the
///   surface's:
///   `LitPixel` carries the ones a family may use, and a caller cuts by
///   the coverage a family returns.
/// - It reads the arena's textures through `family_texel`, never
///   `t_albedos` itself.
#[derive(Clone, Debug, PartialEq)]
pub struct Family {
    /// Lower-case letters and digits, a letter first.
    pub name: String,
    pub wgsl: String,
    /// The source defines `<name>_light`; otherwise the kit's `shade`
    /// lights the family's surfaces.
    pub light: bool,
    /// The source defines `<name>_vertex`, which places each vertex of the
    /// family's placements in the world from `FamilyVertex` (family.wgsl):
    /// its local position, the kit's placing of it, its part's centre
    /// (`Thinning::parts`) and the row's transform. For what one vertex
    /// can't show from the mesh alone: a square that turns to face the
    /// camera. Called in the visibility buffer and its resolve and in
    /// shadow maps. The kit's own meshes (without the arena) and impostors
    /// stand as their positions are, so a family with a vertex stage
    /// doesn't bake.
    pub vertex: bool,
    /// Which of a mesh's slots (`MeshData::slots`, bit k for slot k) its
    /// coverage samples: shadow maps ask residency for these as well as the
    /// albedo, where they ask for nothing else.
    pub coverage_slots: u8,
    /// Its surface reads only what the impostor bake has: the mesh's UVs,
    /// vertex colour and normal, the texels, its payload and slots; never
    /// the world position, the row (`pick`, `word`) or the view. Its pieces
    /// then get impostors, baked through its surface and drawn far off as
    /// the kit draws every impostor: lit by the scene's `shade`, tinted by
    /// the ground, without its emissive. Off, or with [`Family::light`],
    /// they keep their mesh levels.
    pub bake: bool,
}

/// How the scene turns a lit surface into light, as WGSL of the host's own,
/// for every lit thing a renderer draws: placements, the kit's own meshes,
/// impostors, decals, blended meshes, the kit's ground and sea. Each part left `None` is the kit's.
///
/// ```wgsl
/// fn shade(world: vec3<f32>, s: Surface, emissive: vec3<f32>, has_normals: bool) -> vec3<f32>
/// fn fog(color: vec3<f32>, world: vec3<f32>) -> vec3<f32>
/// fn ground_tint_decode(texel: vec4<f32>) -> vec3<f32>
/// ```
///
/// `shade` takes the surface (material AO in `s.occlusion`) and what it
/// gives off itself; the kit's, `kit_shade`, lights by the sun, its shadow
/// and the sky and adds `emissive`. `fog` runs once on what `shade` returns,
/// whose value before it is kept for the ocean (frame.wgsl `shaded`); the
/// kit's is `kit_fog`. `ground_tint_decode` turns a [`GroundTint`] texel,
/// filtered as stored, into the multiplier surfaces with a terrain tint
/// take: the ground that stores the map supplies it; the kit's reads the
/// texel's RGB as the multiplier. Any other name either declares starts
/// with `scene_`, and the family rules hold: no screen derivatives,
/// implicit-level samples or discards.
///
/// `modes` are the scene's other ways of being seen ([`ViewMode`]), which
/// [`super::Scene::view_mode`] picks between frame by frame with no new
/// pipeline: with none, `shade` and `fog` are composed as they are.
///
/// [`GroundTint`]: super::GroundTint
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Lighting {
    pub shade: Option<String>,
    pub fog: Option<String>,
    pub tint: Option<String>,
    pub modes: Vec<ViewMode>,
}

/// A view mode: a `shade` and fog of its own that a scene swaps in for the
/// plain ones while it looks through it (a view from inside a medium). Its
/// WGSL names start with `name` and `_`: `<name>_shade` and `<name>_fog`,
/// with the signatures of [`Lighting`]'s `shade` and `fog`. A part left
/// `None` is the plain one. Mode `k` of [`super::Scene::view_mode`] is
/// `modes[k - 1]`.
///
/// `unlit`, `<name>_unlit` with `fog`'s signature, is what the medium does
/// to an unlit colour ([`super::Material::Flat`]), which no `shade` or
/// `fog` sees: the plain view leaves those colours as they are, and so does
/// a mode without one.
///
/// A mode's `shade` gets the surface's emissive apart, so light that skips
/// the mode's air (glow under water) is added after it; its `fog` then sees
/// the sum. What a mode does after the frame (a grade, a blur) is a
/// Blend-stage pass the scene lists while it is on, and its own textures
/// sit in [`super::Scene::host_texture`].
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ViewMode {
    pub name: String,
    pub shade: Option<String>,
    pub fog: Option<String>,
    pub unlit: Option<String>,
}

/// The kit's `shade`, `fog` and tint decode: what `Lighting`'s `None`s mean.
const KIT_SHADE: &str = "fn shade(world: vec3<f32>, s: Surface, emissive: vec3<f32>, has_normals: bool) -> vec3<f32> { return kit_shade(world, s, has_normals) + emissive; }\n";
const KIT_FOG: &str = kit_fog!();
/// Unlit colours as they are, in the plain view (`ViewMode::unlit`).
const PLAIN_UNLIT: &str =
    "fn unlit(color: vec3<f32>, world: vec3<f32>) -> vec3<f32> { return color; }\n";
const KIT_TINT: &str =
    "fn ground_tint_decode(texel: vec4<f32>) -> vec3<f32> { return texel.rgb; }\n";

/// The surface families lit placements shade with: the kit's `lit`
/// ([`FamilyId::LIT`]), then a host's own in the order given, and the
/// scene's lighting. Built once, before the pipelines it joins.
#[derive(Debug, Default, PartialEq)]
pub struct Families {
    /// Ids 1 and up.
    hosts: Vec<Family>,
    lighting: Lighting,
}

/// The shader modules that draw through surface families: placements from
/// the arena, and the kit's own meshes. Each ends with the families'
/// sources and the dispatchers over them.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum Module {
    /// placed.wgsl: flat meshes, shadows, impostors, decals.
    Arena,
    /// With visbuf.wgsl, and visbuf_corners.wgsl where fragments read
    /// their corners: lit placements.
    Visibility { corners: bool },
    /// pieces.wgsl: the kit's own meshes, drawn mesh by mesh without the
    /// arena, and their shadows.
    Pieces,
}

/// Every module, for the checks.
const MODULES: [Module; 4] = [
    Module::Arena,
    Module::Visibility { corners: false },
    Module::Visibility { corners: true },
    Module::Pieces,
];

/// The dispatchers placed.wgsl calls, generated per registry: what each
/// family's function is called after its name, its parameters, the
/// arguments handed on, and its result.
const DISPATCHERS: [(&str, &str, &str, &str); 4] = [
    (
        "coverage",
        "material: GpuDraw, c: Cover",
        "material, c",
        "f32",
    ),
    (
        "surface",
        "material: GpuDraw, p: LitPixel, t: LitTexels",
        "material, p, t",
        "Lit",
    ),
    (
        "light",
        "material: GpuDraw, p: LitPixel, s: Lit",
        "material, p, s",
        "vec3<f32>",
    ),
    (
        "vertex",
        "material: GpuDraw, v: FamilyVertex",
        "material, v",
        "vec3<f32>",
    ),
];

/// A module's WGSL as it was put together: which host family each piece
/// came from, so an error at a place in it finds the family at fault.
#[derive(Default)]
struct Composed {
    text: String,
    /// Where each piece starts and whose it is; `None` is the kit's.
    pieces: Vec<(usize, Option<usize>)>,
}

impl Composed {
    fn push(&mut self, owner: Option<usize>, text: &str) {
        self.pieces.push((self.text.len(), owner));
        self.text.push_str(text);
    }

    /// The host family whose piece holds byte `at`.
    fn owner(&self, at: usize) -> Option<usize> {
        let piece = self.pieces.partition_point(|(start, _)| *start <= at);
        self.pieces[piece.checked_sub(1)?].1
    }
}

impl Families {
    /// The kit's family alone.
    pub fn kit() -> Arc<Self> {
        static KIT: std::sync::OnceLock<Arc<Families>> = std::sync::OnceLock::new();
        KIT.get_or_init(Arc::default).clone()
    }

    /// The kit's family and `hosts` after it, each checked against the
    /// rules on [`Family`]. An error names the family at fault, so one
    /// broken family fails here, alone, and not as every pipeline's shader.
    pub fn new(hosts: Vec<Family>) -> Result<Arc<Self>> {
        Self::with_lighting(hosts, Lighting::default())
    }

    /// `new`, lit by `lighting`, which is checked as the families are.
    pub fn with_lighting(hosts: Vec<Family>, lighting: Lighting) -> Result<Arc<Self>> {
        // Bits 16-23 of `GpuDraw::flags`, less the kit's own.
        ensure!(
            hosts.len() < 256,
            "{} surface families: at most 255 beside the kit's",
            hosts.len()
        );
        for (i, family) in hosts.iter().enumerate() {
            let name = family.name.as_str();
            let plain = name.starts_with(|c: char| c.is_ascii_lowercase())
                && name
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit());
            ensure!(
                plain,
                "surface family `{name}`: a name is lower-case letters and digits, a letter first"
            );
            // The kit's own family, and the dispatchers' prefix.
            ensure!(
                !["lit", "family", SCENE].contains(&name),
                "surface family `{name}`: the name is the kit's"
            );
            ensure!(
                hosts[..i].iter().all(|f| f.name != name),
                "surface family `{name}` is registered twice"
            );
            ensure!(
                !(family.vertex && family.bake),
                "surface family `{name}`: impostors bake no vertex stage, so it can't bake"
            );
        }
        let moded = |part: fn(&ViewMode) -> bool| lighting.modes.iter().any(part);
        ensure!(
            !moded(|m| m.shade.is_some())
                || lighting
                    .shade
                    .as_deref()
                    .unwrap_or(KIT_SHADE)
                    .contains("fn shade("),
            "scene lighting: view modes need the plain `fn shade(` to stand beside"
        );
        ensure!(
            !moded(|m| m.fog.is_some())
                || lighting
                    .fog
                    .as_deref()
                    .unwrap_or(KIT_FOG)
                    .contains("fn fog("),
            "scene lighting: view modes need the plain `fn fog(` to stand beside"
        );
        for (i, mode) in lighting.modes.iter().enumerate() {
            let name = mode.name.as_str();
            let plain = name.starts_with(|c: char| c.is_ascii_lowercase())
                && name
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit());
            ensure!(
                plain,
                "view mode `{name}`: a name is lower-case letters and digits, a letter first"
            );
            ensure!(
                !["lit", "family", "view", SCENE].contains(&name)
                    && hosts.iter().all(|f| f.name != name)
                    && lighting.modes[..i].iter().all(|m| m.name != name),
                "view mode `{name}`: the name is taken"
            );
        }
        let families = Self { hosts, lighting };
        // Against every module the families join, whatever this device
        // builds: a family that fits one device's shaders fits them all.
        // Side by side: each is a parse and a validation of the kit's
        // whole source, tens of milliseconds.
        let checked = std::thread::scope(|scope| {
            let families = &families;
            let checks = MODULES.map(|module| scope.spawn(move || families.check(module)));
            checks.map(|check| check.join().expect("surface family check"))
        });
        for result in checked {
            result?;
        }
        Ok(Arc::new(families))
    }

    /// The slots `family`'s coverage samples (`Family::coverage_slots`).
    fn coverage_slots(&self, family: FamilyId) -> u8 {
        (family.0.checked_sub(1))
            .and_then(|host| self.hosts.get(usize::from(host)))
            .map_or(0, |f| f.coverage_slots)
    }

    /// Whether `family`'s pieces may draw as impostors (`Family::bake`).
    pub(super) fn bakes(&self, family: FamilyId) -> bool {
        match family.0.checked_sub(1) {
            None => true,
            Some(host) => (self.hosts.get(usize::from(host))).is_some_and(|f| f.bake && !f.light),
        }
    }

    /// `name`'s id here.
    pub fn id(&self, name: &str) -> Option<FamilyId> {
        if name == "lit" {
            return Some(FamilyId::LIT);
        }
        let host = self.hosts.iter().position(|f| f.name == name)?;
        Some(FamilyId(host as u8 + 1))
    }

    /// Families registered, the kit's included.
    pub fn count(&self) -> usize {
        self.hosts.len() + 1
    }

    /// `module`'s WGSL with these families.
    pub(super) fn module(&self, module: Module) -> String {
        self.compose(module, false).text
    }

    /// The frame prelude a plugged pass includes ([`super::FRAME_WGSL`]),
    /// with this set's `fog` in place of the kit's, view modes and all: a
    /// pass drawn in a renderer of this set fogs as its placements and
    /// ground do by calling `fog`. Its lighting's `shade` stays out: that
    /// reads the kit's surface helpers, which a pass doesn't include.
    pub fn pass_prelude(&self) -> String {
        let fog = self.fog();
        // A fog ending in a line comment would swallow the pass's first line.
        let end = if fog.ends_with('\n') { "" } else { "\n" };
        [super::pass::FRAME_PRELUDE, &fog, end].concat()
    }

    /// The scene's `shade` and `fog`, to end a kit module that includes
    /// gpu.wgsl after `FRAME_PRELUDE`.
    pub(super) fn scene(&self) -> String {
        let tint = self.lighting.tint.as_deref().unwrap_or(KIT_TINT);
        [
            &self.shade(),
            "\n",
            &self.fog(),
            "\n",
            &self.unlit(),
            "\n",
            tint,
            "\n",
        ]
        .concat()
    }

    /// The lighting's `shade`, or with view modes, the plain one renamed
    /// and the modes' after it, under a `shade` that picks by the frame's
    /// mode (`lights.view.x`).
    fn shade(&self) -> String {
        let plain = self.lighting.shade.as_deref().unwrap_or(KIT_SHADE);
        let modes = (self.lighting.modes.iter()).map(|m| (m.name.as_str(), m.shade.as_deref()));
        moded(
            plain,
            modes,
            "shade",
            "world: vec3<f32>, s: Surface, emissive: vec3<f32>, has_normals: bool",
            "world, s, emissive, has_normals",
        )
    }

    /// `shade`'s counterpart for `fog`.
    fn fog(&self) -> String {
        let plain = self.lighting.fog.as_deref().unwrap_or(KIT_FOG);
        let modes = (self.lighting.modes.iter()).map(|m| (m.name.as_str(), m.fog.as_deref()));
        moded(
            plain,
            modes,
            "fog",
            "color: vec3<f32>, world: vec3<f32>",
            "color, world",
        )
    }

    /// `shade`'s counterpart for flat colours: plain, they pass unchanged.
    fn unlit(&self) -> String {
        let modes = (self.lighting.modes.iter()).map(|m| (m.name.as_str(), m.unlit.as_deref()));
        moded(
            PLAIN_UNLIT,
            modes,
            "unlit",
            "color: vec3<f32>, world: vec3<f32>",
            "color, world",
        )
    }

    /// `module`'s source: the kit's files, each host family's, then the
    /// dispatchers, a case per host family and the kit's `lit` for every
    /// other id. With `checks`, a compute entry point per host family
    /// besides, calling each of its functions (`check`).
    fn compose(&self, module: Module, checks: bool) -> Composed {
        let mut out = Composed::default();
        // The texture array's enable directive must open the source. The
        // kit's meshes bind no array.
        if module != Module::Pieces {
            out.push(None, "enable wgpu_binding_array;\n");
        }
        if module == (Module::Visibility { corners: true }) {
            out.push(None, "enable wgpu_per_vertex;\n");
        }
        out.push(None, super::pass::FRAME_PRELUDE);
        out.push(None, include_str!("../gpu.wgsl"));
        // The lighting's pieces are blamed as the host's past its families.
        let scene = Some(self.hosts.len());
        let l = &self.lighting;
        let own = |part: &Option<String>, modes: bool| part.is_some() || modes;
        let moded = (l.modes.iter()).any(|m| m.shade.is_some() || m.fog.is_some());
        let unlit = l.modes.iter().any(|m| m.unlit.is_some());
        for (text, blamed) in [
            (self.shade(), own(&l.shade, moded)),
            (self.fog(), own(&l.fog, moded)),
            (self.unlit(), unlit),
            (l.tint.clone().unwrap_or(KIT_TINT.into()), l.tint.is_some()),
        ] {
            out.push(if blamed { scene } else { None }, &text);
            out.push(None, "\n");
        }
        out.push(None, include_str!("family.wgsl"));
        if module != Module::Pieces {
            out.push(None, include_str!("placed.wgsl"));
        }
        match module {
            Module::Arena => {}
            Module::Pieces => out.push(None, include_str!("pieces.wgsl")),
            Module::Visibility { corners } => {
                out.push(None, include_str!("visbuf.wgsl"));
                if corners {
                    out.push(None, include_str!("visbuf_corners.wgsl"));
                }
            }
        }
        for (i, family) in self.hosts.iter().enumerate() {
            out.push(Some(i), &family.wgsl);
            out.push(None, "\n");
        }
        for (what, parameters, arguments, result) in DISPATCHERS {
            let cases: Vec<(usize, &Family)> = (self.hosts.iter().enumerate())
                .filter(|(_, f)| match what {
                    "light" => f.light,
                    "vertex" => f.vertex,
                    _ => true,
                })
                .collect();
            out.push(
                None,
                &format!("fn family_{what}({parameters}) -> {result} {{\n"),
            );
            if !cases.is_empty() {
                out.push(None, "    switch family_of(material) {\n");
                for (i, family) in cases {
                    let (id, name) = (i + 1, &family.name);
                    let case =
                        format!("        case {id}u {{ return {name}_{what}({arguments}); }}\n");
                    out.push(Some(i), &case);
                }
                out.push(None, "        default {}\n    }\n");
            }
            out.push(None, &format!("    return lit_{what}({arguments});\n}}\n"));
        }
        if checks {
            let l = &self.lighting;
            if l.shade.is_some() || l.fog.is_some() || l.tint.is_some() || !l.modes.is_empty() {
                let check = format!(
                    "@compute @workgroup_size(1) fn {CHECK}{SCENE}() {{\n    \
                     var s: Surface;\n    \
                     let lit = fog(shade(vec3<f32>(0.0), s, vec3<f32>(0.0), true), vec3<f32>(0.0));\n    \
                     let flat = unlit(vec3<f32>(0.0), vec3<f32>(0.0));\n    \
                     let tint = ground_tint_decode(vec4<f32>(0.5));\n}}\n"
                );
                out.push(scene, &check);
            }
            for (i, family) in self.hosts.iter().enumerate() {
                let name = &family.name;
                let mut light = if family.light {
                    format!("    let lit = {name}_light(material, p, s);\n")
                } else {
                    String::new()
                };
                if family.vertex {
                    light.push_str(&format!(
                        "    var v: FamilyVertex;\n    let at = {name}_vertex(material, v);\n"
                    ));
                }
                let check = format!(
                    "@compute @workgroup_size(1) fn {CHECK}{name}() {{\n    \
                     var material: GpuDraw;\n    var p: LitPixel;\n    \
                     var t: LitTexels;\n    var c: Cover;\n    \
                     let covered = {name}_coverage(material, c);\n    \
                     let s = {name}_surface(material, p, t);\n{light}}}\n"
                );
                out.push(Some(i), &check);
            }
        }
        out
    }

    /// Checks every host family against `module` as naga would the device's
    /// shader, and against the rules. Each family's functions are also
    /// called from a compute entry point: naga then refuses derivatives,
    /// implicit-level samples and `discard` anywhere under them, which in
    /// the fragment stages that call a surface it would let through.
    fn check(&self, module: Module) -> Result<()> {
        use naga::valid::{Capabilities, EntryPointError, ValidationError, ValidationFlags};
        let source = self.compose(module, true);
        let blame = |owner: Option<usize>, error: String| match owner {
            Some(i) if i == self.hosts.len() => anyhow!("scene lighting: {error}"),
            Some(i) => anyhow!("surface family `{}`: {error}", self.hosts[i].name),
            None => anyhow!("surface families: {error}"),
        };
        // The first place the error names inside a host family's source.
        let at = |spans: &mut dyn Iterator<Item = naga::Span>| {
            spans
                .filter_map(|s| s.to_range())
                .find_map(|r| source.owner(r.start))
        };
        let parsed = naga::front::wgsl::parse_str(&source.text).map_err(|e| {
            let owner = at(&mut e.labels().map(|(span, _)| span));
            blame(owner, e.emit_to_string(&source.text))
        })?;
        // What each family declares, by where it stands in the source.
        let mut declared: Vec<(naga::Span, &Option<String>)> = Vec::new();
        declared.extend(
            (parsed.functions.iter()).map(|(h, f)| (parsed.functions.get_span(h), &f.name)),
        );
        declared.extend(
            (parsed.global_variables.iter())
                .map(|(h, g)| (parsed.global_variables.get_span(h), &g.name)),
        );
        declared.extend(
            (parsed.constants.iter()).map(|(h, c)| (parsed.constants.get_span(h), &c.name)),
        );
        declared.extend(
            (parsed.overrides.iter()).map(|(h, o)| (parsed.overrides.get_span(h), &o.name)),
        );
        declared.extend((parsed.types.iter()).map(|(h, t)| (parsed.types.get_span(h), &t.name)));
        for (span, name) in declared {
            let (Some(range), Some(name)) = (span.to_range(), name) else {
                continue;
            };
            let Some(i) = source.owner(range.start) else {
                continue;
            };
            let (prefix, own) = match self.hosts.get(i) {
                Some(family) => (format!("{}_", family.name), false),
                None => (
                    format!("{SCENE}_"),
                    [
                        "shade",
                        "fog",
                        "unlit",
                        "ground_tint_decode",
                        "view_plain_shade",
                        "view_plain_fog",
                        "view_plain_unlit",
                    ]
                    .contains(&name.as_str())
                        // A view mode's names start with its own.
                        || (self.lighting.modes.iter())
                            .any(|m| name.starts_with(&format!("{}_", m.name))),
                ),
            };
            // The check's entry point is this registry's, not the family's.
            if !own && !name.starts_with(&prefix) && !name.starts_with(CHECK) {
                return Err(blame(
                    Some(i),
                    format!("declares `{name}`: its names start with `{prefix}`"),
                ));
            }
        }
        // The arena's textures by the kit's accessor alone: the kit's own
        // meshes have no array, and `family_texel` reaches the mesh's own
        // bindings (pieces.wgsl).
        for (handle, function) in parsed.functions.iter() {
            let range = parsed.functions.get_span(handle).to_range();
            let Some(i) = range.and_then(|r| source.owner(r.start)) else {
                continue;
            };
            let direct = function.expressions.iter().any(|(_, e)| match e {
                naga::Expression::GlobalVariable(g) => {
                    parsed.global_variables[*g].name.as_deref() == Some("t_albedos")
                }
                _ => false,
            });
            if direct {
                let name = function.name.as_deref().unwrap_or_default();
                return Err(blame(
                    Some(i),
                    format!("`{name}` reads `t_albedos`: slots are sampled with `family_texel`"),
                ));
            }
        }
        Validator::new(ValidationFlags::all(), Capabilities::all())
            .validate(&parsed)
            .map_err(|e| {
                let stub = |name: &str| {
                    let family = name.strip_prefix(CHECK)?;
                    if family == SCENE {
                        return Some(self.hosts.len());
                    }
                    self.hosts.iter().position(|f| f.name == family)
                };
                match e.as_inner() {
                    ValidationError::EntryPoint {
                        name,
                        source: EntryPointError::ForbiddenStageOperations,
                        ..
                    } if stub(name).is_some() => blame(
                        stub(name),
                        "takes a screen derivative, samples a texture at an implicit level \
                         or discards, which the resolve cannot do for it: derivatives come from `LitPixel`, and a caller cuts by coverage"
                            .into(),
                    ),
                    _ => {
                        let owner = at(&mut e.spans().map(|(span, _)| *span));
                        blame(owner, e.emit_to_string(&source.text))
                    }
                }
            })?;
        Ok(())
    }
}

/// Opens the name of the compute entry point `Families::check` calls a
/// family's functions from.
const CHECK: &str = "family_check_";

/// The lighting's names past `shade` and `fog` start with this and `_`; no
/// family may take it.
const SCENE: &str = "scene";

/// `plain`, the WGSL defining `fn <what>(<params>)`, as it is when no mode
/// has a `<what>` of its own. Else `plain` renamed `view_plain_<what>`, the
/// modes' sources after it, and a `<what>` that calls mode `k`'s
/// `<name>_<what>` while `lights.view.x` is `k` and the plain one
/// otherwise.
fn moded<'a>(
    plain: &str,
    modes: impl Iterator<Item = (&'a str, Option<&'a str>)>,
    what: &str,
    params: &str,
    arguments: &str,
) -> String {
    let own: Vec<(usize, &str, &str)> = (modes.enumerate())
        .filter_map(|(i, (name, source))| Some((i + 1, name, source?)))
        .collect();
    if own.is_empty() {
        return plain.to_owned();
    }
    let mut out = plain.replacen(&format!("fn {what}("), &format!("fn view_plain_{what}("), 1);
    for (_, _, source) in &own {
        out.push('\n');
        out.push_str(source);
    }
    out.push_str(&format!(
        "\nfn {what}({params}) -> vec3<f32> {{\n    switch u32(lights.view.x) {{\n"
    ));
    for (k, name, _) in &own {
        out.push_str(&format!(
            "        case {k}u {{ return {name}_{what}({arguments}); }}\n"
        ));
    }
    out.push_str(&format!(
        "        default {{}}\n    }}\n    return view_plain_{what}({arguments});\n}}\n"
    ));
    out
}

/// Draw index ranges by pipeline. Pieces are ordered lit, flat, decals,
/// then the rest (never drawn as placements); shadow casters first in each, and in those
/// the ones whose albedo never cuts them out first, so every pipeline's
/// draws are contiguous. Decals never cast.
#[derive(Clone, Default)]
pub(super) struct Ranges {
    pub flat: Range<u32>,
    /// Lit draws drawn whole, casters then the rest: their depth needs no
    /// fragment stage, where a cutout's samples its albedo for every
    /// fragment and loses early depth tests.
    pub lit_opaque: [Range<u32>; 2],
    /// Lit draws their family's coverage cuts out, split the same way.
    pub lit_cutout: [Range<u32>; 2],
    pub flat_casters: Range<u32>,
    /// Every decal draw: those on the ground (`ground_decals`) first, drawn
    /// in the opaque pass between the ground and the models, then those on
    /// models, drawn in the blend pass after every opaque placement.
    pub decals: Range<u32>,
    pub ground_decals: Range<u32>,
    /// Lit pieces with a pose, after the kept layers (`Renderer::render`);
    /// split as `lit_opaque` and `lit_cutout` by casting.
    pub posed: Range<u32>,
    pub posed_whole: [Range<u32>; 2],
    pub posed_cutout: [Range<u32>; 2],
    /// Impostor quads, after every mesh draw; they all cast shadows.
    pub impostors: Range<u32>,
}

impl Ranges {
    /// The same ranges in other units (the arena's lists).
    fn map(&self, f: impl Fn(Range<u32>) -> Range<u32>) -> Self {
        Ranges {
            flat: f(self.flat.clone()),
            lit_opaque: self.lit_opaque.clone().map(&f),
            lit_cutout: self.lit_cutout.clone().map(&f),
            flat_casters: f(self.flat_casters.clone()),
            decals: f(self.decals.clone()),
            ground_decals: f(self.ground_decals.clone()),
            posed: f(self.posed.clone()),
            posed_whole: self.posed_whole.clone().map(&f),
            posed_cutout: self.posed_cutout.clone().map(&f),
            impostors: f(self.impostors.clone()),
        }
    }
}

/// Every draw's geometry, material and albedo in shared arrays: bind group
/// 3 of placed.wgsl.
pub(super) struct Arena {
    /// Tells readbacks of the culls' wants for this arena's slots apart
    /// from an earlier arena's.
    pub id: u64,
    pub group: wgpu::BindGroup,
    /// Every draw's indices, 32 bits, each value its stream's (`Stream`).
    pub indices: wgpu::Buffer,
    /// The arena's indirect draws, each a list of placements: first index
    /// and index count. One per group of draw slots sharing indices
    /// (`pack`), a thinning group's one per bucket with triangles of its
    /// own, so every draw has placements it may list. Keyed by slot
    /// instead, half the draws on large islands were always empty, as
    /// pieces repeating a mesh listed into its first piece's draw.
    lists: Vec<[u32; 2]>,
    /// Per draw slot, the list taking its placements, or `NO_LIST`.
    list_of: Vec<u32>,
    /// `Placed::ranges` in lists: what each pipeline draws.
    pub ranges: Ranges,
    /// Entries of every mesh's triangle-start table (visbuf.wgsl p_firsts).
    pub tables: u32,
    /// Per atlas layer: the impostor material that bakes it (others with
    /// the same mesh share it) and its source mesh's index range.
    pub impostors: Vec<(u32, Range<u32>)>,
    /// Each texture slot's texture, slot 0 (white) `None`: the binding
    /// array, bound anew whenever residency makes one again (`bind`).
    pub textures: Vec<Option<Arc<StreamedTexture>>>,
    /// Per slot, the want impostor bakes put on it (residency.rs): their
    /// views sample it at their own scale, and a bake lasts.
    pub floors: Vec<u8>,
    /// Per material, its texture slots and how fine a level each can
    /// sample (`GpuTexels`): what the cull asks residency for.
    pub texels: wgpu::Buffer,
    buffers: [wgpu::Buffer; 8],
    /// The poses' clock's place in the shapes buffer, written each frame
    /// (`set_clock`); `None` without poses.
    clock: Option<u64>,
    cap: u32,
}

/// A material's texture slots (albedo, normal map, surface map, alpha map;
/// 0 none) and per slot log2 of the texels a unit of the mesh's own space
/// spans at least (`Mesh::uv_density` times the texture's side): the cull
/// asks for level `bias - log2(pixels per unit)`. -1e30 always wants the
/// full level. `cutout.x`: the albedo's slot when shadow maps sample its
/// alpha (a caster cut out by it), else 0; shadow culls ask for no other.
/// `cutout.y`: how the mesh bends to the terrain, which stretches it by
/// the ground's slope, past what its UVs say: 0 never, 1 where the
/// placement allows (`adapting`), 2 always (draped decals). Bent
/// placements want every level.
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct GpuTexels {
    slots: [u32; 4],
    bias: [f32; 4],
    cutout: [u32; 4],
    /// The family's slots (`MeshData::slots`), 0 none, and their biases
    /// (times `MeshData::slot_repeats`). `cutout.z`: those its coverage
    /// samples, bit k for slot k.
    extra: [u32; MAX_SLOTS],
    extra_bias: [f32; MAX_SLOTS],
}

/// Levels the cull asks for past the bound it computes (cull.wgsl
/// `TEXEL_MARGIN`): samplers round the anisotropic footprint's tap count,
/// and pick a level up to 0.57 finer than its minor axis on an RTX 4080
/// SUPER (stretch 3.5; tests.rs `texel_margin_covers_the_samplers_rounding`
/// measures it).
pub(super) const TEXEL_MARGIN: f32 = 0.75;

/// Arenas packed so far, for `Arena::id`.
static ARENAS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

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
    pub lit: [u32; 4],
    pub eye: [f32; 4],
    pub reach: [f32; 4],
    /// The presented ground's cell ranges (heights.rs): origin x and z, 1
    /// over a cell's side in metres, 1 when bound.
    pub ground: [f32; 4],
    /// Their finest level's cells across x and z, and their level count.
    pub cells: [u32; 4],
    /// The posed draws, first and end, and the target's tiles across
    /// (`visbuf::POSED_TILE`), 0 where the cull marks none.
    pub posed: [u32; 4],
}

/// A draw slot: a piece at one level (with thinning, one bucket of it),
/// which the cull lists placements under, its visible rows from `base`;
/// the arena draws slots sharing indices as one (`Arena::lists`).
#[derive(Clone, Copy)]
pub(super) struct PlacedDraw {
    pub piece: u32,
    /// The impostor's level is one past the piece's meshes.
    pub level: u32,
    pub base: u32,
    pub impostor: bool,
    /// Indices a thinning bucket draws; 0 for the whole mesh. The arena
    /// draws only the bucket's own, past the bucket before (`Stream`).
    pub count: u32,
    /// A thinning level's bucket, 0 for other draws.
    pub bucket: u32,
    /// The mesh this draws, shared by a thinning level's buckets and by
    /// every piece drawing the same mesh: what visible entries carry and
    /// the arena's `GpuDraw` table is indexed by.
    pub material: u32,
    /// Its piece's pipeline, decal placement, shadow and cutout class,
    /// which decide what ranges and passes draw it (`Placed::set_pieces`);
    /// `IMPOSTOR_CLASS` for impostors.
    pub class: u8,
}

const IMPOSTOR_CLASS: u8 = u8::MAX;

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
    /// Each row's instance word (`Placed::words`), beside the transforms
    /// and uploaded with them.
    pub words: wgpu::Buffer,
    pub piece_of: wgpu::Buffer,
    pub pieces: wgpu::Buffer,
    /// Per draw slot, its list and where that starts (`Placed::lists`).
    pub lists: wgpu::Buffer,
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
    /// The distinct poses of the pieces (`Piece::pose`), and per piece its
    /// pose's place among them, `NO_POSE` without. The arena keeps their
    /// records after every material's shape (`Arena::pack`), and each
    /// posed row names its record (`stamp`).
    poses: Vec<Arc<Pose>>,
    pose_of: Vec<u32>,
    /// Baked impostors; until then pieces draw their meshes only.
    pub atlas: Option<Atlas>,
    /// An arena was attempted for this piece list (it fails when the
    /// textures outnumber the device's binding array, and then the
    /// placements don't draw).
    arena_tried: bool,
    pub count: u32,
    /// Visible list length: every placement once per level of its piece.
    pub capacity: u32,
    /// Per piece, the rows placing it.
    rows_of: Vec<u32>,
    /// Bounds of every placement, for fitting the shadow map.
    pub bounds: [Vec3; 2],
    /// Rows that bend to the terrain (`gpu::adapting`): each one's
    /// footprint across (x0, z0, x1, z1) and how far it reaches up or down
    /// from the ground. A sculpt under one moves all of it, so a partial
    /// shadow redraw takes in the footprints it touches
    /// (`Device::ground_moved`). Moved rows add their new footprint and
    /// keep the old one; a new table starts over.
    pub adapting: Vec<([f32; 4], f32)>,
    /// The table as the host gave it, so a released device can upload it
    /// again.
    transforms: Vec<Mat4>,
    /// Per row, the host's opaque word (placed.wgsl p_words), 0 unless set.
    words: Vec<u32>,
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
    /// Which levels of the arena's textures stay on the GPU.
    pub residency: super::residency::Residency,
    /// The surface families pieces name theirs in
    /// (`Renderer::with_families`); the kit's alone by default.
    pub families: Arc<Families>,
}

/// A piece's own place among the poses (`Placed::pose_of`) without one.
const NO_POSE: u32 = u32::MAX;

/// The box a piece keeps to in its mesh's space: its pose's where it has
/// one, its full mesh's otherwise.
fn reach(piece: &Piece) -> [Vec3; 2] {
    match &piece.pose {
        Some(pose) => pose.bounds,
        None => piece.levels[0].0.bounds(),
    }
}

/// Panics on a piece list `Placed::set_pieces` can't draw: the renderer
/// checks it as the host hands it over, before a frame applies it.
pub(super) fn check_pieces(pieces: &[Piece], families: &Families) {
    assert!(pieces.iter().all(|p| {
        (1..=MAX_LEVELS).contains(&(p.levels.len() + usize::from(p.impostor.is_some())))
    }));
    assert!(
        (pieces.iter()).all(|p| p.pose.as_ref().is_none_or(|pose| {
            let data = p.levels[0].0.data();
            matches!(data.material, Material::Lit)
                && data.thinning.is_none()
                && p.impostor.is_none()
                && pose.joints > 0
                && pose.period > 0.
                && !pose.frames.is_empty()
                && pose.frames.len().is_multiple_of(pose.joints as usize)
        })),
        "a posed piece is lit, neither thins nor has an impostor, and has whole frames"
    );
    assert!(
        (pieces.iter()).all(|p| {
            let thin = |(m, _): &(Arc<Mesh>, f32)| m.data().thinning.is_some();
            p.levels.iter().all(thin) || !p.levels.iter().any(thin)
        }),
        "a piece thins at every level or at none"
    );
    assert!(
        (pieces.iter().flat_map(|p| &p.levels))
            .all(|(m, _)| usize::from(m.data().family.0) < families.count()),
        "a piece names a surface family the renderer's registry lacks"
    );
}

impl Placed {
    pub fn set_pieces(&mut self, pieces: Vec<Piece>) {
        let families = self.families.clone();
        check_pieces(&pieces, &families);
        // The atlas bakes through the family's surface (fs_bake), which
        // only a family that says it reads nothing the bake lacks may do;
        // others keep their mesh levels.
        let mut pieces = pieces;
        for piece in &mut pieces {
            if !families.bakes(piece.levels[0].0.data().family) {
                piece.impostor = None;
            }
        }
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
        self.poses.clear();
        let mut pose_ids: HashMap<usize, u32> = HashMap::new();
        self.pose_of = (pieces.iter())
            .map(|piece| match &piece.pose {
                Some(pose) => *pose_ids
                    .entry(Arc::as_ptr(pose) as usize)
                    .or_insert_with(|| {
                        self.poses.push(pose.clone());
                        self.poses.len() as u32 - 1
                    }),
                None => NO_POSE,
            })
            .collect();
        // (pipeline, decal on models, casts no shadow, cut out): the
        // pipelines are 0 lit, 1 flat, 2 decal, 3 lit and posed, 4 none.
        // Posed pieces draw apart, after the layers a view keeps. Decals on
        // the ground sort first, so each kind's draws are contiguous
        // (`Ranges::ground_decals`).
        let class = |piece: &Piece| {
            let data = piece.levels[0].0.data();
            let pipeline = match data.material {
                _ if piece.pose.is_some() => 3,
                Material::Lit => 0,
                Material::Flat => 1,
                Material::Decal(_) => 2,
                _ => 4,
            };
            let on_models = matches!(data.material, Material::Decal(d) if !d.ground);
            // Cut out by the albedo's alpha at some level (fs_placed_depth).
            let cut = (piece.levels.iter())
                .any(|(m, _)| !m.data().ignore_alpha && m.data().texture.is_some());
            (pipeline, on_models, !super::casts_shadow(data), cut)
        };
        let mut order: Vec<usize> = (0..pieces.len()).collect();
        order.sort_by_key(|&p| class(&pieces[p]));
        self.first_draw = vec![0; pieces.len()];
        self.draws.clear();
        let mut ends = [[[[0u32; 2]; 2]; 2]; 5];
        for &p in &order {
            self.first_draw[p] = self.draws.len() as u32;
            let (pipeline, on_models, shadowless, cut) = class(&pieces[p]);
            let code = (pipeline * 8) as u8
                | u8::from(on_models) << 2
                | u8::from(shadowless) << 1
                | u8::from(cut);
            if pieces[p].levels[0].0.data().thinning.is_some() {
                // THIN_BUCKETS prefix draws per level, each level's parts
                // ranked as the full mesh's (cull.wgsl).
                for (level, (mesh, _)) in pieces[p].levels.iter().enumerate() {
                    let counts = mesh.thin_counts().expect("asserted above");
                    self.draws
                        .extend((0..).zip(counts).map(|(bucket, &count)| PlacedDraw {
                            piece: p as u32,
                            level: level as u32,
                            base: 0,
                            impostor: false,
                            count,
                            bucket,
                            material: material[p][level],
                            class: code,
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
                        bucket: 0,
                        material: material[p][level],
                        class: code,
                    }));
            }
            ends[pipeline][usize::from(on_models)][usize::from(shadowless)][usize::from(cut)] =
                self.draws.len() as u32;
        }
        // Empty classes end where the one before did.
        let mut end = 0;
        for e in ends.iter_mut().flatten().flatten().flatten() {
            end = (*e).max(end);
            *e = end;
        }
        // Each pipeline's end. Lit draws are casters, whole then cut out,
        // then the rest the same way; so are posed ones.
        let last = |pipeline: usize| ends[pipeline][1][1][1];
        let lit = ends[0][0];
        let split = |cut: usize| {
            [0, 1].map(|s| {
                let whole = if s == 0 { 0 } else { lit[0][1] };
                match cut {
                    0 => whole..lit[s][0],
                    _ => lit[s][0]..lit[s][1],
                }
            })
        };
        let posed = ends[3][0];
        let posed_split = |cut: usize| {
            [0, 1].map(|s| {
                let whole = if s == 0 { last(2) } else { posed[0][1] };
                match cut {
                    0 => whole..posed[s][0],
                    _ => posed[s][0]..posed[s][1],
                }
            })
        };
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
                    bucket: 0,
                    material: material[p][piece.levels.len()],
                    class: IMPOSTOR_CLASS,
                });
            }
        }
        self.ranges = Ranges {
            flat: last(0)..last(1),
            lit_opaque: split(0),
            lit_cutout: split(1),
            flat_casters: last(0)..ends[1][0][0][1],
            decals: last(1)..last(2),
            ground_decals: last(1)..ends[2][0][1][1],
            posed: last(2)..last(3),
            posed_whole: posed_split(0),
            posed_cutout: posed_split(1),
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
        // Across cores, in chunks merged after: 64k rows' corners took
        // 5.6 ms of an 1800 island's rebuild on one thread, with the
        // renderer's lock held.
        const CHUNK: usize = 4096;
        let pieces = &self.pieces;
        let bounds = crate::task::interactive(|| {
            use rayon::prelude::*;
            (piece_of.par_chunks(CHUNK).zip(transforms.par_chunks(CHUNK)))
                .map(|(ps, ms)| {
                    math::bounds(ps.iter().zip(ms).flat_map(|(&p, m)| {
                        math::corners(reach(&pieces[p as usize])).map(|c| math::transform(m, c))
                    }))
                })
                .reduce(|| math::bounds(std::iter::empty()), math::union)
        });
        self.set_rows(piece_of.to_vec(), transforms.to_vec(), bounds);
    }

    /// `set_placements` with the rows' world bounds already worked out
    /// (off the UI thread, by a host that knows its pieces' bounds).
    pub fn set_rows(&mut self, piece_of: Vec<u32>, transforms: Vec<Mat4>, bounds: [Vec3; 2]) {
        assert_eq!(piece_of.len(), transforms.len());
        let mut transforms = transforms;
        self.stamp(&piece_of, &mut transforms);
        let mut per_piece = vec![0u32; self.pieces.len()];
        for &p in &piece_of {
            per_piece[p as usize] += 1;
        }
        let mut base = 0;
        for d in &mut self.draws {
            d.base = base;
            base += per_piece[d.piece as usize];
        }
        self.capacity = base;
        self.rows_of = per_piece;
        self.count = piece_of.len() as u32;
        self.bounds = bounds;
        self.adapting.clear();
        self.add_adapting(&piece_of, &transforms);
        self.words = vec![0; transforms.len()];
        self.transforms = transforms;
        self.piece_of = piece_of;
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
        let (piece_of, mut moved) = (
            std::mem::take(&mut self.piece_of),
            std::mem::take(&mut self.transforms),
        );
        self.stamp(
            &piece_of[first as usize..end],
            &mut moved[first as usize..end],
        );
        (self.piece_of, self.transforms) = (piece_of, moved);
        // Old footprints stay, so the list grows with every move of an
        // adapting row: past a few per row it starts over from the table.
        if self.adapting.len() > 4 * (self.count as usize).max(256) {
            self.adapting.clear();
            let (piece_of, all) = (
                std::mem::take(&mut self.piece_of),
                std::mem::take(&mut self.transforms),
            );
            self.add_adapting(&piece_of, &all);
            (self.piece_of, self.transforms) = (piece_of, all);
        } else {
            let piece_of = std::mem::take(&mut self.piece_of);
            self.add_adapting(&piece_of[first as usize..end], transforms);
            self.piece_of = piece_of;
        }
        if !self.stale {
            self.moved.push(first..end as u32);
        }
    }

    /// Writes into each row's last element the pose record its piece's
    /// pose has in the arena's shapes, plus 2 (placed.wgsl `pose_turn`),
    /// and 1, as in any affine matrix, where it has none. Records follow
    /// every material's shape.
    fn stamp(&self, piece_of: &[u32], transforms: &mut [Mat4]) {
        for (&p, m) in piece_of.iter().zip(transforms) {
            m[15] = match self.pose_of.get(p as usize) {
                Some(&k) if k != NO_POSE => (2 + self.materials + k) as f32,
                _ => 1.,
            };
        }
    }

    /// Adds `rows`' entries to `adapting`: each adapting row's piece as a
    /// sphere about its origin at the row's largest scale, plus its rise up
    /// or down. A radius per piece and a scale per row, not eight corners
    /// turned: a table's rows all pass through here on the UI thread.
    fn add_adapting(&mut self, piece_of: &[u32], transforms: &[Mat4]) {
        let radius: Vec<f32> = (self.pieces.iter())
            .map(|p| {
                let corners = math::corners(reach(p));
                corners.iter().map(|c| math::length(*c)).fold(0., f32::max)
            })
            .collect();
        for (&p, m) in piece_of.iter().zip(transforms) {
            // Column 0's w: may bend (`gpu::adapting`); column 1's: the rise.
            if m[3] == 0. {
                continue;
            }
            let scale = [0, 4, 8]
                .map(|c| math::length([m[c], m[c + 1], m[c + 2]]))
                .into_iter()
                .fold(0., f32::max);
            let r = radius[p as usize] * scale;
            let (x, z) = (m[12], m[14]);
            self.adapting
                .push(([x - r, z - r, x + r, z + r], r + m[7].abs()));
        }
    }

    /// Sets rows `first..`'s instance words. They ride the transforms'
    /// upload: what a word changes is drawn, never culled or bounded.
    pub fn set_words(&mut self, first: u32, words: &[u32]) {
        // Frames that keep their layers redraw on a version change.
        self.version += 1;
        let end = first as usize + words.len();
        assert!(end <= self.count as usize, "rows past the placement table");
        self.words[first as usize..end].copy_from_slice(words);
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
                let [lo, hi] = reach(piece);
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

    /// Per draw slot, the indirect draw the cull lists its placements in
    /// and where that draw's visible list starts (cull.wgsl `c_lists`).
    /// Until the arena lands each slot is its own (nothing draws them);
    /// then slots sharing indices share its draw (`Arena::lists`), which
    /// has room for the rows of every slot listing in it.
    pub fn lists(&self) -> Vec<[u32; 2]> {
        let Some(arena) = &self.arena else {
            return (0..).zip(&self.draws).map(|(i, d)| [i, d.base]).collect();
        };
        let mut room = vec![0u32; arena.lists.len()];
        for (d, &l) in self.draws.iter().zip(&arena.list_of) {
            if l != NO_LIST {
                room[l as usize] += self.rows_of[d.piece as usize];
            }
        }
        let mut base = 0;
        for r in &mut room {
            base += *r;
            *r = base - *r;
        }
        (arena.list_of.iter())
            .map(|&l| match l {
                NO_LIST => [NO_LIST, 0],
                l => [l, room[l as usize]],
            })
            .collect()
    }

    /// Indirect draws the arguments hold: the arena's lists, else one a
    /// draw slot.
    pub fn indirect_draws(&self) -> u32 {
        match &self.arena {
            Some(arena) => arena.lists.len() as u32,
            None => self.draws.len() as u32,
        }
    }

    /// The indirect arguments each frame starts from (`ARGS` words a draw,
    /// `indirect_draws` of them): index count, no instances yet, and with
    /// an arena the list's first index. Base vertex and first instance
    /// stay zero (`ARGS`). Draws of pieces not yet resident draw nothing,
    /// nor impostors before their bake.
    pub fn args_template(&self) -> Vec<u32> {
        if let Some(arena) = &self.arena {
            let impostors = &arena.ranges.impostors;
            return (0..)
                .zip(&arena.lists)
                .flat_map(|(l, &[first, count])| {
                    let baked = self.atlas.is_some() || !impostors.contains(&l);
                    [if baked { count } else { 0 }, 0, first, 0, 0]
                })
                .collect();
        }
        let mut out = Vec::with_capacity(self.draws.len() * ARGS as usize);
        for d in &self.draws {
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
            out.extend([count, 0, 0, 0, 0]);
        }
        out
    }

    /// Switches impostor levels on: the piece table and indirect arguments
    /// pick them up next frame, after the bake that made the atlas. The
    /// version moves with the table's upload, not here: bumped in the bake's
    /// frame, the shadow map redrew against the old table and then kept it.
    pub(super) fn set_atlas(&mut self, atlas: Atlas) {
        self.atlas = Some(atlas);
        self.table_dirty = true;
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
    /// Whether posed placements draw (`Piece::pose`), from the arena: they
    /// move by the clock, so views keep drawing.
    pub fn posing(&self) -> bool {
        self.count > 0
            && self
                .arena
                .as_ref()
                .is_some_and(|a| !a.ranges.posed.is_empty())
    }

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

    /// Whether piece meshes or placement uploads are still to come. An
    /// arena still packing or filling counts: pieces draw only from it, so
    /// until it lands they draw nothing. Without it a slow
    /// packing thread (a loaded machine) let offscreen frames settle, and
    /// windows stop asking for frames, with the placements missing.
    pub fn pending(&self) -> bool {
        self.resident.iter().any(Option::is_none)
            || !self.unstreamed.is_empty()
            || self.stale
            || !self.moved.is_empty()
            || self.upload.is_some()
    }

    pub(super) fn upload_pending(&self) -> bool {
        self.upload.is_some()
    }

    pub(super) fn placements_pending(&self) -> bool {
        self.stale || !self.moved.is_empty()
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
        // Pieces keep no geometry of their own: the arena holds it once
        // and draws them all, so making their meshes is cheap.
        let span = crate::profile::span("gpu sync: placed meshes");
        for p in 0..self.placed.pieces.len() {
            if self.placed.resident[p].is_some() {
                continue;
            }
            // Pieces upload no bytes, but creating 542 models'
            // meshes and textures took a 175 ms frame: they count time.
            if budget.spent() {
                break;
            }
            let meshes: Vec<Arc<Mesh>> = (self.placed.pieces[p].levels.iter())
                .map(|(m, _)| m.clone())
                .collect();
            let resident = (meshes.iter())
                .map(|mesh| self.mesh_with(gpu, kit, mesh, budget, false))
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
            && !placed.pieces.is_empty()
            && placed.resident.iter().all(Option::is_some)
            && let Some((_, cap)) = &kit.arena
        {
            placed.arena_tried = true;
            if placed.packs() {
                let _span = crate::profile::span("gpu sync: arena pack");
                placed.upload = Arena::pack(gpu, *cap, placed);
            } else {
                eprintln!(
                    "{} placements of {} materials exceed the 32 bits a visible entry packs; they don't draw",
                    placed.count, placed.materials,
                );
            }
        }
        if let Some(upload) = &mut placed.upload {
            let _span = crate::profile::span("gpu sync: arena upload");
            if upload.step(gpu, budget)
                && let Some((layout, _)) = &kit.arena
            {
                placed.arena = placed.upload.take().map(|u| u.finish(gpu, kit, layout));
                // The shadow map follows what placements draw with, and the
                // piece table turns thinning on with the arena.
                placed.version += 1;
                placed.table_dirty = true;
            }
        }
        if std::mem::take(&mut placed.stale) {
            placed.moved.clear();
            let (transforms, piece_of) = (&placed.transforms, &placed.piece_of);
            let lists = placed.lists();
            // A matrix, a piece and a word a row.
            budget.charge(transforms.len() * 72);
            let usage = wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST;
            // At least one element each: a binding smaller than its
            // shader's element fails validation, and with no rows the
            // shadow maps' culls still bind them.
            let buffer = |label, bytes: &[u8], element: usize| {
                let mut bytes = std::borrow::Cow::Borrowed(bytes);
                if bytes.len() < element {
                    bytes.to_mut().resize(element, 0);
                }
                buffer_init(gpu, label, &bytes, usage)
            };
            let pieces = placed.piece_table();
            placed.buffers = Some(PlacedBuffers {
                transforms: buffer(
                    "placed transforms",
                    bytemuck::cast_slice(transforms),
                    size_of::<Mat4>(),
                ),
                words: buffer("placed words", bytemuck::cast_slice(&placed.words), 4),
                piece_of: buffer("placed pieces of", bytemuck::cast_slice(piece_of), 4),
                pieces: buffer(
                    "placed pieces",
                    bytemuck::cast_slice(&pieces),
                    size_of::<GpuPiece>(),
                ),
                lists: buffer("placed lists", bytemuck::cast_slice(&lists), 8),
            });
            placed.table_dirty = false;
        }
        let Some(buffers) = &placed.buffers else {
            return;
        };
        // Moved rows and rows with new words alike write both: a word is
        // a sixteenth of its row's matrix.
        for rows in placed.moved.drain(..) {
            let at = rows.start as usize..rows.end as usize;
            gpu.queue.write_buffer(
                &buffers.transforms,
                u64::from(rows.start) * 64,
                bytemuck::cast_slice(&placed.transforms[at.clone()]),
            );
            gpu.queue.write_buffer(
                &buffers.words,
                u64::from(rows.start) * 4,
                bytemuck::cast_slice(&placed.words[at]),
            );
        }
        if placed.table_dirty && !placed.pieces.is_empty() {
            let table = placed.piece_table();
            gpu.queue
                .write_buffer(&buffers.pieces, 0, bytemuck::cast_slice(&table));
            // The arena landing or going changes which draws list slots.
            let lists = placed.lists();
            gpu.queue
                .write_buffer(&buffers.lists, 0, bytemuck::cast_slice(&lists));
            placed.table_dirty = false;
            // What placements draw with changed: the shadow map and the
            // indirect arguments' template follow.
            placed.version += 1;
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

/// `firsts` entry of a vertex that starts no triangle.
const NO_TRIANGLE: u32 = u32::MAX;
/// Set in a `firsts` entry pointing at a list instead of one triangle.
const FIRSTS_LIST: u32 = 1 << 31;

/// Turns each triangle of one mesh's `indices` (rebased; `base` its first
/// vertex, `first` its first index in the arena; `vertices` the mesh's
/// own) so that as few pixels as possible show a triangle sharing its
/// first vertex with another, keeping winding and order, and fills
/// `firsts` (one per vertex): the index position of the triangle the
/// vertex starts, or `FIRSTS_LIST` and the place of a list in the returned
/// lists of [count, position...]. Those places are the lists' own; the
/// caller moves them, from the vertices returned beside them.
///
/// The visibility buffer stores a pixel's triangle as its first vertex,
/// which the rasteriser hands fragments flat for free; `primitive_index`
/// cost 0.42 ms of Albion's 1.2 ms visibility pass at 5K on an RTX 4080
/// SUPER. Meshes carry about twice as many triangles as vertices, so half
/// the triangles start at a vertex another starts too, and the resolve
/// tells those apart by testing each against the pixel. Vertices go to
/// the largest triangles first, which cover most pixels; a list starts
/// with its largest, which the resolve tries first and stops at.
fn spread_firsts(
    indices: &mut [u32],
    vertices: &[super::ArenaVertex],
    base: u32,
    first: u32,
    firsts: &mut [u32],
) -> (Vec<u32>, Vec<u32>) {
    let n = firsts.len();
    let local = |v: u32| (v - base) as usize;
    // Twice each triangle's area on the mesh's grid, whose step x, y and
    // z share: only the order matters.
    let corner = |v: u32| vertices[local(v)].grid();
    let area = |tri: &[u32]| {
        let [a, b, c] = [corner(tri[0]), corner(tri[1]), corner(tri[2])];
        math::length(math::cross(math::sub(b, a), math::sub(c, a)))
    };
    let mut order: Vec<(f32, u32)> = (indices.chunks_exact(3).enumerate())
        .map(|(t, tri)| (area(tri), t as u32))
        .collect();
    order.sort_unstable_by(|a, b| b.0.total_cmp(&a.0).then(a.1.cmp(&b.1)));
    // Per vertex: started by a triangle yet, and how many share it.
    let mut taken = vec![0u64; n.div_ceil(64)];
    let mut sharing = vec![0u32; n];
    for &(_, t) in &order {
        let tri = &mut indices[3 * t as usize..3 * t as usize + 3];
        let free = (0..3).find(|&k| {
            let v = local(tri[k]);
            taken[v / 64] & 1 << (v % 64) == 0
        });
        // All three taken: the one fewest triangles share, for short lists.
        let k = free.unwrap_or_else(|| (0..3).min_by_key(|&k| sharing[local(tri[k])]).unwrap());
        tri.rotate_left(k);
        let v = local(tri[0]);
        taken[v / 64] |= 1 << (v % 64);
        sharing[v] += 1;
    }
    firsts.fill(NO_TRIANGLE);
    // Each vertex's triangles, largest first.
    let mut shared: Vec<(u32, u32)> = Vec::new();
    for &(_, t) in &order {
        let v = local(indices[3 * t as usize]);
        let at = first + 3 * t;
        if firsts[v] == NO_TRIANGLE {
            firsts[v] = at;
        } else {
            shared.push((v as u32, at));
        }
    }
    let (mut lists, mut owners) = (Vec::new(), Vec::new());
    // Stable: the order within a vertex stays largest first.
    shared.sort_by_key(|s| s.0);
    for run in shared.chunk_by(|a, b| a.0 == b.0) {
        let v = run[0].0 as usize;
        let at = lists.len() as u32;
        lists.push(run.len() as u32 + 1);
        lists.push(firsts[v]);
        lists.extend(run.iter().map(|&(_, p)| p));
        firsts[v] = FIRSTS_LIST | at;
        owners.push(v as u32);
    }
    (lists, owners)
}

/// One run of meshes `pack` filled, at their places in the arena's
/// buffers: vertices and normals from vertex `at[0]`, indices from index
/// `at[1]`, thinning parts from part `at[2]`, and the visibility buffer's
/// triangle-start tables from entry `at[0]` (the meshes' own
/// vertices') and `at[3]` (levels sharing theirs). `Upload::step` writes it
/// and lets it go.
struct Chunk {
    at: [u32; 4],
    vertices: Vec<super::ArenaVertex>,
    normals: Vec<u32>,
    indices: Vec<u32>,
    parts: Vec<[f32; 4]>,
    tables: Vec<u32>,
    shared: Vec<u32>,
}

/// Arena bytes `pack` fills before it hands them on. Packed whole, the
/// continental island's 140 MB of geometry stayed until its last byte was
/// on the GPU, the top of the resident set a few frames after it loaded;
/// now about three runs are held at once. Small in tests, so the arenas
/// they draw cross runs.
const PACK_CHUNK: usize = if cfg!(test) { 4 << 10 } else { 8 << 20 };

/// Indices a mesh takes in the arena: one copy per draw group drawing it
/// (`Arena::pack`).
fn index_count(mesh: &Mesh, copies: u32) -> u32 {
    mesh.index_count() as u32 * copies
}

/// Where `Arena::pack` lays out the meshes it packs, as it goes.
#[derive(Default)]
struct Packing {
    /// Each distinct mesh's [first index, first vertex, first part, table]
    /// and its place in `spans`.
    packed: HashMap<usize, ([u32; 4], usize)>,
    /// Per mesh, the copies of its indices draws take (`Arena::pack`);
    /// one where it has no entry.
    copies: HashMap<usize, u32>,
    /// Distinct meshes in packing order, then sized arrays filled in
    /// parallel: packing 9M vertices on one core, growing the arrays as it
    /// went, took a 330 ms frame on the continental island.
    spans: Vec<Span>,
    vertices: u32,
    indices: u32,
    parts: u32,
    /// Table entries of levels sharing their full mesh's vertices.
    shared: u32,
}

impl Packing {
    /// Where `mesh` packs, and its place in `spans`; packed on first use, a
    /// sharing level's full mesh first.
    fn place(&mut self, mesh: &Arc<Mesh>) -> ([u32; 4], usize) {
        if let Some(&at) = self.packed.get(&key(mesh)) {
            return at;
        }
        let n = mesh.vertex_count() as u32;
        let at = match mesh.shares() {
            None => {
                let data = mesh.data();
                let parts = match data.thinning.is_some() || !data.skin.is_empty() {
                    true => {
                        self.parts += n;
                        self.parts - n
                    }
                    false => NO_PARTS,
                };
                self.vertices += n;
                [self.indices, self.vertices - n, parts, self.vertices - n]
            }
            Some(full) => {
                let ([_, vertex, parts, _], _) = self.place(full);
                self.shared += n;
                [self.indices, vertex, parts, self.shared - n]
            }
        };
        let copies = self.copies.get(&key(mesh)).copied().unwrap_or(1);
        self.indices += index_count(mesh, copies);
        self.spans.push(Span {
            mesh: mesh.clone(),
            index: at[0],
            vertex: at[1],
            part: at[2],
            table: at[3],
            copies,
            streams: Vec::new(),
        });
        let at = (at, self.spans.len() - 1);
        self.packed.insert(key(mesh), at);
        at
    }
}

/// One mesh the arena packs (`pack`): its first index, its first vertex
/// and part (`NO_PARTS` without), and where its triangle-start table
/// begins in the firsts array: at its first vertex, or for a level that
/// shares its full mesh's vertices (`Mesh::shares`), which packs none of
/// its own, that far past every vertex's. Its vertex and part are then the
/// full mesh's. Its indices follow `index` once per copy, valued by
/// `streams`.
struct Span {
    mesh: Arc<Mesh>,
    index: u32,
    vertex: u32,
    part: u32,
    table: u32,
    copies: u32,
    streams: Vec<Stream>,
}

/// A run of indices with values of their own, so the vertex stage knows
/// the run, and from it the draw, by the value alone (placed.wgsl
/// `arena_drawn`): `count` of its mesh's indices from `from`, at `at` in
/// the arena, each valued `virt` plus its vertex less `lo`, in the mesh's
/// own numbering. One a draw group, or with thinning one a bucket's own
/// triangles (`Arena::pack`).
struct Stream {
    at: u32,
    from: u32,
    count: u32,
    lo: u32,
    virt: u32,
}

/// The arena's vertices, indices and thinning parts for `spans`, whose
/// owned vertices, parts, indices and tables each increase, with the
/// impostor quads first, valued `quads` + 0..6 each, and the triangles
/// each vertex starts (`shared` entries past
/// the vertices' for levels sharing vertices), handed to `send` a run of
/// meshes about `PACK_CHUNK` at a time, until it says nobody takes them.
/// Indices are 32 bits, valued as their spans' streams say. A run's
/// meshes fill their own slices, on all cores. Returns the lists of
/// triangles sharing a first vertex (`spread_firsts`), which follow every
/// table.
fn pack(
    spans: &[Span],
    [vertex_count, index_total, part_count, shared]: [u32; 4],
    quads: &[u32],
    mut send: impl FnMut(Chunk) -> bool,
) -> Vec<u32> {
    use bytemuck::allocation::zeroed_vec;
    // Spans each run starts at; the last run ends where the arrays do.
    let mut cuts = vec![0];
    let mut bytes = 0;
    for (k, span) in spans.iter().enumerate() {
        let n = span.mesh.vertex_count();
        bytes += index_count(&span.mesh, span.copies) as usize * 4 + n * 32;
        if bytes >= PACK_CHUNK && k + 1 < spans.len() {
            cuts.push(k + 1);
            bytes = 0;
        }
    }
    cuts.push(spans.len());
    let threads = std::thread::available_parallelism().map_or(4, |n| n.get());
    let mut lists = Vec::new();
    // Where the run starts in each array: vertices (and their tables),
    // indices, parts, sharing levels' tables.
    let mut start = [0u32; 4];
    for (r, run) in cuts.windows(2).enumerate() {
        let run = &spans[run[0]..run[1]];
        let mut end = start;
        for span in run {
            let n = span.mesh.vertex_count() as u32;
            end[1] = end[1].max(span.index + index_count(&span.mesh, span.copies));
            if span.mesh.shares().is_some() {
                end[3] = end[3].max(span.table + n);
            } else {
                end[0] = end[0].max(span.vertex + n);
                if span.part != NO_PARTS {
                    end[2] = end[2].max(span.part + n);
                }
            }
        }
        if r + 2 == cuts.len() {
            end = [vertex_count, index_total, part_count, shared];
        }
        let len = |k: usize| (end[k] - start[k]) as usize;
        let mut chunk = Chunk {
            at: [start[0], start[1], start[2], vertex_count + start[3]],
            vertices: zeroed_vec(len(0)),
            normals: zeroed_vec(len(0)),
            indices: zeroed_vec(len(1)),
            parts: zeroed_vec(len(2)),
            tables: zeroed_vec(len(0)),
            shared: zeroed_vec(len(3)),
        };
        // The quads open the indices, so the first run holds them.
        if r == 0 {
            for (quad, &virt) in chunk.indices.chunks_exact_mut(6).zip(quads) {
                quad.copy_from_slice(&std::array::from_fn::<u32, 6, _>(|k| virt + k as u32));
            }
        }
        // Each mesh's table (in the whole firsts array) and shared-start
        // lists, to append.
        type Lists = (u32, (Vec<u32>, Vec<u32>));
        let found: Vec<Vec<Lists>> = {
            // Split the run's arrays into each span's slices; spans run in
            // order. Positions stay the arena's: `at` is where each rest
            // starts.
            let Chunk {
                vertices,
                normals,
                indices,
                parts,
                tables,
                shared,
                ..
            } = &mut chunk;
            let (mut v, mut n, mut i, mut p) = (
                &mut vertices[..],
                &mut normals[..],
                &mut indices[..],
                &mut parts[..],
            );
            let (mut f, mut g) = (&mut tables[..], &mut shared[..]);
            let [mut v_at, mut i_at, mut p_at, mut g_at] = start.map(|s| s as usize);
            let (mut n_at, mut f_at) = (v_at, v_at);
            // `len` items from `start` off the front of `rest`, which
            // begins at `at`.
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
            let mut jobs = Vec::with_capacity(run.len());
            for span in run {
                let count = span.mesh.vertex_count();
                let owns = span.mesh.shares().is_none();
                let table = if owns {
                    span.table
                } else {
                    vertex_count + span.table
                };
                let mine_v = owns.then(|| {
                    let v = take(&mut v, &mut v_at, span.vertex as usize, count);
                    (v, take(&mut n, &mut n_at, span.vertex as usize, count))
                });
                let indices = index_count(&span.mesh, span.copies) as usize;
                let mine_i = take(&mut i, &mut i_at, span.index as usize, indices);
                let mine_p = (owns && span.part != NO_PARTS)
                    .then(|| take(&mut p, &mut p_at, span.part as usize, count));
                let mine_f = match owns {
                    true => take(&mut f, &mut f_at, span.table as usize, count),
                    false => take(&mut g, &mut g_at, span.table as usize, count),
                };
                jobs.push((span, table, mine_v, mine_i, mine_p, mine_f));
            }
            // Contiguous groups of about equal vertex counts, one per core.
            let vertices: usize = run.iter().map(|s| s.mesh.vertex_count()).sum();
            let per = vertices.div_ceil(threads).max(1);
            let mut groups: Vec<Vec<_>> = vec![Vec::new()];
            let mut filled = 0;
            for job in jobs {
                if filled >= per {
                    groups.push(Vec::new());
                    filled = 0;
                }
                filled += job.0.mesh.vertex_count();
                groups.last_mut().expect("one group").push(job);
            }
            std::thread::scope(|scope| {
                let threads: Vec<_> = (groups.into_iter())
                    .map(|group| {
                        scope.spawn(move || {
                            let mut lists = Vec::new();
                            for (span, table, v, out, p, f) in group {
                                let mesh = &*span.mesh;
                                // Compact meshes decode here, off the frame;
                                // a level sharing its vertices decodes them
                                // for spread_firsts alone.
                                let (mut own_v, mut own_n);
                                let v = match v {
                                    Some((v, n)) => {
                                        mesh.write_arena_vertices(v, n);
                                        &*v
                                    }
                                    None => {
                                        own_v =
                                            vec![super::ArenaVertex::zeroed(); mesh.vertex_count()];
                                        own_n = vec![0; mesh.vertex_count()];
                                        mesh.write_arena_vertices(&mut own_v, &mut own_n);
                                        &own_v[..]
                                    }
                                };
                                let mut i = vec![0; mesh.index_count()];
                                mesh.write_indices(&mut i, 0);
                                match (p, &mesh.data().thinning) {
                                    (Some(p), Some(t)) => p.copy_from_slice(&t.parts),
                                    (Some(p), None) => {
                                        for (p, s) in p.iter_mut().zip(&mesh.data().skin) {
                                            *p = skin_words(s);
                                        }
                                    }
                                    _ => {}
                                }
                                lists.push((table, spread_firsts(&mut i, v, 0, 0, f)));
                                for s in &span.streams {
                                    let at = (s.at - span.index) as usize;
                                    let to = &mut out[at..at + s.count as usize];
                                    let from = &i[s.from as usize..][..s.count as usize];
                                    for (o, &v) in to.iter_mut().zip(from) {
                                        *o = v - s.lo + s.virt;
                                    }
                                }
                            }
                            lists
                        })
                    })
                    .collect();
                threads
                    .into_iter()
                    .map(|t| t.join().expect("arena packing"))
                    .collect()
            })
        };
        // Lists go after every table; their places become the arena's.
        for (table, (list, owners)) in found.into_iter().flatten() {
            let at = vertex_count + shared + lists.len() as u32;
            for v in owners {
                match table + v {
                    e if e < vertex_count => chunk.tables[(e - start[0]) as usize] += at,
                    e => chunk.shared[(e - vertex_count - start[3]) as usize] += at,
                }
            }
            lists.extend(list);
        }
        if !send(chunk) {
            break;
        }
        start = end;
    }
    lists
}

impl Arena {
    /// Packs every draw's mesh, sharing meshes several draws use, for
    /// `Upload` to send. `None` when the textures do not fit `cap` slots.
    fn pack(gpu: &Gpu, cap: u32, placed: &Placed) -> Option<Upload> {
        // Slot 0 is white, for meshes without a texture.
        let mut views: Vec<Option<&Arc<StreamedTexture>>> = vec![None];
        let mut slot_of: HashMap<usize, u32> = HashMap::new();
        let mut packing = Packing::default();
        // Materials of levels sharing their full mesh's vertices, whose
        // tables follow every vertex's.
        let mut sharing = Vec::new();
        // Per material, as visible entries index them: a thinning level's
        // buckets fill the same one.
        let mut draws = vec![GpuDraw::zeroed(); placed.materials as usize];
        // Per material: the impostor's quad (zero for meshes), see `Shape`.
        let mut shapes = vec![Shape::zeroed(); placed.materials as usize];
        // The poses' records, where `Placed::stamp` named them, then the
        // clock and their frames.
        let clock = pose_shapes(&mut shapes, &placed.poses);
        let mut impostors = Vec::new();
        // Pieces sharing a mesh share its baked layer.
        let mut layer_of: HashMap<usize, u32> = HashMap::new();
        let mut mapped: Vec<(usize, &ResidentMesh)> = Vec::new();
        let mut splatted: Vec<(usize, &ResidentMesh)> = Vec::new();
        // Each mesh material's mesh, for what its textures can sample.
        let mut meshes: Vec<Option<&ResidentMesh>> = vec![None; placed.materials as usize];
        // The vertex stage tells draws apart by their index values alone
        // (`ARGS`), so draw slots sharing indices share one indirect draw
        // and its visible list (`lists`, `Placed::lists`): a group of
        // slots. Pieces drawing one mesh in one class are a group. In
        // another class the mesh goes through other pipelines, and decals
        // blend in their own piece's order, so those groups take a copy of
        // the mesh's indices (0.1% of the continental island's 12.3M);
        // impostors each a quad.
        let mut group_of: HashMap<(u32, u64), u32> = HashMap::new();
        // Per group: its first slot, and the copy of its mesh's indices it
        // draws (an impostor's: its quad).
        let mut groups: Vec<[u32; 2]> = Vec::new();
        let mut group_of_draw = Vec::with_capacity(placed.draws.len());
        let mut copies = vec![0u32; placed.materials as usize];
        let mut quads = 0;
        for (i, d) in placed.draws.iter().enumerate() {
            let levels = placed.resident[d.piece as usize].as_ref()?;
            let m = d.material as usize;
            // A decal piece's levels stay apart: one draw's rows blend in
            // row order (cull.wgsl cs_order_decals), not two draws' at once.
            let decal = !d.impostor && d.class >> 3 == 2;
            let class = match decal {
                true => 256 + u64::from(d.piece) * MAX_LEVELS as u64 + u64::from(d.level),
                false => u64::from(d.class),
            };
            let group = *group_of.entry((d.material, class)).or_insert_with(|| {
                let copy = if d.impostor {
                    quads += 1;
                    quads - 1
                } else {
                    copies[m] += 1;
                    let mesh = &levels[d.level as usize].source;
                    packing.copies.insert(key(mesh), copies[m]);
                    copies[m] - 1
                };
                groups.push([i as u32, copy]);
                groups.len() as u32 - 1
            });
            group_of_draw.push(group);
        }
        // The quads open the indices.
        packing.indices = 6 * quads;
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
                // Bakes read the source's indices through its material.
                let layer = *layer_of.entry(key(&mesh.source)).or_insert_with(|| {
                    impostors.push((d.material, 0..mesh.count));
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
                    alpha: 0,
                    opacity: 1.,
                    bias: 0.,
                    tiling: 0.,
                    edge: [0.; 4],
                    edge2: [0.; 2],
                    origin: [0.; 5],
                    step: [0.; 3],
                    index: 0,
                    firsts: 0,
                    payload: 0,
                };
                shapes[m] = Shape {
                    centre: [centre[0], centre[1], centre[2], radius.max(1e-3)],
                    half: [half[0], half[1], half[2], 0.],
                };
                continue;
            }
            let mesh = &levels[d.level as usize];
            let data = mesh.source.data();
            let decal = match &data.material {
                Material::Decal(d) => Some(*d),
                _ => None,
            };
            let edge = super::DecalEdge::code(decal.as_ref().and_then(|d| d.edge.as_ref()));
            // A decal's alpha map is its coverage: it takes a slot as albedo
            // does, never dropped as normal and surface maps may be.
            let [albedo, alpha] = [&mesh.albedo, &mesh.alpha].map(|t| {
                t.as_ref().map_or(0, |t| {
                    *slot_of.entry(key(t)).or_insert_with(|| {
                        views.push(Some(t));
                        views.len() as u32 - 1
                    })
                })
            });
            let ([index, vertex, parts, table], _) = packing.place(&mesh.source);
            if filled[m / 64] & 1 << (m % 64) != 0 {
                continue;
            }
            filled[m / 64] |= 1 << (m % 64);
            meshes[m] = Some(&**mesh);
            if mesh.maps.iter().any(Option::is_some) {
                mapped.push((m, &**mesh));
            }
            if mesh.layers.is_some() {
                splatted.push((m, &**mesh));
            }
            if mesh.source.shares().is_some() {
                sharing.push(m);
            }
            // A level sharing its full mesh's vertices shares its skin.
            let skinned = !mesh
                .source
                .shares()
                .unwrap_or(&mesh.source)
                .data()
                .skin
                .is_empty();
            draws[m] = GpuDraw {
                albedo,
                flags: u32::from(mesh.source.has_normals())
                    | u32::from(data.ignore_alpha) << 1
                    | u32::from(skinned) << 9
                    | (super::adapt_code(data.terrain_adapt) as u32) << 3
                    | u32::from(decal.is_some()) << 5
                    | u32::from(data.specular.0) << 12
                    | u32::from(data.family.0) << 16
                    | super::alpha_cut(data.alpha_ref) << 24,
                gain: data.color_gain,
                source: 0,
                maps: 0,
                tint: data.terrain_tint,
                vertex,
                parts,
                alpha,
                opacity: decal.map_or(1., |d| d.opacity),
                bias: decal.map_or(0., |d| d.lift()),
                tiling: super::tiling_code(data.world_tiling),
                edge: edge.0,
                edge2: [edge.1[0], edge.1[1]],
                origin: mesh.source.quant().origin,
                step: mesh.source.quant().steps(),
                index,
                firsts: table,
                payload: 0,
            };
        }
        for m in sharing {
            draws[m].firsts += packing.vertices;
        }
        // Each group's streams, valued from a range of their own, a page
        // (`PAGE_BITS`) at a time: a slot listing in the stream's draw
        // (`Placed::lists`), whose list holds the instances, and the
        // arena vertex the page's first value names. With thinning, each
        // bucket draws only its own triangles, whose placements the cull
        // lists in it and every bucket before (cull.wgsl).
        let mut pages: Vec<[u32; 2]> = Vec::new();
        let mut value = |draw: u32, vertex: u32, len: u32| -> Option<u32> {
            let page = 1 << PAGE_BITS;
            let virt = pages.len() << PAGE_BITS;
            // Values are 32 bits: a few million vertices use a thousandth.
            if virt + len as usize > 1 << 32 {
                eprintln!("arena index values exceed 32 bits; placements don't draw");
                return None;
            }
            pages.extend((0..len.div_ceil(page)).map(|k| [draw, vertex + k * page]));
            Some(virt as u32)
        };
        // Per group, where its indices start: its mesh's copy, or its quad.
        let mut starts = Vec::with_capacity(groups.len());
        let mut quad_values = vec![0; quads as usize];
        for &[first, copy] in &groups {
            let d = &placed.draws[first as usize];
            if d.impostor {
                quad_values[copy as usize] = value(first, 0, 6)?;
                starts.push(6 * copy);
                continue;
            }
            let mesh = &placed.resident[d.piece as usize].as_ref()?[d.level as usize].source;
            let ([index, vertex, ..], span) = packing.place(mesh);
            let n = mesh.index_count() as u32;
            let start = index + copy * n;
            starts.push(start);
            let streams = match (mesh.thin_counts(), mesh.thin_vertices()) {
                (Some(ends), Some(reach)) => (0..THIN_BUCKETS as usize)
                    .map(|b| {
                        let from = if b == 0 { 0 } else { ends[b - 1] };
                        let [lo, hi] = reach[b];
                        let virt = value(first + b as u32, vertex + lo, hi - lo)?;
                        Some(Stream {
                            at: start + from,
                            from,
                            count: ends[b] - from,
                            lo,
                            virt,
                        })
                    })
                    .collect::<Option<Vec<_>>>()?,
                _ => {
                    let reach = mesh.shares().unwrap_or(mesh).vertex_count() as u32;
                    vec![Stream {
                        at: start,
                        from: 0,
                        count: n,
                        lo: 0,
                        virt: value(first, vertex, reach)?,
                    }]
                }
            };
            packing.spans[span].streams.extend(streams);
        }
        // The arena's draws in the groups' order, which follows the slots':
        // each class's draws stay contiguous, as its slots are. Thinning
        // groups draw a bucket's own triangles, and drop the buckets that
        // have none.
        let mut lists: Vec<[u32; 2]> = Vec::with_capacity(groups.len());
        // Per list, the first slot listing in it, which maps ranges of
        // slots to ranges of lists.
        let mut opening: Vec<u32> = Vec::with_capacity(groups.len());
        // Per group, its first list.
        let mut group_lists = Vec::with_capacity(groups.len());
        for (&[first, _], &start) in groups.iter().zip(&starts) {
            group_lists.push(lists.len() as u32);
            let d = &placed.draws[first as usize];
            if d.impostor {
                lists.push([start, 6]);
                opening.push(first);
                continue;
            }
            let mesh = &placed.resident[d.piece as usize].as_ref()?[d.level as usize].source;
            match mesh.thin_counts() {
                Some(ends) => {
                    for (b, &end) in (0..).zip(ends) {
                        let from = if b == 0 { 0 } else { ends[b as usize - 1] };
                        if end > from {
                            lists.push([start + from, end - from]);
                            opening.push(first + b);
                        }
                    }
                }
                None => {
                    lists.push([start, mesh.index_count() as u32]);
                    opening.push(first);
                }
            }
        }
        let list_of: Vec<u32> = (placed.draws.iter())
            .zip(&group_of_draw)
            .map(|(d, &g)| {
                let at = group_lists[g as usize];
                if d.impostor {
                    return at;
                }
                let levels = placed.resident[d.piece as usize].as_ref();
                match levels.and_then(|l| l[d.level as usize].source.thin_counts()) {
                    // The bucket's place among its group's buckets that drew.
                    Some(ends) => {
                        let b = d.bucket as usize;
                        let from = |b: usize| if b == 0 { 0 } else { ends[b - 1] };
                        if ends[b] == from(b) {
                            return NO_LIST;
                        }
                        at + (0..b).filter(|&k| ends[k] > from(k)).count() as u32
                    }
                    None => at,
                }
            })
            .collect();
        let at = |slot: u32| opening.partition_point(|&f| f < slot) as u32;
        let ranges = placed.ranges.map(|r| at(r.start)..at(r.end));
        let Packing {
            spans,
            vertices: vertex_count,
            indices: index_total,
            parts: part_count,
            shared,
            ..
        } = packing;
        // Filled on a thread, a run of meshes at a time for `Upload::step`
        // to take: 9M vertices took a 56 ms frame on all cores. A run waits
        // there while the last is taken.
        let (runs, taken) = std::sync::mpsc::sync_channel(1);
        let packing = std::thread::spawn(move || {
            let counts = [vertex_count, index_total, part_count, shared];
            pack(&spans, counts, &quad_values, |run| runs.send(run).is_ok())
        });
        // Normal and surface maps take the slots albedo leaves: without
        // them pieces still draw in one go, only flatter and without their
        // glow.
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
                        views.push(Some(t));
                        views.len() as u32 - 1
                    });
                }
                draws[d].maps = slots[0] | slots[1] << 16;
                // Only with a normal map slot: slot 0 is white, which would
                // light the whole mesh. A host family's own words take the
                // payload.
                let data = mesh.source.data();
                let Some([r, g, b]) = data.emissive.filter(|_| slots[0] != 0) else {
                    continue;
                };
                if !data.payload.is_empty() {
                    continue;
                }
                draws[d].flags |= 1 << 8;
                draws[d].payload = shapes.len() as u32;
                let glow = GpuEmissive {
                    color: [r, g, b, 0.],
                    ..GpuEmissive::zeroed()
                };
                shapes.push(bytemuck::cast(glow));
            }
        } else if !mapped.is_empty() {
            eprintln!(
                "{} placement textures with normal and surface maps exceed the {cap} the device binds; drawing without them",
                views.len() + extra.len()
            );
        }
        // Host families' payloads, eight words a shape, after a shape of
        // their slots where they bring some, which take the texture slots
        // left as maps do: without, the family samples white.
        let extra: HashSet<usize> = (meshes.iter().flatten())
            .flat_map(|m| m.slots.iter().map(key))
            .filter(|k| !slot_of.contains_key(k))
            .collect();
        let slotted = views.len() + extra.len() <= cap as usize;
        if !slotted {
            eprintln!(
                "{} placement textures with family slots exceed the {cap} the device binds; drawing without them",
                views.len() + extra.len()
            );
        }
        for (m, mesh) in meshes.iter().enumerate() {
            let Some(mesh) = mesh else { continue };
            let words = &mesh.source.data().payload;
            if words.is_empty() && mesh.slots.is_empty() {
                continue;
            }
            draws[m].payload = shapes.len() as u32;
            if !mesh.slots.is_empty() {
                let mut shape = [0u32; 8];
                for (k, t) in mesh.slots.iter().enumerate().filter(|_| slotted) {
                    let slot = *slot_of.entry(key(t)).or_insert_with(|| {
                        views.push(Some(t));
                        views.len() as u32 - 1
                    });
                    shape[k / 2] |= slot << (16 * (k % 2));
                }
                draws[m].flags |= SLOTS_FLAG;
                shapes.push(bytemuck::cast(shape));
            }
            for chunk in words.chunks(8) {
                let mut shape = [0u32; 8];
                shape[..chunk.len()].copy_from_slice(chunk);
                shapes.push(bytemuck::cast(shape));
            }
        }
        // Splats take what slots are left after those, and their records
        // go after every material's shape.
        let layers = |m: &ResidentMesh| -> Vec<Arc<StreamedTexture>> {
            let l = m.layers.as_ref().expect("splatted mesh");
            std::iter::once(&l.control)
                .chain(l.albedo.iter().flatten())
                .chain(l.normal.iter().flatten())
                .cloned()
                .collect()
        };
        let extra: HashSet<usize> = (splatted.iter())
            .flat_map(|(_, m)| layers(m))
            .map(|t| key(&t))
            .filter(|k| !slot_of.contains_key(k))
            .collect();
        if views.len() + extra.len() <= cap as usize {
            for &(d, mesh) in &splatted {
                let l = mesh.layers.as_ref().expect("splatted mesh");
                let source = mesh.source.data().splat.as_ref().expect("splatted mesh");
                // The slot `t` has, given one if it has none yet.
                fn slot<'a>(
                    t: &'a Arc<StreamedTexture>,
                    views: &mut Vec<Option<&'a Arc<StreamedTexture>>>,
                    slot_of: &mut HashMap<usize, u32>,
                ) -> u32 {
                    *slot_of.entry(key(t)).or_insert_with(|| {
                        views.push(Some(t));
                        views.len() as u32 - 1
                    })
                }
                let mut g = GpuSplat {
                    control: slot(&l.control, &mut views, &mut slot_of),
                    flags: u32::from(source.tint.is_some())
                        | u32::from(source.mix_normals) << 1
                        | u32::from(source.scale_invariant) << 2
                        | u32::from(source.world_tiling) << 3,
                    sharpness: source.sharpness,
                    alpha_floor: source.alpha_floor,
                    fallback: source.fallback,
                    tint: source.tint.unwrap_or(0.),
                    ..GpuSplat::zeroed()
                };
                for k in 0..4 {
                    let (Some(albedo), Some(layer)) = (&l.albedo[k], &source.layers[k]) else {
                        continue;
                    };
                    g.flags |= 1 << (8 + k);
                    g.tiling[k] = layer.tiling;
                    g.albedo[k / 2] |= slot(albedo, &mut views, &mut slot_of) << (16 * (k % 2));
                    if let Some(normal) = &l.normal[k] {
                        g.normal[k / 2] |= slot(normal, &mut views, &mut slot_of) << (16 * (k % 2));
                    }
                }
                draws[d].source = shapes.len() as u32;
                shapes.extend_from_slice(bytemuck::cast_slice(&[g]));
            }
        } else if !splatted.is_empty() {
            eprintln!(
                "{} placement textures with splat layers exceed the {cap} the device binds; drawing without them",
                views.len() + extra.len()
            );
        }
        if views.len() > cap as usize {
            eprintln!(
                "{} placement textures exceed the {cap} the device binds; placements don't draw",
                views.len()
            );
            return None;
        }
        // Two texels of the finest level, in each impostor's model units.
        let tile = impostor_tile(impostors.len() as u32) as f32;
        for (d, shape) in draws.iter().zip(&mut shapes) {
            if d.flags & 4 != 0 {
                shape.half[3] = 2. * (2. * shape.centre[3] / tile);
            }
        }
        let (texels, floors) = texel_table(
            &draws,
            &meshes,
            &views,
            &shapes,
            impostors.len(),
            &placed.families,
        );
        // Indices are an index buffer for drawing and storage for baking.
        let usage =
            wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::INDEX | wgpu::BufferUsages::COPY_DST;
        let tables = vertex_count + shared;
        let sizes = [
            std::mem::size_of_val(draws.as_slice()),
            vertex_count as usize * std::mem::size_of::<super::ArenaVertex>(),
            vertex_count as usize * 4,
            index_total as usize * 4,
            std::mem::size_of_val(shapes.as_slice()),
            part_count.max(1) as usize * 16,
            // The tables alone: made again with the lists after them once
            // packing ends (`step`), whose length is not known before.
            tables as usize * 4,
            std::mem::size_of_val(pages.as_slice()),
        ];
        let buffers = std::array::from_fn(|k| {
            gpu.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some(LABELS[k]),
                // Bindings want at least 16 bytes.
                size: (sizes[k] as u64).max(16),
                usage: if k == 6 {
                    usage | wgpu::BufferUsages::COPY_SRC
                } else {
                    usage
                },
                mapped_at_creation: false,
            })
        });
        Some(Upload {
            buffers,
            clock,
            head: Some((draws, shapes, pages)),
            runs: Some(taken),
            packing: Some(packing),
            textures: views.into_iter().map(|t| t.cloned()).collect(),
            texels,
            floors,
            cap,
            lists,
            list_of,
            ranges,
            tables,
            impostors,
        })
    }
}

/// A skinned vertex's joints and weights as four exact floats, two bytes
/// to each (placed.wgsl `pose_turn`): f32 holds 16 bits exactly, where a
/// bit cast could meet a driver that flushes denormals.
fn skin_words(s: &[u8; 8]) -> [f32; 4] {
    let pair = |a: u8, b: u8| f32::from(u16::from(a) | u16::from(b) << 8);
    [
        pair(s[0], s[1]),
        pair(s[2], s[3]),
        pair(s[4], s[5]),
        pair(s[6], s[7]),
    ]
}

/// Appends the poses (placed.wgsl `pose_turn`) to `shapes`, which holds
/// one shape per material: a record per pose, the clock, then the frames,
/// three vec4s a matrix, two vec4s a shape. Returns the clock's byte
/// offset, `None` without poses.
fn pose_shapes(shapes: &mut Vec<Shape>, poses: &[Arc<Pose>]) -> Option<u64> {
    if poses.is_empty() {
        return None;
    }
    let records = shapes.len();
    let clock = records + poses.len();
    shapes.resize(clock + 1, Shape::zeroed());
    let mut rows: Vec<[f32; 4]> = Vec::new();
    for (k, pose) in poses.iter().enumerate() {
        let frames = pose.frames.len() / pose.joints as usize;
        let loops = 1. / pose.period;
        shapes[records + k] = Shape {
            centre: [
                (2 * (clock + 1) + rows.len()) as f32,
                frames as f32,
                pose.joints as f32,
                if pose.phased { -loops } else { loops },
            ],
            half: [
                pose.origin[0],
                pose.origin[1],
                pose.origin[2],
                (2 * clock) as f32,
            ],
        };
        rows.extend(pose.frames.iter().flatten());
    }
    if rows.len() % 2 == 1 {
        rows.push([0.; 4]);
    }
    shapes.extend(
        rows.as_chunks::<2>()
            .0
            .iter()
            .map(|&[centre, half]| Shape { centre, half }),
    );
    Some((clock * std::mem::size_of::<Shape>()) as u64)
}

/// The arena's buffers, as `Arena::group` binds them.
const LABELS: [&str; 8] = [
    "arena draws",
    "arena vertices",
    "arena normals",
    "arena indices",
    "arena shapes",
    "arena parts",
    "arena firsts",
    "arena pages",
];

/// An arena packed on the CPU, its buffers filling a share of each frame:
/// the continental island's 390 MB in one go took a 100 ms frame. Nothing
/// draws from it until every byte is in.
pub(super) struct Upload {
    buffers: [wgpu::Buffer; 8],
    /// Where the poses' clock sits in the shapes buffer (`pose_shapes`).
    clock: Option<u64>,
    /// Draws, shapes and pages, written first.
    head: Option<(Vec<GpuDraw>, Vec<Shape>, Vec<[u32; 2]>)>,
    /// Runs of meshes as packing fills them (`pack`), until it ends.
    runs: Option<std::sync::mpsc::Receiver<Chunk>>,
    /// The packing thread, which ends with the firsts lists.
    packing: Option<std::thread::JoinHandle<Vec<u32>>>,
    textures: Vec<Option<Arc<StreamedTexture>>>,
    texels: Vec<GpuTexels>,
    floors: Vec<u8>,
    cap: u32,
    lists: Vec<[u32; 2]>,
    list_of: Vec<u32>,
    ranges: Ranges,
    tables: u32,
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
        let mut write = |buffer: &wgpu::Buffer, at: usize, bytes: &[u8], room: &mut usize| {
            if !bytes.is_empty() {
                gpu.queue.write_buffer(buffer, at as u64, bytes);
                *room = room.saturating_sub(bytes.len());
                budget.charge(bytes.len());
            }
        };
        let b = &self.buffers;
        if let Some((draws, shapes, pages)) = self.head.take() {
            write(&b[0], 0, bytemuck::cast_slice(&draws), &mut room);
            write(&b[4], 0, bytemuck::cast_slice(&shapes), &mut room);
            write(&b[7], 0, bytemuck::cast_slice(&pages), &mut room);
        }
        // Each run goes once written; the next waits for a frame with room.
        while let Some(runs) = &self.runs
            && room > 0
        {
            use std::sync::mpsc::TryRecvError;
            let run = match runs.try_recv() {
                Ok(run) => run,
                Err(TryRecvError::Empty) => return false,
                Err(TryRecvError::Disconnected) => {
                    self.runs = None;
                    break;
                }
            };
            let [vertex, index, part, shared] = run.at.map(|a| a as usize);
            let size = std::mem::size_of::<super::ArenaVertex>();
            write(
                &b[1],
                vertex * size,
                bytemuck::cast_slice(&run.vertices),
                &mut room,
            );
            write(
                &b[2],
                vertex * 4,
                bytemuck::cast_slice(&run.normals),
                &mut room,
            );
            write(
                &b[3],
                index * 4,
                bytemuck::cast_slice(&run.indices),
                &mut room,
            );
            write(
                &b[5],
                part * 16,
                bytemuck::cast_slice(&run.parts),
                &mut room,
            );
            write(
                &b[6],
                vertex * 4,
                bytemuck::cast_slice(&run.tables),
                &mut room,
            );
            write(
                &b[6],
                shared * 4,
                bytemuck::cast_slice(&run.shared),
                &mut room,
            );
        }
        if self.runs.is_some() {
            return false;
        }
        if let Some(packing) = self.packing.take() {
            let lists = packing.join().expect("arena packing");
            // The lists follow the tables, in a buffer made at the length
            // packing found, the tables copied across on the GPU.
            if !lists.is_empty() {
                let tables = u64::from(self.tables) * 4;
                let firsts = gpu.device.create_buffer(&wgpu::BufferDescriptor {
                    label: Some(LABELS[6]),
                    size: tables + std::mem::size_of_val(lists.as_slice()) as u64,
                    usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
                    mapped_at_creation: false,
                });
                let mut encoder = gpu.device.create_command_encoder(&Default::default());
                encoder.copy_buffer_to_buffer(&self.buffers[6], 0, &firsts, 0, tables);
                gpu.queue.submit([encoder.finish()]);
                write(
                    &firsts,
                    tables as usize,
                    bytemuck::cast_slice(&lists),
                    &mut room,
                );
                self.buffers[6] = firsts;
            }
        }
        true
    }

    fn finish(self, gpu: &Gpu, kit: &Kit, layout: &wgpu::BindGroupLayout) -> Arena {
        let texels = buffer_init(
            gpu,
            "arena texels",
            bytemuck::cast_slice(&self.texels),
            wgpu::BufferUsages::STORAGE,
        );
        let group = Arena::group(gpu, kit, layout, &self.buffers, &self.textures, self.cap);
        Arena {
            id: ARENAS.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            group,
            indices: self.buffers[3].clone(),
            lists: self.lists,
            list_of: self.list_of,
            ranges: self.ranges,
            tables: self.tables,
            impostors: self.impostors,
            textures: self.textures,
            floors: self.floors,
            texels,
            buffers: self.buffers,
            clock: self.clock,
            cap: self.cap,
        }
    }
}

impl Arena {
    /// Sets the seconds posed placements play at (placed.wgsl `pose_turn`).
    pub fn set_clock(&self, gpu: &Gpu, seconds: f32) {
        if let Some(at) = self.clock {
            gpu.queue.write_buffer(
                &self.buffers[4],
                at,
                bytemuck::bytes_of(&[seconds, 0., 0., 0.]),
            );
        }
    }

    /// Binds the arena's buffers and each slot's texture as it is now,
    /// the array padded with white to `cap`.
    fn group(
        gpu: &Gpu,
        kit: &Kit,
        layout: &wgpu::BindGroupLayout,
        b: &[wgpu::Buffer; 8],
        textures: &[Option<Arc<StreamedTexture>>],
        cap: u32,
    ) -> wgpu::BindGroup {
        let owned: Vec<Option<wgpu::TextureView>> = textures
            .iter()
            .map(|t| t.as_ref().map(|t| t.view()))
            .collect();
        let white = &kit.dummy.white;
        let views: Vec<&wgpu::TextureView> = (owned.iter())
            .map(|v| v.as_ref().unwrap_or(white))
            .chain(std::iter::repeat(white))
            .take(cap as usize)
            .collect();
        let entries = [
            entry(0, b[0].as_entire_binding()),
            entry(1, b[1].as_entire_binding()),
            entry(2, b[3].as_entire_binding()),
            entry(3, wgpu::BindingResource::TextureViewArray(&views)),
            entry(4, b[4].as_entire_binding()),
            entry(5, b[5].as_entire_binding()),
            entry(7, b[2].as_entire_binding()),
            entry(8, b[7].as_entire_binding()),
            entry(6, b[6].as_entire_binding()),
        ];
        gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("placement arena"),
            layout,
            entries: &entries,
        })
    }

    /// Binds the slots' textures anew: residency made some again.
    pub fn bind(&mut self, gpu: &Gpu, kit: &Kit) {
        let Some((layout, _)) = &kit.arena else {
            return;
        };
        self.group = Self::group(gpu, kit, layout, &self.buffers, &self.textures, self.cap);
    }
}

/// Per material what the cull asks residency for (`GpuTexels`), and per
/// slot the want impostor bakes put on it (`Arena::floors`). `meshes`
/// holds each mesh material's mesh, `shapes` each impostor material's
/// quad, `layers` the atlas's layer count.
fn texel_table(
    draws: &[GpuDraw],
    meshes: &[Option<&ResidentMesh>],
    textures: &[Option<&Arc<StreamedTexture>>],
    shapes: &[Shape],
    layers: usize,
    families: &Families,
) -> (Vec<GpuTexels>, Vec<u8>) {
    const FULL: f32 = -1e30;
    let mut table = vec![GpuTexels::zeroed(); draws.len()];
    for ((t, d), mesh) in table.iter_mut().zip(draws).zip(meshes) {
        let Some(mesh) = mesh else { continue };
        let data = mesh.source.data();
        let slots = [d.albedo, d.maps & 0xffff, d.maps >> 16, d.alpha];
        t.slots = slots;
        let cut = super::casts_shadow(data) && !data.ignore_alpha;
        t.cutout[0] = if cut { d.albedo } else { 0 };
        t.cutout[1] = match data.terrain_adapt {
            None => 0,
            Some(super::TerrainAdapt::Drape) => 2,
            Some(_) => 1,
        };
        // Thinning's growth and bending the cull weighs per placement.
        let density = mesh.source.uv_density();
        // UVs from the world, or a projected decal's alpha on its box:
        // spans this mesh's UVs say nothing about.
        let world = super::tiling_code(data.world_tiling) != 0.;
        let projected = matches!(&data.material, Material::Decal(d) if d.edge.is_some());
        let bias = |slot: u32, repeat: f32| {
            let side = (textures.get(slot as usize).copied().flatten())
                .map_or(0, |t| t.layout.width.min(t.layout.height));
            (density * repeat * side as f32).log2()
        };
        for k in 0..4 {
            let b = bias(slots[k], 1.);
            let pinned = world || (k == 3 && projected) || density <= 0. || !b.is_finite();
            t.bias[k] = if pinned { FULL } else { b };
        }
        // A family's slots, from the payload's first shape, each repeating
        // as often as the mesh says it samples it.
        if d.flags & SLOTS_FLAG != 0 {
            let words: [u32; 8] = bytemuck::cast(shapes[d.payload as usize]);
            for k in 0..MAX_SLOTS {
                let slot = (words[k / 2] >> (16 * (k % 2))) & 0xffff;
                if slot == 0 {
                    continue;
                }
                let repeat = data.slot_repeats.get(k).map_or(1., |r| r.abs().max(1e-3));
                let b = bias(slot, repeat);
                let pinned = world || density <= 0. || !b.is_finite();
                t.extra[k] = slot;
                t.extra_bias[k] = if pinned { FULL } else { b };
            }
            t.cutout[2] = u32::from(families.coverage_slots(data.family));
        }
    }
    // A bake draws each impostor's source mesh a tile across two radii.
    let tile = impostor_tile(layers as u32) as f32;
    let mut floors = vec![0u8; textures.len()];
    for (d, shape) in draws.iter().zip(shapes) {
        if d.flags & 4 == 0 {
            continue;
        }
        let source = table[d.source as usize];
        let ppm = tile / (2. * shape.centre[3].max(1e-3));
        for k in 0..4 {
            let slot = source.slots[k] as usize;
            if slot == 0 {
                continue;
            }
            let level = (source.bias[k] - ppm.log2() - TEXEL_MARGIN)
                .floor()
                .clamp(0., 31.);
            floors[slot] = floors[slot].max(32 - level as u8);
        }
    }
    (table, floors)
}

/// An impostor's quad, per material (placed.wgsl `Shape`): the model-space
/// centre of its mesh's box and the radius each view's tile spans; the
/// box's half extents, which bound what a view can show (`impostor_extent`),
/// and two texels of the atlas's finest level in model units.
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct Shape {
    centre: [f32; 4],
    half: [f32; 4],
}

/// One viewport's cull outputs: indirect arguments and visible rows for the
/// view's two culling phases and for the shadow map, and which placements
/// the view showed last frame.
pub(super) struct CullTargets {
    pub capacity: u32,
    /// Placement rows the per-row arrays (`keep`, `shadow_keep`, `seen`)
    /// hold: one per placement, where every draw's visible list reserves
    /// its piece's rows (sixteen times over for a thinning level's buckets).
    pub rows: u32,
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
    /// Per section and arena texture slot, what the culls ask residency
    /// for (cull.wgsl `c_wants`); which sections this frame's culls
    /// wrote, and for which arena.
    pub wants: wgpu::Buffer,
    pub written: u32,
    pub wants_arena: u64,
}

impl CullTargets {
    pub fn new(gpu: &Gpu, capacity: u32, rows: u32, draws: u32) -> Self {
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
        let list = u64::from(capacity) * 4;
        let per_row = u64::from(rows) * 4;
        let args_usage = U::INDIRECT | U::STORAGE | U::COPY_DST | U::COPY_SRC;
        Self {
            capacity,
            rows,
            draws,
            args: buffer("editor cull args", args, args_usage),
            visible: buffer("editor cull visible", list, U::STORAGE),
            revealed_args: buffer("editor revealed args", args, args_usage),
            revealed_visible: buffer("editor revealed visible", list, U::STORAGE),
            shadow_args: buffer("editor shadow args", args, args_usage),
            shadow_visible: buffer("editor shadow visible", list, U::STORAGE),
            keep: buffer("editor thinning", per_row, U::STORAGE),
            shadow_keep: buffer("editor shadow thinning", per_row, U::STORAGE),
            // Zero is "unseen".
            seen: buffer("editor seen", u64::from(rows.div_ceil(32)) * 4, U::STORAGE),
            template: buffer("editor cull template", args, U::COPY_SRC | U::COPY_DST),
            template_version: None,
            totals: buffer(
                "editor cull totals",
                20,
                U::STORAGE | U::COPY_SRC | U::COPY_DST,
            ),
            wants: buffer(
                "editor texel wants",
                u64::from(super::residency::SECTIONS * super::residency::SLOTS) * 4,
                U::STORAGE | U::COPY_SRC | U::COPY_DST,
            ),
            written: 0,
            wants_arena: 0,
        }
    }
}
