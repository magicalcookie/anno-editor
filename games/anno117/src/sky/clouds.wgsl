// Anno 117's volumetric clouds as the view sees them (environment.fx
// RaymarchClouds over functions_clouds.fxh), marched per pixel of a map at
// a fraction of the view's resolution (clouds.rs MAP_DIVISOR), as the game
// marches its clouds below full resolution, that fs_backdrop reads above
// the horizon and fs_haze_up below it: per texel the light the clouds
// and the air in front of them send, and the clouds' transmittance. Follows the frame prelude and pass.wgsl, whose
// atmosphere it marches in front of the clouds. Lengths are world units
// unless named km.
// Engine addresses here are Anno117.exe build 24988830 (Steam).

struct Cloud {
    // where the rays start (world: the eye, at sea level at the lowest),
    // w: the dome's radius (cCloudDomeRadius)
    eye: vec4<f32>,
    // toward the light (the sun by day, the moon by night); w = 1 by day
    toward: vec4<f32>,
    // AmbientRadianceClearSH[0], 3.5 times the clear sky's mean radiance
    // (clouds.rs SH0_OVER_MEAN); w: the sun's strength, fading as it sets
    ambient: vec4<f32>,
    // cCloudAlbedo; w: cWeathermapScale
    albedo: vec4<f32>,
    // the view's clip to world (globals.unclip)
    unclip: mat4x4<f32>,
    // xy: how far the weather map has drifted (cWeatherScroll, in map
    // units); zw: the map's size in texels
    scroll: vec4<f32>,
    // x: what t_curl's snorm texels are over (clouds.rs Curl::scale); y:
    // the march's count, turning the jitter and picking its quarter of the
    // map; z: the shade's texels a side; w = 1 when the march takes every
    // texel of the map, not a quarter (no history to fill in the rest)
    curl: vec4<f32>,
    // How far the base and erosion noise (cCloudNoiseScroll) and the curl
    // noise (cCloudCurlScroll) have drifted (clouds.rs Clouds::drift)
    noise_drift: vec4<f32>,
    curl_drift: vec4<f32>,
    // The last march's world to clip and eye (w = 1 when there was one)
    last_clip: mat4x4<f32>,
    last_eye: vec4<f32>,
    // fs_cloud_shade's square: corner world X and Z, side, and the shade's
    // strength (clouds.rs Clouds::shadow_intensity)
    shade: vec4<f32>,
    // How far the clouds moved with the weather map since the last march,
    // world X and Z in x and z.
    moved: vec4<f32>,
}

@group(2) @binding(0) var<uniform> cloud: Cloud;
@group(2) @binding(1) var t_base: texture_3d<f32>;
@group(2) @binding(2) var t_erosion: texture_3d<f32>;
@group(2) @binding(3) var t_types: texture_2d<f32>;
@group(2) @binding(4) var t_weather: texture_2d<f32>;
@group(2) @binding(5) var s_wrap: sampler;
@group(2) @binding(6) var s_clamp: sampler;
@group(2) @binding(7) var t_curl: texture_3d<f32>;
// fs_clouds' group (clouds.rs resolve_layout): the map as last resolved
// and where its clouds lay (clouds.rs Map), and what fs_clouds_march just
// marched, its light and transmittance and how far its clouds lie.
@group(2) @binding(8) var t_history: texture_2d<f32>;
@group(2) @binding(9) var t_history_info: texture_2d<f32>;
@group(2) @binding(10) var t_fresh: texture_2d<f32>;
@group(2) @binding(11) var t_fresh_info: texture_2d<f32>;

// environment.fx's defines from the engine (Anno117.exe 0x1424887C0).
const LAYER_LOWER: f32 = 190.0;
const LAYER_UPPER: f32 = 370.0;
const LAYER_THICKNESS: f32 = 180.0;
// constants_clouds.fxh
const FINE_NOISE_SCALE: f32 = 4.5 / 106.0;
const CURL_NOISE_SCALE: f32 = 75.0;
// cCloudCurlIntensity: the cbClouds fill (Anno117.exe 0x142482620) copies
// the environment renderer's +0x16C, 7 from its constructor.
const CURL_INTENSITY: f32 = 7.0;
// cVolumetricMaxDistance, and SampleClouds' fade before it
// (VOLUMETRIC_FADE_DISTANCE), along the ray from where it enters the
// layer. TODO: the fill is not traced; 10000 is the environment renderer's
// constructor value (+0x2F0) just before the scatter fade's, which
// cbEnvironment lists right after it.
const VOLUMETRIC_MAX: f32 = 10000.0;
const VOLUMETRIC_FADE: f32 = 500.0;

