// Anno 117's sky as the camera sees it (environment.fx ComputeAtmosphereEnd
// over functions_atmosphere.fxh SampleAtmosphere): the backdrop marched from
// the camera's own height to the atmosphere's top over the night sky and
// moon (sky.fx psSkyBox, psMoon), behind the clouds marched for the view
// at a lower resolution (clouds.wgsl) (fs_backdrop), and the haze over what
// stands in it, marched to the surface (fs_haze). Follows the frame
// prelude. Lengths are kilometres: world units times the preset's Scale,
// so the planet's radius keeps f32 precise.

struct Air {
    // bottom, top (km), Scale, the air's ceiling (world units, AIR_CEILING)
    radii: vec4<f32>,
    // per km; w: scale height (km)
    rayleigh: vec4<f32>,
    // scattering, absorption per km, scale height (km), MieG
    mie: vec4<f32>,
    // ozone absorption per km; w: peak height (km)
    ozone: vec4<f32>,
    // ozone width, custom layer height and width (km), custom strength
    shape: vec4<f32>,
    custom_scattering: vec4<f32>,
    custom_absorption: vec4<f32>,
    // toward the sun, and its irradiance
    sun: vec4<f32>,
    sun_irradiance: vec4<f32>,
    // toward the moon, and its irradiance as the sky scatters it
    // (MoonLight times MoonSkyStrength, by night)
    moon: vec4<f32>,
    moon_irradiance: vec4<f32>,
    // AddSunDiskLight's 150 times the sun's irradiance; w = 1 by day
    disc: vec4<f32>,
    // AtmosphereScatteringFactor: sun y full and none, ray y full and none
    fade: vec4<f32>,
    // minimum scatter, its distances (world units, minimum until, full
    // from), and the light's height (-cLgtDirectionUncapped.y)
    fade_near: vec4<f32>,
    // cTimeOfDay.x, cDayNight.y (night ratio), cAtmosphereStarLight; w = 1
    // with a night cube
    night: vec4<f32>,
    // x = 1 with a cloud map (clouds.wgsl) marched for this view; y: the
    // cloud dome's radius (cCloudDomeRadius)
    clouds: vec4<f32>,
}

@group(1) @binding(0) var<uniform> air: Air;
@group(1) @binding(1) var t_transmittance: texture_2d<f32>;
@group(1) @binding(2) var t_multiscatter: texture_2d<f32>;
@group(1) @binding(3) var s_table: sampler;
@group(1) @binding(4) var t_night: texture_cube<f32>;
@group(1) @binding(5) var t_moon: texture_2d<f32>;
// The clouds the view sees (clouds.wgsl), over the whole target at a lower
// resolution: rgb their and the air in front's light, a their
// transmittance.
@group(1) @binding(6) var t_cloud_map: texture_2d<f32>;
@group(1) @binding(7) var s_cloud_map: sampler;

const PI: f32 = 3.14159265;
// csComputeMultiscatLUT's lowest cos(sun zenith).
const MULTISCATTER_MIN_MU_S: f32 = -0.25;
// LookupTransmittanceToSun: the sun's angular radius.
const SUN_RADIUS: f32 = 0.004675;
// ComputeAtmosphereRange shades the sun's single scattering by the clouds'
// shadow on samples nearer than this (world units, MAX_ATMOSPHERE_SHADOW_DIST)
// and lower than SHADOW_TOP.
const SHADOW_DIST: f32 = 3000.0;
const SHADOW_TOP: f32 = 300.0;
// Steps from the camera to the atmosphere's top, and to a surface: the game
// marches at a quarter resolution and blurs; these sample the same integral
// per pixel, spaced closer near the camera, where its steps are shortest.
const SKY_STEPS: u32 = 20u;
const HAZE_STEPS: u32 = 10u;
// Full-size pixels per side of fs_haze's texels (pass.rs HAZE_SCALE).
const HAZE_SCALE: i32 = 4;
// vsMoon: the quad stands 999999 out and spans 35000 either way.
const MOON_DISTANCE: f32 = 999999.0;
const MOON_SPAN: f32 = 35000.0;

struct SkyOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) ndc: vec2<f32>,
}

@vertex fn vs_full(@builtin(vertex_index) vertex: u32) -> SkyOut {
    let p = vec2<f32>(f32((vertex << 1u) & 2u), f32(vertex & 2u)) * 2.0 - 1.0;
    var out: SkyOut;
    out.clip = vec4<f32>(p, 0.0, 1.0);
    out.ndc = p;
    return out;
}

// A table whose first and last texel centres sit at unit coordinates 0 and
// 1, as the CPU bake builds it, through the filtering sampler.
fn table(t: texture_2d<f32>, uv: vec2<f32>) -> vec3<f32> {
    let size = vec2<f32>(textureDimensions(t));
    let at = (clamp(uv, vec2<f32>(0.0), vec2<f32>(1.0)) * (size - 1.0) + 0.5) / size;
    return textureSampleLevel(t, s_table, at, 0.0).rgb;
}

fn distance_to_top(r: f32, mu: f32) -> f32 {
    let top = air.radii.y;
    let d = r * r * (mu * mu - 1.0) + top * top;
    return max(-r * mu + sqrt(max(d, 0.0)), 0.0);
}

