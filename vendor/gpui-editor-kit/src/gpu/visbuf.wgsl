// The visibility buffer, the one way lit placements reach the screen
// (visbuf.rs). Appended to placed.wgsl.
//
// One geometry pass rasterises the lit placements the view saw last frame
// once, alpha-testing cutouts as their shadows do, and stores per pixel
// which triangle won. A full-screen resolve then rebuilds that triangle
// from the arena, interpolates its corners as the rasteriser would, and
// shades it with placed_lit into the opaque targets.
//
// t_vis, two u32 per pixel:
//   x: the visible entry as cull.wgsl wrote it: placement row in the low
//      globals.viewport.w bits, its mesh's material above (placed::row_bits).
//   y: the triangle's first vertex's place in its mesh's p_firsts table,
//      plus one; 0 is empty. For a mesh with vertices of its own that is
//      the vertex itself; a level sharing its full mesh's has a table of
//      its own (GpuDraw.firsts). The resolve reads p_firsts with it before
//      it knows the material: one dependent load fewer, which the
//      material first had cost 0.025 ms on Albion at 5K.
//      Triangles are named by their first index in their mesh's own
//      (placed.wgsl arena_index). Where fragments see their triangle's
//      corners (visbuf_corners.wgsl), bit 31 is set, the place takes bits
//      0..23 and bits 24..30 hash the corners (vis_hash).
// The first vertex, not primitive_index: fragments get it flat from the
// provoking vertex for free, where primitive_index cost a third of the pass
// (placed.rs spread_firsts). Vulkan, D3D12 and Metal all provoke with the
// first vertex. p_firsts says which triangles start there.

// Per vertex of each mesh's table (GpuDraw.firsts; a level sharing its
// full mesh's vertices has its own): the index position of the one
// triangle it starts, or FIRSTS_LIST and the place in this same array of
// [count, position...].
@group(3) @binding(6) var<storage, read> p_firsts: array<u32>;
const FIRSTS_LIST: u32 = 0x80000000u; // placed::FIRSTS_LIST

struct VisOutput {
    @builtin(position) @invariant clip: vec4<f32>,
    // Tiled per vertex, as DepthOutput.uv.
    @location(1) uv: vec2<f32>,
    @location(2) @interpolate(flat) entry: u32,
    // The vertex's place in its mesh's table, twice: flat for the first
    // vertex, and per vertex where fragments read their corners.
    @location(3) @interpolate(flat) vertex: u32,
    @location(4) @interpolate(flat) first: u32,
}

@vertex fn vs_vis(@builtin(vertex_index) value: u32, @builtin(instance_index) instance: u32) -> VisOutput {
    let drawn = arena_drawn(value, instance);
    let index = drawn.vertex;
    let entry = drawn.entry;
    let unpacked = unpack(entry);
    let material = p_draws[unpacked.y];
    let vertex = arena_vertex(index, material);
    let at = placed_at(index, unpacked.x, material, vertex, p_transforms[unpacked.x]);
    let table = material.firsts + index - material.vertex;
    return VisOutput(globals.clip * vec4<f32>(at.world, 1.0), tiled(vertex.uv, at.world, material.tiling), entry, table, table);
}

// Placements nothing cuts out (placed::Ranges::lit_opaque): no UVs, no
// albedo sample.
struct VisOpaque {
    @builtin(position) @invariant clip: vec4<f32>,
    @location(2) @interpolate(flat) entry: u32,
    @location(3) @interpolate(flat) vertex: u32,
    @location(4) @interpolate(flat) first: u32,
}

@vertex fn vs_vis_opaque(@builtin(vertex_index) value: u32, @builtin(instance_index) instance: u32) -> VisOpaque {
    let drawn = arena_drawn(value, instance);
    let index = drawn.vertex;
    let entry = drawn.entry;
    let unpacked = unpack(entry);
    let material = p_draws[unpacked.y];
    let at = placed_at(index, unpacked.x, material, arena_vertex(index, material), p_transforms[unpacked.x]);
    let table = material.firsts + index - material.vertex;
    return VisOpaque(globals.clip * vec4<f32>(at.world, 1.0), entry, table, table);
}

// What a visibility pass writes per pixel: the buffer, and the depth it
// drew the pixel at (t_vis_depth). The depth target cannot stand in: the
// second cull phase draws over the ground the opaque pass put there, and
// the resolve must still tell which of the first phase's pixels that
// ground covers. Writing it per fragment cost about 0.1 ms at 5K (RTX 4080
// SUPER, branch visbuf-all); the depth copy it replaced was about free.
// After the opaque targets (visbuf.rs VIS_TARGETS), which the revealed
// pass holds beside the buffer.
struct VisWritten {
    @location(3) vis: vec2<u32>,
    @location(4) depth: f32,
}

