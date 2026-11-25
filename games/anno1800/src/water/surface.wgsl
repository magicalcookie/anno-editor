// Anno 1800's sea as the island view draws it, after the kit's frame
// prelude (gpu::FRAME_WGSL): water_ocean.fx vsWaterFFT/psWaterFFT over a
// ring grid around the camera. Small waves from the shipped volume, large
// ones from the FFT (waves.wgsl), Perlin noise past the patch blend; shaded
// by depth color, refraction and absorption, caustics, the sky's
// reflection with the island's over it (the kit's screen-space reflections
// for the game's planar map), light through the crests, the island's
// painted foam and the sun's glints. The physical waves (physical.wgsl) lift the surface and
// foam where coasts set waves off and crests break near the ground. The
// island's painted maps (WaterInfos, WaterFlow) say how deep the water is,
// where beaches are and where foam lies; without them the coast map
// measured from the ground stands in. Left out: waves ships set off, rain
// ripples and the polluted-water tint. Group 1
// is the water's own; group 0 the frame's.
// A world unit is about 2.56 m: the FFT's 4096 cm patch spans 16 units, and
// 1800's residents stand 0.60-0.65 units tall (117's units are 4 m). The
// game's water shaders state their sizes in units, as this one does.

struct SeaParams {
    // WaterColorDeep; w: CausticsIntensity.
    deep: vec4<f32>,
    // WaterColorShallow; w: Absorption.
    shallow: vec4<f32>,
    // ViewDepth, LightDepth, cWaveTime, seconds.
    depths: vec4<f32>,
    // Coast map corner X and Z and 1 / its size; zw 0 without one.
    coast: vec4<f32>,
    // cUVScale, cUVOffset, cDisplacementScale, cTexelLengthX2.
    uv: vec4<f32>,
    // cPerlinAmplitude; w: cPerlinSize.
    perlin_amplitude: vec4<f32>,
    // cPerlinOctave; w unused.
    perlin_octave: vec4<f32>,
    // cPerlinGradient; w unused.
    perlin_gradient: vec4<f32>,
    // cPerlinMovement xy; PATCH_BLEND_BEGIN, PATCH_BLEND_END.
    perlin_move: vec4<f32>,
    // Ring grid: segments, rings, ring growth; w 1 leaves the near detail
    // out (the editor's reduced wave detail).
    grid: vec4<f32>,
    // cWaterInfoCoord (1 / the painted maps' extent); z 1 with them.
    painted: vec4<f32>,
    // Screen-space reflections (the kit's Ssr).
    ssr: vec4<f32>,
    // cPhysicalWaves: the map's world corner X and Z, 1 / its extent; w 1
    // once stepped.
    physical: vec4<f32>,
}

@group(1) @binding(0) var<uniform> sea: SeaParams;
@group(1) @binding(1) var s_large: texture_2d<f32>;
@group(1) @binding(2) var s_large_gradients: texture_2d<f32>;
@group(1) @binding(3) var s_small_gradients: texture_2d<f32>;
@group(1) @binding(4) var s_small_xz: texture_2d_array<f32>;
@group(1) @binding(5) var s_small_y: texture_2d_array<f32>;
@group(1) @binding(6) var s_perlin: texture_2d<f32>;
@group(1) @binding(7) var s_coast: texture_2d<f32>;
@group(1) @binding(8) var s_scene: texture_2d<u32>;
@group(1) @binding(9) var s_clamp: sampler;
@group(1) @binding(10) var s_wrap: sampler;
@group(1) @binding(14) var s_foam: texture_2d<f32>;
@group(1) @binding(15) var s_noise: texture_2d<f32>;
@group(1) @binding(16) var s_noise2: texture_2d<f32>;
@group(1) @binding(17) var s_infos: texture_2d<f32>;
@group(1) @binding(18) var s_flowmap: texture_2d<f32>;
@group(1) @binding(20) var s_noise3d: texture_3d<f32>;
@group(1) @binding(23) var s_phys0: texture_2d<f32>;
@group(1) @binding(24) var s_phys1: texture_2d<f32>;

const FRAMES: f32 = 48.0;

