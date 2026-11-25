// Anno 2070's open sea as the island view draws it, after the kit's frame
// prelude (gpu::FRAME_WGSL): the waves' prepare passes (watergrid.fx
// WavePosition and WaveNormal), the coast waves' simulation (waterwaves.fx
// DynamicWaves and ChoppyWaves) and the surface (waterocean.fx
// OceanSurfaceOverWater), after the frame prelude and the scene's
// scattering (scene.rs ATMOSPHERE_WGSL). World units are metres. As in
// 2070, the surface shades and fogs in gamma space and converts to linear at
// the end. Group 1 is the surface's, group 2 the prepare passes'.

// --- Shared ---------------------------------------------------------------

// World metres one tile of the displacement and normal maps spans: ps_Wave1
// steps a texel as 48/384 of a unit of its own positions, which are world
// metres (they move vertices directly), so the 384-texel maps span 48 m.
const TILE: f32 = 48.0;
const WAVE_TEXELS: i32 = 384;
const DYNAMIC_TEXELS: i32 = 512;

// The water info map at world `xz` as 2070's terrain water map holds it
// (waterwaves.fx:85-91): r land height, g coast distance (0 at the coast, 1
// far out), b steep coast, a water depth (0 at the waterline, 1 deep).
// Off the map, open deep sea.
fn info_at(t: texture_2d<f32>, s: sampler, xz: vec2<f32>, map: vec4<f32>) -> vec4<f32> {
    if map.z <= 0.0 { return vec4<f32>(0.0, 1.0, 0.0, 1.0); }
    let uv = (xz - map.xy) * map.zw;
    if any(uv < vec2<f32>(0.0)) || any(uv > vec2<f32>(1.0)) { return vec4<f32>(0.0, 1.0, 0.0, 1.0); }
    return textureSampleLevel(t, s, uv, 0.0);
}

// The water body's terms (waterocean.fx:302-345), pure so a CPU reference
// can check them (water.rs tests, `Sea::shore`). `path` is how far the view travels
// under water to what lies behind, `seabed_y` that point's height (sea level
// 0), `depth` the info map's depth there, `sun_y` the sun's height over the
// horizon as a unit vector's y. x: ViewDepth, y: LightDepth.
fn shore_terms(path: f32, seabed_y: f32, sun_y: f32, depth: f32, view_depth: f32, light_depth: f32) -> vec2<f32> {
    let view = clamp(path / max(view_depth * depth, 1e-4), 0.0, 1.0);
    let light = clamp((-seabed_y / max(sun_y, 0.05)) / max(light_depth * depth, 1e-4), 0.0, 1.0);
    return vec2<f32>(view, light);
}

// What shows through the water, in gamma space: the refracted scene
// absorbed over the light's way down and back, plus caustics, lerped toward
// the scattering colour by the view's way through the water.
fn water_body(refraction: vec3<f32>, caustics: vec3<f32>, terms: vec2<f32>, absorb: vec3<f32>, scatter: vec3<f32>, view_exponent: f32) -> vec3<f32> {
    let absorption = pow(vec3<f32>(1.0 - terms.y), absorb);
    return mix(refraction * absorption + caustics, scatter, pow(terms.x, view_exponent));
}

// cLgtAmbient and cLgtDiffuse, gamma: the frame's light, which is the
// profile's made linear (scene.rs `ambient`), so the water lights by the
// very light and scale the scene's shade() does.
fn water_ambient() -> vec3<f32> { return to_gamma(lights.sky.rgb); }
fn water_sun() -> vec3<f32> { return to_gamma(lights.sun_color.rgb); }

// functions.fxh LinearToGamma and GammaToLinear.
fn to_gamma(c: vec3<f32>) -> vec3<f32> { return pow(max(c, vec3<f32>(0.0)), vec3<f32>(1.0 / 2.2)); }
fn to_linear(c: vec3<f32>) -> vec3<f32> { return pow(max(c, vec3<f32>(0.0)), vec3<f32>(2.2)); }

// --- Surface --------------------------------------------------------------

