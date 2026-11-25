// Anno 117's and 1800's model material as a surface family, how a mesh
// joins it (`dress`), and the scene numbers it reads (`scene_glow`).

use std::sync::{Arc, OnceLock};

use anno_core::ambient::ModelGlow;
use anno_core::files::ModelLook;
use anno_formats::model::{Alpha, Material, Splat};
use gpui_editor_kit::gpu::{self, Families, Family, Lighting, MeshData};
use gpui_editor_kit::texture::Image;

/// The family's name in its registry.
pub const NAME: &str = "a7mmodel";

// Payload word 0's bits; the slots follow in this order, each only where
// the mesh has it.
const DYE: u32 = 1;
const STEADY: u32 = 2;
const NIGHT: u32 = 4;
const ROOM: u32 = 8;

const WGSL: &str = r#"
// Payload (a7m_models.rs `dress`): word 0 the flags (1 dye mask, 2 the
// normal map's blue glows, 4 night mask, 8 a room behind its windows),
// 1-3 the emissive colour, 4 the night mask's gain, 5-8 the room's depth,
// scale and offset. Scene numbers (a7m_models.rs `scene_glow`): host[0] the dye
// colour, host[1] the night threshold (1 lights none), strength, how much
// colour night light keeps and its steps (0 smooth), host[2] the night
// colour.

// The kit's cut, on every path: this family covers what `lit` does.
fn a7mmodel_coverage(material: GpuDraw, c: Cover) -> f32 {
    return lit_coverage(material, c);
}

fn a7mmodel_number(material: GpuDraw, k: u32) -> f32 {
    return bitcast<f32>(family_word(material, k));
}

// The mesh's slot of map `which` (0 dye, 1 night, 2 room), counting only
// the maps it has, in that order; 0 (white) where the path holds none.
fn a7mmodel_slot(material: GpuDraw, flags: u32, which: u32) -> u32 {
    var k = 0u;
    if which >= 1u && (flags & 1u) != 0u { k += 1u; }
    if which >= 2u && (flags & 4u) != 0u { k += 1u; }
    return family_slot(material, k);
}

// Which channel of a night mask the placement or instance standing at
// `position` lights: one of three, hashed from its position's bits, so it
// keeps its pick while others are placed and removed around it.
fn a7mmodel_night_channel(position: vec3<f32>) -> u32 {
    let h = bitcast<vec3<u32>>(position);
    var x = (h.x * 0x9e3779b1u) ^ (h.y * 0x85ebca77u) ^ (h.z * 0xc2b2ae3du);
    x ^= x >> 15u;
    x *= 0x2c1b3c6du;
    x ^= x >> 12u;
    return x % 3u;
}

// Where a room `depth` UV units behind a surface shows at `world`, in the
// surface's UVs: along the view, in the UV frame the position and UV
// derivatives give (as the kit's `bend` builds it).
fn a7mmodel_behind(world: vec3<f32>, n: vec3<f32>, dp: array<vec3<f32>, 2>, duv: array<vec2<f32>, 2>, uv: vec2<f32>, depth: f32) -> vec2<f32> {
    let a = cross(dp[1], n);
    let b = cross(n, dp[0]);
    let flip = select(1.0, -1.0, dot(dp[0], a) < 0.0);
    let t = a * duv[0].x + b * duv[1].x;
    let bt = a * duv[0].y + b * duv[1].y;
    let along = vec3<f32>(
        dot(-toward_viewer(world), t * flip * inverseSqrt(max(dot(t, t), 1e-30))),
        dot(-toward_viewer(world), bt * flip * inverseSqrt(max(dot(bt, bt), 1e-30))),
        dot(-toward_viewer(world), n),
    );
    let v = normalize(along);
    // Grazing views would reach rooms away; the window's own frame hides
    // them anyway.
    return uv + depth * v.xy / max(-v.z, 0.05);
}

