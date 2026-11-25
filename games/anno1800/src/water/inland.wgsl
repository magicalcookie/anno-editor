// --- Inland water -----------------------------------------------------------
// Anno 1800's rivers and lakes (water_inland_water.fx psRiver/psLake), in
// the sea's pass and with its bindings 0-10: lakes on the editor's water
// lattice (water::inland), each river on its own strip (water::Strips) as
// the game builds one mesh per River record. The lattice is the flow map
// under both. Rivers carry the ocean's small-wave gradients and the foam
// map along the flow map (GetDirectionalWaves), lakes ripple with the small
// waves at twice the ocean's frequency. Each body's colour and turbidity
// come from its record; rivers fade in at their source and out at their end
// as its RenderFlags say (the strips' vertices carry the fade). The
// island's painted water maps add foam to both and duckweed to lakes, and
// say how deep the water is; the island reflects in both (water_ssr). Left
// out: rain ripples, cloud shadows, the polluted-water tint and the sky's
// cloud box beyond what sea_sky draws.

struct InlandParams {
    // Corner (0, 0) X and Z, corner spacing and its inverse.
    grid: vec4<f32>,
    // Corners per row, rows.
    size: vec4<f32>,
}

struct InlandCorner {
    y: f32,
    look: u32,
    // Flow speed, a share of MAX_RIVER_FLOW_SPEED: the flow map's alpha.
    speed: f32,
    // vsRiver's Alpha: 0 at a river end its record fades.
    fade: f32,
    flow: vec2<f32>,
}

// cInlandWaterColor; w: cInlandWaterTurbidity.
struct InlandLook {
    color: vec4<f32>,
}

@group(1) @binding(11) var<uniform> inland: InlandParams;
@group(1) @binding(12) var<storage, read> i_corners: array<InlandCorner>;
@group(1) @binding(13) var<storage, read> i_looks: array<InlandLook>;
// water::StripVertex, packed as stored: position, look, normal, fade, across.
struct InlandStripVertex {
    x: f32, y: f32, z: f32,
    look: u32,
    nx: f32, ny: f32, nz: f32,
    fade: f32,
    across: f32,
}
@group(1) @binding(30) var<storage, read> i_strips: array<InlandStripVertex>;
@group(1) @binding(21) var i_duckweed: texture_2d<f32>;
@group(1) @binding(22) var i_duckweed_norm: texture_2d<f32>;

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
    out.world = vec3<f32>(xz.x, c.y, xz.y);
    out.look = c.look;
    out.fade = c.fade;
    out.clip = globals.clip * vec4<f32>(out.world, 1.0);
    return out;
}

// A river's own strip (vsRiver over the record's InlandWaterVertex rows):
// the flow and the rest still come from the lattice under it.
@vertex fn vs_strip(@builtin(vertex_index) vertex: u32) -> InlandOutput {
    let v = i_strips[vertex];
    var out: InlandOutput;
    out.world = vec3<f32>(v.x, v.y, v.z);
    out.look = v.look;
    out.fade = v.fade;
    out.clip = globals.clip * vec4<f32>(out.world, 1.0);
    return out;
}

fn linear_step(lo: f32, hi: f32, x: f32) -> f32 {
    return clamp((x - lo) / (hi - lo), 0.0, 1.0);
}

fn triangle_wave(x: f32) -> f32 {
    return abs(fract(x - 0.5) * 2.0 - 1.0);
}

// GetSlowWaterFactor: how far a pixel is in "slow flow" mode, by the flow
// map's speed.
fn slow_water(speed: f32) -> f32 {
    let f = linear_step(0.8, 1.0, 1.0 - speed);
    return f * f;
}

// GetFlowDirections: the four flow map texels around `xz` (the lattice's
// corners), their bilinear weights and texel positions.
struct Flow {
    dirs: array<vec2<f32>, 4>,
    speeds: vec4<f32>,
    texels: array<vec2<i32>, 4>,
    weights: vec4<f32>,
}

fn inland_flow(xz: vec2<f32>) -> Flow {
    let t = max((xz - inland.grid.xy) * inland.grid.w, vec2<f32>(0.0));
    let base = vec2<i32>(floor(t));
    let f = t - floor(t);
    let size = vec2<i32>(inland.size.xy);
    let offsets = array<vec2<i32>, 4>(vec2<i32>(0, 0), vec2<i32>(0, 1), vec2<i32>(1, 0), vec2<i32>(1, 1));
    var out: Flow;
    out.weights = vec4<f32>((1.0 - f.x) * (1.0 - f.y), (1.0 - f.x) * f.y, f.x * (1.0 - f.y), f.x * f.y);
    for (var k = 0; k < 4; k++) {
        let p = clamp(base + offsets[k], vec2<i32>(0), size - 1);
        let c = i_corners[u32(p.y * size.x + p.x)];
        out.dirs[k] = c.flow;
        out.speeds[k] = c.speed;
        out.texels[k] = base + offsets[k];
    }
    return out;
}

