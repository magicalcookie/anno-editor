// Depth pyramid for occlusion culling (cull.wgsl). Level 0 is half the
// view's depth target; each texel keeps the farthest depth (reversed: the
// smallest) of the 2x2 texels below it, so a box behind a texel's value is
// behind everything that texel covers. Sizes round up, and reads past the
// last source texel clamp, so no source texel is skipped.
@group(0) @binding(0) var h_depth: texture_depth_2d;
@group(0) @binding(1) var h_out: texture_storage_2d<r32float, write>;
@group(0) @binding(2) var h_level: texture_2d<f32>;

fn farthest(a: f32, b: f32, c: f32, d: f32) -> f32 {
    return min(min(a, b), min(c, d));
}

@compute @workgroup_size(8, 8)
fn cs_hzb_depth(@builtin(global_invocation_id) id: vec3<u32>) {
    if any(id.xy >= textureDimensions(h_out)) { return; }
    let last = vec2<i32>(textureDimensions(h_depth)) - 1;
    let p = vec2<i32>(id.xy) * 2;
    let d = farthest(
        textureLoad(h_depth, min(p, last), 0),
        textureLoad(h_depth, min(p + vec2<i32>(1, 0), last), 0),
        textureLoad(h_depth, min(p + vec2<i32>(0, 1), last), 0),
        textureLoad(h_depth, min(p + vec2<i32>(1, 1), last), 0),
    );
    textureStore(h_out, id.xy, vec4<f32>(d));
}

@compute @workgroup_size(8, 8)
fn cs_hzb_down(@builtin(global_invocation_id) id: vec3<u32>) {
    if any(id.xy >= textureDimensions(h_out)) { return; }
    let last = vec2<i32>(textureDimensions(h_level)) - 1;
    let p = vec2<i32>(id.xy) * 2;
    let d = farthest(
        textureLoad(h_level, min(p, last), 0).r,
        textureLoad(h_level, min(p + vec2<i32>(1, 0), last), 0).r,
        textureLoad(h_level, min(p + vec2<i32>(0, 1), last), 0).r,
        textureLoad(h_level, min(p + vec2<i32>(1, 1), last), 0).r,
    );
    textureStore(h_out, id.xy, vec4<f32>(d));
}