struct SeaOutput {
    @builtin(position) clip: vec4<f32>,
    @location(0) world: vec3<f32>,
    // Where the vertex sat before the waves moved it (PosWS2).
    @location(1) rest: vec3<f32>,
    // x: the patch blend; y: averaged WaterInfo depth; z: the ground under
    // the displaced vertex; w: 1 - shallowFac.
    @location(2) mixed: vec4<f32>,
    // x: the physical waves' height; yz: the FFT waves' sideways
    // displacement; w: shallowFac² (WaveDisplace).
    @location(3) wave: vec4<f32>,
}

// ComputePhysicalWaves: the physical waves' state (height, last height,
// FFT-wave foam, wave foam) and normals with choppy offset at world X/Z;
// still outside the map.
struct Physical {
    state: vec4<f32>,
    normals: vec4<f32>,
}

fn physical_at(xz: vec2<f32>) -> Physical {
    var out: Physical;
    let local = (xz - sea.physical.xy) * sea.physical.z;
    if sea.physical.w < 0.5 || any(local < vec2<f32>(0.0)) || any(local > vec2<f32>(1.0)) {
        out.state = vec4<f32>(0.0);
        out.normals = vec4<f32>(0.0);
        return out;
    }
    // Stored by world texel modulo the map's side, so wrapped sampling
    // finds every texel where it lies.
    let uv = xz * sea.physical.z;
    out.state = textureSampleLevel(s_phys0, s_wrap, uv, 0.0);
    out.normals = textureSampleLevel(s_phys1, s_wrap, uv, 0.0);
    return out;
}

// The camera's position; orthographic views stand far back along the view.
fn sea_eye(world: vec3<f32>) -> vec3<f32> {
    if globals.eye.w > 0.5 { return globals.eye.xyz; }
    return world + globals.eye.xyz * 1e4;
}

// WaterInfo at world X/Z from the coast map (anno_core::water::coast):
// x the signed distance to the coast, y depth (0 at the waterline, 1
// deep), w how much of a beach the shore is. Open deep sea without one.
fn coast_at(xz: vec2<f32>) -> vec4<f32> {
    if sea.coast.z <= 0.0 { return vec4<f32>(1000.0, 1.0, 0.0, 0.0); }
    let uv = (xz - sea.coast.xy) * sea.coast.zw;
    return textureSampleLevel(s_coast, s_clamp, uv, 0.0);
}

// GetWaterInfos: depth (0 at the waterline, 1 deep), coast waves, beach
// (lakes: duckweed) and painted foam. From the coast map without painted
// maps: its depth and beach, waves everywhere, no foam.
fn water_infos(xz: vec2<f32>) -> vec4<f32> {
    if sea.painted.z > 0.5 {
        // Past the island's maps: deep open sea, not their edge clamped out
        // to the horizon (land at the edge flattened the waves in strips).
        let uv = xz * sea.painted.xy;
        if any(uv < vec2<f32>(0.0)) || any(uv > vec2<f32>(1.0)) { return vec4<f32>(1.0, 1.0, 0.0, 0.0); }
        return textureSampleLevel(s_infos, s_clamp, uv, 0.0);
    }
    let c = coast_at(xz);
    return vec4<f32>(c.y, 1.0, c.w, 0.0);
}

// GetWaterFlow: flow direction X and Z biased by a half, and the distance
// from the coast (0 on it, 1 from 13 m out). From the coast map without
// painted maps: no flow, the distance over 20 units.
fn water_flow(xz: vec2<f32>) -> vec4<f32> {
    if sea.painted.z > 0.5 {
        // Still water at full coast distance past the maps, as an island
        // without WaterFlow reads (IslandMaps::read).
        let uv = xz * sea.painted.xy;
        if any(uv < vec2<f32>(0.0)) || any(uv > vec2<f32>(1.0)) { return vec4<f32>(0.5, 0.5, 1.0, 0.0); }
        return textureSampleLevel(s_flowmap, s_clamp, uv, 0.0);
    }
    return vec4<f32>(0.5, 0.5, clamp(coast_at(xz).x / 20.0, 0.0, 1.0), 0.0);
}