// GetWaveTexCoord: `uv` turned into one texel's flow and scrolled along it
// by its speed; `twist` offsets odd texels so their tiles don't line up,
// `slow_scale` stretches the pattern where the water is slow.
fn wave_uv(uv: vec2<f32>, dir: vec2<f32>, speed_share: f32, texel: vec2<i32>, max_speed: f32, twist: bool, slow_scale: bool, seconds: f32) -> vec2<f32> {
    // Flow speed from the spline's control points, a little kept for foam.
    let flow_speed = max(0.01, speed_share * speed_share);
    let speed = max_speed * length(dir) * flow_speed;
    var rot = vec2<f32>(1.0, 0.0);
    if dot(dir, dir) > 1e-12 {
        rot = normalize(dir * vec2<f32>(-1.0, 1.0));
    }
    var out = vec2<f32>(uv.x * rot.x - uv.y * rot.y, uv.x * rot.y + uv.y * rot.x) + vec2<f32>(speed * seconds, 0.0);
    out = fract(out);
    if twist {
        if (texel.x & 1) != 0 { out += 0.5; }
        if (texel.y & 1) != 0 { out += 0.25; }
    }
    if slow_scale {
        out *= 1.0 + slow_water(speed_share);
    }
    return out;
}

// GetDirectionalWaves: `tex` sampled once per flow texel along its flow,
// blended by the texels' weights, at the derivatives of the unturned `uv`.
fn directional(tex: texture_2d<f32>, uv: vec2<f32>, flow: Flow, max_speed: f32, twist: bool, lod_bias: f32, slow_scale: bool, seconds: f32) -> vec4<f32> {
    let dx = dpdx(uv) * lod_bias;
    let dy = dpdy(uv) * lod_bias;
    var sum = vec4<f32>(0.0);
    for (var k = 0; k < 4; k++) {
        let at = wave_uv(uv, flow.dirs[k], flow.speeds[k], flow.texels[k], max_speed, twist, slow_scale, seconds);
        sum += flow.weights[k] * textureSampleGrad(tex, s_wrap, at, dx, dy);
    }
    return sum;
}

// The refracted scene point and colour through `pixel`, offset by
// `offset` (UV units) toward where the seabed lies; straight through where
// the offset point stands in front of the water.
struct Refraction {
    pixel: vec2<f32>,
    ws: vec3<f32>,
}

fn inland_refraction(pixel: vec2<f32>, offset: vec2<f32>, surface_z: f32) -> Refraction {
    var p = pixel + offset * globals.viewport.xy;
    var depth = sea_depth(p);
    if depth > surface_z {
        p = pixel;
        depth = sea_depth(pixel);
    }
    var out: Refraction;
    out.pixel = p;
    out.ws = unproject(sea_ndc(pixel), max(depth, 1e-7));
    return out;
}

// Absorption over the light's path and the view's, by the preset's
// ViewDepth, LightDepth and Absorption, and the body's colour.
struct Murk {
    view: f32,
    absorption: vec3<f32>,
}

fn murk(world: vec3<f32>, eye: vec3<f32>, refracted: vec3<f32>, depth: f32, color: vec3<f32>) -> Murk {
    let view_range = sea.depths.x * (1.0 - 0.8 * depth);
    let light_range = sea.depths.y * (1.0 - 0.8 * depth);
    var out: Murk;
    out.view = pow(clamp((distance(refracted, eye) - distance(world, eye)) / view_range, 0.0, 1.0), 3.0);
    let light_y = max(sea_sun().y, 0.05);
    let light = clamp(((world.y - refracted.y) / light_y) / light_range, 0.0, 1.0);
    out.absorption = pow(vec3<f32>(1.0 - light), max(vec3<f32>(0.2), vec3<f32>(1.0) - color) * sea.shallow.w);
    return out;
}

