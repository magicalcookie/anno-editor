// Screen-space reflections for passes drawn after the opaque scene (the
// Water stage on): `ssr_trace` follows a reflected ray through the frame's
// opaque depth (group 0's t_depth) and returns the opaque color it meets in
// the linear copy a plugged pass gets as `Record::scene`. Appended after
// the frame prelude. What the ray cannot see on screen (the sky, things off
// screen or hidden) comes back with no confidence; the caller blends its
// own reflection (sky or environment) under it.

// What a pass traces with (gpu::Ssr::params): steps along the ray, its
// reach in world units, how thick a surface counts in world units, and how
// far in from the screen's edges, as a share of the viewport, reflections
// fade out.
struct Ssr {
    params: vec4<f32>,
}

// Distance from the camera along the view, which the depth test compares:
// perspective from the eye, orthographic along the view direction.
fn ssr_view_depth(p: vec3<f32>) -> f32 {
    if globals.eye.w > 0.5 { return distance(p, globals.eye.xyz); }
    return -dot(p, globals.eye.xyz);
}

// `p` on screen: pixel, and its depth as the depth buffer stores it; w 0
// off screen or behind the camera.
fn ssr_screen(p: vec3<f32>) -> vec4<f32> {
    let c = globals.clip * vec4<f32>(p, 1.0);
    if c.w <= 1e-6 { return vec4<f32>(0.0); }
    let ndc = c.xyz / c.w;
    if any(abs(ndc.xy) > vec2<f32>(1.0)) || ndc.z < 0.0 || ndc.z > 1.0 { return vec4<f32>(0.0); }
    let size = vec2<f32>(textureDimensions(t_depth));
    return vec4<f32>((ndc.x * 0.5 + 0.5) * size.x, (0.5 - ndc.y * 0.5) * size.y, ndc.z, 1.0);
}

// How far behind the opaque surface at its pixel `s` (from ssr_screen)
// lies, in world units; negative in front of it. Empty pixels (depth 0,
// the far plane) are never hit.
fn ssr_behind(s: vec4<f32>, p: vec3<f32>) -> f32 {
    let size = vec2<i32>(textureDimensions(t_depth));
    let d = textureLoad(t_depth, clamp(vec2<i32>(s.xy), vec2<i32>(0), size - 1), 0);
    if d <= 0.0 { return -1e9; }
    let ndc = vec2<f32>(s.x / f32(size.x) * 2.0 - 1.0, 1.0 - s.y / f32(size.y) * 2.0);
    return ssr_view_depth(p) - ssr_view_depth(unproject(ndc, d));
}