// GetCloudParams: TypeDensity, FineNoiseBase, BaseNoiseScale,
// ErosionNoiseScale, DensityFactor per weather map type.
const TYPES = array<array<f32, 5>, 6>(
    array<f32, 5>(0.750, 0.579, 0.157 * 1024.0, 1.000 * 106.0, 0.06),
    array<f32, 5>(0.770, 0.600, 1.000 * 1024.0, 1.000 * 106.0, 0.20),
    array<f32, 5>(0.600, 0.600, 0.190 * 1024.0, 1.000 * 106.0, 0.03),
    array<f32, 5>(0.880, 0.230, 1.500 * 1024.0, 1.000 * 106.0, 0.25),
    array<f32, 5>(0.620, 0.340, 1.800 * 1024.0, 1.220 * 106.0, 0.25),
    array<f32, 5>(0.650, 0.470, 1.150 * 1024.0, 0.630 * 106.0, 0.50),
);

fn remap(x: f32, a: f32, b: f32, c: f32, d: f32) -> f32 {
    return c + (x - a) * (d - c) / (b - a);
}

// ComputeCloudDome: the layer's lower sphere through LAYER_LOWER above the
// eye, centred under it.
fn dome_inner() -> f32 {
    let r = cloud.eye.w;
    return (r * r + LAYER_LOWER * LAYER_LOWER) / (2.0 * r);
}

fn dome_centre() -> vec3<f32> {
    return vec3<f32>(cloud.eye.x, LAYER_LOWER - dome_inner(), cloud.eye.z);
}

// Where along `dir` from `o` the sphere of `radius` about the dome's
// centre is met, near and far (IntersectRaySphere); both -1 when missed.
// |oc|² − r² as (|oc| − r)(|oc| + r): squared, both are near 10^10 and
// f32 loses the few hundred units between them.
fn sphere_hits(o: vec3<f32>, dir: vec3<f32>, radius: f32) -> vec2<f32> {
    let oc = o - dome_centre();
    let b = dot(oc, dir);
    let l = length(oc);
    let t = b * b - (l - radius) * (l + radius);
    if t <= 0.0 { return vec2<f32>(-1.0); }
    let s = sqrt(t);
    return vec2<f32>(-b - s, -b + s);
}

// ComputeCloudInterval's cases: where along `dir` from the eye the march
// through the layer starts and ends, from below (CRT_BELOW), inside
// (CRT_INSIDE_UP / _DOWN, from the eye) or above it (CRT_ABOVE_DOWN). Ends
// at the sea at the latest: the game ends at the scene's depth, and below
// the clouds' base the editor draws only the sea and islands far lower.
// x > y when the ray misses the layer.
fn cloud_interval(eye: vec3<f32>, dir: vec3<f32>) -> vec2<f32> {
    let lower = sphere_hits(eye, dir, dome_inner());
    let upper = sphere_hits(eye, dir, dome_inner() + LAYER_THICKNESS);
    let none = vec2<f32>(1.0, 0.0);
    var span = none;
    if (lower.y < 0.0) != (upper.y < 0.0) {
        // Inside, looking up and out.
        span = vec2<f32>(0.0, max(lower.y, upper.y));
    } else if (lower.x < 0.0) != (upper.x < 0.0) {
        // Inside, looking down to the base.
        span = vec2<f32>(0.0, lower.x);
    } else if upper.x > 0.0 {
        // Above, looking down into it; out through the base, or the top
        // again where the ray only grazes it.
        span = vec2<f32>(upper.x, select(upper.y, lower.x, lower.x > 0.0));
    } else if lower.x == -1.0 && upper.y > 0.0 {
        span = vec2<f32>(0.0, upper.y);
    } else if lower.y > 0.0 {
        span = vec2<f32>(lower.y, upper.y);
    }
    if dir.y < 0.0 { span.y = min(span.y, eye.y / -dir.y); }
    return span;
}

fn sample_height(p: vec3<f32>) -> f32 {
    return (distance(p, dome_centre()) - dome_inner()) / LAYER_THICKNESS;
}

// The mip for a texture repeating every `tile` units with `side` texels,
// seen over `footprint` units: the map's texel is coarser than the game's
// pixel, so CloudLod's distance rule would alias the fine noise.
fn mip(footprint: f32, tile: f32, side: f32) -> f32 {
    return max(log2(footprint * side / tile), 0.0);
}