@fragment fn fs_river(in: InlandOutput) -> @location(0) vec4<f32> {
    let world = in.world;
    let eye = sea_eye(world);
    let to_eye = normalize(eye - world);
    let distance3 = distance(world, eye);
    let look = i_looks[in.look].color;
    let seconds = sea.depths.w;
    let infos = water_infos(world.xz);
    let depth = infos.r;
    let day = sea_day();
    let night = clamp(day, 0.1, 1.0);
    let fft = world.xz * 2.0 * sea.uv.x;
    let flow = inland_flow(world.xz);
    var g = directional(s_small_gradients, fft, flow, 0.15, false, 1.0, true, seconds);
    let speed = dot(flow.weights, flow.speeds);
    let slow = slow_water(speed);
    // Calmer where slow, and toward the horizon (aliasing).
    g = vec4<f32>(g.xy * (0.01 + 0.99 * pow(clamp(speed * 4.0, 0.0, 1.0), 4.0)) * sqrt(abs(to_eye.y)), g.zw);
    var n = normalize(vec3<f32>(g.x, sea.uv.w * 0.3, g.y));
    // Foam: the map's channels carried along the flow, a little noise
    // scrolling through; fast water foams where foam is painted, and
    // thickly painted foam foams anyway.
    let disturb = textureSample(s_noise, s_wrap, world.xz * 0.125 + seconds * 0.05).rg * 0.02;
    let foam_sample = directional(s_foam, fft * 3.0 + disturb, flow, 0.6, true, 0.7, false, seconds);
    let control_point_foam = clamp(speed * speed * 4.0, 0.0, 1.0);
    var foam_intensity = clamp(control_point_foam * (0.25 + 4.0 * infos.a), 0.0, 1.0) + clamp(infos.a - 0.5, 0.0, 1.0);
    // Normals facing away from the eye turn toward it.
    let ndv = dot(n, to_eye);
    if ndv < 0.5 {
        n = normalize(mix(n, reflect(-n, vec3<f32>(0.0, 1.0, 0.0)), 1.0 - clamp(ndv / 0.5, 0.0, 1.0)));
    }
    let detail = 0.2 + pow(1.0 - clamp(distance3 / 128.0, 0.0, 1.0), 2.0) * 0.8;
    let roughness = 1.2 - detail * 0.2;
    let reflected = reflect(-to_eye, n);
    let fresnel = mix(0.2, 1.0, pow(1.0 - clamp(dot(n, to_eye), 0.0, 1.0), 3.5));
    var reflection = sea_sky(reflected);
    {
        let uv = world.xz * 0.005 - (to_eye.xz / max(to_eye.y, 0.05)) * 0.3 + reflected.xz * 0.5 * 0.3;
        reflection = fake_clouds(reflection, uv, 0.5, 0.5, 0.5, to_eye, reflected, day);
    }
    // The island over the sky (psRiver's GetScreenSpaceReflection, read
    // n.xz * 64 pixels off); where it shows, the water reflects up to 60%
    // more, to present it.
    let ssr = water_ssr(world, to_eye, n.xz * 64.0);
    reflection = mix(reflection, ssr.rgb, ssr.a);
    let sun_shadow = clamp(shadow(world, vec3<f32>(0.0, 1.0, 0.0)), 0.0, 1.0);
    let pixel = in.clip.xy;
    let scene_ws = unproject(sea_ndc(pixel), max(sea_depth(pixel), 1e-7));
    let offset = n.xz * (1.0 / distance3 / 4.0) * 0.6 * clamp(world.y - scene_ws.y, 0.0, 1.0);
    let r = inland_refraction(pixel, offset, in.clip.z);
    // Foam heaps up where the water meets the ground.
    {
        let border = pow(clamp(1.0 - distance(world, r.ws), 0.0, 1.0), 3.0);
        foam_intensity = clamp(foam_intensity + clamp(foam_intensity * 8.0, 0.0, 1.0) * border, 0.0, 1.0);
    }
    var foam_tex = pow(foam_sample.b * 2.0, 2.0) * triangle_wave(linear_step(0.0, 0.8, foam_intensity));
    foam_tex += pow(foam_sample.r * 2.0, 4.0) * triangle_wave(linear_step(0.4, 0.9, foam_intensity));
    foam_tex += pow(foam_sample.g * 2.0, 2.0) * linear_step(0.7, 1.0, foam_intensity);
    let foam = clamp(foam_tex, 0.0, 1.0);
    let m = murk(world, eye, r.ws, depth, look.rgb);
    // Fast, fake scattering: the body's colour, darker at night (but never
    // black) and in shade.
    var water = look.rgb * max(day, 0.025) * mix(0.1, 1.0, sun_shadow);
    water.g *= 1.5 - 0.5 * roughness;
    let darkening = mix(clamp(1.5 - 0.5 * to_eye.y - 0.5 * m.view, 0.0, 1.0), 1.0, slow);
    var refraction = max(sea_scene(r.pixel), vec3<f32>(0.0));
    // Slow water: a blurred floor, by how murky it is and how deep the
    // water-info map says it is.
    if slow > 0.0 {
        let radius = min(3.0, 1024.0 * m.view) * clamp(depth * 2.0, 0.0, 1.0);
        if radius > 0.0 {
            var blurred = refraction;
            for (var y = -1; y <= 1; y++) {
                for (var x = -1; x <= 1; x++) {
                    blurred += max(sea_scene(r.pixel + vec2<f32>(f32(x), f32(y)) * radius), vec3<f32>(0.0));
                }
            }
            refraction = mix(refraction, blurred / 10.0, slow);
        }
    }
    refraction = mix(refraction * m.absorption, water, clamp(pow(m.view, 0.08) * look.w, 0.0, 1.0)) * darkening;
    let foam_mask = pow(1.0 - foam, 4.0);
    let reflectiveness = clamp(fresnel * (0.5 + sun_shadow * 0.5) * (1.0 + ssr.a * 0.6), 0.0, 1.0);
    var color = mix(refraction, reflection, reflectiveness) * foam_mask;
    // Two sun spots on a flattened normal, broader on slow water.
    {
        let h = normalize(sea_sun() + to_eye);
        let s = normalize(vec3<f32>(n.x, n.y * mix(12.0, 3.0, slow), n.z));
        let nh = clamp(dot(s, h), 0.0, 1.0);
        let sun = vec3<f32>(0.85, 0.95, 1.0) * lights.sun_color.rgb * sun_shadow * foam_mask;
        color += mix(1.0, 0.4, slow) * pow(nh, 16384.0) * sun;
        color += mix(0.03, 0.1, slow) * pow(nh, mix(1024.0, 32.0, slow)) * sun;
    }
    color = mix(color, foam_light(sun_shadow, night), foam);
    // Clear at the shore, sooner for clearer water.
    let opacity = clamp(distance(world, scene_ws) * 6.0 * look.w, 0.0, 1.0);
    color = mix(sea_scene(pixel), color, opacity);
    return vec4<f32>(fog(color, world), in.fade);
}

