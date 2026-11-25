// The frame prelude every scene pass includes (gpu::FRAME_WGSL): bind
// group 0 (camera, lights, shadow and opaque depth maps, markers, lines,
// sky map, ground maps) and the shading helpers passes share. Depth is
// reversed: 1 near, 0 far. Passes outside the kit declare their own
// groups from 1 up. Scene passes write linear radiance into the HDR target
// when the scene is toned; the post pass tone maps it (fs_post).
struct Globals {
    clip: mat4x4<f32>,
    // Inverse of clip, for the sky's view rays.
    unclip: mat4x4<f32>,
    // Target width and height in device pixels, device pixels per logical
    // pixel, and the row bits of packed visible entries (placed.wgsl).
    viewport: vec4<f32>,
    // Perspective: eye position, w = 1. Orthographic: unit vector toward the viewer, w = 0.
    eye: vec4<f32>,
    // Camera anchor; w is the near distance in perspective, the depth span in orthographic.
    anchor: vec4<f32>,
    // The scene's corner in the target in pixels, and its scale against
    // the pane's (gpu::Renderer::set_render_scale): pane pixel p shows the
    // scene's xy + (p - xy) * z.
    display: vec4<f32>,
}
struct Lights {
    // World to shadow clip: xy -1..1 across the map, z 0 nearest the sun.
    shadow: mat4x4<f32>,
    // Unit vector toward the sun; w = 1 when the shadow map is live.
    sun: vec4<f32>,
    // w: shadow filter radius in texels.
    sun_color: vec4<f32>,
    // w: shadow map side in texels.
    sky: vec4<f32>,
    ground: vec4<f32>,
    // w = 1 when fog is on.
    fog: vec4<f32>,
    // Fog start and end, shadow texel size in world units, depth bias in shadow depth units.
    fog_range: vec4<f32>,
    // x: exposure, 0 for display-space shading (gpu::Tone); y: sun
    // occlusion; z: diffuse ambient factor.
    tone: vec4<f32>,
    // gpu::Sky: zenith (w = 1 on), horizon (w: sea level), sea color (w = 1 with a sea).
    zenith: vec4<f32>,
    horizon: vec4<f32>,
    sea: vec4<f32>,
    // Ground tint: terrain grid origin x, z, spacing, color gain (0: none).
    tint: vec4<f32>,
    // gpu::Occlusion radius, strength, bias and power.
    occlusion: vec4<f32>,
    // Presented terrain grid for t_ground_heights: origin x, z, spacing; w = 1 with a terrain.
    terrain: vec4<f32>,
    // Diffuse sky light per channel (light::SkyMap::irradiance): a surface
    // facing n gets (n, 1)ᵀ M (n, 1). See sky_light.
    ambient: array<mat4x4<f32>, 3>,
    // Sun disc radiance; w = 1 when t_sky holds the sky's radiance.
    disc: vec4<f32>,
    // gpu::GroundTint in t_tint: corner world X and Z, 1 / size; zw 0
    // without one.
    ground_tint: vec4<f32>,
    // gpu::Curve: x 0 ACES, 1 lite; y, z lite's max linear and white.
    curve: vec4<f32>,
    // gpu::CloudShadows: the layers' scales, contrast, power (0: none).
    clouds: vec4<f32>,
    // Each layer's map offset at this frame.
    cloud_offsets: vec4<f32>,
    // The view's shadow cascades (gpu::Shadows::cascades): world to their
    // clip, nearest first, in t_cascades' layers.
    cascades: array<mat4x4<f32>, 3>,
    // Per cascade: world units per texel, depth bias, filter radius in UV.
    cascade: array<vec4<f32>, 3>,
    // x: cascades in use.
    cascade_info: vec4<f32>,
    // gpu::Occlusion small and large scale weights, pixel cap and floor.
    occlusion_scale: vec4<f32>,
    // Per cascade: the square the view wants of it, as centre x, y and
    // half side in its clip: maps are drawn wider than that, and the
    // cascades take over from each other at the wanted squares' edges.
    cascade_want: array<vec4<f32>, 3>,
    // Baked ground pages (gpu/terrain.rs GroundPages) in t_pages: the
    // quadtree's corner world X and Z, metres its finest page spans; w the
    // least normal Y that takes them, 0 when the view has none.
    pages: vec4<f32>,
    // gpu::CloudShade in t_cloud_shade: corner world X and Z, 1 / size;
    // w = 1 with one.
    cloud_shade: vec4<f32>,
    // gpu::Glow: w the emissive strength; xyz spare.
    glow: vec4<f32>,
    // gpu::Glow::host: numbers of the host's own, which its families and
    // lighting read; the kit never does.
    host: array<vec4<f32>, 8>,
    // w = 1 with a gpu::Environment in t_environment.
    environment: vec4<f32>,
    // x: gpu::Scene::view_mode, which the lighting's view modes switch by
    // (Families' generated `shade` and `fog`); y: the renderer's clock in a
    // view mode, 0 in the plain one; w = 1 with a gpu::HostTexture in t_host.
    view: vec4<f32>,
    // x: gpu::Scene::local_lights in use, three words each in `locals`
    // (gpu::LocalLight::words). See local_light.
    local: vec4<f32>,
    locals: array<vec4<f32>, 384>,
    // gpu::Reflection: x the scene's height in target pixels; y = 1 with a
    // mirror in t_mirror; z its level; w its pixels per target pixel.
    reflection: vec4<f32>,
}
// position.w is the radius in logical pixels; color.w selects the shape.
struct GpuMarker { position: vec4<f32>, color: vec4<f32> }
@group(0) @binding(0) var<uniform> globals: Globals;
@group(0) @binding(1) var<uniform> lights: Lights;
@group(0) @binding(2) var s_albedo: sampler;
@group(0) @binding(3) var t_shadow: texture_depth_2d;
@group(0) @binding(4) var s_shadow: sampler_comparison;
// Opaque depth, copied before the blend pass.
@group(0) @binding(5) var t_depth: texture_depth_2d;
@group(0) @binding(6) var<storage, read> b_markers: array<GpuMarker>;
// Line segments: a.xyz, width in logical pixels; b.xyz, RGBA bits.
struct GpuSegment { a: vec4<f32>, b: vec4<f32> }
@group(0) @binding(7) var<storage, read> b_lines: array<GpuSegment>;
// The terrain's color map, one RGBA8 texel per height sample, loaded.
@group(0) @binding(8) var t_ground_tint: texture_2d<f32>;
// The presented terrain's heights, which adapting placements bend to.
@group(0) @binding(9) var t_ground_heights: texture_2d<f32>;
// The sky's radiance map (light::SkyMap) when lights.disc.w is 1.
@group(0) @binding(10) var t_sky: texture_2d<f32>;
// The ground's encoded tint (gpu::GroundTint) when lights.ground_tint.z > 0.
@group(0) @binding(11) var t_tint: texture_2d<f32>;
// The cloud shadow map (gpu::CloudShadows) when lights.clouds.w > 0.
@group(0) @binding(12) var t_clouds: texture_2d<f32>;
// The view's shadow cascades, one layer each.
@group(0) @binding(13) var t_cascades: texture_depth_2d_array;
// Baked ground pages: per finest-page cell of the quadtree the page that
// covers it (bit 31 set when baked, its level in bits 12..15, its layer
// below), and the pages' albedo with gloss, and their normals, a cell of
// one atlas each (terrain.rs PAGE_COLUMNS).
@group(0) @binding(14) var t_pages: texture_2d<u32>;
@group(0) @binding(15) var t_page_albedo: texture_2d_array<f32>;
@group(0) @binding(16) var t_page_normal: texture_2d_array<f32>;
// The pages' glow in red, for a ground that declares glow (Ground::glow).
@group(0) @binding(20) var t_page_glow: texture_2d_array<f32>;
@group(0) @binding(17) var s_pages: sampler;
// A pass's cloud shade (gpu::CloudShade) when lights.cloud_shade.w is 1.
@group(0) @binding(18) var t_cloud_shade: texture_2d<f32>;
// At Water and Blend, when a Blend-stage pass applies the ambient
// occlusion itself (Pass::applies_occlusion): the blurred factor at half
// size, its depth in alpha. Elsewhere, and where the kit applied the
// factor, a stand-in that reads as no occlusion.
@group(0) @binding(19) var t_occlusion: texture_2d<f32>;
// The scene's environment cube (gpu::Environment) when lights.environment.w
// is 1: read it through `environment`.
@group(0) @binding(21) var t_environment: texture_cube<f32>;
// The host lighting's own pictures (gpu::HostTexture) when lights.view.w is
// 1: read them through `host_texture`.
@group(0) @binding(22) var t_host: texture_2d_array<f32>;
// The view's mirror (gpu::Reflection) when lights.reflection.y is 1: read
// it through `mirror_reflection`.
@group(0) @binding(23) var t_mirror: texture_2d<f32>;
// The opaque pass writes color, and the pick handle under each pixel (the
// pick reads its depth from the depth target, gpu::PICK_DEPTH_WGSL).
struct Opaque {
    @location(0) color: vec4<f32>,
    @location(1) pick: u32,
    // Linear color before fog, RGB9E5: what the ocean refracts.
    @location(2) linear: u32,
}

