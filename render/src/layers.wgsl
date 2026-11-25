// 2070's ground (terrain.fx), from `Layers` paint as stored: each 16 m
// chunk lists its layers in draw rank (`SplatID`) order, each with a 17x17
// coverage plane in an R8 atlas. The base layer is opaque; every layer
// above lays over what is below it by BlendSplat.

struct LayersInfo {
    // The island's corner in world X and Z, and chunks across and down.
    origin: vec2<f32>,
    chunks: vec2<u32>,
    // Metres one repeat of a layer's textures spans (TextureScale), and
    // coverage planes across the atlas.
    scale: f32,
    columns: u32,
    pad: vec2<u32>,
}

@group(3) @binding(0) var<uniform> layers_info: LayersInfo;
// Per chunk: its first entry and how many, base layer first.
@group(3) @binding(1) var<storage, read> layer_spans: array<vec2<u32>>;
// Per entry: the texture arrays' layer in the low byte, its coverage
// plane's atlas cell above.
@group(3) @binding(2) var<storage, read> layer_entries: array<u32>;
@group(3) @binding(3) var t_coverage: texture_2d<f32>;
@group(3) @binding(4) var t_layer_diffuse: texture_2d_array<f32>;
// Tangent x and y in red and green, glow in blue, specular in alpha.
@group(3) @binding(5) var t_layer_normal: texture_2d_array<f32>;

const CHUNK: f32 = 16.0;
const PLANE_SIDE: u32 = 17u;
// BlendSplat (terrain.fx:54-61): 5 (a - 1) + 6 coverage, with the diffuse
// alpha taken as 0.5, an approximation: the game reads it per texel as
// blend noise. The stand-in keeps the cut where coverage is 5/12.
const SHARPNESS: f32 = 5.0;
const DIFFUSE_ALPHA: f32 = 0.5;
// DeepWaterChunk's albedo (terrain.fx:260-279), for chunks without paint.
const DEEP_WATER: f32 = 0.05;

// Coverage of the plane in atlas cell `cell` at `local` metres into its
// chunk: bilinear between the metre vertices, as the game's clamped linear
// sampler reads it at its half-texel corrected coordinates (terrain.fx:151).
fn layer_coverage(cell: u32, local: vec2<f32>) -> f32 {
    let corner = vec2<u32>(cell % layers_info.columns, cell / layers_info.columns) * PLANE_SIDE;
    let p = clamp(local, vec2<f32>(0.0), vec2<f32>(CHUNK));
    let base = min(floor(p), vec2<f32>(CHUNK - 1.0));
    let f = p - base;
    let at = corner + vec2<u32>(base);
    let c00 = textureLoad(t_coverage, at, 0).r;
    let c10 = textureLoad(t_coverage, at + vec2<u32>(1u, 0u), 0).r;
    let c01 = textureLoad(t_coverage, at + vec2<u32>(0u, 1u), 0).r;
    let c11 = textureLoad(t_coverage, at + vec2<u32>(1u, 1u), 0).r;
    return mix(mix(c00, c10, f.x), mix(c01, c11, f.x), f.y);
}

fn layer_alpha(coverage: f32) -> f32 {
    return saturate(SHARPNESS * (DIFFUSE_ALPHA - 1.0) + (SHARPNESS + 1.0) * coverage);
}

fn ground(uv: vec2<f32>, world: vec3<f32>, n: vec3<f32>, dw: array<vec3<f32>, 2>) -> GroundSample {
    let rel = world.xz - layers_info.origin;
    let chunk = vec2<u32>(clamp(floor(rel / CHUNK), vec2<f32>(0.0), vec2<f32>(layers_info.chunks) - 1.0));
    let local = rel - vec2<f32>(chunk) * CHUNK;
    let span = layer_spans[chunk.y * layers_info.chunks.x + chunk.x];
    if span.y == 0u {
        return GroundSample(vec4<f32>(decode(vec3<f32>(DEEP_WATER)), 1.0), n, 0.0, 0.0, 0.0);
    }
    // World XZ over the texture scale, V running against +Z (terrain.fx:150).
    let flip = vec2<f32>(1.0, -1.0) / layers_info.scale;
    let at = world.xz * flip;
    let dx = dw[0].xz * flip;
    let dy = dw[1].xz * flip;
    // Blended as stored, as the game's splat draws blend into its G-buffer.
    var color = vec3<f32>(0.0);
    var normal = vec4<f32>(0.0);
    for (var i = 0u; i < span.y; i++) {
        let entry = layer_entries[span.x + i];
        var alpha = 1.0;
        if i > 0u { alpha = layer_alpha(layer_coverage(entry >> 8u, local)); }
        if alpha <= 0.0 { continue; }
        let layer = entry & 255u;
        let diffuse = textureSampleGrad(t_layer_diffuse, s_albedo, at, layer, dx, dy);
        let bump = textureSampleGrad(t_layer_normal, s_albedo, at, layer, dx, dy);
        color = mix(color, diffuse.rgb, alpha);
        normal = mix(normal, bump, alpha);
    }
    // The tangent frame terrain.fx:163-166 builds from the vertex normal.
    var t = vec3<f32>(n.y, -n.x, 0.0);
    if dot(t, t) < 1e-8 { t = vec3<f32>(1.0, 0.0, 0.0); }
    t = normalize(t);
    let b = -normalize(cross(t, n));
    let xy = normal.xy * 2.0 - 1.0;
    let bent = t * xy.x + b * xy.y + n * sqrt(max(1.0 - dot(xy, xy), 0.0));
    return GroundSample(vec4<f32>(decode(color), 1.0), normalize(bent), normal.a, 0.0, normal.b);
}

fn ground_displacement(uv: vec2<f32>, world: vec3<f32>, n: vec3<f32>, dw: array<vec3<f32>, 2>) -> f32 {
    return 0.0;
}