@fragment fn fs_vis_opaque(in: VisOpaque) -> VisWritten {
    return VisWritten(vec2<u32>(in.entry, in.vertex + 1u), in.clip.z);
}

@fragment fn fs_vis(in: VisOutput) -> VisWritten {
    let material = p_draws[unpack(in.entry).y];
    // The shadow pass's cut (fs_placed_depth), on the same footprint.
    let texel = textureSample(t_albedos[material.albedo], s_albedo, in.uv);
    if cut_away(material, in.uv, texel) { discard; }
    return VisWritten(vec2<u32>(in.entry, in.vertex + 1u), in.clip.z);
}

@group(1) @binding(0) var t_vis: texture_2d<u32>;
// The depth each buffer pixel was drawn at, by either cull phase
// (VisWritten; 0 where none drew): the pick target's depth bits, and what
// tells triangles sharing a first vertex apart.
@group(1) @binding(1) var t_vis_depth: texture_2d<f32>;
// The opaque pass's depth, for the resolve after it (visbuf.rs resolve).
@group(1) @binding(2) var t_opaque_depth: texture_depth_2d;

@vertex fn vs_resolve(@builtin(vertex_index) v: u32) -> @builtin(position) vec4<f32> {
    let p = vec2<f32>(f32((v << 1u) & 2u), f32(v & 2u));
    return vec4<f32>(p * 2.0 - 1.0, 0.0, 1.0);
}

// Clip position of vertex `index` of the visible `entry`, as vs_vis
// places it.
fn vis_clip(index: u32, entry: u32) -> vec4<f32> {
    let unpacked = unpack(entry);
    let material = p_draws[unpacked.y];
    let at = placed_at(index, unpacked.x, material, arena_vertex(index, material), p_transforms[unpacked.x]);
    return globals.clip * vec4<f32>(at.world, 1.0);
}

// The adjugate rows of the corners' clip (x, y, w) matrix; see bary.
fn bary_rows(c0: vec4<f32>, c1: vec4<f32>, c2: vec4<f32>) -> array<vec3<f32>, 3> {
    let v0 = c0.xyw;
    let v1 = c1.xyw;
    let v2 = c2.xyw;
    return array<vec3<f32>, 3>(cross(v1, v2), cross(v2, v0), cross(v0, v1));
}

// Perspective-correct barycentrics at pixel position `p` (y down) of a
// `size` target: the weights b with Σ b·(x, y, w) along the pixel's ray
// (ndc, 1) and Σ b = 1, by the inverse of the corner matrix (its adjugate
// rows; the determinant cancels). No division by w, so corners behind the
// eye are fine, and all three are positive inside the triangle.
fn bary(rows: array<vec3<f32>, 3>, p: vec2<f32>, size: vec2<f32>) -> vec3<f32> {
    let q = vec3<f32>(p.x / size.x * 2.0 - 1.0, 1.0 - p.y / size.y * 2.0, 1.0);
    let b = vec3<f32>(dot(rows[0], q), dot(rows[1], q), dot(rows[2], q));
    return b / (b.x + b.y + b.z);
}

// The triangle among those starting at `first` (a p_firsts entry) that
// covers pixel `p` nearest the depth the pass kept there. Triangles share a
// first vertex where a mesh has more of them than vertices; folded leaves
// can cover one pixel twice, so coverage alone does not settle it. Lists
// run largest triangle first (placed.rs spread_firsts), and one covering
// the pixel at the kept depth, to rounding, ends the search.
// The 7 bits naming a triangle among those starting at one vertex: its
// other corners' places less the first's, XORed, in any order the corners
// come in, and the same in the mesh's own vertices as in its table.
fn vis_hash(a: u32, b: u32, c: u32, first: u32) -> u32 {
    return ((a - first) ^ (b - first) ^ (c - first)) & 0x7fu;
}

// The table place in a visibility buffer pixel's y (> 0), and its corners'
// hash with bit 7 set where the pass wrote one, else 0.
fn vis_place(y: u32) -> vec2<u32> {
    if (y >> 31u) == 0u { return vec2<u32>(y - 1u, 0u); }
    return vec2<u32>((y & 0xffffffu) - 1u, (y >> 24u) & 0xffu);
}

