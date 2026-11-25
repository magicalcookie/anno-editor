// Instanced meshes, their materials, sun shadows, the sky, occlusion, the
// post pass and screen-space markers for gpu.rs, after the frame prelude
// (gpu/frame.wgsl). Vertices are pulled from storage buffers by index, so
// every mesh shares one pipeline layout. Bind groups: 0 the frame, 1 the
// mesh (material, geometry, textures), 2 the draw (instances, visible
// rows, pick handles).
struct Params {
    // Material color and opacity.
    surface: vec4<f32>,
    // x: has normals; y: gpu::Specular's bits (MeshData::specular); w:
    // ignore texture alpha.
    flags: vec4<f32>,
    // Blend: depth fade distance, fresnel strength, depth where it turns opaque.
    blend: vec4<f32>,
    // x: vertex color gain, so colors can brighten as well as darken.
    // z, w: 1 with a normal map (t_normal_map), a surface map
    // (t_surface_map).
    tone: vec4<f32>,
    // Displacement: grid origin x and z, sample spacing; w is 0 off, 1 on,
    // 2 on with UVs from world X/Z.
    terrain: vec4<f32>,
    // x: terrain tint amount (MeshData::terrain_tint). w: see vs_mesh.
    // Decals: surface.a is the opacity and blend.x 1 with an alpha map.
    // Lit and decals: blend.y is world tiling (gpu::tiling_code).
    extra: vec4<f32>,
    // A projected decal's edge: mode, extents x and z, then the three
    // parameters (gpu::DecalEdge::code, decal_edge).
    edge: vec4<f32>,
    edge2: vec4<f32>,
    // Lit and decals (gpu::MeshData::emissive): colour, w 1 when the
    // normal map's blue glows.
    glow: vec4<f32>,
    // x: the reference albedo alpha is cut at on smoothstep(0, 0.9, alpha)
    // (MeshData::alpha_ref, 0.5 without).
    alpha: vec4<f32>,
}
// x: 1 when instances write pick handles.
struct DrawIds { ids: vec4<u32> }
// `normal` is octahedral, two snorm16 halves.
struct GpuVertex { position: vec4<f32>, uv: vec2<f32>, color: u32, normal: u32 }
@group(1) @binding(0) var<uniform> params: Params;
@group(1) @binding(1) var<storage, read> b_vertices: array<GpuVertex>;
@group(1) @binding(2) var<storage, read> b_indices: array<u32>;
@group(1) @binding(3) var t_albedo: texture_2d<f32>;
// One R32Float texel per heightfield sample. Loaded, never filtered: float
// filtering is optional.
@group(1) @binding(10) var t_heights: texture_2d<f32>;
// Displaced meshes with a color map (params.blend.w): RGBA8 per height
// sample, read where vertices land on samples, or filtered per fragment
// by a ground program (ground_colors). Terrain patches (gpu.rs).
@group(1) @binding(11) var t_colors: texture_2d<f32>;
// Displaced meshes: the heights' slopes and second differences per sample,
// filtered (heights.rs ResidentHeights::slopes).
@group(1) @binding(12) var t_slopes: texture_2d<f32>;
// Lit meshes: tangent-space normals with gloss in alpha; metalness in red
// and occlusion in alpha (MeshData::normal_map, surface_map).
@group(1) @binding(13) var t_normal_map: texture_2d<f32>;
@group(1) @binding(14) var t_surface_map: texture_2d<f32>;
// A decal's coverage mask in green (MeshData::alpha_map).
@group(1) @binding(15) var t_alpha_map: texture_2d<f32>;
@group(2) @binding(0) var<uniform> draw: DrawIds;
@group(2) @binding(1) var<storage, read> b_instances: array<mat4x4<f32>>;

struct Output {
    @builtin(position) clip: vec4<f32>,
    @location(0) world: vec3<f32>,
    @location(1) uv: vec2<f32>,
    @location(2) color: vec3<f32>,
    // Markers: their shape.
    @location(3) @interpolate(flat) shape: f32,
    @location(4) normal: vec3<f32>,
    // 0 for nothing pickable; placements set the top bit over their row.
    @location(5) @interpolate(flat) pick: u32,
    // A placed vertex's own X and Z as a projected decal's 0..1 box
    // coordinate, and 1 when the box lies diagonally (decal_edge).
    @location(6) boxed: vec3<f32>,
    // Terrain under a grid overlay: how far the overlay color covers the
    // ground texture instead of multiplying it (the color map's alpha).
    @location(7) cover: f32,
    // Meshes: where the instance stands, its matrix's translation, for a
    // family that tells placements apart (pieces.wgsl family_origin).
    @location(8) @interpolate(flat) origin: vec3<f32>,
}

fn octahedral(v: u32) -> vec3<f32> {
    let e = unpack2x16snorm(v);
    var n = vec3<f32>(e.x, e.y, 1.0 - abs(e.x) - abs(e.y));
    let t = max(-n.z, 0.0);
    n.x += select(t, -t, n.x >= 0.0);
    n.y += select(t, -t, n.y >= 0.0);
    return n;
}

// Bilinear height at fractional sample coordinates, clamped to the grid.
// Exact on samples, where the fraction is zero.
fn height(p: vec2<f32>) -> f32 {
    let last = vec2<i32>(textureDimensions(t_heights)) - 1;
    let q = clamp(p, vec2<f32>(0.0), vec2<f32>(last));
    let b = vec2<i32>(floor(q));
    let f = q - floor(q);
    let e = min(b + 1, last);
    let h00 = textureLoad(t_heights, b, 0).r;
    let h10 = textureLoad(t_heights, vec2<i32>(e.x, b.y), 0).r;
    let h01 = textureLoad(t_heights, vec2<i32>(b.x, e.y), 0).r;
    let h11 = textureLoad(t_heights, e, 0).r;
    return mix(mix(h00, h10, f.x), mix(h01, h11, f.x), f.y);
}

// The bilinear heights at sample position `p` and a sample to either
// side: centre, -X, +X, -Z, +Z. Away from the grid's edge the five
// lookups' footprints overlap: twelve samples, each read once, where
// height() reads twenty, with the same fractions and texels.
struct Cross {
    h: f32,
    x0: f32,
    x1: f32,
    z0: f32,
    z1: f32,
}

fn height_cross(p: vec2<f32>) -> Cross {
    let last = vec2<f32>(textureDimensions(t_heights)) - 1.0;
    if all(p >= vec2<f32>(1.0)) && all(p < last - 1.0) {
        let b = vec2<i32>(floor(p));
        let f = p - floor(p);
        var r0: array<f32, 4>;
        var r1: array<f32, 4>;
        for (var i = 0; i < 4; i++) {
            r0[i] = textureLoad(t_heights, b + vec2<i32>(i - 1, 0), 0).r;
            r1[i] = textureLoad(t_heights, b + vec2<i32>(i - 1, 1), 0).r;
        }
        let up = vec2<f32>(textureLoad(t_heights, b + vec2<i32>(0, -1), 0).r, textureLoad(t_heights, b + vec2<i32>(1, -1), 0).r);
        let down = vec2<f32>(textureLoad(t_heights, b + vec2<i32>(0, 2), 0).r, textureLoad(t_heights, b + vec2<i32>(1, 2), 0).r);
        return Cross(
            mix(mix(r0[1], r0[2], f.x), mix(r1[1], r1[2], f.x), f.y),
            mix(mix(r0[0], r0[1], f.x), mix(r1[0], r1[1], f.x), f.y),
            mix(mix(r0[2], r0[3], f.x), mix(r1[2], r1[3], f.x), f.y),
            mix(mix(up.x, up.y, f.x), mix(r0[1], r0[2], f.x), f.y),
            mix(mix(r1[1], r1[2], f.x), mix(down.x, down.y, f.x), f.y),
        );
    }
    return Cross(
        height(p),
        height(p - vec2<f32>(1.0, 0.0)),
        height(p + vec2<f32>(1.0, 0.0)),
        height(p - vec2<f32>(0.0, 1.0)),
        height(p + vec2<f32>(0.0, 1.0)),
    );
}

// A displaced terrain's normal (xyz) and cavity (w) under world X/Z, from
// the heights a sample each way: shaded per pixel at the heights' own
// resolution, as Anno's terrain shades from its node normal map, so ground
// keeps its relief where the mesh strides over samples far off. The cavity
// is terrain.fxh's (CAVITY_MAP_ENBLED): saturate(4 (n(x+1) - n(x-1)) + 1)
// along X times the same along Z, n the normal map's texels a sample
// apart; that difference of normals is the heights' second difference.
// Both are linear in the heights, so they come filtered from t_slopes.
fn terrain_detail(xz: vec2<f32>) -> vec4<f32> {
    let p = (xz - params.terrain.xy) / params.terrain.z;
    let s = textureSampleLevel(t_slopes, s_pages, (p + 0.5) / vec2<f32>(textureDimensions(t_slopes)), 0.0);
    let n = normalize(vec3<f32>(-s.x, 1.0, -s.y));
    // n.x over a sample each way changes by -(slope change) times n.y.
    let curve_x = s.z;
    let curve_z = s.w;
    let cavity = clamp(1.0 - 4.0 * curve_x * n.y, 0.0, 1.0) * clamp(1.0 - 4.0 * curve_z * n.y, 0.0, 1.0);
    return vec4<f32>(n, cavity);
}

// How much the cavity darkens the ground (terrain.fxh: lerp(1, cavity,
// 0.35) on the albedo, after tint).
const CAVITY: f32 = 0.35;

