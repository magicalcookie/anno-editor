// --- Physical waves --------------------------------------------------------
// One step of Anno 1800's physical waves (water_physical_waves.fx
// csPhysicalWaves) after surface.wgsl: a damped wave equation over a map
// that follows the camera, fed by waves the coasts and beaches set off and
// by the FFT crests near the coast, with the foam both make. Stored by
// world texel modulo the map's side, so the window moves without copying;
// texels it newly covers start still. The ocean reads the result as
// s_phys0 and s_phys1.

struct PhysParams {
    // The window's first world texel X and Z; the last step's.
    window: vec4<f32>,
    // World units a texel, seconds, map side in texels, the coast map's
    // depth scale (0 without one).
    misc: vec4<f32>,
}

@group(2) @binding(0) var<uniform> phys: PhysParams;
@group(2) @binding(1) var p_prev: texture_2d<f32>;
@group(2) @binding(2) var p_out0: texture_storage_2d<rgba16float, write>;
@group(2) @binding(3) var p_out1: texture_storage_2d<rgba16float, write>;

fn phys_side() -> i32 {
    return i32(phys.misc.z);
}

// Where world texel `w` is stored.
fn phys_index(w: vec2<i32>) -> vec2<i32> {
    let n = phys_side();
    return ((w % n) + n) % n;
}

// Last step's state at world texel `w`: still where either window did not
// cover it.
fn phys_prev(w: vec2<i32>) -> vec4<f32> {
    let n = phys_side();
    let now = vec2<i32>(phys.window.xy);
    let was = vec2<i32>(phys.window.zw);
    if any(w < now) || any(w >= now + n) || any(w < was) || any(w >= was + n) {
        return vec4<f32>(0.0);
    }
    return textureLoad(p_prev, phys_index(w), 0);
}

// The ground under the water at world X/Z, as the game's
// SampleTerrainHeightMap gives it: from the coast map, which measures the
// heightfield (depth over phys.misc.w units, linear). Painted WaterInfos
// depth is an artist's wave-blend mask, not the seabed: shipped islands
// paint calm bays near 0 over ground two units down, and reading it as
// the ground put the whole bay within a unit of the surface, foaming
// every step. Its byte stands in only without a coast map, as the
// island's own encoder writes it: 255 · (depth / 5 m)^1.5.
fn phys_ground(xz: vec2<f32>, infos: vec4<f32>) -> f32 {
    if sea.coast.z > 0.0 && phys.misc.w > 0.0 {
        return -coast_at(xz).y * phys.misc.w;
    }
    return -5.0 * pow(infos.r, 2.0 / 3.0);
}

fn phys_noise(uv: vec2<f32>) -> f32 {
    return textureSampleLevel(s_noise2, s_wrap, uv, 0.0).g;
}