// TransmittanceUVFromRay, Bruneton's mapping.
fn to_top(r: f32, mu: f32) -> vec3<f32> {
    let bottom = air.radii.x;
    let top = air.radii.y;
    let h = sqrt(top * top - bottom * bottom);
    let rho = sqrt(max(r * r - bottom * bottom, 0.0));
    let d = distance_to_top(r, mu);
    let dmin = top - r;
    let dmax = rho + h;
    return table(t_transmittance, vec2<f32>((d - dmin) / max(dmax - dmin, 1e-9), rho / h));
}

// LookupTransmittanceToSun: faded over the sun's radius as it sets behind
// the planet.
fn to_sun(r: f32, mu_s: f32) -> vec3<f32> {
    let sin_h = air.radii.x / r;
    let cos_h = -sqrt(max(1.0 - sin_h * sin_h, 0.0));
    let visible = smoothstep(-sin_h * SUN_RADIUS, sin_h * SUN_RADIUS, mu_s - cos_h);
    return to_top(r, mu_s) * visible;
}

fn multiscatter(r: f32, mu_s: f32) -> vec3<f32> {
    let xmu = clamp((mu_s - MULTISCATTER_MIN_MU_S) / (1.0 - MULTISCATTER_MIN_MU_S), 0.0, 1.0);
    let xr = clamp((r - air.radii.x) / (air.radii.y - air.radii.x), 0.0, 1.0);
    return table(t_multiscatter, vec2<f32>(xmu, xr));
}

// Rayleigh, Mie, ozone and custom layer densities at `h` km.
fn density(h: f32) -> vec4<f32> {
    var custom = 0.0;
    if air.shape.z > 0.0 && air.shape.w > 0.0 {
        custom = air.shape.w * exp(-abs(h - air.shape.y) / air.shape.z);
    }
    return vec4<f32>(
        exp(-h / air.rayleigh.w),
        exp(-h / air.mie.z),
        max(1.0 - abs(h - air.ozone.w) / air.shape.x, 0.0),
        custom,
    );
}

fn hg_phase(g: f32, nu: f32) -> f32 {
    let gg = g * g;
    return (1.0 - gg) / (4.0 * PI * pow(1.0 + gg - 2.0 * g * nu, 1.5));
}

fn linear_step(e0: f32, e1: f32, x: f32) -> f32 {
    return clamp((x - e0) / (e1 - e0), 0.0, 1.0);
}

// AtmosphereScatteringFactor without the distant scatter: scattering near
// the camera only when the light is low, less when looking down, always
// when looking up, reaching full at the fade's distance (horizontal, world
// units).
fn scatter_factor(dir: vec3<f32>, horizontal: f32) -> f32 {
    var near = linear_step(air.fade.y, air.fade.x, air.fade_near.w)
        * smoothstep(0.0, 1.0, linear_step(air.fade.w, air.fade.z, dir.y));
    near = mix(near, 1.0, linear_step(0.0, 0.1, dir.y));
    let scatter_near = mix(air.fade_near.x, 1.0, near);
    let far = linear_step(air.fade_near.y, air.fade_near.z, horizontal);
    return mix(scatter_near, 1.0, far);
}

struct Marched {
    light: vec3<f32>,
    through: vec3<f32>,
}

// Step i of `steps` along `dist`: where it starts and ends, spaced closer
// near the camera.
fn step_span(i: u32, steps: u32, dist: f32) -> vec2<f32> {
    let a = f32(i) / f32(steps);
    let b = f32(i + 1u) / f32(steps);
    return dist * vec2<f32>(a * a, b * b);
}

// One step of `march` at `t` km along, `len` km long: the sun's and
// moon's single scattering and the multiple scattering it scatters in,
// apart, so the haze can dim the single by the clouds' shadow there.
struct Step {
    single: vec3<f32>,
    multi: vec3<f32>,
    ext: vec3<f32>,
}

fn march_step(r: f32, dir: vec3<f32>, t: f32) -> Step {
    let mu = dir.y;
    let rd = clamp(sqrt(t * t + 2.0 * r * mu * t + r * r), air.radii.x, air.radii.y);
    let h = rd - air.radii.x;
    let dens = density(h);
    let rayleigh = air.rayleigh.rgb * dens.x;
    let mie = air.mie.x * dens.y;
    let custom = air.custom_scattering.rgb * dens.w;
    let scattering = rayleigh + vec3<f32>(mie) + custom;
    let extinction = scattering + vec3<f32>(air.mie.y * dens.y) + air.ozone.rgb * dens.z
        + air.custom_absorption.rgb * dens.w;
    var out = Step(vec3<f32>(0.0), vec3<f32>(0.0), max(extinction, vec3<f32>(1e-12)));
    let sources = array<vec3<f32>, 2>(air.sun.xyz, air.moon.xyz);
    let irradiance = array<vec3<f32>, 2>(air.sun_irradiance.rgb, air.moon_irradiance.rgb);
    for (var s = 0u; s < 2u; s++) {
        let e = irradiance[s];
        if max(e.r, max(e.g, e.b)) <= 0.0 { continue; }
        let toward = sources[s];
        let nu = dot(dir, toward);
        let single = vec3<f32>(mie * hg_phase(air.mie.w, nu)) + rayleigh * (3.0 * (1.0 + nu * nu) / (16.0 * PI));
        let mu_sd = clamp((r * toward.y + t * nu) / rd, -1.0, 1.0);
        out.single += e * (single + custom) * to_sun(rd, mu_sd);
        out.multi += e * multiscatter(rd, mu_sd) * (rayleigh + vec3<f32>(mie));
    }
    return out;
}