// How lit a window's room is: the window lets light in onto the wall it
// faces, a spot where the mask's coarse level `spot` is up, 0.6 elsewhere;
// seen from above the floor brightens it.
fn a7mmodel_room_light(spot: f32, world: vec3<f32>) -> f32 {
    let up = toward_viewer(world).y;
    return clamp(mix(0.6, 1.0, clamp(spot * 8.0, 0.0, 1.0)) + 0.5 * up * up, 0.0, 1.0);
}

// What a dye mask, a window's room and the material's light make of the
// surface: `dye` and `room` what it shades as, `keep` how much of that
// still shades, `light` what it gives off.
struct a7mmodel_Glowing { dye: vec3<f32>, keep: f32, room: vec4<f32>, light: vec3<f32> }

// `texel` is the decoded albedo texel, `color` the emissive colour (zero
// without), `tint` the dye mask's red and `steady` the lit mask, each 0
// without, `night` the night mask's channel times its gain, `room` the
// room's colour and how much of the window (alpha) shows it. The shading
// gives way to the light as its mask rises; where a room shows, its light
// takes the room's colour.
fn a7mmodel_glowing(texel: vec3<f32>, color: vec3<f32>, tint: f32, steady: f32, night: f32, room: vec4<f32>) -> a7mmodel_Glowing {
    let dye = mix(vec3<f32>(1.0), lights.host[0].rgb, tint);
    var strength = glow_mask(steady);
    var c = mix(texel, room.rgb, room.a);
    var n = night;
    let hour = lights.host[1];
    if hour.w > 0.0 { n = floor(clamp(n, 0.0, 1.0) * hour.w) / hour.w; }
    // The brightest texels pass the threshold first as night falls.
    let lit = clamp((n - hour.x) * 4.0, 0.0, 1.0);
    if lit > 0.0 {
        strength += lit * hour.y;
        let grey = dot(c, vec3<f32>(0.2125, 0.7154, 0.0721));
        c = mix(grey * lights.host[2].rgb, c, hour.z);
    }
    return a7mmodel_Glowing(dye, 1.0 - clamp(strength, 0.0, 1.0), room, c * color * dye * strength * lights.glow.w);
}

// The glow of `material` at `p`, its shading normal `n`: its masks are read
// only where the path holds them, the night mask only by night or for its
// rooms, in the channel where the placement stands picks. `steady` is the
// normal map's blue.
fn a7mmodel_glow(material: GpuDraw, p: LitPixel, n: vec3<f32>, texel: vec3<f32>, steady: f32) -> a7mmodel_Glowing {
    let flags = family_word(material, 0u);
    let color = vec3<f32>(a7mmodel_number(material, 1u), a7mmodel_number(material, 2u), a7mmodel_number(material, 3u));
    let has_normal = (material.maps & 0xffffu) != 0u;
    var tint = 0.0;
    var lit = select(0.0, steady, (flags & 2u) != 0u && has_normal);
    var night = 0.0;
    var room = vec4<f32>(0.0);
    let dye = a7mmodel_slot(material, flags, 0u);
    if (flags & 1u) != 0u && dye != 0u {
        tint = family_texel(dye, p.uv, p.duv).r;
    }
    let mask = a7mmodel_slot(material, flags, 1u);
    let wall = a7mmodel_slot(material, flags, 2u);
    let rooms = (flags & 8u) != 0u && wall != 0u;
    let dark = lights.host[1].x < 1.0;
    if (flags & 4u) != 0u && mask != 0u && (dark || rooms) {
        let m = family_texel(mask, p.uv, p.duv);
        var dim = 1.0;
        let window = max(m.r, max(m.g, m.b));
        if rooms && window > 0.0 {
            let scale = a7mmodel_number(material, 6u);
            let offset = vec2<f32>(a7mmodel_number(material, 7u), a7mmodel_number(material, 8u));
            let at = a7mmodel_behind(p.world, n, p.dp, p.duv, p.uv, a7mmodel_number(material, 5u));
            dim = a7mmodel_room_light(family_texel_level(mask, at, 7.0).r, p.world);
            let w = family_texel(wall, at * scale + offset, array<vec2<f32>, 2>(p.duv[0] * scale, p.duv[1] * scale));
            room = vec4<f32>(decode(w.rgb) * dim, window);
            lit *= dim;
        }
        if dark {
            night = glow_mask(m[a7mmodel_night_channel(family_origin(p))]) * a7mmodel_number(material, 4u) * dim * dim;
        }
    }
    return a7mmodel_glowing(texel, color, tint, lit, night, room);
}

