// Anno 1800's ocean waves stepped on the GPU (water_ocean.fx): the large
// waves' spectrum to time (csUpdateWaveSpectrum), its 512² transform, the
// wrap-up into displacement (csOceanFFTWrapUp) and gradients with fold
// (csOceanGradients2); the small waves' gradients from the shipped volume
// at the same time (csOceanGradients); and the mips both gradient maps are
// sampled through. The spectrum comes from the CPU (water::large_spectrum);
// the line transform is anno_core::water::FFT_WGSL, appended.

const N: u32 = 512u;
const HALF: u32 = 256u;
// IN_WIDTH: the spectrum's rows are four entries wider than the map.
const W: u32 = 516u;
// Frames in the small-wave volume (waves_small_xz/y.dds).
const FRAMES: f32 = 48.0;

struct Step {
    // x: cWaveTime; y: cChoppyScale; z: cGridLen.
    time: vec4<f32>,
}

@group(0) @binding(0) var<uniform> step: Step;
// H(0) and ω, [column][row] with W entries a column, columns 0..=N
// (water.rs upload_spectrum): the vertical pass reads a column's down its
// rows, side by side in memory.
@group(0) @binding(1) var<storage, read> h0: array<vec2<f32>>;
@group(0) @binding(2) var<storage, read> omega: array<f32>;
// After the vertical pass: one component's [column][row] (`part`); 0 Dx,
// 1 height, 2 Dz, one at a time so it holds 2 MiB, not 6. Each column
// written side by side and gathered across by the horizontal pass: written
// across, a cache line took each value.
@group(0) @binding(3) var<storage, read_write> temp: array<vec2<f32>>;
// x: the component this vertical and horizontal pass transform.
@group(0) @binding(11) var<uniform> part: vec4<u32>;
// Displacement in the FFT's units, [component][row][column].
@group(0) @binding(4) var<storage, read_write> disp: array<f32>;
@group(0) @binding(5) var out_large: texture_storage_2d<rgba16float, write>;
@group(0) @binding(6) var out_large_gradients: texture_storage_2d<rgba16float, write>;
@group(0) @binding(7) var small_xz: texture_2d_array<f32>;
@group(0) @binding(8) var small_y: texture_2d_array<f32>;
@group(0) @binding(9) var wrap: sampler;
@group(0) @binding(10) var out_small_gradients: texture_storage_2d<rgba16float, write>;

@group(1) @binding(0) var mip_in: texture_2d<f32>;
@group(1) @binding(1) var mip_out: texture_storage_2d<rgba16float, write>;

var<workgroup> line: array<vec2<f32>, 512>;

// csUpdateWaveSpectrum at column x, row y: h0(-k) is the entry mirrored
// through (N, N) and not conjugated, as the game takes it, and ω·t runs
// 1 / 0.13 faster than cWaveTime.
fn spectrum(x: u32, y: u32, component: u32) -> vec2<f32> {
    let k = h0[x * W + y];
    let mk = h0[(N - x) * W + (N - y)];
    let wt = omega[x * W + y] * step.time.x / 0.13;
    let s = sin(wt);
    let c = cos(wt);
    let ht = vec2<f32>((k.x + mk.x) * c - (k.y + mk.y) * s, (k.x - mk.x) * s + (k.y - mk.y) * c);
    if component == 1u { return ht; }
    let kx = f32(x) - f32(HALF);
    let ky = f32(y) - f32(HALF);
    let sq = kx * kx + ky * ky;
    let r = select(0.0, inverseSqrt(sq), sq > 1e-12);
    let u = select(ky * r, kx * r, component == 0u);
    return vec2<f32>(ht.y * u, -ht.x * u);
}

// One column of `part`'s component: workgroup (column).
@compute @workgroup_size(256)
fn cs_vertical(@builtin(local_invocation_index) t: u32, @builtin(workgroup_id) g: vec3<u32>) {
    let column = g.x;
    let component = part.x;
    line[t] = spectrum(column, t, component);
    line[t + HALF] = spectrum(column, t + HALF, component);
    workgroupBarrier();
    fft(t);
    let base = column * N;
    temp[base + t] = line[t];
    temp[base + t + HALF] = line[t + HALF];
}