// Integrates `step`, `len` km long, its single scattering dimmed by `lit`,
// exactly over its constant medium (IntegrateStep) behind what `m` holds.
fn integrate(m: ptr<function, Marched>, step: Step, len: f32, lit: f32) {
    let tr = exp(-step.ext * len);
    (*m).light += (*m).through * (step.single * lit + step.multi) * (1.0 - tr) / step.ext;
    (*m).through *= tr;
}

// SampleAtmosphere along a ray from radius `r` toward `dir` for `dist` km:
// single scattering of the sun and moon by Rayleigh and Mie, the custom
// layer's, and the multiple-scattering table, each step's length scaled by
// the scatter factor as ComputeAtmosphereRange scales it. The cloud map's
// march has no frame bindings: this one is unshaded; haze() dims by
// the clouds' shadow.
fn march(r: f32, dir: vec3<f32>, dist: f32, steps: u32) -> Marched {
    var out = Marched(vec3<f32>(0.0), vec3<f32>(1.0));
    // World units per km along the ray, horizontally.
    let world = 1000.0 / air.radii.z * length(dir.xz);
    for (var i = 0u; i < steps; i++) {
        let span = step_span(i, steps, dist);
        let t = 0.5 * (span.x + span.y);
        integrate(&out, march_step(r, dir, t), (span.y - span.x) * scatter_factor(dir, t * world), 1.0);
    }
    return out;
}

// A height's radius: the planet's surface at sea level, the height scaled
// with the atmosphere (cbAtmosphere divides every distance by Scale; this
// multiplies the world's instead).
fn radius_at(height: f32) -> f32 {
    return air.radii.x + max(height, 0.0) * air.radii.z * 0.001;
}

// Above AIR_CEILING (the game's highest camera) the editor sees the
// sky as from there.
fn camera_radius() -> f32 {
    return radius_at(min(globals.eye.y, air.radii.w));
}

fn view_ray(ndc: vec2<f32>) -> vec3<f32> {
    let near = unproject(ndc, 1.0);
    return normalize(unproject(ndc, 1e-3) - near);
}

// `v` turned `angle` about unit `axis` (RotateWithQuaternion of
// QuaternionFromAxisAngle).
fn turn(v: vec3<f32>, axis: vec3<f32>, angle: f32) -> vec3<f32> {
    let c = cos(angle);
    let s = sin(angle);
    return v * c + cross(axis, v) * s + axis * dot(axis, v) * (1.0 - c);
}

// psSkyBox: the night cube turned with the time of day (RotateNightSky),
// faded in by night.
fn stars(dir: vec3<f32>) -> vec3<f32> {
    if air.night.w < 0.5 { return vec3<f32>(0.0); }
    let t = air.night.x;
    let angle_y = 2.0 * PI - t * 2.0 * PI;
    let angle_x = (fract(t + 0.5) - 0.5) * 2.0 * PI * 0.5;
    let at = turn(turn(dir, vec3<f32>(1.0, 0.0, 0.0), angle_x), vec3<f32>(0.0, 1.0, 0.0), angle_y);
    let sky = textureSampleLevel(t_night, s_table, at, 0.0).rgb;
    return air.night.z * sky * linear_step(0.3, 0.5, air.night.y);
}

// psMoon over the night sky: the camera-facing quad toward the moon,
// 0.6 × tex², above the horizon, blended by its alpha.
fn moon(dir: vec3<f32>, under: vec3<f32>) -> vec3<f32> {
    if air.moon_irradiance.w < 0.5 || dir.y <= 0.0 { return under; }
    let forward = view_ray(vec2<f32>(0.0, 0.0));
    let along = dot(dir, forward);
    if along <= 0.0 { return under; }
    let right = normalize(unproject(vec2<f32>(1.0, 0.0), 1e-3) - unproject(vec2<f32>(-1.0, 0.0), 1e-3));
    let up = normalize(unproject(vec2<f32>(0.0, 1.0), 1e-3) - unproject(vec2<f32>(0.0, -1.0), 1e-3));
    let center = air.moon.xyz * MOON_DISTANCE;
    // Behind the camera the view plane through it meets no ray ahead.
    if dot(center, forward) <= 0.0 { return under; }
    let p = dir * (dot(center, forward) / along) - center;
    let q = vec2<f32>(dot(p, right), dot(p, up)) / MOON_SPAN;
    if any(abs(q) > vec2<f32>(1.0)) { return under; }
    let tex = textureSampleLevel(t_moon, s_table, q * vec2<f32>(0.5, -0.5) + 0.5, 0.0);
    return mix(under, 0.6 * tex.rgb * tex.rgb, tex.a);
}