struct OceanParams {
    // Ring grid: segments, rings, ring growth; w: wave seconds.
    grid: vec4<f32>,
    // cWaterLightAbsorptionExp; w: AboveWaterLightDepth.
    absorb: vec4<f32>,
    // cWaterScattering; w: AboveWaterViewDepth.
    scatter: vec4<f32>,
    // x: AboveWaterViewDepthExponent; y: the double refracted light's gain
    // (cUserConstant1.x).
    body: vec4<f32>,
    // The water info map's corner X and Z, 1 / its size; z 0 without one.
    info: vec4<f32>,
    // The coast waves' square: corner X and Z, 1 / its side; w 1 once it
    // has stepped.
    dynamic: vec4<f32>,
    // cWaterRipplesOffset.
    ripples: vec4<f32>,
    // cWaterCausticsOffset.
    caustics: vec4<f32>,
    // The profile's scattering, the same the scene's fog writes in.
    atmosphere: scene_atmosphere,
}

@group(1) @binding(0) var<uniform> ocean: OceanParams;
// cTexture0: the waves' displacement, xz choppy offset and y height.
@group(1) @binding(1) var o_displacement: texture_2d<f32>;
// cTexture1: the waves' normal, crest foam in alpha.
@group(1) @binding(2) var o_normal: texture_2d<f32>;
// cTexture5: the opaque scene's linear colour, RGB9E5.
@group(1) @binding(3) var o_scene: texture_2d<u32>;
// cTexture3: the water info map.
@group(1) @binding(4) var o_info: texture_2d<f32>;
// cTexture2: ripples, normal in rbg and foam in alpha.
@group(1) @binding(5) var o_ripples: texture_2d<f32>;
// cTexture7: caustics, b shallow and g deep.
@group(1) @binding(6) var o_caustics: texture_2d<f32>;
// cTexture8 and 9: the coast waves for the vertices (height, speed, choppy
// offset) and the pixels (height, foam, normal).
@group(1) @binding(7) var o_dynamic_vertex: texture_2d<f32>;
@group(1) @binding(8) var o_dynamic_pixel: texture_2d<f32>;
@group(1) @binding(9) var o_wrap: sampler;
@group(1) @binding(10) var o_clamp: sampler;

struct OceanOutput {
    @builtin(position) clip: vec4<f32>,
    @location(0) world: vec3<f32>,
    // TexCoord0: the waves' map coordinates, then the rough foam's.
    @location(1) waves: vec4<f32>,
    // TexCoord1: the two ripple layers.
    @location(2) ripples: vec4<f32>,
    // Position1.w: the water's depth, less the coast waves' lift; then the
    // coast waves' map coordinates.
    @location(3) depth: f32,
    @location(4) dynamic: vec2<f32>,
    // ScatteringVS at the vertex: extinction and inscattered light.
    @location(5) extinction: vec3<f32>,
    @location(6) inscatter: vec3<f32>,
}

fn ocean_eye() -> vec3<f32> {
    return select(globals.anchor.xyz, globals.eye.xyz, globals.eye.w > 0.5);
}

// The coast waves at world `xz` (cTexture8 or 9); still water outside their
// square or before they stepped.
fn dynamic_at(t: texture_2d<f32>, xz: vec2<f32>) -> vec4<f32> {
    return dynamic_uv(t, (xz - ocean.dynamic.xy) * ocean.dynamic.z);
}

fn dynamic_uv(t: texture_2d<f32>, uv: vec2<f32>) -> vec4<f32> {
    if ocean.dynamic.w <= 0.0 { return vec4<f32>(0.0); }
    if any(uv < vec2<f32>(0.0)) || any(uv > vec2<f32>(1.0)) { return vec4<f32>(0.0); }
    return textureSampleLevel(t, o_clamp, uv, 0.0);
}