// GetWaterFakeLightingColor: foam's light, the sky's light standing in for
// cLgtAmbient.
fn foam_light(shade: f32, night: f32) -> vec3<f32> {
    let ambient = lights.sky.rgb * 0.2;
    let total = ambient + lights.sun_color.rgb;
    let intensity = dot(vec3<f32>(0.33), total);
    return mix(ambient * night, mix(vec3<f32>(intensity), total, 0.8), shade * night);
}

// cNoiseTex3D's red at `p`.
fn noise3d(p: vec3<f32>) -> f32 {
    return textureSampleLevel(s_noise3d, s_wrap, p, 0.0).r;
}

// The small waves' displacement at `uv` (GetFFTWaveDisplacement's 3D
// sample): the volume's 48 frames as layers, blended as its linear filter
// blends depth.
fn small_waves(uv: vec2<f32>) -> vec3<f32> {
    let z = fract(sea.depths.z) * FRAMES - 0.5;
    let first = floor(z);
    let t = z - first;
    let a = i32((first + FRAMES) % FRAMES);
    let b = (a + 1) % i32(FRAMES);
    let xz = mix(textureSampleLevel(s_small_xz, s_wrap, uv, a, 0.0).xy, textureSampleLevel(s_small_xz, s_wrap, uv, b, 0.0).xy, t);
    let y = mix(textureSampleLevel(s_small_y, s_wrap, uv, a, 0.0).x, textureSampleLevel(s_small_y, s_wrap, uv, b, 0.0).x, t);
    return vec3<f32>(xz.x, y, xz.y);
}

fn perlin_uv(uv: vec2<f32>, octave: f32) -> vec2<f32> {
    return uv * sea.perlin_amplitude.w * octave + sea.perlin_move.xy;
}

@vertex fn vs_sea(@builtin(vertex_index) vertex: u32) -> SeaOutput {
    // One triangle strip per band between two rings, around its segments
    // and back to the seam, the bands joined by repeating the last corner
    // of one and the first of the next: each corner is shaded once a band
    // instead of once for each of the six triangle corners it is (the
    // triangles are the same). Ring i's radius grows geometrically from a
    // first ring sized to the camera's height, so cells stay a few pixels
    // wide from near to far.
    let segments = u32(sea.grid.x);
    let per = 2u * segments + 4u;
    var band = vertex / per;
    var k = vertex % per;
    if k == per - 2u {
        k = per - 3u;
    } else if k == per - 1u {
        k = 0u;
        band += 1u;
    }
    let ring = f32(band + (k & 1u));
    let segment = f32(k >> 1u);
    let eye = select(globals.anchor.xyz, globals.eye.xyz, globals.eye.w > 0.5);
    let first = max(0.25, abs(eye.y) * 0.004);
    let q = sea.grid.z;
    let radius = first * (pow(q, ring) - 1.0) / (q - 1.0);
    let angle = segment / f32(segments) * 6.2831853;
    let centre = floor(eye.xz / (first * 4.0)) * (first * 4.0);
    let xz = centre + radius * vec2<f32>(cos(angle), sin(angle));
    var out: SeaOutput;
    out.rest = vec3<f32>(xz.x, 0.0, xz.y);
    // The depth averaged over five taps 25 units apart, for smoother
    // changes between the small and large wave sets.
    let taps = array<vec2<f32>, 5>(vec2<f32>(0.0), vec2<f32>(0.0, 25.0), vec2<f32>(0.0, -25.0),
        vec2<f32>(25.0, 0.0), vec2<f32>(-25.0, 0.0));
    var depth = 0.0;
    for (var i = 0; i < 5; i++) { depth += water_infos(xz + taps[i]).r; }
    depth *= 0.2;
    let infos = water_infos(xz);
    let uv = xz * sea.uv.x + sea.uv.y;
    // Past the patch blend the FFT's tiling gives way to Perlin noise.
    let dist2d = distance(xz, sea_eye(out.rest).xz);
    let begin = sea.perlin_move.z;
    let end = sea.perlin_move.w;
    let blend = pow(clamp((end - dist2d) / (end - begin), 0.0, 1.0), 4.0);
    let perlin = (textureSampleLevel(s_perlin, s_wrap, perlin_uv(uv, sea.perlin_octave.x), 0.0).w * sea.perlin_amplitude.x
        + textureSampleLevel(s_perlin, s_wrap, perlin_uv(uv, sea.perlin_octave.y), 0.0).w * sea.perlin_amplitude.y
        + textureSampleLevel(s_perlin, s_wrap, perlin_uv(uv, sea.perlin_octave.z), 0.0).w * sea.perlin_amplitude.z)
        * sea.uv.z;
    // GetFFTWaveDisplacement: the small set always keeps 30%; the large
    // one grows with depth.
    let large = textureSampleLevel(s_large, s_wrap, uv, 0.0).xyz;
    var disp = (small_waves(uv) * 0.8 * (1.0 - depth * 0.7) + large * depth) * sea.uv.z;
    disp = mix(vec3<f32>(0.0, perlin, 0.0), disp, blend);
    // No ocean waves on shallow beaches.
    var ground = -1000.0;
    if lights.terrain.w > 0.5 { ground = ground_height(xz + disp.xz); }
    let shallow = mix(1.0, clamp(0.25 + (0.0 - ground) * 0.5, 0.0, 1.0), infos.b);
    out.world = out.rest + disp * shallow * shallow;
    // The physical waves on top, without the shallow fade.
    let physical = physical_at(xz);
    let lift = pow(abs(physical.state.x), 0.8) * 0.35;
    out.world += vec3<f32>(physical.normals.z * 0.5, lift, physical.normals.w * 0.5);
    out.wave = vec4<f32>(lift, disp.x, disp.z, shallow * shallow);
    out.mixed = vec4<f32>(blend, depth, ground, clamp(1.0 - shallow, 0.0, 1.0));
    out.clip = globals.clip * vec4<f32>(out.world, 1.0);
    return out;
}

