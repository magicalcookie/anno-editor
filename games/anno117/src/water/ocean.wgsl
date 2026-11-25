// Anno 117's ocean waves stepped on the GPU (ocean_simulation.fx): the
// spectra's time step and inverse transform per frame, then displacement
// with the fold, and slopes, with their mips. Eight 256² slices: coastal
// bands 0-3, offshore 4-7; the slopes hold a band's coastal and offshore
// pair in one texel, four layers, so the sea reads both with one fetch
// where it blends them. Spectra come from the CPU (gpu::ocean); the transform is
// anno_core::water::FFT_WGSL, appended.

const N: u32 = 256u;
const HALF: u32 = 128u;
const SLICES: u32 = 8u;

struct Step {
    // x: wave time in seconds.
    time: vec4<f32>,
    // ChoppyScale per slice.
    choppy: array<vec4<f32>, 2>,
    // Patch side in metres per band.
    patches: vec4<f32>,
}

@group(0) @binding(0) var<uniform> step: Step;
// h0, [slice][row][column], and ω, [slice][column][row] (ocean.rs
// Simulation::step): read along a column, as the vertical pass does.
@group(0) @binding(1) var<storage, read> h0: array<vec2<f32>>;
@group(0) @binding(2) var<storage, read> omega: array<f32>;
// Two bands at a time (`part`), their four slices, so 6 MiB, not 12: after
// the vertical pass [component][slice][column][row], component 0 Dx, 1 Dy,
// 2 Dz, each column written side by side and read across by the
// horizontal pass (written across, a cache line took each value, 0.02 ms
// of the step on Albion at 5K).
@group(0) @binding(3) var<storage, read_write> temp: array<vec2<f32>>;
// The part's displacement in metres, [component][slice][row][column]: in
// `temp` in place it was written down columns, which doubled the step.
@group(0) @binding(4) var<storage, read_write> disp: array<f32>;
// x: the bands stepped, 0 for bands 0 and 1, 1 for 2 and 3.
@group(0) @binding(7) var<uniform> part: vec4<u32>;
// (Dx, Dy, Dz, fold) per slice; (-dh/dx, -dh/dz) of the coastal slice and
// of the offshore one per band.
@group(0) @binding(5) var out_displacement: texture_storage_2d_array<rgba16float, write>;
@group(0) @binding(6) var out_gradients: texture_storage_2d_array<rgba16float, write>;

@group(1) @binding(0) var mip_displacement: texture_2d_array<f32>;
@group(1) @binding(1) var mip_displacement_out: texture_storage_2d_array<rgba16float, write>;
@group(1) @binding(2) var mip_gradients: texture_2d_array<f32>;
@group(1) @binding(3) var mip_gradients_out: texture_storage_2d_array<rgba16float, write>;

var<workgroup> line: array<vec2<f32>, 256>;

fn at(slice: u32, row: u32, column: u32) -> u32 {
    return (slice * N + row) * N + column;
}

fn choppy(slice: u32) -> f32 {
    return step.choppy[slice / 4u][slice % 4u];
}

// csUpdateWaveSpectrum at column i, row j: h0 read transposed and h0(-k)
// one texel off and not conjugated, as the game does, so the waves face
// the same way and only the real part of the transform is kept.
fn spectrum(slice: u32, i: u32, j: u32, component: u32) -> vec2<f32> {
    let k = h0[at(slice, i, j)];
    let mk = h0[at(slice, N - 1u - i, N - 1u - j)];
    let wt = omega[at(slice, i, j)] * step.time.x;
    let s = sin(wt);
    let c = cos(wt);
    let ht = vec2<f32>((k.x + mk.x) * c - (k.y - mk.y) * s, (k.x - mk.x) * s + (k.y + mk.y) * c);
    if component == 1u { return ht; }
    var kx = f32(i) - f32(HALF);
    var ky = f32(j) - f32(HALF);
    let sq = kx * kx + ky * ky;
    let r = select(0.0, inverseSqrt(sq), sq > 1e-12);
    let u = select(ky * r, kx * r, component == 0u);
    return vec2<f32>(ht.y * u, -ht.x * u);
}

// Slices a part steps: its two bands' coastal and offshore ones.
const PART: u32 = 4u;

// Part slice `l`'s slice: its bands' coastal slices, then their offshore.
fn part_slice(l: u32) -> u32 {
    return part.x * 2u + (l & 1u) + (l >> 1u) * (SLICES / 2u);
}