@fragment fn fs_lake(in: InlandOutput) -> @location(0) vec4<f32> {
    let world = in.world;
    let eye = sea_eye(world);
    let to_eye = normalize(eye - world);
    let distance3 = distance(world, eye);
    let look = i_looks[in.look].color;
    let seconds = sea.depths.w;
    let infos = water_infos(world.xz);
    let depth = infos.r;
    let day = sea_day();
    let night = clamp(day, 0.1, 1.0);
    // The small waves at twice the ocean's frequency, toned down.
    let fft = world.xz * 2.0 * sea.uv.x;
    let g = textureSample(s_small_gradients, s_wrap, fft);
    var n = normalize(vec3<f32>(g.x, sea.uv.w * 9.0, g.y));
    let coarse = n;
    let detail = 0.2 + pow(1.0 - clamp(distance3 / 128.0, 0.0, 1.0), 2.0) * 0.8;
    let detail_uv = fft * 3.0;
    let g2 = textureSampleGrad(s_small_gradients, s_wrap, detail_uv, dpdx(detail_uv) * 0.5, dpdy(detail_uv) * 0.5);
    n = normalize(n + normalize(vec3<f32>(g2.x, sea.uv.w * 2.5, g2.y)) * detail);
    // Wind gusts: noise that roughens the surface in patches.
    let gust = 1.0 + min(2.0, pow(abs(textureSample(s_noise2, s_wrap, world.xz * 0.03 + seconds * 0.01).g), 6.0) * 100.0);
    n = normalize(n * vec3<f32>(1.0, gust, 1.0));
    let reflected = reflect(-to_eye, n);
    let fresnel = mix(0.2, 1.0, pow(1.0 - clamp(dot(n, to_eye), 0.0, 1.0), 3.5));
    let roughness = 1.2 - detail * 0.2;
    let offset_factor = pow(1.0 - clamp(distance3 / 256.0, 0.0, 1.0), 2.0) * 0.1;
    // The island over the sky (psLake's GetScreenSpaceReflection, read
    // n.xz * 120 pixels off).
    let ssr = water_ssr(world, to_eye, n.xz * 120.0);
    let reflection = mix(sea_sky(reflected), ssr.rgb, ssr.a);
    let sun_shadow = clamp(shadow(world, vec3<f32>(0.0, 1.0, 0.0)), 0.0, 1.0);
    let pixel = in.clip.xy;
    let scene_ws = unproject(sea_ndc(pixel), max(sea_depth(pixel), 1e-7));
    let r = inland_refraction(pixel, n.xz * offset_factor * 0.6 * clamp(world.y - scene_ws.y, 0.0, 1.0), in.clip.z);
    var refraction = max(sea_scene(r.pixel), vec3<f32>(0.0));
    // Caustics as the ocean's, at four times the scale and measured from
    // the lake's own surface.
    let floor_ws = vec3<f32>(r.ws.x, r.ws.y - world.y, r.ws.z);
    let caustics = dot(refraction, vec3<f32>(0.33)) * lights.sun_color.rgb * sea_caustics(floor_ws, 4.0) * sun_shadow;
    let m = murk(world, eye, r.ws, depth, look.rgb);
    var water = look.rgb * 0.1 * night * sun_shadow;
    water.g *= 1.5 - 0.5 * roughness;
    refraction = mix((refraction + caustics) * m.absorption, water, clamp(pow(m.view, 0.08) * look.w, 0.0, 1.0));
    refraction *= clamp(1.5 - 0.5 * to_eye.y - 0.5 * m.view, 0.0, 1.0);
    var color = mix(refraction, reflection, fresnel * (0.5 + sun_shadow * 0.5));
    let spot = pow(clamp(dot(reflected, sea_sun()), 0.0, 1.0), 1400.0);
    color += vec3<f32>(0.85, 0.95, 1.0) * lights.sun_color.rgb * spot * sun_shadow * 6.0;
    // Painted foam, its texture stirred by slow 3D noise.
    {
        let anim = clamp(pow(noise3d(vec3<f32>(world.xz * 0.13 + seconds * 0.01, seconds * 0.025)), 2.0) * 2.0, 0.0, 1.0);
        let anim2 = 0.3 + clamp(pow(noise3d(vec3<f32>(world.xz * 0.097, seconds * 0.025)), 2.0) * 3.0, 0.0, 1.0) * 0.7;
        let foam_uv = world.xz * 0.2;
        let fine = textureSampleGrad(s_foam, s_wrap, foam_uv + coarse.xz * 0.06, dpdx(foam_uv) * 0.6, dpdy(foam_uv) * 0.6);
        let painted = pow(mix(fine.b, mix(fine.g, fine.r, anim), infos.a * infos.a) * infos.a * 2.5 * anim2, 1.5);
        color = mix(color, foam_light(sun_shadow, night), clamp(painted, 0.0, 1.0));
    }
    // Duckweed where the map's third byte paints it (ComputeDuckweed),
    // lit by the sun and the sky. The lake's texture coordinates are the
    // small waves' (TODO: the game's lake vertices carry their own, not
    // traced), six times finer.
    {
        let uv = fft * 6.0;
        let weed = textureSample(i_duckweed, s_wrap, uv + coarse.xz * 0.02);
        let cover = clamp(2.0 * (clamp(weed.a, 0.0, 1.0) - 1.0) + 3.0 * infos.b, 0.0, 1.0);
        if cover > 0.0 {
            let ns = textureSample(i_duckweed_norm, s_wrap, uv + coarse.xz * 0.02);
            let wn = normalize(ns.xzy * 2.0 - 1.0);
            let sun = sea_sun();
            let diffuse = clamp(dot(n, sun), 0.0, 1.0) * max(lights.sun_color.rgb - lights.sky.rgb, vec3<f32>(0.0))
                * clamp(sun_shadow + 0.33, 0.0, 1.0);
            let spec = pow(clamp(dot(reflect(-to_eye, wn), sun), 0.0, 1.0), 250.0) * 2.0 * ns.a * sun_shadow;
            let shade = diffuse * clamp(dot(wn, sun), 0.0, 1.0) + lights.sky.rgb
                + spec * vec3<f32>(0.85, 0.95, 1.0) * lights.sun_color.rgb;
            color = mix(color, pow(weed.rgb, vec3<f32>(2.2)) * shade, cover);
        }
    }
    let opacity = clamp((world.y - scene_ws.y) * 8.0 * look.w, 0.0, 1.0);
    color = mix(sea_scene(pixel), color, opacity);
    return vec4<f32>(fog(color, world), 1.0);
}
