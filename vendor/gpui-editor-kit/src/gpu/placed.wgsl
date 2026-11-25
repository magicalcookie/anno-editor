// Placed pieces in one multi-draw per pipeline (placed.rs). Appended to
// gpu.wgsl, whose pass bindings and lighting it shares, and to the surface
// families' contract (family.wgsl), whose accessors it gives the arena. Every resident piece
// level's geometry, material and albedo sit in shared arrays (group 3); each
// visible-list entry carries its material above its row (group 2). A draw's
// index values name its visible list and its vertices (arena_drawn), so
// its indirect arguments start at instance and vertex zero.
const NO_PARTS: u32 = 0xffffffffu; // placed::NO_PARTS

// The coverage below which a draw is cut away (family.wgsl cut_away): the
// albedo alpha flags bits 24-31 hold in 255ths (gpu::alpha_cut), half
// without.
fn coverage_cut(material: GpuDraw) -> f32 {
    let at = material.flags >> 24u;
    return select(0.5, f32(at) / 255.0, at != 0u);
}

@group(2) @binding(0) var<storage, read> p_transforms: array<mat4x4<f32>>;
@group(2) @binding(1) var<storage, read> p_visible: array<u32>;
// Per row, the share of a thinning mesh's parts drawn (cull.wgsl).
@group(2) @binding(2) var<storage, read> p_keep: array<f32>;
// Per row, the host's instance word (Renderer::set_instance_words), 0
// unless set: opaque to the kit, a family reads it as `LitPixel.word`. A
// buffer of its own, as the transform's bottom row is taken
// (gpu::adapting). Bound for fragments alone; the cull and the selection
// never bind it.
@group(2) @binding(3) var<storage, read> p_words: array<u32>;
// Per draw slot, its list and where that starts in p_visible
// (placed::Placed::lists, cull.wgsl c_lists).
@group(2) @binding(4) var<storage, read> p_lists: array<vec2<u32>>;
@group(3) @binding(0) var<storage, read> p_draws: array<GpuDraw>;
// gpu::ArenaVertex, one load: x | y << 16, z (16 bits spare), u | v << 16 in
// steps of the mesh's grids, and color as GpuVertex's.
@group(3) @binding(1) var<storage, read> p_vertices: array<vec4<u32>>;
// Each arena vertex's octahedral normal, as GpuVertex's.
@group(3) @binding(7) var<storage, read> p_normals: array<u32>;
// Each mesh's indices from material.index, then a copy per further group
// of draws (placed::Arena::pack), 32 bits each: values of their own, which
// name an arena vertex through p_pages.
@group(3) @binding(2) var<storage, read> p_indices: array<u32>;
// Per page of index values (placed::Stream): a draw slot whose list the
// instances drawn with them are in (p_lists), and the arena vertex its first
// value names. wgpu on D3D12 leaves base vertex and first instance out of
// vertex_index and instance_index unless its indirect-draw checks run, so
// draws take neither: their values say both.
@group(3) @binding(8) var<storage, read> p_pages: array<vec2<u32>>;
const PAGE_BITS: u32 = 6u; // placed::PAGE_BITS
@group(3) @binding(3) var t_albedos: binding_array<texture_2d<f32>>;
// Per material; impostors: model-space centre of the mesh's box and the
// radius each view's tile spans, the box's half extents, and in half.w two
// texels of the atlas's finest level (placed::Shape).
struct Shape { centre: vec4<f32>, half: vec4<f32> }
@group(3) @binding(4) var<storage, read> p_shapes: array<Shape>;
// Per vertex of the thinning meshes: its part's centre and rank
// (MeshData::thinning), w 0 for vertices drawn as they are.
@group(3) @binding(5) var<storage, read> p_parts: array<vec4<f32>>;
const THIN_FADE: f32 = 0.05; // placed::THIN_FADE
// Baked impostor views, one layer per impostor mesh (GpuDraw.albedo): color
// and coverage, model-space normals octahedral in two channels, and depth
// toward the view with gloss in two more (BC3 and two BC5 where the device
// has them, gpu.rs bake_impostors).
@group(1) @binding(0) var t_impostor_albedo: texture_2d_array<f32>;
@group(1) @binding(1) var t_impostor_normal: texture_2d_array<f32>;
@group(1) @binding(2) var t_impostor_rest: texture_2d_array<f32>;
// Per layer (row) and view (column, cell.y * VIEWS + cell.x): the texels
// of the view's tile its coverage spans at the finest level, x0, y0 and
// past the last x1, y1 (impostor_mips.wgsl cs_bounds); x1 <= x0 for none.
@group(1) @binding(3) var t_impostor_bounds: texture_2d<u32>;

// The arena vertex index value `value` names.
fn arena_at(value: u32) -> u32 {
    return p_pages[value >> PAGE_BITS].y + (value & ((1u << PAGE_BITS) - 1u));
}

// What a vertex stage draws at index value `value` of instance
// `instance`: the visible entry, from the list of the slot the value's
// page names, and the arena vertex.
struct Drawn { entry: u32, vertex: u32 }

fn arena_drawn(value: u32, instance: u32) -> Drawn {
    let page = p_pages[value >> PAGE_BITS];
    let vertex = page.y + (value & ((1u << PAGE_BITS) - 1u));
    return Drawn(p_visible[p_lists[page.x].y + instance], vertex);
}

// The arena vertex index `k` of `material`'s mesh names (placed::pack).
fn arena_index(k: u32, material: GpuDraw) -> u32 {
    return arena_at(p_indices[material.index + k]);
}

// The arena vertices of the triangle at index `t` of `material`'s mesh.
fn arena_triangle(t: u32, material: GpuDraw) -> vec3<u32> {
    let i = material.index;
    return vec3<u32>(arena_at(p_indices[i + t]), arena_at(p_indices[i + t + 1u]), arena_at(p_indices[i + t + 2u]));
}

// Vertex `index` of `material`'s mesh, off its grids: each coordinate is
// the grid's origin plus its steps, a power of two, so exact in f32. (As
// exponents for ldexp, the resolve's dozen vertices a pixel took 0.15 ms
// more on Albion at 5K.)
fn arena_vertex(index: u32, material: GpuDraw) -> GpuVertex {
    let v = p_vertices[index];
    let q = vec3<f32>(f32(v.x & 0xffffu), f32(v.x >> 16u), f32(v.y & 0xffffu));
    let position = vec3<f32>(material.ox, material.oy, material.oz) + q * material.step_p;
    let t = vec2<f32>(f32(v.z & 0xffffu), f32(v.z >> 16u));
    let uv = vec2<f32>(material.ou, material.ov) + t * vec2<f32>(material.step_u, material.step_v);
    return GpuVertex(vec4<f32>(position, 1.0), uv, v.w, p_normals[index]);
}