// One row (csOceanFFTWrapUp): the real part with the centred spectrum's
// (-1)^(x+y), times cChoppyScale sideways.
@compute @workgroup_size(256)
fn cs_horizontal(@builtin(local_invocation_index) t: u32, @builtin(workgroup_id) g: vec3<u32>) {
    let row = g.x;
    let component = part.x;
    let base = (component * N + row) * N;
    line[t] = temp[t * N + row];
    line[t + HALF] = temp[(t + HALF) * N + row];
    workgroupBarrier();
    fft(t);
    let scale = select(step.time.y, 1.0, component == 1u);
    for (var i = 0u; i < 2u; i++) {
        let x = t + i * HALF;
        let sign = select(1.0, -1.0, ((x + row) & 1u) == 1u);
        disp[base + x] = line[x].x * sign * scale;
    }
}

fn d(x: u32, y: u32) -> vec3<f32> {
    let i = (y % N) * N + (x % N);
    return vec3<f32>(disp[i], disp[N * N + i], disp[2u * N * N + i]);
}

// csOceanGradients(2): the height's differences over two texels (not
// divided by their distance: the surface puts cTexelLengthX2 in the normal's
// Y instead) and the fold, max(0, 1 - J) squared, from the sideways
// displacement's differences times cGridLen · 0.45.
fn gradients(l: vec3<f32>, r: vec3<f32>, b: vec3<f32>, f: vec3<f32>) -> vec4<f32> {
    let gradient = vec2<f32>(-(r.y - l.y), -(f.y - b.y));
    let k = step.time.z * 0.45;
    let dx = (r.xz - l.xz) * k;
    let dz = (f.xz - b.xz) * k;
    let j = (1.0 + dx.x) * (1.0 + dz.y) - dx.y * dz.x;
    let fold = max(1.0 - j, 0.0);
    return vec4<f32>(gradient, 0.0, fold * fold);
}

@compute @workgroup_size(16, 16)
fn cs_large(@builtin(global_invocation_id) id: vec3<u32>) {
    let x = id.x;
    let y = id.y;
    let texel = vec2<i32>(i32(x), i32(y));
    textureStore(out_large, texel, vec4<f32>(d(x, y), 1.0));
    textureStore(out_large_gradients, texel, gradients(d(x + N - 1u, y), d(x + 1u, y), d(x, y + N - 1u), d(x, y + 1u)));
}

// The small waves at `uv` and the volume's time coordinate `w`: its 48
// frames as layers, blended the way a 3D texture's linear filter wrapping
// in depth blends them.
fn small_at(uv: vec2<f32>, w: f32) -> vec3<f32> {
    let z = fract(w) * FRAMES - 0.5;
    let first = floor(z);
    let t = z - first;
    let a = i32((first + FRAMES) % FRAMES);
    let b = (a + 1) % i32(FRAMES);
    let xz = mix(textureSampleLevel(small_xz, wrap, uv, a, 0.0).xy, textureSampleLevel(small_xz, wrap, uv, b, 0.0).xy, t);
    let y = mix(textureSampleLevel(small_y, wrap, uv, a, 0.0).x, textureSampleLevel(small_y, wrap, uv, b, 0.0).x, t);
    return vec3<f32>(xz.x, y, xz.y);
}

@compute @workgroup_size(16, 16)
fn cs_small(@builtin(global_invocation_id) id: vec3<u32>) {
    let one = 1.0 / f32(N);
    let uv = (vec2<f32>(id.xy) + 0.5) * one;
    let w = step.time.x;
    let l = small_at(uv - vec2<f32>(one, 0.0), w);
    let r = small_at(uv + vec2<f32>(one, 0.0), w);
    let b = small_at(uv - vec2<f32>(0.0, one), w);
    let f = small_at(uv + vec2<f32>(0.0, one), w);
    textureStore(out_small_gradients, vec2<i32>(id.xy), gradients(l, r, b, f));
}

// The next mip, a 2x2 box.
@compute @workgroup_size(8, 8)
fn cs_mip(@builtin(global_invocation_id) id: vec3<u32>) {
    let size = textureDimensions(mip_out);
    if id.x >= size.x || id.y >= size.y { return; }
    let p = vec2<i32>(id.xy) * 2;
    var a = vec4<f32>(0.0);
    for (var k = 0; k < 4; k++) {
        a += textureLoad(mip_in, p + vec2<i32>(k & 1, k >> 1), 0);
    }
    textureStore(mip_out, vec2<i32>(id.xy), a * 0.25);
}
