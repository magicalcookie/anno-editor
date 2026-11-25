// Anno 117's sea, rivers and lakes as the island view draws them, after the
// kit's frame prelude (gpu::FRAME_WGSL): the ocean surface over the FFT
// bands (ocean.wgsl), the shore's physical waves, and the inland water
// lattice. Group 1 is the water's own; group 0 the frame's.
// --- Ocean ------------------------------------------------------------------
// Anno 117's open sea (water_ocean.fx vsOcean/psOcean, functions_water.fxh):
// a ring grid around the camera displaced by the FFT bands (gpu::ocean),
// shaded opaque over the copied opaque scene, with the shore's physical
// waves, the island's painted foam and screen-space reflections of the
// island (the kit's SSR_WGSL). Churn is left out. World units are 4 m.

struct OceanParams {
    // AbsorptionShallow times the shallow color's absorption; w: ShallowWaterDepth.
    absorb_shallow: vec4<f32>,
    // ln(0.01) / -ViewDepth times the deep color's absorption; w: AbsorptionSmoothness.
    absorb_deep: vec4<f32>,
    // Scattering, ScatterStrength times ScatterColor at full brightness; w: ViewDepth.
    scatter: vec4<f32>,
    // WaterColorShallow; w: CausticsIntensity.
    shallow: vec4<f32>,
    // OffshoreDistance, OffshoreTransitionDist, wave seconds, AmbientLightFactorOcean.
    offshore: vec4<f32>,
    // Coast map corner X and Z and 1 / its size; zw 0 without one.
    coast: vec4<f32>,
    // Each band's patch side in world units.
    patches: vec4<f32>,
    // Ring grid: segments, rings, ring growth; w: 1 with a foam map.
    grid: vec4<f32>,
    // Physical waves (cPhysicalWaves): the square's corner X and Z, 1 / its
    // side, 1 / its texels a side; z 0 before the first step.
    physical: vec4<f32>,
    // x: world units of water the coast map's depth counts as fully deep.
    // y: 1 leaves the finest band out (the editor's reduced wave detail).
    shore: vec4<f32>,
    // The island's WaterInfos and WaterFlow maps: 1 / their extent in X and
    // Z (their corner is the island's), z 1 with them.
    info: vec4<f32>,
    // Screen-space reflections (the kit's Ssr).
    ssr: vec4<f32>,
}

@group(1) @binding(0) var<uniform> ocean: OceanParams;
// Per slice displacement and fold; per band the coastal slopes in xy and
// the offshore in zw (ocean.wgsl).
@group(1) @binding(1) var o_waves: texture_2d_array<f32>;
@group(1) @binding(2) var o_slopes: texture_2d_array<f32>;
@group(1) @binding(3) var o_scene: texture_2d<u32>;
@group(1) @binding(4) var o_coast: texture_2d<f32>;
@group(1) @binding(5) var o_foam: texture_2d<f32>;
@group(1) @binding(6) var o_clamp: sampler;
// Repeating and filtered, for the waves in the vertex stage.
@group(1) @binding(7) var o_wrap: sampler;
// The island's WaterInfos (depth, coast waves, beach, painted foam) and
// WaterFlow (flow X and Z, coast distance) as stored in its rd3d.
@group(1) @binding(17) var o_info: texture_2d<f32>;
@group(1) @binding(18) var o_flow: texture_2d<f32>;

struct OceanOutput {
    @builtin(position) clip: vec4<f32>,
    @location(0) world: vec3<f32>,
    // Where the vertex sat before the waves moved it (PositionWS2).
    @location(1) rest: vec3<f32>,
    // WaterInfo depth at the vertex, averaged.
    @location(2) depth: f32,
    // How far the physical waves raise the surface here.
    @location(3) lift: f32,
}

// The camera's position; orthographic views stand far back along the view.
fn ocean_eye(world: vec3<f32>) -> vec3<f32> {
    if globals.eye.w > 0.5 { return globals.eye.xyz; }
    return world + globals.eye.xyz * 1e4;
}

// cTerrainSDF and WaterInfo (depth, waves, beach) at world X/Z. Without a
// coast map the sea is deep and open everywhere.
fn coast_at(xz: vec2<f32>) -> vec4<f32> {
    if ocean.coast.z <= 0.0 { return vec4<f32>(1000.0, 1.0, 0.0, 0.0); }
    let uv = (xz - ocean.coast.xy) * ocean.coast.zw;
    return textureSampleLevel(o_coast, o_clamp, uv, 0.0);
}

// GetWaterInfos: the water's depth (0 at the waterline, 1 five units or
// more down), how much coast waves roll in (1 unless painted out), beach
// (1) rather than cliff, and painted foam (on lakes the game reads beach as
// duckweed). Without the island's maps: the coast map's depth and beach,
// waves everywhere, no foam.
fn water_info(xz: vec2<f32>) -> vec4<f32> {
    if ocean.info.z > 0.5 {
        // Past the island's maps: deep open sea, not their edge clamped out
        // to the horizon (land at the edge flattened the waves in strips).
        let uv = xz * ocean.info.xy;
        if any(uv < vec2<f32>(0.0)) || any(uv > vec2<f32>(1.0)) { return vec4<f32>(1.0, 1.0, 0.0, 0.0); }
        return textureSampleLevel(o_info, o_clamp, uv, 0.0);
    }
    let c = coast_at(xz);
    return vec4<f32>(c.y, 1.0, c.w, 0.0);
}

// GetWaterFlow's distance from the coast: 0 at the waterline, 1 from about
// 13 units out. Without the maps, the bake's own rule (6 per squared 2-unit
// cell, over 255) on the coast map's distance.
fn coast_ramp(xz: vec2<f32>) -> f32 {
    if ocean.info.z > 0.5 {
        let uv = xz * ocean.info.xy;
        if any(uv < vec2<f32>(0.0)) || any(uv > vec2<f32>(1.0)) { return 1.0; }
        return textureSampleLevel(o_flow, o_clamp, uv, 0.0).z;
    }
    let cells = max(coast_at(xz).x, 0.0) * 0.5;
    return clamp(6.0 * cells * cells / 255.0, 0.0, 1.0);
}

// ComputeFFTBlendInfo: how much of each band, coastal (first four) and
// offshore (last four), shows at a point `camera` from the camera, by the
// water's normalised depth and its distance from the coast.
fn band_factors(coast: f32, camera: f32, depth: f32) -> array<f32, 8> {
    var f = array<f32, 8>(0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0);
    if coast <= -5.0 { return f; }
    let near = array<f32, 4>(
        mix(0.12, 1.0, smoothstep(0.0, 0.8, depth)),
        mix(0.40, 1.0, smoothstep(0.0, 0.4, depth)) * clamp(1.0 - camera / 3750.0, 0.0, 1.0),
        clamp(1.0 - camera / 1450.0, 0.0, 1.0),
        clamp(1.0 - camera / 200.0, 0.0, 1.0) * (1.0 - ocean.shore.y),
    );
    let fade = clamp((coast - ocean.offshore.x) / max(ocean.offshore.y, 1e-3), 0.0, 1.0);
    for (var b = 0; b < 4; b++) {
        f[b] = near[b] * (1.0 - fade);
        f[b + 4] = near[b] * fade;
    }
    return f;
}

fn band_uv(xz: vec2<f32>, band: i32) -> vec2<f32> {
    return xz / ocean.patches[band];
}

@vertex fn vs_ocean(@builtin(vertex_index) vertex: u32) -> OceanOutput {
    // One triangle strip per band between two rings, around its segments
    // and back to the seam, the bands joined by repeating the last corner
    // of one and the first of the next: each corner is shaded once a band
    // instead of once for each of the six triangle corners it is (the
    // triangles are the same). Ring i's radius grows geometrically from a
    // first ring sized to the camera's height, so cells stay a few pixels
    // wide from near to far.
    let segments = u32(ocean.grid.x);
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
    let q = ocean.grid.z;
    let radius = first * (pow(q, ring) - 1.0) / (q - 1.0);
    let angle = segment / f32(segments) * 6.2831853;
    // Snapped to the first ring's cells so the grid does not swim as the
    // camera moves.
    let centre = floor(eye.xz / (first * 4.0)) * (first * 4.0);
    let xz = centre + radius * vec2<f32>(cos(angle), sin(angle));
    var out: OceanOutput;
    out.rest = vec3<f32>(xz.x, 0.0, xz.y);
    // ComputeWaterDepth: five taps 25 units apart.
    var depth = 0.0;
    let taps = array<vec2<f32>, 5>(vec2<f32>(0.0), vec2<f32>(25.0, 0.0), vec2<f32>(-25.0, 0.0),
        vec2<f32>(0.0, 25.0), vec2<f32>(0.0, -25.0));
    for (var i = 0; i < 5; i++) { depth += water_info(xz + taps[i]).x; }
    depth *= 0.2;
    out.depth = depth;
    let coast = coast_at(xz);
    let f = band_factors(coast.x, distance(xz, ocean_eye(out.rest).xz), depth);
    var disp = vec3<f32>(0.0);
    for (var s = 0; s < 8; s++) {
        if f[s] > 0.0 {
            disp += f[s] * textureSampleLevel(o_waves, o_wrap, band_uv(xz, s % 4), s, 0.0).xyz;
        }
    }
    disp *= 0.25;
    // Beaches flatten the waves where they would reach the ground.
    var ground = -1000.0;
    if lights.terrain.w > 0.5 { ground = ground_height(xz + disp.xz); }
    let shallow = mix(1.0, clamp(0.25 + (0.0 - ground) * 0.5, 0.0, 1.0), water_info(xz).z);
    out.world = out.rest + disp * shallow * shallow;
    // The physical waves lift the surface (never lower it) and lean it.
    let p = physical_uv(out.rest.xz);
    let raise = pow(abs(physical_sample(p).x), 0.8) * 0.35;
    let lean = physical_choppy(p) * 0.5;
    out.world += vec3<f32>(lean.x, raise, lean.y);
    out.lift = raise;
    out.clip = globals.clip * vec4<f32>(out.world, 1.0);
    return out;
}

// ComputePhysicalWavesTexCoords.
fn physical_uv(xz: vec2<f32>) -> vec2<f32> {
    return (xz - ocean.physical.xy) * ocean.physical.z;
}

// The physical waves at `uv`, zero outside their square (the game's
// border sampler) and before the first step.
fn physical_sample(uv: vec2<f32>) -> vec4<f32> {
    if ocean.physical.z <= 0.0 || any(uv < vec2<f32>(0.0)) || any(uv > vec2<f32>(1.0)) { return vec4<f32>(0.0); }
    return textureSampleLevel(o_physical, o_clamp, uv, 0.0);
}

// SamplePhysicalWavesChoppy: last step's heights' slope toward +X and +Z.
fn physical_choppy(uv: vec2<f32>) -> vec2<f32> {
    let t = ocean.physical.w;
    let c = physical_sample(uv).y;
    return (vec2<f32>(physical_sample(uv + vec2<f32>(t, 0.0)).y, physical_sample(uv + vec2<f32>(0.0, t)).y) - c) * 0.25;
}

