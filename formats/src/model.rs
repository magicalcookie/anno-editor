// Model configs (`.cfg`): which rdm meshes make up a graphics object, their
// materials and textures, and where sub-models, nested prop cfgs, decals and
// particles sit. Reads 1800's and 117's cfgs and prps, and 2070's
// `<m_Config>` dialect.
//
// Cfg is Anno's XML dialect: no attributes or declaration, tag names with
// spaces and brackets (`<Common [ModelCommon]>`), `</>` closers in some
// tools' output, and the odd garbage byte in unused strings. Parsed into a
// small tree; files are at most a few hundred KB.
use anyhow::{Context, Result};

use crate::rda::normalize_path;
use crate::rdm::Rdm;
use crate::vfs::Install;
use crate::xml::{Doc, Id};

/// Local placement of a part relative to the cfg's origin. Y up, metres.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Transform {
    pub position: [f32; 3],
    /// Unit quaternion `[x, y, z, w]`.
    pub rotation: [f32; 4],
    /// `Scale` times `NonUniformScale`.
    pub scale: [f32; 3],
}

impl Default for Transform {
    fn default() -> Self {
        Transform::IDENTITY
    }
}

impl Transform {
    pub const IDENTITY: Transform = Transform {
        position: [0.0; 3],
        rotation: [0.0, 0.0, 0.0, 1.0],
        scale: [1.0; 3],
    };

    /// Scale, then rotate, then translate.
    pub fn apply(&self, p: [f32; 3]) -> [f32; 3] {
        let s = [
            p[0] * self.scale[0],
            p[1] * self.scale[1],
            p[2] * self.scale[2],
        ];
        let r = rotate(self.rotation, s);
        [
            r[0] + self.position[0],
            r[1] + self.position[1],
            r[2] + self.position[2],
        ]
    }

    /// Column-major 4x4 matrix of [`Transform::apply`].
    pub fn matrix(&self) -> [f32; 16] {
        let x = rotate(self.rotation, [self.scale[0], 0.0, 0.0]);
        let y = rotate(self.rotation, [0.0, self.scale[1], 0.0]);
        let z = rotate(self.rotation, [0.0, 0.0, self.scale[2]]);
        let t = self.position;
        [
            x[0], x[1], x[2], 0.0, y[0], y[1], y[2], 0.0, z[0], z[1], z[2], 0.0, t[0], t[1], t[2],
            1.0,
        ]
    }

    /// Back from [`Transform::matrix`]: scale from the basis lengths,
    /// rotation from the basis divided by them. A sheared matrix comes back
    /// as its nearest scale and rotation.
    pub fn from_matrix(m: &[f32; 16]) -> Transform {
        let column = |c: usize| [m[c * 4], m[c * 4 + 1], m[c * 4 + 2]];
        let length = |v: [f32; 3]| (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt();
        let scale = [0, 1, 2].map(|c| length(column(c)));
        let r = |row: usize, col: usize| m[col * 4 + row] / scale[col].max(1e-12);
        // Shepperd: from the largest of the trace and the diagonal.
        let trace = r(0, 0) + r(1, 1) + r(2, 2);
        let q = if trace > 0. {
            let s = (trace + 1.).sqrt() * 2.;
            [
                (r(2, 1) - r(1, 2)) / s,
                (r(0, 2) - r(2, 0)) / s,
                (r(1, 0) - r(0, 1)) / s,
                s / 4.,
            ]
        } else if r(0, 0) > r(1, 1) && r(0, 0) > r(2, 2) {
            let s = (1. + r(0, 0) - r(1, 1) - r(2, 2)).sqrt() * 2.;
            [
                s / 4.,
                (r(0, 1) + r(1, 0)) / s,
                (r(0, 2) + r(2, 0)) / s,
                (r(2, 1) - r(1, 2)) / s,
            ]
        } else if r(1, 1) > r(2, 2) {
            let s = (1. + r(1, 1) - r(0, 0) - r(2, 2)).sqrt() * 2.;
            [
                (r(0, 1) + r(1, 0)) / s,
                s / 4.,
                (r(1, 2) + r(2, 1)) / s,
                (r(0, 2) - r(2, 0)) / s,
            ]
        } else {
            let s = (1. + r(2, 2) - r(0, 0) - r(1, 1)).sqrt() * 2.;
            [
                (r(0, 2) + r(2, 0)) / s,
                (r(1, 2) + r(2, 1)) / s,
                s / 4.,
                (r(1, 0) - r(0, 1)) / s,
            ]
        };
        let n = q.iter().map(|v| v * v).sum::<f32>().sqrt().max(1e-12);
        // w ≥ 0, so an unturned entry writes the identity it read.
        let sign = if q[3] < 0. { -1. } else { 1. };
        Transform {
            position: [m[12], m[13], m[14]],
            rotation: q.map(|v| v / n * sign),
            scale,
        }
    }

    /// `self` applied after `inner`. Exact for uniform scale; non-uniform
    /// scale under rotation is not representable and is approximated.
    pub fn then(&self, inner: &Transform) -> Transform {
        Transform {
            position: self.apply(inner.position),
            rotation: quat_mul(self.rotation, inner.rotation),
            scale: [0, 1, 2].map(|k| self.scale[k] * inner.scale[k]),
        }
    }
}

fn rotate(q: [f32; 4], v: [f32; 3]) -> [f32; 3] {
    // v + 2w(u x v) + 2(u x (u x v)), u = q.xyz
    let u = [q[0], q[1], q[2]];
    let cross = |a: [f32; 3], b: [f32; 3]| {
        [
            a[1] * b[2] - a[2] * b[1],
            a[2] * b[0] - a[0] * b[2],
            a[0] * b[1] - a[1] * b[0],
        ]
    };
    let t = cross(u, v).map(|c| 2.0 * c);
    let c = cross(u, t);
    [0, 1, 2].map(|k| v[k] + q[3] * t[k] + c[k])
}

fn quat_mul(a: [f32; 4], b: [f32; 4]) -> [f32; 4] {
    [
        a[3] * b[0] + a[0] * b[3] + a[1] * b[2] - a[2] * b[1],
        a[3] * b[1] - a[0] * b[2] + a[1] * b[3] + a[2] * b[0],
        a[3] * b[2] + a[0] * b[1] - a[1] * b[0] + a[2] * b[3],
        a[3] * b[3] - a[0] * b[0] - a[1] * b[1] - a[2] * b[2],
    ]
}

/// How a material composites, decided by its shader (see the doc for the
/// ShaderID table).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Alpha {
    Opaque,
    /// Alpha test against the diffuse alpha at this reference
    /// (`cShadowAlphaRef`). Plant shaders 21, 22, 23.
    Cutout(f32),
    /// A solid surface the game still alpha-tests: 117's PBR shaders that
    /// define `USE_ALPHA_REF` (8, 14, 20, 24) clip `smoothstep(0, 0.9,
    /// alpha)` below this reference in their depth prepass
    /// (`model_common.fxh` psBaseDepthOnly), and 1800's model_pbr (8) and
    /// model_icon (14) clip too. Most of their textures are opaque; flat
    /// cards such as Albion's marsh grass decals and hanging ropes keep
    /// their shape only in alpha.
    Tested(f32),
    /// Alpha blended at `cOpacity` times texture alpha: hair (12) and the
    /// opacity shaders (19).
    Blend(f32),
    /// Projected onto what is below (mesh decals 1, 6, 26; terrain decal 2).
    Decal,
    /// Not drawn: terrain and water cutout volumes (4, 5).
    Hidden,
}

/// One `<Materials>` entry. Index `i` matches [`crate::rdm::MaterialGroup::material`].
#[derive(Debug, Clone, PartialEq)]
pub struct Material {
    name: String,
    pub shader: Shader,
    /// The format the material expects, e.g. `P4h_N4b_G4b_B4b_T2h_C4b`.
    pub vertex_format: String,
    /// Shipped `.dds` path of `cModelDiffTex`, `None` when `DIFFUSE_ENABLED`
    /// is 0 or the cfg has none.
    pub diffuse: Option<String>,
    /// `cModelNormalTex`, `None` when `NORMAL_ENABLED` is 0.
    pub normal: Option<String>,
    /// `cModelMetallicTex`, `None` when `METALLIC_TEX_ENABLED` is 0.
    pub metallic: Option<String>,
    /// `cDiffuseColor`; multiplies the diffuse texture, values above 1 occur.
    pub diffuse_color: [f32; 3],
    pub double_sided: bool,
    /// `cTerrainTintIntensity` when `cUseTerrainTinting` is 1, else 0: how
    /// much the ground's tint map multiplies the albedo in game.
    pub terrain_tint: f32,
    pub alpha: Alpha,
    /// `ADJUST_TO_TERRAIN_HEIGHT` (or the cutout shader's
    /// `cAdjustToTerrainHeight`) and its options; `None` when off.
    pub terrain_adapt: Option<TerrainAdapt>,
    /// A mesh blended over what is under it (`model_decal.fx`, ShaderID 6,
    /// or a prp of type `Decal`): paving, dirt, stains. Its `alpha` is
    /// [`Alpha::Decal`].
    pub mesh_decal: Option<MeshDecal>,
    /// `cEnableWorldSpaceTiling`: textures take their UVs from world X/Z
    /// over `cWorldSpaceTilingScale` metres, along the diagonals with
    /// `cEnableAQTiling` (`GetWorldSpaceTilingTexCoord`). An alpha map
    /// keeps the mesh's UVs.
    pub world_tiling: Option<WorldTiling>,
    /// `cDyeMask` when `DYE_MASK_ENABLED` is 1: its red is where the
    /// owner's colour shows (`model_common.fxh`). `None` too under
    /// `MULTI_DYE_ENABLED`, which takes per-instance colours from a palette
    /// texture instead, and for the 189 (117) and 393 (1800) shipped
    /// materials that switch the mask on without naming one.
    pub dye_mask: Option<String>,
    /// `GLOW_ENABLED`: light the material gives off; see [`Glow`].
    pub glow: Option<Glow>,
    /// `SPLAT_MAP_ENABLED` (ShaderID 24, `model_terrain.fx`): tiling
    /// textures blended by a control map's channels make the albedo,
    /// normal and gloss (`model_common.fxh` GetSplatMapShading).
    pub splat: Option<Splat>,
    /// The specular terms its shader keeps.
    pub specular: Specular,
}

/// Which specular terms a material's shader keeps, by the `BRDF_DISABLE_*`
/// defines Anno's shaders set. With all on, dark leaves and soil seen at a
/// grazing angle turned sky grey.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Specular {
    #[default]
    Full,
    /// `BRDF_DISABLE_SPEC_ON_ZERO_GLOSS`: the sun's highlight and the sky's
    /// reflection fade out as gloss falls from 0.25 to 0. 117's plants
    /// (shaders 21-23, tagged GBUFFER_MATERIAL_ID_PLANT, which
    /// deferred_shading.fx gives the flag; their own
    /// `BRDF_DISABLE_ENVMAP_REFLECTION` is dead under deferred shading),
    /// 1800's `Terrain` and `Decal Detail` prps.
    MatteFades,
    /// prop_plant.fx (1800 `Plant` prps): no sky reflection
    /// (`BRDF_DISABLE_ENVMAP_REFLECTION`), and the sun's highlight fades
    /// when matte.
    NoSkyMatteFades,
    /// prop_grass.fx (1800 `Grass` prps): no sun highlight
    /// (`BRDF_DISABLE_SPEC_LIGHTING`); the sky's reflection stays.
    NoSun,
}

impl Material {
    /// A leaf cluster's material's `vertex_format`, as `leaves` marks it: a
    /// renderer that turns squares to face the camera finds its meshes by
    /// it.
    pub const BILLBOARDS: &str = "INSTANCING_BBCLUSTER";
}

/// The shader a material draws with, numbered as its cfg's dialect numbers
/// them: the root element says which, so the numbers of two dialects never
/// mix.
#[derive(Debug, Clone, PartialEq)]
pub enum Shader {
    /// `ShaderID` of a `<Config>` cfg (117, 1800), 8 when it names none. A
    /// prp's `<Type>` decides how its materials composite instead.
    Id(u32),
    /// `ShaderIndex` of an `<m_Config>` cfg (2070), a slot in the engine's
    /// model shader table, and what `model.fxh` reads beyond the shared
    /// fields.
    ModelFx(u32, ModelFx),
}

/// An `<m_Config>` material's own inputs to `model.fxh`, by its
/// `ShaderIndex` (0 and 10 `model_glow.fx`, 6 `model_dye.fx`, 3
/// `model_foamfall.fx`) and switches.
#[derive(Debug, Clone, PartialEq)]
pub struct ModelFx {
    /// The normal map's blue is how much a texel glows (`MODEL_GLOW`):
    /// lighting turns toward 8 times the albedo there.
    pub glow: bool,
    /// The mask's green dyes by the owner's colour up to a half and glows
    /// above it (`MODEL_DYE`, index 6).
    pub dye: bool,
    /// `MaskTexture` (`cTexture4`): red reflects the environment under
    /// `EnvironmentEnabled`, green dyes under `dye`. `None` when neither
    /// reads it.
    pub mask: Option<String>,
    /// `EnvironmentEnabled`: the mask's red blends toward the sky cube
    /// times the albedo, ×1.5.
    pub reflect: bool,
    /// `RipplesTexture` under `RipplesEnabled` (`cTexture5`): wind ripples
    /// from world XZ, bending the normal (flags, sails, the liferaft).
    pub ripples: Option<String>,
    /// `HeightTexture` under `ParallaxEnabled` (`cTexture3`): DX11
    /// tessellation displaces by it (the Tech monument).
    pub height: Option<String>,
    /// `model_foamfall.fx` (index 3), waterfalls' foam: the diffuse is no
    /// colour but foam in red, opacity in alpha times blue
    /// (model_foamfall.fx:105-117). Drawn as itself its red and blue show
    /// magenta.
    pub foam: bool,
}

/// A material's own light (`model_common.fxh`, `psBasePrepare`): by day
/// where its normal map's blue says, and by night also where its night
/// map does.
#[derive(Debug, Clone, PartialEq)]
pub struct Glow {
    /// `cEmissiveColor` as the cfg stores it: a gamma colour, which the
    /// engine takes to linear (its default 2 stands in as
    /// `GammaToLinear(2.0)` when a shader has no such constant).
    pub color: [f32; 3],
    /// `cNightGlowMap` under `NIGHT_GLOW_ENABLED`: grey windows, fires and
    /// lamps in red, green and blue, one channel per instance.
    pub night_map: Option<String>,
    /// `WINDOW_INTERIOR_ENABLED` (117, with a night map): a room drawn
    /// behind the night map's windows (`ApplyWindowInterior`).
    pub interior: Option<Interior>,
}

/// A window's fake room: `cWindowInteriorDistance` UV units behind the
/// surface, the engine's wall texture at `cWindowInteriorScale` times the
/// UVs plus `cWindowInteriorOffset`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Interior {
    pub distance: f32,
    pub scale: f32,
    pub offset: [f32; 2],
}

/// See [`Material::splat`]. DLC 3's dunes and terrain blockers draw their
/// rock and sand this way over a flat grey `cModelDiffTex`.
#[derive(Debug, Clone, PartialEq)]
pub struct Splat {
    /// `cModelSplatMap`, on the mesh's UVs: each channel weighs a layer.
    pub map: String,
    /// Red, green, blue, alpha: `SPLAT_<channel>_ENABLED` and its
    /// `cModelSplatDiffTex`, `cModelSplatNormalTex` and `cSplatTiling`
    /// component. A channel left off weighs nothing.
    pub layers: [Option<SplatLayer>; 4],
    /// `cSplatMixAlbedo`: `cModelDiffTex`'s red, doubled, tints the blend
    /// (128 is 1); else the blend is the albedo.
    pub mix_albedo: bool,
    /// `cSplatMixNormal`: the blend's normal bends the normal map's; else
    /// it replaces it.
    pub mix_normal: bool,
    /// `SPLAT_SCALE_INVARIANT`: layer UVs grow with the object's average
    /// scale, so the layers keep their size however large it is placed.
    pub scale_invariant: bool,
    /// `cSplatWorldTiling`: layers take their UVs from world X/Z instead.
    pub world_tiling: bool,
}

/// One [`Splat`] channel's layer.
#[derive(Debug, Clone, PartialEq)]
pub struct SplatLayer {
    pub diffuse: String,
    pub normal: Option<String>,
    /// Repeats over the mesh's UV square.
    pub tiling: f32,
}

/// See [`Material::world_tiling`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WorldTiling {
    pub scale: f32,
    pub diagonal: bool,
}

/// How a mesh decal covers what is under it: `opacity` times diffuse
/// alpha, times the smoothstepped green of `alpha_map` when there is one.
#[derive(Debug, Clone, PartialEq)]
pub struct MeshDecal {
    /// `cOpacity`.
    pub opacity: f32,
    /// Shipped `.dds` of `cAlphaTex` when `ALPHA_TEXTURE_ENABLED` is 1.
    pub alpha_map: Option<String>,
    /// Metres the game moves it toward the camera: 0.01, 0.2 with
    /// `cHigherDepthBias`, 0 with `cDisableDepthBias`.
    pub bias: f32,
    /// `RENDER_BEFORE_DECALS` (117): drawn with the ground, before the box
    /// decals and the models (Anno117.exe's "Model before decals" pass),
    /// as its dirt grounds and plaza tiles are; otherwise over the models.
    pub before_decals: bool,
}

/// How the game bends a material's vertices to the ground under them
/// (`model_footer.fxh`). Each vertex keeps its height above the object's
/// origin, measured from the ground under the vertex, unless `absolute`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TerrainAdapt {
    /// `ABSOLUTE_TERRAIN_ADAPTION`: every vertex goes onto the ground.
    pub absolute: bool,
    /// `VERTEX_COLORED_TERRAIN_ADAPTION`: vertex color blue is how rigid
    /// each vertex stays, strength `1 - blue`.
    pub vertex_colored: bool,
    /// `VERTEX_COLORED_LOCAL_PIVOTS` (Simplygon meshes): vertices with a
    /// green pivot sample the ground at that pivot and adapt fully.
    pub local_pivots: bool,
}