fn vis_triangle(first: u32, table: u32, hash: u32, entry: u32, p: vec2<f32>, size: vec2<f32>, depth: f32) -> u32 {
    if (first & FIRSTS_LIST) == 0u { return first; }
    let list = first & ~FIRSTS_LIST;
    let count = p_firsts[list];
    let material = p_draws[unpack(entry).y];
    // With the corners' hash, the one triangle that matches, by its
    // indices alone; placing corners only where two match (or none).
    if hash != 0u {
        var matched = 0u;
        var found = 0u;
        for (var k = 1u; k <= count; k++) {
            let t = p_firsts[list + k];
            let tri = arena_triangle(t, material);
            if (vis_hash(tri.x, tri.y, tri.z, tri.x) | 0x80u) == hash {
                matched += 1u;
                found = t;
            }
        }
        if matched == 1u { return found; }
    }
    let c0 = vis_clip(table - material.firsts + material.vertex, entry);
    var best = p_firsts[list + 1u];
    var covers = false;
    var off = 1e30;
    var outside = -1e30;
    for (var k = 1u; k <= count; k++) {
        let t = p_firsts[list + k];
        let tri = arena_triangle(t, material);
        let c1 = vis_clip(tri.y, entry);
        let c2 = vis_clip(tri.z, entry);
        let b = bary(bary_rows(c0, c1, c2), p, size);
        let least = min(b.x, min(b.y, b.z));
        let at = c0 * b.x + c1 * b.y + c2 * b.z;
        let d = abs(at.z / at.w - depth);
        // A little outside still covers: the pixel centre on an edge the
        // rasteriser gave this triangle. Past that, the least outside wins,
        // so rounding never leaves a pixel to a far neighbour.
        if least >= -1e-3 {
            if d <= depth * 4e-6 { return t; }
            if !covers || d < off {
                best = t;
                off = d;
            }
            covers = true;
        } else if !covers && least > outside {
            best = t;
            outside = least;
        }
    }
    return best;
}

// Whether the opaque pass drew something nearer than the buffer's surface
// at `depth` (reversed: larger is nearer), the ground or a mesh, which then
// keeps its pixel (visbuf.rs resolve).
fn drawn_nearer(px: vec2<i32>, depth: f32) -> bool {
    return textureLoad(t_opaque_depth, px, 0) > depth;
}

// The derivatives copy the hardware's: differences across the 2x2 quad a
// pixel sits in, helper pixels extrapolating the same triangle. A
// rasterised draw's dpdx, which its normals use, runs along the pixel's own
// row and column (dpdxFine); the footprint its implicit texture samples take
// runs along the quad's first row and column (dpdxCoarse). Each the other
// way, the RTX 4080's image differed from a rasterised draw's on 20 times
// as many pixels.
//
// Shades triangle `first` (its first index) of the visible `entry` at
// pixel position `at`.
fn resolve_triangle(at: vec2<f32>, size: vec2<f32>, entry: u32, first: u32) -> Opaque {
    let tri = arena_triangle(first, p_draws[unpack(entry).y]);
    let c0 = placed_vertex(tri.x, entry);
    let c1 = placed_vertex(tri.y, entry);
    let c2 = placed_vertex(tri.z, entry);
    let rows = bary_rows(c0.clip, c1.clip, c2.clip);
    let quad = floor(at * 0.5) * 2.0 + 0.5;
    let b = bary(rows, at, size);
    let corner = bary(rows, quad, size);
    let right = bary(rows, quad + vec2<f32>(1.0, 0.0), size);
    let below = bary(rows, quad + vec2<f32>(0.0, 1.0), size);
    let dx = select(bary(rows, vec2<f32>(quad.x + 1.0, at.y), size) - bary(rows, vec2<f32>(quad.x, at.y), size), right - corner, at.y == quad.y);
    let dy = select(bary(rows, vec2<f32>(at.x, quad.y + 1.0), size) - bary(rows, vec2<f32>(at.x, quad.y), size), below - corner, at.x == quad.x);
    let world = mat3x3<f32>(c0.world, c1.world, c2.world);
    let uvs = mat3x2<f32>(c0.uv, c1.uv, c2.uv);
    let material = p_draws[unpack(entry).y];
    let at_world = world * b;
    let dp = array<vec3<f32>, 2>(world * dx, world * dy);
    // World tiling is linear in the world: its UVs' derivatives are the
    // same map of the world's.
    let code = material.tiling;
    let uv = tiled(uvs * b, at_world, code);
    let duv = array<vec2<f32>, 2>(tiled(uvs * dx, dp[0], code), tiled(uvs * dy, dp[1], code));
    // The coarse footprint, for sampling.
    let cx = tiled(uvs * (right - corner), world * (right - corner), code);
    let cy = tiled(uvs * (below - corner), world * (below - corner), code);
    let has_normals = (material.flags & 1u) != 0u;
    let p = LitPixel(
        at_world,
        mat3x3<f32>(c0.color, c1.color, c2.color) * b,
        surface_normal(at_world, cross(dp[0], dp[1]), mat3x3<f32>(c0.normal, c1.normal, c2.normal) * b, has_normals),
        dp,
        uv,
        duv,
        // The coarse footprint again, on the mesh's own UVs.
        uvs * b,
        array<vec2<f32>, 2>(uvs * (right - corner), uvs * (below - corner)),
        unpack(entry).x | 0x80000000u,
        p_words[unpack(entry).x],
    );
    var albedo = textureSampleGrad(t_albedos[material.albedo], s_albedo, uv, cx, cy);
    if (material.flags & 2u) != 0u { albedo.a = 1.0; }
    let t = LitTexels(
        vec4<f32>(decode(albedo.rgb), albedo.a),
        textureSampleGrad(t_albedos[material.maps & 0xffffu], s_albedo, uv, cx, cy),
        textureSampleGrad(t_albedos[material.maps >> 16u], s_albedo, uv, cx, cy),
    );
    // No cut: the visibility pass made it.
    return placed_lit(material, p, t);
}