// SamplePhysicalWavesNormal: a Sobel of last step's heights, as a normal.
fn physical_normal(uv: vec2<f32>) -> vec3<f32> {
    let t = ocean.physical.w;
    let s = physical_sample(uv + vec2<f32>(0.0, -t)).y;
    let w = physical_sample(uv + vec2<f32>(-t, 0.0)).y;
    let e = physical_sample(uv + vec2<f32>(t, 0.0)).y;
    let n = physical_sample(uv + vec2<f32>(0.0, t)).y;
    let g = vec2<f32>(s + w - e - n, s - w + e - n);
    return vec3<f32>(g.x, sqrt(clamp(1.0 - dot(g, g), 0.0, 1.0)), g.y);
}

// The foam maps' mip bias, -0.6, as a scale on their footprint.
const FOAM_SHARPEN: f32 = 0.65975396; // exp2(-0.6)

// How far inland, in world units past the coast, the sea is cut out. Shore
// waves run a few units up the beach; marsh lakes lie farther in.
const INLAND: f32 = -4.0;

// D_GGX (functions_pbr_lighting.fxh).
fn ocean_ggx(ndh: f32, roughness: f32) -> f32 {
    let a = max(0.01, roughness);
    let a2 = a * a;
    let d = a2 / (ndh * ndh * (a2 * a2 - 1.0) + 1.0);
    return d * d / 3.14159265;
}

// MeanPathAbsorption: the mean absorption along `length` units of water
// travelled in direction `dir`, shallow absorption near the surface
// blending into deep past ShallowWaterDepth.
fn mean_absorption(dir: vec3<f32>, length: f32) -> vec3<f32> {
    return path_absorption(dir, length, ocean.absorb_shallow, ocean.absorb_deep);
}

// MeanPathAbsorption for any water: `shallow` and `deep` are its
// coefficients, their w ShallowWaterDepth and AbsorptionSmoothness.
fn path_absorption(dir: vec3<f32>, length: f32, shallow4: vec4<f32>, deep4: vec4<f32>) -> vec3<f32> {
    let shallow = shallow4.rgb;
    let deep = deep4.rgb;
    let k = select(1.0, 1.0 / abs(dir.y), dir.y != 0.0);
    let start = shallow4.w * k;
    let end = (shallow4.w + deep4.w) * k;
    var sum = min(length, start) * shallow + max(0.0, length - end) * deep;
    let t = clamp(length / end, 0.0, 1.0);
    let b = -0.5 * t * t;
    sum += ((t + b) * shallow - b * deep) * (end - start);
    return select(vec3<f32>(0.0), sum / length, length != 0.0);
}

fn ocean_hash(p: vec3<f32>) -> f32 {
    let q = fract(p * 0.1031);
    let r = q + dot(q, q.yzx + 33.33);
    return fract((r.x + r.y) * r.z);
}

// Value noise in place of the game's noise_3d.dds, 0 to 1.
fn ocean_noise(p: vec3<f32>) -> f32 {
    let i = floor(p);
    let f = p - i;
    let u = f * f * (3.0 - 2.0 * f);
    let n = mix(
        mix(mix(ocean_hash(i), ocean_hash(i + vec3<f32>(1.0, 0.0, 0.0)), u.x),
            mix(ocean_hash(i + vec3<f32>(0.0, 1.0, 0.0)), ocean_hash(i + vec3<f32>(1.0, 1.0, 0.0)), u.x), u.y),
        mix(mix(ocean_hash(i + vec3<f32>(0.0, 0.0, 1.0)), ocean_hash(i + vec3<f32>(1.0, 0.0, 1.0)), u.x),
            mix(ocean_hash(i + vec3<f32>(0.0, 1.0, 1.0)), ocean_hash(i + vec3<f32>(1.0, 1.0, 1.0)), u.x), u.y),
        u.z);
    return n;
}

// The sun's direction for water (GetWaterSunDirectionWS).
fn ocean_sun() -> vec3<f32> {
    return normalize(lights.sun.xyz);
}

// CalculateWaterCausticIntensity: how much the band-3 ripples focus the
// sun on the seabed at `floor`, from how far nine neighbouring rays land
// from each other after refraction.
fn ocean_caustic(floor_ws: vec3<f32>) -> f32 {
    let depth = max(0.0, -floor_ws.y);
    let toward = -ocean_sun();
    let land = caustic_hit(floor_ws, depth, toward);
    var sum = 0.0;
    for (var i = -1; i <= 1; i++) {
        for (var j = -1; j <= 1; j++) {
            let p = floor_ws + vec3<f32>(f32(i), 0.0, f32(j)) * 0.03;
            sum += distance(land, caustic_hit(p, depth, toward)) / length(vec3<f32>(0.03));
        }
    }
    return sum / 9.0 * exp(-0.5 * depth);
}

fn caustic_hit(p: vec3<f32>, depth: f32, toward: vec3<f32>) -> vec3<f32> {
    let g = textureSampleLevel(o_slopes, s_albedo, band_uv(p.xz, 3), 3, 1.0).xy;
    let n = normalize(vec3<f32>(g.x, 1.0, g.y));
    let r = normalize(refract(toward, n, 1.0 / 1.333));
    return p + (depth / r.y) * r;
}

// Linear opaque color behind the water at pixel `p`.
fn ocean_scene(p: vec2<f32>) -> vec3<f32> {
    let size = vec2<i32>(textureDimensions(o_scene));
    return unpack_rgb9e5(textureLoad(o_scene, clamp(vec2<i32>(p), vec2<i32>(0), size - 1), 0).r);
}

fn ocean_depth(p: vec2<f32>) -> f32 {
    let size = vec2<i32>(textureDimensions(t_depth));
    return textureLoad(t_depth, clamp(vec2<i32>(p), vec2<i32>(0), size - 1), 0);
}

fn pixel_ndc(p: vec2<f32>) -> vec2<f32> {
    return vec2<f32>(p.x / globals.viewport.x * 2.0 - 1.0, 1.0 - p.y / globals.viewport.y * 2.0);
}

// ComputeAbsorptionCoeff: the in and out rays' mean absorption, turning
// deep as the view ray passes ShallowWaterDepth times ViewDepth.
fn absorption_coeff(length_out: f32, absorb_in: vec3<f32>, absorb_out: vec3<f32>, shallow: vec4<f32>, deep: vec4<f32>, view_depth: f32) -> vec3<f32> {
    let end = shallow.w * view_depth;
    let start = max(0.0, end - deep.w);
    let t = clamp((length_out - start) / max(end - start, 1e-6), 0.0, 1.0);
    return mix(0.5 * (absorb_in + absorb_out), deep.rgb, t);
}

// ComputeWaterDiffuse: the ground through the water, plus light the water
// scatters toward the camera (σs `sigma_s`).
fn water_diffuse(refraction: vec3<f32>, caustic: f32, length_in: f32, length_out: f32, absorb_in: vec3<f32>, absorb_out: vec3<f32>, absorb: vec3<f32>, sigma_s: vec3<f32>, sun_shadow: f32) -> vec3<f32> {
    let light = lights.sun_color.rgb;
    let sigma_t = absorb + sigma_s;
    let sdl = sigma_t * (length_out + length_in);
    var inscatter = (0.5 / 3.14159265) * sigma_s * length_out / max(sdl, vec3<f32>(1e-12)) * (1.0 - exp(-sdl)) * light;
    inscatter = select(inscatter, vec3<f32>(0.0), sdl == vec3<f32>(0.0));
    inscatter *= mix(sun_shadow, 1.0, 0.7);
    return refraction * caustic * exp(-length_in * absorb_in) * exp(-length_out * absorb_out) + inscatter;
}

// GetAmbientLightDiffuseDirectionless, for foam.
fn water_ambient() -> vec3<f32> {
    return 0.2 * vec3<f32>(lights.ambient[0][3][3], lights.ambient[1][3][3], lights.ambient[2][3][3]);
}

// The sky a water surface reflects along `r` (SampleReflection without
// planar or screen-space reflections).
fn water_sky(r0: vec3<f32>) -> vec3<f32> {
    var r = r0;
    r.y = abs(r.y);
    if lights.disc.w > 0.5 { return sky_map(r); }
    return sky_radiance(r);
}

// SampleReflection: the island as the screen shows it along `r`, over the
// sky (times `sky_factor`) where the ray finds nothing.
fn reflection(world: vec3<f32>, r: vec3<f32>, sky_factor: f32) -> vec3<f32> {
    let sky = water_sky(r) * sky_factor;
    let hit = ssr_trace(o_scene, world, r, Ssr(ocean.ssr), vec2<f32>(0.0));
    return mix(sky, hit.rgb, hit.a);
}

