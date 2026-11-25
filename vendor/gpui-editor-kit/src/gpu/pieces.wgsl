// Surface families drawn mesh by mesh (placed.rs `Module::Pieces`): the
// kit's own meshes (`Scene` batches), a mesh's family, payload and slots
// from its own bind group (gpu.rs `mesh_with`) instead of the arena's
// arrays, and the lit meshes and shadow casters that draw through them.
// Placements draw from the arena alone. After gpu.wgsl, the scene's
// lighting and family.wgsl.

// The mesh's family, how many slots it brings, and its payload
// (gpu::MeshData::family, slots, payload; placed.rs MAX_PAYLOAD words).
struct PieceFamily { id: u32, slots: u32, words: array<vec4<u32>, 4> }
@group(1) @binding(23) var<uniform> piece: PieceFamily;
// The mesh's family slots, white where it brings none (placed.rs MAX_SLOTS).
@group(1) @binding(24) var t_slot0: texture_2d<f32>;
@group(1) @binding(25) var t_slot1: texture_2d<f32>;
@group(1) @binding(26) var t_slot2: texture_2d<f32>;
@group(1) @binding(27) var t_slot3: texture_2d<f32>;
@group(1) @binding(28) var t_slot4: texture_2d<f32>;
@group(1) @binding(29) var t_slot5: texture_2d<f32>;
@group(1) @binding(30) var t_slot6: texture_2d<f32>;
@group(1) @binding(31) var t_slot7: texture_2d<f32>;

// The slots family_texel reads, as piece_material numbers them: 0 white,
// then the mesh's own maps, then its family slots from PIECE_SLOTS.
const PIECE_ALBEDO: u32 = 1u;
const PIECE_NORMAL: u32 = 2u;
const PIECE_SURFACE: u32 = 3u;
const PIECE_ALPHA: u32 = 4u;
const PIECE_SLOTS: u32 = 8u;

// The fragment fs_lit shades, for what `lit` takes beyond a LitPixel: the
// grid overlay's cover and where the instance stands (Output.cover,
// .origin).
var<private> piece_in: Output;

// The header a family reads of this mesh: its Params as the arena packs a
// placement's (family.wgsl GpuDraw), with its own slot numbers.
fn piece_material() -> GpuDraw {
    var m: GpuDraw;
    m.albedo = PIECE_ALBEDO;
    m.flags = select(0u, 1u, params.flags.x > 0.5)
        | select(0u, 2u, params.flags.w > 0.5)
        | (u32(params.flags.y) << 12u)
        | select(0u, 2048u, piece.slots > 0u)
        | (piece.id << 16u);
    m.gain = params.tone.x;
    m.maps = select(0u, PIECE_NORMAL, params.tone.z > 0.5) | select(0u, PIECE_SURFACE << 16u, params.tone.w > 0.5);
    m.tint = params.extra.x;
    m.alpha = PIECE_ALPHA;
    m.tiling = params.blend.y;
    return m;
}

// Word `k` of the piece's payload (gpu::MeshData::payload).
fn family_word(material: GpuDraw, k: u32) -> u32 {
    return piece.words[k / 4u][k % 4u];
}

// Family slot `k` of the piece, for family_texel: 0, white, where the mesh
// brings none.
fn family_slot(material: GpuDraw, k: u32) -> u32 {
    return select(0u, PIECE_SLOTS + k, k < piece.slots);
}

// A texel of the piece's texture `slot` at `uv`, over the footprint `duv`:
// what placed.wgsl's family_texel is to the arena.
fn family_texel(slot: u32, uv: vec2<f32>, duv: array<vec2<f32>, 2>) -> vec4<f32> {
    switch slot {
        case 1u: { return textureSampleGrad(t_albedo, s_albedo, uv, duv[0], duv[1]); }
        case 2u: { return textureSampleGrad(t_normal_map, s_albedo, uv, duv[0], duv[1]); }
        case 3u: { return textureSampleGrad(t_surface_map, s_albedo, uv, duv[0], duv[1]); }
        case 4u: { return textureSampleGrad(t_alpha_map, s_albedo, uv, duv[0], duv[1]); }
        case 8u: { return textureSampleGrad(t_slot0, s_albedo, uv, duv[0], duv[1]); }
        case 9u: { return textureSampleGrad(t_slot1, s_albedo, uv, duv[0], duv[1]); }
        case 10u: { return textureSampleGrad(t_slot2, s_albedo, uv, duv[0], duv[1]); }
        case 11u: { return textureSampleGrad(t_slot3, s_albedo, uv, duv[0], duv[1]); }
        case 12u: { return textureSampleGrad(t_slot4, s_albedo, uv, duv[0], duv[1]); }
        case 13u: { return textureSampleGrad(t_slot5, s_albedo, uv, duv[0], duv[1]); }
        case 14u: { return textureSampleGrad(t_slot6, s_albedo, uv, duv[0], duv[1]); }
        case 15u: { return textureSampleGrad(t_slot7, s_albedo, uv, duv[0], duv[1]); }
        default: { return vec4<f32>(1.0); }
    }
}