// The row in a visible entry's low bits, the material above; the split follows
// the table (placed::row_bits, carried in globals.viewport.w).
fn unpack(entry: u32) -> vec2<u32> {
    let bits = u32(globals.viewport.w);
    return vec2<u32>(entry & ((1u << bits) - 1u), entry >> bits);
}

struct PlacedOutput {
    // Invariant: shadows and the visibility buffer place these vertices
    // with the same arithmetic, and must land on the same depths.
    @builtin(position) @invariant clip: vec4<f32>,
    @location(0) world: vec3<f32>,
    @location(1) uv: vec2<f32>,
    @location(2) color: vec3<f32>,
    @location(4) normal: vec3<f32>,
    @location(5) @interpolate(flat) pick: u32,
    @location(6) @interpolate(flat) draw: u32,
    // As Output.boxed (gpu.wgsl).
    @location(7) boxed: vec3<f32>,
}

// Where vertex `index` of the placement at `row` lies: in its model with
// its pose and thinning applied, and in the world; and how its pose turns
// the vertex's normal in the model.
struct Placed { local: vec3<f32>, world: vec3<f32>, turn: mat3x3<f32> }

const SKINNED: u32 = 512u; // GpuDraw.flags bit 9

// Vec4 `k` of the shapes array, two to a shape.
fn shape_vec4(k: u32) -> vec4<f32> {
    let s = p_shapes[k >> 1u];
    return select(s.centre, s.half, (k & 1u) == 1u);
}

// A 3x4 matrix by its rows.
struct Turn { r0: vec4<f32>, r1: vec4<f32>, r2: vec4<f32> }

// Where the pose of a row's piece moves vertex `index` now (placed::Pose).
// The row's last element names its record (Placed::stamp): 2 plus the
// shape it sits at, whose centre holds the vec4 its first frame starts at,
// its frames, its joints and its loops a second, negative when each
// placement takes a phase of its own; its half where the object's origin
// lies in the mesh and the vec4 of the clock. The phase follows that
// origin in the world, smoothly, so pieces of one object agree on it
// through rounding, and around the loop's wrap. Frames blend linearly: a
// part turning 12 degrees a frame shrinks half a percent between them.
fn pose_turn(index: u32, material: GpuDraw, m: mat4x4<f32>) -> Turn {
    let record = p_shapes[u32(m[3].w) - 2u];
    let frames = max(u32(record.centre.y), 1u);
    let joints = max(u32(record.centre.z), 1u);
    let clock = shape_vec4(u32(record.half.w)).x;
    var phase = 0.0;
    if record.centre.w < 0.0 {
        let origin = (m * vec4<f32>(record.half.xyz, 1.0)).xz;
        phase = fract(dot(origin, vec2<f32>(1.618034, 2.718282)));
    }
    let at = fract(clock * abs(record.centre.w) + phase) * f32(frames);
    let f0 = min(u32(at), frames - 1u);
    let f1 = select(f0 + 1u, 0u, f0 + 1u >= frames);
    let s = at - f32(f0);
    var joint = vec4<u32>(0u);
    var weight = vec4<f32>(1.0, 0.0, 0.0, 0.0);
    if (material.flags & SKINNED) != 0u {
        // Four joints and their weights, two bytes to an exact float
        // (placed::skin_words).
        let k = vec4<u32>(p_parts[material.parts + index - material.vertex]);
        joint = vec4<u32>(k.x & 255u, k.x >> 8u, k.y & 255u, k.y >> 8u);
        let w = vec4<f32>(vec4<u32>(k.z & 255u, k.z >> 8u, k.w & 255u, k.w >> 8u));
        let total = w.x + w.y + w.z + w.w;
        if total > 0.0 { weight = w / total; }
    }
    let first = u32(record.centre.x);
    var t = Turn(vec4<f32>(0.0), vec4<f32>(0.0), vec4<f32>(0.0));
    for (var i = 0u; i < 4u; i++) {
        let w = weight[i];
        if w <= 0.0 { continue; }
        let j = min(joint[i], joints - 1u);
        let a = first + 3u * (f0 * joints + j);
        let b = first + 3u * (f1 * joints + j);
        t.r0 += w * mix(shape_vec4(a), shape_vec4(b), s);
        t.r1 += w * mix(shape_vec4(a + 1u), shape_vec4(b + 1u), s);
        t.r2 += w * mix(shape_vec4(a + 2u), shape_vec4(b + 2u), s);
    }
    return t;
}

fn placed_at(index: u32, row: u32, material: GpuDraw, vertex: GpuVertex, m: mat4x4<f32>) -> Placed {
    var local = vertex.position.xyz;
    var turn = mat3x3<f32>(vec3<f32>(1.0, 0.0, 0.0), vec3<f32>(0.0, 1.0, 0.0), vec3<f32>(0.0, 0.0, 1.0));
    // Unposed rows hold 1 there, as affine matrices do.
    if m[3].w > 1.5 {
        let t = pose_turn(index, material, m);
        let p = vec4<f32>(local, 1.0);
        local = vec3<f32>(dot(t.r0, p), dot(t.r1, p), dot(t.r2, p));
        turn = transpose(mat3x3<f32>(t.r0.xyz, t.r1.xyz, t.r2.xyz));
    }
    var part = vec4<f32>(0.0, 0.0, 0.0, -1.0);
    if material.parts != NO_PARTS && (material.flags & SKINNED) == 0u {
        part = p_parts[material.parts + index - material.vertex];
    }
    if part.w > 0.0 {
        // Kept parts grow to keep the canopy's coverage; those ranked within
        // a bucket past the share kept shrink to their centre, so parts
        // fade out one by one instead of popping with the bucket.
        let keep = p_keep[row];
        let grow = min(sqrt(1.0 / max(keep, 1e-3)), 2.5);
        let fade = clamp((keep * (1.0 + THIN_FADE) - part.w) / (keep * THIN_FADE), 0.0, 1.0);
        local = part.xyz + (local - part.xyz) * grow * fade;
    }
    var world = adapt_to_terrain((m * vec4<f32>(local, 1.0)).xyz, m, vertex.color, (material.flags >> 3u) & 3u);
    // The family's own vertex stage, where it has one (Family::vertex).
    world = family_vertex(material, FamilyVertex(local, world, part, m));
    if (material.flags & 32u) != 0u { world = decal_lift(world, material.bias); }
    return Placed(local, world, turn);
}

// Drawn indexed from p_indices, so the post-transform cache shades shared
// vertices once.
@vertex fn vs_placed(@builtin(vertex_index) value: u32, @builtin(instance_index) instance: u32) -> PlacedOutput {
    let at = arena_drawn(value, instance);
    return placed_vertex(at.vertex, at.entry);
}

