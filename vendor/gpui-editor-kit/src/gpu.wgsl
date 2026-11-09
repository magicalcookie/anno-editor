// Instanced meshes, their materials, sun shadows and screen-space markers for
// gpu.rs. Vertices are pulled from storage buffers by index, so every mesh
// shares one pipeline layout. Main-pass depth is reversed: 1 near, 0 far.
// Bind groups: 0 the pass (camera, lights, shadow and depth maps, markers),
// 1 the mesh (material, geometry, textures), 2 the draw (instances, visible
// rows, pick handles).
struct Globals {
    clip: mat4x4<f32>,
    // Inverse of clip, for the sky's view rays.
    unclip: mat4x4<f32>,
    // Target width and height in device pixels, device pixels per logical
    // pixel, and the row bits of packed visible entries (placed.wgsl).
    viewport: vec4<f32>,
    // Perspective: eye position, w = 1. Orthographic: unit vector toward the viewer, w = 0.
    eye: vec4<f32>,
    // Camera anchor; w is the near distance in perspective, the depth span in orthographic.
    anchor: vec4<f32>,
}
struct Lights {
    // World to shadow clip: xy -1..1 across the map, z 0 nearest the sun.
    shadow: mat4x4<f32>,
    // Unit vector toward the sun; w = 1 when the shadow map is live.
    sun: vec4<f32>,
    // w: shadow filter radius in texels.
    sun_color: vec4<f32>,
    // w: shadow map side in texels.
    sky: vec4<f32>,
    ground: vec4<f32>,
    // w = 1 when fog is on.
    fog: vec4<f32>,
    // Fog start and end, shadow texel size in world units, depth bias in shadow depth units.
    fog_range: vec4<f32>,
    // x: exposure, 0 for display-space shading (gpu::Tone); y: sun occlusion.
    tone: vec4<f32>,
    // gpu::Sky: zenith (w = 1 on), horizon (w: sea level), sea color (w = 1 with a sea).
    zenith: vec4<f32>,
    horizon: vec4<f32>,
    sea: vec4<f32>,
    // Ground tint: terrain grid origin x, z, spacing, color gain (0: none).
    tint: vec4<f32>,
    // gpu::Occlusion radius and strength.
    occlusion: vec4<f32>,
}
struct Params {
    // Material color and opacity.
    surface: vec4<f32>,
    // Has normals, control texture pairs, layer count, ignore texture alpha.
    flags: vec4<f32>,
    // Blend: depth fade distance, fresnel strength, depth where it turns opaque.
    blend: vec4<f32>,
    // x: vertex color gain, so colors can brighten as well as darken.
    // y: 1 when a splat has normal layers (t_layer_normals). z, w: 1 with a
    // normal map (t_normal_map), a surface map (t_surface_map).
    tone: vec4<f32>,
    // Displacement: grid origin x and z, sample spacing; w is 0 off, 1 on,
    // 2 on with UVs from world X/Z.
    terrain: vec4<f32>,
    // x: terrain tint amount (MeshData::terrain_tint).
    extra: vec4<f32>,
}
// x: 1 when instances write pick handles. y: 1 for placed draws, whose rows
// come from b_visible starting at z, masked by w (placed.rs packs the draw
// index above the row).
struct DrawIds { ids: vec4<u32> }
// `normal` is octahedral, two snorm16 halves.
struct GpuVertex { position: vec4<f32>, uv: vec2<f32>, color: u32, normal: u32 }
// position.w is the radius in logical pixels; color.w selects the shape.
struct GpuMarker { position: vec4<f32>, color: vec4<f32> }
// Splat layer: reciprocal tiling in world units, blend sharpness (0: none).
struct GpuLayer { scale: f32, sharpness: f32 }