// SampleCloudDensity, detailed: the weather map's coverage and type shaping
// the base and erosion noise, `footprint` units across, the base noise
// pushed by the curl noise, most at the layer's foot, out to where CloudLod
// ((distance - 50) / 1000) reaches 3. Each noise is sampled behind where it
// drifted to, the erosion and fine noise twice as far as the base.
fn cloud_density(p: vec3<f32>, footprint: f32, dist: f32) -> f32 {
    let h = sample_height(p);
    if h < 0.0 || h > 1.0 { return 0.0; }
    // ComputeWeatherSamplePosition
    var at = p.xz / cloud.albedo.w + vec2<f32>(0.25);
    at.y = 1.0 - at.y;
    at += cloud.scroll.xy;
    let weather = textureSampleLevel(t_weather, s_wrap, at, 0.0);
    let coverage = weather.r;
    if coverage < 0.0001 { return 0.0; }
    let kind = u32(min(weather.g, 0.999) * 6.0);
    let params = TYPES[kind];
    var base_at = p - cloud.noise_drift.xyz;
    let erosion_at = p - 2.0 * cloud.noise_drift.xyz;
    if dist < 3050.0 {
        let curl_at = (p - cloud.curl_drift.xyz) / CURL_NOISE_SCALE;
        let curl = textureSampleLevel(t_curl, s_wrap, curl_at, mip(footprint, CURL_NOISE_SCALE, 32.0)).xyz;
        base_at += curl * cloud.curl.x * CURL_INTENSITY * (1.0 - h);
    }
    let base = textureSampleLevel(t_base, s_wrap, base_at / params[2], mip(footprint, params[2], 128.0)).r;
    let height_density = textureSampleLevel(t_types, s_clamp, vec2<f32>(h, (f32(kind) + 0.5) / 6.0), 0.0).r;
    var d = remap(base, 1.0 - height_density, 1.0, 0.0, 1.0);
    // The erosion only lowers d (remap to [1 - erosion, 1] of d ≤ 1), and
    // the remaps after it keep order: where d is gone without it, it is
    // gone with it, and its fetch is skipped. Most samples at a cloud's
    // edge end here: 0.66 to 0.57 ms of the map's march on
    // continental_overview at 5K, a fifth off the shade's.
    if remap(remap(d, params[0], 1.0, 0.0, 1.0), 1.0 - coverage, 1.0, 0.0, 1.0) <= 0.0 { return 0.0; }
    let erosion = textureSampleLevel(t_erosion, s_wrap, erosion_at / params[3], mip(footprint, params[3], 32.0)).r;
    d = remap(d, 1.0 - erosion, 1.0, 0.0, 1.0);
    d = remap(d, params[0], 1.0, 0.0, 1.0);
    d = remap(d, 1.0 - coverage, 1.0, 0.0, 1.0);
    if d <= 0.0 { return 0.0; }
    let fine = textureSampleLevel(t_erosion, s_wrap, erosion_at * FINE_NOISE_SCALE, mip(footprint, 1.0 / FINE_NOISE_SCALE, 32.0)).r;
    d = remap(d, params[1] - fine, 1.0, 0.0, 1.0);
    return clamp(d * params[4], 0.0, 1.0);
}

// cloud_density without its early returns: the same density, but every
// fetch issued whatever the ones before return, so the four light samples
// of a step (extinction_to_light) wait on one chain of fetches, not four.
// The march is bound by its slowest rays' latency, not by fetches: 0.29 to
// 0.25 ms on continental_overview at 5K, pirate 0.21 to 0.18. Taken for
// the march's own sample as well it cost continental_overview 0.05 ms.
fn cloud_density_flat(p: vec3<f32>, footprint: f32, dist: f32) -> f32 {
    let h = sample_height(p);
    var at = p.xz / cloud.albedo.w + vec2<f32>(0.25);
    at.y = 1.0 - at.y;
    at += cloud.scroll.xy;
    let weather = textureSampleLevel(t_weather, s_wrap, at, 0.0);
    let coverage = weather.r;
    let kind = u32(min(weather.g, 0.999) * 6.0);
    let params = TYPES[kind];
    var base_at = p - cloud.noise_drift.xyz;
    let erosion_at = p - 2.0 * cloud.noise_drift.xyz;
    if dist < 3050.0 {
        let curl_at = (p - cloud.curl_drift.xyz) / CURL_NOISE_SCALE;
        let curl = textureSampleLevel(t_curl, s_wrap, curl_at, mip(footprint, CURL_NOISE_SCALE, 32.0)).xyz;
        base_at += curl * cloud.curl.x * CURL_INTENSITY * (1.0 - h);
    }
    let base = textureSampleLevel(t_base, s_wrap, base_at / params[2], mip(footprint, params[2], 128.0)).r;
    let height_density = textureSampleLevel(t_types, s_clamp, vec2<f32>(h, (f32(kind) + 0.5) / 6.0), 0.0).r;
    let erosion = textureSampleLevel(t_erosion, s_wrap, erosion_at / params[3], mip(footprint, params[3], 32.0)).r;
    let fine = textureSampleLevel(t_erosion, s_wrap, erosion_at * FINE_NOISE_SCALE, mip(footprint, 1.0 / FINE_NOISE_SCALE, 32.0)).r;
    var d = remap(base, 1.0 - height_density, 1.0, 0.0, 1.0);
    d = remap(d, 1.0 - erosion, 1.0, 0.0, 1.0);
    d = remap(d, params[0], 1.0, 0.0, 1.0);
    d = remap(d, 1.0 - coverage, 1.0, 0.0, 1.0);
    let gone = d <= 0.0 || h < 0.0 || h > 1.0 || coverage < 0.0001;
    d = remap(d, params[1] - fine, 1.0, 0.0, 1.0);
    return select(clamp(d * params[4], 0.0, 1.0), 0.0, gone);
}