/// One `<Models>` entry: an rdm placed in the object.
#[derive(Debug, Clone)]
pub struct Part {
    /// The cfg element it came from: the `<Models>` entry, a prp's root.
    pub element: Id,
    pub name: String,
    /// Shipped `.rdm` path (the cfg names the `.fbx` source).
    pub rdm: String,
    /// Loaded by [`Model::load`]; `None` if missing or unreadable, see
    /// [`Model::problems`].
    pub mesh: Option<Rdm>,
    pub transform: Transform,
    pub materials: Vec<Material>,
    /// Attached to a bone of another part (`BONELINK_TRANSFORM`); the local
    /// transform is relative to that bone, so the static placement is off.
    pub bone_linked: bool,
    /// `DeformToTerrainHeight`: every material of the part bends to the
    /// ground as if it set `ADJUST_TO_TERRAIN_HEIGHT`.
    pub deform_to_terrain: bool,
    /// `<Animations>`: the shipped animation rdms the part plays; a
    /// sequence's `AnimationID` indexes this list.
    pub animations: Vec<String>,
    /// Each animation's `Scale`: a sequence plays it this many times
    /// slower (a clip spans duration × Scale × LoopCount).
    pub animation_scales: Vec<f32>,
    /// Each animation's `LoopCount`: how many times a sequence clip plays
    /// it before holding its last pose; 0 leaves that to the sequence's
    /// `Looped` (docs/model-animation.md, Repeat or hold).
    pub animation_loops: Vec<u32>,
    pub rig: Rig,
}

/// What a `<Transformer>` says beyond the static placement: a joint it
/// hangs on, motion of its own, and what conditions and damage do to it.
/// docs/model-animation.md.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Rig {
    pub link: Option<BoneLink>,
    pub motion: Option<Motion>,
    /// `VISIBILITY_TRANSFORM`'s `Conditions`: shown while a sequence
    /// raises any of these bits (a villager's tool while it works).
    pub shown_if: Option<u32>,
    pub color: Option<ColorStates>,
    pub damage: Option<Damage>,
}

/// `COLOR_TRANSFORM`: a colour for the raised conditions, which the entry
/// blends to over the state's `Time` (Anno117.exe build 26043928,
/// 0x14231C750). Its alpha fades the entry.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ColorStates {
    /// The bits it listens to: others raised count as none.
    pub conditions: u32,
    /// Clear until a state's condition is raised.
    pub start_invisible: bool,
    states: [ColorState; ColorStates::MAX],
    count: u8,
}

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct ColorState {
    pub color: [f32; 4],
    /// Seconds to blend to it.
    pub time: f32,
    pub condition: u32,
}

impl ColorStates {
    /// The most states kept, later ones dropped: the most a shipped one
    /// has is 5 (1800's), and a `Rig` stays small enough to copy.
    pub const MAX: usize = 6;

    pub fn states(&self) -> &[ColorState] {
        &self.states[..usize::from(self.count)]
    }

    /// The state the raised bits pick: the first sharing a bit with them,
    /// or with none raised the first that needs none. `None` picks none.
    pub fn pick(&self, raised: u32) -> Option<usize> {
        let raised = if raised & self.conditions == 0 {
            0
        } else {
            raised
        };
        let hit = |s: &ColorState| raised & s.condition != 0 || (raised == 0 && s.condition == 0);
        self.states().iter().position(hit)
    }

    /// The colour state `k` heads for and the seconds it takes. With none
    /// picked the game heads for white over a second; one that starts
    /// invisible stays clear instead, until a condition picks a state.
    pub fn target(&self, k: Option<usize>) -> ([f32; 4], f32) {
        match k.and_then(|k| self.states().get(k)) {
            Some(s) => (s.color, s.time),
            None if self.start_invisible => ([1., 1., 1., 0.], 0.),
            None => ([1.; 4], 1.),
        }
    }
}

/// A `COLOR_TRANSFORM` running: its colour now, the state it heads for and
/// how long it has been on its way.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ColorRun {
    pub color: [f32; 4],
    state: Option<usize>,
    elapsed: f32,
}

impl ColorRun {
    /// As its first frame shows it: at the state `raised` picks, or clear,
    /// to fade in, when it starts invisible.
    pub fn new(c: &ColorStates, raised: u32) -> ColorRun {
        let state = c.pick(raised);
        ColorRun {
            color: match c.start_invisible {
                true => [1., 1., 1., 0.],
                false => c.target(state).0,
            },
            state,
            elapsed: 0.,
        }
    }

    /// `dt` seconds on toward the state `raised` picks, which a new pick
    /// starts over: the game blends linearly, to arrive after the state's
    /// `Time`. `snap` arrives at once.
    pub fn step(&mut self, c: &ColorStates, raised: u32, dt: f32, snap: bool) {
        let pick = c.pick(raised);
        if pick != self.state {
            (self.state, self.elapsed) = (pick, 0.);
        }
        let (to, time) = c.target(pick);
        if snap || time <= self.elapsed + dt {
            self.color = to;
            self.elapsed = self.elapsed.max(time);
        } else {
            let f = dt / (time - self.elapsed);
            self.color = std::array::from_fn(|k| self.color[k] + (to[k] - self.color[k]) * f);
            self.elapsed += dt;
        }
    }
}

/// `DAMAGE_TRANSFORM`: the entry's scale with the building's damage (0 to
/// 255), `StartScale` up to `StartDamage`, then toward `EndScale` at
/// `EndDamage`. A fire from scale 0 shows only on a damaged building.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Damage {
    pub damage: [f32; 2],
    pub scale: [f32; 2],
}

impl Damage {
    pub fn scale_at(&self, damage: f32) -> f32 {
        let [d0, d1] = self.damage;
        let t = match d1 > d0 {
            true => ((damage - d0) / (d1 - d0)).clamp(0., 1.),
            false => f32::from(u8::from(damage >= d0)),
        };
        self.scale[0] + (self.scale[1] - self.scale[0]) * t
    }
}

/// `BONELINK_TRANSFORM`: the entry hangs on a joint of `Models[model]`,
/// its placement then in that joint's space.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BoneLink {
    pub model: u32,
    /// [`crate::rdm::bone_hash`] of the joint's name.
    pub bone: u32,
}

/// `ANIMATION_TRANSFORM`: a part's own motion, as the game's record holds
/// it and moves it (build 24988830, `0x14A2BF730`; docs/model-animation.md).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Motion {
    pub offset: [f32; 3],
    /// Radians per second of the offset's phase.
    pub offset_speed: f32,
    /// `PositionRotate`: the offset turns with its phase about all axes.
    pub offset_turns: bool,
    /// Radians about X, Y and Z: a turn, `axis(Y,-y)·axis(X,-x)·axis(Z,-z)`.
    pub rotation: [f32; 3],
    /// That turn per second.
    pub rotation_speed: f32,
    /// Nonzero: a swing of ±`oscillation / 2` degrees about the turn's axis
    /// instead of a spin.
    pub oscillation: f32,
    pub alignment: Alignment,
    /// `SpringEnabled`: a wheel on suspension.
    pub spring: Option<Spring>,
}

/// `ANIMATION_TRANSFORM/Alignment`: what the part turns to face each frame,
/// before its own motion (`sub_1421C30D0`, build 24988830).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Alignment {
    #[default]
    None,
    /// 1: +X turned about Y away from a direction the session keeps,
    /// taken here as the wind (no shipped 117 cfg uses it).
    AgainstWind,
    /// 2: +X turned toward the wind (flags), keeping the part's tilt.
    WithWind,
    /// 3: the camera's rotation, a billboard (info-layer icons).
    Camera,
    /// 4: the camera's turn about Y only, an upright billboard.
    CameraYaw,
    /// 5, 6, 7: clock hands, turned about the part's Z by the time of day.
    Hours,
    Minutes,
    Seconds,
}

impl Alignment {
    fn from_byte(v: u32) -> Alignment {
        match v {
            1 => Alignment::AgainstWind,
            2 => Alignment::WithWind,
            3 => Alignment::Camera,
            4 => Alignment::CameraYaw,
            5 => Alignment::Hours,
            6 => Alignment::Minutes,
            7 => Alignment::Seconds,
            _ => Alignment::None,
        }
    }
}

/// `SpringMin`, `SpringMax`, `SpringOffset` (`sub_1421C2A60`, build 24988830): the part
/// rides `clamp(offset - d, min, max)` above where a ray down its -Y meets
/// the ground `d` away, and rolls by the distance it travels over `offset`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Spring {
    pub min: f32,
    pub max: f32,
    pub offset: f32,
}

/// What [`Motion::place`] reads from around the model.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Scene {
    /// The camera's rotation in the model's space: its +X screen right, +Y
    /// screen up, +Z into the screen (the game's convention, left-handed),
    /// so a billboard's -Z faces the viewer.
    pub camera: [f32; 4],
    /// Where the wind blows to, along the ground; any length.
    pub wind: [f32; 3],
    /// Hours (0-23), minutes and seconds of the time of day.
    pub clock: [u32; 3],
    /// Height of the ground a spring rides on.
    pub ground: f32,
}

fn axis(a: usize, angle: f32) -> [f32; 4] {
    let (s, c) = (angle / 2.).sin_cos();
    let mut q = [0., 0., 0., c];
    q[a] = s;
    q
}

/// The game's Euler builder for the X and Z angles it keeps when it aligns:
/// `axis(Z, z) · axis(X, x)`. Its decomposition reads the part's rotation
/// as `axis(Y, heading) · axis(Z, attitude) · axis(X, bank)` and drops the
/// heading; past 0.449 of the norm (not the textbook 0.499) it snaps the
/// attitude to ±90°.
fn tilt(q: [f32; 4]) -> [f32; 4] {
    let [x, y, z, w] = q;
    let n = x * x + y * y + z * z + w * w;
    let test = w * z + y * x;
    let (attitude, bank) = if test > 0.449 * n {
        (std::f32::consts::FRAC_PI_2, 0.)
    } else if test < -0.449 * n {
        (-std::f32::consts::FRAC_PI_2, 0.)
    } else {
        let attitude = (2. * test / n).clamp(-1., 1.).asin();
        let bank = (2. * (x * w - y * z)).atan2(y * y - x * x - z * z + w * w);
        (attitude, bank)
    };
    quat_mul(axis(2, attitude), axis(0, bank))
}

/// The turn about Y of `q`, read as [`tilt`] reads it.
fn heading(q: [f32; 4]) -> f32 {
    let [x, y, z, w] = q;
    let n = x * x + y * y + z * z + w * w;
    let test = w * z + y * x;
    if test > 0.449 * n {
        2. * x.atan2(w)
    } else if test < -0.449 * n {
        -2. * x.atan2(w)
    } else {
        (2. * (w * y - x * z)).atan2(w * w + x * x - y * y - z * z)
    }
}

impl Motion {
    /// `t` seconds in: the offset in the parent's frame and the rotation in
    /// the part's own (its placement times this), as the game steps them
    /// from phases at 0.
    pub fn at(&self, t: f32) -> ([f32; 3], [f32; 4]) {
        let tau = std::f32::consts::TAU;
        let mut rotation = [0., 0., 0., 1.];
        if self.rotation != [0.; 3] {
            let [x, y, z] = self.rotation;
            let q = quat_mul(axis(1, -y), quat_mul(axis(0, -x), axis(2, -z)));
            let len = q.iter().map(|c| c * c).sum::<f32>().sqrt().max(1e-12);
            let n = [q[0], q[1], q[2]];
            let n_len = (n[0] * n[0] + n[1] * n[1] + n[2] * n[2]).sqrt();
            let mut phi = 2. * (q[3] / len).clamp(-1., 1.).acos();
            if phi > std::f32::consts::PI {
                phi -= tau;
            }
            if n_len > 1e-9 {
                let n = n.map(|c| c / n_len);
                let phase = (t * -phi * self.rotation_speed) % tau;
                let a = match self.oscillation == 0. {
                    true => phase,
                    false => phase.sin() * self.oscillation * std::f32::consts::PI / 360.,
                };
                let (s, c) = (-a / 2.).sin_cos();
                rotation = [n[0] * s, n[1] * s, n[2] * s, c];
            }
        }
        let mut offset = [0.; 3];
        if self.offset != [0.; 3] {
            let phase = (t * self.offset_speed) % tau;
            let (s, c) = phase.sin_cos();
            offset = [self.offset[0] * s, self.offset[1] * c, -self.offset[2] * s];
            if self.offset_turns {
                let e = quat_mul(axis(1, -phase), quat_mul(axis(0, -phase), axis(2, -phase)));
                offset = rotate(e, offset);
            }
        }
        (offset, rotation)
    }

    /// The placement `at` (in the model's space) moved `t` seconds in, in
    /// the game's order: the spring, the alignment, the bob, the turn.
    /// Nothing travels in a model on its own, so a spring only rides.
    pub fn place(&self, t: f32, at: &Transform, scene: &Scene) -> Transform {
        let mut out = *at;
        if let Some(spring) = self.spring {
            let down = rotate(out.rotation, [0., -1., 0.]);
            let d = (scene.ground - out.position[1]) / down[1];
            if down[1] < 0. && d >= 0. {
                // The game's order: fmaxf(fminf(max, offset - d), min).
                let lift = (spring.offset - d).min(spring.max).max(spring.min);
                out.position = [0, 1, 2].map(|k| out.position[k] - down[k] * lift);
            }
        }
        out.rotation = match self.alignment {
            Alignment::None => out.rotation,
            Alignment::AgainstWind => {
                let [x, _, z] = scene.wind;
                quat_mul(axis(1, -(-z).atan2(-x)), tilt(out.rotation))
            }
            Alignment::WithWind => {
                let [x, _, z] = scene.wind;
                let len = (x * x + z * z).sqrt();
                // The arc from +X to the wind: (a × h, a · h), h halfway.
                let arc = match len > 1e-9 {
                    true => {
                        let b = [x / len, 0., z / len];
                        let h = [1. + b[0], 0., b[2]];
                        let hl = (h[0] * h[0] + h[2] * h[2]).sqrt();
                        match hl > 1e-9 {
                            true => [0., -h[2] / hl, 0., h[0] / hl],
                            false => [0., 1., 0., 0.],
                        }
                    }
                    false => [0., 0., 0., 1.],
                };
                quat_mul(arc, tilt(out.rotation))
            }
            Alignment::Camera => scene.camera,
            Alignment::CameraYaw => axis(1, heading(scene.camera)),
            Alignment::Hours | Alignment::Minutes | Alignment::Seconds => {
                let [h, m, sec] = scene.clock.map(|v| v as f32);
                let degrees = match self.alignment {
                    Alignment::Hours => (if h > 12. { h - 12. } else { h }) * 30. + m * 0.5,
                    Alignment::Minutes => m * 6.,
                    _ => sec * 6.,
                };
                quat_mul(out.rotation, axis(2, -degrees.to_radians()))
            }
        };
        let (offset, turn) = self.at(t);
        out.position = [0, 1, 2].map(|k| out.position[k] + offset[k]);
        out.rotation = quat_mul(out.rotation, turn);
        out
    }

    /// Seconds after which `at` comes round again, from its turn's and its
    /// offset's phases; `None` when neither moves. With both at rates
    /// that never meet within eight turns, the longer of the two, so a
    /// loop of it jumps once.
    pub fn period(&self) -> Option<f32> {
        let tau = std::f32::consts::TAU;
        // The turn's angle, as `at` takes it.
        let turn = (self.rotation != [0.; 3] && self.rotation_speed != 0.)
            .then(|| {
                let axis = |a: usize, angle: f32| {
                    let (s, c) = (angle / 2.).sin_cos();
                    let mut q = [0., 0., 0., c];
                    q[a] = s;
                    q
                };
                let [x, y, z] = self.rotation;
                let q = quat_mul(axis(1, -y), quat_mul(axis(0, -x), axis(2, -z)));
                let len = q.iter().map(|c| c * c).sum::<f32>().sqrt().max(1e-12);
                if (q[0] * q[0] + q[1] * q[1] + q[2] * q[2]).sqrt() <= 1e-9 {
                    return None;
                }
                let mut phi = 2. * (q[3] / len).clamp(-1., 1.).acos();
                if phi > std::f32::consts::PI {
                    phi -= tau;
                }
                let rate = (phi * self.rotation_speed).abs();
                (rate > 1e-6).then(|| tau / rate)
            })
            .flatten();
        let offset = (self.offset != [0.; 3] && self.offset_speed.abs() > 1e-6)
            .then(|| tau / self.offset_speed.abs());
        match (turn, offset) {
            (Some(a), Some(b)) => {
                let (long, short) = (a.max(b), a.min(b));
                let met = (1..=8).map(|k| k as f32 * long).find(|&p| {
                    let laps = p / short;
                    (laps - laps.round()).abs() < 0.01
                });
                Some(met.unwrap_or(long))
            }
            (a, b) => a.or(b),
        }
    }
}

/// A `<Sequences>` entry: what one state of the object (idle, walk, work)
/// plays, on a clock of its own.
#[derive(Debug, Clone, PartialEq)]
pub struct Sequence {
    pub element: Id,
    /// `action << 24 | (info + 52) << 8 | variant`, or a legacy number.
    pub id: u32,
    /// Rate of the sequence's clock against real time.
    pub speed: f32,
    pub looped: bool,
    /// `DisableEffects`: the model's own effects, the particles no clip
    /// runs, stay off while it plays. Production buildings set it on their
    /// idle sequence, so their smoke shows only while they work.
    pub disable_effects: bool,
    /// `RestartEffects`: starting it starts those effects over. 117's
    /// working sequences set it after an idle one that turned them off.
    pub restart_effects: bool,
    pub clips: Vec<Clip>,
}

impl Sequence {
    /// Seconds on its clock: where the last clip ends.
    pub fn length(&self) -> f32 {
        self.clips.iter().map(|c| c.end).fold(0., f32::max)
    }
}

/// A `TrackElement`: what plays from `start` to `end`, seconds on its
/// sequence's clock (the cfg stores ms).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Clip {
    pub element: Id,
    pub track: u32,
    pub start: f32,
    pub end: f32,
    pub what: Play,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Play {
    /// `Models[model]` plays its `Animations[animation]`.
    Animation { model: u32, animation: u32 },
    /// `Particles[particle]` runs.
    Particle(u32),
    /// These condition bits are raised: entries whose transformer tests
    /// them (`VISIBILITY_TRANSFORM`) show.
    Condition(u32),
    /// An audio asset plays at a joint.
    Sound(Sound),
    /// A type the editor does not know, by number.
    Other(u32),
}

/// A sound clip's fields. docs/audio-format.md, "Sequence sounds", has the
/// counts behind each.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Sound {
    /// `GUID`: the `Audio` asset.
    pub guid: u32,
    /// `Looped`: the sound runs from the clip's start until the sequence
    /// stops playing, rather than once each time the playhead crosses it.
    pub looped: bool,
    /// `ModelID` and `BoneID`: joint `bone`, an index into the skeleton of
    /// `Models[model]`, is where it plays. Out of range they name nothing:
    /// 24 of 117's 2,977 clips and 896 of 1800's 5,342 point past the
    /// cfg's meshes, often with uninitialised exporter bytes that read as
    /// text (`"Box."`, `"doRe"`). `SkeletonID` is 0 beside all but 3
    /// in-range pairs, so it is not kept.
    pub model: u32,
    pub bone: u32,
}

impl Sound {
    /// `guid`, not looped, on the first mesh's first joint: where 2,947 of
    /// 117's 2,977 sound clips play.
    pub fn of(guid: u32) -> Sound {
        Sound {
            guid,
            ..Sound::default()
        }
    }
}