@fragment fn fs_ocean(in: OceanOutput) -> @location(0) vec4<f32> {
    // Behind the opaque scene: out before anything else. The cut-out's
    // discard below holds the depth test until after the shader, which
    // otherwise ran in full for the sea under the whole island (some 0.06
    // ms of Albion's 0.76 at 5K). This is the depth test's own, against
    // the depth the pass starts with; the sea's own overlaps still meet it
    // after.
    if in.clip.z < ocean_depth(in.clip.xy) { discard; }
    let world = in.world;
    let eye = ocean_eye(world);
    let to_eye = normalize(eye - world);
    let distance3 = distance(world, eye);
    let coast = coast_at(in.rest.xz);
    // Well inland the sea stays under the ground, except where marsh sinks
    // below sea level under its lake: the game cuts the sea out there
    // (LakeCutout), the editor by the coast map, under ground that stands
    // above the sea (hollows below it still fill, as in game). Only over
    // ground: seen from under land that runs off the island's edge (DLC 3's
    // quadrants), nothing lies behind the sea, and the game's sea shows.
    // Reversed depth: 0 is nothing.
    if coast.x < INLAND && coast.y <= 0.0 && ocean_depth(in.clip.xy) > 0.0 { discard; }
    let info = water_info(in.rest.xz);
    let f = band_factors(coast.x, distance(world.xz, eye.xz), in.depth);
    // Surface: the bands' slopes. Only bands that show are read: by the
    // coast or out at sea half of them weigh nothing, and the finest fade
    // out with distance. A band's coastal and offshore slopes share a texel
    // (ocean.wgsl), one fetch where the two blend. Gradients come from
    // before the branch, as the implicit ones would.
    var g = vec2<f32>(0.0);
    let dx = dpdx(world.xz);
    let dy = dpdy(world.xz);
    for (var b = 0; b < 4; b++) {
        if f[b] <= 0.0 && f[b + 4] <= 0.0 { continue; }
        let tile = ocean.patches[b];
        let pair = textureSampleGrad(o_slopes, s_albedo, world.xz / tile, b, dx / tile, dy / tile);
        g += f[b] * pair.xy + f[b + 4] * pair.zw;
    }
    let puv = physical_uv(world.xz);
    let n = normalize(normalize(vec3<f32>(g.x, 1.0, g.y)) * 2.0 + physical_normal(puv));
    let roughness = mix(0.05, 0.1, clamp(distance3 / 300.0, 0.0, 1.0));
    // Scene behind: the opaque depth here, then the refracted lookup.
    let pixel = in.clip.xy;
    let scene_ws = unproject(pixel_ndc(pixel), max(ocean_depth(pixel), 1e-7));
    var offset = (1.0 - clamp(distance3 / 256.0, 0.0, 1.0)) * 0.2;
    offset += pow(clamp(-dot(reflect(-to_eye, n), to_eye), 0.0, 1.0), 8.0) * 0.5;
    var refracted = pixel + n.xz * offset * 0.4 * clamp(world.y - scene_ws.y, 0.0, 1.0) * globals.viewport.xy;
    var refracted_depth = ocean_depth(refracted);
    if refracted_depth > in.clip.z {
        refracted = pixel;
        refracted_depth = ocean_depth(pixel);
    }
    let refracted_ws = unproject(pixel_ndc(pixel), max(refracted_depth, 1e-7));
    let length_out = max(0.0, distance(refracted_ws, eye) - distance3);
    let refraction = ocean_scene(refracted);
    // Rays: light in from a fixed sun 45° up, out toward the camera.
    let fake_sun = normalize(vec3<f32>(1.0, 1.0, 0.0));
    let length_in = abs(-refracted_ws.y / fake_sun.y);
    let in_dir = select(vec3<f32>(0.0, 1.0, 0.0), fake_sun, length_in > 0.01);
    let absorb_in = mean_absorption(in_dir, length_in);
    let absorb_out = mean_absorption(eye - world, length_out);
    let absorb = absorption_coeff(length_out, absorb_in, absorb_out, ocean.absorb_shallow, ocean.absorb_deep, ocean.scatter.w);
    let sun_shadow = clamp(shadow(world, n), 0.0, 1.0);
    // Caustics on a seabed within sight.
    var caustic = 1.0;
    if length_out < ocean.scatter.w + ocean.absorb_shallow.w {
        caustic = mix(1.0, ocean_caustic(refracted_ws), ocean.shallow.w);
    }
    let light = lights.sun_color.rgb;
    let diffuse = water_diffuse(refraction, caustic, length_in, length_out, absorb_in, absorb_out, absorb, ocean.scatter.rgb, sun_shadow);
    // ComputeWaterSpecular.
    let sun = ocean_sun();
    let h = normalize(sun + to_eye);
    let specular = 0.25 / 3.14159265 * ocean_ggx(dot(n, h), roughness) * light * dot(n, sun) * sun_shadow;
    // SampleReflection without the planar map: the sky, flattened far off.
    let far = clamp((distance3 - 400.0) / 400.0, 0.0, 1.0);
    let rn = normalize(mix(n, vec3<f32>(0.0, 1.0, 0.0), (1.0 - roughness) * far));
    let reflected = reflection(world, reflect(-to_eye, rn), ocean.offshore.w);
    let f0 = 0.020393;
    let fresnel = mix(f0, 1.0, pow(1.0 - clamp(dot(n, to_eye), 0.0, 1.0), 3.5));
    var color = (1.0 - fresnel) * diffuse + fresnel * specular + fresnel * reflected;
    // Foam (GetFoamInfo, BlendFoamColor): the physical waves' crest and
    // wave foam, where waves touch rocky shores, and the foam painted into
    // the island's WaterInfos. All of it scales by `any`, so where none of
    // those is (most of the open sea) the maps go unread. Their footprint
    // comes from before the branch: the implicit (coarse) one, sharpened
    // as by a bias of -0.6.
    let state = physical_sample(puv);
    let crest_foam = state.z;
    let wave_foam = state.w;
    var contact = (1.0 - coast.x / 15.0) * (1.0 - info.z) * max(0.0, 0.6 - 2.0 * (world.y - scene_ws.y));
    contact = max(contact, 0.0);
    let painted = info.w;
    let any = clamp(painted + crest_foam + wave_foam + contact, 0.0, 1.0);
    let foam_dx = dpdxCoarse(in.rest.xz) * FOAM_SHARPEN;
    let foam_dy = dpdyCoarse(in.rest.xz) * FOAM_SHARPEN;
    if ocean.grid.w > 0.5 && any > 0.0 {
        let time = ocean.offshore.z;
        let coarse = textureSampleGrad(o_foam, s_albedo, in.rest.xz * 0.02, foam_dx * 0.02, foam_dy * 0.02);
        let fine = textureSampleGrad(o_foam, s_albedo, in.rest.xz * 0.36, foam_dx * 0.36, foam_dy * 0.36);
        let anim = clamp(pow(ocean_noise(vec3<f32>(in.rest.xz * 0.25, time * 0.06)), 2.0) * 3.0, 0.0, 1.0);
        let anim2 = clamp(pow(ocean_noise(vec3<f32>(in.rest.xz * 0.1, time * 0.03)), 3.0) * 3.0, 0.0, 1.0);
        let k = clamp(coast.x / 5.0, 0.0, 1.0);
        let crest = crest_foam * mix(fine.b, coarse.b, k) + crest_foam * crest_foam * mix(fine.g, coarse.g, k);
        let wave = pow(wave_foam, 1.5) * fine.b + pow(wave_foam, 3.0) * fine.g + 0.1 * pow(wave_foam, 1.5) * fine.a;
        let bubbles = 0.3 * anim * max(pow(wave_foam, 8.0), crest_foam);
        let still = mix(fine.b, mix(fine.g, fine.r, anim), painted * painted) * painted * 2.5 * anim2;
        let painted_foam = clamp(still * still, 0.0, 1.0);
        let foam = (painted_foam + clamp(crest + wave + contact * fine.a, 0.0, 1.0)) * any;
        let lit = ocean.offshore.w * water_ambient() + light / 3.14159265 * (0.3 + 0.7 * sun_shadow);
        color = mix(color, lit * mix(ocean.shallow.rgb, vec3<f32>(0.9), 0.45), bubbles * any);
        color = mix(color, lit * 0.9, foam);
    }
    // Where the seabed comes up to the surface the water thins to clear.
    var clear = clamp((world.y - scene_ws.y) * (2.0 + min(30.0, info.x * info.x * 20.0 + (1.0 - info.z) * 20.0)), 0.0, 1.0);
    clear = clamp(clear + step(1.0, scene_ws.y), 0.0, 1.0);
    color = mix(refraction, color, clear);
    // Over the opaque scene the Blend stage multiplies the occlusion over
    // (frame.wgsl occlusion_factor): taken off here first.
    return vec4<f32>(fog(color, world) / occlusion_factor(vec2<i32>(in.clip.xy)), 1.0);
}

// --- Inland water -----------------------------------------------------------
// Anno 117's rivers and lakes (water_inland_water.fx psRiver/psLake) on the
// editor's water lattice (gpu::inland), in the ocean's pass and with its
// bindings 0-7: lakes ripple with the ocean's smallest band, rivers with a
// looping ripple volume carried along the flow map. Left out: physical
// waves, churn and the refraction blur.

struct InlandParams {
    // Corner (0, 0) X and Z, corner spacing and its inverse.
    grid: vec4<f32>,
    // Corners per row, rows, wave seconds; w: 1 with river waves.
    size: vec4<f32>,
    // How hard it rains (0 without the ripple volume); y: 1 with duckweed;
    // z: where the river ripples fall between the two frames whose slopes
    // i_waves holds (inland.rs Waves), 0.25 to 0.75.
    weather: vec4<f32>,
    // x: 1 in Egypt, which the game draws with its EGYPT_INLAND_WATER
    // shader; y: cIWSmallWaveWindGustBoost; zw: cAccumulatedWind.
    egypt: vec4<f32>,
    // x: how far the season lifts every river (not lakes), cIWHeightOffset.
    season: vec4<f32>,
    // x: 1 where rivers flow by Egypt's painted map
    // (ISF_USES_FLOW_TEX_FROM_FILE); yz: the island's corner on it; w: the
    // session's side the map spans.
    flow_file: vec4<f32>,
}

struct InlandCorner {
    y: f32,
    look: u32,
    // Flow speed, a fraction of the largest.
    speed: f32,
    // 1 on the water, 0 at a river end its record fades.
    fade: f32,
    flow: vec2<f32>,
}

struct InlandLook {
    // As OceanParams: absorption with ShallowDepth, deep absorption with
    // AbsorptionSmoothness, scattering with ViewDepth.
    absorb_shallow: vec4<f32>,
    absorb_deep: vec4<f32>,
    scatter: vec4<f32>,
    // ColorShallow; w: 1 for lakes.
    shallow: vec4<f32>,
    // WaveIntensity, MaxFoamDepth; z: 1 for permanent lava.
    wave: vec4<f32>,
    // cIWWaveSize, cIWWaveSpeed, cIWPhysicalWaveIntensity.
    motion: vec4<f32>,
    // cIWLarge, Medium and SmallWaveWeight (Egypt's shader only).
    weights: vec4<f32>,
}

@group(1) @binding(8) var<uniform> inland: InlandParams;
@group(1) @binding(9) var<storage, read> i_corners: array<InlandCorner>;
@group(1) @binding(10) var<storage, read> i_looks: array<InlandLook>;
@group(1) @binding(11) var i_waves: texture_3d<f32>;
@group(1) @binding(12) var i_wrap3: sampler;
@group(1) @binding(19) var i_duckweed: texture_2d<f32>;
@group(1) @binding(20) var i_duckweed_normal: texture_2d<f32>;
// cWaterRippleBumpMap: rain splashes rippling out, looping in depth.
@group(1) @binding(21) var i_rain: texture_3d<f32>;
// cWindTex and cPerlinNoiseTex's red channel: Egypt's river gusts.
@group(1) @binding(22) var i_wind: texture_2d<f32>;
@group(1) @binding(23) var i_perlin: texture_2d<f32>;
// Egypt's painted flow map: flow X and Z as 0-1 in rg, speed in a.
@group(1) @binding(24) var i_flow_file: texture_2d<f32>;

struct InlandOutput {
    @builtin(position) clip: vec4<f32>,
    @location(0) world: vec3<f32>,
    @location(1) @interpolate(flat) look: u32,
    // vsRiver's Alpha.
    @location(2) fade: f32,
}

@vertex fn vs_inland(@builtin(vertex_index) corner: u32) -> InlandOutput {
    let c = i_corners[corner];
    let w = u32(inland.size.x);
    let xz = inland.grid.xy + vec2<f32>(f32(corner % w), f32(corner / w)) * inland.grid.z;
    var out: InlandOutput;
    var y = c.y;
    // Dry corners carry no look.
    if c.look < arrayLength(&i_looks) && i_looks[c.look].shallow.w < 0.5 {
        y += inland.season.x;
    }
    out.world = vec3<f32>(xz.x, y, xz.y);
    out.look = c.look;
    out.fade = c.fade;
    out.clip = globals.clip * vec4<f32>(out.world, 1.0);
    return out;
}