@vertex fn vs_ocean(@builtin(vertex_index) vertex: u32) -> OceanOutput {
    // 2070 draws instanced CDLOD chunks (waterocean.fx:104-118). Here, one
    // triangle strip per band between two rings around the camera, the
    // bands joined by repeating a corner, as 117's sea in the editor: ring
    // radii grow geometrically from a first ring sized to the camera's
    // height, so cells stay a few pixels wide from near to far.
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
    let eye = ocean_eye();
    let first = max(0.25, abs(eye.y) * 0.004);
    let q = ocean.grid.z;
    let radius = first * (pow(q, ring) - 1.0) / (q - 1.0);
    let angle = segment / f32(segments) * 6.2831853;
    // Snapped to the first ring's cells so the grid does not swim.
    let centre = floor(eye.xz / (first * 4.0)) * (first * 4.0);
    var world = vec3<f32>(centre.x + radius * cos(angle), 0.0, centre.y + radius * sin(angle));
    var out: OceanOutput;
    let uv = world.xz / TILE;
    out.waves = vec4<f32>(uv, world.xz * 0.015 + ocean.grid.w * 0.005);
    var depth = info_at(o_info, o_clamp, world.xz, ocean.info).a;
    var wave = textureSampleLevel(o_displacement, o_wrap, uv, 0.0).xyz * depth;
    // The coast waves lift the surface and push it about; their choppy
    // offset fades past 32 m from the camera, where the grid is too coarse
    // for it (waterocean.fx:139-157).
    let dynamic = dynamic_at(o_dynamic_vertex, world.xz);
    let l = 1.0 - clamp((length(world - eye) - 32.0) / 32.0, 0.0, 1.0) * 0.6;
    wave.y += dynamic.r;
    wave.x += dynamic.b * l;
    wave.z += dynamic.a * l;
    depth = clamp(depth - dynamic.r * 0.5, 0.0, 1.0);
    out.dynamic = (world.xz - ocean.dynamic.xy) * ocean.dynamic.z;
    world += wave;
    out.depth = clamp(depth - clamp(dynamic.r * 0.5, 0.0, 1.0), 0.0, 1.0);
    out.ripples = world.xzxz * vec4<f32>(0.3, 0.3, 0.15, -0.15) + ocean.ripples - wave.xzxz * 0.25;
    out.world = world;
    out.clip = globals.clip * vec4<f32>(world, 1.0);
    // 2070 scatters per vertex in its forward passes (waterocean.fx:193-196).
    let air = scene_view_scatter(ocean.atmosphere, world);
    out.extinction = air.extinction;
    out.inscatter = air.inscatter;
    return out;
}

// ComputeCaustics: two planar lookups moving against each other, the deep
// layer taking over by 20 m down (waterocean.fx:217-238).
fn ocean_caustics(p: vec3<f32>, light_depth: f32, o: vec2<f32>) -> vec3<f32> {
    let coords = (p.xzxz - p.yyyy * 0.1) * vec4<f32>(0.22, 0.22, -0.14, -0.14) + ocean.caustics;
    let c0 = textureSample(o_caustics, o_wrap, coords.xy + o).rgb;
    let c1 = textureSample(o_caustics, o_wrap, coords.zw + o).rgb;
    let b = clamp(p.y / -20.0, 0.0, 1.0);
    return vec3<f32>(6.2, 6.2, 4.2) * mix(c0.b, c0.g, b) * mix(c1.b, c1.g, b) * light_depth;
}

// The opaque depth at pixel `p`, reversed (0 where nothing drew).
fn ocean_depth(p: vec2<i32>) -> f32 {
    let last = vec2<i32>(textureDimensions(t_depth)) - 1;
    return textureLoad(t_depth, clamp(p, vec2<i32>(0), last), 0);
}