// The cloud map through a cubic B-spline (four bilinear taps): its texels
// are several pixels wide, and the clouds' edges in them nearly hard, so
// a plain bilinear lookup shows them as steps.
fn cloud_map(uv: vec2<f32>) -> vec4<f32> {
    let size = vec2<f32>(textureDimensions(t_cloud_map));
    let p = uv * size - 0.5;
    let i = floor(p);
    let f = p - i;
    let f2 = f * f;
    let f3 = f2 * f;
    let w0 = (1.0 - 3.0 * f + 3.0 * f2 - f3) / 6.0;
    let w1 = (4.0 - 6.0 * f2 + 3.0 * f3) / 6.0;
    let w2 = (1.0 + 3.0 * f + 3.0 * f2 - 3.0 * f3) / 6.0;
    let w3 = f3 / 6.0;
    let g0 = w0 + w1;
    let g1 = w2 + w3;
    let h0 = (i - 0.5 + w1 / g0) / size;
    let h1 = (i + 1.5 + w3 / g1) / size;
    return g0.y * (g0.x * textureSampleLevel(t_cloud_map, s_cloud_map, vec2<f32>(h0.x, h0.y), 0.0)
            + g1.x * textureSampleLevel(t_cloud_map, s_cloud_map, vec2<f32>(h1.x, h0.y), 0.0))
        + g1.y * (g0.x * textureSampleLevel(t_cloud_map, s_cloud_map, vec2<f32>(h0.x, h1.y), 0.0)
            + g1.x * textureSampleLevel(t_cloud_map, s_cloud_map, vec2<f32>(h1.x, h1.y), 0.0));
}

// The atmosphere over the night sky and moon, as psUpsampleScattering
// composes the seen sky (avgTrans × sky + inscatter), with
// AddSunDiskLight. No MinimumSkyLight here: sky.fx raises only the
// lighting cube to it.
//
// Below the horizon it draws only where the kit's sea did not: from a
// raised eye the sea ends at the far plane a little under the horizon, and
// the strip down to it stood empty, a dark band under a hard line. The
// game's ocean reaches the horizon, hazed to the sky's colour there; the
// strip gets that colour (the sky level with the eye), and fs_haze_up and
// clouds_over draw over it as over the sea.
@fragment fn fs_backdrop(in: SkyOut) -> @location(0) vec4<f32> {
    if globals.eye.w < 0.5 { discard; }
    let ray = view_ray(in.ndc);
    let below = ray.y < 0.0;
    let dir = select(ray, normalize(vec3<f32>(ray.x, 0.0, ray.z)), below);
    let sky = sky_air(dir);
    if below { return vec4<f32>(sky.rgb, 1.0); }
    return backdrop(in, dir, sky);
}

// The air toward `dir` at or above the horizon: its light, and its mean
// transmittance in w.
fn sky_air(dir: vec3<f32>) -> vec4<f32> {
    let r = camera_radius();
    let m = march(r, dir, distance_to_top(r, dir.y), SKY_STEPS);
    return vec4<f32>(m.light, dot(m.through, vec3<f32>(1.0 / 3.0)));
}

// Whether full-size pixel `at` shows the sky above the horizon: nothing
// drew there and it looks up.
fn shows_sky(at: vec2<i32>) -> bool {
    let size = vec2<i32>(textureDimensions(t_depth));
    let px = clamp(at, vec2<i32>(0), size - 1);
    if textureLoad(t_depth, px, 0) > 0.0 { return false; }
    let uv = (vec2<f32>(px) + 0.5) / vec2<f32>(size);
    return view_ray(vec2<f32>(uv.x * 2.0 - 1.0, 1.0 - uv.y * 2.0)).y >= 0.0;
}

// fs_backdrop with the air from fs_haze's quarter-size march: the sky's
// light varies slowly across the screen, and marching it per pixel was
// most of the backdrop's 0.23 ms at 5K (Albion). Stars, moon, clouds and
// the sun's disc stay per pixel. The four nearest texels, bilinear, the
// sky's only (fs_haze marks them by alpha below zero); where none is
// (along the horizon and silhouettes), and below the horizon, the pixel
// marches its own.
@fragment fn fs_backdrop_up(in: SkyOut) -> @location(0) vec4<f32> {
    if globals.eye.w < 0.5 { discard; }
    let dir = view_ray(in.ndc);
    if dir.y < 0.0 {
        return vec4<f32>(sky_air(normalize(vec3<f32>(dir.x, 0.0, dir.z))).rgb, 1.0);
    }
    // Texel i holds pixel HAZE_SCALE i.
    let p = floor(in.clip.xy) / f32(HAZE_SCALE);
    let base = vec2<i32>(floor(p));
    let f = p - floor(p);
    let last = vec2<i32>(textureDimensions(t_haze)) - 1;
    var sum = vec4<f32>(0.0);
    var weight = 0.0;
    for (var k = 0; k < 4; k++) {
        let o = vec2<i32>(k & 1, k >> 1);
        let texel = textureLoad(t_haze, min(base + o, last), 0);
        if texel.a >= 0.0 { continue; }
        let w = select(1.0 - f.x, f.x, o.x == 1) * select(1.0 - f.y, f.y, o.y == 1);
        sum += vec4<f32>(texel.rgb, -1.0 - texel.a) * w;
        weight += w;
    }
    if weight < 1e-3 { return backdrop(in, dir, sky_air(dir)); }
    return backdrop(in, dir, sum / weight);
}