/// One `<Files>` entry: another cfg placed in this one.
#[derive(Debug, Clone, PartialEq)]
pub struct Prop {
    /// The `<Files>` entry.
    pub element: Id,
    name: String,
    /// Normalized cfg path, load it with [`Model::load`].
    pub cfg: String,
    pub transform: Transform,
    /// `DeformToTerrainHeight`: every part of the placed cfg bends.
    pub deform_to_terrain: bool,
    /// `AdaptTerrainHeight`: the placed cfg keeps its height above the
    /// ground under its own origin.
    pub adapt_terrain_height: bool,
    pub rig: Rig,
}

/// One `<Decals>` entry: a box laid over the terrain under it. The box
/// spans `±extents` around the transform's origin; the game drapes a grid
/// over its X/Z on the terrain (`decal_pbr.fx`), the texture's U along +X
/// and V against +Z, and Y plays no part.
#[derive(Debug, Clone, PartialEq)]
pub struct Decal {
    /// The `<Decals>` entry.
    pub element: Id,
    pub transform: Transform,
    pub extents: [f32; 3],
    /// `TexCoords` as `[u0, v0, u1, v1]`: the box's 0..1 maps into it.
    pub uv_rect: [f32; 4],
    /// `Terrain`, `Terrain-Baked` (ShaderID 2, which the game bakes into
    /// the terrain's textures) or `Vehicle` (tracks, drawn at run time).
    pub kind: String,
    pub material: Option<Material>,
    /// `cOpacity` (ShaderID 1) or `cDiffuseOpacity` (ShaderID 2).
    pub opacity: f32,
    pub fade: Option<DecalFade>,
    /// `cHeightMap` when `HEIGHT_MAP_ENABLED`: frays a `Grid` fade.
    pub height_map: Option<String>,
    /// `ENABLE_UV_TRANSFORM` and its settings.
    pub uv_transform: Option<UvTransform>,
}

/// How a box decal fades toward its sides, measured past the whole
/// half-metre grid cells its box covers (`decal_pbr.fx`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum DecalFade {
    /// `cEnableBorderFadeout`: over `cFadeOutDiastance` metres; with a
    /// height map, frayed by `cHeightMapFadeoutThreshold` and
    /// `cHeightMapFadeoutFalloff`.
    Grid {
        distance: f32,
        threshold: f32,
        falloff: f32,
    },
    /// `cExperimentalEnableAlphaBasedFade`: from
    /// `cExperimentalAlphaFadeoutBorderDistance` metres past the grid over
    /// `cExperimentalAlphaFadeoutFadeDistance`. Wins over `Grid`.
    Smooth { border: f32, fade: f32 },
}

/// `UVMappingTransform`: UVs turn by `rotation` degrees (about the centre
/// with `about_center`), then scale by `tiling` (negated per flipped axis)
/// and move by `offset` (`ApplyUVTransform`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct UvTransform {
    tiling: [f32; 2],
    flip: [bool; 2],
    offset: [f32; 2],
    rotation: f32,
    about_center: bool,
}

impl UvTransform {
    pub fn apply(&self, uv: [f32; 2]) -> [f32; 2] {
        let center = if self.about_center { 0.5 } else { 0. };
        let (sin, cos) = self.rotation.to_radians().sin_cos();
        let [u, v] = [uv[0] - center, uv[1] - center];
        // HLSL RotateVec2D(v, (cos, sin)).
        let turned = [u * cos - v * sin + center, u * sin + v * cos + center];
        std::array::from_fn(|a| {
            let flip = if self.flip[a] { -1. } else { 1. };
            turned[a] * self.tiling[a] * flip + self.offset[a]
        })
    }
}

/// One `<Lights>` entry: a point or spot light the model carries, like a
/// fireplace's flicker.
#[derive(Debug, Clone, PartialEq)]
pub struct Light {
    /// The `<Lights>` entry.
    pub element: Id,
    pub name: String,
    pub transform: Transform,
    pub rig: Rig,
}

/// One `<Particles>` entry, or a 2070 `<m_Effects>` one.
#[derive(Debug, Clone, PartialEq)]
pub struct Particle {
    /// The `<Particles>` entry.
    pub element: Id,
    /// `.rdp` path; 2070's are `.efc`.
    pub file: String,
    pub transform: Transform,
    pub rig: Rig,
    /// `TimeScale`: the run lasts this many times its own length (3 is
    /// three times slower). docs/particle-format.md.
    pub time_scale: f32,
    /// `Color` and `Alpha`: multiply every particle's colour.
    pub tint: [f32; 4],
    /// `SubImageSize`: a flipbook's columns, rows and frame count.
    pub sub_images: Option<[u32; 3]>,
    /// `WindImpact`: how far wind drifts its particles over their life.
    pub wind_impact: f32,
}

/// The cfg dialects, told apart once by the root element.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Version {
    /// `<Config>`: 117's and 1800's cfgs.
    Config,
    /// `<Prop>`: 117's and 1800's prps, one mesh and its materials.
    Prop,
    /// `<m_Config>`: 2070's cfgs, its serializer's XML.
    MConfig,
}

impl Version {
    fn of(root: Node) -> Version {
        match (root.doc.prefix(), root.child("Prop")) {
            ("m_", _) => Version::MConfig,
            (_, Some(_)) => Version::Prop,
            _ => Version::Config,
        }
    }

    fn layout(self) -> &'static Layout {
        match self {
            Version::Config => &Layout::CONFIG,
            Version::Prop => &Layout::PROP,
            Version::MConfig => &Layout::M_CONFIG,
        }
    }
}

/// One dialect's element names and defaults where the dialects share a
/// shape. Its own fields (2070's material switches, a prp's `Type`) stay in
/// the dialect's reader.
#[derive(Debug)]
struct Layout {
    /// Without the file's prefix ([`Doc::prefix`]), as every name here.
    root: &'static str,
    center: &'static str,
    extent: &'static str,
    decals: &'static str,
    particles: &'static str,
    /// A material's shader number, and its value when the cfg names none.
    shader: &'static str,
    shader_default: f32,
    /// A decal material's shader when it names none.
    decal_shader_default: f32,
    /// The shader numbers that composite other than opaque and how; where a
    /// material field is named, the material's value replaces the number.
    alphas: &'static [(&'static [u32], Alpha, Option<&'static str>)],
    /// Decal shaders that take their opacity from `cDiffuseOpacity` rather
    /// than `cOpacity`.
    diffuse_opacity_decals: &'static [u32],
    /// 2070's [`ModelFx`] slots: every `model_glow.fx` slot, the one of
    /// them that dyes, and the foam fall.
    glow_shaders: &'static [u32],
    dye_shaders: &'static [u32],
    foam_shaders: &'static [u32],
    /// Detail levels count the other way round: `_lod0` is the coarsest.
    /// Of 2070's 956 shipped parts with several levels, the last has more
    /// triangles in 924 and as many in 32 (barracks: 449, 975, 1,283).
    lod0_coarsest: bool,
}

impl Layout {
    const CONFIG: Layout = Layout {
        root: "Config",
        center: "MeshCenter",
        extent: "MeshExtent",
        decals: "Decals",
        particles: "Particles",
        shader: "ShaderID",
        shader_default: 8.0,
        decal_shader_default: 1.0,
        alphas: &[
            // Plants.
            (&[21, 22, 23], Alpha::Cutout(0.5), Some("cShadowAlphaRef")),
            // Hair, and the opacity shaders.
            (&[12, 19], Alpha::Blend(1.0), Some("cOpacity")),
            // Mesh decals, and the terrain decal (2).
            (&[1, 2, 6, 26], Alpha::Decal, None),
            // Terrain and water cutout volumes.
            (&[4, 5], Alpha::Hidden, None),
            // model_pbr, model_icon, model_vat2 and model_terrain.fx.
            (&[8, 14, 20, 24], Alpha::Tested(MATERIAL_ALPHA_REF), None),
        ],
        diffuse_opacity_decals: &[2],
        glow_shaders: &[],
        dye_shaders: &[],
        foam_shaders: &[],
        lod0_coarsest: false,
    };
    /// A prp names no shader; its `<Type>` picks how it composites.
    const PROP: Layout = Layout {
        root: "Prop",
        ..Layout::CONFIG
    };
    const M_CONFIG: Layout = Layout {
        root: "Config",
        center: "Center",
        extent: "Extent",
        decals: "TerrainDecals",
        particles: "Effects",
        shader: "ShaderIndex",
        shader_default: 0.0,
        decal_shader_default: 0.0,
        alphas: &[
            // model_cutout.fx and model_foamfall.fx clip below REF_ALPHA 1
            // of 255, model_video.fx below 50.
            (&[3, 4], Alpha::Cutout(1.0 / 255.0), None),
            (&[13], Alpha::Cutout(50.0 / 255.0), None),
            // Water and holograms draw translucent after the scene, with
            // refraction, scrolling and scan lines; a half-clear sheet
            // stands in for them.
            (&[1, 12], Alpha::Blend(0.5), None),
        ],
        diffuse_opacity_decals: &[],
        // Every ordinary 2070 model draws with `model_glow.fx`; there is no
        // plain `model.fx` slot (port2070 docs/2070-models.md, the table at
        // Anno5.exe build 11514956 0x141665640).
        glow_shaders: &[0, 6, 10],
        dye_shaders: &[6],
        foam_shaders: &[3],
        lod0_coarsest: true,
    };

    /// How shader number `shader` composites, with `material`'s override
    /// where its row names one.
    fn alpha(&self, shader: u32, material: Node) -> Alpha {
        let Some(&(_, alpha, field)) = self.alphas.iter().find(|a| a.0.contains(&shader)) else {
            return Alpha::Opaque;
        };
        match (alpha, field.and_then(|f| material.f32(f))) {
            (Alpha::Cutout(_), Some(v)) => Alpha::Cutout(v),
            (Alpha::Blend(_), Some(v)) => Alpha::Blend(v),
            (alpha, _) => alpha,
        }
    }
}

/// A parsed `.cfg`.
#[derive(Debug, Clone, Default)]
pub struct Model {
    /// Normalized cfg path; empty for [`Model::parse`].
    pub path: String,
    /// Object bounds, `MeshCenter ± MeshExtent`, covering all parts.
    pub mesh_center: [f32; 3],
    pub mesh_extent: [f32; 3],
    pub parts: Vec<Part>,
    pub props: Vec<Prop>,
    pub decals: Vec<Decal>,
    pub particles: Vec<Particle>,
    pub lights: Vec<Light>,
    pub sequences: Vec<Sequence>,
    /// Missing or broken referenced files. A model with problems is still
    /// usable; the editor draws what loaded.
    pub problems: Vec<String>,
}

impl Model {
    /// Parses a cfg and maps source paths to shipped names (`.fbx` -> `.rdm`,
    /// textures -> `<stem>_0.dds`) without checking they exist.
    pub fn parse(cfg: &[u8]) -> Result<Model> {
        Model::from_doc(&Doc::parse(cfg))
    }

    /// [`Model::parse`] of a cfg already in a document, as an editor holds
    /// it. Each entry keeps its element's [`Id`].
    pub fn from_doc(doc: &Doc) -> Result<Model> {
        Ok(Model::parse_versioned(doc)?.0)
    }

    fn parse_versioned(doc: &Doc) -> Result<(Model, Version)> {
        let root = Node::root(doc);
        let version = Version::of(root);
        let layout = version.layout();
        let main = || root.child(layout.root).expect("Version::of found it");
        let model = match version {
            Version::MConfig => parse_m_config(main()),
            Version::Prop => parse_prop(main()),
            Version::Config => parse_config(root)?,
        };
        Ok((model, version))
    }
}

/// A prp: one mesh, materials that composite as its `<Type>` says.
fn parse_prop(prop: Node) -> Model {
    let layout = Version::Prop.layout();
    let materials = prop
        .child("Materials")
        .into_iter()
        .flat_map(Node::children)
        .map(|c| {
            let mut m = material(c, layout);
            m.vertex_format = prop.text("VertexFormat").into();
            // A prp has no ShaderID; its `<Type>` picks the shader,
            // and the rest stay solid, not the cfgs' tested default.
            m.alpha = Alpha::Opaque;
            match prop.text("Type") {
                "Plant" => {
                    m.alpha = Alpha::Cutout(c.f32("cAlphaRef").unwrap_or(0.5));
                    m.specular = Specular::NoSkyMatteFades;
                }
                "Grass" => {
                    m.alpha = Alpha::Cutout(c.f32("cAlphaRef").unwrap_or(0.5));
                    m.specular = Specular::NoSun;
                }
                "Terrain" => m.specular = Specular::MatteFades,
                // Ice floes and building ground details blend onto
                // what's below. They're never a solid sheet.
                kind @ ("Decal" | "Decal Detail") => {
                    m.alpha = Alpha::Decal;
                    // prop_decal_detail.fx fades matte specular;
                    // prop_decal.fx keeps it.
                    if kind == "Decal Detail" {
                        m.specular = Specular::MatteFades;
                    }
                    m.mesh_decal = Some(MeshDecal {
                        opacity: 1.,
                        alpha_map: None,
                        bias: 0.01,
                        before_decals: false,
                    });
                }
                "SimplePBR" if c.text("FORCE_ALPHA_BLEND") == "1" => m.alpha = Alpha::Blend(1.0),
                _ => {}
            }
            m
        })
        .collect();
    Model {
        parts: vec![Part {
            element: prop.id,
            name: prop.text("Type").into(),
            rdm: mesh_path(prop.text("MeshFileName")),
            mesh: None,
            transform: Transform::IDENTITY,
            materials,
            bone_linked: false,
            deform_to_terrain: false,
            animations: Vec::new(),
            animation_scales: Vec::new(),
            animation_loops: Vec::new(),
            rig: Rig::default(),
        }],
        ..Model::default()
    }
}

/// 117's and 1800's cfg: the MAIN config's models, nested cfgs, decals,
/// particles and lights.
fn parse_config(root: Node) -> Result<Model> {
    let layout = Version::Config.layout();
    let main = root
        .configs()
        .find(|c| c.config_type() == Some("MAIN"))
        .or_else(|| root.child(layout.root))
        .context("cfg has no MAIN Config")?;
    let mut model = Model::placed(main, layout);
    for c in main.list("Models") {
        let (transform, bone_linked) = transformer(c);
        model.parts.push(Part {
            element: c.id,
            name: c.text("Name").to_string(),
            rdm: mesh_path(c.text("FileName")),
            mesh: None,
            transform,
            materials: c.list("Materials").map(|m| material(m, layout)).collect(),
            bone_linked,
            deform_to_terrain: c.text("DeformToTerrainHeight") == "1",
            animations: (c.list("Animations"))
                .map(|a| mesh_path(a.text("FileName")))
                .collect(),
            animation_scales: (c.list("Animations"))
                .map(|a| a.f32("Scale").filter(|s| *s > 0.).unwrap_or(1.))
                .collect(),
            animation_loops: (c.list("Animations"))
                .map(|a| a.u32("LoopCount").unwrap_or(0))
                .collect(),
            rig: rig(c),
        });
    }
    for c in main.list("Files") {
        let file = c.text("FileName");
        if file.is_empty() {
            continue;
        }
        model.props.push(Prop {
            element: c.id,
            name: c.text("Name").to_string(),
            cfg: normalize_path(file).into_owned(),
            transform: transformer(c).0,
            deform_to_terrain: c.text("DeformToTerrainHeight") == "1",
            adapt_terrain_height: c.text("AdaptTerrainHeight") == "1",
            rig: rig(c),
        });
    }
    for c in main.list(layout.decals) {
        let tex = c.vec4("TexCoords").unwrap_or([0.0, 0.0, 1.0, 1.0]);
        let m = c.list("Materials").next();
        let flag = |name: &str| m.is_some_and(|m| m.text(name) == "1");
        let num = |name: &str, or: f32| m.and_then(|m| m.f32(name)).unwrap_or(or);
        let fade = if flag("cExperimentalEnableAlphaBasedFade") {
            Some(DecalFade::Smooth {
                border: num("cExperimentalAlphaFadeoutBorderDistance", 0.25),
                fade: num("cExperimentalAlphaFadeoutFadeDistance", 0.1).max(1e-3),
            })
        } else if flag("cEnableBorderFadeout") {
            Some(DecalFade::Grid {
                distance: num("cFadeOutDiastance", 0.2),
                threshold: num("cHeightMapFadeoutThreshold", 0.45),
                falloff: num("cHeightMapFadeoutFalloff", 0.1),
            })
        } else {
            None
        };
        let shader = m.and_then(|m| m.f32(layout.shader));
        let shader = shader.unwrap_or(layout.decal_shader_default) as u32;
        model.decals.push(Decal {
            element: c.id,
            transform: transformer(c).0,
            extents: c.vec3("Extents").unwrap_or_default(),
            uv_rect: tex,
            kind: c.text("Type").to_string(),
            material: m.map(|m| material(m, layout)),
            opacity: match layout.diffuse_opacity_decals.contains(&shader) {
                true => num("cDiffuseOpacity", 1.0),
                false => num("cOpacity", 1.0),
            }
            .clamp(0.0, 1.0),
            fade,
            height_map: m
                .filter(|_| flag("HEIGHT_MAP_ENABLED"))
                .map(|m| m.text("cHeightMap"))
                .filter(|t| !t.is_empty())
                .map(texture_path),
            uv_transform: flag("ENABLE_UV_TRANSFORM").then(|| UvTransform {
                tiling: [num("cTexTiling.x", 1.0), num("cTexTiling.y", 1.0)],
                flip: [flag("cTexFlipU"), flag("cTexFlipV")],
                offset: [num("cTexOffset.x", 0.0), num("cTexOffset.y", 0.0)],
                rotation: num("cTexRotation", 0.0),
                about_center: flag("cTexRotateAboutCenter"),
            }),
        });
    }
    for c in main.list("Lights") {
        model.lights.push(Light {
            element: c.id,
            name: c.text("Name").to_string(),
            transform: transformer(c).0,
            rig: rig(c),
        });
    }
    for c in main.list("Sequences") {
        model.sequences.push(sequence(c));
    }
    Ok(model)
}