@group(0) @binding(0) var<uniform> globals: Globals;
@group(0) @binding(1) var<uniform> lights: Lights;
@group(0) @binding(2) var s_albedo: sampler;
@group(0) @binding(3) var t_shadow: texture_depth_2d;
@group(0) @binding(4) var s_shadow: sampler_comparison;
// Opaque depth, copied before the blend pass.
@group(0) @binding(5) var t_depth: texture_depth_2d;
@group(0) @binding(6) var<storage, read> b_markers: array<GpuMarker>;
// Line segments: a.xyz, width in logical pixels; b.xyz, RGBA bits.
struct GpuSegment { a: vec4<f32>, b: vec4<f32> }
@group(0) @binding(7) var<storage, read> b_lines: array<GpuSegment>;
// The terrain's color map, one RGBA8 texel per height sample, loaded.
@group(0) @binding(8) var t_ground_tint: texture_2d<f32>;
@group(1) @binding(0) var<uniform> params: Params;
@group(1) @binding(1) var<storage, read> b_vertices: array<GpuVertex>;
@group(1) @binding(2) var<storage, read> b_indices: array<u32>;
@group(1) @binding(3) var t_albedo: texture_2d<f32>;
@group(1) @binding(4) var<storage, read> b_layers: array<GpuLayer>;
@group(1) @binding(5) var t_layers: texture_2d_array<f32>;
// Splat palettes (palette.wgsl): eight layer weights per control texel in
// per-tile regions of two atlases, and each tile's eight layer ids.
@group(1) @binding(6) var t_palette_weights0: texture_2d<f32>;
@group(1) @binding(7) var t_palette_weights1: texture_2d<f32>;
@group(1) @binding(8) var t_palette_ids0: texture_2d<f32>;
@group(1) @binding(9) var t_palette_ids1: texture_2d<f32>;
// One R32Float texel per heightfield sample. Loaded, never filtered: float
// filtering is optional.
@group(1) @binding(10) var t_heights: texture_2d<f32>;
// Displaced meshes with a color map (params.blend.w): RGBA8 per height
// sample, read where vertices land on samples. Terrain patches (gpu.rs).
@group(1) @binding(11) var t_colors: texture_2d<f32>;
// Splat normal maps, one per layer, when params.tone.y is 1.
@group(1) @binding(12) var t_layer_normals: texture_2d_array<f32>;
// Lit meshes: tangent-space normals with gloss in alpha; metalness in red
// and occlusion in alpha (MeshData::normal_map, surface_map).
@group(1) @binding(13) var t_normal_map: texture_2d<f32>;
@group(1) @binding(14) var t_surface_map: texture_2d<f32>;
@group(2) @binding(0) var<uniform> draw: DrawIds;
@group(2) @binding(1) var<storage, read> b_instances: array<mat4x4<f32>>;
@group(2) @binding(2) var<storage, read> b_visible: array<u32>;

struct Output {
    @builtin(position) clip: vec4<f32>,
    @location(0) world: vec3<f32>,
    @location(1) uv: vec2<f32>,
    @location(2) color: vec3<f32>,
    @location(3) @interpolate(flat) shape: f32,
    @location(4) normal: vec3<f32>,
    // 0 for nothing pickable; placements set the top bit over their row.
    @location(5) @interpolate(flat) pick: u32,
}

// The opaque pass writes color, and the pick handle and depth under each
// pixel: depth goes in the pick target because depth textures cannot be
// copied a texel at a time.
struct Opaque {
    @location(0) color: vec4<f32>,
    @location(1) pick: vec2<u32>,
}