// Anno's terrain adaption (model_footer.fxh, ApplyTerrainAdaption in
// functions_raytrace_model.fxh), for a placed vertex at `world`. The row's
// bottom row (gpu::adapting) says whether the placement may bend (x), the
// piece origin's height above the object's (y) and whether the piece rides
// the ground at its own origin (z). `mode` is gpu::adapt_code: relative
// keeps the vertex's height above the object's origin, measured from the
// ground under the vertex; absolute puts it on the ground. The vertex
// color's top byte is how rigid the vertex stays: the game's blue channel,
// strength 1 - b, so a foundation's foot drapes and its top stays level.
// Normals stay as authored.
fn adapt_to_terrain(world: vec3<f32>, m: mat4x4<f32>, color: u32, mode: u32) -> vec3<f32> {
    // Draped decals lie on the ground wherever they are placed.
    if mode == 3u && lights.terrain.w > 0.5 {
        return vec3<f32>(world.x, ground_height(world.xz), world.z);
    }
    if lights.terrain.w < 0.5 || m[0].w < 0.5 { return world; }
    var out = world;
    if m[2].w > 0.5 {
        out.y += ground_height(m[3].xz) + m[1].w - m[3].y;
    }
    if mode == 0u { return out; }
    let object_y = m[3].y - m[1].w;
    let g = ground_height(out.xz);
    // Absolute vertices would lie in the ground and z-fight with it; the
    // game biases them too (0.1 m in its ray-traced path).
    let target_y = select(g + (out.y - object_y), g + 0.02, mode == 2u);
    out.y = mix(out.y, target_y, 1.0 - f32(color >> 24u) / 255.0);
    return out;
}

// The presented terrain's normal under world X/Z, from central differences
// half a metre each way, as decal_pbr.fx shades a draped decal.
fn ground_normal(xz: vec2<f32>) -> vec3<f32> {
    let dx = ground_height(xz + vec2<f32>(0.5, 0.0)) - ground_height(xz - vec2<f32>(0.5, 0.0));
    let dz = ground_height(xz + vec2<f32>(0.0, 0.5)) - ground_height(xz - vec2<f32>(0.0, 0.5));
    return normalize(vec3<f32>(-dx, 1.0, -dz));
}

// Where a decal's vertex at `world` draws: moved toward the viewer along
// the line of sight, so only its depth changes. A positive `bias` grows over
// the cosine of the view's angle from straight up (capped at 10x), as 117's
// ApplyCameraAngleDepthBias does; a negative one moves -bias, as 1800's
// ApplyDepthBias (gpu::Decal::lift). Plus a millimetre per metre of
// distance, the kit's own, so it stays above the coarser terrain far off.
fn decal_lift(world: vec3<f32>, bias: f32) -> vec3<f32> {
    var far = 0.0;
    if globals.eye.w > 0.5 { far = distance(globals.eye.xyz, world); }
    let toward = toward_viewer(world);
    let lift = select(bias / max(0.1, toward.y), -bias, bias < 0.0);
    return world + toward * (lift + far * 1e-3);
}

// A placed vertex's box coordinate for decal_edge: its own X and Z (the
// box's unit grid) as 0..1, V running against Z as the texture does, and
// whether the row's X axis lies diagonally (decal_pbr.fx's isDiagonal).
fn boxed(local: vec3<f32>, m: mat4x4<f32>) -> vec3<f32> {
    let x = normalize(m[0].xyz);
    let diagonal = abs(abs(x.z) - abs(x.x)) < 0.2;
    return vec3<f32>(local.x + 0.5, 0.5 - local.z, select(0.0, 1.0, diagonal));
}

// A projected decal's coverage toward its box's sides (decal_pbr.fx
// FadeDecalBorder and the alpha-based fade). `edge` is mode, extents x and
// z, first parameter; `more` the other two; `height` the height map's red.
fn decal_edge(boxed: vec3<f32>, edge: vec4<f32>, more: vec2<f32>, height: f32) -> f32 {
    let mode = u32(edge.x + 0.5);
    if mode == 0u { return 1.0; }
    let extents = edge.yz;
    var grid = floor(extents * 2.0 - 0.001);
    if boxed.z > 0.5 { grid = floor(grid / 0.70710678 + 0.5) * 0.70710678; }
    let dist = abs(boxed.xy * 2.0 - 1.0) * extents;
    if mode == 3u {
        let from_border = (grid * 0.5 + edge.w - dist) / more.x;
        // SmoothMin(a, b, 0.2) from functions.fxh.
        let k = 0.8;
        let h = max(k - abs(from_border.x - from_border.y), 0.0) / k;
        let near = min(from_border.x, from_border.y) - h * h * k * 0.25;
        return smoothstep(0.0, 1.0, clamp(near, 0.0, 1.0));
    }
    if any(grid <= vec2<f32>(1.5)) { return 1.0; }
    let past = dist - grid * 0.5;
    let fade = clamp(1.0 - max(past.x, past.y) / (edge.w + 0.0001), 0.0, 1.0);
    if mode == 2u {
        if fade >= 1.0 { return 1.0; }
        return clamp((height - (1.0 - fade) * more.y - more.x) / 0.06, 0.0, 1.0);
    }
    return smoothstep(0.0, 1.0, fade);
}

// Texture UVs under world tiling `code` (gpu::tiling_code): world X/Z
// over the scale, along the diagonals when negative; the mesh's otherwise.
fn tiled(uv: vec2<f32>, world: vec3<f32>, code: f32) -> vec2<f32> {
    if code == 0.0 { return uv; }
    var xz = world.xz * vec2<f32>(1.0, -1.0);
    if code < 0.0 { xz = vec2<f32>(world.x + world.z, world.z - world.x); }
    return xz / abs(code);
}

// A decal's coverage: opacity, texture alpha, and the alpha map's green
// through a smoothstep (functions_pbr.fxh, FORCE_ALPHA_BLEND).
fn decal_coverage(opacity: f32, texel_alpha: f32, mask: f32) -> f32 {
    return opacity * texel_alpha * smoothstep(0.0, 1.0, mask);
}

// Drawn indexed, so `index` is the vertex itself.
@vertex fn vs_mesh(@builtin(vertex_index) index: u32, @builtin(instance_index) instance: u32) -> Output {
    return mesh_vertex(index, instance);
}

// What a ground fragment takes from its vertices: a ground program's
// fragments find their normal and cavity from the heights
// (terrain_detail), read the color map themselves (ground_colors) and are
// never picked. Every attribute a vertex hands on held the ground's pixel
// warps back: the full Output's thirteen floats against these five kept
// an RTX 4080 SUPER at about ten pixel warps a TPC, and dropping eight
// took the ground from 1.67 to 1.38 ms on pirate and 2.07 to 1.66 ms on
// continental_overview at 5K.
struct GroundVertex {
    @builtin(position) clip: vec4<f32>,
    @location(0) world: vec3<f32>,
    @location(1) uv: vec2<f32>,
}

@vertex fn vs_ground(@builtin(vertex_index) index: u32, @builtin(instance_index) instance: u32) -> GroundVertex {
    let v = mesh_vertex(index, instance);
    return GroundVertex(v.clip, v.world, v.uv);
}

fn mesh_vertex(index: u32, instance: u32) -> Output {
    let vertex = b_vertices[index];
    var pick = 0u;
    if draw.ids.x == 1u {
        // Batch instances index the scene's one instance buffer; 0 is none.
        pick = instance + 1u;
    }
    let m = b_instances[instance];
    let displaced = params.terrain.w > 0.5;
    var local = vertex.position;
    // Terrain skirts mark y far below anything real and drop by the rest
    // (gpu.rs SKIRT); other displaced meshes ignore y.
    var drop = 0.0;
    if displaced {
        if local.y < -0.5e6 { drop = local.y + 1e6; }
        local.y = 0.0;
    }
    var world = (m * local).xyz;
    var out: Output;
    out.uv = vertex.uv;
    out.normal = vec3<f32>(0.0);
    if displaced {
        let spacing = params.terrain.z;
        let p = (world.xz - params.terrain.xy) / spacing;
        let c = height_cross(p);
        world.y = c.h + drop;
        // Central differences one sample each way; at the edge the clamp
        // turns them one-sided, as the CPU tile normals did.
        let dx = (c.x1 - c.x0) / (2.0 * spacing);
        let dz = (c.z1 - c.z0) / (2.0 * spacing);
        out.normal = normalize(vec3<f32>(-dx, 1.0, -dz));
        if params.terrain.w > 1.5 {
            out.uv = p / max(vec2<f32>(textureDimensions(t_heights)) - 1.0, vec2<f32>(1.0));
        }
        // Relief: the ground raised or sunk along its normal by its
        // displacement, as Anno's terrain vertex shader does
        // (SampleMaterialDisplacement, from the baked height's mip 2). Read
        // over the footprint the pixels there filter it over, so the
        // raised shape matches the shading; near the camera the patches
        // (terrain.rs TERRAIN_STRIDES) are fine enough to carry that, down
        // to a quarter sample.
        if relief() && drop == 0.0 {
            let footprint = max(spacing * 0.25, pixel_size(world) * RELIEF_FOOTPRINT);
            let dwv = array<vec3<f32>, 2>(vec3<f32>(footprint, 0.0, 0.0), vec3<f32>(0.0, 0.0, footprint));
            world += out.normal * ground_displacement(out.uv, world, out.normal, dwv);
        }
    }
    out.clip = globals.clip * vec4<f32>(world, 1.0);
    out.world = world;
    out.color = rgb(vertex.color) * params.tone.x;
    // Blend meshes (params.extra.w): the color's top byte is transparency.
    if params.extra.w > 0.5 && !displaced {
        out.cover = f32(vertex.color >> 24u) / 255.0;
    }
    if displaced && params.blend.w > 0.5 {
        let spacing = params.terrain.z;
        let last = vec2<i32>(textureDimensions(t_colors)) - 1;
        let sample = clamp(vec2<i32>(round((world.xz - params.terrain.xy) / spacing)), vec2<i32>(0), last);
        let texel = textureLoad(t_colors, sample, 0);
        out.color = texel.rgb * params.tone.x;
        out.cover = 1.0 - texel.a;
    }
    out.origin = m[3].xyz;
    out.pick = pick;
    if params.flags.x > 0.5 && !displaced {
        // Cofactor matrix: the inverse transpose up to scale, so sheared and
        // non-uniformly scaled instances keep correct normals.
        let c0 = m[0].xyz;
        let c1 = m[1].xyz;
        let c2 = m[2].xyz;
        let cofactor = mat3x3<f32>(cross(c1, c2), cross(c2, c0), cross(c0, c1));
        out.normal = cofactor * octahedral(vertex.normal) * sign(dot(c0, cross(c1, c2)));
    }
    return out;
}