// Lit opaque output: the display color, and the linear one before fog for
// the ocean. `fog` is the scene's (gpu::Lighting).
fn shaded(linear: vec3<f32>, world: vec3<f32>, pick: u32) -> Opaque {
    return Opaque(vec4<f32>(fog(linear, world), 1.0), pick, pack_rgb9e5(linear));
}

// Shared-exponent RGB: 9-bit mantissas, 5-bit exponent (bias 15).
fn pack_rgb9e5(c: vec3<f32>) -> u32 {
    let v = clamp(c, vec3<f32>(0.0), vec3<f32>(65408.0));
    let top = max(v.r, max(v.g, v.b));
    var e = max(-16, i32(floor(log2(max(top, 1e-30))))) + 16;
    var scale = exp2(f32(e - 24));
    if floor(top / scale + 0.5) >= 512.0 {
        e += 1;
        scale *= 2.0;
    }
    let m = vec3<u32>(floor(v / scale + 0.5));
    return min(m.r, 511u) | (min(m.g, 511u) << 9u) | (min(m.b, 511u) << 18u) | (u32(e) << 27u);
}

fn unpack_rgb9e5(p: u32) -> vec3<f32> {
    let scale = exp2(f32(i32(p >> 27u) - 24));
    return vec3<f32>(f32(p & 511u), f32((p >> 9u) & 511u), f32((p >> 18u) & 511u)) * scale;
}