fn rgb(v: u32) -> vec3<f32> {
    return vec3<f32>(f32((v >> 16u) & 255u), f32((v >> 8u) & 255u), f32(v & 255u)) / 255.0;
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

// Drawn indexed, so `index` is the vertex itself.
@vertex fn vs_mesh(@builtin(vertex_index) index: u32, @builtin(instance_index) instance: u32) -> Output {
    let vertex = b_vertices[index];
    var row = instance;
    var pick = 0u;
    if draw.ids.y == 1u {
        row = b_visible[draw.ids.z + instance] & draw.ids.w;
        pick = row | 0x80000000u;
    } else if draw.ids.x == 1u {
        // Batch instances index the scene's one instance buffer; 0 is none.
        pick = instance + 1u;
    }
    let m = b_instances[row];
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
        world.y = height(p) + drop;
        // Central differences one sample each way; at the edge the clamp
        // turns them one-sided, as the CPU tile normals did.
        let dx = (height(p + vec2<f32>(1.0, 0.0)) - height(p - vec2<f32>(1.0, 0.0))) / (2.0 * spacing);
        let dz = (height(p + vec2<f32>(0.0, 1.0)) - height(p - vec2<f32>(0.0, 1.0))) / (2.0 * spacing);
        out.normal = normalize(vec3<f32>(-dx, 1.0, -dz));
        if params.terrain.w > 1.5 {
            out.uv = p / max(vec2<f32>(textureDimensions(t_heights)) - 1.0, vec2<f32>(1.0));
        }
    }
    out.clip = globals.clip * vec4<f32>(world, 1.0);
    out.world = world;
    out.color = rgb(vertex.color) * params.tone.x;
    if displaced && params.blend.w > 0.5 {
        let spacing = params.terrain.z;
        let last = vec2<i32>(textureDimensions(t_colors)) - 1;
        let sample = clamp(vec2<i32>(round((world.xz - params.terrain.xy) / spacing)), vec2<i32>(0), last);
        out.color = textureLoad(t_colors, sample, 0).rgb * params.tone.x;
    }
    out.shape = 0.0;
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

fn shadow(world: vec3<f32>, n: vec3<f32>) -> f32 {
    if lights.sun.w < 0.5 { return 1.0; }
    // Normal offset of about a texel, toward the sun, stops self-shadow acne
    // on slopes without the light leaks a large depth bias causes.
    let toward_sun = n * select(-1.0, 1.0, dot(n, lights.sun.xyz) >= 0.0);
    let p = lights.shadow * vec4<f32>(world + toward_sun * lights.fog_range.z * 1.5, 1.0);
    let uv = vec2<f32>(p.x * 0.5 + 0.5, 0.5 - p.y * 0.5);
    if any(uv < vec2<f32>(0.0)) || any(uv > vec2<f32>(1.0)) || p.z > 1.0 { return 1.0; }
    let reference = p.z - lights.fog_range.w;
    let step = lights.sun_color.w / lights.sky.w;
    // 3x3 taps, each a hardware 2x2 compare: 36 texels of PCF. The four
    // corner taps read every texel the others do, so when they agree the
    // pixel is wholly lit or shadowed and the rest would agree too; only
    // shadow edges take all nine. Most pixels take four, which saved 0.2 ms
    // of a low 1080p view whose foliage shades several layers deep.
    let corners = shadow_tap(uv, vec2<f32>(-step, -step), reference)
        + shadow_tap(uv, vec2<f32>(step, -step), reference)
        + shadow_tap(uv, vec2<f32>(-step, step), reference)
        + shadow_tap(uv, vec2<f32>(step, step), reference);
    if corners == 0.0 || corners == 4.0 { return corners * 0.25; }
    let edges = shadow_tap(uv, vec2<f32>(0.0, -step), reference)
        + shadow_tap(uv, vec2<f32>(-step, 0.0), reference)
        + shadow_tap(uv, vec2<f32>(step, 0.0), reference)
        + shadow_tap(uv, vec2<f32>(0.0, step), reference);
    return (corners + edges + shadow_tap(uv, vec2<f32>(0.0), reference)) / 9.0;
}

fn shadow_tap(uv: vec2<f32>, offset: vec2<f32>, reference: f32) -> f32 {
    return textureSampleCompareLevel(t_shadow, s_shadow, uv + offset, reference);
}

// What a fragment hands to `shade`. `geometry` is the unbent normal, for
// the shadow lookup's offset.
struct Surface {
    albedo: vec3<f32>,
    normal: vec3<f32>,
    geometry: vec3<f32>,
    gloss: f32,
    metal: f32,
    occlusion: f32,
}

fn plain(albedo: vec3<f32>, n: vec3<f32>) -> Surface {
    return Surface(albedo, n, n, 0.0, 0.0, 1.0);
}

fn toned() -> bool { return lights.tone.x > 0.0; }

// Albedo and authored colors are sRGB; a toned scene shades them linear.
fn decode(c: vec3<f32>) -> vec3<f32> {
    if !toned() { return c; }
    return select(pow((c + 0.055) / 1.055, vec3<f32>(2.4)), c / 12.92, c <= vec3<f32>(0.04045));
}

// Exposure, then the ACES fit Anno uses (Narkowicz, input scaled by 0.6),
// then sRGB for the display.
fn present(c: vec3<f32>) -> vec3<f32> {
    if !toned() { return c; }
    let v = max(c * lights.tone.x, vec3<f32>(0.0)) * 0.6;
    let m = clamp((v * (2.51 * v + 0.03)) / (v * (2.43 * v + 0.59) + 0.14), vec3<f32>(0.0), vec3<f32>(1.0));
    return select(1.055 * pow(m, vec3<f32>(1.0 / 2.4)) - 0.055, m * 12.92, m <= vec3<f32>(0.0031308));
}

// Karis's fit of the split-sum environment BRDF ("Physically Based Shading
// on Mobile"): what share of the surroundings a surface reflects.
fn env_brdf(f0: vec3<f32>, rough: f32, ndv: f32) -> vec3<f32> {
    let r = rough * vec4<f32>(-1.0, -0.0275, -0.572, 0.022) + vec4<f32>(1.0, 0.0425, 1.04, -0.04);
    let a = min(r.x * r.x, exp2(-9.28 * ndv)) * r.x + r.y;
    let ab = vec2<f32>(-1.04, 1.04) * a + r.zw;
    return f0 * ab.x + ab.y;
}

// Display-space scenes: albedo times hemisphere ambient plus sun. Toned
// scenes: Lambert plus GGX from the sun (Schlick-Smith visibility, Schlick
// Fresnel), the hemisphere for diffuse ambient and, along the reflection,
// for specular ambient. Light is radiance times albedo times N·L with no
// 1/π, as Anno's GetAnalyticalDiffuse. Fog is the caller's.
fn shade(world: vec3<f32>, s: Surface, has_normals: bool) -> vec3<f32> {
    let n = s.normal;
    var facing = dot(n, lights.sun.xyz);
    // Derived normals face the viewer, not outward; light them two-sided.
    if !has_normals { facing = abs(facing); }
    let ndl = clamp(facing, 0.0, 1.0);
    let hemisphere = mix(lights.ground.rgb, lights.sky.rgb, n.y * 0.5 + 0.5);
    if !toned() {
        var lit = hemisphere * s.occlusion;
        if ndl > 0.0 { lit += lights.sun_color.rgb * ndl * shadow(world, s.geometry); }
        return s.albedo * lit;
    }
    let v = toward_viewer(world);
    let rough = clamp(1.0 - s.gloss, 0.04, 1.0);
    let diffuse = s.albedo * (1.0 - s.metal);
    let f0 = mix(vec3<f32>(0.04), s.albedo, s.metal);
    let ndv = max(dot(n, v), 1e-4);
    let r = reflect(-v, n);
    let around = mix(lights.ground.rgb, lights.sky.rgb, clamp(r.y * 0.5 + 0.5, 0.0, 1.0));
    var color = (diffuse * hemisphere + around * env_brdf(f0, rough, ndv)) * s.occlusion;
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
        color += sun * (diffuse * mix(1.0, s.occlusion, lights.tone.y) + d * visibility * fresnel);
    }
    return color;
}