fn toward_viewer(world: vec3<f32>) -> vec3<f32> {
    if globals.eye.w > 0.5 { return normalize(globals.eye.xyz - world); }
    return globals.eye.xyz;
}

// Shading normal, turned toward the viewer. Without authored normals it comes
// from screen derivatives: game meshes often ship normals in packed formats
// this renderer does not decode, and editors want facets visible. Derived
// normals have no outward side; single-sided foliage has the same problem.
fn surface_normal(world: vec3<f32>, derived: vec3<f32>, normal: vec3<f32>, has_normals: bool) -> vec3<f32> {
    var n = normalize(derived);
    if has_normals && dot(normal, normal) > 1e-12 { n = normalize(normal); }
    if dot(n, toward_viewer(world)) < 0.0 { n = -n; }
    return n;
}

// Albedo and authored colors are sRGB; a toned scene shades them linear.
// Ground albedo under the vertex color: multiplied, as tint is, or for
// overlay cells mostly the color itself. Multiplied alone, a green tree
// layer vanished into green grass and anything on dark rock read black.
// Tint and vertex colors can brighten; albedo stops at 1, as Anno's
// saturate(albedo * tint).
fn covered(texel: vec3<f32>, color: vec3<f32>, cover: f32) -> vec3<f32> {
    return mix(min(texel * color, vec3<f32>(1.0)), decode(color), cover);
}

// Display-space scenes: albedo times hemisphere ambient plus sun. Toned
// scenes: Lambert plus GGX from the sun (Schlick-Smith visibility, Schlick
// Fresnel), the hemisphere for diffuse ambient and, along the reflection,
// for specular ambient. Light is radiance times albedo times N·L with no
// 1/π, as Anno's GetAnalyticalDiffuse. Both add the scene's local lights,
// diffuse only (local_light). Fog is the caller's. The scene's
// `shade` unless a host replaces it (gpu::Lighting).
// TODO: which AO reaches `shade` is open: this
// takes material AO (`s.occlusion`); screen-space AO runs at the Occlusion
// stage after Opaque and multiplies the finished colour, glow included,
// which 2070's light blend cannot tell apart.
fn kit_shade(world: vec3<f32>, s: Surface, has_normals: bool) -> vec3<f32> {
    let n = s.normal;
    var facing = dot(n, lights.sun.xyz);
    // Derived normals face the viewer, not outward; light them two-sided.
    if !has_normals { facing = abs(facing); }
    let ndl = clamp(facing, 0.0, 1.0);
    let hemisphere = sky_light(n);
    if !toned() {
        var lit = hemisphere * s.occlusion;
        if ndl > 0.0 { lit += lights.sun_color.rgb * ndl * shadow(world, s.geometry); }
        if has_local_lights() { lit += local_light(world, n, !has_normals, wrap_local); }
        return s.albedo * lit;
    }
    let v = toward_viewer(world);
    let rough = clamp(1.0 - s.gloss, 0.04, 1.0);
    let diffuse = s.albedo * (1.0 - s.metal);
    let f0 = mix(vec3<f32>(0.04), s.albedo, s.metal);
    let ndv = max(dot(n, v), 1e-4);
    // TODO: glossy surfaces should see the sky map itself along r, blurred
    // by roughness (Anno prefilters its sky cube); the diffuse light is a
    // rough surface's reflection.
    let around = sky_light(reflect(-v, n));
    // The ambient factor scales the sky's diffuse light only, as Anno's
    // NoRTAmbientFactor does; reflections keep the full sky.
    // gpu::Specular: Anno's BRDF_DISABLE_SPEC_ON_ZERO_GLOSS takes both
    // terms out by LinearStep(0, 0.25, gloss) (functions_pbr_lighting.fxh).
    let fade = select(1.0, clamp(s.gloss * 4.0, 0.0, 1.0), (s.specular & 4u) != 0u);
    let sky = select(fade, 0.0, (s.specular & 1u) != 0u);
    let sun_spec = select(fade, 0.0, (s.specular & 2u) != 0u);
    var color = (diffuse * hemisphere * lights.tone.z + around * env_brdf(f0, rough, ndv) * sky) * s.occlusion;
    if ndl > 0.0 {
        let h = normalize(v + lights.sun.xyz);
        let a = rough * rough;
        let a2 = a * a;
        let ndh = clamp(dot(n, h), 0.0, 1.0);
        let d = a2 / (3.14159265 * pow(ndh * ndh * (a2 - 1.0) + 1.0, 2.0));
        let k = a * 0.5;
        let visibility = 0.25 / ((ndl * (1.0 - k) + k) * (ndv * (1.0 - k) + k));
        let fresnel = f0 + (1.0 - f0) * pow(1.0 - clamp(dot(v, h), 0.0, 1.0), 5.0);
        let sun = lights.sun_color.rgb * ndl * shadow(world, s.geometry);
        color += sun * (diffuse * mix(1.0, s.occlusion, lights.tone.y) + d * visibility * fresnel * sun_spec);
    }
    if has_local_lights() { color += diffuse * local_light(world, n, !has_normals, wrap_local); }
    return color;
}

// Set by fs_blend before it shades: blended sheets (particles) take the
// scene's local lights wrapped round them (local_light).
var<private> wrap_local: bool = false;

fn has_ground_tint() -> bool { return lights.ground_tint.z > 0.0; }

// The ground tint map's texel under `world` (gpu::GroundTint), filtered as
// stored, the edge texels held off the map.
fn ground_tint_texel(world: vec3<f32>) -> vec4<f32> {
    let half = 0.5 / vec2<f32>(textureDimensions(t_tint));
    let uv = clamp((world.xz - lights.ground_tint.xy) * lights.ground_tint.zw, half, 1.0 - half);
    return textureSampleLevel(t_tint, s_albedo, uv, 0.0);
}

// The ground tint's multiplier under `world`: its texel as the scene's
// lighting decodes it (gpu::Lighting::tint, `ground_tint_decode`).
fn ground_tint(world: vec3<f32>) -> vec3<f32> {
    return ground_tint_decode(ground_tint_texel(world));
}

// `albedo` times the ground's tint under `world` mixed in by `amount`,
// kept at most 1: the ground tint map's where the scene has one
// (ground_tint), else the terrain's colours (bilinear between height
// samples, times its colour gain). Off the grid the edge texels hold.
fn tinted(albedo: vec3<f32>, world: vec3<f32>, amount: f32) -> vec3<f32> {
    if amount <= 0.0 { return albedo; }
    if has_ground_tint() {
        return clamp(albedo * mix(vec3<f32>(1.0), ground_tint(world), amount), vec3<f32>(0.0), vec3<f32>(1.0));
    }
    if lights.tint.w <= 0.0 { return albedo; }
    let last = vec2<i32>(textureDimensions(t_ground_tint)) - 1;
    let q = clamp((world.xz - lights.tint.xy) / lights.tint.z, vec2<f32>(0.0), vec2<f32>(last));
    let b = vec2<i32>(floor(q));
    let f = q - floor(q);
    let e = min(b + 1, last);
    let c00 = textureLoad(t_ground_tint, b, 0).rgb;
    let c10 = textureLoad(t_ground_tint, vec2<i32>(e.x, b.y), 0).rgb;
    let c01 = textureLoad(t_ground_tint, vec2<i32>(b.x, e.y), 0).rgb;
    let c11 = textureLoad(t_ground_tint, e, 0).rgb;
    let c = mix(mix(c00, c10, f.x), mix(c01, c11, f.x), f.y) * lights.tint.w;
    return clamp(albedo * mix(vec3<f32>(1.0), c, amount), vec3<f32>(0.0), vec3<f32>(1.0));
}

