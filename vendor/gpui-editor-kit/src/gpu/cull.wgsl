// Culls placements and picks each one's detail level on the GPU (placed.rs).
// One thread per placement: test its box against the frustum, measure how
// many screen pixels a metre covers at its nearest point, and append its row
// to the visible list of the (piece, level) draw, counting in that draw's
// indirect arguments.
//
// Views cull in two phases around a depth pyramid (hzb.wgsl). Phase 1 takes
// only placements visible last frame (c_bits); they and the terrain are
// drawn, and the pyramid is built from that depth. Phase 2 tests every
// placement against it: hidden ones lose their bit, visible ones keep or
// gain it, and those phase 1 skipped are appended for a second draw. So
// newly revealed placements draw the frame they appear.
struct CullGlobals {
    // Clip rows (x, y, z, w) of the view or the shadow map: levels follow
    // how large a metre appears in it.
    rows: array<vec4<f32>, 4>,
    // Planes to cull against, inside where dot(xyz, p) + w >= 0.
    planes: array<vec4<f32>, 6>,
    // x: height in pixels of what the rows project onto. z: 1 to count what
    // is drawn into c_totals.
    info: vec4<f32>,
    // x: placement count. y: bits to shift the material left by in
    // visible entries, 0 to store bare rows (placed.rs). z: phase, 0 for
    // one pass without occlusion (shadow maps), else 1 or 2. w: pyramid
    // levels.
    counts: vec4<u32>,
    // World to the render target's clip space, for occlusion.
    clip: mat4x4<f32>,
    // Render target width and height, pyramid level 0 width and height.
    hzb: vec4<f32>,
}
// Model-space box of the full-detail mesh (lo.w 1: levels count world
// metres, not the placement's own); the minimum pixels per metre of
// each level (the last is drawn below every threshold); the first draw
// index, level count, and when w is 1 the impostor's draw (z), which is the
// last level. material: each level's, the impostor's last; pieces drawing
// the same mesh share one.
// thin: a thinning mesh's full and least (MeshData::thinning); z 1 when
// each level is THIN_BUCKETS prefix draws from draw.x, w 1 when thinning is
// on (else the last, whole bucket draws).
struct GpuPiece { lo: vec4<f32>, hi: vec4<f32>, levels: vec4<f32>, draw: vec4<u32>, thin: vec4<f32>, material: vec4<u32> }
const THIN_BUCKETS: u32 = 16u; // placed::THIN_BUCKETS

@group(0) @binding(0) var<uniform> cull: CullGlobals;
@group(0) @binding(1) var<storage, read> c_transforms: array<mat4x4<f32>>;
@group(0) @binding(2) var<storage, read> c_piece_of: array<u32>;
@group(0) @binding(3) var<storage, read> c_pieces: array<GpuPiece>;
@group(0) @binding(4) var<storage, read> c_bases: array<u32>;
// Five words per draw (placed::ARGS): index or vertex count, instance
// count, first index, base vertex, first instance. Only the instance count
// is written here.
@group(0) @binding(5) var<storage, read_write> c_args: array<atomic<u32>>;
@group(0) @binding(6) var<storage, read_write> c_visible: array<u32>;
// Farthest depth (reversed: smallest) over each texel's footprint.
@group(0) @binding(7) var c_hzb: texture_2d<f32>;
// One bit per placement: visible last frame.
@group(0) @binding(8) var<storage, read_write> c_bits: array<atomic<u32>>;
// Placements drawn per level, then their triangles: the view's profile.
@group(0) @binding(9) var<storage, read_write> c_totals: array<atomic<u32>>;
// Per row, the share of a thinning mesh's parts drawn (placed.wgsl).
@group(0) @binding(10) var<storage, read_write> c_keep: array<f32>;

// Whether the box lies wholly behind the depth pyramid. Boxes reaching
// behind the camera or off the target count as visible.
fn occluded(center: vec3<f32>, extent: vec3<f32>) -> bool {
    var lo = vec3<f32>(1e30);
    var hi = vec3<f32>(-1e30);
    for (var i = 0u; i < 8u; i++) {
        let corner = center + extent * vec3<f32>(f32(i & 1u) * 2.0 - 1.0, f32((i >> 1u) & 1u) * 2.0 - 1.0, f32(i >> 2u) * 2.0 - 1.0);
        let c = cull.clip * vec4<f32>(corner, 1.0);
        if c.w <= 1e-4 { return false; }
        let n = c.xyz / c.w;
        lo = min(lo, n);
        hi = max(hi, n);
    }
    // Clip to level-0 texels (half the target), y down.
    let size = cull.hzb.xy;
    let t0 = max(vec2<f32>(lo.x * 0.5 + 0.5, 0.5 - hi.y * 0.5) * size * 0.5, vec2<f32>(0.0));
    let t1 = min(vec2<f32>(hi.x * 0.5 + 0.5, 0.5 - lo.y * 0.5) * size * 0.5, cull.hzb.zw - 1.0);
    if any(t1 < t0) { return false; }
    // The level where the box spans at most two texels each way.
    let span = max(t1.x - t0.x, t1.y - t0.y);
    let level = u32(clamp(ceil(log2(max(span, 1.0))), 0.0, f32(cull.counts.w - 1u)));
    let last = vec2<i32>(textureDimensions(c_hzb, level)) - 1;
    let a = min(vec2<i32>(t0 / exp2(f32(level))), last);
    let b = min(vec2<i32>(t1 / exp2(f32(level))), last);
    let far = min(
        min(textureLoad(c_hzb, a, i32(level)).r, textureLoad(c_hzb, vec2<i32>(b.x, a.y), i32(level)).r),
        min(textureLoad(c_hzb, vec2<i32>(a.x, b.y), i32(level)).r, textureLoad(c_hzb, b, i32(level)).r),
    );
    // Reversed depth: the box's nearest point is its largest z.
    return hi.z < far;
}

