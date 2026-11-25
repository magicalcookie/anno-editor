// Each mip of a baked impostor layer from the one above (gpu.rs
// bake_impostors): 2x2 texels, color, normal, depth and gloss weighed
// by coverage, so
// leaves do not darken toward the empty texels round them. And the texels
// each view covers at the finest level, which size the drawn quads.
@group(0) @binding(0) var t_albedo: texture_2d<f32>;
@group(0) @binding(1) var t_normal: texture_2d<f32>;
@group(0) @binding(2) var t_rest: texture_2d<f32>;

// placed.wgsl's octahedral normals in two bytes.
fn oct_encode(n: vec3<f32>) -> vec2<f32> {
    let p = n.xz / (abs(n.x) + abs(n.y) + abs(n.z));
    var e = p;
    if n.y < 0.0 { e = (1.0 - abs(p.yx)) * select(vec2<f32>(-1.0), vec2<f32>(1.0), p >= vec2<f32>(0.0)); }
    return e * 0.5 + 0.5;
}

fn oct_decode(e: vec2<f32>) -> vec3<f32> {
    let p = e * 2.0 - 1.0;
    var n = vec3<f32>(p.x, 1.0 - abs(p.x) - abs(p.y), p.y);
    let t = max(-n.y, 0.0);
    n.x += select(t, -t, n.x >= 0.0);
    n.z += select(t, -t, n.z >= 0.0);
    return normalize(n);
}

@vertex fn vs_mip(@builtin(vertex_index) i: u32) -> @builtin(position) vec4<f32> {
    let uv = vec2<f32>(f32((i << 1u) & 2u), f32(i & 2u));
    return vec4<f32>(uv * 2.0 - 1.0, 0.0, 1.0);
}

struct Mip {
    @location(0) albedo: vec4<f32>,
    @location(1) normal: vec2<f32>,
    @location(2) rest: vec2<f32>,
}

@fragment fn fs_mip(@builtin(position) p: vec4<f32>) -> Mip {
    let b = vec2<i32>(p.xy) * 2;
    var color = vec3<f32>(0.0);
    var cover = 0.0;
    var n = vec3<f32>(0.0);
    // Depth and gloss.
    var rest = vec2<f32>(0.0);
    for (var k = 0; k < 4; k++) {
        let at = b + vec2<i32>(k & 1, k >> 1);
        let a = textureLoad(t_albedo, at, 0);
        color += a.rgb * a.a;
        cover += a.a;
        n += oct_decode(textureLoad(t_normal, at, 0).xy) * a.a;
        rest += textureLoad(t_rest, at, 0).xy * a.a;
    }
    if cover <= 0.0 { return Mip(vec4<f32>(0.0), vec2<f32>(0.5), vec2<f32>(0.0, 1.0)); }
    let normal = select(vec3<f32>(0.0, 1.0, 0.0), normalize(n), dot(n, n) > 1e-12);
    return Mip(vec4<f32>(color / cover, cover * 0.25), oct_encode(normal), rest / cover);
}

// The covered texels of each view's tile in a batch of staged layers: one
// workgroup per tile (x, y its cell, z the staged layer), written as x0,
// y0, x1, y1 past the last into the layer's row of t_impostor_bounds
// (placed.wgsl), x1 <= x0 for a view covering nothing. Group 1 keeps them
// apart from the mips' bindings. The bake writes
// coverage 0 or 1 at this level.
@group(1) @binding(0) var t_staged: texture_2d_array<f32>;
@group(1) @binding(1) var t_bounds: texture_storage_2d<rgba8uint, write>;
// x: the batch's first layer, y: texels per tile side.
@group(1) @binding(2) var<uniform> batch: vec4<u32>;
const VIEWS: u32 = 8u; // placed::IMPOSTOR_VIEWS

var<workgroup> least: array<atomic<u32>, 2>;
var<workgroup> past: array<atomic<u32>, 2>;

@compute @workgroup_size(8, 8)
fn cs_bounds(@builtin(workgroup_id) tile_id: vec3<u32>, @builtin(local_invocation_id) at: vec3<u32>, @builtin(local_invocation_index) i: u32) {
    let tile = batch.y;
    if i == 0u {
        atomicStore(&least[0], tile);
        atomicStore(&least[1], tile);
        atomicStore(&past[0], 0u);
        atomicStore(&past[1], 0u);
    }
    workgroupBarrier();
    let origin = tile_id.xy * tile;
    for (var y = at.y; y < tile; y += 8u) {
        for (var x = at.x; x < tile; x += 8u) {
            if textureLoad(t_staged, origin + vec2<u32>(x, y), tile_id.z, 0).a > 0.0 {
                atomicMin(&least[0], x);
                atomicMin(&least[1], y);
                atomicMax(&past[0], x + 1u);
                atomicMax(&past[1], y + 1u);
            }
        }
    }
    workgroupBarrier();
    if i == 0u {
        let bounds = vec4<u32>(atomicLoad(&least[0]), atomicLoad(&least[1]), atomicLoad(&past[0]), atomicLoad(&past[1]));
        textureStore(t_bounds, vec2<u32>(tile_id.y * VIEWS + tile_id.x, batch.x + tile_id.z), bounds);
    }
}