// Vertex `index` of the visible `entry` as vs_placed outputs it. The
// visibility buffer's resolve (visbuf.wgsl) runs it per triangle corner, so
// flat meshes, decals and the resolve place and attribute vertices alike.
fn placed_vertex(index: u32, entry: u32) -> PlacedOutput {
    let unpacked = unpack(entry);
    let row = unpacked.x;
    let d = unpacked.y;
    let material = p_draws[d];
    let vertex = arena_vertex(index, material);
    let m = p_transforms[row];
    let at = placed_at(index, row, material, vertex, m);
    let local = at.local;
    let world = at.world;
    var out: PlacedOutput;
    out.clip = globals.clip * vec4<f32>(world, 1.0);
    out.world = world;
    out.uv = vertex.uv;
    out.color = rgb(vertex.color) * material.gain;
    out.normal = vec3<f32>(0.0);
    if (material.flags & 1u) != 0u {
        // As vs_mesh: the cofactor keeps sheared and scaled normals right.
        let c0 = m[0].xyz;
        let c1 = m[1].xyz;
        let c2 = m[2].xyz;
        let cofactor = mat3x3<f32>(cross(c1, c2), cross(c2, c0), cross(c0, c1));
        out.normal = cofactor * (at.turn * octahedral(vertex.normal)) * sign(dot(c0, cross(c1, c2)));
    }
    if ((material.flags >> 3u) & 3u) == 3u && lights.terrain.w > 0.5 {
        out.normal = ground_normal(world.xz);
    }
    out.boxed = boxed(local, m);
    out.pick = row | 0x80000000u;
    out.draw = d;
    return out;
}

// Word `k` of `material`'s payload (gpu::MeshData::payload), eight to a
// shape from material.payload, after the shape of its slots when it has
// some (flags bit 11). The words are the host's as given: read floats with
// bitcast<f32>.
fn family_word(material: GpuDraw, k: u32) -> u32 {
    let s = p_shapes[material.payload + select(0u, 1u, (material.flags & 2048u) != 0u) + k / 8u];
    let half = select(s.centre, s.half, (k & 4u) != 0u);
    return bitcast<u32>(half[k & 3u]);
}

// The arena slot of `material`'s family slot `k` (gpu::MeshData::slots),
// for family_texel: 0, white, where the mesh brings none.
fn family_slot(material: GpuDraw, k: u32) -> u32 {
    if (material.flags & 2048u) == 0u { return 0u; }
    // Two to a word, in the shape's first four (placed.rs `MAX_SLOTS`).
    let words = bitcast<vec4<u32>>(p_shapes[material.payload].centre);
    return (words[(k / 2u) & 3u] >> ((k & 1u) * 16u)) & 0xffffu;
}

// A texel of the arena's texture `slot` (one of the material's) at `uv`,
// over the footprint `duv`: how a family samples a slot again, or one the
// caller did not sample for it. Families never index `t_albedos`
// themselves (placed.rs checks). Residency keeps the levels the mesh's own
// UVs ask of each slot, times the slot's repeat (placed.rs GpuTexels): a
// family sampling one finer than those gets the finest held.
fn family_texel(slot: u32, uv: vec2<f32>, duv: array<vec2<f32>, 2>) -> vec4<f32> {
    return textureSampleGrad(t_albedos[slot], s_albedo, uv, duv[0], duv[1]);
}

// `family_texel` at mip `level` of the texture, whatever the footprint.
fn family_texel_level(slot: u32, uv: vec2<f32>, level: f32) -> vec4<f32> {
    return textureSampleLevel(t_albedos[slot], s_albedo, uv, level);
}

// Where the placement `p` shades stands: its row's translation. A family
// that tells placements apart by where they stand reads it here.
fn family_origin(p: LitPixel) -> vec3<f32> {
    return p_transforms[p.pick & 0x7fffffffu][3].xyz;
}

// `texel`, the material's decoded albedo, under `p`'s vertex colour as
// this path composes the two.
fn family_albedo(p: LitPixel, texel: vec3<f32>) -> vec3<f32> {
    return texel * p.color;
}

// `t` under `material`'s mesh splat where it has one (gpu::MeshSplat), as
// the kit's `lit` takes it; `t` itself without.
fn family_splat(material: GpuDraw, p: LitPixel, t: LitTexels) -> LitTexels {
    // Lit meshes alone: an impostor's `source` is the material it bakes.
    if material.source != 0u && (material.flags & 4u) == 0u {
        return splatted(material.source, t, p);
    }
    return t;
}

// `lit` covers what its albedo's alpha does.
fn lit_coverage(material: GpuDraw, c: Cover) -> f32 {
    return c.albedo.a;
}