/// A SEQUENCE config: its tracks' elements, flattened, times in seconds.
fn sequence(c: Node) -> Sequence {
    let num = |n: Node, name: &str| n.f32(name).unwrap_or(0.);
    let mut clips = Vec::new();
    for track in c.children().filter(|t| t.name() == "Track") {
        let id = track.u32("TrackID").unwrap_or(0);
        for e in track.children().filter(|e| e.name() == "TrackElement") {
            let field = |name: &str| e.u32(name).unwrap_or(0);
            let what = match field("Type") {
                0 => Play::Animation {
                    model: field("ModelID"),
                    animation: field("AnimationID"),
                },
                1 => Play::Particle(field("ParticleID")),
                2 => Play::Condition(field("Condition")),
                3 => Play::Sound(Sound {
                    guid: field("GUID"),
                    looped: field("Looped") != 0,
                    model: field("ModelID"),
                    bone: field("BoneID"),
                }),
                k => Play::Other(k),
            };
            clips.push(Clip {
                element: e.id,
                track: id,
                start: num(e, "StartTime") / 1000.,
                end: num(e, "EndTime") / 1000.,
                what,
            });
        }
    }
    Sequence {
        element: c.id,
        id: c.u32("SequenceID").unwrap_or(0),
        speed: c.f32("Speed").unwrap_or(1.),
        looped: c.text("Looped") != "0",
        disable_effects: c.u32("DisableEffects").is_some_and(|v| v != 0),
        restart_effects: c.u32("RestartEffects").is_some_and(|v| v != 0),
        clips,
    }
}

impl Model {
    /// Where an entry hung on `link` stands in the bind pose: the host
    /// part's placement times the joint's bind pose. Its own placement goes
    /// after this, in the joint's space. `None` when the host has no
    /// loaded skeleton or no such joint.
    pub fn joint_placement(&self, link: BoneLink) -> Option<Transform> {
        let host = self.parts.get(link.model as usize)?;
        let skeleton = host.mesh.as_ref()?.skeleton.as_ref()?;
        let bind = skeleton.bind[skeleton.find_hash(link.bone)?];
        Some(host.transform.then(&Transform {
            position: bind.translation,
            rotation: bind.rotation,
            scale: [1.; 3],
        }))
    }

    /// A model with `main`'s bounds and particles, which every `tags`
    /// dialect stores alike.
    fn placed(main: Node, layout: &Layout) -> Model {
        let mut model = Model {
            mesh_center: main.vec3(layout.center).unwrap_or_default(),
            mesh_extent: main.vec3(layout.extent).unwrap_or_default(),
            ..Model::default()
        };
        for c in main.list(layout.particles) {
            model.particles.push(Particle {
                element: c.id,
                file: normalize_path(c.text("FileName")).into_owned(),
                transform: transformer(c).0,
                rig: rig(c),
                time_scale: c.f32("TimeScale").filter(|s| *s > 0.).unwrap_or(1.),
                tint: {
                    let [r, g, b] = c.vec3("Color").unwrap_or([1.; 3]);
                    [r, g, b, c.f32("Alpha").unwrap_or(1.)]
                },
                wind_impact: c.f32("WindImpact").unwrap_or(0.),
                sub_images: c.child("SubImageSize").and_then(|s| {
                    let axis = |a| s.f32(a).map(|v| v as u32);
                    let grid = [axis("x")?, axis("y")?, axis("z").unwrap_or(0)];
                    (grid[0] > 0 && grid[1] > 0).then(|| {
                        let count = if grid[2] == 0 {
                            grid[0] * grid[1]
                        } else {
                            grid[2]
                        };
                        [grid[0], grid[1], count]
                    })
                }),
            });
        }
        model
    }

    /// Reads `cfg_path` from the install, loads every part's rdm and checks
    /// that referenced textures and prop cfgs exist. Missing files land in
    /// [`Model::problems`] and their fields become `None`.
    pub fn load(install: &Install, cfg_path: &str) -> Result<Model> {
        Model::load_doc(install, cfg_path, None)
    }

    /// [`Model::load`] of a cfg an editor holds, edits and all, in place of
    /// the file at `cfg_path`; `None` reads the file.
    pub fn load_doc(install: &Install, cfg_path: &str, doc: Option<&Doc>) -> Result<Model> {
        // BRP is the baked prop cache. The matching PRP keeps its full
        // source mesh and materials, which the editor can render directly.
        let prop = cfg_path
            .strip_suffix(".brp")
            .map(|stem| format!("{stem}.prp"));
        let source = prop
            .as_deref()
            .filter(|p| install.exists(p))
            .unwrap_or(cfg_path);
        let read;
        let doc = match doc {
            Some(doc) => doc,
            None => {
                read = Doc::parse(&install.read(source)?);
                &read
            }
        };
        let (mut model, version) =
            Model::parse_versioned(doc).with_context(|| cfg_path.to_string())?;
        model.path = normalize_path(cfg_path).into_owned();
        let mut problems = Vec::new();
        // Shipped files and a mounted mod's: mods add models of their own.
        let exists = |p: &str| install.exists(p);
        // Cfgs name `_lod0`; where that is the coarsest, the model is its
        // last shipped level.
        if version.layout().lod0_coarsest {
            for part in &mut model.parts {
                let Some(stem) = part.rdm.strip_suffix("_lod0.rdm") else {
                    continue;
                };
                let finest = (1..=4)
                    .map(|k| format!("{stem}_lod{k}.rdm"))
                    .take_while(|p| exists(p))
                    .last();
                if let Some(finest) = finest {
                    part.rdm = finest;
                }
            }
        }
        for part in &mut model.parts {
            // Built from the cfg itself (2070's leaf clusters): named after
            // it, as the renderer's mesh cache keys by the name.
            if part.mesh.is_some() {
                part.rdm = format!("{}#{}", model.path, part.name);
                continue;
            }
            if !exists(&part.rdm) {
                problems.push(format!("{}: no mesh {}", part.name, part.rdm));
                continue;
            }
            match install.read(&part.rdm).and_then(|b| Rdm::parse(&b)) {
                Ok(mesh) => part.mesh = Some(mesh),
                Err(e) => problems.push(format!("{}: {e:#}", part.rdm)),
            }
        }
        // PRP sources have no cfg MeshCenter/MeshExtent. Placement tools
        // still need their footprint, in the same frame as the renderer.
        if model.mesh_extent == [0.; 3] {
            let mut bounds: Option<([f32; 3], [f32; 3])> = None;
            for part in &model.parts {
                for p in part.mesh.iter().flat_map(|m| &m.positions) {
                    let p = part.transform.apply(*p);
                    bounds = Some(match bounds {
                        None => (p, p),
                        Some((lo, hi)) => (
                            std::array::from_fn(|i| lo[i].min(p[i])),
                            std::array::from_fn(|i| hi[i].max(p[i])),
                        ),
                    });
                }
            }
            if let Some((lo, hi)) = bounds {
                model.mesh_center = std::array::from_fn(|i| (lo[i] + hi[i]) / 2.);
                model.mesh_extent = std::array::from_fn(|i| (hi[i] - lo[i]) / 2.);
            }
        }
        let materials = model
            .parts
            .iter_mut()
            .flat_map(|p| p.materials.iter_mut())
            .chain(model.decals.iter_mut().filter_map(|d| d.material.as_mut()));
        for m in materials {
            let alpha = m.mesh_decal.as_mut().map(|d| &mut d.alpha_map);
            let night = m.glow.as_mut().map(|g| &mut g.night_map);
            let own = match &mut m.shader {
                Shader::ModelFx(_, o) => vec![&mut o.mask, &mut o.ripples, &mut o.height],
                Shader::Id(_) => Vec::new(),
            };
            for tex in [
                &mut m.diffuse,
                &mut m.normal,
                &mut m.metallic,
                &mut m.dye_mask,
            ]
            .into_iter()
            .chain(alpha)
            .chain(night)
            .chain(own)
            {
                if let Some(p) = tex.take_if(|p| !exists(p)) {
                    problems.push(format!("{}: no texture {p}", m.name));
                }
            }
        }
        let splats = (model.parts.iter_mut())
            .flat_map(|p| p.materials.iter_mut())
            .filter(|m| m.splat.is_some());
        for m in splats {
            let name = m.name.clone();
            let splat = m.splat.as_mut().expect("filtered");
            for layer in &mut splat.layers {
                if let Some(p) = layer.take_if(|l| !exists(&l.diffuse)) {
                    problems.push(format!("{name}: no splat texture {}", p.diffuse));
                }
                let normal = layer.as_mut().map(|l| &mut l.normal);
                if let Some(p) = normal.and_then(|n| n.take_if(|n| !exists(n))) {
                    problems.push(format!("{name}: no splat texture {p}"));
                }
            }
            if !exists(&splat.map) || splat.layers.iter().all(Option::is_none) {
                problems.push(format!("{name}: no splat map {}", splat.map));
                m.splat = None;
            }
        }
        for d in &mut model.decals {
            if let Some(p) = d.height_map.take_if(|p| !exists(p)) {
                problems.push(format!("decal: no height map {p}"));
            }
        }
        for p in &model.props {
            if !exists(&p.cfg) {
                problems.push(format!("{}: no prop cfg {}", p.name, p.cfg));
            }
        }
        model.problems = problems;
        Ok(model)
    }
}

/// Anno 2070's cfg, read without its `m_` prefixes (`Doc::prefix`): the same
/// Models, Files and Transformer shape as the later games, with its own
/// material fields and a flat terrain decal. Effects come back as
/// particles naming their `.efc`; lights and sequences are not read.
fn parse_m_config(main: Node) -> Model {
    let layout = Version::MConfig.layout();
    let mut model = Model::placed(main, layout);
    for c in main.list("Models") {
        if c.config_type() != Some("MODEL") {
            continue;
        }
        let (transform, bone_linked) = transformer(c);
        // Already the shipped `.rdm`; parts have no names of their own.
        let rdm = normalize_path(c.text("FileName")).into_owned();
        let file = rdm.rsplit('/').next().unwrap_or(&rdm);
        model.parts.push(Part {
            element: c.id,
            name: file.trim_end_matches(".rdm").to_owned(),
            materials: c.list("Materials").map(m_config_material).collect(),
            mesh: None,
            transform,
            bone_linked,
            deform_to_terrain: false,
            animations: Vec::new(),
            animation_scales: Vec::new(),
            animation_loops: Vec::new(),
            rig: rig(c),
            rdm,
        });
    }
    // Trees and plants: their stems as instanced models, the same shape.
    for c in main.list("InstancingModels") {
        if c.config_type() != Some("INSTANCING_MODEL") {
            continue;
        }
        let rdm = normalize_path(c.text("InstancingModel")).into_owned();
        let file = rdm.rsplit('/').next().unwrap_or(&rdm);
        model.parts.push(Part {
            element: c.id,
            name: file.trim_end_matches(".rdm").to_owned(),
            materials: c.list("Materials").map(instanced_material).collect(),
            mesh: None,
            transform: transformer(c).0,
            bone_linked: false,
            deform_to_terrain: false,
            animations: Vec::new(),
            animation_scales: Vec::new(),
            animation_loops: Vec::new(),
            rig: Rig::default(),
            rdm,
        });
    }
    for (k, c) in main.list("InstancingBBClusters").enumerate() {
        if c.config_type() == Some("INSTANCING_BBCLUSTER") {
            model.parts.extend(leaves(c, k));
        }
    }
    for c in main.list("Files") {
        let file = c.text("FileName");
        if file.is_empty() {
            continue;
        }
        let cfg = normalize_path(file).into_owned();
        model.props.push(Prop {
            element: c.id,
            name: cfg.rsplit('/').next().unwrap_or(&cfg).to_owned(),
            cfg,
            transform: transformer(c).0,
            deform_to_terrain: false,
            adapt_terrain_height: false,
            rig: rig(c),
        });
    }
    // `Extend` is the half size along X and Z, as the `.ifo` footprint
    // beside it is (bakery: ±2 × ±1.5 m both).
    for c in main.list(layout.decals) {
        model.decals.push(Decal {
            element: c.id,
            transform: transformer(c).0,
            extents: [
                c.f32("Extend.x").unwrap_or(1.0),
                1.0,
                c.f32("Extend.y").unwrap_or(1.0),
            ],
            uv_rect: c.vec4("TexCoords").unwrap_or([0.0, 0.0, 1.0, 1.0]),
            kind: "Terrain".to_owned(),
            material: c.list("Materials").next().map(m_config_material),
            opacity: 1.0,
            fade: None,
            height_map: None,
            uv_transform: None,
        });
    }
    model
}

/// A tree's leaf cluster (`INSTANCING_BBCLUSTER`) as a part with its mesh
/// built here: the game draws each of its `BBClusterSize` billboards as a
/// camera-facing square, `BBSize<k>` across at `BBPos<k>`, showing the
/// rect `BBTex<k>` (u0, v0, u1, v1) of the cluster's leaf texture, tinted
/// by one of its colours. A mesh can't face the camera, so each square
/// stands as two crossed vertical quads, both ways round, normals pointing
/// out from the crown's middle as a canopy's light falls, tinted by the
/// colours' mean, cut out by the texture's alpha. Each board's eight
/// vertices follow each other, the quad across X first, and the material's
/// `vertex_format` is [`Material::BILLBOARDS`], so a renderer with a vertex stage
/// (anno-render's 2070 leaf family) turns the first quad to the camera
/// instead. `rdm` is left empty for `Model::load` to name after the cfg.
fn leaves(c: Node, k: usize) -> Option<Part> {
    let count = c.f32("BBClusterSize")? as usize;
    let field = |name: &str, i: usize| c.f32(&format!("{name}{i}"));
    let boards: Vec<([f32; 3], f32, [f32; 4])> = (0..count)
        .filter_map(|i| {
            let at = [
                field("BBPosX", i)?,
                field("BBPosY", i)?,
                field("BBPosZ", i)?,
            ];
            let tex = ["X", "Y", "Z", "W"].map(|a| field(&format!("BBTex{a}"), i));
            Some((
                at,
                field("BBSize", i)?,
                [tex[0]?, tex[1]?, tex[2]?, tex[3]?],
            ))
        })
        .collect();
    if boards.is_empty() {
        return None;
    }
    let mid = boards.iter().fold([0.; 3], |m, (p, _, _)| {
        [0, 1, 2].map(|a| m[a] + p[a] / boards.len() as f32)
    });
    let mut mesh = Rdm::default();
    for (p, size, [u0, v0, u1, v1]) in &boards {
        let h = size / 2.;
        let out = [0, 1, 2].map(|a| p[a] - mid[a]);
        let len = (out.iter().map(|v| v * v).sum::<f32>()).sqrt();
        let normal = if len > 1e-4 {
            out.map(|v| v / len)
        } else {
            [0., 1., 0.]
        };
        for across in [[h, 0., 0.], [0., 0., h]] {
            let first = mesh.positions.len() as u32;
            for (side, up, uv) in [
                (-1., 1., [*u0, *v0]),
                (1., 1., [*u1, *v0]),
                (1., -1., [*u1, *v1]),
                (-1., -1., [*u0, *v1]),
            ] {
                mesh.positions.push(
                    [0, 1, 2].map(|a| p[a] + across[a] * side + if a == 1 { h * up } else { 0. }),
                );
                mesh.normals.push(normal);
                mesh.uvs.push(uv);
            }
            mesh.indices
                .extend([0, 1, 2, 0, 2, 3, 0, 2, 1, 0, 3, 2].map(|i| first + i));
        }
    }
    mesh.groups.push(crate::rdm::MaterialGroup {
        material: 0,
        start: 0,
        count: mesh.indices.len() as u32,
    });
    mesh.bounds = mesh.computed_bounds();
    let colors = c.f32("ColorCount").unwrap_or(0.) as usize;
    let tints: Vec<[f32; 3]> = (0..colors)
        .filter_map(|i| {
            Some([
                field("ColorR", i)?,
                field("ColorG", i)?,
                field("ColorB", i)?,
            ])
        })
        .collect();
    let mut material = c.list("Materials").next().map(instanced_material)?;
    if !tints.is_empty() {
        material.diffuse_color = tints.iter().fold([0.; 3], |m, t| {
            [0, 1, 2].map(|a| m[a] + t[a] / tints.len() as f32)
        });
    }
    // REF_ALPHA, as the instancing shaders test it.
    material.alpha = Alpha::Cutout(50. / 255.);
    material.double_sided = true;
    material.vertex_format = Material::BILLBOARDS.into();
    Some(Part {
        element: c.id,
        name: format!("leaves{k}"),
        materials: vec![material],
        mesh: Some(mesh),
        transform: transformer(c).0,
        bone_linked: false,
        deform_to_terrain: false,
        animations: Vec::new(),
        animation_scales: Vec::new(),
        animation_loops: Vec::new(),
        rig: Rig::default(),
        rdm: String::new(),
    })
}

/// A 2070 material on an instanced model or leaf cluster: those draw with
/// the instancing shaders (instancingbase.fxh), which read no glow or dye
/// whatever the `ShaderIndex`.
fn instanced_material(c: Node) -> Material {
    let mut m = m_config_material(c);
    m.glow = None;
    if let Shader::ModelFx(_, own) = &mut m.shader {
        own.glow = false;
        own.dye = false;
    }
    m
}

/// A 2070 material. `ShaderIndex` picks a slot in Anno5's model shader
/// table (`model_glow.fx` for 0 and 10, the others below by their fx).
fn m_config_material(c: Node) -> Material {
    let layout = Version::MConfig.layout();
    let shader = c.f32(layout.shader).unwrap_or(layout.shader_default) as u32;
    let tex = |flag: &str, field: &str| {
        let src = c.text(field);
        (c.text(flag) != "0" && !src.is_empty()).then(|| texture_path(src))
    };
    let glow = layout.glow_shaders.contains(&shader);
    let dye = layout.dye_shaders.contains(&shader);
    let reflect = c.text("EnvironmentEnabled") == "1";
    Material {
        name: c.text("Name").to_string(),
        vertex_format: c.text("VertexFormat").to_string(),
        diffuse: tex("DiffuseEnabled", "DiffuseTexture"),
        // Normal XY in red and green, glow in blue and specular strength in
        // alpha (model.fxh): the later games' layout, gloss for specular.
        normal: tex("NormalEnabled", "NormalTexture"),
        // Mask red is how much of the environment map a texel reflects, read
        // only under EnvironmentEnabled; metal is the renderer's nearest
        // term. The mask's green is player colour (model_dye.fx), not drawn.
        // Its alpha reads as occlusion: full in all but one shipped mask, a
        // street decal's.
        metallic: tex("EnvironmentEnabled", "MaskTexture"),
        // The model shaders never read `DiffuseColor`.
        diffuse_color: [1.0; 3],
        double_sided: false,
        terrain_tint: 0.0,
        alpha: layout.alpha(shader, c),
        terrain_adapt: None,
        mesh_decal: None,
        world_tiling: None,
        // The mask's green, not the later games' red: `ModelFx::dye`.
        dye_mask: None,
        // The normal map's blue under `MODEL_GLOW`, lit toward the albedo:
        // the look's deferred glow, at 2070's GLOW_BASE.
        glow: (glow && c.text("NormalEnabled") != "0").then_some(Glow {
            color: [1.; 3],
            night_map: None,
            interior: None,
        }),
        splat: None,
        specular: Specular::Full,
        shader: Shader::ModelFx(
            shader,
            ModelFx {
                glow,
                dye,
                mask: (dye || reflect)
                    .then(|| c.text("MaskTexture"))
                    .filter(|m| !m.is_empty())
                    .map(texture_path),
                reflect,
                ripples: tex("RipplesEnabled", "RipplesTexture"),
                height: tex("ParallaxEnabled", "HeightTexture"),
                foam: layout.foam_shaders.contains(&shader),
            },
        ),
    }
}

