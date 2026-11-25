// Anno 2070's sky as the camera sees it (sky.fx ps_Sky over
// ComputeSkyColor): the scattering's gradient from zenith to horizon, the
// sun's glow, and clouds from three turning samples of a noise volume.
// Follows the frame prelude; every view ray is a dome vertex's direction,
// the dome centred on the camera. Below the horizon the gradient holds the
// horizon's colour and the clouds thin out to none.

struct Look {
    // cZenithColor, gamma; w: the far plane the eye's height is taken against
    zenith: vec4<f32>,
    // cHorizonColor; w: how far the eye may back off the anchor
    horizon: vec4<f32>,
    // cHorizonFallOff; w: cSunFallOff
    horizon_falloff: vec4<f32>,
    // cSunColor; w: one over (cos inner − cos outer)
    sun: vec4<f32>,
    // toward the sun; w: cos outer
    toward: vec4<f32>,
    // cCloudColor; w: the cloud density
    cloud: vec4<f32>,
    // the two cloud turns (cMtxCloudRotate0/1), radians about y
    turn: vec4<f32>,
}

@group(1) @binding(0) var<uniform> look: Look;
@group(1) @binding(1) var t_noise: texture_3d<f32>;
@group(1) @binding(2) var s_noise: sampler;

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

fn view_ray(ndc: vec2<f32>) -> vec3<f32> {
    let near = unproject(ndc, 1.0);
    return normalize(unproject(ndc, 1e-3) - near);
}

// The eye's height as the scattering takes it: held within the game
// camera's reach of the anchor, as the scene's fog is (scene.rs).
fn sky_height() -> f32 {
    if globals.eye.w < 0.5 { return 0.0; }
    var eye = globals.eye.xyz;
    let back = eye - globals.anchor.xyz;
    if length(back) > look.horizon.w { eye = globals.anchor.xyz + back * (look.horizon.w / length(back)); }
    return eye.y;
}

fn turned(p: vec3<f32>, a: f32) -> vec3<f32> {
    let c = cos(a);
    let s = sin(a);
    return vec3<f32>(c * p.x + s * p.z, p.y, c * p.z - s * p.x);
}

fn noise(p: vec3<f32>) -> f32 {
    return textureSampleLevel(t_noise, s_noise, p, 0.0).r;
}

// ComputeSkyColor toward unit `p`, gamma-encoded.
fn sky(p: vec3<f32>) -> vec3<f32> {
    // vs_Sky's cloud coordinates: the second turn is applied as M·p and as
    // p·M, which turns the other way.
    let stretch = 1.0 - p.y;
    let c0 = turned(p, look.turn.x) * vec3<f32>(0.5, 3.0, 0.5) * stretch;
    let c1 = turned(p, look.turn.y) * vec3<f32>(2.0, 10.0, 2.0) * stretch;
    let c2 = turned(p, -look.turn.y) * vec3<f32>(0.3, 0.9, 0.3) * stretch;
    let thickness = clamp(2.0 * p.y, 0.0, 1.0) * look.cloud.w;
    let cloud = clamp(noise(c0) + noise(c1) + noise(c2) - 1.0, 0.0, 1.0) * thickness;

    let offset = sky_height() / look.zenith.w;
    let zenith = 1.0 - clamp((p.y + offset) / (offset + 1.0), 0.0, 1.0);
    var sun_angle = dot(p, look.toward.xyz);
    let glow = pow(max(clamp(clamp(sun_angle, 0.0, 1.0) - look.toward.w, 0.0, 1.0) * look.sun.w, 0.0), look.horizon_falloff.w);
    sun_angle = clamp(sun_angle * sun_angle * 0.3 + 0.7, 0.0, 1.0);
    var color = sun_angle * mix(look.zenith.rgb, look.horizon.rgb, pow(vec3<f32>(zenith), look.horizon_falloff.rgb));
    color += look.sun.rgb * glow;
    return mix(color, look.cloud.rgb * (1.0 - clamp(cloud, 0.0, 1.0)), clamp(8.0 * cloud, 0.0, 1.0));
}

// Where nothing drew, the sky, linear as the scene's light is: sky.fx
// writes GammaToLinear of it to an sRGB target. The Top view, with no eye,
// sees it from sea level.
@fragment fn fs_sky(in: SkyOut) -> @location(0) vec4<f32> {
    let c = sky(view_ray(in.ndc));
    return vec4<f32>(pow(max(c, vec3<f32>(1e-4)), vec3<f32>(2.2)), 1.0);
}
