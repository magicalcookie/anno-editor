// underwater.fx's plankton and sunrays (underwater.rs `Sprites`), after the
// frame prelude and the under-water view's shared WGSL. Distances are from
// the eye the water is measured from (`underwater_eye`); the sprites face
// the frame's own camera.
// Engine addresses here are Anno5.exe build 11514956 (Steam).

struct SpriteParams {
    // x: seconds on the renderer's clock (cTimer.x); y: the plankton's
    // fade (cPlanktonSettings.w); z: sunrays alive.
    time: vec4<f32>,
}
@group(1) @binding(0) var<uniform> sprite: SpriteParams;
// Two vec4 a ray (underwater.rs `Rays::words`): start and alpha, then
// length, width and texture offset.
@group(1) @binding(1) var<storage, read> sprite_rays: array<vec4<f32>>;
@group(1) @binding(2) var t_sprite: texture_2d<f32>;
@group(1) @binding(3) var s_sprite: sampler;

// A quad's corners, two triangles, -1..1 (the vertex streams' x and y).
fn sprite_corner(v: u32) -> vec2<f32> {
    var corners = array<vec2<f32>, 6>(
        vec2<f32>(-1.0, -1.0), vec2<f32>(1.0, -1.0), vec2<f32>(1.0, 1.0),
        vec2<f32>(-1.0, -1.0), vec2<f32>(1.0, 1.0), vec2<f32>(-1.0, 1.0),
    );
    return corners[v];
}

// 0..1 from a word (PCG): the plankton's `rand`, fixed per sprite and life.
fn sprite_hash(x: u32) -> f32 {
    var h = x * 747796405u + 2891336453u;
    h = ((h >> ((h >> 28u) + 4u)) ^ h) * 277803737u;
    h = (h >> 22u) ^ h;
    return f32(h >> 8u) / 16777216.0;
}

// The frame camera's heading and the axes its sprites face along.
fn sprite_forward() -> vec3<f32> {
    return normalize(globals.anchor.xyz - globals.eye.xyz);
}

fn sprite_right() -> vec3<f32> {
    return normalize(cross(sprite_forward(), vec3<f32>(0.0, 1.0, 0.0)));
}

struct PlanktonOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) uv0: vec2<f32>,
    @location(1) uv1: vec4<f32>,
    @location(2) alpha: f32,
}

// vs_Plankton (underwater.fx:50-85) for sprite `instance % 40` of cell
// `instance / 40` in the 5×5×5 block of 4 m cells around the eye, only
// cells wholly under the surface (Anno5.exe 0x14086E670). Each sprite
// lives 13 s from a random spot, drifting and rising, fading in and out
// over half of it (0x14086E270).
@vertex fn vs_plankton(@builtin(vertex_index) v: u32, @builtin(instance_index) instance: u32) -> PlanktonOut {
    var out: PlanktonOut;
    out.clip = vec4<f32>(2.0, 2.0, 2.0, 1.0);
    let eye = underwater_eye();
    if eye.w < 0.5 { return out; }
    let k = instance % 40u;
    let n = instance / 40u;
    let cell = floor(eye.xyz / 4.0) + vec3<f32>(f32(n % 5u), f32((n / 5u) % 5u), f32(n / 25u)) - 2.0;
    if cell.y >= 0.0 { return out; }
    let t = sprite.time.x + 13.0 * sprite_hash(k * 7919u + 1u);
    let life = floor(t / 13.0);
    let age = t - life * 13.0;
    let s = k * 7919u + u32(life) * 104729u;
    var p = vec3<f32>(sprite_hash(s + 2u), sprite_hash(s + 3u), sprite_hash(s + 4u)) * 4.0;
    let radius = 0.2 + 0.4 * sprite_hash(s + 5u);
    p.x += max(sin(p.x), 0.0) * 0.03 * age;
    p.y += 0.025 * age;
    let left = 13.0 - age;
    var shown = 1.0 - max(left - 6.5, 0.0) / 6.5;
    if left <= 6.5 { shown = left / 6.5; }
    let c = sprite_corner(v);
    let right = sprite_right();
    let up = cross(right, sprite_forward());
    let world = cell * 4.0 + p + right * c.x * radius + up * c.y * radius;
    let depth = dot(world - eye.xyz, sprite_forward());
    out.clip = globals.clip * vec4<f32>(world, 1.0);
    out.uv0 = c * vec2<f32>(0.5, -0.5) + 0.5;
    out.uv1 = out.uv0.xyxy * (2.0 - radius);
    out.uv1.x += 0.4 + radius;
    out.uv1.y -= sprite.time.x * 0.010 + 8.0 * radius;
    out.uv1.w += sprite.time.x * 0.005;
    let near = globals.anchor.w;
    let fade = min(1.0 - pow(clamp(depth / 10.0, 0.0, 1.0), 2.0), clamp(depth - near * 2.0, 0.0, 1.0));
    out.alpha = fade * 7.0 * shown * sprite.time.y;
    return out;
}