fn rgb(v: u32) -> vec3<f32> {
    return vec3<f32>(f32((v >> 16u) & 255u), f32((v >> 8u) & 255u), f32(v & 255u)) / 255.0;
}

// Bilinear height of the presented terrain under world X/Z, clamped to it.
fn ground_height(xz: vec2<f32>) -> f32 {
    let last = vec2<i32>(textureDimensions(t_ground_heights)) - 1;
    let q = clamp((xz - lights.terrain.xy) / lights.terrain.z, vec2<f32>(0.0), vec2<f32>(last));
    let b = vec2<i32>(floor(q));
    let f = q - floor(q);
    let e = min(b + 1, last);
    let h00 = textureLoad(t_ground_heights, b, 0).r;
    let h10 = textureLoad(t_ground_heights, vec2<i32>(e.x, b.y), 0).r;
    let h01 = textureLoad(t_ground_heights, vec2<i32>(b.x, e.y), 0).r;
    let h11 = textureLoad(t_ground_heights, e, 0).r;
    return mix(mix(h00, h10, f.x), mix(h01, h11, f.x), f.y);
}

// The sun's light left at `world`: the shadow map's, and the clouds', the
// darker of the two.
fn shadow(world: vec3<f32>, n: vec3<f32>) -> f32 {
    return min(map_shadow(world, n), cloud_light(world));
}

