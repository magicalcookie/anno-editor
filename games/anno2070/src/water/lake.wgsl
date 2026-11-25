
// --- Lakes ------------------------------------------------------------------
// 2070's lakes (lake.fx ps_Lake) in the sea's pass and with its group 1:
// each lake's outline at its level (water/lake.rs), rippled by three layers
// of the river ripples, its colour deepening over its MaxDepth, lit by the
// profile's light. Left out: the per-lake dynamic waves, which lake.fx
// computes and then leaves unread.

struct LakeLook {
    // lakesettings.xml Color; a how much of it deep water shows.
    color: vec4<f32>,
    // x: MaxDepth (cLakeDesc.w).
    depth: vec4<f32>,
}

// The lakes' ripples (effects/river/ripples.dds): normal in xzy, foam in a.
@group(1) @binding(11) var o_lake_ripples: texture_2d<f32>;
@group(1) @binding(12) var<storage, read> o_lake_looks: array<LakeLook>;

struct LakeOutput {
    @builtin(position) clip: vec4<f32>,
    @location(0) world: vec3<f32>,
    @location(1) @interpolate(flat) look: u32,
    @location(2) extinction: vec3<f32>,
    @location(3) inscatter: vec3<f32>,
}

@vertex fn vs_lake(@location(0) p: vec4<f32>) -> LakeOutput {
    var out: LakeOutput;
    out.world = p.xyz;
    out.look = u32(p.w);
    out.clip = globals.clip * vec4<f32>(p.xyz, 1.0);
    // Scattered per vertex, as lake.fx does (:90-92).
    let air = scene_view_scatter(ocean.atmosphere, p.xyz);
    out.extinction = air.extinction;
    out.inscatter = air.inscatter;
    return out;
}

@fragment fn fs_lake(in: LakeOutput) -> @location(0) vec4<f32> {
    let world = in.world;
    let eye = ocean_eye();
    var to_point = normalize(world - eye);
    if globals.eye.w < 0.5 { to_point = -globals.eye.xyz; }
    let t = vec2<f32>(ocean.grid.w, 0.0);
    // 1. Three ripple layers, the normal their sum.
    let r0 = textureSample(o_lake_ripples, o_wrap, world.xz * 0.5 + t * 0.04);
    let r1 = textureSample(o_lake_ripples, o_wrap, world.xz * 0.3 - t * 0.03);
    let r2 = textureSample(o_lake_ripples, o_wrap, world.xz * 0.07 + t * 0.02);
    let n = normalize((r0.xzy * 2.0 - 1.0) + (r1.xzy * 2.0 - 1.0) + (r2.xzy * 2.0 - 1.0));
    // 2. The sky toward the reflected view, mirrored at the horizon as the
    // sky cube is.
    // 2070 mirrors each lake's own view (cMtxLakeMVPP, lake.fx:27,
    // :124-128). The kit's mirror lies at sea level, one a scene, so lakes
    // above it take the cube alone: a level-1 loss, as the plan has it.
    let reflected = normalize(reflect(to_point, n));
    let reflection = to_gamma(environment(normalize(vec3<f32>(reflected.x, abs(reflected.y), reflected.z))));
    // 3. Refraction, offset by the normal and taken straight where that
    // lands in front of the lake; the view's way through the water to it.
    let px = in.clip.xy;
    let size = vec2<f32>(textureDimensions(o_scene));
    var refract_px = vec2<i32>(px + n.xz * 0.03 * size);
    var z = ocean_depth(refract_px);
    if z > in.clip.z {
        refract_px = vec2<i32>(px);
        z = ocean_depth(refract_px);
    }
    var path = 1e4;
    if z > 0.0 {
        let uv = (vec2<f32>(refract_px) + 0.5) / size;
        let floor_ws = unproject(vec2<f32>(uv.x * 2.0 - 1.0, 1.0 - uv.y * 2.0), z);
        if globals.eye.w > 0.5 {
            path = max(distance(eye, floor_ws) - distance(eye, world), 0.0);
        } else {
            path = max(dot(world - floor_ws, globals.eye.xyz), 0.0);
        }
    }
    let look = o_lake_looks[in.look];
    let view = clamp(path / max(look.depth.x, 1e-4), 0.0, 1.0);
    let last = vec2<i32>(size) - 1;
    var refraction = to_gamma(unpack_rgb9e5(textureLoad(o_scene, clamp(refract_px, vec2<i32>(0), last), 0).r));
    // 4. The lake's colour under the profile's light, unshadowed.
    let sun = normalize(lights.sun.xyz);
    let lighting = mix(water_ambient(), water_sun(), clamp(dot(n, sun), 0.0, 1.0));
    refraction = mix(refraction, look.color.rgb * lighting, look.color.a * view);
    // 5. Fresnel at n = 2, scaled by how much water lies behind.
    let r_0 = 1.0 / 9.0;
    let fresnel = clamp(r_0 + (1.0 - r_0) * pow(1.0 - clamp(dot(-to_point, n), 0.0, 1.0), 5.0), 0.0, 1.0);
    var color = mix(refraction, reflection, fresnel * view);
    // 6. Foam where all three layers carry it.
    let foam = r0.a * r1.a * r2.a;
    color += foam * foam;
    // 7. The atmosphere's scattering, then linear.
    color = scene_scattered(color, scene_scattering(in.extinction, in.inscatter));
    return vec4<f32>(to_linear(color) / occlusion_factor(vec2<i32>(in.clip.xy)), 1.0);
}