// `t` under a lit mesh's splat (gpu::MeshSplat; placed.rs GpuSplat, at
// `at` in p_shapes): each layer weighs in by its control channel raised to
// the splat's sharpness times its albedo's alpha (at least its floor), so
// the alpha sharpens where layers meet; the blend's albedo is tinted by
// the mesh albedo's red times the splat's tint or replaces it, its normal
// bends the normal map's or replaces it, and its gloss replaces the map's.
// The control map is on the mesh's own UVs; the layers are too, or under
// world tiling (flags bit 3) on world (x, -z), where their normals stand
// on the world's up and lean with the surface's own normal before they
// come back into the mesh's tangent space. The placement's scale, under
// bit 2, scales either. Gradients throughout: callers branch on the draw.
fn splatted(at: u32, t: LitTexels, p: LitPixel) -> LitTexels {
    let tiling = p_shapes[at].centre;
    let words = bitcast<vec4<u32>>(p_shapes[at].half);
    let rest = p_shapes[at + 1u];
    let normals = bitcast<vec2<u32>>(rest.centre.xy);
    let sharpness = rest.centre.z;
    let alpha_floor = rest.centre.w;
    let flags = words.y;
    var scale = 1.0;
    if (flags & 4u) != 0u {
        let m = p_transforms[p.pick & 0x7fffffffu];
        scale = (length(m[0].xyz) + length(m[1].xyz) + length(m[2].xyz)) / 3.0;
    }
    let world = (flags & 8u) != 0u;
    let flip = vec2<f32>(1.0, -1.0);
    let uv = select(p.mesh_uv, p.world.xz * flip, world);
    let gx = select(p.mesh_duv[0], p.dp[0].xz * flip, world);
    let gy = select(p.mesh_duv[1], p.dp[1].xz * flip, world);
    let control = textureSampleGrad(t_albedos[words.x], s_albedo, p.mesh_uv, p.mesh_duv[0], p.mesh_duv[1]);
    var albedo = vec3<f32>(0.0);
    var normal = vec3<f32>(0.0);
    var gloss = 0.0;
    var sum = 0.0;
    for (var k = 0u; k < 4u; k += 1u) {
        if (flags & (256u << k)) == 0u { continue; }
        let shift = (k & 1u) * 16u;
        let f = tiling[k] * scale;
        let layer = uv * f;
        let a = textureSampleGrad(t_albedos[(select(words.z, words.w, k >= 2u) >> shift) & 0xffffu], s_albedo, layer, gx * f, gy * f);
        // Without a normal map a layer is flat and matte.
        var n = vec4<f32>(0.5, 0.5, 1.0, 0.0);
        let slot = (select(normals.x, normals.y, k >= 2u) >> shift) & 0xffffu;
        if slot != 0u { n = textureSampleGrad(t_albedos[slot], s_albedo, layer, gx * f, gy * f); }
        let w = pow(saturate(control[k]), sharpness * max(a.a, alpha_floor));
        let xy = n.xy * 2.0 - 1.0;
        albedo += decode(a.rgb) * w;
        normal += vec3<f32>(xy, sqrt(saturate(1.0 - dot(xy, xy)))) * w;
        gloss += n.a * w;
        sum += w;
    }
    // Where no layer weighs in, the splat's fallback.
    var blend = rest.half.rgb;
    var bent = vec3<f32>(0.0, 0.0, 1.0);
    if sum > 1e-4 {
        blend = albedo / sum;
        bent = normal / sum;
        gloss /= sum;
    }
    if world {
        // The layers' V runs along -Z, so their tangent space's (x, y, z)
        // is the world's (x, z, -y). That leans by the surface normal's
        // sideways parts and scales by its up (a whiteout blend), then
        // goes back through the frame the normal map bends by.
        let up = vec3<f32>(bent.x, bent.z, -bent.y);
        let n = p.normal;
        let leaned = normalize(vec3<f32>(up.x + n.x, up.y * n.y, up.z + n.z));
        bent = leaned * cotangent(n, p.dp, p.duv);
    }
    var out = t;
    let tint = saturate(blend * rest.half.w * t.albedo.r);
    out.albedo = vec4<f32>(select(blend, tint, (flags & 1u) != 0u), t.albedo.a);
    var n = normalize(bent);
    if (flags & 2u) != 0u {
        let xy = t.normal.xy * 2.0 - 1.0;
        let base = vec3<f32>(xy, sqrt(saturate(1.0 - dot(xy, xy))));
        n = normalize(vec3<f32>(bent.xy + base.xy, bent.z * base.z));
    }
    // Blue stays the normal map's: its emissive mask (lit_glow).
    out.normal = vec4<f32>(n.xy * 0.5 + 0.5, t.normal.b, gloss);
    return out;
}

// `lit`'s surface: what a lit placement's material makes of the pixel `p`,
// from the texels `sampled` for it: under its splat where it has one, its
// glow, then the composition every lit path shares (gpu.wgsl `lit`). The
// row's matrix, which a splat may scale by, is the pick's.
fn lit_surface(material: GpuDraw, p: LitPixel, sampled: LitTexels) -> Lit {
    let t = family_splat(material, p, sampled);
    let n = p.normal;
    let texel = t.albedo.rgb;
    let glow = lit_glow(material, texel, t.normal.b);
    var l = lit(family_albedo(p, texel), p.world, material.tint, glow, n, p.dp, p.duv, t.normal, (material.maps & 0xffffu) != 0u, t.surface, (material.maps >> 16u) != 0u);
    l.surface.specular = material_specular(material);
    return l;
}

// The shading of lit placements (the visibility buffer's resolve,
// visbuf.wgsl): the material's family makes the surface and lights it.
fn placed_lit(material: GpuDraw, p: LitPixel, t: LitTexels) -> Opaque {
    let s = family_surface(material, p, t);
    return shaded(family_light(material, p, s), p.world, p.pick);
}

// `glowing` for `material` (flags bit 8, gpu::MeshData::emissive): its
// colour is the payload's (placed.rs GpuEmissive), its mask `steady`, the
// normal map's blue.
fn lit_glow(material: GpuDraw, texel: vec3<f32>, steady: f32) -> Glowing {
    if (material.flags & 256u) == 0u { return Glowing(1.0, vec3<f32>(0.0)); }
    return glowing(texel, p_shapes[material.payload].centre.rgb, steady);
}

// Decals, in the blend pass: lit as lit placements are, blended by coverage.
@fragment fn fs_placed_decal(in: PlacedOutput) -> @location(0) vec4<f32> {
    let material = p_draws[in.draw];
    let uv = tiled(in.uv, in.world, material.tiling);
    let dp = array<vec3<f32>, 2>(dpdx(in.world), dpdy(in.world));
    let duv = array<vec2<f32>, 2>(dpdx(uv), dpdy(uv));
    let texel = textureSample(t_albedos[material.albedo], s_albedo, uv);
    let normal = textureSample(t_albedos[material.maps & 0xffffu], s_albedo, uv);
    let surface = textureSample(t_albedos[material.maps >> 16u], s_albedo, uv);
    // A projected decal's alpha slot holds its height map, on the box.
    let projected = material.edge_mode > 0.5;
    let map = textureSample(t_albedos[material.alpha], s_albedo, select(in.uv, in.boxed.xy, projected));
    let mask = select(map.g, 1.0, projected);
    let edge = vec4<f32>(material.edge_mode, material.edge_x, material.edge_z, material.edge_a);
    let coverage = decal_coverage(material.opacity, texel.a, mask)
        * decal_edge(in.boxed, edge, vec2<f32>(material.edge_b, material.edge_c), map.r);
    if coverage <= 0.0 { discard; }
    let has_normals = (material.flags & 1u) != 0u;
    let n = surface_normal(in.world, cross(dp[0], dp[1]), in.normal, has_normals);
    // Decals glow by their normal map alone.
    let decoded = decode(texel.rgb);
    var steady = 0.0;
    var colour = vec3<f32>(0.0);
    if (material.flags & 256u) != 0u {
        steady = normal.b;
        colour = p_shapes[material.payload].centre.rgb;
    }
    let glow = glowing(decoded, colour, steady);
    let l = lit(decoded * in.color, in.world, material.tint, glow, n, dp, duv, normal, (material.maps & 0xffffu) != 0u, surface, (material.maps >> 16u) != 0u);
    return vec4<f32>(fog(shade(in.world, l.surface, l.light, has_normals), in.world), coverage);
}

@fragment fn fs_placed_flat(in: PlacedOutput) -> Opaque {
    return Opaque(vec4<f32>(unlit(in.color, in.world), 1.0), in.pick, pack_rgb9e5(in.color));
}