// `family_texel` at mip `level` of the texture, whatever the footprint.
fn family_texel_level(slot: u32, uv: vec2<f32>, level: f32) -> vec4<f32> {
    switch slot {
        case 1u: { return textureSampleLevel(t_albedo, s_albedo, uv, level); }
        case 2u: { return textureSampleLevel(t_normal_map, s_albedo, uv, level); }
        case 3u: { return textureSampleLevel(t_surface_map, s_albedo, uv, level); }
        case 4u: { return textureSampleLevel(t_alpha_map, s_albedo, uv, level); }
        case 8u: { return textureSampleLevel(t_slot0, s_albedo, uv, level); }
        case 9u: { return textureSampleLevel(t_slot1, s_albedo, uv, level); }
        case 10u: { return textureSampleLevel(t_slot2, s_albedo, uv, level); }
        case 11u: { return textureSampleLevel(t_slot3, s_albedo, uv, level); }
        case 12u: { return textureSampleLevel(t_slot4, s_albedo, uv, level); }
        case 13u: { return textureSampleLevel(t_slot5, s_albedo, uv, level); }
        case 14u: { return textureSampleLevel(t_slot6, s_albedo, uv, level); }
        case 15u: { return textureSampleLevel(t_slot7, s_albedo, uv, level); }
        default: { return vec4<f32>(1.0); }
    }
}

// Where the instance `p` shades stands: its matrix's translation.
fn family_origin(p: LitPixel) -> vec3<f32> {
    return piece_in.origin;
}

// `texel` under `p`'s vertex colour, mixed by the grid overlay's cover
// (`covered`) as the kit's meshes compose them.
fn family_albedo(p: LitPixel, texel: vec3<f32>) -> vec3<f32> {
    return covered(texel, p.color, piece_in.cover);
}

// `t`: only the arena draws a mesh's splat.
fn family_splat(material: GpuDraw, p: LitPixel, t: LitTexels) -> LitTexels {
    return t;
}

// `lit` on the kit's meshes cuts as Anno's AlphaTest(smoothstep(0, 0.9, alpha),
// MATERIAL_ALPHA_REF) does (gpu.wgsl `cut`), not at the arena's half.
fn lit_coverage(material: GpuDraw, c: Cover) -> f32 {
    return smoothstep(0.0, 0.9, c.albedo.a);
}

// The mesh's reference on that smoothstep (gpu::MeshData::alpha_ref), a
// half without; the arena takes it back to raw alpha (gpu::alpha_cut).
fn coverage_cut(material: GpuDraw) -> f32 {
    return params.alpha.x;
}

// `lit`'s surface on the kit's meshes: its glow, then the composition every lit
// path shares (gpu.wgsl `lit`), with vertex colour mixed by the grid
// overlay's cover (`family_albedo`). No mesh splat: only the arena draws
// one.
fn lit_surface(material: GpuDraw, p: LitPixel, t: LitTexels) -> Lit {
    let glow = glowing(t.albedo.rgb, params.glow.rgb, select(0.0, t.normal.b, params.glow.w > 0.5));
    var l = lit(family_albedo(p, t.albedo.rgb), p.world, material.tint, glow, p.normal, p.dp, p.duv, t.normal, (material.maps & 0xffffu) != 0u, t.surface, (material.maps >> 16u) != 0u);
    l.surface.specular = material_specular(material);
    return l;
}

// The kit's lit meshes: their family's coverage cuts, its surface and
// light shade.
@fragment fn fs_lit(in: Output) -> Opaque {
    piece_in = in;
    let material = piece_material();
    let uv = tiled(in.uv, in.world, material.tiling);
    let dp = array<vec3<f32>, 2>(dpdx(in.world), dpdy(in.world));
    let duv = array<vec2<f32>, 2>(dpdx(uv), dpdy(uv));
    let texel = albedo(uv);
    let normal = textureSample(t_normal_map, s_albedo, uv);
    let surface = textureSample(t_surface_map, s_albedo, uv);
    // Cutout, not blending: foliage and fences sort correctly with depth writes.
    if cut_away(material, uv, texel) { discard; }
    let p = LitPixel(
        in.world, in.color,
        surface_normal(in.world, cross(dp[0], dp[1]), in.normal, (material.flags & 1u) != 0u),
        dp, uv, duv,
        in.uv,
        array<vec2<f32>, 2>(dpdx(in.uv), dpdy(in.uv)),
        picked(in),
        0u,
    );
    let t = LitTexels(vec4<f32>(decode(texel.rgb), texel.a), normal, surface);
    return shaded(family_light(material, p, family_surface(material, p, t)), in.world, picked(in));
}

// The kit's shadow casters write depth only; their family's coverage
// punches holes in the shadow as it cuts the mesh.
@fragment fn fs_piece_shadow(in: Output) {
    if cut_away(piece_material(), in.uv, albedo(in.uv)) { discard; }
}