// A tangent-space normal map texel bent onto `n` with the cotangent frame
// of the fragment's position and UV derivatives (Schüler, "Normal Mapping
// Without Precomputed Tangents"): no vertex tangents, and mirrored UVs
// mirror the frame with them. The determinant's sign is kept: screen Y
// runs down here, which the original's GL derivation does not have.
fn bend(n: vec3<f32>, dp: array<vec3<f32>, 2>, duv: array<vec2<f32>, 2>, texel: vec4<f32>) -> vec3<f32> {
    let a = cross(dp[1], n);
    let b = cross(n, dp[0]);
    let t = a * duv[0].x + b * duv[1].x;
    let bt = a * duv[0].y + b * duv[1].y;
    let det = dot(dp[0], a);
    let scale = select(1.0, -1.0, det < 0.0) * inverseSqrt(max(max(dot(t, t), dot(bt, bt)), 1e-30));
    let xy = texel.xy * 2.0 - 1.0;
    let z = sqrt(clamp(1.0 - dot(xy, xy), 0.0, 1.0));
    let bent = (t * xy.x + bt * xy.y) * scale + n * z;
    if dot(bent, bent) < 1e-12 { return n; }
    return normalize(bent);
}

// `bend`'s frame as a matrix: tangent and bitangent scaled so the longer
// is a unit, then `n`. Its transpose takes a world direction back into the
// map's tangent space where the UVs map without shear, as for most meshes.
// Kept apart from `bend`, whose own order of operations the recorded
// frames pin to the bit.
fn cotangent(n: vec3<f32>, dp: array<vec3<f32>, 2>, duv: array<vec2<f32>, 2>) -> mat3x3<f32> {
    let a = cross(dp[1], n);
    let b = cross(n, dp[0]);
    let t = a * duv[0].x + b * duv[1].x;
    let bt = a * duv[0].y + b * duv[1].y;
    let det = dot(dp[0], a);
    let scale = select(1.0, -1.0, det < 0.0) * inverseSqrt(max(max(dot(t, t), dot(bt, bt)), 1e-30));
    return mat3x3<f32>(t * scale, bt * scale, n);
}

// A lit surface from its albedo, unbent normal and map texels; maps it
// does not have are ignored.
fn mapped(albedo: vec3<f32>, n: vec3<f32>, dp: array<vec3<f32>, 2>, duv: array<vec2<f32>, 2>, normal: vec4<f32>, has_normal: bool, surface: vec4<f32>, has_surface: bool) -> Surface {
    var s = plain(albedo, n);
    if has_normal {
        s.normal = bend(n, dp, duv, normal);
        s.gloss = normal.a;
    }
    if has_surface {
        s.metal = surface.r;
        s.occlusion = surface.a;
    }
    return s;
}

// Emissive masks fade in below this as their square: block compression
// leaves faint blocks in a black channel, which then stay dark.
const GLOW_FLOOR: f32 = 0.35;

fn glow_mask(m: f32) -> f32 {
    if m < GLOW_FLOOR { return m * m / GLOW_FLOOR; }
    return m;
}

// What a lit surface's own light makes of it (gpu::MeshData::emissive):
// `keep` how much of its albedo still shades, `light` what it gives off.
// `texel` is the decoded albedo texel, `color` the emissive colour and
// `mask` how far it glows (the normal map's blue, 0 without). The shading
// gives way to the light as the mask rises.
struct Glowing { keep: f32, light: vec3<f32> }

fn glowing(texel: vec3<f32>, color: vec3<f32>, mask: f32) -> Glowing {
    let strength = glow_mask(mask);
    return Glowing(1.0 - clamp(strength, 0.0, 1.0), texel * color * strength * lights.glow.w);
}

// A lit surface as `shade` takes it, and the light it gives off itself:
// `shade`'s emissive input, which the kit's adds to its light and a
// game's may let override it.
struct Lit { surface: Surface, light: vec3<f32> }

// The composition every lit path shares, over plain values: `albedo` (the
// texel under the vertex colour) takes the ground's tint under `world` by
// `tint`, then gives way to its own light; the normal and surface maps'
// texels bend `n` and fill in gloss, metal and occlusion where the mesh has
// them. The kit's meshes and decals (fs_lit, fs_decal) and placements from
// the arena (placed.wgsl) differ in how they find these values, not in
// this.
fn lit(albedo: vec3<f32>, world: vec3<f32>, tint: f32, glow: Glowing, n: vec3<f32>, dp: array<vec3<f32>, 2>, duv: array<vec2<f32>, 2>, normal: vec4<f32>, has_normal: bool, surface: vec4<f32>, has_surface: bool) -> Lit {
    let base = tinted(albedo, world, tint) * glow.keep;
    return Lit(mapped(base, n, dp, duv, normal, has_normal, surface, has_surface), glow.light);
}

// Albedo texel; alpha is 1 when the mesh says it stores something else.
// A mip sharper than the footprint asks, as Anno's GetDiffuseColor samples
// with its gradients halved (mipBiasFactor 0.5): lower mips average leaves
// with the gaps between them, and cut at 0.5 a far canopy thinned to bare
// branches.
fn albedo(uv: vec2<f32>) -> vec4<f32> {
    let texel = textureSampleGrad(t_albedo, s_albedo, uv, 0.5 * dpdx(uv), 0.5 * dpdy(uv));
    return select(texel, vec4<f32>(texel.rgb, 1.0), params.flags.w > 0.5);
}

// Whether a cutout texel is cut away: Anno's AlphaTest(smoothstep(0, 0.9,
// alpha), MATERIAL_ALPHA_REF), the reference the mesh's (MeshData::
// alpha_ref), 0.5 without.
fn cut(alpha: f32) -> bool {
    return smoothstep(0.0, 0.9, alpha) < params.alpha.x;
}

fn picked(in: Output) -> u32 {
    return in.pick;
}

// Decal placements without the arena: lit as fs_lit (pieces.wgsl),
// blended by coverage.
@fragment fn fs_decal(in: Output) -> @location(0) vec4<f32> {
    let uv = tiled(in.uv, in.world, params.blend.y);
    let dp = array<vec3<f32>, 2>(dpdx(in.world), dpdy(in.world));
    let duv = array<vec2<f32>, 2>(dpdx(uv), dpdy(uv));
    let texel = textureSample(t_albedo, s_albedo, uv);
    let normal = textureSample(t_normal_map, s_albedo, uv);
    let surface = textureSample(t_surface_map, s_albedo, uv);
    // A projected decal's alpha map is its height map, on the box.
    let map = textureSample(t_alpha_map, s_albedo, select(in.uv, in.boxed.xy, params.edge.x > 0.5));
    var mask = map.g;
    if params.blend.x < 0.5 || params.edge.x > 0.5 { mask = 1.0; }
    let edge = decal_edge(in.boxed, params.edge, params.edge2.xy, map.r);
    let coverage = decal_coverage(params.surface.a, texel.a, mask) * edge;
    if coverage <= 0.0 { discard; }
    let has_normals = params.flags.x > 0.5;
    let n = surface_normal(in.world, cross(dp[0], dp[1]), in.normal, has_normals);
    // Decals glow by their normal map alone.
    let decoded = decode(texel.rgb);
    let glow = glowing(decoded, params.glow.rgb, select(0.0, normal.b, params.glow.w > 0.5));
    let l = lit(decoded * in.color, in.world, params.extra.x, glow, n, dp, duv, normal, params.tone.z > 0.5, surface, params.tone.w > 0.5);
    return vec4<f32>(fog(shade(in.world, l.surface, l.light, has_normals), in.world), coverage);
}

// Unlit: no shade and no fog, but what a view mode's medium does to it
// (`ViewMode::unlit`).
@fragment fn fs_flat(in: Output) -> Opaque {
    return Opaque(vec4<f32>(unlit(in.color, in.world), 1.0), picked(in), pack_rgb9e5(in.color));
}

// Shadow casters write depth only; cutouts punch holes in the shadow too.
// The ground's (gpu/terrain.rs); meshes cast through their family's
// coverage (pieces.wgsl fs_piece_shadow).
@fragment fn fs_shadow(in: Output) {
    if cut(albedo(in.uv).a) { discard; }
}

struct GroundSample {
    color: vec4<f32>,
    // World normal bent by the normal layers, stacked like color.
    normal: vec3<f32>,
    // Glossiness from the gloss layers, stacked like color.
    gloss: f32,
    // Relief: the stacked displacement in world units (0 without).
    height: f32,
    // Light the ground gives off itself, 0 to 1: how far it replaces the
    // lighting (Ground::glow; 0 for grounds that don't declare it).
    glow: f32,
}

const RELIEF_FOOTPRINT: f32 = 2.0;

// World units one pixel of the view spans at `world`, for what a vertex
// reads without screen derivatives: the clip's vertical scale over the
// target's height, at the distance in perspective.
fn pixel_size(world: vec3<f32>) -> f32 {
    let row = vec3<f32>(globals.clip[0].y, globals.clip[1].y, globals.clip[2].y);
    let per_unit = length(row) * globals.viewport.y * 0.5;
    var d = 1.0;
    if globals.eye.w > 0.5 { d = distance(globals.eye.xyz, world); }
    return d / max(per_unit, 1e-6);
}

// The ground's normal bent by a height `h` (world units along `n`) that
// varies over the pixel: Mikkelsen's bump mapping from the height's screen
// derivatives, so the slope is the height's over the pixel's own footprint,
// as Anno's bake derives its page normals from the baked height per page
// texel (BUMP from cBakedHeightTexArray). Call in uniform control flow.
fn relief_normal(world: vec3<f32>, n: vec3<f32>, h: f32) -> vec3<f32> {
    let ss = dpdx(world);
    let st = dpdy(world);
    let r1 = cross(st, n);
    let r2 = cross(n, ss);
    let det = dot(ss, r1);
    let grad = sign(det) * (dpdx(h) * r1 + dpdy(h) * r2);
    let bent = abs(det) * n - grad;
    if dot(bent, bent) < 1e-20 { return n; }
    return normalize(bent);
}