// The clouds' light left at `world`: gpu::CloudShade near the view, handing
// over across its outer tenth to gpu::CloudShadows, which inside that goes
// unread.
fn cloud_light(world: vec3<f32>) -> f32 {
    if lights.cloud_shade.w > 0.0 {
        // Along the line toward the sun down to sea level, where it was drawn.
        let sun = lights.sun.xyz;
        let ground = world.xz - sun.xz * (world.y / max(sun.y, 0.05));
        let uv = (ground - lights.cloud_shade.xy) * lights.cloud_shade.z;
        let edge = min(min(uv.x, uv.y), min(1.0 - uv.x, 1.0 - uv.y));
        if edge > 0.0 {
            let near = textureSampleLevel(t_cloud_shade, s_pages, uv, 0.0).r;
            let k = clamp(edge * 10.0, 0.0, 1.0);
            if k >= 1.0 { return near; }
            return mix(cloud_map_light(world), near, k);
        }
    }
    return cloud_map_light(world);
}

// gpu::CloudShadows: two scrolling layers of the map's alpha, averaged,
// shaped by contrast and power.
fn cloud_map_light(world: vec3<f32>) -> f32 {
    if lights.clouds.w <= 0.0 { return 1.0; }
    let a = textureSampleLevel(t_clouds, s_albedo, world.xz * lights.clouds.x - lights.cloud_offsets.xy, 0.0).a;
    let b = textureSampleLevel(t_clouds, s_albedo, world.xz * lights.clouds.y - lights.cloud_offsets.zw, 0.0).a;
    let noise = 0.5 * (a + b);
    let left = 1.0 - clamp(mix(1.0, noise, lights.clouds.z), 0.0, 1.0);
    return clamp(pow(max(left, 0.0), lights.clouds.w), 0.0, 1.0);
}

// The sun's light the shadow maps leave at `world`: the nearest view
// cascade that holds it, fading into the next over its outer tenth as
// Anno's overlapping cascades do, then the scene's map.
fn map_shadow(world: vec3<f32>, n: vec3<f32>) -> f32 {
    if lights.sun.w < 0.5 { return 1.0; }
    // Normal offset of about a texel, toward the sun, stops self-shadow acne
    // on slopes without the light leaks a large depth bias causes.
    let toward_sun = n * select(-1.0, 1.0, dot(n, lights.sun.xyz) >= 0.0);
    var lit = 0.0;
    var left = 1.0;
    for (var k = 0u; k < u32(lights.cascade_info.x); k++) {
        let c = lights.cascade[k];
        let p = lights.cascades[k] * vec4<f32>(world + toward_sun * c.x * 1.5, 1.0);
        let w = lights.cascade_want[k];
        let edge = max(abs(p.x - w.x), abs(p.y - w.y)) / w.z;
        if edge >= 1.0 || max(abs(p.x), abs(p.y)) >= 1.0 || p.z > 1.0 { continue; }
        let uv = vec2<f32>(p.x * 0.5 + 0.5, 0.5 - p.y * 0.5);
        let fade = clamp((edge - 0.9) * 10.0, 0.0, 1.0);
        lit += left * (1.0 - fade) * cascade_pcf(k, uv, p.z - c.y, c.z);
        left *= fade;
        if left <= 0.0 { return lit; }
    }
    return lit + left * scene_shadow(world, toward_sun);
}

// Anno's soft shadows (functions_shadow.fxh, SampleShadowmap): the centre
// and five Poisson-disc taps `radius` out (BRDF_SHADOW_SAMPLES 6), each a
// hardware 2x2 compare. The game's disc offsets come from the engine; these
// are the first five of the usual 16-point disc.
fn cascade_pcf(k: u32, uv: vec2<f32>, reference: f32, radius: f32) -> f32 {
    let disc = array<vec2<f32>, 5>(
        vec2<f32>(-0.942, -0.399), vec2<f32>(0.946, -0.769), vec2<f32>(-0.094, -0.929),
        vec2<f32>(0.345, 0.294), vec2<f32>(-0.916, 0.458));
    var sum = textureSampleCompareLevel(t_cascades, s_shadow, uv, k, reference);
    for (var i = 0; i < 5; i++) {
        sum += textureSampleCompareLevel(t_cascades, s_shadow, uv + disc[i] * radius, k, reference);
    }
    return sum / 6.0;
}