// The backdrop toward `dir`, above the horizon, behind `sky` (sky_air's
// light and mean transmittance).
fn backdrop(in: SkyOut, dir: vec3<f32>, sky: vec4<f32>) -> vec4<f32> {
    let behind = moon(dir, stars(dir));
    var color = sky.w * behind + sky.rgb;
    // The clouds in front of the air beyond them, the stars and the sun.
    var clouds = CLEAR_AIR;
    if air.clouds.x > 0.5 {
        clouds = cloud_map(vec2<f32>(in.ndc.x * 0.5 + 0.5, 0.5 - in.ndc.y * 0.5));
    }
    color = color * clouds.a + clouds.rgb;
    // AddSunDiskLight: inside the disc, limb darkened, through the air.
    let nu = dot(dir, air.sun.xyz);
    let min_nu = 0.99997;
    if air.disc.w > 0.5 && nu >= min_nu {
        let center_to_edge = 1.0 - clamp((nu - min_nu) / (1.0 - min_nu), 0.0, 1.0);
        let mm = sqrt(1.0 - center_to_edge * center_to_edge);
        let a = 4.0 * vec3<f32>(0.397, 0.503, 0.652);
        let limb = clamp(1.0 - (1.0 - pow(vec3<f32>(mm), a)), vec3<f32>(0.0), vec3<f32>(1.0));
        // The disc reddens by each channel's transmittance: its few pixels
        // march their own.
        let r = camera_radius();
        let m = march(r, dir, distance_to_top(r, dir.y), SKY_STEPS);
        color += air.disc.rgb * limb * m.through * clouds.a;
    }
    return vec4<f32>(color, 1.0);
}

// The cloud map below the horizon, over the sea, the islands and their
// haze: there the clouds lie in front of anything the scene draws, which
// stays under their base. Above the horizon fs_backdrop composites them
// behind the scene.
//
// Not over a surface the view sees from under the layer's base: the base is
// a sphere (clouds.wgsl dome_inner) that bends down to the sea some 200 km
// out, so rays just under the horizon met clouds there and drew them over
// near mountains, a band of sky across every peak at the horizon. With the
// eye and the surface both inside the base's sphere, the whole sight line
// is (the sphere is convex), and no cloud lies in front.
//
// CLEAR_AIR where it draws nothing; `z` is the pixel's depth.
fn clouds_over(in: SkyOut, z: f32) -> vec4<f32> {
    if air.clouds.x < 0.5 || view_ray(in.ndc).y >= 0.0 { return CLEAR_AIR; }
    if z > 0.0 {
        let r = air.clouds.y;
        let radius = (r * r + LAYER_LOWER * LAYER_LOWER) / (2.0 * r);
        let eye = globals.eye.xyz;
        let centre = vec3<f32>(eye.x, LAYER_LOWER - radius, eye.z);
        if distance(eye, centre) < radius && distance(unproject(in.ndc, z), centre) < radius {
            return CLEAR_AIR;
        }
    }
    let clouds = cloud_map(vec2<f32>(in.ndc.x * 0.5 + 0.5, 0.5 - in.ndc.y * 0.5));
    if clouds.a > 0.999 { return CLEAR_AIR; }
    return clouds;
}

// Light and transmittance as the premultiplied blend takes them; CLEAR_AIR
// leaves the target as it is.
const CLEAR_AIR: vec4<f32> = vec4<f32>(0.0, 0.0, 0.0, 1.0);

// From the camera to what a pixel shows: the surface its depth gives, but
// no farther than the sea surface (the depth here is the opaque scene's,
// under the water), or where nothing drew below the horizon, the sea
// level, which the kit's far sea stands for; the game's ocean reaches the
// horizon and hazes with it. Zero for the sky. w = 1 where the scene drew
// something nearer than the sea.
fn behind(ndc: vec2<f32>, z: f32) -> vec4<f32> {
    let dir = view_ray(ndc);
    let eye = globals.eye.xyz;
    var to_sea = 0.0;
    if dir.y < 0.0 && eye.y > 0.0 { to_sea = eye.y / -dir.y; }
    if z > 0.0 {
        let offset = unproject(ndc, z) - eye;
        if to_sea > 0.0 && length(offset) > to_sea { return vec4<f32>(dir * to_sea, 0.0); }
        return vec4<f32>(offset, 1.0);
    }
    return vec4<f32>(dir * to_sea, 0.0);
}

// ComputeAtmosphereEnd with the surface's depth: the light the air between
// camera and surface scatters in, added over the surface dimmed by the
// air's transmittance. One transmittance for all three channels, their
// mean: the blend has a single factor for the color behind.
//
// fs_haze runs it at a sixteenth of the pixels, every fourth on each axis
// (the haze is smooth across the screen), and fs_haze_up composites that
// over the surfaces.
@fragment fn fs_haze(in: SkyOut) -> @location(0) vec4<f32> {
    list_block(vec2<i32>(in.clip.xy));
    let at = vec2<i32>(in.clip.xy) * HAZE_SCALE;
    let uv = (vec2<f32>(at) + 0.5) / vec2<f32>(textureDimensions(t_depth));
    let ndc = vec2<f32>(uv.x * 2.0 - 1.0, 1.0 - uv.y * 2.0);
    // Where the sky shows, nothing hazes: the texel holds its air for
    // fs_backdrop_up, its transmittance t as -1 - t, which fs_haze_up
    // reads as no haze.
    if globals.eye.w > 0.5 && shows_sky(at) {
        let sky = sky_air(view_ray(ndc));
        return vec4<f32>(sky.rgb, -1.0 - sky.w);
    }
    return haze(ndc, at);
}