// The XZ projection's tangent frame bent onto `n`, as a ground bends its
// normal map texels: a page stores its normal in this frame, so it turns
// with the ground drawn under it.
fn page_frame(n: vec3<f32>) -> array<vec3<f32>, 2> {
    let t = vec3<f32>(1.0, 0.0, 0.0);
    let b = vec3<f32>(0.0, 0.0, -1.0);
    let tt = normalize(t - n * dot(n, t));
    let bb = normalize(b - n * dot(n, b) - tt * dot(tt, b));
    return array<vec3<f32>, 2>(tt, bb);
}

// Ground at least as level as lights.pages.w (normal y, terrain.rs
// page_flat) takes its baked page where it has one; steeper shades live.
// Pages store their normal in their own frame (page_frame), so the ground
// drawn under it turns it.
// Page texels: payload and border per side at the finest mip
// (terrain.rs PAGE_TEXELS, PAGE_BORDER).
const PAGE_TEXELS: f32 = 256.0;
const PAGE_BORDER: f32 = 16.0;
// Pages a sheet's side, and a sheet's pages (terrain.rs PAGE_SHEET_SIDE).
const PAGE_SHEET_SIDE: f32 = 7.0;
const PAGE_SHEET: u32 = 49u;

// BEGIN GROUND
// No ground here: the kit's own pipelines never shade one. A ground program
// (terrain.rs GroundProgram) puts its relief and page features and the
// mounted ground's own WGSL (terrain::Ground::wgsl) in place of these.
fn relief() -> bool { return false; }

fn ground_glows() -> bool { return false; }
fn ground_normals() -> bool { return false; }
fn ground_specular() -> u32 { return 0u; }

fn ground_page_normal(dw: array<vec3<f32>, 2>) -> bool { return false; }

fn ground(uv: vec2<f32>, world: vec3<f32>, n: vec3<f32>, dw: array<vec3<f32>, 2>) -> GroundSample {
    return GroundSample(vec4<f32>(1.0), n, 0.0, 0.0, 0.0);
}

fn ground_displacement(uv: vec2<f32>, world: vec3<f32>, n: vec3<f32>, dw: array<vec3<f32>, 2>) -> f32 {
    return 0.0;
}
// END GROUND
// The baked page under `world`, as `ground` would shade it, or alpha -1
// where the view has none baked there (gpu/terrain.rs picks pages by how
// many pixels a metre of them covers).
fn baked_ground(world: vec3<f32>, n: vec3<f32>, dw: array<vec3<f32>, 2>) -> GroundSample {
    var out: GroundSample;
    out.color = vec4<f32>(0.0, 0.0, 0.0, -1.0);
    let at = (world.xz - lights.pages.xy) / lights.pages.z;
    let cell = vec2<i32>(floor(at));
    if any(cell < vec2<i32>(0)) || any(cell >= vec2<i32>(textureDimensions(t_pages))) { return out; }
    let entry = textureLoad(t_pages, cell, 0).r;
    if entry < 0x80000000u { return out; }
    let level = (entry >> 12u) & 15u;
    let layer = entry & 0xfffu;
    // The page's corner cell and its side in cells.
    let side = f32(1u << level);
    let f = at / side - floor(at / side);
    // Pages sit PAGE_SHEET_SIDE a side in sheets of a fixed size, however
    // many the pool holds, so a page's UVs never depend on its growth.
    let grid = PAGE_SHEET_SIDE;
    let scale = PAGE_TEXELS / (PAGE_TEXELS + 2.0 * PAGE_BORDER) / grid;
    let slot = layer % PAGE_SHEET;
    let sheet = layer / PAGE_SHEET;
    let page = vec2<f32>(f32(slot % u32(PAGE_SHEET_SIDE)), f32(slot / u32(PAGE_SHEET_SIDE)));
    let uv = (page + (PAGE_BORDER + f * PAGE_TEXELS) / (PAGE_TEXELS + 2.0 * PAGE_BORDER)) / grid;
    let k = scale / (side * lights.pages.z);
    let dx = dw[0].xz * k;
    let dy = dw[1].xz * k;
    let texel = textureSampleGrad(t_page_albedo, s_pages, uv, sheet, dx, dy);
    out.color = vec4<f32>(texel.rgb, 1.0);
    out.gloss = texel.a;
    out.normal = n;
    out.height = 0.0;
    out.glow = 0.0;
    if ground_glows() { out.glow = textureSampleGrad(t_page_glow, s_pages, uv, sheet, dx, dy).r; }
    // Live shading drops normal maps on far ground (FAR_GROUND), and so
    // skips the fetch here; relief keeps it, its slope being the point far
    // off too.
    if ground_page_normal(dw) {
        let t = textureSampleGrad(t_page_normal, s_pages, uv, sheet, dx, dy).xy * 2.0 - 1.0;
        let frame = page_frame(n);
        out.normal = normalize(frame[0] * t.x + frame[1] * t.y + n * sqrt(max(1.0 - dot(t, t), 0.0)));
    }
    return out;
}

// A ground fragment's screen footprint (`ground`'s dw), normal and cavity.
struct GroundFragment {
    dw: array<vec3<f32>, 2>,
    n: vec3<f32>,
    cavity: f32,
}

// A ground program draws terrain patches only, so its normal is always
// the heights'.
fn ground_fragment(in: GroundVertex) -> GroundFragment {
    let dw = array<vec3<f32>, 2>(dpdx(in.world), dpdy(in.world));
    let detail = terrain_detail(in.world.xz);
    return GroundFragment(dw, detail.xyz, mix(1.0, detail.w, CAVITY));
}

// The color map under a ground fragment and how far it covers the ground
// (its alpha, as vs_mesh hands them on), filtered between samples where
// vs_mesh takes each vertex's nearest and interpolates. `uv` runs over the
// samples as vs_mesh sets it for patches.
fn ground_colors(uv: vec2<f32>) -> vec4<f32> {
    let samples = vec2<f32>(textureDimensions(t_colors));
    let at = (uv * max(vec2<f32>(textureDimensions(t_heights)) - 1.0, vec2<f32>(1.0)) + 0.5) / samples;
    let texel = textureSampleLevel(t_colors, s_pages, at, 0.0);
    return vec4<f32>(texel.rgb * params.tone.x, 1.0 - texel.a);
}

// The page under a ground fragment where it is level enough for one, or
// alpha -1.
fn paged(in: GroundVertex, g: GroundFragment) -> GroundSample {
    if lights.pages.w > 0.0 && g.n.y >= lights.pages.w { return baked_ground(in.world, g.n, g.dw); }
    var none: GroundSample;
    none.color.a = -1.0;
    return none;
}

// Glow gives way the lighting to the ground's own light, as the glowing
// variants of `glowing` do: the albedo at the emissive strength, handed to
// `shade()` as emissive, so a game's `shade()` decides how it mixes (2070
// lerps its lighting toward a constant 8, deferredshading.fx:105-114).
fn lit_ground(in: GroundVertex, g: GroundFragment, sample: GroundSample) -> Opaque {
    let colors = ground_colors(in.uv);
    let albedo = covered(sample.color.rgb * g.cavity, colors.rgb, colors.a);
    let glow = clamp(sample.glow, 0.0, 1.0);
    var s = plain(albedo * (1.0 - glow), sample.normal);
    s.geometry = g.n;
    s.gloss = sample.gloss;
    s.specular = ground_specular();
    let light = albedo * glow * lights.glow.w;
    return shaded(shade(in.world, s, light, params.flags.x > 0.5), in.world, 0u);
}

@fragment fn fs_ground(in: GroundVertex) -> Opaque {
    let g = ground_fragment(in);
    var sample = paged(in, g);
    let baked = sample.color.a >= 0.0;
    if !baked {
        sample = ground(in.uv, in.world, g.n, g.dw);
    }
    // Relief lights the ground from its height alone, as Anno's baked
    // normals come from the baked height; a page holds that normal already.
    let bumped = relief_normal(in.world, g.n, sample.height);
    if relief() && !baked { sample.normal = bumped; }
    return lit_ground(in, g, sample);
}

// The ground with pages draws twice (terrain.rs TerrainPass): first only
// what its pages shade, then fs_ground over the rest. In one draw a warp
// holding both kinds ran the live ground with its paged lanes idle; the
// second draw's depth test drops every pixel the first shaded, so its warps
// hold live pixels alone. The paged pixels shade as fs_ground shades them.
@fragment fn fs_ground_paged(in: GroundVertex) -> Opaque {
    let g = ground_fragment(in);
    let sample = paged(in, g);
    if sample.color.a < 0.0 { discard; }
    return lit_ground(in, g, sample);
}

// --- Ground pages ---------------------------------------------------------
// Bakes `ground` into page layers (gpu/terrain.rs GroundPages): one quad
// per page mip in a staging atlas, which the frame then copies into the
// pages' layers. A job (b_instances) holds its staging rect (x, y, w, h in
// pixels), the world X and Z of its first texel's corner, the texel's
// side in metres and the share of it layers are sampled at (a mip sharp,
// terrain.rs PAGE_BAKE_GRADIENT), and the staging atlas's size.
struct PageBake {
    @builtin(position) clip: vec4<f32>,
    // The job's staging corner; its first texel's world corner, side and
    // gradient share.
    @location(0) @interpolate(flat) corner: vec2<f32>,
    @location(1) @interpolate(flat) texel: vec4<f32>,
}