@fragment fn fs_ocean(in: OceanOutput) -> @location(0) vec4<f32> {
    let world = in.world;
    let eye = ocean_eye();
    var to_point = normalize(world - eye);
    if globals.eye.w < 0.5 { to_point = -globals.eye.xyz; }
    // 1. The waves' normal and the ripples' (ComputeRipples: .rbga, and
    // the two layers summed without the factor of two).
    let normal_map = textureSample(o_normal, o_wrap, in.waves.xy);
    var n = normal_map.rgb * 2.0 - 1.0;
    var foam = normal_map.a;
    let r0 = textureSample(o_ripples, o_wrap, in.ripples.xy).rbga;
    let r1 = textureSample(o_ripples, o_wrap, in.ripples.zw).rbga;
    let ripples = (r0.xyz - 0.5) + (r1.xyz - 0.5);
    let foam_detail = r0.w * r1.w;
    // 2. The coast waves bend the normal, and the ripples ride on it.
    let dynamic = dynamic_uv(o_dynamic_pixel, in.dynamic);
    n = n + normalize(vec3<f32>(dynamic.z, 0.04, dynamic.w)) * dynamic.r;
    let tangent = vec3<f32>(1.0, 0.0, 0.0);
    let bitangent = -cross(n, tangent);
    n = normalize(n * ripples.y * 1.5 + tangent * ripples.x + bitangent * ripples.z);
    // 3-4. The reflected view, and the sky toward it.
    let reflected = reflect(to_point, n);
    // SkyCubeMap mirrors the sky at the horizon (sky.fx:159-182).
    let mirrored = vec3<f32>(reflected.x, abs(reflected.y), reflected.z);
    let px = in.clip.xy;
    let size = vec2<f32>(textureDimensions(o_scene));
    // The scene's mirror at sea level, offset by the normal, lerped over the
    // sky by its mask where the wave faces up (waterocean.fx:287-296). The
    // mirror holds the scene's own colours, already fogged; without one it
    // reads 0 and the sky stands alone.
    let planar = mirror_reflection(px + n.xz * 0.08 * size);
    let sky_and_mirror = mix(
        to_gamma(environment(normalize(mirrored))),
        to_gamma(planar.rgb),
        planar.a * pow(max(n.y, 0.0), 64.0),
    );
    // 5. Refraction: offset by the normal, less from high up, and taken
    // straight where the offset lands on something in front of the water.
    let shift = n.xz * 0.03 * clamp(1.0 - eye.y / 100.0, 0.0, 1.0);
    var refract_px = vec2<i32>(px + shift * size);
    var z = ocean_depth(refract_px);
    if z > in.clip.z {
        refract_px = vec2<i32>(px);
        z = ocean_depth(refract_px);
    }
    // 5.1 What lies behind, and how far the view and the light travel
    // through water to it. 2070 differences view-space depths; here the
    // distances along the view, which agree at the screen's centre. Where
    // nothing opaque drew, the floor lies far out of reach.
    var seabed = vec3<f32>(world.x, -1000.0, world.z);
    var path = 1e4;
    if z > 0.0 {
        let uv = (vec2<f32>(refract_px) + 0.5) / size;
        seabed = unproject(vec2<f32>(uv.x * 2.0 - 1.0, 1.0 - uv.y * 2.0), z);
        if globals.eye.w > 0.5 {
            path = max(distance(eye, seabed) - distance(eye, world), 0.0);
        } else {
            path = max(dot(world - seabed, globals.eye.xyz), 0.0);
        }
    }
    let sun = normalize(lights.sun.xyz);
    let terms = shore_terms(path, seabed.y, sun.y, in.depth, ocean.scatter.w, ocean.absorb.w);
    let caustics = ocean_caustics(seabed, terms.y, n.xz * 0.05);
    let last = vec2<i32>(size) - 1;
    let behind = to_gamma(unpack_rgb9e5(textureLoad(o_scene, clamp(refract_px, vec2<i32>(0), last), 0).r));
    let refraction = water_body(behind, caustics, terms, ocean.absorb.rgb, ocean.scatter.rgb, ocean.body.x);
    // 6. Foam: the crests' over the rough foam, and the coast waves'.
    foam *= textureSample(o_ripples, o_wrap, in.waves.zw).a;
    let spec_factor = 0.7 * clamp(1.0 - 2.0 * dynamic.r, 0.0, 1.0);
    foam = (foam * 3.0 + dynamic.g) * foam_detail;
    // 7. Fresnel at n = 1.85, scaled by how much water lies behind.
    let r_0 = (0.85 * 0.85) / (2.85 * 2.85);
    let fresnel = clamp(r_0 + (1.0 - r_0) * pow(1.0 - clamp(dot(-to_point, n), 0.0, 1.0), 5.0), 0.0, 1.0);
    var color = mix(refraction, sky_and_mirror, fresnel * terms.x);
    // 8. Foam on top.
    color = clamp(color + min(foam, 0.45), vec3<f32>(0.0), vec3<f32>(1.0));
    // 9. Light: 2070 multiplies by the profile's light over the water's
    // grey, and adds a tight sun glint.
    let shade = shadow(world, vec3<f32>(0.0, 1.0, 0.0));
    let diffuse = clamp(dot(n, sun), 0.0, 1.0);
    let glint = pow(clamp(dot(sun, reflected), 0.0, 1.0), 200.0) * spec_factor;
    let lighting = mix(water_ambient(), water_sun(), diffuse * shade);
    color = color * clamp((1.0 - terms.x) * shade + dot(lighting, vec3<f32>(0.33)), 0.0, 1.0) + glint;
    // 10. Light refracted twice through the waves' faces, toward a camera
    // looking along the sea.
    let fake = normalize(vec3<f32>(to_point.x, to_point.y * 0.05, to_point.z));
    color += vec3<f32>(0.3, 0.7, 0.6) * 0.35 * clamp(dot(n, -fake), 0.0, 1.0) * ocean.body.y;
    // 11. The atmosphere's scattering, the scene's own (ScatteringPS),
    // then linear. Over the opaque scene the Blend stage multiplies the
    // occlusion back in (frame.wgsl occlusion_factor): taken off here first.
    color = scene_scattered(color, scene_scattering(in.extinction, in.inscatter));
    return vec4<f32>(to_linear(color) / occlusion_factor(vec2<i32>(in.clip.xy)), 1.0);
}