// ApplyRainRipplesStrength: rain's splash ripples bent into `n`, fading
// out by 180 units from the camera. The game reads the strength from its
// weather map's rain; the editor from its Rain switch.
fn rain_ripples(n: vec3<f32>, world: vec3<f32>, dist: f32) -> vec3<f32> {
    let strength = inland.weather.x;
    var fade = 1.0 - clamp(dist / 180.0, 0.0, 1.0);
    fade = fade * fade * fade;
    if strength <= 0.0 || fade <= 0.0 { return n; }
    let step = 1.0 / vec3<f32>(textureDimensions(i_rain));
    let at = vec3<f32>(world.xz * 0.6, inland.size.z * 1.2);
    let bump = textureSampleLevel(i_rain, i_wrap3, at, 0.0).r;
    let bx = textureSampleLevel(i_rain, i_wrap3, at + vec3<f32>(step.x, 0.0, 0.0), 0.0).r;
    let by = textureSampleLevel(i_rain, i_wrap3, at + vec3<f32>(0.0, step.y, 0.0), 0.0).r;
    let intensity = 3.0 * strength * pow(strength, 0.33) * fade;
    return normalize(n + vec3<f32>(bx - bump, 0.0, by - bump) * intensity);
}

// DetermineRiverFlow: the four flow map texels around `xz` (the lattice's
// corners), their bilinear weights, and the blended direction and speed.
struct RiverFlow {
    dirs: array<vec2<f32>, 4>,
    speeds: vec4<f32>,
    weights: vec4<f32>,
    dir: vec2<f32>,
    speed: f32,
}

// `wave_speed` is cIWWaveSpeed: it scales the texels' directions the ripples
// scroll along, not the blended direction or speed.
fn river_flow(xz: vec2<f32>, wave_speed: f32) -> RiverFlow {
    if inland.flow_file.x > 0.5 { return river_flow_file(xz, wave_speed); }
    let t = max((xz - inland.grid.xy) * inland.grid.w, vec2<f32>(0.0));
    let base = vec2<i32>(floor(t));
    let f = t - floor(t);
    let size = vec2<i32>(inland.size.xy);
    let offsets = array<vec2<i32>, 4>(vec2<i32>(0, 0), vec2<i32>(0, 1), vec2<i32>(1, 0), vec2<i32>(1, 1));
    var out: RiverFlow;
    out.weights = vec4<f32>((1.0 - f.x) * (1.0 - f.y), (1.0 - f.x) * f.y, f.x * (1.0 - f.y), f.x * f.y);
    out.dir = vec2<f32>(0.0);
    out.speed = 0.0;
    for (var k = 0; k < 4; k++) {
        let p = clamp(base + offsets[k], vec2<i32>(0), size - 1);
        let c = i_corners[u32(p.y * size.x + p.x)];
        out.dirs[k] = c.flow * wave_speed;
        out.speeds[k] = c.speed;
        out.dir += out.weights[k] * c.flow;
        out.speed += out.weights[k] * c.speed;
    }
    return out;
}

// DetermineRiverFlow with ISF_USES_FLOW_TEX_FROM_FILE: Egypt's session-wide
// painted map in place of the island's computed one, upside down (the
// game flips it), its speed a constant 1 but per texel for the ripples.
fn river_flow_file(xz: vec2<f32>, wave_speed: f32) -> RiverFlow {
    let size = vec2<f32>(textureDimensions(i_flow_file, 0));
    var uv = (xz + inland.flow_file.yz) / inland.flow_file.w - 0.5 / size;
    uv.y = 1.0 - uv.y;
    let t = uv * size;
    let base = vec2<i32>(floor(t));
    let f = t - floor(t);
    let offsets = array<vec2<i32>, 4>(vec2<i32>(0, 0), vec2<i32>(0, 1), vec2<i32>(1, 0), vec2<i32>(1, 1));
    var out: RiverFlow;
    out.weights = vec4<f32>((1.0 - f.x) * (1.0 - f.y), (1.0 - f.x) * f.y, f.x * (1.0 - f.y), f.x * f.y);
    var blended = vec4<f32>(0.0);
    for (var k = 0; k < 4; k++) {
        let p = clamp(base + offsets[k], vec2<i32>(0), vec2<i32>(size) - 1);
        let v = textureLoad(i_flow_file, p, 0);
        out.dirs[k] = (v.xy * 2.0 - 1.0) * wave_speed;
        out.speeds[k] = v.w;
        blended += out.weights[k] * v;
    }
    out.dir = blended.xy * 2.0 - 1.0;
    out.speed = 1.0;
    return out;
}

// GetWaveTexCoord: `coords` turned into the flow and scrolled along it,
// slower water stretched (GetSlowWaterFactor).
fn wave_uv(coords: vec2<f32>, dir: vec2<f32>, speed_in: f32, max_speed: f32) -> vec2<f32> {
    let speed = max_speed * length(dir) * max(0.01, speed_in * speed_in);
    let d = vec2<f32>(-dir.x, dir.y);
    let l = length(d);
    let cs = select(vec2<f32>(1.0, 0.0), d / max(l, 1e-6), l > 1e-6);
    let turned = vec2<f32>(coords.x * cs.x - coords.y * cs.y, coords.x * cs.y + coords.y * cs.x);
    var uv = fract(turned + vec2<f32>(speed * inland.size.z, 0.0));
    let slow = clamp((1.0 - speed_in - 0.8) / 0.2, 0.0, 1.0);
    return uv * (1.0 + slow * slow);
}

// A value noise repeating every `period` units, in place of the game's
// noise textures where wrapped coordinates would show a seam.
fn tile_noise(p: vec2<f32>, period: f32) -> f32 {
    let i = floor(p);
    let f = p - i;
    let u = f * f * (3.0 - 2.0 * f);
    let a = wrapped_hash(i, period);
    let b = wrapped_hash(i + vec2<f32>(1.0, 0.0), period);
    let c = wrapped_hash(i + vec2<f32>(0.0, 1.0), period);
    let d = wrapped_hash(i + vec2<f32>(1.0, 1.0), period);
    return mix(mix(a, b, u.x), mix(c, d, u.x), u.y);
}

fn wrapped_hash(q: vec2<f32>, period: f32) -> f32 {
    return ocean_hash(vec3<f32>(q - period * floor(q / period), 0.0));
}

// SampleRiverFlowTex: `t` (the foam map) at four flow-turned coordinates,
// blended by the flow texels' weights, with mips chosen from the unturned
// coordinates.
fn river_flow_sample(t: texture_2d<f32>, xz: vec2<f32>, flow: RiverFlow, scale: f32, dx: vec2<f32>, dy: vec2<f32>) -> vec4<f32> {
    let uv_scale = 0.18 * scale;
    let time = inland.size.z;
    let disturb = 0.007 * vec2<f32>(tile_noise(xz * 0.125 * 8.0 + time * 0.4, 1e4), tile_noise(xz * 0.125 * 8.0 + 31.0 + time * 0.4, 1e4));
    let coords = uv_scale * xz + disturb;
    var sum = vec4<f32>(0.0);
    for (var k = 0; k < 4; k++) {
        let uv = wave_uv(coords, flow.dirs[k], max(0.01, sqrt(flow.speeds[k])), uv_scale * 2.5);
        sum += flow.weights[k] * textureSampleGrad(t, s_albedo, uv, dx * scale, dy * scale);
    }
    return sum;
}

// The perlin noise ComputeRiverFoam scrolls with the flow.
fn river_noise(xz: vec2<f32>, flow: RiverFlow) -> f32 {
    let uv_scale = 0.18 * 2.0;
    var sum = 0.0;
    for (var k = 0; k < 4; k++) {
        let uv = wave_uv(uv_scale * xz, flow.dirs[k], max(0.01, sqrt(flow.speeds[k])), uv_scale * 2.5);
        sum += flow.weights[k] * tile_noise(uv * 16.0, 16.0);
    }
    return sum;
}

// SamplePhysicalWavesNormal as the inland shader reads it: last step's
// heights differenced across a texel, times 1.4, as world X and Z.
fn physical_normal_xz(uv: vec2<f32>) -> vec2<f32> {
    let t = ocean.physical.w;
    let s = physical_sample(uv + vec2<f32>(0.0, -t)).y;
    let w = physical_sample(uv + vec2<f32>(-t, 0.0)).y;
    let e = physical_sample(uv + vec2<f32>(t, 0.0)).y;
    let n = physical_sample(uv + vec2<f32>(0.0, t)).y;
    return 1.4 * vec2<f32>(w - e, s - n);
}

// ComputeGustIntensity at Egypt's river scales (scroll 1.5, wind texture
// 1.5 / 513, factor 1 / 513): two wind texture taps drifting with the
// accumulated wind, gated by slower drifting perlin noise.
fn river_gusts(xz: vec2<f32>) -> f32 {
    let wind = inland.egypt.zw;
    let wind_scale = 1.5 / 513.0;
    let fac_scale = 1.0 / 513.0;
    let wind_at = wind_scale * xz - wind * wind_scale * 1.5;
    let fac_at = fac_scale * xz - wind * fac_scale * 0.2;
    let g0 = textureSampleLevel(i_wind, o_wrap, wind_at, 0.0).r;
    let g1 = textureSampleLevel(i_wind, o_wrap, wind_at + vec2<f32>(0.5, 0.0), 0.0).r;
    let gust = clamp(g0 + g1, 0.0, 1.0);
    let factor = textureSampleLevel(i_perlin, o_wrap, fac_at, 0.0).r;
    return clamp((gust - 0.5) / 0.5, 0.0, 1.0) * factor;
}

// ScaledTriangleWave (functions.fxh).
fn triangle_wave(x: f32, peak: f32, width: f32) -> f32 {
    if abs(x - peak) > width { return 0.0; }
    return abs(fract((x - peak) / (2.0 * width)) * 2.0 - 1.0);
}

// The opaque scene's world position behind pixel `p`.
fn scene_at(p: vec2<f32>) -> vec3<f32> {
    return unproject(pixel_ndc(p), max(ocean_depth(p), 1e-7));
}

fn world_pixel(p: vec3<f32>) -> vec2<f32> {
    let c = globals.clip * vec4<f32>(p, 1.0);
    let ndc = c.xy / c.w;
    return vec2<f32>((ndc.x * 0.5 + 0.5) * globals.viewport.x, (0.5 - ndc.y * 0.5) * globals.viewport.y);
}