@vertex fn vs_page_bake(@builtin(vertex_index) vertex: u32, @builtin(instance_index) job: u32) -> PageBake {
    let j = b_instances[job];
    let corner = array<vec2<f32>, 6>(
        vec2<f32>(0.0, 0.0), vec2<f32>(1.0, 0.0), vec2<f32>(0.0, 1.0),
        vec2<f32>(0.0, 1.0), vec2<f32>(1.0, 0.0), vec2<f32>(1.0, 1.0),
    )[vertex];
    let px = j[0].xy + corner * j[0].zw;
    let ndc = px / j[2].xy * vec2<f32>(2.0, -2.0) + vec2<f32>(-1.0, 1.0);
    return PageBake(vec4<f32>(ndc, 0.0, 1.0), j[0].xy, j[1]);
}

struct PageTexel {
    // Albedo (an sRGB target, so linear in) and gloss.
    @location(0) albedo: vec4<f32>,
    // The bent normal in the XZ projection's frame (page_frame), x and y
    // as 0..1.
    @location(1) normal: vec4<f32>,
}

// A glowing ground's pages bake a third target (terrain.rs GLOW_FORMAT).
struct GlowingPageTexel {
    @location(0) albedo: vec4<f32>,
    @location(1) normal: vec4<f32>,
    @location(2) glow: vec4<f32>,
}

@fragment fn fs_page_bake(in: PageBake) -> PageTexel {
    let t = page_texel(in);
    return PageTexel(t.albedo, t.normal);
}

@fragment fn fs_page_bake_glow(in: PageBake) -> GlowingPageTexel {
    return page_texel(in);
}

fn page_texel(in: PageBake) -> GlowingPageTexel {
    let t = in.texel.z;
    let xz = in.texel.xy + (in.clip.xy - in.corner) * t;
    // The ground as vs_mesh lays it: heights between samples, and the
    // normal from central differences a sample each way.
    let spacing = params.terrain.z;
    let p = (xz - params.terrain.xy) / spacing;
    let c = height_cross(p);
    let dx = (c.x1 - c.x0) / (2.0 * spacing);
    let dz = (c.z1 - c.z0) / (2.0 * spacing);
    let n = normalize(vec3<f32>(-dx, 1.0, -dz));
    let world = vec3<f32>(xz.x, c.h, xz.y);
    let uv = p / max(vec2<f32>(textureDimensions(t_heights)) - 1.0, vec2<f32>(1.0));
    let g = t * in.texel.w;
    let dw = array<vec3<f32>, 2>(vec3<f32>(g, dx * g, 0.0), vec3<f32>(0.0, dz * g, g));
    var sample = ground(uv, world, n, dw);
    // One fragment per page texel: the relief's slope over a texel, as the
    // game bakes its page normals.
    let bumped = relief_normal(world, n, sample.height);
    if relief() { sample.normal = bumped; }
    let frame = page_frame(n);
    let m = vec2<f32>(dot(sample.normal, frame[0]), dot(sample.normal, frame[1]));
    return GlowingPageTexel(vec4<f32>(sample.color.rgb, sample.gloss), vec4<f32>(m * 0.5 + 0.5, 0.0, 1.0), vec4<f32>(sample.glow, 0.0, 0.0, 1.0));
}

// World distance from this fragment to the opaque surface behind it, along the view.
fn depth_gap(scene: f32, own: f32) -> f32 {
    if globals.eye.w > 0.5 {
        return globals.anchor.w / max(scene, 1e-30) - globals.anchor.w / own;
    }
    return (own - scene) * globals.anchor.w;
}

@fragment fn fs_blend(in: Output) -> @location(0) vec4<f32> {
    let derived = cross(dpdx(in.world), dpdy(in.world));
    let texel = albedo(in.uv);
    let n = surface_normal(in.world, derived, in.normal, params.flags.x > 0.5);
    let base = decode(texel.rgb) * in.color * decode(params.surface.rgb);
    var alpha = texel.a * params.surface.a * (1.0 - in.cover);
    // Additive (blend.w, the add pipeline): unlit light added to what is
    // behind, premultiplied, with a coverage by brightness so it shows
    // over a target nothing else drew.
    if params.blend.w > 0.5 && params.blend.w < 1.5 {
        let glow = base * alpha;
        return vec4<f32>(glow, clamp(max(glow.r, max(glow.g, glow.b)), 0.0, 1.0));
    }
    var color = base;
    // blend.w 2: unlit, the colour as it is.
    if params.blend.w < 1.5 {
        wrap_local = true;
        color = shade(in.world, plain(base, n), vec3<f32>(0.0), params.flags.x > 0.5);
    }
    // Schlick: grazing angles turn opaque and mirror the sky.
    let facing = clamp(dot(n, toward_viewer(in.world)), 0.0, 1.0);
    let fresnel = params.blend.y * pow(1.0 - facing, 5.0);
    color = mix(color, sky_light(reflect(-toward_viewer(in.world), n)), fresnel);
    alpha = mix(alpha, 1.0, fresnel);
    if params.blend.x > 0.0 || params.blend.z > 0.0 {
        let gap = depth_gap(textureLoad(t_depth, vec2<i32>(in.clip.xy), 0), in.clip.z);
        if params.blend.z > 0.0 {
            alpha = mix(alpha, 1.0, smoothstep(params.blend.x, params.blend.z, gap));
        }
        if params.blend.x > 0.0 { alpha *= clamp(gap / params.blend.x, 0.0, 1.0); }
    }
    return vec4<f32>(fog(color, in.world), alpha);
}

@vertex fn vs_marker(@builtin(vertex_index) vertex: u32, @builtin(instance_index) instance: u32) -> Output {
    let corners = array<vec2<f32>, 6>(vec2<f32>(-1.0, -1.0), vec2<f32>(1.0, -1.0), vec2<f32>(-1.0, 1.0),
        vec2<f32>(-1.0, 1.0), vec2<f32>(1.0, -1.0), vec2<f32>(1.0, 1.0));
    let marker = b_markers[instance];
    var out: Output;
    let center = globals.clip * vec4<f32>(marker.position.xyz, 1.0);
    let radius = marker.position.w * globals.viewport.z;
    let offset = corners[vertex] * vec2<f32>(2.0 / globals.viewport.x, 2.0 / globals.viewport.y) * radius * center.w;
    // Markers draw on top of everything; depth is irrelevant.
    out.clip = vec4<f32>(center.xy + offset, 0.0, center.w);
    out.uv = corners[vertex];
    out.color = marker.color.rgb;
    out.shape = marker.color.w;
    out.normal = vec3<f32>(0.0);
    out.pick = 0u;
    return out;
}

@fragment fn fs_marker(in: Output) -> @location(0) vec4<f32> {
    // 0 circle, 1 diamond, 2 square, 3 ring.
    var distance = length(in.uv);
    if in.shape > 0.5 && in.shape < 1.5 { distance = abs(in.uv.x) + abs(in.uv.y); }
    if in.shape > 1.5 && in.shape < 2.5 { distance = max(abs(in.uv.x), abs(in.uv.y)); }
    let edge = max(fwidth(distance), 0.01);
    var alpha = 1.0 - smoothstep(1.0 - edge, 1.0, distance);
    if in.shape > 2.5 { alpha *= smoothstep(0.65, 0.65 + edge, distance); }
    if alpha <= 0.0 { discard; }
    return vec4<f32>(in.color, alpha);
}

// A segment as a screen-space quad of its width, ends squared off half a
// width past each point so polylines join without gaps.
@vertex fn vs_line(@builtin(vertex_index) vertex: u32, @builtin(instance_index) instance: u32) -> Output {
    let corners = array<vec2<f32>, 6>(vec2<f32>(0.0, -1.0), vec2<f32>(1.0, -1.0), vec2<f32>(1.0, 1.0),
        vec2<f32>(0.0, -1.0), vec2<f32>(1.0, 1.0), vec2<f32>(0.0, 1.0));
    let seg = b_lines[instance];
    let a = globals.clip * vec4<f32>(seg.a.xyz, 1.0);
    let b = globals.clip * vec4<f32>(seg.b.xyz, 1.0);
    var out: Output;
    out.uv = vec2<f32>(0.0);
    out.normal = vec3<f32>(0.0);
    out.shape = 0.0;
    out.pick = 0u;
    let bits = bitcast<u32>(seg.b.w);
    out.color = vec3<f32>(f32(bits >> 24u), f32((bits >> 16u) & 255u), f32((bits >> 8u) & 255u)) / 255.0;
    out.world = vec3<f32>(f32(bits & 255u) / 255.0, 0.0, 0.0);
    // Behind the camera: nothing to draw.
    if a.w <= 1e-4 || b.w <= 1e-4 {
        out.clip = vec4<f32>(0.0, 0.0, 0.0, 0.0);
        return out;
    }
    let half_px = globals.viewport.xy * 0.5;
    let sa = a.xy / a.w * half_px;
    let sb = b.xy / b.w * half_px;
    let along = sb - sa;
    let dir = select(vec2<f32>(1.0, 0.0), normalize(along), dot(along, along) > 1e-8);
    let across = vec2<f32>(-dir.y, dir.x);
    let corner = corners[vertex];
    let half = seg.a.w * globals.viewport.z * 0.5;
    let at = mix(sa, sb, corner.x) + (across * corner.y + dir * (corner.x * 2.0 - 1.0)) * half;
    out.clip = vec4<f32>(at / half_px, 0.0, 1.0);
    return out;
}

@fragment fn fs_line(in: Output) -> @location(0) vec4<f32> {
    return vec4<f32>(in.color, in.world.x);
}