// `albedo` times the ground's tint under `world` (bilinear between height
// samples, times the terrain's color gain) mixed in by `amount`, kept at
// most 1, as Anno multiplies models by its tint map
// (cTerrainTintIntensity). Off the grid the edge texels hold.
fn tinted(albedo: vec3<f32>, world: vec3<f32>, amount: f32) -> vec3<f32> {
    if amount <= 0.0 || lights.tint.w <= 0.0 { return albedo; }
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

fn fog(color: vec3<f32>, world: vec3<f32>) -> vec3<f32> {
    if lights.fog.w < 0.5 { return color; }
    var distance = length(globals.eye.xyz - world);
    if globals.eye.w < 0.5 { distance = max(dot(globals.anchor.xyz - world, globals.eye.xyz), 0.0); }
    let amount = clamp((distance - lights.fog_range.x) / max(lights.fog_range.y - lights.fog_range.x, 1e-6), 0.0, 1.0);
    return mix(color, lights.fog.rgb, amount);
}

// Albedo texel; alpha is 1 when the mesh says it stores something else.
fn albedo(uv: vec2<f32>) -> vec4<f32> {
    let texel = textureSample(t_albedo, s_albedo, uv);
    return select(texel, vec4<f32>(texel.rgb, 1.0), params.flags.w > 0.5);
}

fn picked(in: Output) -> vec2<u32> {
    return vec2<u32>(in.pick, bitcast<u32>(in.clip.z));
}

@fragment fn fs_lit(in: Output) -> Opaque {
    let dp = array<vec3<f32>, 2>(dpdx(in.world), dpdy(in.world));
    let duv = array<vec2<f32>, 2>(dpdx(in.uv), dpdy(in.uv));
    let texel = albedo(in.uv);
    let normal = textureSample(t_normal_map, s_albedo, in.uv);
    let surface = textureSample(t_surface_map, s_albedo, in.uv);
    // Cutout, not blending: foliage and fences sort correctly with depth writes.
    if texel.a < 0.5 { discard; }
    let has_normals = params.flags.x > 0.5;
    let n = surface_normal(in.world, cross(dp[0], dp[1]), in.normal, has_normals);
    let albedo = tinted(decode(texel.rgb) * in.color, in.world, params.extra.x);
    let s = mapped(albedo, n, dp, duv, normal, params.tone.z > 0.5, surface, params.tone.w > 0.5);
    return Opaque(vec4<f32>(present(fog(shade(in.world, s, has_normals), in.world)), 1.0), picked(in));
}

@fragment fn fs_flat(in: Output) -> Opaque {
    return Opaque(vec4<f32>(in.color, 1.0), picked(in));
}

// Shadow casters write depth only; cutouts punch holes in the shadow too.
@fragment fn fs_shadow(in: Output) {
    if albedo(in.uv).a < 0.5 { discard; }
}

// Layers stack bottom to top in layer index order. Each layer's weight is
// interpolated over the four nearest control texels first and only then
// cut by its image's alpha: alpha = saturate(s (a - 1) + (s + 1) w),
// Anno's BlendSplat, or plain w without sharpness. Cutting before
// interpolating left texel-sized steps. The lowest layer present is opaque.
// Weights come from the pixel's palette tile, whose eight channels are the
// same layers on every texel it covers, so they interpolate channel by
// channel. Layers sample with explicit gradients (world xz per pixel):
// this runs in non-uniform control flow, where implicit derivatives are
// undefined, and mips still apply.
const PALETTE_TILE: f32 = 8.0; // control::PALETTE_TILE
const PALETTE_SIDE: i32 = 10;
const NO_LAYER: u32 = 255u;

struct SplatSample {
    color: vec4<f32>,
    // World normal bent by the normal layers, stacked like color.
    normal: vec3<f32>,
}

// Layers tile in world XZ on gentle ground. Steeper faces also project
// along X (ZY plane) and Z (XY plane), as Anno's bake does, so cliffs show
// the texture instead of streaks; flat ground samples once.
fn triplanar(n: vec3<f32>) -> vec3<f32> {
    var w = pow(abs(n), vec3<f32>(6.0));
    w = w / (w.x + w.y + w.z);
    // Drop slivers so gentle ground pays for one projection.
    w = select(vec3<f32>(0.0), w, w > vec3<f32>(0.02));
    return w / (w.x + w.y + w.z);
}

// One layer's color and bent world normal, blended over the projections.
fn splat_layer(layer: u32, scale: f32, world: vec3<f32>, dw: array<vec3<f32>, 2>, n: vec3<f32>, w: vec3<f32>, normals: bool) -> SplatSample {
    var out: SplatSample;
    out.color = vec4<f32>(0.0);
    out.normal = vec3<f32>(0.0);
    for (var p = 0u; p < 3u; p++) {
        if w[p] <= 0.0 { continue; }
        // X: ZY plane, Y: XZ, Z: XY; tangent and bitangent along those axes.
        var at = world.xz;
        var dx = dw[0].xz;
        var dy = dw[1].xz;
        var t = vec3<f32>(1.0, 0.0, 0.0);
        var b = vec3<f32>(0.0, 0.0, 1.0);
        if p == 0u {
            at = world.zy; dx = dw[0].zy; dy = dw[1].zy;
            t = vec3<f32>(0.0, 0.0, 1.0); b = vec3<f32>(0.0, 1.0, 0.0);
        } else if p == 2u {
            at = world.xy; dx = dw[0].xy; dy = dw[1].xy;
            b = vec3<f32>(0.0, 1.0, 0.0);
        }
        let texel = textureSampleGrad(t_layers, s_albedo, at * scale, layer, dx * scale, dy * scale);
        out.color += w[p] * texel;
        if normals {
            let ts = textureSampleGrad(t_layer_normals, s_albedo, at * scale, layer, dx * scale, dy * scale).xyz * 2.0 - 1.0;
            let tt = normalize(t - n * dot(n, t));
            let bb = normalize(b - n * dot(n, b) - tt * dot(tt, b));
            out.normal += w[p] * (tt * ts.x + bb * ts.y + n * max(ts.z, 0.05));
        }
    }
    return out;
}

// Metres a ground pixel covers past which the splat drops its layers'
// normal maps and samples each layer from one projection only: their relief
// is below a pixel there. 0.1 ms of a 5K overview (2.28 to 2.18).
const FAR_GROUND: f32 = 0.12;

fn splat(uv: vec2<f32>, world: vec3<f32>, geometry: vec3<f32>) -> SplatSample {
    let dw = array<vec3<f32>, 2>(dpdx(world), dpdy(world));
    let far = max(length(dw[0]), length(dw[1])) > FAR_GROUND;
    var w = triplanar(geometry);
    if far {
        let a = abs(geometry);
        w = select(select(vec3<f32>(0.0, 0.0, 1.0), vec3<f32>(0.0, 1.0, 0.0), a.y >= a.z), vec3<f32>(1.0, 0.0, 0.0), a.x > a.y && a.x > a.z);
    }
    let texels = uv * params.extra.yz;
    let tiles = vec2<i32>(textureDimensions(t_palette_ids0));
    let tile = clamp(vec2<i32>(floor(texels / PALETTE_TILE)), vec2<i32>(0), tiles - 1);
    // Texel centres in the tile's region, which starts a border texel early.
    let local = texels - 0.5 - vec2<f32>(tile) * PALETTE_TILE + 1.0;
    let base = floor(local);
    let f = local - base;
    let b = vec2<i32>(base) + tile * PALETTE_SIDE;
    var cover = array<vec4<f32>, 2>(vec4<f32>(0.0), vec4<f32>(0.0));
    for (var t = 0u; t < 4u; t++) {
        let offset = vec2<i32>(i32(t & 1u), i32(t >> 1u));
        let fx = select(1.0 - f.x, f.x, (t & 1u) == 1u);
        let fy = select(1.0 - f.y, f.y, (t >> 1u) == 1u);
        cover[0] += textureLoad(t_palette_weights0, b + offset, 0) * (fx * fy);
        cover[1] += textureLoad(t_palette_weights1, b + offset, 0) * (fx * fy);
    }
    let ids = array<vec4<u32>, 2>(
        vec4<u32>(round(textureLoad(t_palette_ids0, tile, 0) * 255.0)),
        vec4<u32>(round(textureLoad(t_palette_ids1, tile, 0) * 255.0)),
    );
    let normals = params.tone.y > 0.5 && !far;
    // The lowest layer present, which is opaque.
    var lowest = 8u;
    var top = 0u;
    for (var k = 0u; k < 8u; k++) {
        if ids[k >> 2u][k & 3u] == NO_LAYER { break; }
        if cover[k >> 2u][k & 3u] > 0.0 {
            lowest = min(lowest, k);
            top = k;
        }
    }
    if lowest == 8u {
        var out = splat_layer(0u, b_layers[0].scale, world, dw, geometry, w, false);
        out.normal = geometry;
        return out;
    }
    // Composited top down with what still shows through below: the same
    // as laying each layer over those beneath, but a layer that covers the
    // pixel (full grass) ends the loop, so most pixels sample one or two
    // layers instead of every one present: at a 5K viewport the splat went
    // from 1.37 to 1.20 ms with the same pixels.
    var out: SplatSample;
    out.color = vec4<f32>(0.0);
    out.normal = vec3<f32>(0.0);
    var through = 1.0;
    for (var k = i32(top); k >= i32(lowest); k--) {
        let i = u32(k);
        let weight = cover[i >> 2u][i & 3u];
        if weight <= 0.0 { continue; }
        let layer = ids[i >> 2u][i & 3u];
        let info = b_layers[layer];
        let one = splat_layer(layer, info.scale, world, dw, geometry, w, normals);
        let texel = one.color;
        var alpha = saturate(weight);
        if info.sharpness > 0.0 {
            alpha = saturate(info.sharpness * (saturate(texel.a) - 1.0) + (info.sharpness + 1.0) * weight);
        }
        if i == lowest { alpha = 1.0; }
        out.color += texel * (alpha * through);
        if normals { out.normal += one.normal * (alpha * through); }
        through *= 1.0 - alpha;
        if through < 1.0 / 512.0 { break; }
    }
    if !normals { out.normal = geometry; }
    if dot(out.normal, out.normal) < 1e-12 { out.normal = geometry; }
    out.normal = normalize(out.normal);
    return out;
}

@fragment fn fs_splat(in: Output) -> Opaque {
    let derived = cross(dpdx(in.world), dpdy(in.world));
    var n = surface_normal(in.world, derived, in.normal, params.flags.x > 0.5);
    let sample = splat(in.uv, in.world, n);
    var s = plain(decode(sample.color.rgb) * in.color, sample.normal);
    s.geometry = n;
    return Opaque(vec4<f32>(present(fog(shade(in.world, s, params.flags.x > 0.5), in.world)), 1.0), picked(in));
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
    var color = shade(in.world, plain(base, n), params.flags.x > 0.5);
    var alpha = texel.a * params.surface.a;
    // Schlick: grazing angles turn opaque and mirror the sky.
    let facing = clamp(dot(n, toward_viewer(in.world)), 0.0, 1.0);
    let fresnel = params.blend.y * pow(1.0 - facing, 5.0);
    color = mix(color, lights.sky.rgb, fresnel);
    alpha = mix(alpha, 1.0, fresnel);
    if params.blend.x > 0.0 || params.blend.z > 0.0 {
        let gap = depth_gap(textureLoad(t_depth, vec2<i32>(in.clip.xy), 0), in.clip.z);
        if params.blend.z > 0.0 {
            alpha = mix(alpha, 1.0, smoothstep(params.blend.x, params.blend.z, gap));
        }
        if params.blend.x > 0.0 { alpha *= clamp(gap / params.blend.x, 0.0, 1.0); }
    }
    return vec4<f32>(present(fog(color, in.world)), alpha);
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

fn unproject(ndc: vec2<f32>, z: f32) -> vec3<f32> {
    let p = globals.unclip * vec4<f32>(ndc, z, 1.0);
    return p.xyz / p.w;
}

// Zenith to horizon over a band that narrows near the horizon, the
// horizon color below it, and the sun: a disc about 0.5° across and two
// lobes of glow, as forward scattering in haze.
fn sky_radiance(d: vec3<f32>) -> vec3<f32> {
    let up = clamp(d.y, 0.0, 1.0);
    var c = mix(lights.zenith.rgb, lights.horizon.rgb, pow(1.0 - up, 5.0));
    let toward = clamp(dot(d, normalize(lights.sun.xyz)), 0.0, 1.0);
    c += lights.sun_color.rgb * (0.06 * pow(toward, 12.0) + 0.25 * pow(toward, 400.0));
    if toward > 0.99996 && d.y > 0.0 { c += lights.sun_color.rgb * 12.0; }
    return c;
}

@fragment fn fs_sky(in: SkyOutput) -> @location(0) vec4<f32> {
    // Reversed depth: z = 1 is the near plane. The near plane sits almost
    // on the eye, so the ray runs from it to a point far out instead.
    let near = unproject(in.ndc, 1.0);
    let dir = normalize(unproject(in.ndc, 1e-3) - near);
    var origin = near;
    if globals.eye.w > 0.5 { origin = globals.eye.xyz; }
    var color = sky_radiance(dir);
    let level = lights.horizon.w;
    if lights.sea.w > 0.5 && dir.y < 0.0 && origin.y > level {
        let at = origin + dir * ((level - origin.y) / dir.y);
        let n = vec3<f32>(0.0, 1.0, 0.0);
        // Schlick from water's 2% at normal incidence.
        let fresnel = 0.02 + 0.98 * pow(1.0 - clamp(-dir.y, 0.0, 1.0), 5.0);
        let sun = lights.sun_color.rgb * clamp(normalize(lights.sun.xyz).y, 0.0, 1.0) * shadow(at, n);
        let body = lights.sea.rgb * (lights.sky.rgb + sun);
        color = fog(mix(body, sky_radiance(reflect(dir, n)), fresnel), at);
    } else if dir.y < 0.0 {
        color = lights.horizon.rgb;
    }
    return vec4<f32>(present(color), 1.0);
}

// Ambient occlusion (gpu::Occlusion), from the opaque depth's copy in
// t_depth: an estimate per pixel into an R8 target, then a depth-aware
// blur that multiplies it over the color.
@group(1) @binding(0) var t_occlusion: texture_2d<f32>;

// The world point under a pixel of the depth copy; w is its depth, 0
// where nothing opaque drew.
fn depth_point(px: vec2<i32>) -> vec4<f32> {
    let size = vec2<i32>(textureDimensions(t_depth));
    let p = clamp(px, vec2<i32>(0), size - 1);
    let z = textureLoad(t_depth, p, 0);
    let uv = (vec2<f32>(p) + 0.5) / vec2<f32>(size);
    return vec4<f32>(unproject(vec2<f32>(uv.x * 2.0 - 1.0, 1.0 - uv.y * 2.0), z), z);
}

const OCCLUSION_TAPS: u32 = 12u;

// Twelve points in the hemisphere over the surface, spread on a golden
// spiral and turned per pixel by interleaved gradient noise (the blur
// averages the pattern out). A point counts as occluded when the surface
// the view sees there is nearer the viewer than the point, weighted down
// the farther that surface is, so distant backgrounds do not darken.
// Runs at half resolution: each texel takes the depth of the full-size
// pixel at its top left.
@fragment fn fs_occlusion(in: SkyOutput) -> @location(0) vec4<f32> {
    let px = vec2<i32>(in.clip.xy) * 2;
    let c = depth_point(px);
    // xyz: that pixel's world point, for the blur; w: the estimate, or -1
    // where nothing was drawn.
    if c.w <= 0.0 { return vec4<f32>(0.0, 0.0, 0.0, -1.0); }
    // The normal from the nearer neighbour on each axis keeps edges sharp.
    let l = depth_point(px - vec2<i32>(1, 0));
    let r = depth_point(px + vec2<i32>(1, 0));
    let u = depth_point(px - vec2<i32>(0, 1));
    let d = depth_point(px + vec2<i32>(0, 1));
    var dx = r.xyz - c.xyz;
    if abs(l.w - c.w) < abs(r.w - c.w) { dx = c.xyz - l.xyz; }
    var dy = d.xyz - c.xyz;
    if abs(u.w - c.w) < abs(d.w - c.w) { dy = c.xyz - u.xyz; }
    var n = cross(dx, dy);
    if dot(n, n) < 1e-20 { return vec4<f32>(c.xyz, 1.0); }
    n = normalize(n);
    let view = toward_viewer(c.xyz);
    if dot(n, view) < 0.0 { n = -n; }
    let radius = lights.occlusion.x;
    let angle = 6.2831853 * fract(52.9829189 * fract(dot(in.clip.xy, vec2<f32>(0.06711056, 0.00583715))));
    let reference = select(vec3<f32>(0.0, 1.0, 0.0), vec3<f32>(1.0, 0.0, 0.0), abs(n.y) > 0.99);
    let t0 = normalize(cross(n, reference));
    let t = t0 * cos(angle) + cross(n, t0) * sin(angle);
    let b = cross(n, t);
    let size = vec2<f32>(textureDimensions(t_depth));
    var occluded = 0.0;
    for (var i = 0u; i < OCCLUSION_TAPS; i++) {
        let f = (f32(i) + 0.5) / f32(OCCLUSION_TAPS);
        let phi = f32(i) * 2.3999632;
        let spread = sqrt(f);
        let dir = (t * cos(phi) + b * sin(phi)) * spread + n * sqrt(1.0 - f);
        let reach = mix(0.15, 1.0, fract(f32(i) * 0.618034) * fract(f32(i) * 0.618034));
        let p = c.xyz + dir * radius * reach;
        let clip = globals.clip * vec4<f32>(p, 1.0);
        if clip.w <= 0.0 { continue; }
        let ndc = clip.xy / clip.w;
        let o = depth_point(vec2<i32>(vec2<f32>(ndc.x * 0.5 + 0.5, 0.5 - ndc.y * 0.5) * size));
        if o.w <= 0.0 { continue; }
        if dot(o.xyz - p, toward_viewer(p)) > 0.02 * radius {
            occluded += clamp(radius / max(length(o.xyz - c.xyz), 1e-4) - 0.5, 0.0, 1.0);
        }
    }
    return vec4<f32>(c.xyz, 1.0 - occluded / f32(OCCLUSION_TAPS));
}

// 4x4 blur over the half-size estimate's texels (8x8 pixels) whose points
// lie on the same surface as this pixel's (within a fifth of the radius),
// then darkening by the strength.
@fragment fn fs_occlusion_apply(in: SkyOutput) -> @location(0) vec4<f32> {
    let px = vec2<i32>(in.clip.xy);
    let c = depth_point(px);
    if c.w <= 0.0 { return vec4<f32>(1.0); }
    let last = vec2<i32>(textureDimensions(t_occlusion)) - 1;
    let half = px / 2;
    var sum = 0.0;
    var weight = 0.0;
    for (var y = -2; y < 2; y++) {
        for (var x = -2; x < 2; x++) {
            let q = clamp(half + vec2<i32>(x, y) + vec2<i32>(px & vec2<i32>(1)), vec2<i32>(0), last);
            let o = textureLoad(t_occlusion, q, 0);
            if o.w < 0.0 || length(o.xyz - c.xyz) > lights.occlusion.x * 0.2 { continue; }
            sum += o.w;
            weight += 1.0;
        }
    }
    let ao = select(1.0, sum / weight, weight > 0.0);
    return vec4<f32>(vec3<f32>(mix(1.0, ao, lights.occlusion.y)), 1.0);
}