// --- Prepare passes ---------------------------------------------------------

struct Step {
    // cWaterVelocity0 and 1: where the two layers of each wave texture lie.
    velocity0: vec4<f32>,
    velocity1: vec4<f32>,
    // cWaterAmplitude xy, cWaterChoppiness x, cWaterFrequency x.
    waves: vec4<f32>,
    // The coast waves' square: corner X and Z, side; w the noise's clock
    // (cWaterDynamicWave2.z).
    dynamic: vec4<f32>,
    // The texels the square moved since the last step (cWaterDynamicWave1.xy,
    // in texels); z 1 starts from still water.
    scroll: vec4<f32>,
    // The water info map's corner X and Z, 1 / its size; z 0 without one.
    info: vec4<f32>,
}

@group(2) @binding(0) var<uniform> prep: Step;
@group(2) @binding(1) var w_great: texture_2d<f32>;
@group(2) @binding(2) var w_medium: texture_2d<f32>;
@group(2) @binding(3) var w_wrap: sampler;
@group(2) @binding(4) var w_displacement_out: texture_storage_2d<rgba16float, write>;
@group(2) @binding(5) var w_displacement: texture_2d<f32>;
@group(2) @binding(6) var w_normal_out: texture_storage_2d<rgba16float, write>;
@group(2) @binding(7) var d_state: texture_2d<f32>;
@group(2) @binding(8) var d_state_out: texture_storage_2d<rgba16float, write>;
@group(2) @binding(9) var d_foam_out: texture_storage_2d<rgba16float, write>;
@group(2) @binding(10) var d_info: texture_2d<f32>;
@group(2) @binding(11) var d_coast_noise: texture_2d<f32>;
@group(2) @binding(12) var d_foam: texture_2d<f32>;
@group(2) @binding(13) var d_vertex_out: texture_storage_2d<rgba16float, write>;
@group(2) @binding(14) var d_pixel_out: texture_storage_2d<rgba16float, write>;
@group(2) @binding(15) var d_clamp: sampler;

// GetWaveOffset: the great and medium waves' heights at `xz` (the map's
// coordinates) moved by `offset`. GetWave is the same with the great
// waves' frequency, which the profiles all leave at 1.
fn wave_heights(xz: vec2<f32>, offset: vec4<f32>, frequency: f32) -> vec2<f32> {
    let g = (xz + offset.xy) * frequency;
    let m = xz + offset.zw;
    let g0 = textureSampleLevel(w_great, w_wrap, g + prep.velocity0.xy, 0.0).r;
    let g1 = textureSampleLevel(w_great, w_wrap, g + prep.velocity0.zw, 0.0).r;
    let m0 = textureSampleLevel(w_medium, w_wrap, m + prep.velocity1.xy, 0.0).r;
    let m1 = textureSampleLevel(w_medium, w_wrap, m + prep.velocity1.zw, 0.0).r;
    return vec2<f32>((g0 + g1 - 1.0) * prep.waves.x, (m0 + m1 - 1.0) * prep.waves.y);
}

// ps_Wave0: height plus a choppy offset from the slope toward two
// neighbours, raised by GlobalWaveHeightOffset (0.1). cWaterChoppiness.y,
// half the highest wave, is taken as 0: both layers already centre on 0.
@compute @workgroup_size(8, 8) fn cs_wave_position(@builtin(global_invocation_id) id: vec3<u32>) {
    if any(id.xy >= vec2<u32>(u32(WAVE_TEXELS))) { return; }
    let xz = (vec2<f32>(id.xy) + 0.5) / f32(WAVE_TEXELS);
    let here = wave_heights(xz, vec4<f32>(0.0), prep.waves.w);
    let along_x = wave_heights(xz, vec4<f32>(0.01, 0.0, 0.005, 0.0), 1.0);
    let along_z = wave_heights(xz, vec4<f32>(0.0, 0.01, 0.0, 0.005), 1.0);
    let height = here.x + here.y;
    let chop = (vec2<f32>(along_x.x + along_x.y, along_z.x + along_z.y) - height) * prep.waves.z;
    textureStore(w_displacement_out, id.xy, vec4<f32>(chop.x, height + 0.1, chop.y, 1.0));
}

