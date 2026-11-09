// Placed pieces in one multi-draw per pipeline (placed.rs). Appended to
// gpu.wgsl, whose pass bindings and lighting it shares. Every resident piece
// level's geometry, material and albedo sit in shared arrays (group 3); each
// visible-list entry carries its material above its row (group 2), and the
// indirect arguments start each draw's instances at its visible base, so
// instance_index finds the entry.
// Material of one piece level: albedo slot in t_albedos, flags (bit 0 has
// normals, bit 1 ignore texture alpha, bit 2 impostor) and the vertex color
// gain. Impostors: albedo is the atlas layer, source the material they bake.
// One per distinct mesh, indexed by material: thinning buckets and pieces
// repeating the mesh share it.
// maps: normal map slot in the low half, surface map slot above; 0 none.
// tint: how much the ground's tint multiplies the albedo. vertex: the
// mesh's first vertex; parts: where its thinning parts start in p_parts,
// NO_PARTS for meshes that do not thin.
struct GpuDraw { albedo: u32, flags: u32, gain: f32, source: u32, maps: u32, tint: f32, vertex: u32, parts: u32 }
const NO_PARTS: u32 = 0xffffffffu; // placed::NO_PARTS

@group(2) @binding(0) var<storage, read> p_transforms: array<mat4x4<f32>>;
@group(2) @binding(1) var<storage, read> p_visible: array<u32>;
// Per row, the share of a thinning mesh's parts drawn (cull.wgsl).
@group(2) @binding(2) var<storage, read> p_keep: array<f32>;
@group(3) @binding(0) var<storage, read> p_draws: array<GpuDraw>;
@group(3) @binding(1) var<storage, read> p_vertices: array<GpuVertex>;
// Rebased onto the shared vertex array; each draw's first vertex is its
// first index here.
@group(3) @binding(2) var<storage, read> p_indices: array<u32>;
@group(3) @binding(3) var t_albedos: binding_array<texture_2d<f32>>;
// Per material; impostors: model-space centre and radius of the quad.
@group(3) @binding(4) var<storage, read> p_shapes: array<vec4<f32>>;
// Per vertex of the thinning meshes: its part's centre and rank
// (MeshData::thinning), w 0 for vertices drawn as they are.
@group(3) @binding(5) var<storage, read> p_parts: array<vec4<f32>>;
const THIN_FADE: f32 = 0.05; // placed::THIN_FADE
// Baked impostor views, one layer per impostor mesh (GpuDraw.albedo): color
// and coverage, and model-space normals octahedral in two bytes.
@group(1) @binding(0) var t_impostor_albedo: texture_2d_array<f32>;
@group(1) @binding(1) var t_impostor_normal: texture_2d_array<f32>;

// The row in a visible entry's low bits, the material above; the split follows
// the table (placed::row_bits, carried in globals.viewport.w).
fn unpack(entry: u32) -> vec2<u32> {
    let bits = u32(globals.viewport.w);
    return vec2<u32>(entry & ((1u << bits) - 1u), entry >> bits);
}

struct PlacedOutput {
    @builtin(position) clip: vec4<f32>,
    @location(0) world: vec3<f32>,
    @location(1) uv: vec2<f32>,
    @location(2) color: vec3<f32>,
    @location(4) normal: vec3<f32>,
    @location(5) @interpolate(flat) pick: u32,
    @location(6) @interpolate(flat) draw: u32,
}

// Drawn indexed from p_indices, so `index` is the vertex and the post-
// transform cache shades shared vertices once.
@vertex fn vs_placed(@builtin(vertex_index) index: u32, @builtin(instance_index) instance: u32) -> PlacedOutput {
    let entry = p_visible[instance];
    let unpacked = unpack(entry);
    let row = unpacked.x;
    let d = unpacked.y;
    let material = p_draws[d];
    let vertex = p_vertices[index];
    let m = p_transforms[row];
    var local = vertex.position.xyz;
    var part = vec4<f32>(0.0);
    if material.parts != NO_PARTS { part = p_parts[material.parts + index - material.vertex]; }
    if part.w > 0.0 {
        // Kept parts grow to keep the canopy's coverage; those ranked within
        // a bucket past the share kept shrink to their centre, so parts
        // fade out one by one instead of popping with the bucket.
        let keep = p_keep[row];
        let grow = min(sqrt(1.0 / max(keep, 1e-3)), 2.5);
        let fade = clamp((keep * (1.0 + THIN_FADE) - part.w) / (keep * THIN_FADE), 0.0, 1.0);
        local = part.xyz + (local - part.xyz) * grow * fade;
    }
    let world = (m * vec4<f32>(local, 1.0)).xyz;
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
        out.normal = cofactor * octahedral(vertex.normal) * sign(dot(c0, cross(c1, c2)));
    }
    out.pick = row | 0x80000000u;
    out.draw = d;
    return out;
}