// The compute resolve (visbuf.rs `Device::resolve`): fs_resolve per pixel
// of the scissor rect, into the opaque targets bound as storage. A pixel
// with no triangle, or where the opaque pass drew nearer, keeps its texels.
@group(1) @binding(3) var o_color: texture_storage_2d<rgba16float, write>;
@group(1) @binding(4) var o_pick: texture_storage_2d<r32uint, write>;
@group(1) @binding(5) var o_linear: texture_storage_2d<r32uint, write>;
// The scissor rect: x, y, width, height in target pixels.
@group(1) @binding(7) var<uniform> o_rect: vec4<u32>;

@compute @workgroup_size(8, 8)
fn cs_resolve(@builtin(global_invocation_id) id: vec3<u32>) {
    if id.x >= o_rect.z || id.y >= o_rect.w { return; }
    resolve_pixel(vec2<i32>(o_rect.xy + id.xy));
}

// The posed placements' tiles (cull.wgsl c_posed): word 0 not 0 for
// everywhere, a bit per tile from word 4 on (here, vector 1 on).
@group(1) @binding(8) var<uniform> o_posed: array<vec4<u32>, POSED_VECTORS>;
const POSED_VECTORS: u32 = 129u; // visbuf.rs POSED_WORDS / 4
const POSED_TILE: u32 = 64u; // visbuf.rs POSED_TILE

// The posed resolve: a workgroup per POSED_TILE square of the target that
// the scissor touches, leaving at once where no posed placement's box
// reached (cull.wgsl mark_posed), most of the screen; its threads 16
// pixels apart. Over the whole scissor, a thread a pixel, it read every
// pixel's buffer for the few they cover: 0.085 ms of a 1.3-1.9 ms still
// frame at 5K (RTX 4080 SUPER), and launching that many threads to leave
// at once still cost 0.074. With 8x8 threads looping over a tile, pirate,
// whose posed boxes reach most tiles, took 0.29 ms.
@compute @workgroup_size(16, 16)
fn cs_resolve_posed(@builtin(workgroup_id) tile: vec3<u32>, @builtin(local_invocation_id) id: vec3<u32>) {
    let at = o_rect.xy / POSED_TILE + tile.xy;
    let t = at.y * ((textureDimensions(t_vis).x + POSED_TILE - 1u) / POSED_TILE) + at.x;
    let word = o_posed[1u + t / 128u][(t / 32u) % 4u];
    if o_posed[0].x == 0u && (word & (1u << (t & 31u))) == 0u { return; }
    let end = o_rect.xy + o_rect.zw;
    for (var y = 0u; y < POSED_TILE; y += 16u) {
        for (var x = 0u; x < POSED_TILE; x += 16u) {
            let p = at * POSED_TILE + id.xy + vec2<u32>(x, y);
            if all(p >= o_rect.xy) && all(p < end) { resolve_pixel(vec2<i32>(p)); }
        }
    }
}

// What the resolve writes at `px`: the buffer's surface there, shaded,
// unless something nearer drew over it.
fn resolve_pixel(px: vec2<i32>) {
    let vis = textureLoad(t_vis, px, 0).xy;
    if vis.y == 0u { return; }
    let depth = textureLoad(t_vis_depth, px, 0).r;
    if drawn_nearer(px, depth) { return; }
    let at = vec2<f32>(px) + 0.5;
    let size = vec2<f32>(textureDimensions(t_vis));
    let place = vis_place(vis.y);
    let first = vis_triangle(p_firsts[place.x], place.x, place.y, vis.x, at, size, depth);
    let o = resolve_triangle(at, size, vis.x, first);
    textureStore(o_color, px, o.color);
    textureStore(o_pick, px, vec4<u32>(o.pick));
    textureStore(o_linear, px, vec4<u32>(o.linear));
}