@fragment fn fs_river(in: InlandOutput) -> @location(0) vec4<f32> {
    let look = i_looks[in.look];
    let world = in.world;
    let eye = ocean_eye(world);
    let to_eye = normalize(eye - world);
    let dist = distance(world, eye);
    let time = inland.size.z;
    let flow = river_flow(world.xz, look.motion.y);
    // Foam mips from the unturned coordinates, in uniform control flow.
    let foam_dx = dpdx(0.18 * world.xz) * 0.7;
    let foam_dy = dpdy(0.18 * world.xz) * 0.7;
    let egypt = inland.egypt.x > 0.5;
    let pixel = in.clip.xy;
    let scene_unbent = scene_at(pixel);
    // DetermineRiverSurfaceInfo: Egypt's shader adds the ocean's wind gusts
    // as patches of rougher water.
    var gusts = 0.0;
    if egypt {
        gusts = river_gusts(world.xz);
        gusts *= gusts;
    }
    // ComputeRiverNormal (SampleDirectionalGradient): the ripple volume's
    // slopes in the flow's frame, at cIWWaveSize. Latium and Albion take
    // the medium waves only; Egypt mixes in large and small ones by the
    // look's weights, calms them over beaches and lets gusts stir the small
    // ones. Flatter where the water is slow and with distance.
    let uv_scale = 1.0 / (look.motion.x * 4.0);
    let frame = inland.weather.z;
    var w = vec3<f32>(0.0, 1.0, 0.0);
    if egypt {
        w = look.weights.xyz;
        // The game measures depth to its terrain heightmap; the opaque
        // scene behind the water stands in.
        let dh = max(0.0, world.y - scene_unbent.y);
        let beach = clamp((dh - 2.5) / (0.3 - 2.5), 0.0, 1.0);
        let flat_beach = clamp((dh - 0.3) / (0.0 - 0.3), 0.0, 1.0);
        w = mix(w, vec3<f32>(0.2, 0.0, 0.0), beach);
        w.x *= 1.0 - flat_beach;
        w.y *= 1.0 - flat_beach;
        w.z += flat_beach * 0.1;
        let boost = inland.egypt.y;
        w.z += gusts * boost;
        w.y *= 1.0 - gusts * clamp(boost, 0.0, 1.0);
    }
    var g = vec2<f32>(0.0);
    if inland.size.w > 0.5 {
        // Each band: its scale on the coordinates and on the flow speed.
        let bands = array<vec3<f32>, 3>(vec3<f32>(0.187, 0.1, w.x), vec3<f32>(1.0, 1.0, w.y), vec3<f32>(5.0, 1.0, w.z));
        for (var b = 0; b < 3; b++) {
            let band = bands[b];
            if band.z == 0.0 { continue; }
            var sum = vec2<f32>(0.0);
            for (var k = 0; k < 4; k++) {
                let speed = max(0.01, sqrt(flow.speeds[k])) * band.y;
                let uv = wave_uv(uv_scale * world.xz * band.x, flow.dirs[k], speed, uv_scale * 2.0);
                sum += flow.weights[k] * textureSampleLevel(i_waves, i_wrap3, vec3<f32>(uv, frame), 0.0).xy;
            }
            g += band.z * sum;
        }
    }
    g *= 0.05 + 0.95 * pow(clamp(flow.speed * 4.0, 0.0, 1.0), 4.0);
    g *= sqrt(abs(to_eye.y));
    let wave_intensity = look.wave.x;
    let normal_y = mix(25.0, 2.5, pow(wave_intensity, 0.15)) * 0.15 * dist;
    let fft = normalize(vec3<f32>(g.x, normal_y, g.y));
    let l = length(flow.dir);
    let along = select(vec3<f32>(1.0, 0.0, 0.0), vec3<f32>(flow.dir.x, 0.0, flow.dir.y) / max(l, 1e-6), l > 1e-6);
    let across = vec3<f32>(along.z, 0.0, -along.x);
    var combined = fft.x * across + vec3<f32>(0.0, fft.y, 0.0) + fft.z * along;
    if egypt {
        // Crisper waves far off.
        combined = vec3<f32>(combined.x * (1.0 + dist * 0.05), combined.y, combined.z * (1.0 + dist * 0.05));
    }
    // The shore's physical waves, held back where they run against the
    // flow (coast waves only: ship bow waves are negative heights), by
    // cIWPhysicalWaveIntensity.
    let puv = physical_uv(world.xz);
    let phys_n = physical_normal_xz(puv);
    var reductor = sqrt(clamp(dot(normalize(flow.dir + vec2<f32>(1e-6, 0.0)), phys_n), 0.0, 1.0));
    reductor = mix(1.0, reductor, clamp(physical_sample(puv).x / 0.5, 0.0, 1.0));
    combined = vec3<f32>(combined.x + phys_n.x * clamp(reductor * look.motion.z, 0.0, 1.0), combined.y, combined.z + phys_n.y * clamp(reductor * look.motion.z, 0.0, 1.0));
    let n = rain_ripples(normalize(combined), world, dist);
    let info = water_info(world.xz);
    let fade = clamp(0.12 * dist - 1.8, 0.0, 1.0);
    var roughness = mix(0.04, mix(0.05, 0.08, wave_intensity), fade);
    if egypt {
        // Rougher in gusts far off, against specular aliasing. The game
        // scales the distance by its camera's FOV correction, 1 here.
        roughness += gusts * (0.1 + 0.3 * clamp(dist / 100.0, 0.0, 1.0));
        roughness = clamp(roughness, 0.0, 1.0);
    }
    // DetermineRiverSceneInfo: refracted a quarter of the way past the
    // ground along the bent view ray. Egypt's shader also shifts the
    // lookups by a fixed share of the screen along the normal, fading at
    // the banks.
    var shift = vec2<f32>(0.0);
    if egypt {
        let border = smoothstep(0.0, 0.15, distance(world, scene_unbent));
        shift = n.xz * border * 0.15 * globals.viewport.xy;
    }
    var scene_pixel = pixel + shift;
    if ocean_depth(scene_pixel) > in.clip.z { scene_pixel = pixel; }
    let scene_ws = scene_at(scene_pixel);
    let view = normalize(scene_ws - eye);
    var bent = refract(view, n, 1.0 / 1.333);
    if dot(bent, bent) < 1e-6 { bent = view; }
    let refracted_ws = scene_ws + normalize(bent) * 0.25 * max(0.0, distance(scene_ws, eye) - dist);
    var coord = world_pixel(refracted_ws) + shift;
    if ocean_depth(coord) > in.clip.z { coord = pixel; }
    let refraction = ocean_scene(coord);
    // DetermineInlandRays.
    let sun = ocean_sun();
    let view_ray = world - refracted_ws;
    let length_out = distance(refracted_ws, world);
    let length_in = abs(view_ray.y / select(sun.y, 1e-3, abs(sun.y) < 1e-3));
    let absorb_in = path_absorption(sun, length_in, look.absorb_shallow, look.absorb_deep);
    let absorb_out = path_absorption(normalize(view_ray + vec3<f32>(0.0, 1e-6, 0.0)), length_out, look.absorb_shallow, look.absorb_deep);
    let absorb = absorption_coeff(length_out, absorb_in, absorb_out, look.absorb_shallow, look.absorb_deep, look.scatter.w);
    let sun_shadow = clamp(shadow(world, n), 0.0, 1.0);
    // ComputeRiverFoam: foam where the water runs fast, and along shallow
    // banks of fast water.
    var foam = 0.0;
    if ocean.grid.w > 0.5 {
        let speed = flow.speed;
        let noise = mix(0.25 + clamp((river_noise(world.xz, flow) - 0.2) / 0.8, 0.0, 1.0), 1.0, clamp(speed * speed, 0.0, 1.0));
        let foam0 = river_flow_sample(o_foam, world.xz, flow, 1.0, foam_dx, foam_dy);
        let foam1 = river_flow_sample(o_foam, world.xz, flow, 4.0, foam_dx, foam_dy);
        // Fast water foams, and so does foam painted into WaterInfos. Not
        // fast water in Egypt, whose rivers are so wide speed would cover
        // them in foam.
        let running_foam = select(clamp(pow(speed + 0.25, 4.0), 0.0, 1.0), 0.0, egypt);
        let intensity = clamp(running_foam + info.w, 0.0, 1.0);
        var running = 0.33 * foam0.b * foam0.b * triangle_wave(intensity, 0.3, 0.2);
        running += 0.33 * foam0.b * triangle_wave(clamp(intensity - 0.1, 0.0, 1.0), 0.5, 0.5);
        running += 0.33 * foam0.g * triangle_wave(clamp(intensity - 0.1, 0.0, 1.0), 1.0, 0.5);
        foam += clamp(running * 2.5 * 0.5, 0.0, 1.0);
        // Banks of fast water foam; not in Egypt, whose rivers stand in
        // for the sea.
        if !egypt {
            let depth = world.y - scene_ws.y;
            var border = clamp(1.0 - depth / max(look.wave.y, 1e-3), 0.0, 1.0);
            border = clamp(5.0 * pow(max(border - 0.05, 0.0), 4.0) * speed, 0.0, 1.0);
            var bank = mix(foam1.g, 0.0, border);
            bank += foam0.a * mix(0.7, 1.0, bank);
            foam += noise * border * bank;
        }
        // GetInlandPhysicalWavesFoam: the physical waves' crest foam, by
        // cIWPhysicalWaveIntensity and as held back against the flow.
        let has_physical = clamp(physical_sample(puv).z * look.motion.z * reductor, 0.0, 1.0);
        if has_physical > 0.0 {
            let fuv = world.xz * 0.36;
            let fine = textureSampleGrad(o_foam, s_albedo, fuv, foam_dx / 0.7 * 2.0 * FOAM_SHARPEN, foam_dy / 0.7 * 2.0 * FOAM_SHARPEN);
            let p15 = pow(has_physical, 1.5);
            foam += p15 * fine.b + has_physical * has_physical * has_physical * fine.g + 0.1 * p15 * fine.a;
        }
    }
    let light = lights.sun_color.rgb;
    let f0 = 0.020393 + 0.08;
    var fresnel = mix(f0, 1.0, pow(1.0 - clamp(dot(n, to_eye), 0.0, 1.0), 3.5));
    fresnel = mix(fresnel, f0, sqrt(roughness) * 0.5);
    let diffuse = water_diffuse(refraction, 1.0, length_in, length_out, absorb_in, absorb_out, absorb, look.scatter.rgb, sun_shadow);
    let h = normalize(sun + to_eye);
    let specular = 0.25 / 3.14159265 * ocean_ggx(dot(n, h), roughness) * light * max(dot(n, sun), 0.0) * sun_shadow;
    let reflected = reflection(world, reflect(-to_eye, n), 1.0);
    var color = (1.0 - fresnel) * diffuse + fresnel * specular + fresnel * reflected;
    // BlendRiverFoamColor.
    let lit = water_ambient() + light / 3.14159265 * (0.3 + 0.7 * sun_shadow);
    let keep = clamp(1.0 - foam, 0.0, 1.0);
    color = mix(lit * 0.9, color, keep * keep);
    // Thin water at the banks shows the ground as it is.
    color = mix(refraction, color, smoothstep(0.0, 0.15, distance(world, scene_ws)));
    // Over the opaque scene the Blend stage multiplies the occlusion over
    // (frame.wgsl occlusion_factor): taken off here first.
    return vec4<f32>(fog(color, world) / occlusion_factor(vec2<i32>(in.clip.xy)), in.fade);
}