@compute @workgroup_size(8, 8)
fn cs_physical(@builtin(global_invocation_id) id: vec3<u32>) {
    let n = phys_side();
    let k = vec2<i32>(id.xy);
    if any(k >= vec2<i32>(n)) { return; }
    let w = vec2<i32>(phys.window.xy) + k;
    let texel = phys.misc.x;
    let seconds = phys.misc.y;
    var pos = (vec2<f32>(w) + 0.5) * texel;
    let infos = water_infos(pos);
    let depth = infos.r;
    // The FFT waves' displacement, as the ocean's vertices take it.
    let uv = pos * sea.uv.x + sea.uv.y;
    let large = textureSampleLevel(s_large, s_wrap, uv, 1.0).xyz;
    let displacement = (small_waves(uv) * 0.8 * (1.0 - depth * 0.7) + large * depth) * sea.uv.z;
    pos += displacement.xz;
    let flow = water_flow(pos);
    let ground = phys_ground(pos, water_infos(pos));
    // Beaches slow the waves down, wide ones most.
    let coast_height = pow(1.0 - clamp(-ground / 2.0, 0.0, 1.0), 6.0) * infos.b;
    var wave = phys_prev(w);
    let south = phys_prev(w + vec2<i32>(0, -1)).x;
    let west = phys_prev(w + vec2<i32>(-1, 0)).x;
    let east = phys_prev(w + vec2<i32>(1, 0)).x;
    let north = phys_prev(w + vec2<i32>(0, 1)).x;
    let current = wave.x;
    let old = wave.y;
    let acceleration = 64.0 * (south + west + east + north - 4.0 * current);
    let dt = 1.0 / (16.0 + coast_height * 64.0);
    wave.y = current;
    wave.x = 1.99 * current - 0.99 * old + 0.5 * acceleration * dt * dt;
    // Foam dies down: the FFT waves' (z) and the physical waves' (w).
    wave.z *= 0.93;
    wave.w *= 0.91;
    var normal_choppy = vec4<f32>(south + west - east - north, south - west + east - north, 0.0, 0.0);
    let choppy = (vec2<f32>(east, north) - current) * 0.25;
    normal_choppy = vec4<f32>(normal_choppy.xy, choppy);
    // The map's border, 8 world units wide, stays still.
    let border_at = abs((vec2<f32>(k) + 0.5) / f32(n) * 2.0 - 1.0);
    let border = clamp(((1.0 - max(border_at.x, border_at.y)) * 0.5 * f32(n) * texel) / 8.0, 0.0, 1.0);
    let noise_uv = pos * 0.0055;
    var foam_noise = phys_noise(noise_uv - seconds * 0.057) * 0.6 + phys_noise(noise_uv + 0.5 + seconds * 0.043) * 0.4;
    foam_noise = foam_noise * foam_noise * foam_noise;
    foam_noise *= clamp(pow(phys_noise(noise_uv * 4.0 - seconds * 0.05), 2.0) * 3.0, 0.0, 1.0);
    foam_noise = clamp(foam_noise * 1.5, 0.0, 1.0);
    let crests_large = textureSampleLevel(s_large_gradients, s_wrap, uv, 1.0).a;
    let crests_small = textureSampleLevel(s_small_gradients, s_wrap, uv, 1.0).a;
    let crests_fft = clamp(0.5 * crests_small * (1.0 - depth * 0.5) + crests_large * depth, 0.0, 1.0);
    let crests_phys = clamp(crests_small * (1.0 - depth * 0.5) + crests_large * depth, 0.0, 1.0);
    wave.z += pow(crests_large, 2.0) * 2.5 * foam_noise * depth * depth * depth;
    // Wake foam and foam on the physical waves' peaks.
    wave.w += pow(clamp(wave.x * 0.75 - 0.15, 0.0, 1.0), 3.0) * 0.45;
    wave.w += pow((abs(choppy.x) + abs(choppy.y)) * 7.0, 3.0) * 10.0;
    // Near the ground the crests raise waves and break into foam. The game
    // measures the water against the depth buffer along the view; here
    // straight down to the ground, as from above.
    let delta = -ground;
    if delta < 1.0 {
        let coastal = flow.b * flow.b;
        wave.x += 0.025 * pow(crests_phys, 1.5) * coastal;
        let impact = pow(crests_fft, 1.5) * (1.0 - infos.b);
        wave.w += impact;
        wave.z += impact;
    }
    // A slim foam border wherever water meets something, not on beaches.
    if delta < 0.25 {
        let contact = (1.0 - infos.b) * (0.33 + flow.b * 0.67) * 0.5 * (0.25 - delta);
        wave.w += contact;
        wave.z += contact;
    }
    if wave.x > 0.05 {
        wave.x *= 1.0 + 0.02 * clamp(crests_fft * crests_fft, 0.0, 1.0);
    }
    // Waves the coast sets off: three rings moving in from 13 m out every
    // 14 seconds, two on beaches, broken up by noise.
    let coast_uv = pos * 0.018;
    let n1 = clamp(pow(phys_noise(coast_uv - floor(seconds * 0.3) / 0.3 * 0.4), 3.0) * 4.0, 0.0, 1.0);
    let n2 = clamp(pow(phys_noise(coast_uv * 1.2 + 0.35 - floor(seconds * 0.2) / 0.2 * 0.3), 3.0) * 4.0, 0.0, 1.0);
    let n3 = clamp(pow(phys_noise(coast_uv * 1.1 + 0.65 - floor(seconds * 0.25) / 0.25 * 0.35), 3.0) * 4.0, 0.0, 1.0);
    let beach_noise = phys_noise(pos * 0.006 + seconds * 0.006);
    let coast_dist = clamp(infos.g * flow.b, 0.0, 1.0);
    let beach_dist = clamp(pow(1.0 - clamp(-ground / 6.0 + 0.1, 0.0, 1.0), 2.0), 0.0, 1.0) * infos.g * infos.b
        * clamp(1.0 - pow(flow.b, 100.0), 0.0, 1.0);
    let c1 = 1.0 - fract(seconds * 0.07) * 0.95;
    let c2 = 1.0 - fract(seconds * 0.07 + 0.3) * 0.95;
    let c3 = 1.0 - fract(seconds * 0.07 + 0.6) * 0.95;
    let b2 = fract(seconds * 0.07 + 0.5 + beach_noise) * 0.81;
    let b1 = fract(seconds * 0.07 + beach_noise) * 0.81;
    let coast1 = clamp(pow(1.0 - abs(c1 - coast_dist), 150.0) * (1.0 - coast_dist) * 0.6, 0.0, 1.0) * n1;
    let coast2 = clamp(pow(1.0 - abs(c2 - coast_dist), 150.0) * (1.0 - coast_dist) * 0.65, 0.0, 1.0) * n2;
    let coast3 = clamp(pow(1.0 - abs(c3 - coast_dist), 150.0) * (1.0 - coast_dist) * 0.7, 0.0, 1.0) * n3;
    let beach1 = clamp(pow(1.0 - abs(b1 - beach_dist), 150.0) * beach_dist * 1.2, 0.0, 1.0);
    let beach2 = clamp(pow(1.0 - abs(b2 - beach_dist), 150.0) * beach_dist * 1.5, 0.0, 1.0);
    // TODO: the game spawns none where buildings stand (its blurred prop
    // visibility); the editor spawns them along every coast.
    wave.x += (coast1 + coast2 + coast3 + beach1 + beach2) * 0.01 * (1.0 + displacement.y * 3.0);
    wave.x = clamp(wave.x, -1.0, 1.0);
    wave.w = clamp(wave.w, 0.0, 1.0);
    wave *= border;
    textureStore(p_out0, phys_index(w), wave);
    textureStore(p_out1, phys_index(w), normal_choppy);
}