fn displacement_texel(p: vec2<i32>) -> vec4<f32> {
    return textureLoad(w_displacement, (p % WAVE_TEXELS + WAVE_TEXELS) % WAVE_TEXELS, 0);
}

// ps_Wave1 (its Direct3D 10 path, no half texel): the normal across the
// displaced grid, and foam where the crest stands over its neighbours 16
// texels on and the grid bunches up.
@compute @workgroup_size(8, 8) fn cs_wave_normal(@builtin(global_invocation_id) id: vec3<u32>) {
    if any(id.xy >= vec2<u32>(u32(WAVE_TEXELS))) { return; }
    let p = vec2<i32>(id.xy);
    let grid = TILE / f32(WAVE_TEXELS);
    let w00 = displacement_texel(p);
    var w10 = displacement_texel(p + vec2<i32>(1, 0));
    var w01 = displacement_texel(p + vec2<i32>(0, 1));
    let v00 = w00.xyz;
    let v10 = vec3<f32>(w10.x + grid, w10.y, w10.z);
    let v01 = vec3<f32>(w01.x, w01.y, w01.z + grid);
    var n = cross(v01 - v00, v10 - v00);
    n.y = max(0.001, n.y);
    let foam0 = pow(1.0 - clamp(5.0 * (v10.x - v00.x), 0.0, 1.0), 3.0);
    let foam1 = pow(1.0 - clamp(5.0 * (v01.z - v00.z), 0.0, 1.0), 3.0);
    w10 = displacement_texel(p + vec2<i32>(16, 0));
    w01 = displacement_texel(p + vec2<i32>(0, 16));
    let foam = clamp(foam0 + foam1, 0.0, 1.0) * clamp(w00.y - max(w10.y, w01.y), 0.0, 1.0) * prep.waves.x * 4.0;
    textureStore(w_normal_out, id.xy, vec4<f32>(normalize(n) * 0.5 + 0.5, foam));
}

fn state_texel(p: vec2<i32>) -> vec4<f32> {
    if any(p < vec2<i32>(0)) || any(p >= vec2<i32>(DYNAMIC_TEXELS)) || prep.scroll.z > 0.5 { return vec4<f32>(0.0); }
    return textureLoad(d_state, p, 0);
}

fn dynamic_world(p: vec2<f32>) -> vec2<f32> {
    return prep.dynamic.xy + p / f32(DYNAMIC_TEXELS) * prep.dynamic.z;
}

fn dynamic_info(world: vec2<f32>) -> vec4<f32> {
    return info_at(d_info, d_clamp, world, prep.info);
}