// The haze at the full-size pixel `at`, whose clip position is `ndc`.
// CLEAR_AIR where nothing lies behind.
fn haze(ndc: vec2<f32>, at: vec2<i32>) -> vec4<f32> {
    if globals.eye.w < 0.5 { return CLEAR_AIR; }
    let size = vec2<i32>(textureDimensions(t_depth));
    let px = clamp(at, vec2<i32>(0), size - 1);
    let hit = behind(ndc, textureLoad(t_depth, px, 0));
    let reach = length(hit.xyz);
    if reach <= 0.0 { return CLEAR_AIR; }
    let dir = hit.xyz / reach;
    // Above AIR_CEILING only the air below it hazes: the march starts where
    // the ray comes down through it.
    let above = globals.eye.y - air.radii.w;
    var skip = 0.0;
    if above > 0.0 && dir.y < 0.0 { skip = min(above / -dir.y, reach); }
    let r = radius_at(min(globals.eye.y - skip * -dir.y, air.radii.w));
    // The sea is flat and endless, the atmosphere round. A ray just under
    // the horizon that misses the planet from where the march starts sees
    // sky there, as the backdrop just above it does: marched to the
    // atmosphere's top with nothing behind. Hazed to the flat sea instead,
    // it ran for hundreds of kilometres along the ground and left a bright
    // band under a dark seam at the horizon, wider the higher the eye.
    let bottom = air.radii.x;
    if hit.w < 0.5 && r * r * (dir.y * dir.y - 1.0) + bottom * bottom < 0.0 {
        let m = march(r, dir, distance_to_top(r, dir.y), SKY_STEPS);
        return vec4<f32>(m.light, 0.0);
    }
    // HAZE_STEPS of march with SampleCloudShadow at each step's middle,
    // near and low enough, dimming its single scattering
    // (SampleAtmosphere's `shadow`; multiple scattering stays lit). Looked
    // up as it goes: a per-step array of the shadow kept the composite's
    // fallback heavy enough to halve the occupancy of every pixel's.
    let start = globals.eye.xyz + dir * skip;
    let dist = (reach - skip) * air.radii.z * 0.001;
    var m = Marched(vec3<f32>(0.0), vec3<f32>(1.0));
    let world = 1000.0 / air.radii.z;
    for (var i = 0u; i < HAZE_STEPS; i++) {
        let span = step_span(i, HAZE_STEPS, dist);
        let t = 0.5 * (span.x + span.y);
        let along = t * world;
        let at = start + dir * along;
        var lit = 1.0;
        if along < SHADOW_DIST && at.y < SHADOW_TOP { lit = cloud_light(at); }
        let len = (span.y - span.x) * scatter_factor(dir, along * length(dir.xz));
        integrate(&m, march_step(r, dir, t), len, lit);
    }
    return vec4<f32>(m.light, dot(m.through, vec3<f32>(1.0 / 3.0)));
}

// fs_haze's quarter-size haze (light and transmittance; light 0 and
// transmittance 1 where it drew none) over the full-size pixels: the four
// nearest texels, bilinear, each weighed by how near its pixel's distance
// is to this one's, so haze does not bleed across silhouettes. Where none
// of them lies on this pixel's surface (a branch against far ground), the
// texel nearest its distance stands in when within HAZE_NEAREST_GAP of it,
// and only the rest march their own, in fs_haze_fallback. Marching in
// fs_haze_up held it to the march's occupancy over the whole screen, and
// even a full-screen fallback that only discarded cost 0.2 ms at that
// occupancy at 5K, so fs_haze lists the 4x4 blocks that may fall back and
// the fallback draws over those alone.
@group(2) @binding(0) var t_haze: texture_2d<f32>;
// Where no texel lies on a pixel's surface, one within this share of its
// distance stands in rather than the pixel marching its own: those pixels
// line every silhouette against far ground, and marching them cost 0.23 ms
// of the composite's 0.41 at 5K over Albion's forest.
const HAZE_NEAREST_GAP: f32 = 0.1;

// Distance along the view to what the depth shows; far for nothing drawn.
fn haze_distance(z: f32) -> f32 {
    if z <= 0.0 { return 1e9; }
    return globals.anchor.w / z;
}

// The last haze texel; the target is the depth's size over HAZE_SCALE,
// rounded up (pass.rs).
fn last_texel() -> vec2<i32> {
    return (vec2<i32>(textureDimensions(t_depth)) + HAZE_SCALE - 1) / HAZE_SCALE - 1;
}

// Texel i holds pixel HAZE_SCALE i: full-size pixel `px` lies between texel
// `base` and the next on each axis.
fn texel_base(px: vec2<i32>) -> vec2<i32> {
    return px / HAZE_SCALE;
}

// The distances of texel `base`, the next on x, on y, and on both.
fn texel_distances(base: vec2<i32>) -> vec4<f32> {
    return vec4<f32>(
        texel_distance(base),
        texel_distance(base + vec2<i32>(1, 0)),
        texel_distance(base + vec2<i32>(0, 1)),
        texel_distance(base + vec2<i32>(1, 1)),
    );
}