// CloudShadowMarch outside the self-shadow cache: the mean density at a
// tenth, a fifth, three tenths and half the layer's thickness toward the
// light, times the thickness.
fn extinction_to_light(p: vec3<f32>, footprint: f32, dist: f32) -> f32 {
    let ray = cloud.toward.xyz * LAYER_THICKNESS;
    let sum = cloud_density_flat(p + ray * 0.1, footprint, dist) + cloud_density_flat(p + ray * 0.2, footprint, dist)
        + cloud_density_flat(p + ray * 0.3, footprint, dist) + cloud_density_flat(p + ray * 0.5, footprint, dist);
    return 0.25 * sum * LAYER_THICKNESS;
}

// π × LgtSky (csComputeAtmosphericLight): the sun through the air to the
// ground, its height held to 0.05 or more so a low sun keeps lighting the
// clouds, fading out as it sets; by night the moon, as the sky scatters it.
fn light_on_clouds() -> vec3<f32> {
    let bottom = air.radii.x;
    if cloud.toward.w > 0.5 {
        return to_sun(bottom, max(air.sun.y, 0.05)) * air.sun_irradiance.rgb * cloud.ambient.w;
    }
    return to_sun(bottom, max(air.moon.y, 0.05)) * air.moon_irradiance.rgb;
}

// CloudInscattering: three phase octaves standing in for multiple
// scattering, a floor for backscatter, and a quarter of the clear sky.
fn inscattering(h: f32, d: f32, to_light: f32, phase: vec2<f32>, direct: vec3<f32>) -> vec3<f32> {
    let through = exp(-to_light);
    let sum = 0.5 + 0.5 + 0.06;
    var fn_ = (0.5 / sum) * through * phase.x
        + (0.5 / sum) * pow(through, 0.75) * phase.y
        + (0.06 / sum) * pow(through, 0.4);
    fn_ *= 2.0;
    fn_ = max(fn_, mix(0.02, 0.2, d) / PI);
    let height = clamp(remap(h, 0.0, 0.6, 0.2, 1.0), 0.0, 1.0);
    return cloud.albedo.rgb * (direct * fn_ + 0.25 * cloud.ambient.rgb * height);
}

// The view's ray through `ndc`, as the prelude's view_ray has it.
fn cloud_ray(ndc: vec2<f32>) -> vec3<f32> {
    let near = cloud.unclip * vec4<f32>(ndc, 1.0, 1.0);
    let far = cloud.unclip * vec4<f32>(ndc, 1e-3, 1.0);
    return normalize(far.xyz / far.w - near.xyz / near.w);
}

// The most a cloud may hold at `p`, from the weather map and the type's
// height profile alone (MarchClouds' sparse test): zero where the march
// may stride on.
fn cloud_bound(p: vec3<f32>) -> f32 {
    let h = sample_height(p);
    if h < 0.0 || h > 1.0 { return 0.0; }
    var at = p.xz / cloud.albedo.w + vec2<f32>(0.25);
    at.y = 1.0 - at.y;
    at += cloud.scroll.xy;
    let weather = textureSampleLevel(t_weather, s_wrap, at, 0.0);
    if weather.r <= 0.001 { return 0.0; }
    let kind = u32(min(weather.g, 0.999) * 6.0);
    let height_density = textureSampleLevel(t_types, s_clamp, vec2<f32>(h, (f32(kind) + 0.5) / 6.0), 0.0).r;
    return remap(weather.r, 1.0 - height_density, 1.0, 0.0, 1.0);
}