// Depth alone, for sun shadows: the position as vs_placed finds it, and
// only what the cutout needs.
struct DepthOutput {
    @builtin(position) @invariant clip: vec4<f32>,
    // The albedo's UVs with world tiling applied per vertex: tiling is
    // linear in the world, so they interpolate as the world would.
    @location(1) uv: vec2<f32>,
    @location(2) @interpolate(flat) draw: u32,
}

@vertex fn vs_placed_depth(@builtin(vertex_index) value: u32, @builtin(instance_index) instance: u32) -> DepthOutput {
    let drawn = arena_drawn(value, instance);
    let unpacked = unpack(drawn.entry);
    let material = p_draws[unpacked.y];
    let vertex = arena_vertex(drawn.vertex, material);
    let at = placed_at(drawn.vertex, unpacked.x, material, vertex, p_transforms[unpacked.x]);
    return DepthOutput(globals.clip * vec4<f32>(at.world, 1.0), tiled(vertex.uv, at.world, material.tiling), unpacked.y);
}

@fragment fn fs_placed_depth(in: DepthOutput) {
    let material = p_draws[in.draw];
    let texel = textureSample(t_albedos[material.albedo], s_albedo, in.uv);
    if cut_away(material, in.uv, texel) { discard; }
}

// Depth alone of placements nothing cuts out (placed::Ranges::lit_opaque):
// the position as vs_placed finds it, which shadow maps draw without a
// fragment stage.
@vertex fn vs_placed_position(@builtin(vertex_index) value: u32, @builtin(instance_index) instance: u32) -> @invariant @builtin(position) vec4<f32> {
    let drawn = arena_drawn(value, instance);
    let unpacked = unpack(drawn.entry);
    let material = p_draws[unpacked.y];
    let at = placed_at(drawn.vertex, unpacked.x, material, arena_vertex(drawn.vertex, material), p_transforms[unpacked.x]);
    return globals.clip * vec4<f32>(at.world, 1.0);
}

// Impostors. Views sit on a grid over a hemi-octahedron: grid cell (i, j)
// of VIEWS² looks from direction hemi_decode((i, j) / (VIEWS - 1)), and its
// tile shows the piece projected onto that view's right and up axes, one
// radius across. Drawing faces the quad to the camera and blends the three
// views round the camera's direction on the grid by their barycentric
// weights (octahedral impostors, as Brucks's frame blending): each view
// reads where the camera's ray through the quad point crosses that view's
// own plane, so the three line up instead of ghosting, and a turning
// camera fades between views instead of snapping.
const VIEWS: u32 = 8u; // placed::IMPOSTOR_VIEWS

// Unit vector to and from 0..1 octahedral coordinates (two-byte normals).
fn oct_encode(n: vec3<f32>) -> vec2<f32> {
    let p = n.xz / (abs(n.x) + abs(n.y) + abs(n.z));
    var e = p;
    if n.y < 0.0 { e = (1.0 - abs(p.yx)) * select(vec2<f32>(-1.0), vec2<f32>(1.0), p >= vec2<f32>(0.0)); }
    return e * 0.5 + 0.5;
}

fn oct_decode(e: vec2<f32>) -> vec3<f32> {
    let p = e * 2.0 - 1.0;
    var n = vec3<f32>(p.x, 1.0 - abs(p.x) - abs(p.y), p.y);
    let t = max(-n.y, 0.0);
    n.x += select(t, -t, n.x >= 0.0);
    n.z += select(t, -t, n.z >= 0.0);
    return normalize(n);
}

fn hemi_decode(uv: vec2<f32>) -> vec3<f32> {
    let p = uv * 2.0 - 1.0;
    let x = (p.x + p.y) * 0.5;
    let z = (p.x - p.y) * 0.5;
    return normalize(vec3<f32>(x, max(1.0 - abs(x) - abs(z), 0.0), z));
}

fn hemi_encode(d: vec3<f32>) -> vec2<f32> {
    var v = vec3<f32>(d.x, max(d.y, 0.0), d.z);
    v /= max(abs(v.x) + v.y + abs(v.z), 1e-6);
    return vec2<f32>(v.x + v.z, v.x - v.z) * 0.5 + 0.5;
}

// Right and up for a view looking back along `dir` (which points at the
// camera). Straight down needs another reference axis.
fn view_axes(dir: vec3<f32>) -> mat2x3<f32> {
    let reference = select(vec3<f32>(0.0, 1.0, 0.0), vec3<f32>(0.0, 0.0, 1.0), abs(dir.y) > 0.999);
    let right = normalize(cross(reference, dir));
    return mat2x3<f32>(right, cross(dir, right));
}

struct ImpostorOutput {
    // Invariant: the depth prepass must match the shading draw exactly.
    @builtin(position) @invariant clip: vec4<f32>,
    @location(0) world: vec3<f32>,
    // Each blended view's place in the atlas in tile units (its cell plus
    // 0..1 across the tile), its cell, and its weight.
    @location(1) uv0: vec2<f32>,
    @location(3) uv1: vec2<f32>,
    @location(4) uv2: vec2<f32>,
    @location(10) @interpolate(flat) weights: vec3<f32>,
    @location(11) @interpolate(flat) cells01: vec4<f32>,
    @location(12) @interpolate(flat) cell2: vec2<f32>,
    // The baked mesh's ground tint amount, and 1 when it lights by the
    // ground's normal (adaption mode 3) as lit placements do.
    // The source's terrain tint, 1 where it lies on the ground, and its
    // Surface::specular.
    @location(13) @interpolate(flat) tint: vec3<f32>,
    // A radius toward the camera, in the world.
    @location(14) @interpolate(flat) lift: vec3<f32>,
    @location(2) color: vec3<f32>,
    @location(5) @interpolate(flat) pick: u32,
    @location(6) @interpolate(flat) layer: u32,
    // Model-to-world for normals: cofactor columns, sign folded in.
    @location(7) @interpolate(flat) n0: vec3<f32>,
    @location(8) @interpolate(flat) n1: vec3<f32>,
    @location(9) @interpolate(flat) n2: vec3<f32>,
}

// Where corner `index` of the quad of the impostor at `row` (material `d`)
// lies, and the three views it blends there: the shaded and the depth-only
// vertex stages both place it with this, so their depths agree.
struct ImpostorQuad {
    world: vec3<f32>,
    // In tile units: the fragment finds each view's tile from its floor.
    uvs: array<vec2<f32>, 3>,
    cells: array<vec2<f32>, 3>,
    weights: vec3<f32>,
    // Toward the camera, in the model.
    local: vec3<f32>,
    // Model-to-world for normals: cofactor columns, sign folded in.
    cofactor: mat3x3<f32>,
}