// The scene's own map, over everything the scene holds.
fn scene_shadow(world: vec3<f32>, toward_sun: vec3<f32>) -> f32 {
    let p = lights.shadow * vec4<f32>(world + toward_sun * lights.fog_range.z * 1.5, 1.0);
    let uv = vec2<f32>(p.x * 0.5 + 0.5, 0.5 - p.y * 0.5);
    if any(uv < vec2<f32>(0.0)) || any(uv > vec2<f32>(1.0)) || p.z > 1.0 { return 1.0; }
    let reference = p.z - lights.fog_range.w;
    let step = lights.sun_color.w / lights.sky.w;
    // 3x3 taps, each a hardware 2x2 compare: 36 texels of PCF. The four
    // corner taps read every texel the others do, so when they agree the
    // pixel is wholly lit or shadowed and the rest would agree too; only
    // shadow edges take all nine. Most pixels take four, which saved 0.2 ms
    // of a low 1080p view whose foliage shades several layers deep.
    let corners = shadow_tap(uv, vec2<f32>(-step, -step), reference)
        + shadow_tap(uv, vec2<f32>(step, -step), reference)
        + shadow_tap(uv, vec2<f32>(-step, step), reference)
        + shadow_tap(uv, vec2<f32>(step, step), reference);
    if corners == 0.0 || corners == 4.0 { return corners * 0.25; }
    let edges = shadow_tap(uv, vec2<f32>(0.0, -step), reference)
        + shadow_tap(uv, vec2<f32>(-step, 0.0), reference)
        + shadow_tap(uv, vec2<f32>(step, 0.0), reference)
        + shadow_tap(uv, vec2<f32>(0.0, step), reference);
    return (corners + edges + shadow_tap(uv, vec2<f32>(0.0), reference)) / 9.0;
}

fn shadow_tap(uv: vec2<f32>, offset: vec2<f32>, reference: f32) -> f32 {
    return textureSampleCompareLevel(t_shadow, s_shadow, uv + offset, reference);
}

// What a fragment hands to `shade`. `geometry` is the unbent normal, for
// the shadow lookup's offset. `specular` is gpu::Specular's bits: which
// specular terms `kit_shade` leaves out.
struct Surface {
    albedo: vec3<f32>,
    normal: vec3<f32>,
    geometry: vec3<f32>,
    gloss: f32,
    metal: f32,
    occlusion: f32,
    specular: u32,
}

fn plain(albedo: vec3<f32>, n: vec3<f32>) -> Surface {
    return Surface(albedo, n, n, 0.0, 0.0, 1.0, 0u);
}

fn toned() -> bool { return lights.tone.x > 0.0; }

fn decode(c: vec3<f32>) -> vec3<f32> {
    if !toned() { return c; }
    return select(pow((c + 0.055) / 1.055, vec3<f32>(2.4)), c / 12.92, c <= vec3<f32>(0.04045));
}