// Interleaved gradient noise over `px`, turned by the golden ratio each
// `turn`: the game offsets each pixel's samples by blue noise and
// accumulates over frames; each march takes new offsets and blends into
// the ones before (fs_clouds), so the slices a coarse step cuts into the
// layer average out instead of standing as grain.
fn cloud_jitter(px: vec2<f32>, turn: f32) -> f32 {
    let ign = fract(52.9829189 * fract(dot(px, vec2<f32>(0.06711056, 0.00583715))));
    return fract(ign + turn * 0.618034);
}

// The most fine steps a ray takes, and samples overall: the game allows
// 512 at a quarter of the screen and accumulates over frames; the editor
// redraws only when the view moves, in one go. A level ray crosses the
// layer in about 4000 units: some 180 fine steps at the game's spacing,
// 80 sparse ones.
const FINE_STEPS: u32 = 128u;
const ALL_STEPS: u32 = 256u;

// How far a texel with no clouds counts as lying: past any cloud, within
// the info map's half floats.
const NO_CLOUD: f32 = 60000.0;
// The most of the history a march keeps: nine parts in ten, after
// HISTORY_MARCHES marches of one point.
const HISTORY_MAX: f32 = 0.9;
const HISTORY_MARCHES: f32 = 16.0;
// What a history that lay at another distance counts for, clipped: thrown
// out, a cloud's edge moving over far ground left a band of texels some
// marches wide that started over, each marched every fourth frame.
const REJECTED_MARCHES: f32 = 2.0;
// The history's box, in deviations of this march's texels around it. The
// extremes alone flickered more than the full-rate marcher when the view
// turned (gpu_bench BENCH_FLICKER, continental_overview orbit: 0.88 levels
// frame to frame against 0.66), 1 matched it, 0.75 is under it; tighter
// starts to trade the still image's grain for the moving one's.
const CLIP_SIGMA: f32 = 0.75;
// What a texel's neighbours' march counts for, against its own.
const FILL_SHARE: f32 = 0.5;

struct CloudMarch {
    color: vec4<f32>,
    // Along the ray to where the clouds' light comes from.
    dist: f32,
}

struct CloudOut {
    @location(0) map: vec4<f32>,
    // r: `dist`, g: how many marches the texel blends
    @location(1) info: vec4<f32>,
}

// Each march takes one texel of every 2x2 block of the map, in turn, into
// a target a quarter its size, and fs_clouds fills the other three from
// the history: a march of every texel cost 0.57 ms at 5K on
// continental_overview, a quarter 0.29 with the fill, and the clouds
// drift a few units a second. Packed, not every fourth texel of a
// full-size draw: a warp waits for its longest ray, and with three lanes
// in four idle it would take as long as all four.
const QUARTER = array<vec2<u32>, 4>(vec2<u32>(0u, 0u), vec2<u32>(1u, 1u), vec2<u32>(1u, 0u), vec2<u32>(0u, 1u));

fn quarter() -> vec2<i32> {
    return vec2<i32>(QUARTER[u32(cloud.curl.y) % 4u]);
}

fn whole_map() -> bool {
    return cloud.curl.w > 0.5;
}

struct Fresh {
    @location(0) map: vec4<f32>,
    // r: CloudMarch's `dist`
    @location(1) info: vec4<f32>,
}

// The march into the quarter target (the whole map's size when every texel
// marches).
@fragment fn fs_clouds_march(in: SkyOut) -> Fresh {
    var px = in.clip.xy;
    // The jitter turns once per march of this texel, by the noise over the
    // marched texels side by side (the quarter target's), shifted a
    // quarter for each texel of a block: turned each march of the map, a
    // texel marched every fourth took steps of 4 × 0.618, near halves,
    // and its average swung between two.
    var offset = cloud_jitter(px, 0.0);
    if !whole_map() {
        let turn = u32(cloud.curl.y);
        offset = fract(cloud_jitter(px, f32((turn + 3u) / 4u)) + 0.25 * f32(turn % 4u));
        px = floor(px) * 2.0 + vec2<f32>(quarter()) + 0.5;
    }
    let ndc = vec2<f32>(px.x / cloud.scroll.z * 2.0 - 1.0, 1.0 - px.y / cloud.scroll.w * 2.0);
    let now = march_clouds(ndc, offset);
    return Fresh(now.color, vec4<f32>(now.dist, 0.0, 0.0, 0.0));
}