// One column of one component of one slice: workgroup (column, part
// slice, component).
@compute @workgroup_size(128)
fn cs_vertical(@builtin(local_invocation_index) t: u32, @builtin(workgroup_id) g: vec3<u32>) {
    let column = g.x;
    let slice = part_slice(g.y);
    let component = g.z;
    line[t] = spectrum(slice, column, t, component);
    line[t + HALF] = spectrum(slice, column, t + HALF, component);
    workgroupBarrier();
    fft(t);
    let base = (component * PART + g.y) * N * N;
    temp[base + column * N + t] = line[t];
    temp[base + column * N + t + HALF] = line[t + HALF];
}

// One row: the real part, with the centred spectrum's (-1)^(x+y), times
// ChoppyScale for the sideways components.
@compute @workgroup_size(128)
fn cs_horizontal(@builtin(local_invocation_index) t: u32, @builtin(workgroup_id) g: vec3<u32>) {
    let row = g.x;
    let slice = part_slice(g.y);
    let component = g.z;
    let columns = (component * PART + g.y) * N * N;
    line[t] = temp[columns + t * N + row];
    line[t + HALF] = temp[columns + (t + HALF) * N + row];
    workgroupBarrier();
    fft(t);
    let scale = select(choppy(slice), 1.0, component == 1u);
    for (var i = 0u; i < 2u; i++) {
        let x = t + i * HALF;
        let sign = select(1.0, -1.0, ((x + row) & 1u) == 1u);
        disp[columns + row * N + x] = line[x].x * sign * scale;
    }
}

// Part slice `l`'s displacement `component` at texel (x, y), wrapped.
fn d(component: u32, l: u32, x: u32, y: u32) -> f32 {
    return disp[(component * PART + l) * N * N + (y % N) * N + (x % N)];
}

// csOceanWaveGradients' slope (-dh/dx, -dh/dz) and fold at texel (x, y) of
// part slice `l`, slice `slice`. ChoppyScale applies to the fold a second
// time, as in game.
fn slope_fold(slice: u32, p: u32, x: u32, y: u32) -> vec3<f32> {
    let w = x + N - 1u;
    let r = x + 1u;
    let b = y + N - 1u;
    let f = y + 1u;
    let dist = 2.0 * step.patches[slice % 4u] / f32(N);
    let chop = choppy(slice);
    let gradient = vec2<f32>(
        -(d(1u, p, r, y) - d(1u, p, w, y)) / dist,
        -(d(1u, p, x, f) - d(1u, p, x, b)) / dist,
    );
    let jxx = 1.0 + chop * (d(0u, p, r, y) - d(0u, p, w, y)) / dist;
    let jyy = 1.0 + chop * (d(2u, p, x, f) - d(2u, p, x, b)) / dist;
    let jxy = chop * (d(0u, p, x, f) - d(0u, p, x, b)) / dist;
    let low = 0.5 * ((jxx + jyy) - sqrt((jxx - jyy) * (jxx - jyy) + 4.0 * jxy * jxy));
    return vec3<f32>(gradient, max(0.0, 1.0 - low));
}

// Displacement and fold per slice, and per band its two slopes: thread
// (x, y, part slice) writes its slice's displacement, and a coastal
// slice's thread both slopes of its band.
@compute @workgroup_size(16, 16)
fn cs_finish(@builtin(global_invocation_id) id: vec3<u32>) {
    let x = id.x;
    let y = id.y;
    let l = id.z;
    let slice = part_slice(l);
    let texel = vec2<i32>(i32(x), i32(y));
    let own = slope_fold(slice, l, x, y);
    textureStore(out_displacement, texel, slice, vec4<f32>(d(0u, l, x, y), d(1u, l, x, y), d(2u, l, x, y), own.z));
    if l < 2u {
        let offshore = slope_fold(slice + SLICES / 2u, l + 2u, x, y);
        textureStore(out_gradients, texel, slice, vec4<f32>(own.xy, offshore.xy));
    }
}

// The next mip of both textures, a 2x2 box; the slopes have half the
// layers.
@compute @workgroup_size(8, 8)
fn cs_mip(@builtin(global_invocation_id) id: vec3<u32>) {
    let size = textureDimensions(mip_displacement_out);
    if id.x >= size.x || id.y >= size.y { return; }
    let p = vec2<i32>(id.xy) * 2;
    var a = vec4<f32>(0.0);
    var b = vec4<f32>(0.0);
    let slopes = id.z < SLICES / 2u;
    for (var k = 0; k < 4; k++) {
        let q = p + vec2<i32>(k & 1, k >> 1);
        a += textureLoad(mip_displacement, q, id.z, 0);
        if slopes { b += textureLoad(mip_gradients, q, id.z, 0); }
    }
    textureStore(mip_displacement_out, vec2<i32>(id.xy), id.z, a * 0.25);
    if slopes { textureStore(mip_gradients_out, vec2<i32>(id.xy), id.z, b * 0.25); }
}