fn sea_scene(p: vec2<f32>) -> vec3<f32> {
    let size = vec2<i32>(textureDimensions(s_scene));
    return unpack_rgb9e5(textureLoad(s_scene, clamp(vec2<i32>(p), vec2<i32>(0), size - 1), 0).r);
}

fn sea_depth(p: vec2<f32>) -> f32 {
    let size = vec2<i32>(textureDimensions(t_depth));
    return textureLoad(t_depth, clamp(vec2<i32>(p), vec2<i32>(0), size - 1), 0);
}

fn sea_ndc(p: vec2<f32>) -> vec2<f32> {
    return vec2<f32>(p.x / globals.viewport.x * 2.0 - 1.0, 1.0 - p.y / globals.viewport.y * 2.0);
}

// The sky a water surface reflects along `r`, folded above the horizon.
fn sea_sky(r0: vec3<f32>) -> vec3<f32> {
    var r = r0;
    r.y = abs(r.y);
    if lights.disc.w > 0.5 { return sky_map(r); }
    return sky_radiance(r);
}

// How far psScreenSpaceReflect's ray reaches from `world`, at most the
// kit's reach (sea.ssr.y). Its 40 steps start at 8 times the pixel's
// footprint (fwidth of its view position, summed over the axes), each
// 1.03 times the last and taken i / 40 of: 45.9 starting steps in all.
// So the ray is a fixed number of pixels long, reaching far over a wide
// view and little up close. The footprint is the pixel's on the flat
// water, its two screen steps summed over world axes rather than view
// ones, which the waves do not change.
fn ssr_reach(world: vec3<f32>) -> f32 {
    let c = globals.clip * vec4<f32>(world, 1.0);
    if c.w <= 1e-6 { return sea.ssr.y; }
    let ndc = c.xy / c.w;
    let pixel = 2.0 / globals.viewport.xy;
    let x = ssr_on_water(ndc + vec2<f32>(pixel.x, 0.0), world.y) - world;
    let y = ssr_on_water(ndc + vec2<f32>(0.0, pixel.y), world.y) - world;
    let footprint = dot(abs(x) + abs(y), vec3<f32>(1.0));
    return clamp(8.0 * 45.9 * footprint, 1.0, sea.ssr.y);
}

