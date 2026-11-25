// Puts a view's kept opaque layers back (gpu.rs `Layers`): the depth from
// its copy, and with it the HDR target as the occlusion stage left it.
// Drawn, not copied: after a texture copy into them the targets lost the
// GPU's compressed layout, and the water's depth tests that frame ran 0.2
// to 0.35 ms slower at 5K.
@group(0) @binding(0) var t_depth: texture_depth_2d;
@group(0) @binding(1) var t_hdr: texture_2d<f32>;

@vertex fn vs_kept(@builtin(vertex_index) v: u32) -> @builtin(position) vec4<f32> {
    let p = vec2<f32>(f32((v << 1u) & 2u), f32(v & 2u));
    return vec4<f32>(p * 2.0 - 1.0, 0.0, 1.0);
}

@fragment fn fs_kept_depth(@builtin(position) p: vec4<f32>) -> @builtin(frag_depth) f32 {
    return textureLoad(t_depth, vec2<i32>(p.xy), 0);
}

struct Whole {
    @location(0) color: vec4<f32>,
    @builtin(frag_depth) depth: f32,
}

@fragment fn fs_kept_whole(@builtin(position) p: vec4<f32>) -> Whole {
    let at = vec2<i32>(p.xy);
    return Whole(textureLoad(t_hdr, at, 0), textureLoad(t_depth, at, 0));
}

// The HDR target into the kept copy, drawn too: copying out of it needs a
// usage the moving frames' target would carry for nothing.
@fragment fn fs_kept_color(@builtin(position) p: vec4<f32>) -> @location(0) vec4<f32> {
    return textureLoad(t_hdr, vec2<i32>(p.xy), 0);
}