@fragment fn fs_lake(in: InlandOutput) -> @location(0) vec4<f32> {
    let look = i_looks[in.look];
    let world = in.world;
    let eye = ocean_eye(world);
    let to_eye = normalize(eye - world);
    let dist = distance(world, eye);
    let time = inland.size.z;
    // ComputeLakeNormal: the ocean's smallest band, softened by
    // WaveIntensity, with a finer copy near the camera and gusts of noise.
    let detail = 0.2 + pow(1.0 - clamp(dist / 512.0, 0.0, 1.0), 2.0) * 0.8;
    let uv = band_uv(world.xz, 3);
    // Painted-out coast waves (WaterInfos) calm a lake too.
    let info = water_info(world.xz);
    let flatten = 10.0 * clamp(1.0 - look.wave.x + info.y, 0.0, 1.0);
    let g = textureSample(o_slopes, s_albedo, uv, 3).xy;
    let g2 = textureSampleGrad(o_slopes, s_albedo, uv * 3.0, 3, dpdx(uv * 3.0) * 0.5, dpdy(uv * 3.0) * 0.5).xy;
    var n = normalize(vec3<f32>(g.x, flatten, g.y));
    n = normalize(n + normalize(vec3<f32>(g2.x, flatten, g2.y)) * detail);
    let gust = 1.0 + min(2.0, pow(ocean_noise(vec3<f32>((world.xz * 0.03 + time * 0.01) * 16.0, 0.0)), 6.0) * 100.0);
    n = normalize(n * vec3<f32>(1.0, gust, 1.0));
    let coarse = normalize(vec3<f32>(g.x, flatten, g.y));
    n = rain_ripples(n, world, dist);
    let roughness = mix(0.03, 0.1, clamp((dist - 100.0) / 200.0, 0.0, 1.0));
    // DetermineLakeSceneInfo: the scene nudged by the normal near the
    // camera.
    let pixel = in.clip.xy;
    let scene_ws = scene_at(pixel);
    let nudge = pow(1.0 - clamp(dist / 256.0, 0.0, 1.0), 2.0) * 0.1;
    var coord = pixel + n.xz * nudge * 0.6 * clamp(world.y - scene_ws.y, 0.0, 1.0) * globals.viewport.xy;
    if ocean_depth(coord) > in.clip.z { coord = pixel; }
    let refracted_ws = unproject(pixel_ndc(pixel), max(ocean_depth(coord), 1e-7));
    let length_out = max(0.0, distance(refracted_ws, eye) - dist);
    let refraction = ocean_scene(coord);
    let sun = ocean_sun();
    let view_ray = world - refracted_ws;
    let length_in = abs(view_ray.y / select(sun.y, 1e-3, abs(sun.y) < 1e-3));
    let absorb_in = path_absorption(sun, length_in, look.absorb_shallow, look.absorb_deep);
    let absorb_out = path_absorption(normalize(view_ray + vec3<f32>(0.0, 1e-6, 0.0)), length_out, look.absorb_shallow, look.absorb_deep);
    let absorb = absorption_coeff(length_out, absorb_in, absorb_out, look.absorb_shallow, look.absorb_deep, look.scatter.w);
    let sun_shadow = clamp(shadow(world, n), 0.0, 1.0);
    // Caustics from the same band, on a bed measured from the surface.
    let caustic = mix(1.0, ocean_caustic(refracted_ws - vec3<f32>(0.0, world.y, 0.0)), 0.7);
    let light = lights.sun_color.rgb;
    let f0 = 0.020393;
    let fresnel = mix(f0, 1.0, pow(1.0 - clamp(dot(n, to_eye), 0.0, 1.0), 3.5));
    let diffuse = water_diffuse(refraction, caustic, length_in, length_out, absorb_in, absorb_out, absorb, look.scatter.rgb, sun_shadow);
    let h = normalize(sun + to_eye);
    let specular = 0.25 / 3.14159265 * ocean_ggx(dot(n, h), roughness) * light * max(dot(n, sun), 0.0) * sun_shadow;
    let reflected = reflection(world, reflect(-to_eye, n), 1.0);
    var color = fresnel * specular + (1.0 - fresnel) * diffuse + fresnel * reflected;
    let lit = water_ambient() + light / 3.14159265 * (0.3 + 0.7 * sun_shadow);
    // BlendLakeFoam: foam painted into WaterInfos, drifting with noise.
    let painted = info.w;
    if painted > 0.0 && ocean.grid.w > 0.5 {
        let anim = clamp(pow(ocean_noise(vec3<f32>(world.xz * 0.13 + time * 0.01, time * 0.025) * 4.0), 2.0) * 2.0, 0.0, 1.0);
        let anim2 = 0.3 + clamp(pow(ocean_noise(vec3<f32>(world.xz * 0.097, time * 0.025) * 4.0), 2.0) * 3.0, 0.0, 1.0) * 0.7;
        let fuv = world.xz * 0.2;
        let fine = textureSampleGrad(o_foam, s_albedo, fuv + coarse.xz * 0.06, dpdx(fuv) * 0.6, dpdy(fuv) * 0.6);
        let foam = pow(abs(mix(fine.b, mix(fine.g, fine.r, anim), painted * painted) * painted * 2.5 * anim2), 1.5);
        color = mix(color, lit * 0.9, clamp(foam, 0.0, 1.0));
    }
    // Duckweed where WaterInfos' beach is painted on a lake (the game
    // reads that channel as its cover), lit as a rough leaf layer.
    if inland.weather.y > 0.5 && info.z > 0.0 {
        let duv = world.xz * 0.375 + coarse.xz * 0.02;
        var weed = textureSample(i_duckweed, s_albedo, duv);
        weed = vec4<f32>(pow(weed.rgb, vec3<f32>(2.2)), weed.a);
        // BlendLayer, sharpness 2.
        let cover = clamp(2.0 * (clamp(weed.a, 0.0, 1.0) - 1.0) + 3.0 * info.z, 0.0, 1.0);
        let wn = normalize(textureSample(i_duckweed_normal, s_albedo, duv).xzy * 2.0 - 1.0);
        let hv = normalize(to_eye + sun);
        let ndl = dot(wn, sun);
        let ndv = max(dot(wn, to_eye), 1e-3);
        let k = 0.85 * 0.85 * 0.5;
        let vis = 0.25 / ((max(ndl, 1e-3) * (1.0 - k) + k) * (ndv * (1.0 - k) + k));
        let f = mix(f0, 1.0, pow(1.0 - clamp(ndv, 0.0, 1.0), 3.5));
        let spec = 0.25 / 3.14159265 * f * ocean_ggx(clamp(dot(wn, hv), 0.0, 1.0), 0.85) * vis * ndl * (0.3 + 0.7 * sun_shadow);
        // The game multiplies the sun by an ambient-lit albedo; the
        // editor's ambient is brighter than its, so the leaves are lit as
        // the foam is, plus their sheen.
        let shaded = weed.rgb * (water_ambient() + light / 3.14159265 * max(ndl, 0.0) * (0.3 + 0.7 * sun_shadow)) + light * spec * fresnel;
        color = mix(color, shaded, cover);
    }
    color = mix(refraction, color, smoothstep(0.0, 0.08, abs(world.y - scene_ws.y)));
    // Over the opaque scene the Blend stage multiplies the occlusion over
    // (frame.wgsl occlusion_factor): taken off here first.
    return vec4<f32>(fog(color, world) / occlusion_factor(vec2<i32>(in.clip.xy)), 1.0);
}

// --- Lava ---------------------------------------------------------------------
// psRiverLava (water_inland_water.fx): a river whose record sets IWF_LAVA is
// molten rock. Fractal simplex noise carried down the flow map splits it
// into melt that glows by the blackbody colour of its heat, brightest where
// it runs fast, and crust lit as dark rock; no refraction, reflection,
// waves or foam. It shows as while the volcano erupts (cLavaTimer > 0,
// every stream whole and flowing), which is how the game draws all but
// the permanent streams. Each stream is drawn as its own strip, as the game
// draws it (core::water::Strips), flowing on the lattice's flow map: it
// bows up mid-stream and sinks under its banks. Left out: how streams
// spread in and cool off as an eruption starts and ends, slow lava's
// bunched and bent pattern (lava_noise), and dsRiverLava's
// noise relief (crust up to 0.2 units proud of the melt, cLavaDisplacement
// with cLavaNoiseDispMin -1 and Max 2 over cLavaDisplacementDetail 2
// octaves): the strips are too coarse to carry it, and moving each pixel's
// depth instead cost 4.5 ms on a 5K frame of lava, as the depth test then
// waits for the shader.
// The game counts the pattern's octaves by distance, from one far off up to
// cLavaNoiseDetail within a few units; the editor counts them per pixel, as
// a mip level, so a 5K pane close up gets the detail the game shows close
// up.
// The engine fills cbInlandWaterGlobal from code: the inland water
// renderer's constructor writes these into its copy of the buffer
// (this+0x70) and after that only cAshRiverStrength changes. The
// constructor is obfuscated; the values come from running it under an
// emulator (Anno117.exe build 26607411: 0x14257D440; build 26043928:
// 0x14248E580). Each line names its cbuffer field.
const LAVA_NOISE_SCALE: f32 = 1.2; // cLavaNoiseScale
const LAVA_NOISE_DISTORTION: f32 = 0.5; // cLavaNoiseDistortion
const LAVA_NOISE_DETAIL: f32 = 8.0; // cLavaNoiseDetail
const LAVA_NOISE_ROUGHNESS: f32 = 0.7; // cLavaNoiseRoughness
const LAVA_NOISE_FACTOR: f32 = 1.0; // cLavaNoiseFactor
const LAVA_NOISE_OFFSET: f32 = 1.5; // cLavaNoiseOffset
const LAVA_NOISE_SPEED_OFFSET: f32 = 4.0; // cLavaNoiseSpeedOffsetFactor
const LAVA_SPEED_MULT: f32 = 0.4; // cLavaSpeedMult
const LAVA_SPEED_OFF: f32 = 0.0; // cLavaSpeedOff
const LAVA_SLOW_COMPRESS: f32 = 0.35; // cLavaSlowCompress
const LAVA_NORMAL_STRENGTH: f32 = 1.0; // cLavaNormalStrength
const LAVA_BRIGHTNESS: f32 = 2.5; // cLavaBrightness
const LAVA_TEMPERATURE: f32 = 0.35; // cLavaColorTemperatureFactor
const LAVA_GLOW_TINT: vec3<f32> = vec3<f32>(1.0, 0.36, 0.16); // cLavaGlowTint
const LAVA_LAVA_CLAMP: f32 = -1.5; // cLavaNoiseLavaClamp
const LAVA_STONE_CLAMP: f32 = 1.0; // cLavaNoiseStoneClamp
const LAVA_STONE_ALBEDO: f32 = 0.056; // cLavaStoneAlbedo
const LAVA_GLOSS: f32 = 0.8; // cLavaGloss
const LAVA_STONE_GLOSS: f32 = 0.15; // cLavaStoneGloss
// The editor's own: seconds the pattern is carried before it starts over,
// and the seconds either side of a restart a second copy half a period on
// fades in over it; only then is the pattern worked out twice. The game
// carries it as far as its render timer runs, but faster and slower lava
// then shear one blended pattern apart; 40 s keeps the shear within about
// a feature's width a metre across the stream.
const LAVA_PERIOD: f32 = 40.0;
const LAVA_RESTART: f32 = 10.0;