// ps_Plankton (:89-103), outside the black tide.
@fragment fn fs_plankton(in: PlanktonOut) -> @location(0) vec4<f32> {
    let a = textureSample(t_sprite, s_sprite, in.uv0).r;
    let b = textureSample(t_sprite, s_sprite, in.uv1.xy).g;
    let c = textureSample(t_sprite, s_sprite, in.uv1.zw).g;
    let color = underwater_linear(vec3<f32>(0.5, 0.6, 0.6));
    return vec4<f32>(color, min(a * b * c * in.alpha, 0.5));
}

struct RayOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) uv0: vec4<f32>,
    // xy the board's own coordinates, z its world height.
    @location(1) uv1: vec3<f32>,
    // Fades near the eye and near the surface, and the ray's own alpha.
    @location(2) alpha: vec4<f32>,
}

// vs_Sunray (:145-180): a 25 m board slanting down from the surface,
// turned to the camera. Only while the sun stands above the horizon
// (0x14086F760).
@vertex fn vs_ray(@builtin(vertex_index) v: u32, @builtin(instance_index) i: u32) -> RayOut {
    var out: RayOut;
    out.clip = vec4<f32>(2.0, 2.0, 2.0, 1.0);
    let eye = underwater_eye();
    if eye.w < 0.5 || f32(i) >= sprite.time.z || lights.sun.y < 0.034 { return out; }
    let head = sprite_rays[i * 2u];
    let body = sprite_rays[i * 2u + 1u];
    let width = 2.0 * body.y;
    // The start sits on the surface, lifted by the sun's slant (0x14086F0D0).
    let start = vec3<f32>(head.x, head.y + (1.0 - abs(lights.sun.y)) * body.y, head.z);
    let along = normalize(vec3<f32>(1.0, -4.5, 1.0));
    let across = normalize(cross(along, sprite_forward()));
    let c = sprite_corner(v);
    let world = start + (c.y * 0.5 + 0.5) * body.x * along + c.x * width * across;
    out.clip = globals.clip * vec4<f32>(world, 1.0);
    out.uv1 = vec3<f32>(c * 0.5 + 0.5, world.y);
    out.uv0 = vec4<f32>(out.uv1.xy + vec2<f32>(body.z, 0.0), out.uv1.xy - vec2<f32>(body.z, 0.0));
    let depth = dot(world - eye.xyz, sprite_forward()) - globals.anchor.w;
    out.alpha = vec4<f32>(0.0, depth / 9.0, -(world.y - start.y + 0.5), head.w);
    return out;
}

// ps_Sunray (:182-200): the sun's light through the water down to here,
// GammaToLinear'd twice as the shader does.
@fragment fn fs_ray(in: RayOut) -> @location(0) vec4<f32> {
    let a = underwater_absorb();
    let light_depth = clamp((in.uv1.z / -lights.sun.y) / (a.w * 0.5), 0.0, 1.0);
    let absorption = pow(vec3<f32>(max(1.0 - light_depth, 1e-6)), underwater_linear(a.rgb));
    let color = vec3<f32>(0.3, 1.0, 1.0) * lights.sun_color.rgb * absorption;
    let shown = clamp(in.alpha.y, 0.0, 1.0) * clamp(in.alpha.z, 0.0, 1.0) * in.alpha.w;
    let alpha = textureSample(t_sprite, s_sprite, in.uv1.xy).g
        * textureSample(t_sprite, s_sprite, in.uv0.xy).r
        * textureSample(t_sprite, s_sprite, in.uv0.zw).b
        * shown * (1.0 - in.uv1.y);
    return vec4<f32>(underwater_linear(color), alpha);
}
