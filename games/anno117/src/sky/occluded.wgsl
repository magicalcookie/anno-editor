// The haze composite with the ambient occlusion multiplied in (the kit's
// Pass::applies_occlusion), on devices that blend two sources: the target
// takes the haze plus what it held times the occlusion factor and the
// air's transmittance, in one write instead of the kit's full-screen pass
// and this one. Follows pass.wgsl; pass.rs puts `enable
// dual_source_blending;` before the module.

struct Occluded {
    @location(0) @blend_src(0) color: vec4<f32>,
    @location(0) @blend_src(1) under: vec4<f32>,
}

// The factor at the pixel, one where the kit's pass would have skipped it.
fn occluded_factor(in: SkyOut) -> vec3<f32> {
    let f = occlusion_factor(pixel(in));
    return select(f, vec3<f32>(1.0), all(f > vec3<f32>(1.0 - 1.0 / 512.0)));
}

@fragment fn fs_haze_up_occluded(in: SkyOut) -> Occluded {
    let out = haze_up(in);
    let f = occluded_factor(in);
    if all(out == CLEAR_AIR) && all(f == vec3<f32>(1.0)) { discard; }
    return Occluded(out, vec4<f32>(f * out.a, out.a));
}

@fragment fn fs_haze_fallback_occluded(in: SkyOut) -> Occluded {
    let out = haze_fallback(in);
    return Occluded(out, vec4<f32>(occluded_factor(in) * out.a, out.a));
}