// The last map's texels where the clouds at `p` lay, blended, and how many
// marches they blend. The four texels around it, bilinear, each only where
// its clouds lay as far as these: a plain bilinear lookup pulled a cloud's
// edge into the sea beside it, again each march, into trails behind every
// cloud. `weight` under 0.25 when it is not in the last view.
struct History {
    color: vec4<f32>,
    marches: f32,
    weight: f32,
    // The four texels bilinear whatever their distance, and their marches.
    any_color: vec4<f32>,
    any_marches: f32,
}

fn history(p: vec3<f32>) -> History {
    var h = History(vec4<f32>(0.0), 0.0, 0.0, vec4<f32>(0.0), 0.0);
    let c = cloud.last_clip * vec4<f32>(p, 1.0);
    if c.w <= 0.0 { return h; }
    let uv = vec2<f32>(c.x / c.w * 0.5 + 0.5, 0.5 - c.y / c.w * 0.5);
    if any(uv < vec2<f32>(0.0)) || any(uv > vec2<f32>(1.0)) { return h; }
    let was = min(distance(p, cloud.last_eye.xyz), NO_CLOUD);
    let size = vec2<i32>(textureDimensions(t_history_info));
    let at = uv * vec2<f32>(size) - 0.5;
    let base = vec2<i32>(floor(at));
    let f = at - floor(at);
    for (var k = 0; k < 4; k++) {
        let o = vec2<i32>(k & 1, k >> 1);
        let q = clamp(base + o, vec2<i32>(0), size - 1);
        let info = textureLoad(t_history_info, q, 0);
        let w = select(1.0 - f.x, f.x, o.x == 1) * select(1.0 - f.y, f.y, o.y == 1);
        let c = textureLoad(t_history, q, 0);
        h.any_color += c * w;
        h.any_marches += info.g * w;
        if abs(info.r - was) > 0.03 * was + 5.0 { continue; }
        h.color += c * w;
        h.marches += info.g * w;
        h.weight += w;
    }
    if h.weight >= 0.25 {
        h.color /= h.weight;
        h.marches /= h.weight;
    }
    return h;
}

// The map, as TAA builds a frame (and RaymarchClouds accumulates its
// clouds): the history, where the clouds are found in the last march's
// view (moved back as far as the weather map drifted) and lay as far from
// the last eye, clipped to the box this march's texels around it span, and
// blended with them: a texel's own march, or for the other three of its
// block this march's texels around it, bilinear (`fill`). Unclipped, the
// texels between marches, reprojected at the distance they held, left a
// cloud bank's edge as a trail two texels wide behind it as the view
// turned. Texels with no history take the fill and count for nothing in
// their next blend. Measured with gpu_bench BENCH_FLICKER.
@fragment fn fs_clouds(in: SkyOut) -> CloudOut {
    let px = vec2<i32>(in.clip.xy);
    let whole = whole_map();
    let o = select(quarter(), vec2<i32>(0), whole);
    let step = select(2, 1, whole);
    let size = vec2<i32>(cloud.scroll.zw);
    let last = select((size + 1) / 2, size, whole) - 1;
    let marched = all((px - o) % step == vec2<i32>(0));
    // Where px lies among this march's texels.
    let at = vec2<f32>(px - o) / f32(step);
    let base = vec2<i32>(floor(at));
    let f = at - floor(at);
    let lit = (light_on_clouds() + cloud.ambient.rgb) * cloud.albedo.rgb;
    let unit = max(dot(lit, LUMA), 1e-6);
    var fill = vec4<f32>(0.0);
    var fill_dist = 0.0;
    for (var k = 0; k < 4; k++) {
        let d = vec2<i32>(k & 1, k >> 1);
        let q = clamp(base + d, vec2<i32>(0), last);
        let w = select(1.0 - f.x, f.x, d.x == 1) * select(1.0 - f.y, f.y, d.y == 1);
        fill += textureLoad(t_fresh, q, 0) * w;
        fill_dist += textureLoad(t_fresh_info, q, 0).r * w;
    }
    // The box: this march's 3x3 texels around px, their mean within
    // CLIP_SIGMA deviations and their extremes.
    let near = clamp(vec2<i32>(round(at)), vec2<i32>(0), last);
    var lo = vec4<f32>(1e9);
    var hi = vec4<f32>(-1e9);
    var m1 = vec4<f32>(0.0);
    var m2 = vec4<f32>(0.0);
    for (var k = 0; k < 9; k++) {
        let q = clamp(near + vec2<i32>(k % 3 - 1, k / 3 - 1), vec2<i32>(0), last);
        let c = squashed(textureLoad(t_fresh, q, 0), unit);
        lo = min(lo, c);
        hi = max(hi, c);
        m1 += c / 9.0;
        m2 += c * c / 9.0;
    }
    let sigma = sqrt(max(m2 - m1 * m1, vec4<f32>(0.0)));
    lo = max(lo, m1 - CLIP_SIGMA * sigma);
    hi = min(hi, m1 + CLIP_SIGMA * sigma);
    // A marched texel's fill is its own march.
    var out = CloudOut(fill, vec4<f32>(fill_dist, 0.0, 0.0, 0.0));
    if cloud.last_eye.w < 0.5 { return out; }
    var dist = out.info.r;
    if !marched { dist = textureLoad(t_history_info, px, 0).r; }
    let p = cloud.eye.xyz + cloud_ray(in.ndc) * dist;
    var before = history(p - cloud.moved.xyz);
    if before.weight < 0.25 {
        if before.any_marches <= 0.0 {
            if !marched { out.info.g = -1.0; }
            return out;
        }
        before.color = before.any_color;
        before.marches = min(before.any_marches, REJECTED_MARCHES);
    }
    let kept = clamp(squashed(before.color, unit), lo, hi);
    // A texel's own march counts whole, its neighbours' around it
    // (`fill`) half: held for three frames in four, the texels between
    // marches lagged a turning view and stood out as a 2x2 pattern.
    let share = select(FILL_SHARE, 1.0, marched);
    let marches = clamp(before.marches + share, 0.0, HISTORY_MARCHES);
    let keep = 1.0 - share * max(1.0 / (marches + 1.0), 1.0 - HISTORY_MAX);
    // Blended with light squashed against what a lit cloud sends: the
    // forward peak (hg 0.975, some 250 times the mean toward a low sun)
    // lands on a thin edge now and then as the jitter turns, and kept nine
    // parts in ten it stood for a dozen marches, a field of sparks at dusk.
    // The sun and the sky both: the sun's share fades out as it sets.
    let blend = mix(squashed(fill, unit), kept, keep);
    if !marched { out.info.r = dist; }
    out.map = vec4<f32>(blend.rgb * unit / max(1.0 - dot(blend.rgb, LUMA), 1e-3), blend.a);
    out.info.g = marches;
    return out;
}

