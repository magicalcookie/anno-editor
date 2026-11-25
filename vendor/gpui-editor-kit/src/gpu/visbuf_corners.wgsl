// The visibility pass where fragments see their triangle's corners
// (wgpu's per-vertex inputs, VK_KHR_fragment_shader_barycentric):
// appended to visbuf.wgsl, whose module then opens with `enable
// wgpu_per_vertex`. A pixel's y takes the first vertex's table place and a
// hash of the corners (vis_hash), which the resolve picks a triangle by
// among those sharing that vertex, without placing their corners: that
// search cost the resolve 0.08 ms on Albion at 5K.

fn vis_cornered(entry: u32, corners: array<u32, 3>, first: u32, depth: f32) -> VisWritten {
    let hash = vis_hash(corners[0], corners[1], corners[2], first) | 0x80u;
    return VisWritten(vec2<u32>(entry, (first + 1u) | 1u << 31u | (hash & 0x7fu) << 24u), depth);
}

@fragment fn fs_vis_corners(
    @builtin(position) at: vec4<f32>,
    @location(1) uv: vec2<f32>,
    @location(2) @interpolate(flat) entry: u32,
    @location(3) @interpolate(per_vertex) corners: array<u32, 3>,
    @location(4) @interpolate(flat) first: u32,
) -> VisWritten {
    let material = p_draws[unpack(entry).y];
    // The shadow pass's cut (fs_placed_depth), on the same footprint.
    let texel = textureSample(t_albedos[material.albedo], s_albedo, uv);
    if cut_away(material, uv, texel) { discard; }
    return vis_cornered(entry, corners, first, at.z);
}

@fragment fn fs_vis_opaque_corners(
    @builtin(position) at: vec4<f32>,
    @location(2) @interpolate(flat) entry: u32,
    @location(3) @interpolate(per_vertex) corners: array<u32, 3>,
    @location(4) @interpolate(flat) first: u32,
) -> VisWritten {
    return vis_cornered(entry, corners, first, at.z);
}
