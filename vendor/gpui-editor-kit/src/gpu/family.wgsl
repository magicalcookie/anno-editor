// The surface families' contract (placed.rs `Families`): the material
// header a family reads, the pixel and texels it is handed, and what every
// path that draws families shares. Each path brings the rest: its own
// accessors and `lit`'s surface and coverage, over the arena (placed.wgsl)
// or a piece's fixed bindings (pieces.wgsl). The accessors:
//   family_word(material, k) -> u32: word k of the mesh's payload
//   family_slot(material, k) -> u32: its slot k, 0 (white) without
//   family_texel(slot, uv, duv) and family_texel_level(slot, uv, level)
//   family_origin(p) -> vec3<f32>: where the row or instance stands
//   family_albedo(p, texel) -> vec3<f32>: the texel under the vertex
//     colour, as `lit` composes them on this path
//   family_splat(material, p, t) -> LitTexels: `t` under the mesh splat
//   coverage_cut(material) -> f32: the coverage under which the mesh is
//     cut away, on this path's measure of `lit`'s coverage
// A family may also call `lit_coverage` and `lit_surface`, the kit's own,
// to draw as `lit` does where it adds nothing.
// Composed after gpu.wgsl and the scene's lighting.

// Material of one piece level: albedo slot in t_albedos, flags (bit 0 has
// normals, bit 1 ignore texture alpha, bit 2 impostor, bits 3-4 terrain
// adaption, gpu::adapt_code, bit 5 decal, bit 6 32-bit indices, bit 8 the
// normal map's blue glows, bit 9 skinned, bit 11 slots, bits 16-23 the surface family
// that shades a lit mesh, family_of, bits 24-31 where its albedo's alpha
// cuts, coverage_cut) and the vertex color gain. Impostors: albedo is the atlas layer, source the material they bake.
// Lit meshes: source is where their splat starts in p_shapes, 0 without.
// One per distinct mesh, indexed by material: thinning buckets and pieces
// repeating the mesh share it.
// maps: normal map slot in the low half, surface map slot above; 0 none.
// tint: how much the ground's tint multiplies the albedo. vertex: the
// mesh's first vertex; parts: where its thinning parts start in p_parts,
// or with flags bit 9 its skin (pose_turn), NO_PARTS for meshes with
// neither.
// Decals (flags bit 5): alpha is the alpha map's slot (0, white, without),
// with the coverage's opacity and the bias toward the viewer.
// tiling: world-space texture tiling (gpu::tiling_code), lit and decals.
// edge*: a projected decal's edge (gpu::DecalEdge::code, decal_edge).
// o*, step*: the mesh's vertex grids (gpu::Quant, arena_vertex): origins,
// and the steps of positions, u and v. index: its first word in p_indices.
// firsts: where its table starts in p_firsts (visbuf.wgsl).
// payload: where the mesh's family payload starts in p_shapes, 0 without
// (family_word); `lit`'s holds its emissive colour (lit_glow).
// The kit's own meshes take their slots from their own bindings instead
// (pieces.wgsl piece_material).
struct GpuDraw { albedo: u32, flags: u32, gain: f32, source: u32, maps: u32, tint: f32, vertex: u32, parts: u32, alpha: u32, opacity: f32, bias: f32, tiling: f32, edge_mode: f32, edge_x: f32, edge_z: f32, edge_a: f32, edge_b: f32, edge_c: f32, ox: f32, oy: f32, oz: f32, ou: f32, ov: f32, step_p: f32, step_u: f32, step_v: f32, index: u32, firsts: u32, payload: u32 }