@compute @workgroup_size(64)
fn cs_cull(@builtin(global_invocation_id) id: vec3<u32>) {
    let row = id.x;
    if row >= cull.counts.x { return; }
    let phase = cull.counts.z;
    let word = row >> 5u;
    let bit = 1u << (row & 31u);
    let was = (atomicLoad(&c_bits[word]) & bit) != 0u;
    if phase == 1u && !was { return; }
    let piece = c_pieces[c_piece_of[row]];
    if piece.draw.y == 0u { return; }
    let m = c_transforms[row];
    let c = (piece.lo.xyz + piece.hi.xyz) * 0.5;
    let e = (piece.hi.xyz - piece.lo.xyz) * 0.5;
    let center = (m * vec4<f32>(c, 1.0)).xyz;
    let extent = abs(m[0].xyz) * e.x + abs(m[1].xyz) * e.y + abs(m[2].xyz) * e.z;
    for (var i = 0; i < 6; i++) {
        let p = cull.planes[i];
        if dot(p.xyz, center) + p.w + dot(abs(p.xyz), extent) < 0.0 {
            if phase == 2u { atomicAnd(&c_bits[word], ~bit); }
            return;
        }
    }
    if phase == 2u {
        if occluded(center, extent) {
            atomicAnd(&c_bits[word], ~bit);
            return;
        }
        atomicOr(&c_bits[word], bit);
        // Phase 1 drew it already.
        if was { return; }
    }
    // w is linear in position; its minimum over the box is at one corner.
    let w = cull.rows[3];
    let nearest = dot(w.xyz, center) - dot(abs(w.xyz), extent) + w.w;
    // Pixels per metre of the model's own space: a rock placed at 3x has
    // triangles 3x as large on screen, so it keeps each level 3x as far.
    var scale = max(length(m[0].xyz), max(length(m[1].xyz), length(m[2].xyz)));
    if piece.lo.w > 0.5 { scale = 1.0; }
    var ppm = 1e30;
    if nearest > 1e-6 {
        ppm = length(cull.rows[1].xyz) * cull.info.x * 0.5 / nearest * scale;
    }
    var level = piece.draw.y - 1u;
    for (var k = 0u; k < piece.draw.y; k++) {
        if ppm >= piece.levels[k] {
            level = k;
            break;
        }
    }
    var d = piece.draw.x + level;
    if piece.draw.w != 0u && level + 1u == piece.draw.y {
        d = piece.draw.z;
    } else if piece.thin.z > 0.5 && piece.thin.w < 0.5 {
        d = piece.draw.x + level * THIN_BUCKETS + THIN_BUCKETS - 1u;
    } else if piece.thin.w > 0.5 {
        // Parts kept fall with the placement's area on screen, so its
        // triangles per pixel stay about the same.
        let least = max(piece.thin.y, 1e-3);
        let keep = clamp(pow(ppm / piece.thin.x, 2.0), least, 1.0);
        // Buckets end at least · (1 / least)^((b + 1) / buckets), as placed.rs
        // counts them.
        var bucket = THIN_BUCKETS - 1u;
        if least < 1.0 {
            let at = log(keep / least) / log(1.0 / least) * f32(THIN_BUCKETS);
            bucket = min(u32(max(ceil(at - 1e-4), 1.0)) - 1u, THIN_BUCKETS - 1u);
        }
        d = piece.draw.x + level * THIN_BUCKETS + bucket;
        c_keep[row] = keep;
    }
    let slot = atomicAdd(&c_args[d * 5u + 1u], 1u);
    if cull.info.z > 0.5 {
        atomicAdd(&c_totals[level], 1u);
        atomicAdd(&c_totals[4], atomicLoad(&c_args[d * 5u]) / 3u);
    }
    var entry = row;
    // The mesh's material, not the draw: thinning buckets and pieces
    // repeating a mesh share it, and draw indices took too many bits.
    if cull.counts.y > 0u { entry |= piece.material[level] << cull.counts.y; }
    c_visible[c_bases[d] + slot] = entry;
}
