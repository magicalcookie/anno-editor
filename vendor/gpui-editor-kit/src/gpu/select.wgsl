// The selection's per-frame work over placement rows, so a selection of
// 20k items costs the CPU nothing per frame (placed.rs `Selection`).
//
// cs_move: while a drag runs, every selected row's transform is the drag's
// matrix applied to the row's transform when the drag began (`dragged`).
// cs_marks: one marker per selected item at its origin, read through one of
// its rows, written after the scene's own markers. Runs after cs_move, so
// the markers follow the drag.

struct Params {
    delta: mat4x4<f32>,
    // rgb, shape
    color: vec4<f32>,
    rows: u32,
    marks: u32,
    // First marker this pass writes: the scene's come before it.
    base: u32,
    radius: f32,
}

struct Marker {
    position: vec4<f32>,
    color: vec4<f32>,
}

@group(0) @binding(0) var<uniform> p: Params;
@group(0) @binding(1) var<storage, read> rows: array<u32>;
@group(0) @binding(2) var<storage, read> starts: array<mat4x4<f32>>;
@group(0) @binding(3) var<storage, read_write> transforms: array<mat4x4<f32>>;
// xyz: the item's origin in the row's own space; w: the row, bitcast.
@group(0) @binding(4) var<storage, read> marks: array<vec4<f32>>;
@group(0) @binding(5) var<storage, read_write> markers: array<Marker>;

// `start` moved by the drag's matrix, as an affine product that keeps the
// start's bottom row. That row is not a projective one: it carries the
// placement's terrain adaption (gpu::adapting: allow, rise, follow), and
// `delta * start` multiplied the drag's translation by it into the first
// three columns, shearing every row that may bend.
fn dragged(delta: mat4x4<f32>, start: mat4x4<f32>) -> mat4x4<f32> {
    let turn = mat3x3<f32>(delta[0].xyz, delta[1].xyz, delta[2].xyz);
    return mat4x4<f32>(
        vec4<f32>(turn * start[0].xyz, start[0].w),
        vec4<f32>(turn * start[1].xyz, start[1].w),
        vec4<f32>(turn * start[2].xyz, start[2].w),
        vec4<f32>(turn * start[3].xyz + delta[3].xyz, start[3].w),
    );
}

@compute @workgroup_size(64)
fn cs_move(@builtin(global_invocation_id) id: vec3<u32>) {
    let i = id.x;
    if i >= p.rows {
        return;
    }
    transforms[rows[i]] = dragged(p.delta, starts[i]);
}

@compute @workgroup_size(64)
fn cs_marks(@builtin(global_invocation_id) id: vec3<u32>) {
    let i = id.x;
    if i >= p.marks {
        return;
    }
    let m = marks[i];
    let at = transforms[bitcast<u32>(m.w)] * vec4<f32>(m.xyz, 1.0);
    markers[p.base + i] = Marker(vec4<f32>(at.xyz, p.radius), p.color);
}