// Where the opaque scene seen from `origin` along unit `dir` lies on
// screen: its pixel in xy, how far to trust it in z, and w 1 on a hit (all
// 0 without one). Steps grow along the ray, so near the surface they are
// short and contacts stay sharp; a hit is refined by bisection.
fn ssr_hit(origin: vec3<f32>, dir: vec3<f32>, ssr: Ssr) -> vec4<f32> {
    // No steps: reflections off (the sky alone).
    if ssr.params.x < 0.5 { return vec4<f32>(0.0); }
    let steps = u32(ssr.params.x);
    let reach = ssr.params.y;
    let thickness = ssr.params.z;
    let growth = 1.06;
    let span = (pow(growth, f32(steps)) - 1.0) / (growth - 1.0);
    // Clip space is linear along the ray: each step adds to the origin's
    // clip position instead of transforming its point (ssr_at).
    let c0 = globals.clip * vec4<f32>(origin, 1.0);
    let cd = globals.clip * vec4<f32>(dir, 0.0);
    var near = 0.0;
    var far = -1.0;
    var t = 0.0;
    var step = reach / span;
    let perspective = globals.eye.w > 0.5;
    let size = vec2<f32>(textureDimensions(t_depth));
    let last = vec2<i32>(size) - 1;
    for (var i = 0u; i < steps; i++) {
        t += step;
        let c = c0 + cd * t;
        // Most steps lie on screen in front of the surface at their pixel:
        // ssr_at's tests for that without its square root and all but one
        // division (bounds in clip space; in front while the view depth
        // c.w is at most the surface's, anchor.w / depth). Inline here: as
        // a function shared with the quad march, the plain march lost the
        // gain (1800's water 0.97 ms against 1.12).
        if perspective {
            if c.w <= 1e-6 || any(abs(c.xy) > vec2<f32>(c.w)) || c.z < 0.0 || c.z > c.w { break; }
            let ndc = c.xy / c.w;
            let px = vec2<f32>((ndc.x * 0.5 + 0.5) * size.x, (0.5 - ndc.y * 0.5) * size.y);
            let d = textureLoad(t_depth, clamp(vec2<i32>(px), vec2<i32>(0), last), 0);
            if d <= 0.0 || c.w * d <= globals.anchor.w {
                near = t;
                step *= growth;
                continue;
            }
        }
        let behind = ssr_at(c, origin + dir * t);
        if behind.y < 0.5 { break; }
        if behind.x > 0.0 {
            // Behind a surface by less than its thickness (and this step):
            // the ray met it. Further behind it passed under something.
            if behind.x < thickness + step { far = t; }
            break;
        }
        near = t;
        step *= growth;
    }
    if far < 0.0 { return vec4<f32>(0.0); }
    for (var i = 0; i < 6; i++) {
        let mid = 0.5 * (near + far);
        let behind = ssr_at(c0 + cd * mid, origin + dir * mid);
        if behind.y > 0.5 && behind.x > 0.0 { far = mid; } else { near = mid; }
    }
    let s = ssr_screen(origin + dir * far);
    if s.w < 0.5 { return vec4<f32>(0.0); }
    // Fade toward the screen's edges, the ray's reach, and rays heading back
    // at the camera (whose hits lie on surfaces the view barely sees).
    let uv = s.xy / vec2<f32>(textureDimensions(t_depth));
    let edge = min(min(uv.x, 1.0 - uv.x), min(uv.y, 1.0 - uv.y));
    var trust = clamp(edge / max(ssr.params.w, 1e-3), 0.0, 1.0);
    trust *= 1.0 - pow(clamp(far / reach, 0.0, 1.0), 4.0);
    if globals.eye.w > 0.5 {
        trust *= clamp(1.0 + 2.0 * dot(dir, normalize(origin - globals.eye.xyz)), 0.0, 1.0);
    } else {
        trust *= clamp(1.0 - 2.0 * dot(dir, globals.eye.xyz), 0.0, 1.0);
    }
    return vec4<f32>(s.xy, trust, 1.0);
}

// ssr_behind for the ray's point `p` at clip position `c`: x how far behind
// the opaque surface at its pixel it lies, y 1 when on screen. In
// perspective the surface and `p` share the pixel's ray, so their distances
// differ by their view depths' difference times the ray's secant: no
// unprojection.
fn ssr_at(c: vec4<f32>, p: vec3<f32>) -> vec2<f32> {
    if c.w <= 1e-6 { return vec2<f32>(0.0); }
    let ndc = c.xyz / c.w;
    if any(abs(ndc.xy) > vec2<f32>(1.0)) || ndc.z < 0.0 || ndc.z > 1.0 { return vec2<f32>(0.0); }
    let size = vec2<f32>(textureDimensions(t_depth));
    let s = vec4<f32>((ndc.x * 0.5 + 0.5) * size.x, (0.5 - ndc.y * 0.5) * size.y, ndc.z, 1.0);
    if globals.eye.w < 0.5 { return vec2<f32>(ssr_behind(s, p), 1.0); }
    let d = textureLoad(t_depth, clamp(vec2<i32>(s.xy), vec2<i32>(0), vec2<i32>(size) - 1), 0);
    if d <= 0.0 { return vec2<f32>(-1e9, 1.0); }
    // Reversed depth is near / view depth, and clip w the view depth.
    let secant = distance(p, globals.eye.xyz) / c.w;
    return vec2<f32>((c.w - globals.anchor.w / d) * secant, 1.0);
}

// The opaque scene's linear colour at `pixel`, clamped to the screen.
fn ssr_color(scene: texture_2d<u32>, pixel: vec2<f32>) -> vec3<f32> {
    let size = vec2<i32>(textureDimensions(scene));
    return unpack_rgb9e5(textureLoad(scene, clamp(vec2<i32>(pixel), vec2<i32>(0), size - 1), 0).r);
}

// The opaque scene seen from `origin` along unit `dir`: rgb, and in alpha
// how far to trust it. The colour is read `shift` pixels off the hit (1800
// reads it where its waves move it; 0 for the hit itself).
fn ssr_trace(scene: texture_2d<u32>, origin: vec3<f32>, dir: vec3<f32>, ssr: Ssr, shift: vec2<f32>) -> vec4<f32> {
    let hit = ssr_hit(origin, dir, ssr);
    if hit.w < 0.5 { return vec4<f32>(0.0); }
    return vec4<f32>(ssr_color(scene, hit.xy + shift), hit.z);
}