fn placed_albedo(in: PlacedOutput) -> vec4<f32> {
    let material = p_draws[in.draw];
    let texel = textureSample(t_albedos[material.albedo], s_albedo, in.uv);
    return select(texel, vec4<f32>(texel.rgb, 1.0), (material.flags & 2u) != 0u);
}

@fragment fn fs_placed_lit(in: PlacedOutput) -> Opaque {
    let dp = array<vec3<f32>, 2>(dpdx(in.world), dpdy(in.world));
    let duv = array<vec2<f32>, 2>(dpdx(in.uv), dpdy(in.uv));
    let material = p_draws[in.draw];
    let texel = placed_albedo(in);
    // Slot 0 (white) when a map is missing; sampled anyway, since sampling
    // under a branch on the draw is not uniform control flow.
    let normal = textureSample(t_albedos[material.maps & 0xffffu], s_albedo, in.uv);
    let surface = textureSample(t_albedos[material.maps >> 16u], s_albedo, in.uv);
    if texel.a < 0.5 { discard; }
    let has_normals = (material.flags & 1u) != 0u;
    let n = surface_normal(in.world, cross(dp[0], dp[1]), in.normal, has_normals);
    let albedo = tinted(decode(texel.rgb) * in.color, in.world, material.tint);
    let s = mapped(albedo, n, dp, duv, normal, (material.maps & 0xffffu) != 0u, surface, (material.maps >> 16u) != 0u);
    let color = present(fog(shade(in.world, s, has_normals), in.world));
    return Opaque(vec4<f32>(color, 1.0), vec2<u32>(in.pick, bitcast<u32>(in.clip.z)));
}

@fragment fn fs_placed_flat(in: PlacedOutput) -> Opaque {
    return Opaque(vec4<f32>(in.color, 1.0), vec2<u32>(in.pick, bitcast<u32>(in.clip.z)));
}

@fragment fn fs_placed_shadow(in: PlacedOutput) {
    if placed_albedo(in).a < 0.5 { discard; }
}

// Impostors. Views sit on a grid over a hemi-octahedron: grid cell (i, j)
// of VIEWS² looks from direction hemi_decode((i, j) / (VIEWS - 1)), and its
// tile shows the piece projected onto that view's right and up axes, one
// radius across. Drawing picks the cell nearest the camera and orients the
// quad the same way, so quad corners map straight onto the tile.
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
    @builtin(position) clip: vec4<f32>,
    @location(0) world: vec3<f32>,
    @location(1) uv: vec2<f32>,
    @location(2) color: vec3<f32>,
    @location(5) @interpolate(flat) pick: u32,
    @location(6) @interpolate(flat) layer: u32,
    // Model-to-world for normals: cofactor columns, sign folded in.
    @location(7) @interpolate(flat) n0: vec3<f32>,
    @location(8) @interpolate(flat) n1: vec3<f32>,
    @location(9) @interpolate(flat) n2: vec3<f32>,
}

@vertex fn vs_impostor(@builtin(vertex_index) index: u32, @builtin(instance_index) instance: u32) -> ImpostorOutput {
    let corners = array<vec2<f32>, 6>(vec2<f32>(-1.0, -1.0), vec2<f32>(1.0, -1.0), vec2<f32>(1.0, 1.0),
        vec2<f32>(-1.0, -1.0), vec2<f32>(1.0, 1.0), vec2<f32>(-1.0, 1.0));
    let entry = p_visible[instance];
    let unpacked = unpack(entry);
    let row = unpacked.x;
    let d = unpacked.y;
    let shape = p_shapes[d];
    let material = p_draws[d];
    let m = p_transforms[row];
    let c0 = m[0].xyz;
    let c1 = m[1].xyz;
    let c2 = m[2].xyz;
    let flip = sign(dot(c0, cross(c1, c2)));
    let cofactor = mat3x3<f32>(cross(c1, c2), cross(c2, c0), cross(c0, c1)) * flip;
    let centre = (m * vec4<f32>(shape.xyz, 1.0)).xyz;
    var toward = globals.eye.xyz;
    if globals.eye.w > 0.5 { toward = globals.eye.xyz - centre; }
    // The inverse is the cofactor transposed over the determinant; only the
    // direction matters here.
    let local = normalize(transpose(cofactor) * toward);
    let cell = clamp(round(hemi_encode(local) * f32(VIEWS - 1u)), vec2<f32>(0.0), vec2<f32>(f32(VIEWS - 1u)));
    let axes = view_axes(hemi_decode(cell / f32(VIEWS - 1u)));
    let st = corners[index];
    let p = shape.xyz + (axes[0] * st.x + axes[1] * st.y) * shape.w;
    let world = (m * vec4<f32>(p, 1.0)).xyz;
    var out: ImpostorOutput;
    out.clip = globals.clip * vec4<f32>(world, 1.0);
    out.world = world;
    out.uv = (cell + vec2<f32>(st.x + 1.0, 1.0 - st.y) * 0.5) / f32(VIEWS);
    out.color = vec3<f32>(material.gain);
    out.pick = row | 0x80000000u;
    out.layer = material.albedo;
    out.n0 = cofactor[0];
    out.n1 = cofactor[1];
    out.n2 = cofactor[2];
    return out;
}