// Display values for linear radiance `c` (gpu::Curve), applied once per
// pixel by the post pass (fs_post) to what the scene passes left in the
// HDR target. ACES: exposure, the shadow lift, Narkowicz's ACES fit of
// the scaled value, then sRGB. Shoulder: gamma first and exposure after,
// then the shoulder per channel above max linear.
fn tone_map(c: vec3<f32>) -> vec3<f32> {
    if !toned() { return c; }
    if lights.curve.x > 0.5 {
        let gamma = max(lights.curve.w, 1e-3);
        let g = pow(max(c, vec3<f32>(0.0)), vec3<f32>(1.0 / gamma)) * lights.tone.x;
        let o = lights.curve.y;
        let w = max(lights.curve.z, o + 1e-3);
        let p = -(w * (o - 1.0) - o * o + o) / (w - 1.0);
        let over = g - o;
        return select(g, p * over / (over + p) + o, g > vec3<f32>(o));
    }
    let v = lift(max(c * lights.tone.x, vec3<f32>(0.0))) * lights.curve.w;
    let m = clamp((v * (2.51 * v + 0.03)) / (v * (2.43 * v + 0.59) + 0.14), vec3<f32>(0.0), vec3<f32>(1.0));
    return select(1.055 * pow(m, vec3<f32>(1.0 / 2.4)) - 0.055, m * 12.92, m <= vec3<f32>(0.0031308));
}

// The ACES curve's shadow lift: exposed values under the edge bend up along
// a power curve, so shadows keep detail instead of crushing to black (at
// edge 0.1, power 0.7, 0.02 comes out 0.032). Edge 0 leaves them.
fn lift(v: vec3<f32>) -> vec3<f32> {
    let edge = lights.curve.y;
    if edge <= 0.0 { return v; }
    let bent = edge * pow(max(v, vec3<f32>(1e-8)) / edge, vec3<f32>(lights.curve.z));
    return select(v, bent, v < vec3<f32>(edge));
}

// Karis's fit of the split-sum environment BRDF ("Physically Based Shading
// on Mobile"): what share of the surroundings a surface reflects.
fn env_brdf(f0: vec3<f32>, rough: f32, ndv: f32) -> vec3<f32> {
    let r = rough * vec4<f32>(-1.0, -0.0275, -0.572, 0.022) + vec4<f32>(1.0, 0.0425, 1.04, -0.04);
    let a = min(r.x * r.x, exp2(-9.28 * ndv)) * r.x + r.y;
    let ab = vec2<f32>(-1.04, 1.04) * a + r.zw;
    return f0 * ab.x + ab.y;
}

// Diffuse light from the sky on a surface facing unit `n`: the sky map's
// irradiance over π, or Light's sky-to-ground hemisphere, both packed as
// (n, 1)ᵀ M (n, 1) per channel.
fn sky_light(n: vec3<f32>) -> vec3<f32> {
    let p = vec4<f32>(n, 1.0);
    let e = vec3<f32>(dot(p, lights.ambient[0] * p), dot(p, lights.ambient[1] * p), dot(p, lights.ambient[2] * p));
    return max(e, vec3<f32>(0.0));
}

fn has_local_lights() -> bool { return lights.local.x > 0.5; }

// The scene's own lights (gpu::LocalLight) on a surface at `world` facing
// unit `n`: colour times falloff times the spot's cone times N·L, summed;
// the caller multiplies by its diffuse albedo. `two_sided` takes |N·L|
// for normals that face the viewer, not outward. `wrap` lights a sheet
// round, (N·L / 2 + ½)^2.5, as 117's particles take the sun
// (functions_particles.fxh).
fn local_light(world: vec3<f32>, n: vec3<f32>, two_sided: bool, wrap: bool) -> vec3<f32> {
    var sum = vec3<f32>(0.0);
    let count = u32(lights.local.x);
    for (var i = 0u; i < count; i++) {
        let at = lights.locals[i * 3u];
        let color = lights.locals[i * 3u + 1u];
        let cone = lights.locals[i * 3u + 2u];
        let to = at.xyz - world;
        let d = length(to);
        if d >= at.w || d < 1e-6 { continue; }
        let l = to / d;
        var falloff: f32;
        if color.w >= 0.0 {
            falloff = pow((at.w - d) / at.w, color.w);
        } else {
            let r = max(0.1, 0.001 * at.w);
            let fade = clamp(1.0 - (d / at.w) * (d / at.w), 0.0, 1.0);
            falloff = 2.0 / (r * r + d * d + d * sqrt(d * d + r * r)) * fade;
        }
        // The aim's length is 1 / (cos inner - cos outer); none: a point.
        let spread = length(cone.xyz);
        if spread > 0.0 {
            let s = clamp((dot(-l, cone.xyz / spread) - cone.w) * spread, 0.0, 1.0);
            falloff *= s * s;
        }
        var facing = dot(n, l);
        if wrap {
            facing = pow(facing * 0.5 + 0.5, 2.5);
        } else if two_sided {
            facing = abs(facing);
        }
        sum += color.rgb * falloff * max(facing, 0.0);
    }
    return sum;
}