fn texel_distance(q: vec2<i32>) -> f32 {
    let size = vec2<i32>(textureDimensions(t_depth));
    let p = min(min(q, last_texel()) * HAZE_SCALE, size - 1);
    return haze_distance(textureLoad(t_depth, p, 0));
}

// Texel `q` as fs_haze wrote it: alpha below zero for the sky's.
fn haze_texel(q: vec2<i32>) -> vec4<f32> {
    return textureLoad(t_haze, min(q, last_texel()), 0);
}

// A sky texel holds the backdrop's air, not haze: none.
fn hazes(texel: vec4<f32>) -> vec4<f32> {
    return select(texel, CLEAR_AIR, texel.a < 0.0);
}

// Of the four texels at distances `dq`, the one nearest distance `d` that
// is not the sky's, when within HAZE_NEAREST_GAP of it (the first of
// equals); alpha -1 for none.
fn nearest_texel(d: f32, dq: vec4<f32>, t0: vec4<f32>, t1: vec4<f32>, t2: vec4<f32>, t3: vec4<f32>) -> vec4<f32> {
    let g = abs(dq - d) / d;
    var nearest = vec4<f32>(-1.0);
    var gap = HAZE_NEAREST_GAP;
    if t0.a >= 0.0 && g.x < gap { nearest = t0; gap = g.x; }
    if t1.a >= 0.0 && g.y < gap { nearest = t1; gap = g.y; }
    if t2.a >= 0.0 && g.z < gap { nearest = t2; gap = g.z; }
    if t3.a >= 0.0 && g.w < gap { nearest = t3; }
    return nearest;
}

// The four nearest texels' weights at pixel `px`, `d` away, against their
// distances `dq` (x, y, z, w: offsets 00, 10, 01, 11). Shared by the
// composite, the fallback and fs_haze's block list, so all agree on where
// a pixel is off every texel's surface.
fn texel_weights(px: vec2<i32>, d: f32, dq: vec4<f32>) -> vec4<f32> {
    let f = vec2<f32>(px % HAZE_SCALE) / f32(HAZE_SCALE);
    let bilinear = vec4<f32>((1.0 - f.x) * (1.0 - f.y), f.x * (1.0 - f.y), (1.0 - f.x) * f.y, f.x * f.y);
    // Within 2% of this pixel's distance counts as the same surface.
    let same = 1.0 - smoothstep(vec4<f32>(0.01), vec4<f32>(0.02), abs(dq - d) / d);
    return bilinear * same;
}

fn off_surface(w: vec4<f32>) -> bool {
    return w.x + w.y + w.z + w.w < 1e-3;
}

// Whether a pixel of haze texel `base`'s block may march its own: off
// every texel's surface with none near enough to stand in, sky pixels and
// all (the fallback tells those apart itself). The texels' haze is not
// written yet, so any with nothing drawn counts as the sky's, which can
// only list a block more, as can the gathers clamping past the target's
// edge. Its pixels by 2x2 gathers: one load each was 0.21 ms at 5K.
fn block_falls_back(base: vec2<i32>) -> bool {
    let size = vec2<f32>(textureDimensions(t_depth));
    let dq = texel_distances(base);
    let drawn = dq < vec4<f32>(1e9);
    var hit = false;
    for (var j = 0; j < HAZE_SCALE; j += 2) {
        for (var i = 0; i < HAZE_SCALE; i += 2) {
            let px = base * HAZE_SCALE + vec2<i32>(i, j);
            // Between pixels px and px + 1 on both axes; gather order
            // (0, 1), (1, 1), (1, 0), (0, 0).
            let z = textureGather(t_depth, s_pages, vec2<f32>(px + 1) / size);
            hit = hit || falls_back(px + vec2<i32>(0, 1), haze_distance(z.x), dq, drawn)
                || falls_back(px + vec2<i32>(1, 1), haze_distance(z.y), dq, drawn)
                || falls_back(px + vec2<i32>(1, 0), haze_distance(z.z), dq, drawn)
                || falls_back(px, haze_distance(z.w), dq, drawn);
        }
    }
    return hit;
}

// block_falls_back's pixel `px`, `d` away: off every texel's surface, and
// no texel `drawn` near enough to stand in.
fn falls_back(px: vec2<i32>, d: f32, dq: vec4<f32>, drawn: vec4<bool>) -> bool {
    // Most pixels have a texel near enough: the weights only for the rest.
    let near = drawn & (abs(dq - d) / d < vec4<f32>(HAZE_NEAREST_GAP));
    return !any(near) && off_surface(texel_weights(px, d, dq));
}

// Whether haze reaches the pixel at depth `z` at all: behind() reaches
// nowhere only where nothing drew, at or above the horizon or from under
// the sea. Drawn pixels skip its view ray.
fn hazed(in: SkyOut, z: f32) -> bool {
    return z > 0.0 || (view_ray(in.ndc).y < 0.0 && globals.eye.y > 0.0);
}

fn pixel(in: SkyOut) -> vec2<i32> {
    return clamp(vec2<i32>(in.clip.xy), vec2<i32>(0), vec2<i32>(textureDimensions(t_depth)) - 1);
}