fn impostor_quad(index: u32, row: u32, d: u32) -> ImpostorQuad {
    let corners = array<vec2<f32>, 6>(vec2<f32>(-1.0, -1.0), vec2<f32>(1.0, -1.0), vec2<f32>(1.0, 1.0),
        vec2<f32>(-1.0, -1.0), vec2<f32>(1.0, 1.0), vec2<f32>(-1.0, 1.0));
    let shape = p_shapes[d];
    let m = p_transforms[row];
    let c0 = m[0].xyz;
    let c1 = m[1].xyz;
    let c2 = m[2].xyz;
    let flip = sign(dot(c0, cross(c1, c2)));
    var out: ImpostorQuad;
    out.cofactor = mat3x3<f32>(cross(c1, c2), cross(c2, c0), cross(c0, c1)) * flip;
    let centre = (m * vec4<f32>(shape.centre.xyz, 1.0)).xyz;
    var toward = globals.eye.xyz;
    if globals.eye.w > 0.5 { toward = globals.eye.xyz - centre; }
    // The inverse is the cofactor transposed over the determinant; only the
    // direction matters here.
    let local = normalize(transpose(out.cofactor) * toward);
    out.local = local;
    // The camera's place on the view grid and the triangle of views
    // round it, with barycentric weights.
    let last = f32(VIEWS - 1u);
    let g = hemi_encode(local) * last;
    let base = clamp(floor(g), vec2<f32>(0.0), vec2<f32>(last - 1.0));
    let f = clamp(g - base, vec2<f32>(0.0), vec2<f32>(1.0));
    if f.x + f.y <= 1.0 {
        out.cells = array<vec2<f32>, 3>(base, base + vec2<f32>(1.0, 0.0), base + vec2<f32>(0.0, 1.0));
        out.weights = vec3<f32>(1.0 - f.x - f.y, f.x, f.y);
    } else {
        out.cells = array<vec2<f32>, 3>(base + vec2<f32>(1.0), base + vec2<f32>(0.0, 1.0), base + vec2<f32>(1.0, 0.0));
        out.weights = vec3<f32>(f.x + f.y - 1.0, 1.0 - f.x, 1.0 - f.y);
    }
    // The quad faces the camera exactly, as far each way as the views can
    // show something.
    let facing = view_axes(local);
    let reach = impostor_extent(local, facing, out.cells, p_draws[d].albedo, shape, centre, m);
    let st = corners[index];
    let q = facing[0] * select(reach.x, reach.y, st.x > 0.0) + facing[1] * select(reach.z, reach.w, st.y > 0.0);
    out.world = (m * vec4<f32>(shape.centre.xyz + q, 1.0)).xyz;
    for (var k = 0; k < 3; k++) {
        let dir = hemi_decode(out.cells[k] / last);
        let axes = view_axes(dir);
        // Along the camera ray onto the view's plane through the centre.
        let on = q - local * (dot(q, dir) / max(dot(local, dir), 0.2));
        let t = vec2<f32>(dot(on, axes[0]), dot(on, axes[1])) / shape.centre.w;
        out.uvs[k] = out.cells[k] + vec2<f32>(t.x + 1.0, 1.0 - t.y) * 0.5;
    }
    return out;
}

// How far along the quad's right and up axes (`facing`) quad points can
// show anything, each way: least right, most right, least up, most up, in
// model units from the centre. A point q reads view k (direction d) where
// q's camera ray crosses d's plane through the centre, and that crossing
// p, in the plane, has p·a = q·a for each quad axis a (both lie across the
// camera's direction); so q's reach along a is how far along a view k's
// coverage lies in its plane, from the texels it covers
// (t_impostor_bounds), the most of the three views'. Past that, a margin
// for filtering: two texels of the finest level, and for the coarser of
// the two levels a trilinear sample blends (up to two pixels a texel, more
// where the crossing stretches a view by 1 / (local·d)), whose 2x2 means
// and bilinear taps reach past what is covered, six pixels over that
// stretch: at four, 1800's far trees still lost a few edge pixels. Every
// pixel this leaves out reads no coverage in any of the three views, so
// the picture is the one the full quad draws but for a few hundred crown
// edge pixels a view, where coverage sits on the cut and rounding moves
// with the corners. It is the depth prepass that pays per quad pixel: a
// full radius each way left half a crown's quad to be cut away again, and
// the mesh's box, projected, cost Albion's view at 5K 0.15 ms where this
// costs 0.105. Perspective views only: shadow maps and orthographic views
// keep the full quad, their pixels being no view's.
fn impostor_extent(local: vec3<f32>, facing: mat2x3<f32>, cells: array<vec2<f32>, 3>, layer: u32, shape: Shape, centre: vec3<f32>, m: mat4x4<f32>) -> vec4<f32> {
    let radius = shape.centre.w;
    let full = vec4<f32>(-radius, radius, -radius, radius);
    if globals.eye.w < 0.5 { return full; }
    let tile = f32(textureDimensions(t_impostor_albedo).x / VIEWS);
    var lo = vec2<f32>(1e30);
    var hi = vec2<f32>(-1e30);
    var facing_least = 1.0;
    for (var k = 0; k < 3; k++) {
        let d = hemi_decode(cells[k] / f32(VIEWS - 1u));
        // impostor_quad clamps the crossing there: no longer this map.
        if dot(local, d) < 0.25 { return full; }
        facing_least = min(facing_least, dot(local, d));
        let b = vec4<f32>(textureLoad(t_impostor_bounds, vec2<u32>(u32(cells[k].y) * VIEWS + u32(cells[k].x), layer), 0));
        if b.z <= b.x { continue; }
        // The covered texels in radii along the view's right and up, as
        // impostor_quad's tile coordinates run (rows down).
        let s = vec4<f32>(b.x, b.z, tile - b.w, tile - b.y) * (2.0 / tile) - 1.0;
        let axes = view_axes(d);
        for (var a = 0; a < 2; a++) {
            let x = dot(axes[0], facing[a]) * s.xy;
            let y = dot(axes[1], facing[a]) * s.zw;
            lo[a] = min(lo[a], (min(x.x, x.y) + min(y.x, y.y)) * radius);
            hi[a] = max(hi[a], (max(x.x, x.y) + max(y.x, y.y)) * radius);
        }
    }
    // None of the three covers anything.
    if lo.x > hi.x { return vec4<f32>(0.0); }
    // Screen pixels a radius spans along each axis at the centre; a pixel
    // in model units is the radius over the fewer.
    let c = globals.clip * vec4<f32>(centre, 1.0);
    if c.w <= 1e-4 { return full; }
    var span = 1e30;
    for (var a = 0; a < 2; a++) {
        // The point's xyz with w 1, as `centre`: m's bottom row carries
        // terrain adaption (gpu::adapting), no part of a point's w. Taken
        // as one it moved an adapting row's point toward the world
        // origin's place on screen, and the margin shrank with the span.
        let at = (m * vec4<f32>(shape.centre.xyz + facing[a] * shape.centre.w, 1.0)).xyz;
        let e = globals.clip * vec4<f32>(at, 1.0);
        if e.w <= 1e-4 { return full; }
        span = min(span, length((e.xy / e.w - c.xy / c.w) * globals.viewport.xy * 0.5));
    }
    let pixel = shape.centre.w / max(span, 1e-3);
    let margin = 6.0 * pixel / facing_least + shape.half.w;
    return clamp(vec4<f32>(lo.x - margin, hi.x + margin, lo.y - margin, hi.y + margin), vec4<f32>(-radius), vec4<f32>(radius));
}