// `lit`'s surface with the material's glow: under the mesh's splat where
// it has one, dyed, the room where a window shows one, less what its own
// light replaces, then the maps bend and fill it in as `lit` does.
fn a7mmodel_surface(material: GpuDraw, p: LitPixel, sampled: LitTexels) -> Lit {
    let t = family_splat(material, p, sampled);
    let n = p.normal;
    let texel = t.albedo.rgb;
    let g = a7mmodel_glow(material, p, n, texel, t.normal.b);
    let albedo = tinted(family_albedo(p, texel), p.world, material.tint);
    let base = mix(albedo * g.dye, g.room.rgb, g.room.a) * g.keep;
    return Lit(mapped(base, n, p.dp, p.duv, t.normal, (material.maps & 0xffffu) != 0u, t.surface, (material.maps >> 16u) != 0u), g.light);
}
"#;

/// The family, as its registry takes it.
///
/// Dye masks blend the albedo toward the owner's colour, the normal map's
/// blue glows at every hour, night masks light by the hour (each placement
/// one of their three channels), and rooms show behind the night mask's
/// windows (interior mapping). The registry the games' renderers draw with
/// (`families`) decodes the island's tint as the a7m ground does.
///
/// Meshes that only glow steadily stay the kit's: its `lit` lights the
/// normal map's blue alike (`MeshData::emissive`). The family is the kit's
/// `lit` where it adds nothing, so a mesh draws the same pixels whichever
/// of the two it is in.
pub fn family() -> Family {
    Family {
        name: NAME.into(),
        wgsl: WGSL.into(),
        light: false,
        // Its coverage is the kit's `lit`'s.
        vertex: false,
        coverage_slots: 0,
        // It reads where the row stands and gives off light, which
        // impostors don't bake; the shell gives such pieces none anyway.
        bake: false,
    }
}

/// The registry 117's and 1800's renderers draw with: the kit's family and
/// this one, lit by the kit's shade and fog, the island's tint decoded as
/// the a7m ground decodes it (`a7m::TINT_DECODE`). One per process, so
/// every renderer shares its pipelines.
pub fn families() -> Arc<Families> {
    static FAMILIES: OnceLock<Arc<Families>> = OnceLock::new();
    FAMILIES
        .get_or_init(|| {
            let lighting = Lighting {
                tint: Some(format!(
                    "fn ground_tint_decode(texel: vec4<f32>) -> vec3<f32> {{\n{}}}\n",
                    crate::a7m::TINT_DECODE
                )),
                ..Default::default()
            };
            Families::with_lighting(vec![family()], lighting)
                .expect("the a7m model family registers")
        })
        .clone()
}

/// `glow` as the renderer takes it: its strength the kit's, the rest the
/// family's scene numbers (`lights.host`, the WGSL's layout).
pub fn scene_glow(glow: &ModelGlow) -> gpu::Glow {
    let mut host = [0.; gpu::HOST_WORDS];
    host[..3].copy_from_slice(&glow.tint);
    host[4..8].copy_from_slice(&[
        glow.night,
        glow.night_strength,
        glow.night_keep,
        glow.night_steps,
    ]);
    host[8..11].copy_from_slice(&glow.night_color);
    gpu::Glow {
        strength: glow.strength,
        host,
    }
}