// Where the view ray through `ndc` meets the water at height `y`.
fn ssr_on_water(ndc: vec2<f32>, y: f32) -> vec3<f32> {
    let near = unproject(ndc, 1.0);
    let ray = unproject(ndc, 0.5) - near;
    let t = (y - near.y) / select(ray.y, -1e-6, abs(ray.y) < 1e-6);
    return near + ray * t;
}

// The island reflected in the water, rgb and how far to trust it in alpha.
// The game traces the flat mirror's ray (its SSR pass ignores the waves)
// and reads the result `shift` pixels away (sized for a 1080-line screen),
// which the waves make wobble; so does the kit's trace here. Its 0.9 gloss
// keeps 99% (pow(gloss, 1 / 16)). Call in uniform control flow: with quad
// operations each 2x2 quad traces its rays together (gpu::ssr_module).
fn water_ssr(world: vec3<f32>, to_eye: vec3<f32>, shift: vec2<f32>) -> vec4<f32> {
    let ssr = Ssr(vec4<f32>(sea.ssr.x, ssr_reach(world), sea.ssr.zw));
    let dir = reflect(-to_eye, vec3<f32>(0.0, 1.0, 0.0));
    let hit = ssr_trace(s_scene, world, dir, ssr, shift * globals.viewport.y / 1080.0);
    return vec4<f32>(hit.rgb, hit.a * 0.993);
}

// Fake clouds the water reflects, so waves show without a glint: a noise
// texture at `uv`, more contrast around `centre`, faded toward grazing
// views, darkened at night (DarkenCloudsAtNight, cDayNight.y estimated as
// 1 - day) and lit from the sun's side (ApplyCloudIllumination's front
// light for an opaque cloud), mixed into `sky` by up to `weight`.
fn fake_clouds(sky: vec3<f32>, uv: vec2<f32>, centre: f32, grey: f32, weight: f32, to_eye: vec3<f32>,
    reflected: vec3<f32>, day: f32) -> vec3<f32> {
    let mask = clamp(mix(centre, textureSample(s_noise2, s_wrap, uv).g, 3.0), 0.0, 1.0) * to_eye.y;
    var cloud = vec3<f32>(grey) * pow(abs(1.0 - (1.0 - day) * 0.8), 2.5);
    let front = pow((-dot(reflected, sea_sun()) + 1.0) * 0.5, 0.3) * 0.5 * pow(day, 4.0);
    let tint = normalize(lights.sun_color.rgb + vec3<f32>(1e-6)) * 10.0;
    cloud = mix(cloud, cloud * tint, clamp(front, 0.0, 1.0));
    return mix(sky, cloud, mask * weight);
}

// Toward the sun (GetWaterSunDirectionWS).
fn sea_sun() -> vec3<f32> {
    return normalize(lights.sun.xyz);
}

// cDayNight.x and .z, which the game ramps over dawn and dusk: an estimate
// from the sun's height.
fn sea_day() -> f32 {
    return smoothstep(-0.05, 0.15, sea_sun().y);
}

// CalculateWaterCausticIntensity: rays from a fixed low light refracted by
// the small waves' normals over nine nearby seabed points, brighter where
// they bunch up. `scale` tiles the waves finer: the sea's 1, lakes' 4.
fn sea_caustics(floor_ws: vec3<f32>, scale: f32) -> f32 {
    let to_light = normalize(vec3<f32>(1.0, 0.07, 0.0));
    let water_depth = (0.5 - floor_ws.y) * 0.5;
    var p = floor_ws;
    p = vec3<f32>(p.x - p.y * to_light.x, p.y, p.z - p.y * to_light.z);
    let to_surface = to_light * water_depth;
    let m = 0.3;
    let k = 0.25;
    var cc = 0.0;
    for (var i = -1; i <= 1; i++) {
        for (var j = -1; j <= 1; j++) {
            let at = p.xz * scale * sea.uv.z * 6.0 + vec2<f32>(f32(i), f32(j)) * k * m;
            let g = textureSampleLevel(s_small_gradients, s_wrap, at, 1.0).xy;
            let n = normalize(vec3<f32>(g.x, sea.uv.w, g.y));
            let refracted = m * (to_surface + vec3<f32>(f32(i) * k, 0.0, f32(j) * k)) - 0.25 * vec3<f32>(n.x, 0.0, n.z);
            cc += 0.12 * pow(max(0.0, dot(normalize(refracted), to_light)), 600.0);
        }
    }
    let camera = distance(sea_eye(floor_ws), floor_ws);
    cc *= 50.0 / (50.0 + min(4950.0, camera));
    cc *= clamp(-floor_ws.y, 0.0, 1.0);
    return cc;
}