// An impostor's quad is its own six index values, whose vertex is the
// corner (placed::Arena::pack).
@vertex fn vs_impostor(@builtin(vertex_index) value: u32, @builtin(instance_index) instance: u32) -> ImpostorOutput {
    let drawn = arena_drawn(value, instance);
    let unpacked = unpack(drawn.entry);
    let row = unpacked.x;
    let d = unpacked.y;
    let material = p_draws[d];
    let quad = impostor_quad(drawn.vertex, row, d);
    var out: ImpostorOutput;
    out.clip = globals.clip * vec4<f32>(quad.world, 1.0);
    out.world = quad.world;
    out.uv0 = quad.uvs[0];
    out.uv1 = quad.uvs[1];
    out.uv2 = quad.uvs[2];
    out.weights = quad.weights;
    out.lift = (p_transforms[row] * vec4<f32>(quad.local * p_shapes[d].centre.w, 0.0)).xyz;
    out.cells01 = vec4<f32>(quad.cells[0], quad.cells[1]);
    out.cell2 = quad.cells[2];
    let source = p_draws[material.source];
    out.tint = vec3<f32>(source.tint, select(0.0, 1.0, ((source.flags >> 3u) & 3u) == 3u), f32(material_specular(source)));
    out.color = vec3<f32>(material.gain);
    out.pick = row | 0x80000000u;
    out.layer = material.albedo;
    out.n0 = quad.cofactor[0];
    out.n1 = quad.cofactor[1];
    out.n2 = quad.cofactor[2];
    return out;
}

// What the cut alone needs (fs_impostor_shadow): the prepass and shadows.
struct ImpostorDepthOutput {
    // Invariant: the depth prepass must match the shaded draw exactly.
    @builtin(position) @invariant clip: vec4<f32>,
    @location(1) uv0: vec2<f32>,
    @location(3) uv1: vec2<f32>,
    @location(4) uv2: vec2<f32>,
    @location(10) @interpolate(flat) weights: vec3<f32>,
    @location(11) @interpolate(flat) cells01: vec4<f32>,
    @location(12) @interpolate(flat) cell2: vec2<f32>,
    @location(6) @interpolate(flat) layer: u32,
}

@vertex fn vs_impostor_depth(@builtin(vertex_index) value: u32, @builtin(instance_index) instance: u32) -> ImpostorDepthOutput {
    let drawn = arena_drawn(value, instance);
    let unpacked = unpack(drawn.entry);
    let quad = impostor_quad(drawn.vertex, unpacked.x, unpacked.y);
    return ImpostorDepthOutput(
        globals.clip * vec4<f32>(quad.world, 1.0),
        quad.uvs[0], quad.uvs[1], quad.uvs[2],
        quad.weights, vec4<f32>(quad.cells[0], quad.cells[1]), quad.cells[2],
        p_draws[unpacked.y].albedo,
    );
}

// One view's texels at `uv` (in tile units: cell plus 0..1 across it);
// nothing outside its tile. Gradients come from the caller, before any
// branch.
struct ImpostorTexel { albedo: vec4<f32>, normal: vec4<f32> }

fn impostor_view(uv: vec2<f32>, cell: vec2<f32>, layer: u32, dx: vec2<f32>, dy: vec2<f32>, normals: bool) -> ImpostorTexel {
    let inside = all(uv >= cell) && all(uv <= cell + 1.0);
    let at = uv / f32(VIEWS);
    var out: ImpostorTexel;
    out.albedo = textureSampleGrad(t_impostor_albedo, s_albedo, at, layer, dx, dy);
    if normals {
        out.normal = vec4<f32>(
            textureSampleGrad(t_impostor_normal, s_albedo, at, layer, dx, dy).xy,
            textureSampleGrad(t_impostor_rest, s_albedo, at, layer, dx, dy).xy,
        );
    }
    if !inside { out.albedo = vec4<f32>(0.0); }
    return out;
}

// The three views blended: color, normal, depth (-1..1 radii toward the
// camera) and gloss weighed by coverage.
struct Impostor { albedo: vec4<f32>, normal: vec3<f32>, depth: f32, gloss: f32 }

fn impostor(in: ImpostorOutput, normals: bool) -> Impostor {
    // All three tiles share one footprint: the camera sits between them.
    let at = in.uv0 / f32(VIEWS);
    let dx = dpdx(at);
    let dy = dpdy(at);
    let uvs = array<vec2<f32>, 3>(in.uv0, in.uv1, in.uv2);
    let cells = array<vec2<f32>, 3>(in.cells01.xy, in.cells01.zw, in.cell2);
    var out: Impostor;
    out.albedo = vec4<f32>(0.0);
    out.normal = vec3<f32>(0.0);
    out.depth = 0.0;
    out.gloss = 0.0;
    for (var k = 0; k < 3; k++) {
        let w = in.weights[k];
        let t = impostor_view(uvs[k], cells[k], in.layer, dx, dy, normals);
        let a = t.albedo.a * w;
        out.albedo += vec4<f32>(t.albedo.rgb * a, a);
        if normals {
            out.normal += oct_decode(t.normal.xy) * a;
            out.depth += (t.normal.z * 2.0 - 1.0) * a;
            out.gloss += t.normal.w * a;
        }
    }
    if out.albedo.a > 0.0 {
        out.albedo = vec4<f32>(out.albedo.rgb / out.albedo.a, out.albedo.a);
        out.depth /= out.albedo.a;
        out.gloss /= out.albedo.a;
    }
    return out;
}

// After the prepass, at equal depth: the prepass made the cut, and cutting
// again here could round the other way at the threshold and leave a hole.
@fragment fn fs_impostor_shaded(in: ImpostorOutput) -> Opaque {
    return impostor_lit(in, impostor(in, true));
}