// The clouds over the haze over what lies behind, as one premultiplied
// layer (the target's alpha stays).
fn over(clouds: vec4<f32>, haze: vec4<f32>) -> vec4<f32> {
    return vec4<f32>(clouds.rgb + clouds.a * haze.rgb, clouds.a * haze.a);
}

// HAZE and CLOUDS: what the Graphics pane shows of the two (pass.rs
// Pipelines::haze_up).
override HAZE: bool = true;
override CLOUDS: bool = true;

@fragment fn fs_haze_up(in: SkyOut) -> @location(0) vec4<f32> {
    let out = haze_up(in);
    if all(out == CLEAR_AIR) { discard; }
    return out;
}

// fs_haze_up's layer over the pixel; CLEAR_AIR where it adds nothing.
// Discards the pixels fs_haze_fallback draws.
fn haze_up(in: SkyOut) -> vec4<f32> {
    if globals.eye.w < 0.5 { return CLEAR_AIR; }
    let px = pixel(in);
    let z = textureLoad(t_depth, px, 0);
    var haze = CLEAR_AIR;
    if HAZE && hazed(in, z) {
        let base = texel_base(px);
        let d = haze_distance(z);
        let dq = texel_distances(base);
        let w = texel_weights(px, d, dq);
        let t0 = haze_texel(base);
        let t1 = haze_texel(base + vec2<i32>(1, 0));
        let t2 = haze_texel(base + vec2<i32>(0, 1));
        let t3 = haze_texel(base + vec2<i32>(1, 1));
        if off_surface(w) {
            haze = nearest_texel(d, dq, t0, t1, t2, t3);
            // None near: fs_haze_fallback marches it.
            if haze.a < 0.0 { discard; }
        } else {
            let sum = hazes(t0) * w.x + hazes(t1) * w.y + hazes(t2) * w.z + hazes(t3) * w.w;
            haze = sum / (w.x + w.y + w.z + w.w);
        }
    }
    var clouds = CLEAR_AIR;
    if CLOUDS { clouds = clouds_over(in, z); }
    return over(clouds, haze);
}

// The blocks fs_haze_fallback draws over: a draw's indirect arguments
// (vertex count, which pass.rs sets to 6, and instance count, one per
// block) and each block's NDC rectangle, one at most per haze texel;
// fs_haze's own group 2 (bindings past t_haze's).
struct BlockArgs {
    vertices: u32,
    instances: atomic<u32>,
    first_vertex: u32,
    first_instance: u32,
}
@group(2) @binding(1) var<storage, read_write> block_args: BlockArgs;
@group(2) @binding(2) var<storage, read_write> block_list: array<vec4<f32>>;

// fs_haze's texel `base`: lists its block when a pixel of it may fall
// back.
fn list_block(base: vec2<i32>) {
    if block_falls_back(base) {
        let size = vec2<f32>(textureDimensions(t_depth));
        let lo = vec2<f32>(base * HAZE_SCALE) / size;
        let hi = min(vec2<f32>((base + 1) * HAZE_SCALE) / size, vec2<f32>(1.0));
        let i = atomicAdd(&block_args.instances, 1u);
        block_list[i] = vec4<f32>(lo.x * 2.0 - 1.0, 1.0 - lo.y * 2.0, hi.x * 2.0 - 1.0, 1.0 - hi.y * 2.0);
    }
}

// One block of block_list, two triangles.
@vertex fn vs_block(@builtin(vertex_index) vertex: u32, @location(0) rect: vec4<f32>) -> SkyOut {
    let corners = array<vec2<f32>, 6>(
        vec2<f32>(0.0, 0.0), vec2<f32>(1.0, 0.0), vec2<f32>(0.0, 1.0),
        vec2<f32>(0.0, 1.0), vec2<f32>(1.0, 0.0), vec2<f32>(1.0, 1.0),
    );
    let c = corners[vertex];
    let p = vec2<f32>(mix(rect.x, rect.z, c.x), mix(rect.y, rect.w, c.y));
    var out: SkyOut;
    out.clip = vec4<f32>(p, 0.0, 1.0);
    out.ndc = p;
    return out;
}

// fs_haze_up's pixels that fall back, over the listed blocks: marched
// here, clouds and all.
@fragment fn fs_haze_fallback(in: SkyOut) -> @location(0) vec4<f32> {
    return haze_fallback(in);
}

// fs_haze_fallback's layer; discards the pixels fs_haze_up drew.
fn haze_fallback(in: SkyOut) -> vec4<f32> {
    if globals.eye.w < 0.5 { discard; }
    let px = pixel(in);
    let z = textureLoad(t_depth, px, 0);
    if !hazed(in, z) { discard; }
    let base = texel_base(px);
    let d = haze_distance(z);
    let dq = texel_distances(base);
    if !off_surface(texel_weights(px, d, dq)) { discard; }
    let t0 = haze_texel(base);
    let t1 = haze_texel(base + vec2<i32>(1, 0));
    let t2 = haze_texel(base + vec2<i32>(0, 1));
    let t3 = haze_texel(base + vec2<i32>(1, 1));
    if nearest_texel(d, dq, t0, t1, t2, t3).a >= 0.0 { discard; }
    var clouds = CLEAR_AIR;
    if CLOUDS { clouds = clouds_over(in, z); }
    return over(clouds, haze(in.ndc, px));
}
