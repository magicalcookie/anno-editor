// Anno 1800's sky as the camera sees it (sky.fx psSkyBox and psMoon over
// functions_scattering.fxh ComputeSkyColorHelper), and the haze over what
// stands in it (ComputeScattering, ApplyScattering). Follows the frame
// prelude. The cloud layer is baked per preset and hour (sky::pass);
// what depends on the camera is evaluated here per pixel.

struct Look {
    // cScatteringZenithColor; w: 1 with a cloud layer
    zenith: vec4<f32>,
    // cScatteringHorizonColor; w: 1 when the sun is drawn
    horizon: vec4<f32>,
    // cScatteringHorizonFalloff
    horizon_falloff: vec4<f32>,
    // cScatteringSunColor; w: cScatteringSunFalloff
    sun: vec4<f32>,
    // toward the sun (-cLgtDirectionUncapped); w: 1 / (cos inner - cos outer)
    toward: vec4<f32>,
    // cTimeOfDay.y, cDayNight.z, cDayNight.w, cos outer angle
    day: vec4<f32>,
    // ComputeScattering: start, factor, intensity, distance falloff
    haze: vec4<f32>,
    // the far plane the camera's relative height is taken against; the
    // moon's weight; the camera lift per world unit (0.0008); the game
    // camera's top height, which the sky's height is capped at
    camera: vec4<f32>,
}

@group(1) @binding(0) var<uniform> look: Look;
@group(1) @binding(1) var t_cloud_layer: texture_2d<f32>;
@group(1) @binding(2) var t_moon: texture_2d<f32>;
@group(1) @binding(3) var s_linear: sampler;

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

// The camera's height as the sky takes it: no higher than the game's
// camera goes. Past that the sky stays as the game's top zoom shows it.
fn sky_height() -> f32 {
    return clamp(globals.eye.y, 0.0, look.camera.w);
}

// SKY_SCATTERING_OPTIMIZATION_FACTOR: the sky pulled down by the camera's
// height.
fn camera_lift() -> f32 {
    return sky_height() * look.camera.z;
}

// ComputeSkyColorHelper toward `d` at scattering depth `depth`.
fn sky_color(d: vec3<f32>, depth: f32) -> vec3<f32> {
    let n = normalize(d + vec3<f32>(0.0, camera_lift(), 0.0));
    // cScatteringRelativeCameraHeight and CameraHeightFactor, as the sky
    // bake takes them: the camera's height over the far plane.
    let relative = sky_height() / look.camera.x;
    let horizon_amount = 1.0 - clamp((n.y + relative) / (relative + 1.0), 0.0, 1.0);
    let v_dot_l = max(dot(n, look.toward.xyz), 0.0);
    let illumination = 0.5 + v_dot_l * v_dot_l * v_dot_l * (look.day.y + look.day.z * 1.2);
    let toward_horizon = pow(vec3<f32>(horizon_amount), look.horizon_falloff.rgb) * depth;
    var color = mix(look.zenith.rgb, look.horizon.rgb, toward_horizon) * illumination;
    if look.horizon.w > 0.5 {
        let amount = clamp(v_dot_l - look.day.w, 0.0, 1.0) * look.toward.w
            * pow(clamp(look.day.x + look.day.z, 0.0, 1.0), 0.2);
        color += look.sun.rgb * pow(max(amount, 0.0), look.sun.w) * pow(depth, 30.0);
    }
    return color;
}

// The cloud layer toward unit `u`, laid out as light::SkyMap.
fn clouds(u: vec3<f32>) -> vec4<f32> {
    let lat = asin(clamp(u.y, -1.0, 1.0));
    let s = sign(lat) * sqrt(abs(lat) / 1.5707963);
    let rows = f32(textureDimensions(t_cloud_layer).y);
    let v = clamp((1.0 - s) * 0.5, 0.5 / rows, 1.0 - 0.5 / rows);
    let uv = vec2<f32>(atan2(u.z, u.x) / 6.2831853 + 0.5, v);
    return textureSampleLevel(t_cloud_layer, s_linear, uv, 0.0);
}

fn view_ray(ndc: vec2<f32>) -> vec3<f32> {
    let near = unproject(ndc, 1.0);
    return normalize(unproject(ndc, 1e-3) - near);
}

// psSkyBox toward `d`: the scattered sky with the cloud layer, faded
// toward the lifted horizon, at twice its brightness; a the clouds' cover.
fn dome(d: vec3<f32>) -> vec4<f32> {
    var color = sky_color(d, 1.0);
    let at = d + vec3<f32>(0.0, camera_lift(), 0.0);
    var cover = 0.0;
    if look.zenith.w > 0.5 {
        let cloud = clouds(normalize(at));
        cover = cloud.a;
        let fade = 1.0 - pow(abs(1.0 - at.y), 64.0);
        color = mix(color, cloud.rgb, clamp(cloud.a * fade, 0.0, 1.0));
    }
    return vec4<f32>(color, cover);
}