@fragment fn fs_sea(in: SeaOutput) -> @location(0) vec4<f32> {
    let world = in.world;
    let eye = sea_eye(world);
    let to_eye = normalize(eye - world);
    let distance3 = distance(world, eye);
    let blend = in.mixed.x;
    let depth = in.mixed.y;
    let infos = water_infos(in.rest.xz);
    let flow_data = water_flow(in.rest.xz);
    let day = sea_day();
    let night = clamp(day * day, 0.2, 1.0);
    let uv = in.rest.xz * sea.uv.x + sea.uv.y;
    // Normals: the two sets' gradients by depth, faded into Perlin's far
    // off, scaled by how deep the water is, plus near detail six times
    // finer.
    let small = textureSample(s_small_gradients, s_wrap, uv);
    let large = textureSample(s_large_gradients, s_wrap, uv);
    let gradient = mix(small, large, depth);
    let perlin = textureSample(s_perlin, s_wrap, perlin_uv(uv, sea.perlin_octave.x)).xy * sea.perlin_gradient.x
        + textureSample(s_perlin, s_wrap, perlin_uv(uv, sea.perlin_octave.y)).xy * sea.perlin_gradient.y
        + textureSample(s_perlin, s_wrap, perlin_uv(uv, sea.perlin_octave.z)).xy * sea.perlin_gradient.z;
    var g = mix(perlin, gradient.xy, blend);
    g *= 0.5 + pow(abs(infos.r), 0.1) * 0.5;
    // Rougher where the physical waves churn the open sea, not on coasts.
    let seconds = sea.depths.w;
    let lift = in.wave.x;
    let anim = clamp(pow(noise3d(vec3<f32>(in.rest.xz * 0.2, seconds * 0.06)), 3.0) * 3.0, 0.0, 1.0);
    let turbulence = clamp(pow(clamp((lift - 0.02) / 0.38, 0.0, 1.0), 0.5) * 2.5, 0.0, 1.0);
    let roughness = (0.5 + anim * 0.5) * turbulence * flow_data.b * flow_data.b;
    var n = normalize(vec3<f32>(g.x, sea.uv.w * (1.0 + roughness), g.y));
    let detail = smoothstep(0.1, 1.0, pow(1.0 - clamp(distance3 / 96.0, 0.0, 1.0), 2.5));
    let detail_uv = uv * 6.0;
    // Branching on the uniform keeps the samples in uniform control flow.
    var g2 = vec2<f32>(0.0);
    if sea.grid.w < 0.5 {
        g2 = mix(textureSample(s_small_gradients, s_wrap, detail_uv).xy, textureSample(s_large_gradients, s_wrap, detail_uv).xy, depth);
    }
    if detail > 0.0 && sea.grid.w < 0.5 {
        n = normalize(n + normalize(vec3<f32>(g2.x, sea.uv.w, g2.y)) * detail);
    }
    let physical = physical_at(in.rest.xz);
    {
        let p = physical.normals.xy;
        n = normalize(n * 2.0 + vec3<f32>(p.x, sqrt(max(0.0, 1.0 - dot(p, p))), p.y));
    }
    let reflected = reflect(-to_eye, n);
    // Fresnel, f0 0.2, power 3.5 at full water quality.
    let fresnel = mix(0.2, 1.0, pow(1.0 - clamp(dot(n, to_eye), 0.0, 1.0), 3.5));
    let sun_shadow = clamp(shadow(world, n), 0.0, 1.0);
    var reflection = sea_sky(reflected);
    {
        let uv = in.rest.xz * 0.001 - (to_eye.xz / max(to_eye.y, 0.05)) * 0.02 + reflected.xz * 0.6;
        reflection = fake_clouds(reflection, uv, 0.8, 0.75, 0.25, to_eye, reflected, day);
    }
    var offset = (1.0 - clamp(distance3 / 256.0, 0.0, 1.0)) * 0.2;
    offset += pow(clamp(-dot(reflected, to_eye), 0.0, 1.0), 8.0) * 0.5;
    // The island: the game blends its planar reflection map
    // (ENABLE_REFLECTION_MAP) over the sky, read offset by n.xz * offset of
    // the screen; screen-space reflections stand in for it. Never fully
    // (0.9), less on rough water (pow(n.y, 1 + roughness * 32), the game's
    // roughness 1 + noise * 1.25) and by up to 40% toward the horizon.
    {
        let ssr = water_ssr(world, to_eye, n.xz * offset * 1080.0);
        let planar = pow(abs(n.y), 1.0 + (1.0 + roughness * 1.25) * 32.0);
        let horizon = 1.0 - pow(1.0 - abs(to_eye.y), 4.0);
        let alpha = min(0.9, ssr.a * planar) * (horizon * 0.4 + 0.6);
        reflection = mix(reflection, ssr.rgb, alpha);
    }
    // Fast, fake scattering: the deep color, darker at night and in shade.
    var color = sea.deep.rgb * (0.5 + night * 0.5) * (0.55 + sun_shadow * 0.45);
    color.g *= 1.25 - 0.25 * (1.0 + roughness * 1.25);
    let view_range = sea.depths.x * (1.0 - 0.85 * infos.r);
    let light_range = sea.depths.y * (1.0 - 0.55 * infos.r);
    // The scene behind: refracted toward the shore less, back to straight
    // through where the refracted point stands in front of the water.
    let pixel = in.clip.xy;
    let scene_ws = unproject(sea_ndc(pixel), max(sea_depth(pixel), 1e-7));
    var refracted = pixel + n.xz * offset * 0.4 * clamp(world.y - scene_ws.y, 0.0, 1.0) * globals.viewport.xy;
    var refracted_depth = sea_depth(refracted);
    if refracted_depth > in.clip.z {
        refracted = pixel;
        refracted_depth = sea_depth(pixel);
    }
    let refracted_ws = unproject(sea_ndc(pixel), max(refracted_depth, 1e-7));
    var refraction = max(sea_scene(refracted), vec3<f32>(0.0));
    let view_depth = clamp((distance(refracted_ws, eye) - distance3) / view_range, 0.0, 1.0);
    // How far the light travels through the water to the seabed.
    let light_y = max(sea_sun().y, 0.05);
    let light_depth = clamp(((world.y - refracted_ws.y) / light_y) / light_range, 0.0, 1.0);
    let fade = pow(view_depth, 0.2);
    if fade < 0.99 {
        let caustics = dot(refraction, vec3<f32>(1.0)) * lights.sun_color.rgb * sea_caustics(refracted_ws, 1.0)
            * (0.3 + 0.7 * sun_shadow) * sea.deep.w;
        let power = max(vec3<f32>(0.01), vec3<f32>(2.0) - sea.shallow.rgb * 2.0) * sea.shallow.w;
        let absorption = pow(vec3<f32>(1.0 - light_depth), power);
        refraction = (refraction + caustics) * absorption;
        color = mix(refraction, color, fade);
    }
    // Less reflection in shade.
    color = mix(color, reflection, fresnel * (0.6 + sun_shadow * 0.4));
    // Light refracted twice through the crests that face away from the
    // sun, and gathered under the crests.
    {
        let sun = sea_sun();
        let toward = normalize(vec3<f32>(sun.x, 0.16, sun.z));
        var scatter = 1.5 * clamp(world.y * 0.25 + 0.25, 0.0, 1.0);
        scatter *= sun_shadow * pow(max(0.0, dot(normalize(vec3<f32>(toward.x, 0.0, toward.z)), -to_eye)), 2.0);
        scatter *= pow(max(0.0, 1.0 - dot(toward, n)), 8.0);
        scatter += sun_shadow * 0.5 * clamp(gradient.a + max(0.0, world.y * 0.25 + 0.2), 0.0, 1.0)
            * max(0.0, dot(to_eye, n)) * pow(max(0.0, 1.0 - to_eye.y), 1.5) * (100.0 / (100.0 + distance3));
        scatter *= 0.1 + 0.9 * flow_data.b;
        scatter *= day * 0.95 + 0.05;
        scatter *= 0.5;
        color += vec3<f32>(0.1, 0.8, 0.6) * scatter;
    }
    // Foam: the physical waves' (crests breaking near the ground, waves
    // the coast set off) and the painted, drifting with the flow map near
    // the coast and more on beaches.
    var foam = 0.0;
    let wave_foam = clamp(physical.state.z, 0.0, 1.0);
    let physical_foam = clamp(physical.state.w, 0.0, 1.0);
    if infos.a + wave_foam + pow(physical_foam, 1.5) > 0.01 {
        let intensity = sin(seconds * 0.5) * pow(1.0 - clamp(flow_data.b, 0.0, 1.0), 3.0) * (2.0 + infos.b);
        var flow_dir = flow_data.xy * 2.0 - 1.0;
        let flow_length = length(flow_dir);
        flow_dir = select(vec2<f32>(0.0), flow_dir / flow_length * min(0.03, flow_length) * intensity, flow_length > 1e-4);
        let foam_uv = in.rest.xz * 0.18;
        let fake = mix(in.wave.yz, vec2<f32>(0.0), in.wave.w);
        let dx = dpdx(foam_uv) * 0.6;
        let dy = dpdy(foam_uv) * 0.6;
        let fine = textureSampleGrad(s_foam, s_wrap, foam_uv + flow_dir + fake * 0.0007, dx, dy);
        let coarse = textureSampleGrad(s_foam, s_wrap, foam_uv * 0.7 + flow_dir + fake * 0.0005, dx, dy) * 0.8;
        let light = foam_light(sun_shadow, night);
        {
            // Coarser on shallow coasts and high waves; streakier as the
            // waves subside.
            var foamses = mix(fine, coarse, clamp(in.mixed.w + pow(lift, 4.0), 0.0, 1.0));
            foamses.r = mix(foamses.b * foamses.b, foamses.r, clamp(pow(lift, 6.0), 0.0, 1.0));
            let crest = wave_foam * foamses.b + pow(wave_foam, 2.0) * foamses.g + pow(wave_foam, 4.0) * foamses.r * 3.0;
            let breaking = pow(physical_foam, 1.5) * foamses.b + pow(physical_foam, 3.0) * foamses.g
                + pow(physical_foam, 6.0) * foamses.r * 2.0;
            // Air bubbles under the foam.
            let bubbles = foamses.a * anim * (wave_foam + pow(physical_foam, 2.0));
            color = mix(color, vec3<f32>(0.3, 0.7, 0.6) * light, clamp(bubbles, 0.0, 1.0));
            foam += clamp(crest + breaking, 0.0, 1.0);
            color = mix(color, light, clamp(foam, 0.0, 1.0));
        }
        let anim2 = clamp(pow(noise3d(vec3<f32>(in.rest.xz * 0.1, seconds * 0.03)), 3.0) * 3.0, 0.0, 1.0);
        let painted = pow(mix(fine.b, mix(fine.g, fine.r, anim), infos.a * infos.a) * infos.a * 2.5 * anim2, 2.0);
        color = mix(color, light, clamp(painted, 0.0, 1.0));
        foam += painted;
    }
    // The sun's glints, soft and hard, none on foam.
    let spec = clamp(dot(reflected, sea_sun()), 0.0, 1.0);
    let glint = vec3<f32>(0.85, 0.95, 1.0) * lights.sun_color.rgb * pow(sun_shadow, 4.0)
        * (1.0 - pow(clamp(foam * 2.0, 0.0, 1.0), 0.4));
    color += glint * pow(spec, 350.0) * 0.1 + glint * pow(spec, 4000.0) * 4.0;
    // Where the seabed comes up to the surface the water thins to clear.
    var clear = clamp((world.y - scene_ws.y) * (2.0 + min(30.0, infos.r * infos.r * 20.0 + (1.0 - infos.b) * 20.0)), 0.0, 1.0);
    clear = clamp(clear + step(1.0, scene_ws.y), 0.0, 1.0);
    color = mix(sea_scene(pixel), color, clear);
    return vec4<f32>(fog(color, world), 1.0);
}