// A map texel's light squashed (`squash`), its transmittance as it is.
fn squashed(c: vec4<f32>, unit: f32) -> vec4<f32> {
    return vec4<f32>(squash(c.rgb, unit), c.a);
}

const LUMA = vec3<f32>(0.2126, 0.7152, 0.0722);

// `c` over `unit`, its luminance squashed below 1: c / (1 + luma).
fn squash(c: vec3<f32>, unit: f32) -> vec3<f32> {
    let v = c / unit;
    return v / (1.0 + dot(v, LUMA));
}

// The march through clip position `ndc`, its samples offset by `offset`
// of a step.
fn march_clouds(ndc: vec2<f32>, offset: f32) -> CloudMarch {
    let dir = cloud_ray(ndc);
    let eye = cloud.eye.xyz;
    // Past VOLUMETRIC_MAX from where the ray enters nothing shows.
    let interval = cloud_interval(eye, dir);
    let start = interval.x;
    let end = interval.y;
    if end <= start {
        return CloudMarch(vec4<f32>(0.0, 0.0, 0.0, 1.0), NO_CLOUD);
    }
    let span = min(end - start, VOLUMETRIC_MAX);
    // RaymarchClouds' steps: 6 units while the weather map shows no cloud,
    // 2 where it does, both growing toward the horizon with the distance
    // from where the ray entered the layer (its start, not the eye's). Fine
    // steps stretch so a ray through the whole span fits FINE_STEPS.
    let toward_horizon = 1.0 - clamp(dir.y / 0.3, 0.0, 1.0);
    let fine_scale = 0.02 * toward_horizon;
    let sparse_scale = 0.04 * toward_horizon;
    let stretch = max(1.0, span / (f32(FINE_STEPS) * (2.0 + fine_scale * 0.5 * span)));
    let nu = dot(dir, cloud.toward.xyz);
    let phase = vec2<f32>(hg_phase(0.975, nu), hg_phase(0.9, nu));
    // A texel's width, in radians: what one covers at a distance.
    let texel = length(cloud_ray(ndc + vec2<f32>(2.0 / cloud.scroll.z, 0.0)) - dir);
    let direct = light_on_clouds();
    var light = vec3<f32>(0.0);
    var through = 1.0;
    // Where along the ray the clouds' light comes from, weighted by what
    // each step hides: the air is marched out to there.
    var depth = 0.0;
    var at = 0.0;
    var missed = 0u;
    var fine = false;
    for (var i = 0u; i < ALL_STEPS && at < span; i++) {
        if !fine {
            let step = 6.0 + sparse_scale * at;
            if cloud_bound(eye + dir * (start + at + offset * step)) <= 0.0001 {
                at += step;
                continue;
            }
            fine = true;
            missed = 0u;
        }
        let step = min((2.0 + fine_scale * at) * stretch, span - at);
        let along = at + offset * step;
        let t = start + along;
        let p = eye + dir * t;
        let footprint = max(t * texel, 1.0);
        // SampleClouds' fade toward cVolumetricMaxDistance.
        let fade = linear_step(VOLUMETRIC_MAX, VOLUMETRIC_MAX - VOLUMETRIC_FADE, along);
        let d = cloud_density(p, footprint, t) * fade;
        at += step;
        if d <= 0.0 {
            missed++;
            if missed > 5u { fine = false; }
            continue;
        }
        missed = 0u;
        let s = inscattering(sample_height(p), d, extinction_to_light(p, footprint, t), phase, direct);
        // IntegrateStep with the medium constant over the step.
        let tr = exp(-d * step);
        light += through * s * (1.0 - tr);
        depth += through * (1.0 - tr) * t;
        through *= tr;
        if through < 0.01 { break; }
    }
    // The air in front of the clouds. RaymarchClouds integrates the air
    // step by step with them, so a cloud deep in a long, low span sits
    // behind more haze than one where the ray enters; this marches the
    // air once, out to the clouds' mean depth by what they hide.
    let covered = 1.0 - through;
    // No step met a cloud: nothing to see the air in front of. Half the
    // clouds' time from high up, where most rays pass between them.
    if through >= 1.0 {
        return CloudMarch(vec4<f32>(0.0, 0.0, 0.0, 1.0), NO_CLOUD);
    }
    var to = start;
    if covered > 1e-4 { to = depth / covered; }
    let km = air.radii.z * 0.001;
    let front = march(radius_at(eye.y), dir, to * km, 8u);
    let seen = dot(front.through, vec3<f32>(1.0 / 3.0));
    let color = vec4<f32>(covered * front.light + seen * light, through);
    // Faint clouds count as none: their mean depth wanders from march to
    // march with the jitter, and the history would keep being thrown out.
    if covered <= 0.05 { return CloudMarch(color, NO_CLOUD); }
    return CloudMarch(color, min(to, NO_CLOUD));
}