// A lit surface at one pixel: its interpolated attributes, the derivatives
// of its world position and of its tiled UVs, and the pick target's handle
// and depth bits. The hardware paths fill it from their varyings and quad
// derivatives, the visibility buffer's resolve from the triangle it
// rebuilds, the impostor bake from the mesh in model space. What shades the
// pixel takes every derivative from here: under the resolve there are no
// others.
struct LitPixel {
    world: vec3<f32>,
    color: vec3<f32>,
    // The shading normal before any map bends it, facing the viewer: the
    // mesh's own where it has normals, the face's otherwise
    // (surface_normal).
    normal: vec3<f32>,
    dp: array<vec3<f32>, 2>,
    // The tiled UVs and their derivatives.
    uv: vec2<f32>,
    duv: array<vec2<f32>, 2>,
    // The mesh's own UVs, untiled, and the footprint its texels were
    // sampled over on them: what a splat's layers tile from.
    mesh_uv: vec2<f32>,
    mesh_duv: array<vec2<f32>, 2>,
    pick: u32,
    // The row's instance word (p_words); 0 for the kit's own meshes.
    word: u32,
}

// The material's albedo (decoded; alpha 1 when the mesh ignores it), normal
// map and surface map texels, sampled by the caller: a rasterised draw
// (pieces.wgsl) with its implicit footprint; the resolve with the footprint
// it derives.
struct LitTexels { albedo: vec4<f32>, normal: vec4<f32>, surface: vec4<f32> }

// Surface families: what a lit placement's pixels are made of. A family is
// three functions over plain values; the registry appends every family's
// source to the module and generates the dispatchers that switch on a
// material's family:
//   family_coverage(material, c: Cover) -> f32
//     How much of the pixel the surface covers, under the mesh's cut
//     (coverage_cut, a half unless it sets one) cut away. On
//     any stage: depth passes, shadow maps and the resolve call it.
//   family_surface(material, p: LitPixel, t: LitTexels) -> Lit
//     The lit surface and its own light. Derivatives come from `p` alone:
//     the resolve shades pixels away from their triangles. The impostor
//     bake calls it too, for families that say they bake (`Family::bake`):
//     there `p.world` is the mesh's model space, `pick` and `word` are 0,
//     colours stay as stored (the impostor decodes what it draws) and the
//     light it returns is dropped.
//   family_light(material, p: LitPixel, s: Lit) -> vec3<f32>
//     The surface lit: the kit's `shade` (lit_light) unless the family
//     brings its own.
// The kit's own family, `lit`, is id 0 and what an unknown id draws as.
// `material` is the kit's header for the mesh; a family's own words are its
// payload (family_word).

// What a family's coverage sees of a pixel: the tiled UVs and the albedo's
// texel as stored, sampled by the caller over the footprint its stage has.
struct Cover { uv: vec2<f32>, albedo: vec4<f32> }

fn family_of(material: GpuDraw) -> u32 { return (material.flags >> 16u) & 0xffu; }

// Surface::specular for `material`: flags bits 12-14 (gpu::MeshData::specular).
fn material_specular(material: GpuDraw) -> u32 { return (material.flags >> 12u) & 7u; }

// Whether the surface is cut away at a pixel: its family's coverage under
// the mesh's cut (gpu::MeshData::alpha_ref), unless the mesh draws whole whatever its albedo's alpha holds
// (flags bit 1). The caller discards or skips: the resolve has no fragment
// to discard.
fn cut_away(material: GpuDraw, uv: vec2<f32>, albedo: vec4<f32>) -> bool {
    return (material.flags & 2u) == 0u && family_coverage(material, Cover(uv, albedo)) < coverage_cut(material);
}

// `lit`'s light, and every family's that brings none of its own: the
// scene's `shade` (gpu::Lighting), given what the surface gives off.
// One vertex of a placement as a family's vertex stage (`<name>_vertex`)
// takes it: its local position (after thinning), where the kit places it
// (terrain adaption included), its part's centre and rank (w below 0 for a
// mesh without parts) and the row's transform. The stage returns its world
// position.
struct FamilyVertex { local: vec3<f32>, world: vec3<f32>, part: vec4<f32>, row: mat4x4<f32> }

// `lit` places vertices as the kit does.
fn lit_vertex(material: GpuDraw, v: FamilyVertex) -> vec3<f32> {
    return v.world;
}

fn lit_light(material: GpuDraw, p: LitPixel, s: Lit) -> vec3<f32> {
    return shade(p.world, s.surface, s.light, (material.flags & 1u) != 0u);
}