// The kit's distance fog: the scene's `fog` unless a host replaces it
// (gpu::Lighting).
fn kit_fog(color: vec3<f32>, world: vec3<f32>) -> vec3<f32> {
    if lights.fog.w < 0.5 { return color; }
    var distance = length(globals.eye.xyz - world);
    if globals.eye.w < 0.5 { distance = max(dot(globals.anchor.xyz - world, globals.eye.xyz), 0.0); }
    let amount = clamp((distance - lights.fog_range.x) / max(lights.fog_range.y - lights.fog_range.x, 1e-6), 0.0, 1.0);
    if amount <= 0.0 { return color; }
    // With a sky map, into the horizon behind the point, so distant land
    // fades into the sky it stands against, not one color all round.
    var haze = lights.fog.rgb;
    if lights.disc.w > 0.5 {
        var away = -globals.eye.xyz;
        if globals.eye.w > 0.5 { away = world - globals.eye.xyz; }
        let flat = vec3<f32>(away.x, 0.0, away.z);
        if dot(flat, flat) > 1e-12 { haze = sky_map(normalize(flat)); }
    }
    return mix(color, haze, amount);
}

fn unproject(ndc: vec2<f32>, z: f32) -> vec3<f32> {
    let p = globals.unclip * vec4<f32>(ndc, z, 1.0);
    return p.xyz / p.w;
}

// t_sky's coordinates for unit direction `d` (light::SkyMap): around from
// +X through +Z, and down from the zenith with the square root of the
// latitude. Rows clamp to their centres so the poles do not wrap.
fn sky_uv(d: vec3<f32>) -> vec2<f32> {
    let lat = asin(clamp(d.y, -1.0, 1.0));
    let s = sign(lat) * sqrt(abs(lat) / 1.5707963);
    let rows = f32(textureDimensions(t_sky).y);
    let v = clamp((1.0 - s) * 0.5, 0.5 / rows, 1.0 - 0.5 / rows);
    return vec2<f32>(atan2(d.z, d.x) / 6.2831853 + 0.5, v);
}

// The sky map's radiance toward `d`. Level 0: the seam where u wraps
// would otherwise pick a tiny mip.
fn sky_map(d: vec3<f32>) -> vec3<f32> {
    return textureSampleLevel(t_sky, s_albedo, sky_uv(d), 0.0).rgb;
}

// The sun's disc as Anno draws it (environment.fx AddSunDiskLight): 0.44°
// in radius (cos 0.99997), darkening toward its limb by wavelength.
fn sun_disc(d: vec3<f32>) -> vec3<f32> {
    let nu = dot(d, normalize(lights.sun.xyz));
    if nu < 0.99997 || d.y <= 0.0 { return vec3<f32>(0.0); }
    let edge = 1.0 - clamp((nu - 0.99997) / 0.00003, 0.0, 1.0);
    let m = sqrt(1.0 - edge * edge);
    let a = 4.0 * vec3<f32>(0.397, 0.503, 0.652);
    return lights.disc.rgb * clamp(pow(vec3<f32>(m), a), vec3<f32>(0.0), vec3<f32>(1.0));
}