// Placement, and whether a bone carries the part, from a `<Transformer>`
// list. Several ORIENTATION_TRANSFORMs compose in file order (outer first);
// the rest (animation, visibility, color, day/night, damage, ...) are
// state-driven and ignored: the static pose is a valid preview.
fn transformer(c: Node) -> (Transform, bool) {
    let mut t = Transform::IDENTITY;
    let mut bone = false;
    for x in c.list("Transformer") {
        match x.config_type() {
            Some("ORIENTATION_TRANSFORM") => {
                let s = x.f32("Scale").unwrap_or(1.0);
                let n = x.vec3("NonUniformScale").unwrap_or([1.0; 3]);
                let local = Transform {
                    position: x.vec3("Position").unwrap_or_default(),
                    rotation: x.vec4("Rotation").unwrap_or([0.0, 0.0, 0.0, 1.0]),
                    scale: n.map(|v| v * s),
                };
                t = t.then(&local);
            }
            Some("BONELINK_TRANSFORM") => bone = true,
            _ => {}
        }
    }
    (t, bone)
}

// The joint an entry hangs on and its own motion, from its `<Transformer>`.
// A BONELINK without its fields (159 shipped, conditions only) links to
// nothing; an all-zero ANIMATION_TRANSFORM (the exporter's default on
// units) does not move.
fn rig(c: Node) -> Rig {
    let mut rig = Rig::default();
    for x in c.list("Transformer") {
        match x.config_type() {
            Some("BONELINK_TRANSFORM") => {
                rig.link = (x.u32("ModelID").zip(x.u32("BoneHash")))
                    .filter(|&(_, h)| h != u32::MAX)
                    .map(|(model, bone)| BoneLink { model, bone });
            }
            Some("VISIBILITY_TRANSFORM") => {
                rig.shown_if = x.u32("Conditions").filter(|&c| c != 0);
            }
            Some("COLOR_TRANSFORM") => {
                let mut c = ColorStates {
                    conditions: x.u32("Conditions").unwrap_or(0),
                    start_invisible: x.text("StartInvisible") == "1",
                    states: [ColorState::default(); ColorStates::MAX],
                    count: 0,
                };
                let states = x.children().filter(|s| s.name() == "State");
                for (slot, s) in c.states.iter_mut().zip(states) {
                    let one = |n: &str| s.f32(n).unwrap_or(1.);
                    *slot = ColorState {
                        color: [
                            one("Color.r"),
                            one("Color.g"),
                            one("Color.b"),
                            one("Color.a"),
                        ],
                        time: s.f32("Time").unwrap_or(0.),
                        condition: s.u32("Condition").unwrap_or(0),
                    };
                    c.count += 1;
                }
                rig.color = Some(c);
            }
            Some("DAMAGE_TRANSFORM") => {
                let num = |name: &str, or: f32| x.f32(name).unwrap_or(or);
                rig.damage = Some(Damage {
                    damage: [num("StartDamage", 0.), num("EndDamage", 255.)],
                    scale: [num("StartScale", 1.), num("EndScale", 1.)],
                });
            }
            Some("ANIMATION_TRANSFORM") => {
                let num = |name: &str| x.f32(name).unwrap_or(0.);
                let motion = Motion {
                    offset: x.vec3("PositionOffset").unwrap_or_default(),
                    offset_speed: num("PositionSpeed"),
                    offset_turns: x.text("PositionRotate") == "1",
                    rotation: x.vec3("Rotation").unwrap_or_default(),
                    rotation_speed: num("RotationSpeed"),
                    oscillation: num("Oscillation"),
                    alignment: Alignment::from_byte(x.u32("Alignment").unwrap_or(0)),
                    // The record's defaults where a field is left out.
                    spring: (x.text("SpringEnabled") == "1").then(|| Spring {
                        min: x.f32("SpringMin").unwrap_or(-0.1),
                        max: x.f32("SpringMax").unwrap_or(0.1),
                        offset: x.f32("SpringOffset").unwrap_or(0.1),
                    }),
                };
                // The game skips each half when its vector is zero.
                let moves = motion.offset != [0.; 3]
                    || motion.rotation != [0.; 3]
                    || motion.alignment != Alignment::None
                    || motion.spring.is_some();
                rig.motion = moves.then_some(motion);
            }
            _ => {}
        }
    }
    rig
}

/// The placement an entry's `<Transformer>` gives it, as [`Model`] reads
/// it: a part's, prop's, decal's, particle's or light's.
pub fn placement(doc: &Doc, entry: Id) -> Transform {
    transformer(Node { doc, id: entry }).0
}

/// Writes `t` as an entry's placement, into its first
/// ORIENTATION_TRANSFORM, made when it has none. Later ones (149 shipped
/// entries carry two) are removed: `t` replaces what they composed to.
/// Fields that already read as `t` keep their text, so a move rewrites
/// only the position. A uniform scale goes in `Scale`; an uneven one in
/// `NonUniformScale`, over the `Scale` the entry had.
pub fn set_placement(doc: &mut Doc, entry: Id, t: &Transform) {
    const ORIENTATION: &str = "ORIENTATION_TRANSFORM";
    let list = match doc.child(entry, "Transformer") {
        Some(list) => list,
        None => doc.insert(entry, None, "<Transformer></Transformer>"),
    };
    let is_orientation = |doc: &Doc, c: Id| {
        doc.name(c) == "Config"
            && doc
                .child(c, "ConfigType")
                .is_some_and(|k| doc.text(k) == ORIENTATION)
    };
    let mut found = doc.children(list).filter(|&c| is_orientation(doc, c));
    let (first, later): (Option<Id>, Vec<Id>) = (found.next(), found.collect());
    for c in later {
        doc.remove(c);
    }
    let config = match first {
        Some(c) => c,
        None => doc.insert(
            list,
            None,
            &format!("<Config><ConfigType>{ORIENTATION}</ConfigType></Config>"),
        ),
    };
    // A vector's fields go together: the reader takes none of them unless
    // all are there. Each is written unless it already reads as its value;
    // a vector that is missing whole stays so where its default is right.
    let group = |doc: &mut Doc, names: &[String], values: &[f32], defaults: &[f32]| {
        let same = |a: f32, v: f32| (a - v).abs() <= 1e-6 * v.abs().max(1.);
        let old: Vec<Option<f32>> = (names.iter())
            .map(|n| doc.child(config, n).and_then(|c| doc.text(c).parse().ok()))
            .collect();
        let missing = old.iter().all(Option::is_none);
        if missing && values.iter().zip(defaults).all(|(&v, &d)| same(v, d)) {
            return;
        }
        for ((name, &v), old) in names.iter().zip(values).zip(old) {
            if !old.is_some_and(|a| same(a, v)) {
                doc.set_child_text(config, name, &v.to_string());
            }
        }
    };
    let names = |prefix: &str, axes: &[&str]| -> Vec<String> {
        axes.iter().map(|a| format!("{prefix}.{a}")).collect()
    };
    group(
        doc,
        &names("Position", &["x", "y", "z"]),
        &t.position,
        &[0.; 3],
    );
    group(
        doc,
        &names("Rotation", &["x", "y", "z", "w"]),
        &t.rotation,
        &[0., 0., 0., 1.],
    );
    let [x, y, z] = t.scale;
    let even = |a: f32, b: f32| (a - b).abs() <= 1e-6 * a.abs().max(1.);
    let scale = match even(x, y) && even(x, z) {
        true => x,
        false => (doc.child(config, "Scale"))
            .and_then(|c| doc.text(c).parse::<f32>().ok())
            .filter(|s| *s != 0.)
            .unwrap_or(1.),
    };
    group(doc, &["Scale".to_owned()], &[scale], &[1.]);
    group(
        doc,
        &names("NonUniformScale", &["x", "y", "z"]),
        &t.scale.map(|v| v / scale),
        &[1.; 3],
    );
}

/// Hangs an entry on a joint, or takes it off with `None`: its
/// BONELINK_TRANSFORM written before its ORIENTATION_TRANSFORM, the order
/// shipped cfgs keep, or removed. The placement is left as it is; whoever
/// links decides whether it means joint space now.
pub fn set_bone_link(doc: &mut Doc, entry: Id, link: Option<BoneLink>) {
    const BONELINK: &str = "BONELINK_TRANSFORM";
    let list = match doc.child(entry, "Transformer") {
        Some(list) => list,
        None if link.is_none() => return,
        None => doc.insert(entry, None, "<Transformer></Transformer>"),
    };
    let kind = |doc: &Doc, c: Id| doc.child(c, "ConfigType").map(|k| doc.text(k).to_owned());
    let found = doc
        .children(list)
        .find(|&c| kind(doc, c).as_deref() == Some(BONELINK));
    let Some(link) = link else {
        if let Some(c) = found {
            doc.remove(c);
        }
        return;
    };
    let config = match found {
        Some(c) => c,
        None => {
            let before = (doc.children(list))
                .find(|&c| kind(doc, c).as_deref() == Some("ORIENTATION_TRANSFORM"));
            let xml = format!("<Config><ConfigType>{BONELINK}</ConfigType></Config>");
            doc.insert(list, before, &xml)
        }
    };
    doc.set_child_text(config, "ModelID", &link.model.to_string());
    doc.set_child_text(config, "SkeletonID", "0");
    doc.set_child_text(config, "BoneHash", &link.bone.to_string());
}

/// 117's `MATERIAL_ALPHA_REF`, which the shaders do not define: the engine
/// passes it to its shader compiler as a float, 0.3 in build 26607411.
/// Unit skins that crowds draw with the PBR shader keep 0.36 to 0.46 in
/// alpha and must stay visible, so the reference cannot be the common 0.5.
/// 1800's model_pbr.fx tests too, at `ALPHA_REF` 0.3 (model_materials.fxh)
/// but on raw alpha, without the smoothstep: through this reference its
/// cut lands at 0.327 instead of 0.3, which its mostly two-level alphas do
/// not notice.
const MATERIAL_ALPHA_REF: f32 = 0.3;

/// A `<Config>` or prp material.
fn material(c: Node, layout: &Layout) -> Material {
    let shader = c.f32(layout.shader).unwrap_or(layout.shader_default) as u32;
    let on = |flag: &str| c.text(flag) != "0";
    let tex = |flag: &str, field: &str| {
        let src = match (field, c.text(field)) {
            ("cModelDiffTex", "") => c.text("cPropDiffuseTex"),
            ("cModelNormalTex", "") => c.text("cPropNormalTex"),
            _ => c.text(field),
        };
        (on(flag) && !src.is_empty()).then(|| texture_path(src))
    };
    let alpha = layout.alpha(shader, c);
    Material {
        name: c.text("Name").to_string(),
        shader: Shader::Id(shader),
        vertex_format: c.text("VertexFormat").to_string(),
        diffuse: tex("DIFFUSE_ENABLED", "cModelDiffTex"),
        normal: tex("NORMAL_ENABLED", "cModelNormalTex"),
        metallic: tex("METALLIC_TEX_ENABLED", "cModelMetallicTex"),
        diffuse_color: c.vec3_rgb("cDiffuseColor").unwrap_or([1.0; 3]),
        double_sided: c.text("DOUBLE_SIDED") == "1",
        terrain_tint: if c.text("cUseTerrainTinting") == "1" {
            c.f32("cTerrainTintIntensity")
                .unwrap_or(1.0)
                .clamp(0.0, 1.0)
        } else {
            0.0
        },
        alpha,
        splat: splat(c),
        specular: if matches!(shader, 21..=23) {
            Specular::MatteFades
        } else {
            Specular::Full
        },
        world_tiling: (c.text("cEnableWorldSpaceTiling") == "1").then(|| WorldTiling {
            scale: c.f32("cWorldSpaceTilingScale").unwrap_or(1.0).max(0.01),
            diagonal: c.text("cEnableAQTiling") == "1",
        }),
        mesh_decal: (shader == 6).then(|| MeshDecal {
            opacity: c.f32("cOpacity").unwrap_or(1.0).clamp(0.0, 1.0),
            alpha_map: (c.text("ALPHA_TEXTURE_ENABLED") == "1" && !c.text("cAlphaTex").is_empty())
                .then(|| texture_path(c.text("cAlphaTex"))),
            bias: match (c.text("cDisableDepthBias"), c.text("cHigherDepthBias")) {
                ("1", _) => 0.,
                (_, "1") => 0.2,
                _ => 0.01,
            },
            before_decals: c.text("RENDER_BEFORE_DECALS") == "1",
        }),
        terrain_adapt: (c.text("ADJUST_TO_TERRAIN_HEIGHT") == "1"
            || c.text("cAdjustToTerrainHeight") == "1")
            .then(|| TerrainAdapt {
                absolute: c.text("ABSOLUTE_TERRAIN_ADAPTION") == "1",
                vertex_colored: c.text("VERTEX_COLORED_TERRAIN_ADAPTION") == "1",
                local_pivots: c.text("VERTEX_COLORED_LOCAL_PIVOTS") == "1",
            }),
        // Switches the shaders default on or that some (glass) leave out
        // count only when the cfg sets them: a missing GLOW_ENABLED must
        // not light a normal map whose blue is something else (1800's
        // glow-off normal maps average 0.33 blue).
        dye_mask: (c.text("DYE_MASK_ENABLED") == "1" && c.text("MULTI_DYE_ENABLED") != "1")
            .then(|| c.text("cDyeMask"))
            .filter(|t| !t.is_empty())
            .map(texture_path),
        glow: (c.text("GLOW_ENABLED") == "1").then(|| {
            let night_map = (c.text("NIGHT_GLOW_ENABLED") == "1")
                .then(|| c.text("cNightGlowMap"))
                .filter(|t| !t.is_empty())
                .map(texture_path);
            Glow {
                color: c.vec3_rgb("cEmissiveColor").unwrap_or([2.0; 3]),
                interior: (night_map.is_some() && c.text("WINDOW_INTERIOR_ENABLED") == "1").then(
                    || Interior {
                        distance: c.f32("cWindowInteriorDistance").unwrap_or(0.2),
                        scale: c.f32("cWindowInteriorScale").unwrap_or(2.0),
                        offset: [
                            c.f32("cWindowInteriorOffset.x").unwrap_or(0.0),
                            c.f32("cWindowInteriorOffset.y").unwrap_or(0.0),
                        ],
                    },
                ),
                night_map,
            }
        }),
    }
}

/// A material's [`Splat`], when `SPLAT_MAP_ENABLED` and it names a map.
fn splat(c: Node) -> Option<Splat> {
    let map = c.text("cModelSplatMap");
    if c.text("SPLAT_MAP_ENABLED") != "1" || map.is_empty() {
        return None;
    }
    let layer = |(channel, axis): (&str, &str)| {
        let diffuse = c.text(&format!("cModelSplatDiffTex{channel}"));
        if c.text(&format!("SPLAT_{}_ENABLED", channel_name(channel))) != "1" || diffuse.is_empty()
        {
            return None;
        }
        let normal = c.text(&format!("cModelSplatNormalTex{channel}"));
        Some(SplatLayer {
            diffuse: texture_path(diffuse),
            normal: (!normal.is_empty()).then(|| texture_path(normal)),
            tiling: (c.f32(&format!("cSplatTiling.{axis}")).unwrap_or(1.0)).max(1e-3),
        })
    };
    fn channel_name(channel: &str) -> &'static str {
        match channel {
            "R" => "RED",
            "G" => "GREEN",
            "B" => "BLUE",
            _ => "ALPHA",
        }
    }
    // The fxm's defaults for what a cfg leaves out.
    let on = |name: &str, default: bool| match c.text(name) {
        "" => default,
        v => v == "1",
    };
    Some(Splat {
        map: texture_path(map),
        layers: [("R", "x"), ("G", "y"), ("B", "z"), ("A", "w")].map(layer),
        mix_albedo: on("cSplatMixAlbedo", false),
        mix_normal: on("cSplatMixNormal", false),
        scale_invariant: on("SPLAT_SCALE_INVARIANT", false),
        world_tiling: on("cSplatWorldTiling", false),
    })
}

/// Shipped mesh for a cfg `FileName`: the exporter's `.fbx` (or `.max`)
/// becomes `.rdm` next to it.
pub fn mesh_path(source: &str) -> String {
    let p = normalize_path(source);
    match p.rsplit_once('.') {
        Some((stem, _)) if !stem.ends_with('/') => format!("{stem}.rdm"),
        _ => p.into_owned(),
    }
}

/// Shipped texture for a cfg texture field: `maps/x_diff.psd` (or `.png`,
/// `.tga`) ships as `maps/x_diff_0.dds`, the `_0` being the top mip chain.
pub fn texture_path(source: &str) -> String {
    let p = normalize_path(source);
    match p.rsplit_once('.') {
        Some((_, "dds")) => p.into_owned(),
        Some((stem, _)) => format!("{stem}_0.dds"),
        None => format!("{p}_0.dds"),
    }
}

// A read view of one cfg element. Names come without the file's prefix,
// so 2070's `m_Transformer` reads as the later games' `Transformer`.
#[derive(Clone, Copy)]
struct Node<'a> {
    doc: &'a Doc,
    id: Id,
}

impl<'a> Node<'a> {
    fn root(doc: &'a Doc) -> Node<'a> {
        Node { doc, id: Id::ROOT }
    }

    fn name(self) -> &'a str {
        self.doc.name(self.id)
    }