// The sky, drawn as one triangle over the view at the far plane where
// nothing opaque drew. Each pixel's view ray comes from unprojecting it.
struct SkyOutput {
    @builtin(position) clip: vec4<f32>,
    @location(0) ndc: vec2<f32>,
}

@vertex fn vs_sky(@builtin(vertex_index) vertex: u32) -> SkyOutput {
    let p = vec2<f32>(f32((vertex << 1u) & 2u), f32(vertex & 2u)) * 2.0 - 1.0;
    var out: SkyOutput;
    out.clip = vec4<f32>(p, 0.0, 1.0);
    out.ndc = p;
    return out;
}

// Where the far sea plane meets the view ray through `ndc`, when the
// scene has one (gpu::Sea, lights.sea.w) and the ray comes down onto it
// from above: what the kit's sea draws (fs_sea) and its sky leaves to it.
struct SeaRay { origin: vec3<f32>, dir: vec3<f32>, hit: bool }

fn sea_ray(ndc: vec2<f32>) -> SeaRay {
    // Reversed depth: z = 1 is the near plane. The near plane sits almost
    // on the eye, so the ray runs from it to a point far out instead.
    let near = unproject(ndc, 1.0);
    let dir = normalize(unproject(ndc, 1e-3) - near);
    var origin = near;
    if globals.eye.w > 0.5 { origin = globals.eye.xyz; }
    let hit = lights.sea.w > 0.5 && dir.y < 0.0 && origin.y > lights.horizon.w;
    return SeaRay(origin, dir, hit);
}

// The kit's backdrop, at the Sky stage where nothing drew: the sky, and
// under the horizon its colour, but where the kit's sea lies (fs_sea).
@fragment fn fs_sky(in: SkyOutput) -> @location(0) vec4<f32> {
    let ray = sea_ray(in.ndc);
    if ray.hit { discard; }
    var color = sky_radiance(ray.dir);
    if ray.dir.y < 0.0 && lights.disc.w < 0.5 {
        color = lights.horizon.rgb;
    }
    return vec4<f32>(color, 1.0);
}

// The kit's far sea, at the Water stage where nothing opaque drew: a plane
// at sea level, its body lit by the sky and the sun, reflecting the sky.
// A game's Water pass that replaces it draws its own sea instead
// (Pass::replaces), and the backdrop then fills under the horizon.
@fragment fn fs_sea(in: SkyOutput) -> @location(0) vec4<f32> {
    let ray = sea_ray(in.ndc);
    if !ray.hit { discard; }
    let level = lights.horizon.w;
    let at = ray.origin + ray.dir * ((level - ray.origin.y) / ray.dir.y);
    let n = vec3<f32>(0.0, 1.0, 0.0);
    // Schlick from water's 2% at normal incidence.
    let fresnel = 0.02 + 0.98 * pow(1.0 - clamp(-ray.dir.y, 0.0, 1.0), 5.0);
    let sun = lights.sun_color.rgb * clamp(normalize(lights.sun.xyz).y, 0.0, 1.0) * shadow(at, n);
    let body = lights.sea.rgb * (sky_light(n) + sun);
    return vec4<f32>(fog(mix(body, sky_radiance(reflect(ray.dir, n)), fresnel), at), 1.0);
}

// Ambient occlusion (gpu::Occlusion), from the opaque depth's copy in
// t_depth: an estimate per texel at half size, a depth-aware blur there
// into the factor the color takes, and that factor brought up to full size
// along depth edges and multiplied over the color.
// The occlusion passes' inputs (half-size depth, estimate, factor); the
// HDR target for the post pass. Read by load, so any float format binds.
@group(1) @binding(0) var t_screen: texture_2d<f32>;

// The world point under a pixel of the depth copy; w is its depth, 0
// where nothing opaque drew.
fn depth_point(px: vec2<i32>) -> vec4<f32> {
    let size = vec2<i32>(textureDimensions(t_depth));
    let p = clamp(px, vec2<i32>(0), size - 1);
    let z = textureLoad(t_depth, p, 0);
    let uv = (vec2<f32>(p) + 0.5) / vec2<f32>(size);
    return vec4<f32>(unproject(vec2<f32>(uv.x * 2.0 - 1.0, 1.0 - uv.y * 2.0), z), z);
}

// The surface normal at pixel `px` (whose point is `c`) from the depth
// copy, facing the viewer; zero where the neighbours give none. The
// nearer neighbour on each axis keeps edges sharp.
fn depth_normal(px: vec2<i32>, c: vec4<f32>) -> vec3<f32> {
    let l = depth_point(px - vec2<i32>(1, 0));
    let r = depth_point(px + vec2<i32>(1, 0));
    let u = depth_point(px - vec2<i32>(0, 1));
    let d = depth_point(px + vec2<i32>(0, 1));
    var dx = r.xyz - c.xyz;
    if abs(l.w - c.w) < abs(r.w - c.w) { dx = c.xyz - l.xyz; }
    var dy = d.xyz - c.xyz;
    if abs(u.w - c.w) < abs(d.w - c.w) { dy = c.xyz - u.xyz; }
    let n = cross(dx, dy);
    if dot(n, n) < 1e-20 { return vec3<f32>(0.0); }
    return normalize(n) * select(1.0, -1.0, dot(n, toward_viewer(c.xyz)) < 0.0);
}

// HBAO+ as ambient_ssao.fx runs it (csAOCompute), out to the radius
// projected on screen, four steps per direction. A sample counts by how far
// it rises over the tangent plane past the bias, fading out toward the
// radius; the first step is the small (contact) scale, the rest the large.
// Four directions where the game takes eight: each pixel turns them by its
// own noise and the apply pass blurs 16 texels, so the 64 directions a
// pixel ends up with match 128 within run-to-run noise (gpu_bench), for
// half the estimate's 0.77 ms at 5K.
const OCCLUSION_DIRECTIONS: u32 = 4u;
const OCCLUSION_STEPS: u32 = 4u;

// The depth of the full-size pixel at each half-size texel's top left,
// which fs_occlusion steps through (in t_screen).
@fragment fn fs_occlusion_depth(in: SkyOutput) -> @location(0) vec4<f32> {
    let size = vec2<i32>(textureDimensions(t_depth));
    return vec4<f32>(textureLoad(t_depth, min(vec2<i32>(in.clip.xy) * 2, size - 1), 0));
}

// The world point under half-size texel `q` of fs_occlusion_depth's depth,
// at its full-size pixel; w is its depth, 0 where nothing drew.
fn half_depth_point(q: vec2<i32>) -> vec4<f32> {
    let p = clamp(q, vec2<i32>(0), vec2<i32>(textureDimensions(t_screen)) - 1);
    let z = textureLoad(t_screen, p, 0).r;
    let uv = (vec2<f32>(p * 2) + 0.5) / vec2<f32>(textureDimensions(t_depth));
    return vec4<f32>(unproject(vec2<f32>(uv.x * 2.0 - 1.0, 1.0 - uv.y * 2.0), z), z);
}

// Runs at half resolution: each texel takes the depth of the full-size
// pixel at its top left, and its normal from the full-size pixels around
// it, but steps through the half-size depth, two pixels to a texel. The game
// jitters each of its 4x4 interleaved slices; interleaved gradient noise
// turns the directions and starts per pixel here, and the blur averages
// the pattern out.
@fragment fn fs_occlusion(in: SkyOutput) -> @location(0) vec4<f32> {
    let half = vec2<i32>(in.clip.xy);
    let px = half * 2;
    let c = depth_point(px);
    // r: the estimate, or -1 where nothing was drawn; g: the pixel's
    // depth, from which the blur takes its point again.
    if c.w <= 0.0 { return vec4<f32>(-1.0, 0.0, 0.0, 0.0); }
    let n = depth_normal(px, c);
    if dot(n, n) == 0.0 { return vec4<f32>(1.0, c.w, 0.0, 0.0); }
    // The radius in pixels at this depth: clip y per world unit over w.
    // Like the game's GetAORadiusDesc, it grows to a least size on screen,
    // so distant ground keeps its occlusion, and shrinks to a cap near the
    // camera; the world radius follows.
    let clip = globals.clip * vec4<f32>(c.xyz, 1.0);
    let row = vec3<f32>(globals.clip[0].y, globals.clip[1].y, globals.clip[2].y);
    let size = vec2<f32>(textureDimensions(t_depth));
    let natural = lights.occlusion.x * length(row) / max(clip.w, 1e-6) * size.y * 0.5;
    let pixels = clamp(natural, lights.occlusion_scale.w, lights.occlusion_scale.z);
    let radius = lights.occlusion.x * pixels / max(natural, 1e-6);
    if pixels < 1.0 { return vec4<f32>(1.0, c.w, 0.0, 0.0); }
    let step = pixels / f32(OCCLUSION_STEPS + 1u);
    let noise = fract(52.9829189 * fract(dot(in.clip.xy, vec2<f32>(0.06711056, 0.00583715))));
    let turn = 6.2831853 * noise;
    let falloff = -1.0 / (radius * radius);
    var small = 0.0;
    var large = 0.0;
    for (var d = 0u; d < OCCLUSION_DIRECTIONS; d++) {
        let angle = turn + 6.2831853 * f32(d) / f32(OCCLUSION_DIRECTIONS);
        let dir = vec2<f32>(cos(angle), sin(angle));
        // From two pixels out: the first step lands off the texel itself.
        var ray = fract(noise * 7.0) * step + 2.0;
        for (var k = 0u; k < OCCLUSION_STEPS; k++) {
            let o = half_depth_point(half + vec2<i32>(round(dir * ray * 0.5)));
            ray += step;
            if o.w <= 0.0 { continue; }
            let v = o.xyz - c.xyz;
            let vv = dot(v, v);
            let term = clamp(dot(n, v) * inverseSqrt(max(vv, 1e-12)) - lights.occlusion.z, 0.0, 1.0)
                * clamp(vv * falloff + 1.0, 0.0, 1.0);
            if k == 0u { small += term; } else { large += term; }
        }
    }
    let ao = (small * lights.occlusion_scale.x + large * lights.occlusion_scale.y)
        / f32(OCCLUSION_DIRECTIONS * OCCLUSION_STEPS);
    return vec4<f32>(clamp(1.0 - ao * 2.0, 0.0, 1.0), c.w, 0.0, 0.0);
}