// ps_DynamicWaves: one step of the wave equation over the square, slower
// toward the coast, damped; then new waves running in to the coast where
// the ocean's great waves are. Left out: waves objects in the water make
// (from the G-buffer's depth and flags; the editor has no ships) and the
// steep coasts' waves, which read a noise volume not found among the
// shipped textures.
// TODO: steep coast waves once the noise volume is known.
@compute @workgroup_size(8, 8) fn cs_dynamic_step(@builtin(global_invocation_id) id: vec3<u32>) {
    if any(id.xy >= vec2<u32>(u32(DYNAMIC_TEXELS))) { return; }
    let p = vec2<i32>(id.xy);
    let src = p + vec2<i32>(prep.scroll.xy);
    let world = dynamic_world(vec2<f32>(p) + 0.5);
    let info = dynamic_info(world);
    let steep = info.b;
    let coast = info.g;
    let land = clamp(info.r / 0.7, 0.0, 1.0);
    let speed = coast * 0.55 + 0.45;
    var wave = state_texel(src);
    let w0 = state_texel(src + vec2<i32>(0, -1)).r;
    let w1 = state_texel(src + vec2<i32>(-1, 0)).r;
    let w2 = state_texel(src + vec2<i32>(1, 0)).r;
    let w3 = state_texel(src + vec2<i32>(0, 1)).r;
    let f = w0 + w1 + w2 + w3 - 4.0 * wave.r;
    wave.y += 0.021 * f * speed;
    wave.x += 0.990 * wave.y * speed;
    wave *= 0.9975;
    wave.z = 0.25 * (w0 + w1 - w2 - w3);
    wave.w = 0.25 * (w0 - w1 + w2 - w3);
    let old = wave.x;
    let timer = prep.dynamic.w;
    // 3.1-3.4: noise bands running in to the coast, strongest a little
    // off it, and only under the ocean's great waves.
    let noise_uv = vec2<f32>((world.x + world.y) * 0.02, coast * 0.2 + timer * 0.013);
    let coast_waves = textureSampleLevel(d_coast_noise, w_wrap, noise_uv, 0.0).r * 2.0 - 1.0;
    let shallow = pow(clamp(coast * 12.0, 0.0, 1.0), 1.5) * clamp(1.0 - coast * coast, 0.0, 1.0);
    let great = clamp(4.0 * (textureSampleLevel(w_displacement, w_wrap, world / TILE, 0.0).y + 0.45), 0.0, 1.0);
    wave.r += shallow * shallow * coast_waves * 0.028 * great * (1.0 - steep * 0.5);
    // No waves at the square's edge (8 texels), and heights held to ±0.5.
    let edge = abs((vec2<f32>(p) + 0.5) / f32(DYNAMIC_TEXELS) * 2.0 - 1.0);
    let inside = (1.0 - max(edge.x, edge.y)) * 0.5 * f32(DYNAMIC_TEXELS);
    let border = clamp(inside / 8.0, 0.0, 1.0);
    wave.r *= border;
    wave.g *= border;
    wave.r = clamp(wave.r, -0.5, 0.5);
    wave.g = clamp(wave.g, -0.5, 0.5);
    textureStore(d_state_out, id.xy, wave);
    // Foam behind a falling wave and on high crests, more where it runs
    // onto land.
    let difference = wave.x - old;
    let border2 = clamp((inside - 8.0) / 8.0, 0.0, 1.0);
    var foam = (clamp(-difference * 8.0, 0.0, 1.0) * border2 + pow(max(wave.r, 0.0), 4.0)) * 8.0 * (1.0 + clamp(land * 3.0, 0.0, 1.0));
    foam = min(foam, 1.0 - coast * 0.4);
    textureStore(d_foam_out, id.xy, vec4<f32>(foam, 0.0, 0.0, 0.0));
}

fn stepped(p: vec2<i32>) -> f32 {
    let q = clamp(p, vec2<i32>(0), vec2<i32>(DYNAMIC_TEXELS - 1));
    return textureLoad(d_state, q, 0).r;
}

// ps_ChoppyWaves: the coast waves' choppy offset from their slope, strongest
// at 40% of the coast distance, into the vertices' map; the normal less that
// offset into the pixels' map, beside the step's foam (its target's green,
// which this pass's write mask keeps).
@compute @workgroup_size(8, 8) fn cs_choppy(@builtin(global_invocation_id) id: vec3<u32>) {
    if any(id.xy >= vec2<u32>(u32(DYNAMIC_TEXELS))) { return; }
    let p = vec2<i32>(id.xy);
    // RTS 1.1/512: a texel and a tenth; the nearest texel here.
    let s00 = textureLoad(d_state, p, 0);
    let p10 = stepped(p + vec2<i32>(1, 0));
    let p01 = stepped(p + vec2<i32>(0, 1));
    let p11 = stepped(p + vec2<i32>(1, 1));
    let n10 = stepped(p - vec2<i32>(1, 0));
    let n01 = stepped(p - vec2<i32>(0, 1));
    let n11 = stepped(p - vec2<i32>(1, 1));
    let info = dynamic_info(dynamic_world(vec2<f32>(p) + 0.5));
    var choppiness = clamp(abs(info.g - 0.4) / 0.4, 0.0, 1.0);
    choppiness = (1.0 - choppiness * choppiness) * 0.7 + 0.3;
    let r = s00.r;
    var chop = (vec2<f32>(p10, p01) - r) + (vec2<f32>(p11) - r) * 0.707 - (vec2<f32>(n10, n01) - r) - (vec2<f32>(n11) - r) * 0.707;
    chop *= choppiness * (0.46 + info.b * 0.17);
    textureStore(d_vertex_out, id.xy, vec4<f32>(s00.r, s00.g, chop));
    let foam = textureLoad(d_foam, p, 0).r;
    textureStore(d_pixel_out, id.xy, vec4<f32>(s00.r, foam, s00.zw - chop));
}