fn linear_step(a: f32, b: f32, x: f32) -> f32 {
    return clamp((x - a) / (b - a), 0.0, 1.0);
}

fn mod289_3(x: vec3<f32>) -> vec3<f32> { return x - floor(x * (1.0 / 289.0)) * 289.0; }
fn mod289_4(x: vec4<f32>) -> vec4<f32> { return x - floor(x * (1.0 / 289.0)) * 289.0; }
fn permute4(x: vec4<f32>) -> vec4<f32> { return mod289_4((x * 34.0 + 10.0) * x); }

// SNoiseGrad (functions_noise.fxh, Ashima Arts' simplex noise): the noise
// at `v` in w, its gradient in xyz.
fn snoise_grad(v: vec3<f32>) -> vec4<f32> {
    let c = vec2<f32>(1.0 / 6.0, 1.0 / 3.0);
    var i = floor(v + dot(v, c.yyy));
    let x0 = v - i + dot(i, c.xxx);
    let g = step(x0.yzx, x0.xyz);
    let l = 1.0 - g;
    let i1 = min(g, l.zxy);
    let i2 = max(g, l.zxy);
    let x1 = x0 - i1 + c.xxx;
    let x2 = x0 - i2 + c.yyy;
    let x3 = x0 - 0.5;
    i = mod289_3(i);
    let p = permute4(permute4(permute4(i.z + vec4<f32>(0.0, i1.z, i2.z, 1.0))
        + i.y + vec4<f32>(0.0, i1.y, i2.y, 1.0))
        + i.x + vec4<f32>(0.0, i1.x, i2.x, 1.0));
    // Gradients: 7x7 points over a square, mapped onto an octahedron.
    let ns = vec3<f32>(2.0 / 7.0, 0.5 / 7.0 - 1.0, 1.0 / 7.0);
    let j = p - 49.0 * floor(p * ns.z * ns.z);
    let x_ = floor(j * ns.z);
    let y_ = floor(j - 7.0 * x_);
    let x = x_ * ns.x + ns.yyyy;
    let y = y_ * ns.x + ns.yyyy;
    let h = 1.0 - abs(x) - abs(y);
    let b0 = vec4<f32>(x.xy, y.xy);
    let b1 = vec4<f32>(x.zw, y.zw);
    let s0 = floor(b0) * 2.0 + 1.0;
    let s1 = floor(b1) * 2.0 + 1.0;
    let sh = -step(h, vec4<f32>(0.0));
    let a0 = b0.xzyw + s0.xzyw * sh.xxyy;
    let a1 = b1.xzyw + s1.xzyw * sh.zzww;
    var p0 = vec3<f32>(a0.xy, h.x);
    var p1 = vec3<f32>(a0.zw, h.y);
    var p2 = vec3<f32>(a1.xy, h.z);
    var p3 = vec3<f32>(a1.zw, h.w);
    let norm = 1.79284291400159 - 0.85373472095314 * vec4<f32>(dot(p0, p0), dot(p1, p1), dot(p2, p2), dot(p3, p3));
    p0 *= norm.x;
    p1 *= norm.y;
    p2 *= norm.z;
    p3 *= norm.w;
    let m = max(0.6 - vec4<f32>(dot(x0, x0), dot(x1, x1), dot(x2, x2), dot(x3, x3)), vec4<f32>(0.0));
    let m2 = m * m;
    let m4 = m2 * m2;
    let pdotx = vec4<f32>(dot(p0, x0), dot(p1, x1), dot(p2, x2), dot(p3, x3));
    let t = m2 * m * pdotx;
    var grad = -8.0 * (t.x * x0 + t.y * x1 + t.z * x2 + t.w * x3);
    grad += m4.x * p0 + m4.y * p1 + m4.z * p2 + m4.w * p3;
    return 42.0 * vec4<f32>(grad, dot(m4, pdotx));
}

// FbmGrad (Blender's fbm): `detail` octaves of snoise_grad, each twice the
// frequency and LAVA_NOISE_ROUGHNESS the amplitude of the last, the
// fraction past a whole octave blending in the next. Not normalized.
fn lava_fbm(pos: vec3<f32>, detail: f32) -> vec4<f32> {
    var sum = vec4<f32>(0.0);
    var amp = 1.0;
    var freq = 1.0;
    let whole = min(u32(detail), 10u);
    for (var k = 0u; k <= whole; k++) {
        let n = amp * snoise_grad(freq * pos);
        sum += vec4<f32>(freq * n.xyz, n.w);
        amp *= LAVA_NOISE_ROUGHNESS;
        freq *= 2.0;
    }
    let rest = detail - f32(whole);
    if detail < 10.0 && rest > 0.0 {
        let n = amp * snoise_grad(freq * pos);
        sum += rest * vec4<f32>(freq * n.xyz, n.w);
    }
    return sum;
}

// LavaNoiseGrad's inner one: the pattern flat over the ground at `at`,
// pushed about by a coarser noise as far as the lava runs fast.
fn lava_pattern(at: vec3<f32>, detail: f32, distort: f32) -> vec4<f32> {
    var pos = LAVA_NOISE_SCALE * at;
    pos.y = 0.0;
    pos += distort * LAVA_NOISE_DISTORTION * snoise_grad(pos + vec3<f32>(-0.2, 0.935, 1.56)).w;
    return lava_fbm(pos, detail);
}

// LavaNoiseGrad: the pattern carried down the flow, slow lava moving
// nearly as fast as the rest (cLavaSlowCompress), and the noise turned into
// how cold the lava is: higher is crust, and fast lava runs hotter. Octaves
// until the finest is two pixels across (finer ones only shimmer), at most
// the game's close-up count.
// The game works the pattern out for each of the four flow texels around
// the pixel and blends them, which costs four times over (3 ms more on a
// 5K frame of lava); one pattern at the blended flow differs only where
// neighbouring texels flow apart. Left out with it: where slow lava's
// pattern bunches up down the stream and bends back toward the banks
// (cLavaSlowCompress x 5 x TexCoord.x, cLavaSlowBend 0.82 x 100 x
// (shore - 0.5)^2). Those shift slow texels' patterns metres apart: the
// game's blend of four patterns hides the shear that makes, one blended
// pattern turns it into hair along the banks.
fn lava_noise(world: vec3<f32>, flow: RiverFlow) -> vec4<f32> {
    let speed = clamp(flow.speed + LAVA_SPEED_OFF, 0.0, 1.0);
    let slow = clamp(1.0 - 2.0 * (flow.speed + LAVA_SPEED_OFF), 0.0, 1.0);
    let rate = LAVA_SPEED_MULT * mix(speed, 1.0, clamp(slow * LAVA_SLOW_COMPRESS, 0.0, 1.0));
    let l = length(flow.dir);
    let dir = select(vec3<f32>(0.0), vec3<f32>(flow.dir.x, 0.0, flow.dir.y) / max(l, 1e-6), l > 1e-6);
    let footprint = max(length(fwidth(LAVA_NOISE_SCALE * world.xz)), 1e-6);
    let detail = clamp(log2(0.5 / footprint), 1.0, LAVA_NOISE_DETAIL);
    let t = inland.size.z / LAVA_PERIOD;
    let phases = fract(vec2<f32>(t, t + 0.5));
    // The copy about to start over fades out to the one half a period on;
    // the blend keeps the pattern's contrast, which a plain average halves.
    let edge = LAVA_RESTART / LAVA_PERIOD;
    let wa = min(linear_step(0.0, edge, phases.x), linear_step(1.0, 1.0 - edge, phases.x));
    let wb = 1.0 - wa;
    var n = vec4<f32>(0.0);
    // Both weights depend on the clock alone, so every pixel takes the same
    // branch.
    if wa > 0.0 {
        n += wa * lava_pattern(world - rate * LAVA_PERIOD * phases.x * dir, detail, flow.speed);
    }
    if wb > 0.0 {
        n += wb * lava_pattern(world - rate * LAVA_PERIOD * phases.y * dir + vec3<f32>(37.0, 0.0, 91.0), detail, flow.speed);
    }
    n = -LAVA_NOISE_FACTOR * n / sqrt(wa * wa + wb * wb);
    n.w += LAVA_NOISE_OFFSET - LAVA_NOISE_SPEED_OFFSET * speed;
    return n;
}

// Blackbody (functions.fxh): Unity's fit of a blackbody's colour at `t`
// kelvin, darkening below 1000.
fn blackbody(t0: f32) -> vec3<f32> {
    let t = max(t0, 1.0);
    let p = pow(t, -1.5);
    let l = log(t);
    var c = vec3<f32>(220000.0 * p + 0.5804, 0.3923 * l - 2.4431, 0.7615 * l - 5.681);
    if t > 6500.0 { c.g = 138039.0 * p + 0.738; }
    c = clamp(c, vec3<f32>(0.0), vec3<f32>(1.0));
    if t < 1000.0 { c *= t / 1000.0; }
    return c;
}

// The crust as PbrLighting lights it with reflections, ambient occlusion
// and global illumination off: the sky's diffuse light and the sun's GGX.
fn lava_rock(world: vec3<f32>, n: vec3<f32>, albedo: f32, gloss: f32) -> vec3<f32> {
    let v = normalize(ocean_eye(world) - world);
    let sun = ocean_sun();
    let ndl = clamp(dot(n, sun), 0.0, 1.0);
    var color = albedo * sky_light(n) * lights.tone.z;
    if ndl > 0.0 {
        let rough = clamp(1.0 - gloss, 0.04, 1.0);
        let h = normalize(v + sun);
        let a = rough * rough;
        let a2 = a * a;
        let ndh = clamp(dot(n, h), 0.0, 1.0);
        let d = a2 / (3.14159265 * pow(ndh * ndh * (a2 - 1.0) + 1.0, 2.0));
        let k = a * 0.5;
        let ndv = max(dot(n, v), 1e-4);
        let vis = 0.25 / ((ndl * (1.0 - k) + k) * (ndv * (1.0 - k) + k));
        let f = 0.04 + 0.96 * pow(1.0 - clamp(dot(v, h), 0.0, 1.0), 5.0);
        color += lights.sun_color.rgb * ndl * shadow(world, n) * (albedo + d * vis * f);
    }
    return color;
}

// A lava river's strip vertex (core::water::StripVertex).
struct LavaVertex {
    @location(0) position: vec3<f32>,
    @location(1) look: u32,
    // Up from the strip as it slopes down the river.
    @location(2) normal: vec3<f32>,
    // vsRiver's Alpha.
    @location(3) fade: f32,
    // Across the strip, -0.5 to 0.5 bank to bank.
    @location(4) across: f32,
}