    fn own_text(self) -> &'a str {
        self.doc.text(self.id)
    }

    fn children(self) -> impl Iterator<Item = Node<'a>> + use<'a> {
        (self.doc.children(self.id)).map(move |id| Node { id, ..self })
    }

    fn child(self, name: &str) -> Option<Node<'a>> {
        self.children().find(|c| c.name() == name)
    }

    fn configs(self) -> impl Iterator<Item = Node<'a>> + use<'a> {
        self.children().filter(|c| c.name() == "Config")
    }

    // `<name><Config>…</Config><Config>…</Config></name>`
    fn list(self, name: &str) -> impl Iterator<Item = Node<'a>> + use<'a> {
        self.child(name).into_iter().flat_map(Node::configs)
    }

    fn config_type(self) -> Option<&'a str> {
        self.child("ConfigType").map(Node::own_text)
    }

    fn text(self, name: &str) -> &'a str {
        self.child(name).map_or("", Node::own_text)
    }

    fn f32(self, name: &str) -> Option<f32> {
        self.child(name)?.own_text().parse().ok()
    }

    // Ids and hashes pass f32's 24 bits (SequenceID 100691457).
    fn u32(self, name: &str) -> Option<u32> {
        self.child(name)?.own_text().parse().ok()
    }

    fn fields<const N: usize>(self, prefix: &str, axes: [&str; N]) -> Option<[f32; N]> {
        let mut out = [0.0; N];
        for (o, axis) in out.iter_mut().zip(axes) {
            *o = self.f32(&format!("{prefix}.{axis}"))?;
        }
        Some(out)
    }

    // `Prefix.x`, `Prefix.y`, `Prefix.z` siblings.
    fn vec3(self, prefix: &str) -> Option<[f32; 3]> {
        self.fields(prefix, ["x", "y", "z"])
    }

    fn vec4(self, prefix: &str) -> Option<[f32; 4]> {
        self.fields(prefix, ["x", "y", "z", "w"])
    }

    fn vec3_rgb(self, prefix: &str) -> Option<[f32; 3]> {
        self.fields(prefix, ["r", "g", "b"])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vfs::FileId;

    // 1800's airship: its burning oil (particle 3) starts invisible and
    // fades in over 5 s while condition 128 is up, out over 1 s on 32768;
    // its damage smoke (particle 2) has scale 0 below 100 damage and is
    // clear unless 32 or 64 is up.
    #[test]
    fn color_and_damage_transforms_read_onto_the_rig() {
        let state = |a: u32, time: u32, condition: u32| {
            format!(
                "<State><Color.r>1</Color.r><Color.g>1</Color.g><Color.b>1</Color.b><Color.a>{a}</Color.a>\
                 <Time>{time}</Time><Condition>{condition}</Condition><Multiply>0</Multiply></State>"
            )
        };
        let particle = |transformers: String| {
            format!(
                "<Config><ConfigType>PARTICLE</ConfigType><FileName>data/fire.rdp</FileName>\
                 <Transformer>{transformers}</Transformer></Config>"
            )
        };
        let oil = format!(
            "<Config><ConfigType>COLOR_TRANSFORM</ConfigType><Conditions>32896</Conditions>\
             <ApplyAlways>1</ApplyAlways><StartInvisible>1</StartInvisible>{}{}</Config>",
            state(1, 5, 128),
            state(0, 1, 32768)
        );
        let smoke = format!(
            "<Config><ConfigType>DAMAGE_TRANSFORM</ConfigType><Conditions>0</Conditions>\
             <StartDamage>100</StartDamage><EndDamage>190</EndDamage><StartScale>0.000000</StartScale>\
             <EndScale>1.500000</EndScale></Config>\
             <Config><ConfigType>COLOR_TRANSFORM</ConfigType><Conditions>96</Conditions>\
             <ApplyAlways>1</ApplyAlways><StartInvisible>0</StartInvisible>{}{}{}</Config>",
            state(1, 2, 32),
            state(0, 0, 0),
            state(1, 2, 64)
        );
        let xml = format!(
            "<Config><ConfigType>MAIN</ConfigType><Particles>{}{}</Particles></Config>",
            particle(oil),
            particle(smoke)
        );
        let model = Model::parse(xml.as_bytes()).unwrap();
        let (oil, smoke) = (model.particles[0].rig, model.particles[1].rig);
        let c = oil.color.expect("the oil's colours");
        assert_eq!((c.states().len(), c.start_invisible), (2, true));
        let mut run = ColorRun::new(&c, 0);
        run.step(&c, 0, 10., false);
        assert_eq!(run.color[3], 0., "clear while nothing is raised");
        run.step(&c, 128, 2.5, false);
        assert!((run.color[3] - 0.5).abs() < 1e-6, "{:?}", run.color);
        run.step(&c, 128, 2.5, false);
        assert_eq!(run.color[3], 1.);
        run.step(&c, 32768, 0.5, false);
        assert!((run.color[3] - 0.5).abs() < 1e-6);
        let d = smoke.damage.expect("the smoke's damage");
        assert_eq!(d.scale_at(0.), 0.);
        assert!((d.scale_at(145.) - 0.75).abs() < 1e-6);
        assert_eq!(d.scale_at(255.), 1.5);
        let c = smoke.color.expect("the smoke's colours");
        let mut run = ColorRun::new(&c, 0);
        assert_eq!(run.color[3], 0., "clear by default");
        run.step(&c, 64, 2., false);
        assert_eq!(run.color[3], 1.);
    }

    #[test]
    fn parses_1800_plant_props() {
        let model = Model::parse(br#"<Prop><Type>Plant</><VertexFormat>P4h_N4b_G4b_B4b_T2h_C4b_C4b</><MeshFileName>data/tree_lod0.rdm</><Materials><Material><DIFFUSE_ENABLED>1</><cPropDiffuseTex>data/tree_diff.psd</><cAlphaRef>0.25</><DOUBLE_SIDED>1</></></></>"#).unwrap();
        assert_eq!(model.parts.len(), 1);
        assert_eq!(model.parts[0].rdm, "data/tree_lod0.rdm");
        let material = &model.parts[0].materials[0];
        assert_eq!(material.diffuse.as_deref(), Some("data/tree_diff_0.dds"));
        assert_eq!(material.alpha, Alpha::Cutout(0.25));
        assert!(material.double_sided);
    }

    // A prp's `<Type>` decides how it composites: decals blend onto the
    // ground, a SimplePBR material asking for it blends, the rest is solid.
    #[test]
    fn prp_types_composite_as_the_game_draws_them() {
        let alpha = |kind: &str, extra: &str| {
            let xml = format!(
                "<Prop><Type>{kind}</><MeshFileName>data/a.rdm</><Materials><Material><cPropDiffuseTex>data/a_diff.psd</>{extra}</></></>"
            );
            Model::parse(xml.as_bytes()).unwrap().parts[0].materials[0].alpha
        };
        assert_eq!(alpha("Decal", ""), Alpha::Decal);
        assert_eq!(alpha("Decal Detail", ""), Alpha::Decal);
        assert_eq!(
            alpha("SimplePBR", "<FORCE_ALPHA_BLEND>1</>"),
            Alpha::Blend(1.0)
        );
        assert_eq!(alpha("SimplePBR", "<FORCE_ALPHA_BLEND>0</>"), Alpha::Opaque);
        assert_eq!(alpha("Terrain", ""), Alpha::Opaque);
    }

    // The prop shaders' BRDF defines, and 117's deferred plant tag on
    // shaders 21-23.
    #[test]
    fn materials_keep_the_specular_their_shaders_keep() {
        let prop = |kind: &str| {
            let xml = format!(
                "<Prop><Type>{kind}</><MeshFileName>data/a.rdm</><Materials><Material><cPropDiffuseTex>data/a_diff.psd</></></></>"
            );
            Model::parse(xml.as_bytes()).unwrap().parts[0].materials[0].specular
        };
        assert_eq!(prop("Plant"), Specular::NoSkyMatteFades);
        assert_eq!(prop("Grass"), Specular::NoSun);
        assert_eq!(prop("Terrain"), Specular::MatteFades);
        assert_eq!(prop("Decal Detail"), Specular::MatteFades);
        assert_eq!(prop("Decal"), Specular::Full);
        assert_eq!(prop("SimplePBR"), Specular::Full);
        let cfg = Model::parse(CFG.as_bytes()).unwrap();
        assert_eq!(cfg.parts[0].materials[0].specular, Specular::MatteFades);
        let pbr = CFG.replace("<ShaderID>22<", "<ShaderID>8<");
        let pbr = Model::parse(pbr.as_bytes()).unwrap();
        assert_eq!(pbr.parts[0].materials[0].specular, Specular::Full);
    }

    const CFG: &str = "<Config><ConfigType>MAIN</ConfigType>\r\n\
<MeshCenter.x>0</MeshCenter.x><MeshCenter.y>1</MeshCenter.y><MeshCenter.z>0</MeshCenter.z>\r\n\
<MeshExtent.x>2</MeshExtent.x><MeshExtent.y>1</MeshExtent.y><MeshExtent.z>2</MeshExtent.z>\r\n\
<Models><Config><ConfigType>MODEL</ConfigType>\r\n\
<Transformer><Config><ConfigType>ORIENTATION_TRANSFORM</ConfigType>\
<Position.x>1</Position.x><Position.y>0</Position.y><Position.z>0</Position.z>\
<Rotation.x>0</Rotation.x><Rotation.y>0.7071068</Rotation.y><Rotation.z>0</Rotation.z><Rotation.w>0.7071068</Rotation.w>\
<Scale>2</Scale><NonUniformScale.x>1</NonUniformScale.x><NonUniformScale.y>1</NonUniformScale.y><NonUniformScale.z>1</NonUniformScale.z>\
</Config><Config><ConfigType>ANIMATION_TRANSFORM</ConfigType></Config></Transformer>\r\n\
<Materials><Config><ConfigType>MATERIAL</ConfigType><Name>&lt;Default Material&gt;</Name><ShaderID>22</ShaderID>\
<Common [ModelCommon]>Common [ModelCommon]</Common [ModelCommon]><DIFFUSE_ENABLED>1</DIFFUSE_ENABLED>\
<cModelDiffTex>Data\\Base\\maps\\leaf_diff.psd</cModelDiffTex><NORMAL_ENABLED>0</NORMAL_ENABLED>\
<cModelNormalTex>data/base/maps/leaf_norm.png</cModelNormalTex><DOUBLE_SIDED>1</DOUBLE_SIDED><cUseTerrainTinting>1</cUseTerrainTinting><cTerrainTintIntensity>0.8</cTerrainTintIntensity>\
<cShadowAlphaRef>0.4</cShadowAlphaRef><cDiffuseColor.r>1.2</cDiffuseColor.r><cDiffuseColor.g>1</cDiffuseColor.g>\
<cDiffuseColor.b>1</cDiffuseColor.b></></Materials>\r\n\
<Name>leaf</Name><FileName>data/base/rdm/leaf_lod0.fbx</FileName>\
<AABBMin><x>-1</x><y>0</y><z>-1</z></AABBMin><AABBMax><x>1</x><y>2</y><z>1</z></AABBMax>\
</Config></Models>\r\n\
<Files><Config><ConfigType>FILE</ConfigType><Name>bucket</Name>\
<FileName>data/base/props/bucket.cfg</FileName></Config></Files>\r\n\
</Config>";

    // The dog's shape: a part with its animations, a prop on a hand
    // joint, a swaying barrel, and a sequence playing one animation while
    // it raises a condition.
    #[test]
    fn reads_sequences_bone_links_and_motion() {
        let cfg = "<Config><ConfigType>MAIN</ConfigType>\r\n\
<Models><Config><ConfigType>MODEL</ConfigType><Name>body</Name><FileName>a/body.fbx</FileName>\
<Animations><Config><ConfigType>ANIMATION</ConfigType><FileName>a/anim/idle.rdm</FileName></Config>\
<Config><ConfigType>ANIMATION</ConfigType><FileName>a/anim/walk.fbx</FileName></Config></Animations>\
<Transformer><Config><ConfigType>ANIMATION_TRANSFORM</ConfigType><Rotation.x>0.087266</Rotation.x><Rotation.y>0</Rotation.y>\
<Rotation.z>0.069813</Rotation.z><RotationSpeed>18</RotationSpeed><Oscillation>16</Oscillation></Config></Transformer></Config></Models>\r\n\
<Files><Config><ConfigType>FILE</ConfigType><FileName>a/basket.cfg</FileName><Transformer>\
<Config><ConfigType>BONELINK_TRANSFORM</ConfigType><ModelID>0</ModelID><SkeletonID>0</SkeletonID><BoneHash>3963261774</BoneHash></Config>\
<Config><ConfigType>ANIMATION_TRANSFORM</ConfigType><RotationSpeed>0</RotationSpeed></Config></Transformer></Config></Files>\r\n\
<Sequences><Config><ConfigType>SEQUENCE</ConfigType><SequenceID>100691457</SequenceID><Speed>0.5</Speed><Looped>1</Looped>\
<Track><TrackID>1</TrackID><TrackElement><Type>0</Type><StartTime>0</StartTime><EndTime>5333</EndTime><ModelID>0</ModelID><AnimationID>1</AnimationID></TrackElement></Track>\
<Track><TrackID>2</TrackID><TrackElement><Type>2</Type><StartTime>100</StartTime><EndTime>200</EndTime><Condition>4</Condition></TrackElement></Track>\
<Track><TrackID>3</TrackID><TrackElement><Type>3</Type><StartTime>0</StartTime><EndTime>1</EndTime><GUID>37756</GUID>\
<Looped>1</Looped><ModelID>0</ModelID><SkeletonID>0</SkeletonID><BoneID>2</BoneID></TrackElement></Track>\
</Config></Sequences></Config>";
        let m = Model::parse(cfg.as_bytes()).unwrap();
        assert_eq!(
            m.parts[0].animations,
            ["a/anim/idle.rdm", "a/anim/walk.rdm"]
        );
        let motion = m.parts[0].rig.motion.expect("the barrel sways");
        assert_eq!((motion.rotation_speed, motion.oscillation), (18., 16.));
        // At rest at 0, then rocking ±Oscillation/2 = ±8° about the axis
        // the shipped flotsam's angles give, as the game swings it.
        assert_eq!(motion.at(0.).1, [0., 0., 0., 1.]);
        let widest = (0..400)
            .map(|k| 2. * motion.at(k as f32 * 0.01).1[3].clamp(-1., 1.).acos())
            .fold(0f32, f32::max);
        assert!(
            (widest.to_degrees() - 8.).abs() < 0.1,
            "{}",
            widest.to_degrees()
        );
        // A spin turns by the angles' turn every 1 / RotationSpeed seconds.
        let spin = Motion {
            oscillation: 0.,
            rotation: [0., 20f32.to_radians(), 0.],
            rotation_speed: 1.5,
            ..motion
        };
        let turned = 2. * spin.at(1.).1[3].clamp(-1., 1.).acos();
        assert!(
            (turned.to_degrees() - 30.).abs() < 0.01,
            "{}",
            turned.to_degrees()
        );
        // 30 degrees a second comes round in 12 s, and the swing at its
        // own rate; one that turns nothing a second has no period.
        assert!((spin.period().unwrap() - 12.).abs() < 1e-3);
        let back = spin.at(spin.period().unwrap()).1;
        assert!(back[3].abs() > 0.9999, "{back:?}");
        let swing = motion.period().unwrap();
        assert!(motion.at(swing).1[3] > 0.9999 && motion.at(swing / 4.).1[3] < 0.999);
        let still = Motion {
            rotation_speed: 0.,
            ..motion
        };
        assert_eq!(still.period(), None);
        let link = m.props[0].rig.link.expect("the basket hangs on a joint");
        assert_eq!((link.model, link.bone), (0, 3963261774));
        assert_eq!(
            m.props[0].rig.motion, None,
            "an all-zero motion stays still"
        );
        let s = &m.sequences[0];
        assert_eq!((s.id, s.speed, s.looped), (100691457, 0.5, true));
        assert_eq!(s.clips.len(), 3);
        assert_eq!(
            s.clips[0].what,
            Play::Animation {
                model: 0,
                animation: 1
            }
        );
        assert_eq!((s.clips[1].track, s.clips[1].what), (2, Play::Condition(4)));
        assert_eq!(
            s.clips[2].what,
            Play::Sound(Sound {
                guid: 37756,
                looped: true,
                model: 0,
                bone: 2
            })
        );
        assert!((s.length() - 5.333).abs() < 1e-6);
    }

    // Alignment as the game's `sub_1421C30D0` (build 24988830) turns a part: billboards take
    // the camera's rotation or its turn about Y, a flag turns +X into the
    // wind keeping its own tilt, a clock hand turns about Z; a spring rides
    // its offset above the ground, clamped.
    #[test]
    fn motion_faces_and_rides_as_the_game_aligns() {
        let still = Motion {
            offset: [0.; 3],
            offset_speed: 0.,
            offset_turns: false,
            rotation: [0.; 3],
            rotation_speed: 0.,
            oscillation: 0.,
            alignment: Alignment::None,
            spring: None,
        };
        let close = |a: [f32; 3], b: [f32; 3]| (0..3).all(|k| (a[k] - b[k]).abs() < 1e-4);
        // Looking down 45° from the side, turned 30° about Y.
        let camera = quat_mul(axis(1, 30f32.to_radians()), axis(0, 45f32.to_radians()));
        let scene = Scene {
            camera,
            wind: [0., 0., 2.],
            clock: [15, 0, 30],
            ground: 0.,
        };
        let tilted = Transform {
            position: [0., 2., 0.],
            rotation: quat_mul(axis(1, 1.), axis(0, 0.3)),
            scale: [1.; 3],
        };
        let face = |alignment| {
            Motion { alignment, ..still }
                .place(0., &tilted, &scene)
                .rotation
        };
        assert_eq!(face(Alignment::Camera), camera);
        // Upright: the camera's heading, no tilt; its -Z toward the viewer
        // along the ground.
        let upright = face(Alignment::CameraYaw);
        assert!(close(rotate(upright, [0., 1., 0.]), [0., 1., 0.]));
        let ahead = rotate(camera, [0., 0., 1.]);
        let level = rotate(upright, [0., 0., 1.]);
        let flat = (ahead[0] * ahead[0] + ahead[2] * ahead[2]).sqrt();
        assert!(
            close(level, [ahead[0] / flat, 0., ahead[2] / flat]),
            "{level:?}"
        );
        // Into the wind blowing to +Z: +X turns to +Z; the part's own turn
        // about X stays, its heading goes.
        let flag = face(Alignment::WithWind);
        assert!(close(rotate(flag, [1., 0., 0.]), [0., 0., 1.]), "{flag:?}");
        let against = face(Alignment::AgainstWind);
        assert!(close(rotate(against, [1., 0., 0.]), [0., 0., -1.]));
        // Three in the afternoon: a quarter turn clockwise seen from +Z.
        let hours = Motion {
            alignment: Alignment::Hours,
            ..still
        };
        let at3 = hours.place(0., &Transform::IDENTITY, &scene).rotation;
        assert!(close(rotate(at3, [0., 1., 0.]), [1., 0., 0.]), "{at3:?}");
        // 2 m up, riding 0.5 above where its -Y meets the ground, held to
        // ±0.1: lifted by -0.1, down to 1.9.
        let wheel = Motion {
            spring: Some(Spring {
                min: -0.1,
                max: 0.1,
                offset: 0.5,
            }),
            ..still
        };
        let up = Transform {
            position: [0., 2., 0.],
            ..Transform::IDENTITY
        };
        assert!(close(wheel.place(0., &up, &scene).position, [0., 1.9, 0.]));
        // An alignment alone is motion the rig keeps.
        let cfg = "<Config><ConfigType>MAIN</ConfigType><Models><Config><ConfigType>MODEL</ConfigType>\
<FileName>a/icon.fbx</FileName><Transformer><Config><ConfigType>ANIMATION_TRANSFORM</ConfigType>\
<Alignment>3</Alignment></Config></Transformer></Config></Models></Config>";
        let m = Model::parse(cfg.as_bytes()).unwrap();
        let motion = m.parts[0].rig.motion.expect("a billboard moves");
        assert_eq!((motion.alignment, motion.spring), (Alignment::Camera, None));
    }

    // A 2070 entry's placement writes in the file's `m_` spelling and reads
    // back as the later games' does.
    #[test]
    fn placements_write_into_a_2070_cfg() {
        let cfg = "<m_Config><m_ConfigType>ROOT</m_ConfigType><m_Models><m_Config>\
<m_ConfigType>MODEL</m_ConfigType><m_FileName>data/a.rdm</m_FileName></m_Config></m_Models></m_Config>";
        let mut doc = Doc::parse(cfg.as_bytes());
        let part = Model::from_doc(&doc).unwrap().parts[0].element;
        let t = Transform {
            position: [1., 2., 3.],
            ..Transform::IDENTITY
        };
        set_placement(&mut doc, part, &t);
        let written = String::from_utf8(doc.write()).unwrap();
        assert!(written.contains("<m_Transformer>"), "{written}");
        assert!(written.contains("<m_ConfigType>ORIENTATION_TRANSFORM</m_ConfigType>"));
        assert!(
            written.contains("<m_Position.x>1</m_Position.x>"),
            "{written}"
        );
        let again = Model::parse(written.as_bytes()).unwrap();
        assert_eq!(again.parts[0].transform, t);
    }

    // A link goes in before the placement and reads back; unlinking
    // leaves the cfg as it was.
    #[test]
    fn bone_links_write_before_the_placement() {
        let mut doc = Doc::parse(CFG.as_bytes());
        let m = Model::from_doc(&doc).unwrap();
        let part = m.parts[0].element;
        let link = BoneLink {
            model: 0,
            bone: 734730020,
        };
        set_bone_link(&mut doc, part, Some(link));
        let again = Model::from_doc(&Doc::parse(&doc.write())).unwrap();
        assert_eq!(again.parts[0].rig.link, Some(link));
        assert!(again.parts[0].bone_linked);
        let text = String::from_utf8(doc.write()).unwrap();
        assert!(text.find("BONELINK").unwrap() < text.find("ORIENTATION").unwrap());
        set_bone_link(&mut doc, part, None);
        assert_eq!(doc.write(), CFG.as_bytes());
    }

    #[test]
    fn matrices_decompose_into_their_transform() {
        let s = std::f32::consts::FRAC_1_SQRT_2;
        for t in [
            Transform::IDENTITY,
            Transform {
                position: [1., -2., 3.],
                rotation: [0., s, 0., s],
                scale: [2., 0.5, 3.],
            },
            Transform {
                position: [0., 0., 0.],
                rotation: [1., 0., 0., 0.],
                scale: [1.; 3],
            },
        ] {
            let back = Transform::from_matrix(&t.matrix());
            for (a, b) in back.matrix().iter().zip(t.matrix()) {
                assert!((a - b).abs() < 1e-5, "{t:?} came back as {back:?}");
            }
        }
    }

    // A move rewrites only the position; an entry without a Transformer
    // gets one; an uneven scale keeps the entry's uniform `Scale`.
    #[test]
    fn placements_write_back_into_the_cfg() {
        let mut doc = Doc::parse(CFG.as_bytes());
        let m = Model::from_doc(&doc).unwrap();
        let (part, prop) = (m.parts[0].element, m.props[0].element);
        let mut t = placement(&doc, part);
        t.position[0] = 4.5;
        set_placement(&mut doc, part, &t);
        let written = String::from_utf8(doc.write()).unwrap();
        assert_eq!(
            written,
            CFG.replace("<Position.x>1</Position.x>", "<Position.x>4.5</Position.x>")
        );
        t.scale = [2., 4., 2.];
        set_placement(&mut doc, part, &t);
        let moved = Transform {
            position: [0., 1., 2.],
            ..Transform::IDENTITY
        };
        set_placement(&mut doc, prop, &moved);
        let again = Doc::parse(&doc.write());
        let m = Model::from_doc(&again).unwrap();
        assert_eq!(m.parts[0].transform.scale, [2., 4., 2.]);
        assert_eq!(m.parts[0].transform.position, [4.5, 0., 0.]);
        assert!(!m.parts[0].bone_linked);
        assert_eq!(m.props[0].transform, moved);
        let text = String::from_utf8(again.write()).unwrap();
        assert!(
            text.contains("<Scale>2</Scale>")
                && text.contains("<NonUniformScale.y>2</NonUniformScale.y>")
        );
    }

    #[test]
    fn parses_cfg_dialect() {
        let m = Model::parse(CFG.as_bytes()).unwrap();
        assert_eq!(
            (m.mesh_center, m.mesh_extent),
            ([0.0, 1.0, 0.0], [2.0, 1.0, 2.0])
        );
        let p = &m.parts[0];
        assert_eq!(
            (p.name.as_str(), p.rdm.as_str()),
            ("leaf", "data/base/rdm/leaf_lod0.rdm")
        );
        assert!(!p.bone_linked);
        let mat = &p.materials[0];
        assert_eq!(mat.name, "<Default Material>");
        assert_eq!(
            mat.diffuse.as_deref(),
            Some("data/base/maps/leaf_diff_0.dds")
        );
        assert_eq!(mat.normal, None, "NORMAL_ENABLED 0 drops the map");
        assert_eq!(mat.alpha, Alpha::Cutout(0.4));
        assert!(mat.double_sided);
        assert_eq!(mat.terrain_tint, 0.8);
        assert_eq!(mat.diffuse_color, [1.2, 1.0, 1.0]);
        assert_eq!(m.props[0].cfg, "data/base/props/bucket.cfg");
        assert_eq!(m.props[0].transform, Transform::IDENTITY);

        // 90 degrees about +Y, scale 2, then +1 on X: +X maps to -Z.
        let t = p.transform;
        let q = t.apply([1.0, 0.0, 0.0]);
        assert!(
            (q[0] - 1.0).abs() < 1e-5 && (q[2] + 2.0).abs() < 1e-5,
            "{q:?}"
        );
        let mx = t.matrix();
        let by_matrix = [0, 1, 2].map(|r| mx[r] + mx[12 + r]);
        assert!((0..3).all(|k| (by_matrix[k] - q[k]).abs() < 1e-5));
    }

    // Trimmed from 117's heavy_bronze.cfg: a foundation plate whose foot
    // follows the ground by vertex color, a stairs part deformed as a
    // whole, a cutout bending through its shader constant, and a bush
    // placed relative to the ground.
    #[test]
    fn parses_terrain_adaption_flags() {
        let cfg = "<Config><ConfigType>MAIN</ConfigType><Models>\
<Config><ConfigType>MODEL</ConfigType><Materials><Config><ConfigType>MATERIAL</ConfigType>\
<TerrainAdaption [ModelCommon]>TerrainAdaption [ModelCommon]</TerrainAdaption [ModelCommon]>\
<ADJUST_TO_TERRAIN_HEIGHT>1</ADJUST_TO_TERRAIN_HEIGHT><VERTEX_COLORED_TERRAIN_ADAPTION>1</VERTEX_COLORED_TERRAIN_ADAPTION>\
<VERTEX_COLORED_LOCAL_PIVOTS>0</VERTEX_COLORED_LOCAL_PIVOTS><ABSOLUTE_TERRAIN_ADAPTION>0</ABSOLUTE_TERRAIN_ADAPTION>\
</Config></Materials><Name>ground</Name><FileName>ground.fbx</FileName><DeformToTerrainHeight>0</DeformToTerrainHeight></Config>\
<Config><ConfigType>MODEL</ConfigType><Materials><Config><ConfigType>MATERIAL</ConfigType>\
<ADJUST_TO_TERRAIN_HEIGHT>0</ADJUST_TO_TERRAIN_HEIGHT></Config></Materials>\
<Name>props</Name><FileName>props.fbx</FileName><DeformToTerrainHeight>1</DeformToTerrainHeight></Config>\
<Config><ConfigType>MODEL</ConfigType><Materials><Config><ConfigType>MATERIAL</ConfigType><ShaderID>5</ShaderID>\
<cAdjustToTerrainHeight>1</cAdjustToTerrainHeight><ABSOLUTE_TERRAIN_ADAPTION>1</ABSOLUTE_TERRAIN_ADAPTION></Config></Materials>\
<Name>cutout</Name><FileName>cutout.fbx</FileName></Config></Models>\
<Files><Config><ConfigType>FILE</ConfigType><AdaptTerrainHeight>1</AdaptTerrainHeight><Name>bush</Name>\
<FileName>bush.cfg</FileName><DeformToTerrainHeight>0</DeformToTerrainHeight></Config></Files></Config>";
        let m = Model::parse(cfg.as_bytes()).unwrap();
        let adapt = |p: usize| m.parts[p].materials[0].terrain_adapt;
        assert_eq!(
            adapt(0),
            Some(TerrainAdapt {
                absolute: false,
                vertex_colored: true,
                local_pivots: false
            })
        );
        assert!(!m.parts[0].deform_to_terrain);
        assert_eq!(adapt(1), None);
        assert!(m.parts[1].deform_to_terrain);
        assert_eq!(adapt(2).map(|a| a.absolute), Some(true));
        assert!(m.props[0].adapt_terrain_height && !m.props[0].deform_to_terrain);
    }

    // DLC 3's terrain blockers: ShaderID 24 with a splat map, three channels
    // on and the alpha left off. A channel without a texture weighs nothing,
    // and the fxm's defaults fill in what the cfg leaves out.
    #[test]
    fn parses_splat_maps() {
        let material = |fields: &str| {
            let cfg = format!(
                "<Config><ConfigType>MAIN</ConfigType><Models><Config><ConfigType>MODEL</ConfigType>\
<Materials><Config><ConfigType>MATERIAL</ConfigType><ShaderID>24</ShaderID>{fields}</Config></Materials>\
<Name>blocker</Name><FileName>blocker.fbx</FileName></Config></Models></Config>"
            );
            Model::parse(cfg.as_bytes()).unwrap().parts[0].materials[0].clone()
        };
        let m = material(
            "<SPLAT_MAP_ENABLED>1</SPLAT_MAP_ENABLED><cSplatTiling.x>1</cSplatTiling.x>\
<cSplatTiling.y>2</cSplatTiling.y><cSplatTiling.z>4</cSplatTiling.z><cSplatTiling.w>1</cSplatTiling.w>\
<SPLAT_SCALE_INVARIANT>1</SPLAT_SCALE_INVARIANT><cSplatMixAlbedo>1</cSplatMixAlbedo>\
<cSplatMixNormal>1</cSplatMixNormal><cModelSplatMap>maps/b_00_splat.tga</cModelSplatMap>\
<SPLAT_RED_ENABLED>1</SPLAT_RED_ENABLED><cModelSplatDiffTexR>maps/b_01_diff.tga</cModelSplatDiffTexR>\
<cModelSplatNormalTexR>maps/b_01_norm.tga</cModelSplatNormalTexR>\
<SPLAT_GREEN_ENABLED>1</SPLAT_GREEN_ENABLED><cModelSplatDiffTexG>maps/b_02_diff.tga</cModelSplatDiffTexG>\
<SPLAT_BLUE_ENABLED>1</SPLAT_BLUE_ENABLED><cModelSplatDiffTexB></cModelSplatDiffTexB>\
<SPLAT_ALPHA_ENABLED>0</SPLAT_ALPHA_ENABLED><cModelSplatDiffTexA>maps/b_04_diff.tga</cModelSplatDiffTexA>",
        );
        let splat = m.splat.unwrap();
        assert_eq!(splat.map, "maps/b_00_splat_0.dds");
        let [r, g, b, a] = &splat.layers;
        assert_eq!(
            r,
            &Some(SplatLayer {
                diffuse: "maps/b_01_diff_0.dds".into(),
                normal: Some("maps/b_01_norm_0.dds".into()),
                tiling: 1.,
            })
        );
        assert_eq!(
            g.as_ref().map(|g| (g.normal.clone(), g.tiling)),
            Some((None, 2.))
        );
        assert_eq!((b, a), (&None, &None));
        assert!(splat.mix_albedo && splat.mix_normal && splat.scale_invariant);
        assert!(!splat.world_tiling);
        let plain = material(
            "<SPLAT_MAP_ENABLED>1</SPLAT_MAP_ENABLED><cModelSplatMap>s.tga</cModelSplatMap>\
<SPLAT_RED_ENABLED>1</SPLAT_RED_ENABLED><cModelSplatDiffTexR>r.tga</cModelSplatDiffTexR>",
        )
        .splat
        .unwrap();
        assert!(!plain.mix_albedo && !plain.mix_normal && !plain.scale_invariant);
        assert_eq!(plain.layers[0].as_ref().map(|l| l.tiling), Some(1.));
        // Off, or on without a map: no splat.
        assert_eq!(
            material(
                "<SPLAT_MAP_ENABLED>0</SPLAT_MAP_ENABLED><cModelSplatMap>s.tga</cModelSplatMap>"
            )
            .splat,
            None
        );
        assert_eq!(
            material("<SPLAT_MAP_ENABLED>1</SPLAT_MAP_ENABLED>").splat,
            None
        );
    }

    // A dirt patch as 117's building grounds draw it: ShaderID 6 with an
    // alpha map and the higher bias. Other shaders are no mesh decal.
    #[test]
    fn parses_mesh_decals() {
        let mat = |fields: &str| {
            let cfg = format!(
                "<Config><ConfigType>MAIN</ConfigType><Models><Config><ConfigType>MODEL</ConfigType>\
<Materials><Config><ConfigType>MATERIAL</ConfigType>{fields}</Config></Materials>\
<FileName>a.fbx</FileName></Config></Models></Config>"
            );
            Model::parse(cfg.as_bytes()).unwrap().parts[0].materials[0].clone()
        };
        let dirt = mat(
            "<ShaderID>6</ShaderID><cOpacity>0.6</cOpacity><ALPHA_TEXTURE_ENABLED>1</ALPHA_TEXTURE_ENABLED>\
<cAlphaTex>data/maps/dirt_mask.psd</cAlphaTex><cHigherDepthBias>1</cHigherDepthBias>",
        );
        assert_eq!(dirt.alpha, Alpha::Decal);
        assert_eq!(
            dirt.mesh_decal,
            Some(MeshDecal {
                opacity: 0.6,
                alpha_map: Some("data/maps/dirt_mask_0.dds".into()),
                bias: 0.2,
                before_decals: false,
            })
        );
        let plain = mat(
            "<ShaderID>6</ShaderID><ALPHA_TEXTURE_ENABLED>0</ALPHA_TEXTURE_ENABLED><cAlphaTex>x.psd</cAlphaTex>",
        );
        assert_eq!(
            plain.mesh_decal.map(|d| (d.opacity, d.alpha_map, d.bias)),
            Some((1., None, 0.01))
        );
        assert_eq!(dirt.world_tiling, None);
        let tiled = mat(
            "<ShaderID>6</ShaderID><cEnableWorldSpaceTiling>1</cEnableWorldSpaceTiling>\
<cWorldSpaceTilingScale>3</cWorldSpaceTilingScale><cEnableAQTiling>0</cEnableAQTiling>",
        );
        assert_eq!(
            tiled.world_tiling,
            Some(WorldTiling {
                scale: 3.,
                diagonal: false
            })
        );
        assert_eq!(mat("<ShaderID>1</ShaderID>").mesh_decal, None);
        assert_eq!(mat("<ShaderID>8</ShaderID>").mesh_decal, None);
    }

    // The PBR shaders clip by diffuse alpha at the engine's reference even
    // with no switch in the cfg (Albion's flower_decal_02_marshveg is one:
    // a 5 m card whose grass is all in alpha); cloth, water, flipbooks, skin
    // and low-poly models do not.
    #[test]
    fn pbr_shaders_alpha_test() {
        let alpha = |shader: u32| {
            let cfg = format!(
                "<Config><ConfigType>MAIN</ConfigType><Models><Config><ConfigType>MODEL</ConfigType>\
<Materials><Config><ConfigType>MATERIAL</ConfigType><ShaderID>{shader}</ShaderID></Config></Materials>\
<FileName>a.rdm</FileName></Config></Models></Config>"
            );
            Model::parse(cfg.as_bytes()).unwrap().parts[0].materials[0].alpha
        };
        for shader in [8, 14, 20, 24] {
            assert_eq!(alpha(shader), Alpha::Tested(0.3), "shader {shader}");
        }
        for shader in [0, 7, 9, 11, 18] {
            assert_eq!(alpha(shader), Alpha::Opaque, "shader {shader}");
        }
    }

    // Trimmed from 1800's marketplace (logistic_01_02) and a 117 shield
    // maker's flag: glow with a night map, dye by the owner's colour. A
    // multi-dye mask takes palette colours, not the owner's; switches the
    // cfg leaves out stay off.
    #[test]
    fn parses_dye_and_glow() {
        let mat = |fields: &str| {
            let cfg = format!(
                "<Config><ConfigType>MAIN</ConfigType><Models><Config><ConfigType>MODEL</ConfigType>\
<Materials><Config><ConfigType>MATERIAL</ConfigType>{fields}</Config></Materials>\
<FileName>a.fbx</FileName></Config></Models></Config>"
            );
            Model::parse(cfg.as_bytes()).unwrap().parts[0].materials[0].clone()
        };
        let market = mat(
            "<ShaderID>8</ShaderID><DYE_MASK_ENABLED>0</DYE_MASK_ENABLED>\
<GLOW_ENABLED>1</GLOW_ENABLED><cEmissiveColor.r>0.650980</cEmissiveColor.r>\
<cEmissiveColor.g>0.650980</cEmissiveColor.g><cEmissiveColor.b>0.650980</cEmissiveColor.b>\
<NIGHT_GLOW_ENABLED>1</NIGHT_GLOW_ENABLED><cNightGlowMap>maps/marketplace_glow.psd</cNightGlowMap>",
        );
        assert_eq!(market.dye_mask, None);
        assert_eq!(
            market.glow,
            Some(Glow {
                color: [0.65098; 3],
                night_map: Some("maps/marketplace_glow_0.dds".into()),
                interior: None,
            })
        );
        let flag = mat(
            "<ShaderID>8</ShaderID><DYE_MASK_ENABLED>1</DYE_MASK_ENABLED>\
<cDyeMask>maps/flag_dye_mask.psd</cDyeMask><MULTI_DYE_ENABLED>0</MULTI_DYE_ENABLED>\
<GLOW_ENABLED>1</GLOW_ENABLED><NIGHT_GLOW_ENABLED>0</NIGHT_GLOW_ENABLED>\
<cNightGlowMap>maps/x.psd</cNightGlowMap>",
        );
        assert_eq!(flag.dye_mask.as_deref(), Some("maps/flag_dye_mask_0.dds"));
        assert_eq!(
            flag.glow.map(|g| (g.color, g.night_map)),
            Some(([2.; 3], None))
        );
        // A 117 window: its room behind the night map.
        let window = mat(
            "<GLOW_ENABLED>1</GLOW_ENABLED><NIGHT_GLOW_ENABLED>1</NIGHT_GLOW_ENABLED>\
<cNightGlowMap>maps/window_atlas_inner_mask.psd</cNightGlowMap>\
<WINDOW_INTERIOR_ENABLED>1</WINDOW_INTERIOR_ENABLED><cWindowInteriorDistance>0.2</cWindowInteriorDistance>\
<cWindowInteriorScale>2</cWindowInteriorScale><cWindowInteriorOffset.x>0</cWindowInteriorOffset.x>\
<cWindowInteriorOffset.y>0.5</cWindowInteriorOffset.y>",
        );
        assert_eq!(
            window.glow.and_then(|g| g.interior),
            Some(Interior {
                distance: 0.2,
                scale: 2.,
                offset: [0., 0.5]
            })
        );
        let cloth = mat(
            "<DYE_MASK_ENABLED>1</DYE_MASK_ENABLED><cDyeMask>m.psd</cDyeMask>\
<MULTI_DYE_ENABLED>1</MULTI_DYE_ENABLED>",
        );
        assert_eq!((cloth.dye_mask, cloth.glow), (None, None));
    }

    // A 2070 tree, trimmed: its stem is an instanced model, read as a part;
    // its leaf cluster's two billboards become two crossed quads each,
    // both ways round, at their positions and sizes, showing their texture
    // rects, tinted by the colours' mean and cut out.
    #[test]
    fn parses_2070_tree_stems_and_leaf_clusters() {
        let material = "<m_Materials><m_Config><m_ShaderIndex>0</m_ShaderIndex>\
<m_DiffuseTexture>data\\trees\\maps\\leaves.png</m_DiffuseTexture><m_DiffuseEnabled>1</m_DiffuseEnabled>\
<m_NormalEnabled>0</m_NormalEnabled><m_ConfigType>MATERIAL</m_ConfigType></m_Config></m_Materials>";
        let cfg = format!(
            "<m_Config><m_InstancingModels><m_Config>\
<m_InstancingModel>data\\trees\\rdm\\stem.rdm</m_InstancingModel>\
<m_ConfigType>INSTANCING_MODEL</m_ConfigType>{material}</m_Config></m_InstancingModels>\
<m_InstancingBBClusters><m_Config><m_BBClusterSize>2</m_BBClusterSize>\
<m_BBPosX0>1</m_BBPosX0><m_BBPosY0>2</m_BBPosY0><m_BBPosZ0>0</m_BBPosZ0><m_BBSize0>1</m_BBSize0>\
<m_BBTexX0>0.5</m_BBTexX0><m_BBTexY0>0.5</m_BBTexY0><m_BBTexZ0>0.75</m_BBTexZ0><m_BBTexW0>1</m_BBTexW0>\
<m_BBPosX1>-1</m_BBPosX1><m_BBPosY1>2</m_BBPosY1><m_BBPosZ1>0</m_BBPosZ1><m_BBSize1>2</m_BBSize1>\
<m_BBTexX1>0</m_BBTexX1><m_BBTexY1>0</m_BBTexY1><m_BBTexZ1>0.25</m_BBTexZ1><m_BBTexW1>0.5</m_BBTexW1>\
<m_ColorCount>2</m_ColorCount><m_ColorR0>0.2</m_ColorR0><m_ColorG0>0.8</m_ColorG0><m_ColorB0>0.4</m_ColorB0>\
<m_ColorR1>0.4</m_ColorR1><m_ColorG1>0.6</m_ColorG1><m_ColorB1>0.2</m_ColorB1>\
<m_ConfigType>INSTANCING_BBCLUSTER</m_ConfigType>{material}</m_Config></m_InstancingBBClusters>\
<m_ConfigType>ROOT</m_ConfigType></m_Config>"
        );
        let m = Model::parse(cfg.as_bytes()).unwrap();
        let [stem, leaves] = &m.parts[..] else {
            panic!("{:?}", m.parts);
        };
        assert_eq!(
            (stem.rdm.as_str(), stem.mesh.is_none()),
            ("data/trees/rdm/stem.rdm", true)
        );
        let mesh = leaves.mesh.as_ref().unwrap();
        // Two boards, two quads each, four corners and four triangles a quad.
        assert_eq!((mesh.positions.len(), mesh.indices.len()), (16, 48));
        assert_eq!(mesh.groups[0].count, 48);
        // The first board's quad across X: 1 m wide, its top at 2.5 m, its
        // rect's top-left corner there; its normal points away from the
        // crown's middle (0, 2, 0).
        assert_eq!(mesh.positions[0], [0.5, 2.5, 0.]);
        assert_eq!(mesh.uvs[0], [0.5, 0.5]);
        assert_eq!(mesh.uvs[2], [0.75, 1.]);
        assert_eq!(mesh.normals[0], [1., 0., 0.]);
        // The second board is 2 m: its quad across Z reaches 1 m either side.
        assert_eq!(mesh.positions[12], [-1., 3., -1.]);
        assert_eq!(mesh.bounds, ([-2., 1., -1.], [1.5, 3., 1.]));
        let material = &leaves.materials[0];
        assert_eq!(material.alpha, Alpha::Cutout(50. / 255.));
        assert!((0..3).all(|a| (material.diffuse_color[a] - [0.3, 0.7, 0.3][a]).abs() < 1e-6));
        assert_eq!(
            material.diffuse.as_deref(),
            Some("data/trees/maps/leaves_0.dds")
        );
    }

    // Trimmed from 117 building grounds: a box decal with a height-frayed
    // border fade and a UV transform, and a terrain-baked one whose opacity
    // is cDiffuseOpacity.
    #[test]
    fn parses_box_decals() {
        let cfg = "<Config><ConfigType>MAIN</ConfigType><Decals>\
<Config><ConfigType>DECAL</ConfigType><Materials><Config><ConfigType>MATERIAL</ConfigType>\
<ShaderID>1</ShaderID><cModelDiffTex>data/maps/mud_diff.psd</cModelDiffTex><cOpacity>0.8</cOpacity>\
<HEIGHT_MAP_ENABLED>1</HEIGHT_MAP_ENABLED><cHeightMap>data/maps/mud_height.psd</cHeightMap>\
<ENABLE_UV_TRANSFORM>1</ENABLE_UV_TRANSFORM><cTexTiling.x>2</cTexTiling.x><cTexTiling.y>1</cTexTiling.y>\
<cTexFlipU>1</cTexFlipU><cTexOffset.x>0.5</cTexOffset.x><cTexRotation>90</cTexRotation><cTexRotateAboutCenter>1</cTexRotateAboutCenter>\
<cEnableBorderFadeout>1</cEnableBorderFadeout><cFadeOutDiastance>0.3</cFadeOutDiastance>\
<cHeightMapFadeoutThreshold>0.4</cHeightMapFadeoutThreshold><cHeightMapFadeoutFalloff>0.2</cHeightMapFadeoutFalloff>\
<cExperimentalEnableAlphaBasedFade>0</cExperimentalEnableAlphaBasedFade></Config></Materials>\
<Type>Terrain</Type><Extents.x>2</Extents.x><Extents.y>0.25</Extents.y><Extents.z>2.5</Extents.z></Config>\
<Config><ConfigType>DECAL</ConfigType><Materials><Config><ConfigType>MATERIAL</ConfigType>\
<ShaderID>2</ShaderID><cDiffuseOpacity>0.6</cDiffuseOpacity><cOpacity>0</cOpacity>\
<cExperimentalEnableAlphaBasedFade>1</cExperimentalEnableAlphaBasedFade><cEnableBorderFadeout>1</cEnableBorderFadeout>\
</Config></Materials><Type>Terrain-Baked</Type></Config></Decals></Config>";
        let m = Model::parse(cfg.as_bytes()).unwrap();
        let (mud, baked) = (&m.decals[0], &m.decals[1]);
        assert_eq!((mud.kind.as_str(), mud.opacity), ("Terrain", 0.8));
        assert_eq!(mud.extents, [2., 0.25, 2.5]);
        assert_eq!(
            mud.height_map.as_deref(),
            Some("data/maps/mud_height_0.dds")
        );
        assert_eq!(
            mud.fade,
            Some(DecalFade::Grid {
                distance: 0.3,
                threshold: 0.4,
                falloff: 0.2
            })
        );
        // A quarter turn about the centre takes (1, 0.5) to (0.5, 1); the
        // flipped, doubled U then moves by the offset.
        let t = mud.uv_transform.unwrap();
        let [u, v] = t.apply([1., 0.5]);
        assert!(
            (u - (0.5 * -2. + 0.5)).abs() < 1e-5 && (v - 1.).abs() < 1e-5,
            "{u} {v}"
        );
        assert_eq!((baked.kind.as_str(), baked.opacity), ("Terrain-Baked", 0.6));
        assert!(matches!(baked.fade, Some(DecalFade::Smooth { .. })));
        assert_eq!((baked.height_map.clone(), baked.uv_transform), (None, None));
    }

    #[test]
    fn source_paths_map_to_shipped_names() {
        assert_eq!(
            mesh_path("Data\\Base\\x\\rdm\\Tree_07.FBX"),
            "data/base/x/rdm/tree_07.rdm"
        );
        assert_eq!(
            texture_path("data/a/maps/b_diff.psd"),
            "data/a/maps/b_diff_0.dds"
        );
        assert_eq!(
            texture_path("data/engine/default_model_normal.png"),
            "data/engine/default_model_normal_0.dds"
        );
        assert_eq!(texture_path("data/a/x_0.dds"), "data/a/x_0.dds");
    }

    fn install() -> Option<Install> {
        let Some(dir) = crate::vfs::install_dir("ANNO117_DIR") else {
            eprintln!("skip: ANNO117_DIR unset");
            return None;
        };
        Some(Install::open(dir, crate::vfs::Install::DATA_FIRST).unwrap())
    }

    // Every loaded mesh: indices in range, unit normals, groups pointing at
    // cfg materials, and the rdm bounds inside the part's cfg AABB.
    fn check_parts(m: &Model) {
        for p in &m.parts {
            let mesh = p
                .mesh
                .as_ref()
                .unwrap_or_else(|| panic!("{}: {:?}", p.rdm, m.problems));
            let n = mesh.positions.len() as u32;
            assert!(mesh.indices.iter().all(|&i| i < n));
            for v in &mesh.normals {
                assert!((v.iter().map(|c| c * c).sum::<f32>().sqrt() - 1.0).abs() < 1e-3);
            }
            for g in &mesh.groups {
                assert!(
                    (g.material as usize) < p.materials.len(),
                    "{}: group {g:?}",
                    p.rdm
                );
            }
            // The rdm's stored bounds, which the cfg's AABBMin/AABBMax
            // repeat, hold every decoded position.
            {
                let (lo, hi) = mesh.bounds;
                let (mlo, mhi) = mesh.computed_bounds();
                for k in 0..3 {
                    // f16 positions: ~1e-3 relative.
                    let tol = 0.01 + 1e-3 * hi[k].abs().max(lo[k].abs());
                    assert!(
                        mlo[k] >= lo[k] - tol && mhi[k] <= hi[k] + tol,
                        "{}: {k}",
                        p.rdm
                    );
                }
            }
        }
    }

    // Union of every part's transformed mesh corners; the cfg's
    // MeshCenter/MeshExtent box must contain it.
    fn assert_inside_mesh_bounds(m: &Model, tol: f32) {
        let lo = [0, 1, 2].map(|k| m.mesh_center[k] - m.mesh_extent[k] - tol);
        let hi = [0, 1, 2].map(|k| m.mesh_center[k] + m.mesh_extent[k] + tol);
        for p in &m.parts {
            let (a, b) = p.mesh.as_ref().unwrap().computed_bounds();
            for corner in 0..8 {
                let c = [0, 1, 2].map(|k| if corner >> k & 1 == 0 { a[k] } else { b[k] });
                let w = p.transform.apply(c);
                assert!(
                    (0..3).all(|k| w[k] >= lo[k] && w[k] <= hi[k]),
                    "{}: {w:?} outside {lo:?}..{hi:?}",
                    p.rdm
                );
            }
        }
    }

    #[test]
    fn olive_tree() {
        let Some(install) = install() else { return };
        let m = Model::load(
            &install,
            "data/base/graphics/roman/environment/props/vegetation/trees/olive_trees/olive_tree_07.cfg",
        )
        .unwrap();
        assert!(m.problems.is_empty(), "{:?}", m.problems);
        assert_eq!(m.parts.len(), 1);
        let p = &m.parts[0];
        assert!(p.rdm.ends_with("olive_trees/rdm/tree_07.rdm"));
        let [bark, leaf] = &p.materials[..] else {
            panic!()
        };
        assert_eq!((bark.alpha, bark.double_sided), (Alpha::Cutout(0.3), false));
        assert_eq!((leaf.alpha, leaf.double_sided), (Alpha::Cutout(0.3), true));
        assert!(
            bark.diffuse
                .as_deref()
                .unwrap()
                .ends_with("modules/maps/tree_bark_diff_0.dds")
        );
        assert!(
            leaf.normal
                .as_deref()
                .unwrap()
                .ends_with("olive_leaf_01_norm_0.dds")
        );
        assert_eq!(bark.normal, None);
        assert_eq!(bark.diffuse_color, [1.2; 3]);
        // The rdm material slots line up with the cfg list by name.
        let mesh = p.mesh.as_ref().unwrap();
        assert_eq!(mesh.materials[1].name, leaf.name);
        check_parts(&m);
        // A single untransformed part: the cfg's mesh box is the rdm AABB.
        let (lo, hi) = mesh.bounds;
        for k in 0..3 {
            assert!(((lo[k] + hi[k]) / 2.0 - m.mesh_center[k]).abs() < 1e-3);
            assert!(((hi[k] - lo[k]) / 2.0 - m.mesh_extent[k]).abs() < 1e-3);
        }
    }

    // DLC 3's terrain blocker ships every splat texture its cfg names.
    #[test]
    fn terrain_blocker_splat() {
        let Some(install) = install() else { return };
        let m = Model::load(
            &install,
            "data/dlc03/graphics/egypt/environment/props/terrainblocker/egypt_terrainblocker_01/egypt_terrainblocker_01_b.cfg",
        )
        .unwrap();
        assert!(m.problems.is_empty(), "{:?}", m.problems);
        let material = &m.parts[0].materials[0];
        assert_eq!(material.shader, Shader::Id(24));
        let splat = material.splat.as_ref().unwrap();
        assert!(
            splat
                .map
                .ends_with("egypt_terrainblocker_01_00_splat_0.dds")
        );
        assert_eq!(splat.layers.iter().flatten().count(), 3);
        assert!(splat.layers.iter().flatten().all(|l| l.normal.is_some()));
        assert!(splat.mix_albedo && splat.mix_normal && splat.scale_invariant);
    }

    #[test]
    fn rock() {
        let Some(install) = install() else { return };
        let m = Model::load(
            &install,
            "data/base/graphics/roman/environment/props/rocks/roman_rock_02/roman_rock_02_c.cfg",
        )
        .unwrap();
        assert!(m.problems.is_empty(), "{:?}", m.problems);
        assert!(!m.parts.is_empty());
        assert!(
            m.parts
                .iter()
                .flat_map(|p| &p.materials)
                .all(|m| matches!(m.alpha, Alpha::Tested(_) | Alpha::Hidden))
        );
        check_parts(&m);
        assert_inside_mesh_bounds(&m, 0.02);
    }

    #[test]
    fn building_with_parts_and_props() {
        let Some(install) = install() else { return };
        let m = Model::load(
            &install,
            "data/base/graphics/roman/buildings/institution/institution_01/institution_01.cfg",
        )
        .unwrap();
        assert!(m.problems.is_empty(), "{:?}", m.problems);
        assert!(m.parts.len() >= 5, "{}", m.parts.len());
        assert!(m.parts.iter().any(|p| p.transform != Transform::IDENTITY));
        let buckets: Vec<_> = m
            .props
            .iter()
            .filter(|p| p.name == "bucket_water_01")
            .collect();
        assert!(buckets.len() >= 4);
        assert_eq!(buckets[0].transform.position, [1.27158, 0.0, -1.711167]);
        check_parts(&m);
        assert_inside_mesh_bounds(&m, 0.05);
        // Nested props load the same way.
        let bucket = Model::load(&install, &buckets[0].cfg).unwrap();
        assert!(!bucket.parts.is_empty() && bucket.problems.is_empty());
    }

    // ANNO117_SWEEP=1: load every cfg, count parts, props and problem kinds.
    #[test]
    fn sweep_install() {
        if std::env::var_os("ANNO117_SWEEP").is_none() {
            return;
        }
        let Some(install) = install() else { return };
        let (mut cfgs, mut parts, mut meshes, mut props, mut decals) = (0, 0, 0, 0, 0);
        let mut failed = Vec::new();
        let mut problems = std::collections::BTreeMap::<&str, (usize, Vec<String>)>::new();
        let mut alpha = std::collections::BTreeMap::<String, usize>::new();
        for i in 0..install.file_count() {
            let path = install.file_path(FileId(i as u32));
            if !path.ends_with(".cfg") || !path.starts_with("data/") {
                continue;
            }
            let m = match Model::load(&install, &path) {
                Ok(m) => m,
                Err(e) => {
                    failed.push(format!("{path}: {e:#}"));
                    continue;
                }
            };
            cfgs += 1;
            parts += m.parts.len();
            meshes += m.parts.iter().filter(|p| p.mesh.is_some()).count();
            props += m.props.len();
            decals += m.decals.len();
            for p in m.parts.iter().flat_map(|p| &p.materials) {
                let key = format!("{:?}", p.alpha)
                    .split('(')
                    .next()
                    .unwrap()
                    .to_string();
                *alpha.entry(key).or_default() += 1;
            }
            for p in &m.problems {
                let kind = if p.contains("no texture") {
                    "missing texture"
                } else if p.contains("no mesh") {
                    "missing mesh"
                } else if p.contains("no prop") {
                    "missing prop cfg"
                } else {
                    "bad rdm"
                };
                let e = problems.entry(kind).or_default();
                e.0 += 1;
                if e.1.len() < 3 {
                    e.1.push(format!("{path}: {p}"));
                }
            }
        }
        eprintln!(
            "{cfgs} cfgs, {parts} parts ({meshes} meshes loaded), {props} props, {decals} decals"
        );
        eprintln!("materials by alpha: {alpha:?}");
        for (kind, (n, examples)) in &problems {
            eprintln!("{n:6} {kind}");
            for e in examples {
                eprintln!("         {e}");
            }
        }
        for f in failed.iter().take(10) {
            eprintln!("FAIL {f}");
        }
        assert!(failed.is_empty(), "{} cfgs failed", failed.len());
    }
}