/// A material's glow as the renderer lights it: its colour and its night
/// mask's gain. The cfg's emissive colour is gamma: the engine takes it to
/// linear (the shaders' default 2 stands in as `GammaToLinear(2.0)`). 1800
/// lights the albedo texture times that colour; 117 lights from its
/// G-buffer (`deferred`), which keeps the albedo times `cDiffuseColor` and
/// not the emissive colour, whose grey over 4 scales the night mask
/// instead (`GBUFFER_NIGHT_GLOW_MASK_FACTOR`), so its windows burn dimmer
/// than its default 2 would.
pub fn emissive(
    glow: &anno_formats::model::Glow,
    diffuse: [f32; 3],
    deferred: bool,
) -> ([f32; 3], f32) {
    let linear = glow.color.map(|c| c.max(0.).powf(2.2));
    let grey = 0.2125 * linear[0] + 0.7154 * linear[1] + 0.0721 * linear[2];
    if deferred {
        (diffuse.map(|c| c.clamp(0., 1.)), grey / 4.)
    } else {
        (linear, 1.)
    }
}

/// What a mesh of the family draws beside the kit's maps.
#[derive(Clone, Debug, Default)]
pub struct Maps {
    /// Red is how far the albedo blends toward the scene's dye colour
    /// ([`ModelGlow::tint`]).
    pub dye: Option<Image>,
    pub glow: Option<Glow>,
}

/// Light a mesh gives off itself: its albedo texel times `color`, times
/// the scene's strength, where a mask says so. The surface's own shading
/// gives way as the mask rises.
#[derive(Clone, Debug)]
pub struct Glow {
    /// Linear; may pass 1.
    pub color: [f32; 3],
    /// The normal map's blue is a mask lit at every hour. Without a normal
    /// map nothing is.
    pub steady: bool,
    /// A mask lit by night ([`ModelGlow::night`]), on the albedo's UVs:
    /// each placement or instance lights one of its red, green and blue
    /// channels, picked from where it stands, so a street's windows do not
    /// all light alike.
    pub night_map: Option<Image>,
    /// Scales the night mask before the night threshold takes it.
    pub night_gain: f32,
    /// A room seen through the night mask's windows, by day and night.
    pub room: Option<Room>,
}

/// A room behind a window (interior mapping): where the night mask's
/// brightest channel is up, the surface shows `map` (sRGB) `depth` UV
/// units behind it along the view, at `scale` times the UVs plus `offset`,
/// darker away from the light the window lets in (the mask's coarse
/// levels) and brighter seen from above. Its light, steady and by night,
/// dims with it.
#[derive(Clone, Debug)]
pub struct Room {
    pub map: Image,
    pub depth: f32,
    pub scale: f32,
    pub offset: [f32; 2],
}

/// Puts `data`, a lit mesh, in the family with `maps` when it dyes or has
/// a night mask, with its payload and slots. A mesh that only glows
/// steadily stays the kit's, glowing as [`MeshData::emissive`]; one with
/// neither is left alone.
pub fn join(data: &mut MeshData, maps: Maps) {
    let Maps { dye, glow } = maps;
    let night = glow.as_ref().and_then(|g| g.night_map.clone());
    if dye.is_none() && night.is_none() {
        if let Some(g) = glow.filter(|g| g.steady) {
            data.emissive = Some(g.color);
        }
        return;
    }
    let mut flags = 0;
    let mut slots = Vec::new();
    let mut repeats = Vec::new();
    if let Some(mask) = dye {
        flags |= DYE;
        slots.push(mask);
        repeats.push(1.);
    }
    let mut room = [0.; 4];
    if let Some(night) = night {
        flags |= NIGHT;
        slots.push(night);
        repeats.push(1.);
        if let Some(r) = glow.as_ref().and_then(|g| g.room.clone()) {
            flags |= ROOM;
            slots.push(r.map);
            // The room repeats `scale` times across the UVs, so residency
            // holds the levels that asks for.
            repeats.push(r.scale.abs().max(1e-3));
            room = [r.depth, r.scale, r.offset[0], r.offset[1]];
        }
    }
    let (color, gain) = glow
        .as_ref()
        .map_or(([0.; 3], 1.), |g| (g.color, g.night_gain));
    if glow.as_ref().is_some_and(|g| g.steady) {
        flags |= STEADY;
    }
    data.family = families().id(NAME).expect("registered");
    data.payload = [flags]
        .into_iter()
        .chain(color.map(f32::to_bits))
        .chain([gain.to_bits()])
        .chain(room.map(f32::to_bits))
        .collect();
    data.slots = slots;
    data.slot_repeats = repeats;
    // The family gives off the material's light itself.
    data.emissive = None;
}