// The dome, then psMoon over it: the moon's quad toward the light by
// night, tinted by the sky near the horizon and shrouded by cloud. Above
// the horizon; below it the kit's sea stays, and the haze pass draws the
// dome again where that sea runs past the far plane.
@fragment fn fs_backdrop(in: SkyOut) -> @location(0) vec4<f32> {
    if globals.eye.w < 0.5 { discard; }
    let d = view_ray(in.ndc);
    if d.y < 0.0 { discard; }
    let sky = dome(d);
    var color = sky.rgb;
    let cover = sky.a;
    if look.camera.y > 0.0 {
        // The quad faces the camera: its plane is the view plane, its axes
        // the view's right and up.
        let forward = view_ray(vec2<f32>(0.0, 0.0));
        let right = normalize(unproject(vec2<f32>(1.0, 0.0), 1e-3) - unproject(vec2<f32>(-1.0, 0.0), 1e-3));
        let up = normalize(unproject(vec2<f32>(0.0, 1.0), 1e-3) - unproject(vec2<f32>(0.0, -1.0), 1e-3));
        let center = normalize(look.toward.xyz - vec3<f32>(0.0, camera_lift(), 0.0)) * MOON_DISTANCE;
        let along = dot(d, forward);
        // Behind the camera the view plane through it meets no ray ahead.
        if along > 0.0 && dot(center, forward) > 0.0 {
            let p = d * (dot(center, forward) / along) - center;
            let q = vec2<f32>(dot(p, right), dot(p, up)) / MOON_SPAN;
            if all(abs(q) <= vec2<f32>(1.0)) {
                let tex = textureSampleLevel(t_moon, s_linear, q * vec2<f32>(0.5, -0.5) + 0.5, 0.0);
                let bright = pow(abs(tex.rgb * 2.0), vec3<f32>(2.0));
                let tint = mix(bright, sky_color(d, 1.0), 1.0 - clamp(d.y + camera_lift(), 0.0, 1.0));
                let alpha = clamp(tex.a * clamp(1.0 - cover * 1.5, 0.0, 1.0), 0.0, 1.0) * look.camera.y;
                color = mix(color, tint, alpha);
            }
        }
    }
    return vec4<f32>(color, 1.0);
}

// From the camera to what a pixel shows: the surface its depth gives, but
// no farther than the sea surface (the depth here is the opaque scene's,
// under the water), or where nothing drew below the horizon, the sea
// level, which the kit's far sea stands for; the game's ocean reaches the
// horizon and hazes with it. Zero for the sky.
fn behind(ndc: vec2<f32>, z: f32) -> vec3<f32> {
    let dir = view_ray(ndc);
    let eye = globals.eye.xyz;
    var to_sea = 0.0;
    if dir.y < 0.0 && eye.y > 0.0 { to_sea = eye.y / -dir.y; }
    if z > 0.0 {
        let offset = unproject(ndc, z) - eye;
        if to_sea > 0.0 && length(offset) > to_sea { return dir * to_sea; }
        return offset;
    }
    return dir * to_sea;
}

// ComputeScattering and ApplyScattering without VOLUMETRIC_SCATTERING
// (sample 0.5): the sky's color for the distance over the surface, by
// how far it lies past the start toward the far plane. One extinction for
// all three channels: the blend has a single factor for the color behind.
@fragment fn fs_haze(in: SkyOut) -> @location(0) vec4<f32> {
    if globals.eye.w < 0.5 { discard; }
    let size = vec2<i32>(textureDimensions(t_depth));
    let px = clamp(vec2<i32>(in.clip.xy), vec2<i32>(0), size - 1);
    let offset = behind(in.ndc, textureLoad(t_depth, px, 0));
    let dist = length(offset);
    if dist <= 0.0 { discard; }
    let d = offset / dist;
    // The haze thickens with distance from the camera, which in the game
    // stands no higher than its top zoom. From the editor's higher camera
    // the stretch of the ray above that height does not count, as the sky
    // takes the capped height: the haze is what the game's highest camera
    // on the same ray would see. Counted in full, a camera a kilometre up
    // hazed the whole island over.
    var above = 0.0;
    if globals.eye.y > look.camera.w && d.y < 0.0 {
        above = (globals.eye.y - look.camera.w) / -d.y;
    }
    let hazed = max(dist - above, 0.0);
    // The game's ocean is drawn only so far around the camera, and the sky
    // dome shows past it: the lift exists, says constants_scattering.fxh,
    // to pull the sky's horizon down to where that water ends. The dip it
    // gives has a tangent of 0.0008 times the camera's height, which is
    // the edge of a sea 1 / 0.0008 = 1250 units out at any height. The
    // kit's sea runs on to the true horizon and stood above the sky's,
    // so past that edge the dome takes its place, measured from the same
    // capped camera as the haze. The game's haze is whole by then and
    // hides the edge; with the far plane untraced it may not be here, so
    // the water gives way over its last 40% rather than at a line, which
    // from the editor's higher camera drew a ring around the island.
    let open_sea = d.y < 0.0 && abs(offset.y + globals.eye.y) < 1e-3 * dist;
    var edge = 0.0;
    if open_sea {
        let reach = 1.0 / look.camera.z;
        edge = smoothstep(0.6 * reach, reach, hazed * length(d.xz));
    }
    let depth = clamp((hazed - look.haze.x) * look.haze.y, 0.0, 1.0);
    let color = sky_color(d, depth);
    let amount = pow(clamp(depth * look.haze.z, 0.0, 1.0), look.haze.w);
    if edge > 0.0 {
        let sky = dome(d).rgb;
        return vec4<f32>(mix(color * amount, sky, edge), (1.0 - amount) * (1.0 - edge));
    }
    return vec4<f32>(color * amount, 1.0 - amount);
}