@fragment fn fs_impostor(in: ImpostorOutput) -> Opaque {
    let albedo = textureSample(t_impostor_albedo, s_albedo, in.uv, in.layer);
    let packed = textureSample(t_impostor_normal, s_albedo, in.uv, in.layer).xy;
    if albedo.a < 0.5 { discard; }
    let n = normalize(mat3x3<f32>(in.n0, in.n1, in.n2) * oct_decode(packed));
    let color = present(fog(shade(in.world, plain(decode(albedo.rgb) * in.color, n), true), in.world));
    return Opaque(vec4<f32>(color, 1.0), vec2<u32>(in.pick, bitcast<u32>(in.clip.z)));
}

@fragment fn fs_impostor_shadow(in: ImpostorOutput) {
    if textureSample(t_impostor_albedo, s_albedo, in.uv, in.layer).a < 0.5 { discard; }
}

// Baking: one draw per impostor of its full-detail mesh, VIEWS² instances,
// each projecting into its own tile of the impostor's layer. The instance
// number encodes the impostor's material: instance = material * VIEWS² + view.
struct BakeOutput {
    @builtin(position) clip: vec4<f32>,
    @location(0) local: vec3<f32>,
    @location(1) uv: vec2<f32>,
    @location(2) color: vec3<f32>,
    @location(3) st: vec2<f32>,
    @location(4) normal: vec3<f32>,
    @location(5) @interpolate(flat) source: u32,
    @location(6) @interpolate(flat) dir: vec3<f32>,
}

struct Baked {
    @location(0) albedo: vec4<f32>,
    @location(1) normal: vec4<f32>,
}

@vertex fn vs_bake(@builtin(vertex_index) index: u32, @builtin(instance_index) instance: u32) -> BakeOutput {
    let views = VIEWS * VIEWS;
    let d = instance / views;
    let view = instance % views;
    let cell = vec2<f32>(f32(view % VIEWS), f32(view / VIEWS));
    let shape = p_shapes[d];
    let source = p_draws[d].source;
    let material = p_draws[source];
    let vertex = p_vertices[p_indices[index]];
    let dir = hemi_decode(cell / f32(VIEWS - 1u));
    let axes = view_axes(dir);
    let q = (vertex.position.xyz - shape.xyz) / shape.w;
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
    return out;
}

@fragment fn fs_bake(in: BakeOutput) -> Baked {
    let dp = array<vec3<f32>, 2>(dpdx(in.local), dpdy(in.local));
    let duv = array<vec2<f32>, 2>(dpdx(in.uv), dpdy(in.uv));
    let material = p_draws[in.source];
    var texel = textureSample(t_albedos[material.albedo], s_albedo, in.uv);
    let normal = textureSample(t_albedos[material.maps & 0xffffu], s_albedo, in.uv);
    let surface = textureSample(t_albedos[material.maps >> 16u], s_albedo, in.uv);
    // Outside its own tile a view would spill into its neighbours.
    if any(abs(in.st) > vec2<f32>(1.0)) { discard; }
    if (material.flags & 2u) != 0u { texel.a = 1.0; }
    if texel.a < 0.5 { discard; }
    var n = normalize(cross(dp[0], dp[1]));
    if (material.flags & 1u) != 0u && dot(in.normal, in.normal) > 1e-12 { n = normalize(in.normal); }
    if dot(n, in.dir) < 0.0 { n = -n; }
    // The bake keeps no gloss or metalness; occlusion darkens the albedo.
    let s = mapped(texel.rgb * in.color, n, dp, duv, normal, (material.maps & 0xffffu) != 0u, surface, (material.maps >> 16u) != 0u);
    return Baked(vec4<f32>(s.albedo * s.occlusion, 1.0), vec4<f32>(oct_encode(s.normal), 0.0, 1.0));
}
