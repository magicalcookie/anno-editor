// ssr_trace with each 2x2 pixel quad tracing one ray together, where the
// device has quad operations (gpu::ssr_module puts this after ssr.wgsl,
// whose own ssr_trace it renames). The quad marches its mean ray, each
// pixel a quarter of the steps, and narrows a hit with four probes a
// round; then every pixel finds its own crossing near the hit's distance
// along its own ray, else keeps the quad's hit. About 21 depth reads a
// pixel where the plain trace takes up to 47, at the detail of the
// pixel's own ray wherever the surface it meets is continuous.
// Call it in the same control flow for all four pixels of a quad. A pass
// that sizes its reach per pixel (1800's water) marches the quad's mean.
// One function to the colour read, no quad ssr_hit: split there, the 117
// sea's march compiled slower (its water 0.77 to 0.80 ms on Albion at 5K).

// The four lanes' values, in quad order (top left, top right, bottom
// left, bottom right).
fn ssr_quad_f(v: f32) -> vec4<f32> {
    return vec4<f32>(quadBroadcast(v, 0u), quadBroadcast(v, 1u), quadBroadcast(v, 2u), quadBroadcast(v, 3u));
}

fn ssr_quad_mean(v: vec3<f32>) -> vec3<f32> {
    let x = ssr_quad_f(v.x);
    let y = ssr_quad_f(v.y);
    let z = ssr_quad_f(v.z);
    return vec3<f32>(dot(x, vec4<f32>(0.25)), dot(y, vec4<f32>(0.25)), dot(z, vec4<f32>(0.25)));
}

// What a lane's quarter of the march met first: 0 nothing, 1 off screen,
// 2 passed behind a surface, 3 met one.
const SSR_NOTHING: f32 = 0.0;
const SSR_OFF: f32 = 1.0;
const SSR_UNDER: f32 = 2.0;
const SSR_MET: f32 = 3.0;

fn ssr_trace(scene: texture_2d<u32>, origin: vec3<f32>, dir: vec3<f32>, ssr: Ssr, shift: vec2<f32>) -> vec4<f32> {
    // No steps (reflections off): no lane marches, so nothing is met. Not
    // an early return: one ahead of the quad operations cost the 117 sea
    // 0.03 ms on Albion at 5K.
    let steps = u32(ssr.params.x);
    let reach = dot(ssr_quad_f(ssr.params.y), vec4<f32>(0.25));
    let thickness = ssr.params.z;
    let growth = 1.06;
    let span = (pow(growth, f32(steps)) - 1.0) / (growth - 1.0);
    let first = reach / span;
    // The lane's place in its quad, from its pixel.
    let own = ssr_screen(origin);
    let lane = (u32(own.x) & 1u) + 2u * (u32(own.y) & 1u);
    let o = ssr_quad_mean(origin);
    let d = normalize(ssr_quad_mean(dir));
    let c0 = globals.clip * vec4<f32>(o, 1.0);
    let cd = globals.clip * vec4<f32>(d, 0.0);
    // Step i ends at first (g^(i+1) - 1) / (g - 1); this lane takes steps
    // [lane n, lane n + n).
    let n = (steps + 3u) / 4u;
    let start = lane * n;
    var near = first * (pow(growth, f32(start)) - 1.0) / (growth - 1.0);
    var step = first * pow(growth, f32(start));
    var event = SSR_NOTHING;
    var far = 0.0;
    for (var i = start; i < min(start + n, steps); i++) {
        let t = near + step;
        let behind = ssr_at(c0 + cd * t, o + d * t);
        if behind.y < 0.5 { event = SSR_OFF; break; }
        if behind.x > 0.0 {
            event = select(SSR_UNDER, SSR_MET, behind.x < thickness + step);
            far = t;
            break;
        }
        near = t;
        step *= growth;
    }
    // The first lane along the ray that met anything decides.
    let events = ssr_quad_f(event);
    let nears = ssr_quad_f(near);
    let fars = ssr_quad_f(far);
    var met = false;
    var lo = 0.0;
    var hi = 0.0;
    for (var k = 0; k < 4; k++) {
        if events[k] != SSR_NOTHING {
            met = events[k] == SSR_MET;
            lo = nears[k];
            hi = fars[k];
            break;
        }
    }
    if !met { return vec4<f32>(0.0); }
    // Four probes a round, a fifth of the span apart: three rounds narrow
    // it 125 times, as six halvings did 64.
    for (var round = 0; round < 3; round++) {
        let t = lo + (hi - lo) * f32(lane + 1u) / 5.0;
        let behind = ssr_at(c0 + cd * t, o + d * t);
        let past = ssr_quad_f(select(0.0, 1.0, behind.y > 0.5 && behind.x > 0.0));
        let ts = ssr_quad_f(t);
        var next_lo = ts[3];
        var next_hi = hi;
        for (var k = 3; k >= 0; k--) {
            if past[k] > 0.5 {
                next_hi = ts[k];
                next_lo = select(lo, ts[max(k - 1, 0)], k > 0);
            }
        }
        lo = next_lo;
        hi = next_hi;
    }
    // This pixel's own ray: where it crosses the surface within a fifth of
    // the quad's distance either side, halved four times; else the quad's
    // hit.
    let a0 = origin + dir * (hi * 0.8);
    let b0 = origin + dir * (hi * 1.2);
    let before = ssr_at(globals.clip * vec4<f32>(a0, 1.0), a0);
    let after = ssr_at(globals.clip * vec4<f32>(b0, 1.0), b0);
    var p = o + d * hi;
    if before.y > 0.5 && after.y > 0.5 && before.x <= 0.0 && after.x > 0.0 && after.x < thickness + hi * 0.4 {
        var a = hi * 0.8;
        var b = hi * 1.2;
        for (var i = 0; i < 4; i++) {
            let m = 0.5 * (a + b);
            let q = origin + dir * m;
            let at = ssr_at(globals.clip * vec4<f32>(q, 1.0), q);
            if at.y > 0.5 && at.x > 0.0 { b = m; } else { a = m; }
        }
        p = origin + dir * b;
    }
    let s = ssr_screen(p);
    if s.w < 0.5 { return vec4<f32>(0.0); }
    let uv = s.xy / vec2<f32>(textureDimensions(t_depth));
    let edge = min(min(uv.x, 1.0 - uv.x), min(uv.y, 1.0 - uv.y));
    var trust = clamp(edge / max(ssr.params.w, 1e-3), 0.0, 1.0);
    trust *= 1.0 - pow(clamp(hi / reach, 0.0, 1.0), 4.0);
    if globals.eye.w > 0.5 {
        trust *= clamp(1.0 + 2.0 * dot(dir, normalize(origin - globals.eye.xyz)), 0.0, 1.0);
    } else {
        trust *= clamp(1.0 - 2.0 * dot(dir, globals.eye.xyz), 0.0, 1.0);
    }
    return vec4<f32>(ssr_color(scene, s.xy + shift), trust);
}