struct LavaVaryings {
    @builtin(position) clip: vec4<f32>,
    @location(0) world: vec3<f32>,
    @location(1) @interpolate(flat) look: u32,
    @location(2) fade: f32,
}

// dsRiverLava's bow: the strip raised a quarter unit mid-stream and sunk
// three quarters at its banks, where the ground takes it, along its
// normal, and flat where its ends fade (the vertex Alpha).
@vertex fn vs_lava(v: LavaVertex) -> LavaVaryings {
    var out: LavaVaryings;
    let bow = (0.25 - pow(2.0 * v.across, 2.0)) * v.fade;
    out.world = v.position + bow * v.normal + vec3<f32>(0.0, inland.season.x, 0.0);
    out.look = v.look;
    out.fade = v.fade;
    out.clip = globals.clip * vec4<f32>(out.world, 1.0);
    return out;
}

@fragment fn fs_lava(in: LavaVaryings) -> @location(0) vec4<f32> {
    let look = i_looks[in.look];
    let world = in.world;
    let flow = river_flow(world.xz, 1.0);
    let noise = lava_noise(world, flow);
    let cold = linear_step(LAVA_LAVA_CLAMP, LAVA_STONE_CLAMP, noise.w);
    let permanent = look.wave.z > 0.5;
    let glow = clamp(flow.speed - 0.25, 0.0, 1.0) * LAVA_BRIGHTNESS
        * blackbody(LAVA_TEMPERATURE * (1.0 - cold) * 5000.0) * LAVA_GLOW_TINT
        * pow(linear_step(0.75, 0.5, cold), 2.0);
    let n = normalize(vec3<f32>(-noise.x, 1.0 / (0.001 + LAVA_NORMAL_STRENGTH), -noise.z));
    // Lava that never stops flowing has black, nearly matte crust.
    let albedo = select(LAVA_STONE_ALBEDO, 0.001, permanent);
    let gloss = select(mix(LAVA_GLOSS, LAVA_STONE_GLOSS, cold), 0.05, permanent);
    let color = mix(glow, lava_rock(world, n, albedo, gloss), cold);
    // Over the opaque scene the Blend stage multiplies the occlusion over
    // (frame.wgsl occlusion_factor): taken off here first.
    return vec4<f32>(fog(color, world) / occlusion_factor(vec2<i32>(in.clip.xy)), in.fade);
}

// --- Physical waves -----------------------------------------------------------
// Anno 117's camera-local wave sim (water_physical_waves.fx csPhysicalWaves)
// in a square of texels around the camera's focus: a damped wave equation
// that the coast feeds rings of waves into, and the foam the FFT crests,
// steep wave fronts and the shore leave, fading every step. One texel is
// (h, h last step, crest foam, wave foam); gpu::ocean steps it at 60 Hz.

struct PhysicalStep {
    // Square's corner X and Z, its side in world units, 1 / texels a side.
    area: vec4<f32>,
    // Texel offset into the last step's map (x <= -5000 drops it), wave
    // seconds; w: 1 with the game's noise map.
    step: vec4<f32>,
}

// The square as stepped last: read by the step, drawn by the ocean.
@group(1) @binding(13) var o_physical: texture_2d<f32>;
@group(1) @binding(14) var o_noise: texture_2d<f32>;
@group(1) @binding(15) var<uniform> physical: PhysicalStep;
@group(1) @binding(16) var p_next: texture_storage_2d<rgba16float, write>;

// cNoiseTex2's green channel (noise01), or value noise without it.
fn noise2(uv: vec2<f32>) -> f32 {
    if physical.step.w > 0.5 { return textureSampleLevel(o_noise, o_wrap, uv, 0.0).g; }
    return tile_noise(uv * 128.0, 128.0);
}

fn physical_at(p: vec2<i32>) -> vec4<f32> {
    let size = vec2<i32>(textureDimensions(o_physical));
    if physical.step.x <= -5000.0 || any(p < vec2<i32>(0)) || any(p >= size) { return vec4<f32>(0.0); }
    return textureLoad(o_physical, p, 0);
}

@compute @workgroup_size(8, 8)
fn cs_physical(@builtin(global_invocation_id) id: vec3<u32>) {
    let n = textureDimensions(p_next);
    if id.x >= n.x || id.y >= n.y { return; }
    let uv = (vec2<f32>(id.xy) + 0.5) * physical.area.w;
    let texel = vec2<i32>(id.xy) + vec2<i32>(physical.step.xy);
    let rest = physical.area.xy + uv * physical.area.z;
    let time = physical.step.z;
    // WaterInfo and the FFT waves here, at mip 1, with no distance fade.
    let coast = coast_at(rest);
    let info = water_info(rest);
    let f = band_factors(coast.x, 0.0, info.x);
    var disp = vec3<f32>(0.0);
    var crests = 0.0;
    let crest_weights = array<f32, 4>(1.0, 1.0, 0.5, 0.25);
    for (var s = 0; s < 8; s++) {
        if f[s] > 0.0 {
            let b = band_uv(rest, s % 4);
            let waves = textureSampleLevel(o_waves, o_wrap, b, s, 1.0);
            disp += f[s] * waves.xyz;
            crests += f[s] * crest_weights[s % 4] * waves.w;
        }
    }
    disp *= 0.25;
    let world = vec3<f32>(rest.x, 0.0, rest.y) + disp;
    // The ground (SampleTerrainHeightMap) from WaterInfos' depth, which the
    // bake stores as (depth / 5)^1.5; land and depths past 5 read 0 and 5.
    let ground = select(-coast.y * ocean.shore.x, -5.0 * pow(info.x, 2.0 / 3.0), ocean.info.z > 0.5);
    let coast_height = pow(1.0 - clamp(-ground / 2.0, 0.0, 1.0), 6.0) * info.z;
    // The wave equation, stiffer (slower) on beaches.
    let wave = physical_at(texel);
    let s = physical_at(texel + vec2<i32>(0, -1)).x;
    let w = physical_at(texel + vec2<i32>(-1, 0)).x;
    let e = physical_at(texel + vec2<i32>(1, 0)).x;
    let nn = physical_at(texel + vec2<i32>(0, 1)).x;
    let acceleration = 64.0 * (s + w + e + nn - 4.0 * wave.x);
    let dt = 1.0 / (16.0 + coast_height * 64.0);
    var next = vec2<f32>(1.99 * wave.x - 0.99 * wave.y + 0.5 * acceleration * dt * dt, wave.x);
    // Foam fades each step and snaps off when faint.
    var foam = wave.zw;
    foam.x = select(foam.x * 0.95, 0.0, foam.x < 0.06);
    foam.y = select(foam.y * 0.915, 0.0, foam.y < 0.06);
    let choppy = (vec2<f32>(e, nn) - wave.x) * 0.25;
    let border = clamp(((1.0 - max(abs(uv.x * 2.0 - 1.0), abs(uv.y * 2.0 - 1.0))) * 0.5 * physical.area.z) / 8.0, 0.0, 1.0);
    let noise = noise2(world.xz * 0.045 - time * 0.057);
    let sdf = coast.x;
    let crest_foam = crests * 0.055 * info.x * info.x * info.x - noise * 0.025;
    foam.x += crest_foam;
    foam.y += pow(clamp(next.x * 0.65 - 0.15, 0.0, 1.0), 3.0) * 0.05;
    foam.y += pow((abs(choppy.x) + abs(choppy.y)) * 7.0, 3.0) * 6.5;
    // Where waves meet the ground: the game compares view depths on
    // screen; the editor the ground under the water, which is the same
    // seen from above and also holds off screen.
    let delta = abs(ground - world.y);
    if delta < 1.0 {
        let coast_fade = 1.0 - clamp(sdf / 2.0, 0.0, 1.0);
        foam.x += 0.45 * (1.0 - delta) * crests * coast_fade;
    }
    if next.x > 0.05 { next.x *= 1.0 + 0.02 * clamp(crest_foam * crest_foam, 0.0, 1.0); }
    // Rings of waves rolling in from the open sea, and two breaking on
    // beaches, each gated by noise that jumps every few seconds.
    let cw = world.xz * 0.018;
    var n1 = noise2(cw - floor(time * 0.3) / 0.3 * 0.4);
    n1 = clamp(n1 * n1 * n1 * 4.0, 0.0, 1.0);
    var n2 = noise2(cw * 1.2 + 0.35 - floor(time * 0.2) / 0.2 * 0.3);
    n2 = clamp(n2 * n2 * n2 * 4.0, 0.0, 1.0);
    var n3 = noise2(cw * 1.1 + 0.65 - floor(time * 0.25) / 0.25 * 0.35);
    n3 = clamp(n3 * n3 * n3 * 4.0, 0.0, 1.0);
    let beach_noise = noise2(world.xz * 0.006 + time * 0.006);
    // The rings follow WaterFlow's coast distance, a ramp of about 13
    // units, scaled by the painted coast waves: both smooth, so each ring
    // keeps parallel to the shore and rolls straight in.
    let flow_distance = coast_ramp(rest);
    let coast_dist = clamp(info.y * flow_distance, 0.0, 1.0);
    let b0 = 1.0 - clamp(-ground / 6.0 + 0.1, 0.0, 1.0);
    let beach_dist = clamp(b0 * b0, 0.0, 1.0) * info.y * info.y * clamp(1.0 - pow(flow_distance, 100.0), 0.0, 1.0);
    let p1 = 1.0 - fract(time * 0.07) * 0.95;
    let p2 = 1.0 - fract(time * 0.07 + 0.3) * 0.95;
    let p3 = 1.0 - fract(time * 0.07 + 0.6) * 0.95;
    let q2 = fract(time * 0.07 + 0.5 + beach_noise) * 0.81;
    let q1 = fract(time * 0.07 + beach_noise) * 0.81;
    let ring = 1.0 - coast_dist;
    var spawn = clamp(pow(1.0 - abs(p1 - coast_dist), 150.0) * ring * 0.6, 0.0, 1.0) * n1;
    spawn += clamp(pow(1.0 - abs(p2 - coast_dist), 150.0) * ring * 0.65, 0.0, 1.0) * n2;
    spawn += clamp(pow(1.0 - abs(p3 - coast_dist), 150.0) * ring * 0.7, 0.0, 1.0) * n3;
    spawn += clamp(pow(1.0 - abs(q1 - beach_dist), 150.0) * beach_dist * 1.2, 0.0, 1.0);
    spawn += clamp(pow(1.0 - abs(q2 - beach_dist), 150.0) * beach_dist * 1.5, 0.0, 1.0);
    next.x += spawn * 0.01 * (1.0 + disp.y * 3.0);
    next.x = clamp(next.x, -1.0, 1.0);
    // The game keeps foam in 8-bit unorm.
    foam = clamp(foam, vec2<f32>(0.0), vec2<f32>(1.0));
    next *= border;
    textureStore(p_next, vec2<i32>(id.xy), vec4<f32>(next, foam));
}