fn impostor_lit(in: ImpostorOutput, i: Impostor) -> Opaque {
    var n = normalize(mat3x3<f32>(in.n0, in.n1, in.n2) * i.normal);
    if in.tint.y > 0.5 && lights.terrain.w > 0.5 { n = ground_normal(in.world.xz); }
    // Lit where the crown is, not on the quad through its centre: there
    // the tree's own shadow covers it.
    let world = in.world + in.lift * i.depth;
    let albedo = tinted(decode(i.albedo.rgb) * in.color, world, in.tint.x);
    var s = plain(albedo, n);
    s.gloss = i.gloss;
    s.specular = u32(in.tint.z);
    return shaded(shade(world, s, vec3<f32>(0.0), true), world, in.pick);
}

// Depth alone, cut at half the blended coverage: the prepass, and shadows. The
// views go heaviest first and stop once the rest cannot move the blended
// coverage across the cut: crowns far off overlap many times over, and
// reading all three views everywhere made the prepass cost three times the
// shading (Albion, gpu_bench).
@fragment fn fs_impostor_shadow(in: ImpostorDepthOutput) {
    let at = in.uv0 / f32(VIEWS);
    let dx = dpdx(at);
    let dy = dpdy(at);
    let uvs = array<vec2<f32>, 3>(in.uv0, in.uv1, in.uv2);
    let cells = array<vec2<f32>, 3>(in.cells01.xy, in.cells01.zw, in.cell2);
    let w = in.weights;
    var order = vec3<u32>(0u, 1u, 2u);
    if w[order.y] > w[order.x] { order = order.yxz; }
    if w[order.z] > w[order.y] { order = order.xzy; }
    if w[order.y] > w[order.x] { order = order.yxz; }
    var covered = 0.0;
    var left = w.x + w.y + w.z;
    for (var i = 0u; i < 3u; i++) {
        let k = order[i];
        covered += impostor_view(uvs[k], cells[k], in.layer, dx, dy, false).albedo.a * w[k];
        left -= w[k];
        if covered >= 0.5 { return; }
        if covered + left < 0.5 { break; }
    }
    discard;
}

// Baking: one draw per impostor of its full-detail mesh, VIEWS² instances,
// each projecting into its own tile of the impostor's layer. The instance
// number encodes the impostor's material: instance = material * VIEWS² + view,
// through a direct draw's first instance, which wgpu keeps on every backend.
struct BakeOutput {
    @builtin(position) clip: vec4<f32>,
    @location(0) local: vec3<f32>,
    @location(1) uv: vec2<f32>,
    @location(2) color: vec3<f32>,
    @location(3) st: vec2<f32>,
    @location(4) normal: vec3<f32>,
    @location(5) @interpolate(flat) source: u32,
    @location(6) @interpolate(flat) dir: vec3<f32>,
    // The vertex about the quad's centre, in radii.
    @location(7) lift: vec3<f32>,
}

struct Baked {
    @location(0) albedo: vec4<f32>,
    @location(1) normal: vec2<f32>,
    // Depth and gloss.
    @location(2) rest: vec2<f32>,
}

@vertex fn vs_bake(@builtin(vertex_index) index: u32, @builtin(instance_index) instance: u32) -> BakeOutput {
    let views = VIEWS * VIEWS;
    let d = instance / views;
    let view = instance % views;
    let cell = vec2<f32>(f32(view % VIEWS), f32(view / VIEWS));
    let shape = p_shapes[d];
    let source = p_draws[d].source;
    let material = p_draws[source];
    let vertex = arena_vertex(arena_index(index, material), material);
    let dir = hemi_decode(cell / f32(VIEWS - 1u));
    let axes = view_axes(dir);
    let q = (vertex.position.xyz - shape.centre.xyz) / shape.centre.w;
    let st = vec2<f32>(dot(q, axes[0]), dot(q, axes[1]));
    var out: BakeOutput;
    // Tile (i, j) of the layer; texture rows run down, clip y up.
    let x = -1.0 + 2.0 * (cell.x + (st.x + 1.0) * 0.5) / f32(VIEWS);
    let y = 1.0 - 2.0 * (cell.y + (1.0 - st.y) * 0.5) / f32(VIEWS);
    out.clip = vec4<f32>(x, y, 0.5 - 0.5 * dot(q, dir), 1.0);
    out.local = vertex.position.xyz;
    out.uv = vertex.uv;
    out.color = rgb(vertex.color);
    out.st = st;
    out.normal = vec3<f32>(0.0);
    if (material.flags & 1u) != 0u { out.normal = octahedral(vertex.normal); }
    out.source = source;
    out.dir = dir;
    out.lift = q;
    return out;
}

@fragment fn fs_bake(in: BakeOutput) -> Baked {
    let dp = array<vec3<f32>, 2>(dpdx(in.local), dpdy(in.local));
    let duv = array<vec2<f32>, 2>(dpdx(in.uv), dpdy(in.uv));
    // The bake has the mesh alone, in model space: no row, instance word
    // or view. `lit` leaves out what it would take from those: the
    // ground's tint and its glow, which the impostor's draw adds or does
    // without (gpu::MeshData::emissive), and its splat.
    // TODO: bake the splat; only one that scales with its placement needs
    // the row.
    var material = p_draws[in.source];
    material.tint = 0.0;
    material.flags &= ~0x100u;
    material.source = 0u;
    // Cut where the mesh cuts (`coverage_cut`, the flags' top byte): baked
    // at the half, a far impostor of a 0.3 cutout lost the texels its own
    // levels keep and read thinner.
    var texel = textureSample(t_albedos[material.albedo], s_albedo, in.uv);
    let normal = textureSample(t_albedos[material.maps & 0xffffu], s_albedo, in.uv);
    let surface = textureSample(t_albedos[material.maps >> 16u], s_albedo, in.uv);
    // Outside its own tile a view would spill into its neighbours.
    if any(abs(in.st) > vec2<f32>(1.0)) { discard; }
    if (material.flags & 2u) != 0u { texel.a = 1.0; }
    if cut_away(material, in.uv, texel) { discard; }
    var n = normalize(cross(dp[0], dp[1]));
    if (material.flags & 1u) != 0u && dot(in.normal, in.normal) > 1e-12 { n = normalize(in.normal); }
    if dot(n, in.dir) < 0.0 { n = -n; }
    // Colours as stored: the impostor decodes what it draws.
    let p = LitPixel(in.local, in.color, n, dp, in.uv, duv, in.uv, duv, 0u, 0u);
    let s = family_surface(material, p, LitTexels(texel, normal, surface)).surface;
    // Depth (toward the view, in radii, as 0..1) and gloss bake beside the
    // normal; occlusion darkens the albedo.
    let depth = saturate(0.5 + 0.5 * dot(in.lift, in.dir));
    return Baked(vec4<f32>(s.albedo * s.occlusion, 1.0), oct_encode(s.normal), vec2<f32>(depth, s.gloss));
}