// With a sky map: the map and the sun's disc. Without: zenith to horizon
// over a band that narrows near the horizon, the horizon color below it,
// and the sun: a disc about 0.5° across and two lobes of glow, as forward
// scattering in haze.
fn sky_radiance(d: vec3<f32>) -> vec3<f32> {
    if lights.disc.w > 0.5 { return sky_map(d) + sun_disc(d); }
    let up = clamp(d.y, 0.0, 1.0);
    var c = mix(lights.zenith.rgb, lights.horizon.rgb, pow(1.0 - up, 5.0));
    let toward = clamp(dot(d, normalize(lights.sun.xyz)), 0.0, 1.0);
    c += lights.sun_color.rgb * (0.06 * pow(toward, 12.0) + 0.25 * pow(toward, 400.0));
    if toward > 0.99996 && d.y > 0.0 { c += lights.sun_color.rgb * 12.0; }
    return c;
}

// What a surface reflects toward unit `d`, linear: the scene's
// environment cube, or the sky (`sky_radiance`) where it has none. Level 0:
// families may call it, and they take no implicit derivatives.
fn environment(d: vec3<f32>) -> vec3<f32> {
    if lights.environment.w > 0.5 { return textureSampleLevel(t_environment, s_albedo, d, 0.0).rgb; }
    return sky_radiance(d);
}

// What the scene's mirror (gpu::Reflection) shows at target pixel `pixel`:
// the mirror view's colour there, linear, alpha 1 where a surface drew and
// 0 where only sky lies past it. The mirror view sees the world upside
// down, so its rows are flipped about the scene's height. It is drawn
// smaller than the view (reflection.w per pixel), so it is read filtered.
// All 0 without a mirror.
fn mirror_reflection(pixel: vec2<f32>) -> vec4<f32> {
    if lights.reflection.y < 0.5 { return vec4<f32>(0.0); }
    let at = vec2<f32>(pixel.x, lights.reflection.x - pixel.y) * lights.reflection.w;
    return textureSampleLevel(t_mirror, s_pages, at / vec2<f32>(textureDimensions(t_mirror)), 0.0);
}

// Layer `layer` of the host's texture at `uv`, repeating, level 0; white
// where the scene has none.
fn host_texture(uv: vec2<f32>, layer: u32) -> vec4<f32> {
    return textureSampleLevel(t_host, s_albedo, uv, layer, 0.0);
}

// The occlusion factor in `t` (half size, depth in alpha) brought up to
// pixel `px`, whose opaque depth is `z`: the texels around it bilinearly,
// but only those taken at about its own depth, so occlusion does not bleed
// across a silhouette; where none is, the nearest in depth. Pixels at a
// texel's own point (even x and y) read it alone.
fn occlusion_upsample(t: texture_2d<f32>, px: vec2<i32>, z: f32) -> vec3<f32> {
    let last = vec2<i32>(textureDimensions(t)) - 1;
    let base = px / 2;
    let odd = vec2<f32>(px & vec2<i32>(1)) * 0.5;
    var sum = vec3<f32>(0.0);
    var weight = 0.0;
    var closest = vec4<f32>(1.0, 1.0, 1.0, 1e30);
    for (var i = 0; i < 4; i++) {
        let d = vec2<i32>(i & 1, i >> 1);
        let o = textureLoad(t, min(base + d, last), 0);
        // Reversed depth is near 1 / distance, so this is the relative
        // difference in distance.
        let gap = abs(o.w - z) / z;
        if gap < closest.w { closest = vec4<f32>(o.rgb, gap); }
        let f = select(1.0 - odd, odd, d == vec2<i32>(1));
        let w = f.x * f.y * select(0.0, 1.0, gap < 0.02);
        sum += o.rgb * w;
        weight += w;
    }
    if weight > 0.0 { return sum / weight; }
    return closest.rgb;
}

// What a Blend-stage pass that applies the occlusion itself multiplies the
// target by at pixel `px`: the opaque surface's factor, 1 where nothing
// opaque drew. Water draws over that surface first, so a Water-stage pass
// divides its color by the same factor, which the multiply then takes
// back off (1 when no Blend-stage pass applies it).
fn occlusion_factor(px: vec2<i32>) -> vec3<f32> {
    let z = textureLoad(t_depth, px, 0);
    if z <= 0.0 { return vec3<f32>(1.0); }
    return occlusion_upsample(t_occlusion, px, z);
}