// csVolumetricShadow into the cloud shade (clouds.rs Shade): per texel of
// a world square, the clouds' transmittance along the line from sea level
// toward the light, marched down from the layer's top in intervals that
// grow along the ray, each sampled at a jittered point (the game's blue
// noise). The game marches from each shadow cascade texel's own depth and
// stops at cShadowCutoffHeight; sea level stands in for the depth (the
// kit looks higher points up where their line toward the sun meets it),
// and the cutoff is the layer's base. TODO: cShadowCutoffHeight's value
// is not traced (cbEnvironment's fill takes it from a field nothing writes
// a constant to). Written as GetShadow's artist version uses it: the
// light left is lerp(1 - intensity, 1, transmittance).
@fragment fn fs_cloud_shade(in: SkyOut) -> @location(0) vec4<f32> {
    let toward = cloud.toward.xyz;
    if toward.y < 0.001 { return vec4<f32>(1.0); }
    // Rows run down the texture, toward +Z.
    let uv = vec2<f32>(0.5 + 0.5 * in.ndc.x, 0.5 - 0.5 * in.ndc.y);
    let ground = cloud.shade.xy + uv * cloud.shade.z;
    let top = vec3<f32>(ground.x, 0.0, ground.y) + toward * (LAYER_UPPER / toward.y);
    let span = LAYER_THICKNESS / toward.y;
    let footprint = cloud.shade.z / cloud.curl.z;
    let jitter = cloud_jitter(in.clip.xy, cloud.curl.y);
    // A low sun's ray crosses far more of the layer: the steps stretch so
    // it fits, as the game's do.
    let min_step = min(4.0, span / 64.0);
    var start = 0.0025;
    var end = min_step;
    var at = 0.0;
    var extinction = 0.0;
    for (var i = 0u; i < 256u && at < span && extinction < 5.0; i++) {
        let step = min_step + 0.02 * end;
        start = end;
        end = min(end + step, span);
        at = mix(start, end, jitter);
        extinction += (end - start) * cloud_density(top - toward * at, footprint, 0.0);
    }
    let light = 1.0 - cloud.shade.w * (1.0 - exp(-extinction));
    return vec4<f32>(light, 0.0, 0.0, 1.0);
}