// The share of a surface's light, per channel, that comes from the sky
// rather than the sun, as `shade` lights it. Albedo cancels; specular and
// maps are left out, so this is the matte share.
fn ambient_share(world: vec3<f32>, n: vec3<f32>) -> vec3<f32> {
    var sky = sky_light(n);
    if toned() { sky *= lights.tone.z; }
    let ndl = clamp(dot(n, lights.sun.xyz), 0.0, 1.0);
    var sun = vec3<f32>(0.0);
    if ndl > 0.0 { sun = lights.sun_color.rgb * ndl * shadow(world, n); }
    return sky / max(sky + sun, vec3<f32>(1e-6));
}

// 4x4 blur over the half-size estimate's texels whose points lie on the
// same surface as this texel's (within a fifth of the radius), then the
// factor the color is multiplied by. Anno's SSAO darkens ambient light,
// and the sun only by its SunAO (functions_lighting_base.fxh, PbrLighting),
// so the darkening is weighted by the ambient share: sunlit ground at a
// wall's foot keeps most of its light, shaded ground loses it. Runs at
// half size as the estimate does, for the point the estimate took (the
// full-size pixel at the texel's top left); rgb: the factor, a: that
// pixel's depth, 0 where nothing drew, for fs_occlusion_apply.
@fragment fn fs_occlusion_blur(in: SkyOutput) -> @location(0) vec4<f32> {
    let half = vec2<i32>(in.clip.xy);
    if textureLoad(t_screen, half, 0).r < 0.0 { return vec4<f32>(1.0, 1.0, 1.0, 0.0); }
    // Each texel's point as fs_occlusion took it.
    let px = min(half * 2, vec2<i32>(textureDimensions(t_depth)) - 1);
    let c = depth_point(px);
    let z = c.w;
    let last = vec2<i32>(textureDimensions(t_screen)) - 1;
    let n = depth_normal(px, c);
    var sum = 0.0;
    var weight = 0.0;
    // Same surface: near this texel's tangent plane (or point, without a
    // normal) for how far the other lies along it, which grows with
    // distance from the camera: off <= 0.2 max(radius, |v|), squared.
    let tilted = dot(n, n) > 0.0;
    let near = 0.04 * lights.occlusion.x * lights.occlusion.x;
    // The taps' points, as half_depth_point gives them: clip to world is
    // linear in the texel, so each takes three multiply-adds and a divide
    // where unproject takes a matrix product (0.04 ms of the blur at 5K).
    let size = vec2<f32>(textureDimensions(t_depth));
    let u = globals.unclip;
    let corner = u * vec4<f32>(1.0 / size.x - 1.0, 1.0 - 1.0 / size.y, 0.0, 1.0);
    let across = u[0] * (4.0 / size.x);
    let down = u[1] * (-4.0 / size.y);
    // The window's centre lies a quarter texel before the point's.
    let start = half - 2;
    for (var y = 0; y < 4; y++) {
        let qy = clamp(start.y + y, 0, last.y);
        let row = corner + down * f32(qy);
        for (var x = 0; x < 4; x++) {
            let q = vec2<i32>(clamp(start.x + x, 0, last.x), qy);
            let o = textureLoad(t_screen, q, 0).rg;
            let estimate = o.x;
            if estimate < 0.0 { continue; }
            let h = row + across * f32(q.x) + u[2] * o.y;
            let v = h.xyz / h.w - c.xyz;
            let vv = dot(v, v);
            let along = dot(v, n);
            let off = select(vv, along * along, tilted);
            if off > max(near, 0.04 * vv) { continue; }
            sum += estimate;
            weight += 1.0;
        }
    }
    // HBAO+ raises the blurred estimate to its power exponent.
    let ao = pow(select(1.0, sum / weight, weight > 0.0), lights.occlusion.w);
    let k = lights.occlusion.y * (1.0 - ao);
    // Open ground, most of the view: no shadow lookups.
    if k < 1.0 / 512.0 { return vec4<f32>(1.0, 1.0, 1.0, z); }
    var share = vec3<f32>(1.0);
    if tilted { share = ambient_share(c.xyz, n); }
    return vec4<f32>(1.0 - k * (share + (1.0 - share) * lights.tone.y), z);
}

// The blurred factor brought up to full size (frame.wgsl
// occlusion_upsample) and multiplied over the color, where no Blend-stage
// pass applies it itself.
@fragment fn fs_occlusion_apply(in: SkyOutput) -> @location(0) vec4<f32> {
    return occlusion_applied(vec2<i32>(in.clip.xy));
}

fn occlusion_applied(px: vec2<i32>) -> vec4<f32> {
    let z = textureLoad(t_depth, px, 0);
    // Multiplying by one would leave the target as it is: skip the write.
    if z <= 0.0 { discard; }
    let f = occlusion_upsample(t_screen, px, z);
    if all(f > vec3<f32>(1.0 - 1.0 / 512.0)) { discard; }
    // The HDR target holds linear radiance, which the factor scales as the
    // game's lighting does, before tone mapping.
    return vec4<f32>(f, 1.0);
}

// The posed placements' depth as their pass drew it (visbuf.rs draw_posed),
// 0 where none drew; a 1x1 stand-in without them.
@group(1) @binding(1) var t_posed: texture_2d<f32>;

// fs_occlusion_apply as the water and blend pass's first draw, after the
// posed placements, which it leaves as they are: applied in a pass of its
// own before them, as frames that keep their layers still do, it never
// reached them. Where they drew, their resolve shaded the pixel (the depth
// it tests is theirs), so its depth there is not 0.
@fragment fn fs_occlusion_apply_late(in: SkyOutput) -> @location(0) vec4<f32> {
    let px = vec2<i32>(in.clip.xy);
    let posed = min(px, vec2<i32>(textureDimensions(t_posed)) - 1);
    if textureLoad(t_posed, posed, 0).r > 0.0 { discard; }
    return occlusion_applied(px);
}

// --- Post ---------------------------------------------------------------------
// The scene under window pixel `p` when drawn at a lower render scale (the
// scene's targets start at the pane's corner, display.xy):
// Catmull-Rom over its 4x4 texels, so its edges stay crisp stretched, with
// colors weighed by coverage (alpha) so empty texels do not darken edges.
fn scene_at(p: vec2<f32>) -> vec4<f32> {
    let at = (p - globals.display.xy) * globals.display.z - 0.5;
    let base = floor(at);
    let f = at - base;
    let last = vec2<i32>(textureDimensions(t_screen)) - 1;
    var w: array<vec2<f32>, 4>;
    for (var i = 0; i < 4; i++) {
        let x = abs(f32(i - 1) - f);
        w[i] = select(
            select(vec2<f32>(0.0), ((-0.5 * x + 2.5) * x - 4.0) * x + 2.0, x < vec2<f32>(2.0)),
            (1.5 * x - 2.5) * x * x + 1.0,
            x <= vec2<f32>(1.0),
        );
    }
    var sum = vec4<f32>(0.0);
    for (var y = 0; y < 4; y++) {
        for (var x = 0; x < 4; x++) {
            let q = clamp(vec2<i32>(base) + vec2<i32>(x - 1, y - 1), vec2<i32>(0), last);
            let t = textureLoad(t_screen, q, 0);
            sum += vec4<f32>(t.rgb * t.a, t.a) * (w[x].x * w[y].y);
        }
    }
    let a = clamp(sum.a, 0.0, 1.0);
    if a <= 0.0 { return vec4<f32>(0.0); }
    // Catmull-Rom overshoots: no negative radiance.
    return vec4<f32>(max(sum.rgb / sum.a, vec3<f32>(0.0)) * a, a);
}

// The scene's HDR target to display values, once per pixel: the tone curve
// (tone_map) over what the scene passes left. Alpha is coverage: where
// nothing drew the target keeps what was there, and blended surfaces over
// nothing tone map their unpremultiplied color and blend premultiplied
// (One, OneMinusSrcAlpha), which an untoned scene leaves exactly as the
// old in-place blending did.
@fragment fn fs_post(in: SkyOutput) -> @location(0) vec4<f32> {
    var c = textureLoad(t_screen, vec2<i32>(in.clip.xy - globals.display.xy), 0);
    if globals.display.z < 1.0 { c = scene_at(in.clip.xy); }
    if c.a <= 0.0 { discard; }
    let a = min(c.a, 1.0);
    return vec4<f32>(tone_map(c.rgb / a * auto_exposure[0]) * a, a);
}

// gpu::AutoExposure's measured exposure (exposure.wgsl), 1 without it.
@group(1) @binding(1) var<storage, read> auto_exposure: array<f32, 4>;