/// What of the family a mesh draws, as [`join`] put it: `None` for a mesh
/// that isn't in it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Worn {
    pub dye: bool,
    pub steady: bool,
    pub night: bool,
    pub night_gain: f32,
    /// The room's depth, scale and offset.
    pub room: Option<[f32; 4]>,
}

pub fn worn(data: &MeshData) -> Option<Worn> {
    if Some(data.family) != families().id(NAME) {
        return None;
    }
    let flags = *data.payload.first()?;
    let number = |k: usize| data.payload.get(k).copied().map_or(0., f32::from_bits);
    Some(Worn {
        dye: flags & DYE != 0,
        steady: flags & STEADY != 0,
        night: flags & NIGHT != 0,
        night_gain: number(4),
        room: (flags & ROOM != 0).then(|| [5, 6, 7, 8].map(number)),
    })
}

/// Puts `data`, a lit mesh of `material` drawn with `look`, in the family
/// as [`join`] does, from the material's dye mask, glow, night mask and
/// room; `texture` reads a texture at the mesh's size. Meshes without a
/// texture take none of them, as the game shows them.
pub fn dress(
    look: &ModelLook,
    material: &Material,
    data: &mut MeshData,
    texture: &mut dyn FnMut(&str) -> Option<Image>,
) {
    if data.texture.is_none() || matches!(material.alpha, Alpha::Decal | Alpha::Hidden) {
        return;
    }
    let dye = material.dye_mask.as_deref().and_then(&mut *texture);
    let glow = material.glow.as_ref().map(|g| {
        let (color, night_gain) = emissive(g, material.diffuse_color, look.deferred_glow);
        let night_map = g.night_map.as_deref().and_then(&mut *texture);
        // The room map only for glows that have a room, behind a night
        // mask's windows.
        let room = (g.interior.zip(look.window_interior))
            .filter(|_| night_map.is_some())
            .and_then(|(i, path)| {
                Some(Room {
                    map: texture(path)?,
                    depth: i.distance,
                    scale: i.scale,
                    offset: i.offset,
                })
            });
        Glow {
            color,
            steady: true,
            night_map,
            night_gain,
            room,
        }
    });
    join(data, Maps { dye, glow });
}

/// A model's splat maps as the game blends them (`GetSplatMapShading`,
/// model_common.fxh): each layer weighs in by its control channel raised
/// to 8 times its albedo's alpha, at least 0.1; where none weighs in, a 0.3
/// grey; a tinting blend takes the albedo's red doubled, so a mid grey
/// (128, 0.5 encoded) leaves it as it is. Whether it tints, mixes normals,
/// scales with the placement or tiles in the world is the material's
/// `splat`; `cSplatWorldTiling` layers sample world (x, -z) and their
/// normals lean in the world (`FixSplatMapNormal`).
pub fn mesh_splat(
    control: Image,
    layers: [Option<gpu::MeshSplatLayer>; 4],
    splat: &Splat,
) -> gpu::MeshSplat {
    gpu::MeshSplat {
        control,
        layers,
        sharpness: 8.,
        alpha_floor: 0.1,
        fallback: [0.3; 3],
        tint: splat.mix_albedo.then_some(2.),
        mix_normals: splat.mix_normal,
        scale_invariant: splat.scale_invariant,
        world_tiling: splat.world_tiling,
    }
}

/// [`dress`] and [`families`] as the games' model looks name them.
pub struct Models;

impl anno_core::files::Surfaces for Models {
    fn glow(&self, glow: &ModelGlow) -> gpu::Glow {
        scene_glow(glow)
    }
    fn families(&self) -> Arc<Families> {
        families()
    }
    fn dress(
        &self,
        look: &ModelLook,
        material: &Material,
        data: &mut MeshData,
        texture: &mut dyn FnMut(&str) -> Option<Image>,
    ) {
        dress(look, material, data, texture);
    }
}

#[cfg(test)]
mod tests;
