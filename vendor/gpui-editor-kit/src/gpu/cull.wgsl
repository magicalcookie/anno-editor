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
//
// Decals blend, so the order their list holds them in shows. Atomics
// hand out slots in whatever order the threads run, so decals are listed
// by one phase only, every one in the frustum, and cs_order_decals then
// sorts each decal draw's list by row. That phase is the first (or the one
// pass without occlusion): decals on the ground draw between the ground
// and the models (gpu::Decal::ground), before any depth to test them
// against exists. They hide nothing, so the pyramid never needs them.
//
// Culling further, per cluster or triangle in compute (Wihlidal's
// approach without back faces, which two-sided meshes keep), was measured
// and loses: on an RTX 4080 the rasterizer drops off-screen, zero-area and
// between-pixel-centre triangles almost for free. Removing every one of
// them saved 0.18 ms of Albion's 1.6 ms placed passes at 5K, under the
// 0.13 ms launching such a pass costs alone (branch gpu-cull-clusters).
struct CullGlobals {
    // Clip rows (x, y, z, w) of the view or the shadow map: levels follow
    // how large a metre appears in it.
    rows: array<vec4<f32>, 4>,
    // Planes to cull against, inside where dot(xyz, p) + w >= 0.
    planes: array<vec4<f32>, 6>,
    // x: height in pixels of what the rows project onto. y: the least
    // pixels a placement's widest extent must span to be drawn, 0 for all
    // (gpu::Detail::min_pixels). z: 1 to count what is drawn into c_totals.
    // w: target pixels per pixel of x, where levels and least sizes count
    // logical pixels but textures are sampled per physical one.
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
    // x: 0, unused. y: 1 + the section of c_wants this cull answers in, 0
    // for none (residency.rs). z, w: the decal draws, first and end; equal
    // for shadow culls, which list decals as any.
    lit: vec4<u32>,
    // The view's eye (w 1), which `reach` measures from; w 0 reaches
    // anywhere. Shadow cascades get their view's.
    eye: vec4<f32>,
    // Placements with an impostor (gpu::Reach): from x on the impostor
    // draws, past y nothing, and over the last z share of y they drop out
    // one by one.
    reach: vec4<f32>,
    // The presented ground's cell ranges (c_ground): origin x and z, 1 over
    // a cell's side in metres, w 1 when bound.
    ground: vec4<f32>,
    // Their finest level's cells across x and z, and their level count.
    cells: vec4<u32>,
    // x, y: the posed draws, first and end, whose placements mark the
    // screen tiles they may cover in c_posed (visbuf.rs POSED_TILE); z: tiles
    // across the target, 0 for culls that mark none.
    posed: vec4<u32>,
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
// Per draw slot (piece.draw counts in them): the indirect draw whose list
// takes its placements, and where that list starts in c_visible. Without
// the arena each slot is its own draw; with it, slots sharing indices
// share one, as the vertex stage tells draws apart by their index values
// alone (placed::Placed::lists). NO_LIST: a thinning bucket without
// triangles of its own, which lists nothing.
@group(0) @binding(4) var<storage, read> c_lists: array<vec2<u32>>;
const NO_LIST: u32 = 0xffffffffu; // placed::NO_LIST
// Five words per indirect draw (placed::ARGS): index or vertex count, instance
// count, first index, base vertex, first instance; the last two zero.
// Only the instance count is written here.
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
// Per arena material: its texture slots (albedo, normal, surface, alpha;
// 0 none), per slot log2 of the texels a unit of the mesh's own space
// spans at least, and the albedo's slot again when shadow maps cut out by
// it, and how it bends to the terrain (placed::GpuTexels).
// extra: a family's slots, and their biases in extra_bias;
// cutout.z the ones its coverage samples.
struct Texels { slots: vec4<u32>, bias: vec4<f32>, cutout: vec4<u32>, extra: array<vec4<u32>, 2>, extra_bias: array<vec4<f32>, 2> }
@group(0) @binding(13) var<storage, read> c_texels: array<Texels>;
// Per section (view, shadow map, cascades) and texture slot: 32 less the
// finest level a placement this cull passed may sample, 0 for none.
@group(0) @binding(14) var<storage, read_write> c_wants: array<atomic<u32>>;
const SLOTS: u32 = 2048u; // residency::SLOTS
const TEXEL_MARGIN: f32 = 0.75; // placed::TEXEL_MARGIN
// Lowest and highest ground per cell of the presented heights, each level
// after the finest over two by two of the one before (heights.rs cells).
@group(0) @binding(15) var<storage, read> c_ground: array<vec2<f32>>;
// A bit per POSED_TILE square of the target that a listed posed placement's
// box reaches, for the posed resolve (visbuf.wgsl cs_resolve_posed), from
// word 4 on; word 0 not 0: everywhere (a box reaching behind the eye, or
// a target wider or taller than the words hold, 8192 pixels).
@group(0) @binding(16) var<storage, read_write> c_posed: array<atomic<u32>>;
const POSED_TILE: f32 = 64.0; // visbuf.rs POSED_TILE

fn mark_posed(center: vec3<f32>, extent: vec3<f32>) {
    var lo = vec2<f32>(1e30);
    var hi = vec2<f32>(-1e30);
    for (var i = 0u; i < 8u; i++) {
        let corner = center + extent * vec3<f32>(f32(i & 1u) * 2.0 - 1.0, f32((i >> 1u) & 1u) * 2.0 - 1.0, f32(i >> 2u) * 2.0 - 1.0);
        let c = cull.clip * vec4<f32>(corner, 1.0);
        if c.w <= 1e-4 {
            atomicOr(&c_posed[0], 1u);
            return;
        }
        lo = min(lo, c.xy / c.w);
        hi = max(hi, c.xy / c.w);
    }
    // To target pixels, y down, a pixel out each way, then tiles.
    let size = cull.hzb.xy;
    let p0 = max(vec2<f32>(lo.x * 0.5 + 0.5, 0.5 - hi.y * 0.5) * size - 1.0, vec2<f32>(0.0));
    let p1 = min(vec2<f32>(hi.x * 0.5 + 0.5, 0.5 - lo.y * 0.5) * size + 1.0, size - 1.0);
    if any(p1 < p0) { return; }
    let t0 = vec2<u32>(p0 / POSED_TILE);
    let t1 = vec2<u32>(p1 / POSED_TILE);
    let across = cull.posed.z;
    let words = arrayLength(&c_posed);
    if 4u + (t1.y * across + t1.x) / 32u >= words {
        atomicOr(&c_posed[0], 1u);
        return;
    }
    for (var y = t0.y; y <= t1.y; y++) {
        for (var x = t0.x; x <= t1.x; x++) {
            let t = y * across + x;
            atomicOr(&c_posed[4u + t / 32u], 1u << (t & 31u));
        }
    }
}

// The lowest and highest ground over world x/z `lo..hi`, clamped to the
// heights as gpu.wgsl ground_height clamps: four cells of the level where
// the box spans at most two each way.
fn ground_range(lo: vec2<f32>, hi: vec2<f32>) -> vec2<f32> {
    let last = vec2<f32>(cull.cells.xy - 1u);
    let a0 = vec2<u32>(clamp(floor((lo - cull.ground.xy) * cull.ground.z), vec2<f32>(0.0), last));
    let b0 = vec2<u32>(clamp(floor((hi - cull.ground.xy) * cull.ground.z), vec2<f32>(0.0), last));
    let span = max(b0.x - a0.x, b0.y - a0.y);
    // ceil(log2(span)): a span of up to 2^level cells meets at most two of
    // that level's.
    let level = min(32u - countLeadingZeros(max(span, 1u) - 1u), cull.cells.z - 1u);
    var first = 0u;
    var size = cull.cells.xy;
    for (var l = 0u; l < level; l++) {
        first += size.x * size.y;
        size = (size - 1u) / 2u + 1u;
    }
    let a = a0 >> vec2<u32>(level);
    let b = b0 >> vec2<u32>(level);
    let r0 = c_ground[first + a.y * size.x + a.x];
    let r1 = c_ground[first + a.y * size.x + b.x];
    let r2 = c_ground[first + b.y * size.x + a.x];
    let r3 = c_ground[first + b.y * size.x + b.x];
    return vec2<f32>(min(min(r0.x, r1.x), min(r2.x, r3.x)), max(max(r0.y, r1.y), max(r2.y, r3.y)));
}

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
    let piece = c_pieces[c_piece_of[row]];
    if piece.draw.y == 0u { return; }
    // A decal piece's levels are all decal draws (placed::Ranges).
    let decal = piece.draw.x >= cull.lit.z && piece.draw.x < cull.lit.w;
    if (phase == 1u && !was && !decal) || (phase == 2u && decal) { return; }
    let m = c_transforms[row];
    let c = (piece.lo.xyz + piece.hi.xyz) * 0.5;
    let e = (piece.hi.xyz - piece.lo.xyz) * 0.5;
    let center = (m * vec4<f32>(c, 1.0)).xyz;
    var extent = abs(m[0].xyz) * e.x + abs(m[1].xyz) * e.y + abs(m[2].xyz) * e.z;
    // Too small on screen to matter: its widest extent at its nearest
    // point, before the terrain allowance below widens it. Dropped like a
    // placement off screen, so phase 2 clears its bit.
    if cull.info.y > 0.0 {
        let w = cull.rows[3];
        let nearest = dot(w.xyz, center) - dot(abs(w.xyz), extent) + w.w;
        let wide = 2.0 * max(extent.x, max(extent.y, extent.z));
        if nearest > 1e-6 && wide * length(cull.rows[1].xyz) * cull.info.x * 0.5 / nearest < cull.info.y {
            if phase == 2u { atomicAnd(&c_bits[word], ~bit); }
            return;
        }
    }
    // Foliage out of reach, as a placement off screen. Each drops at its
    // own distance within the band, from a hash of where it stands, so it
    // stays put as the eye moves; pieces of one plant share its origin.
    let reaching = cull.eye.w > 0.5 && piece.draw.w != 0u;
    var far = false;
    if reaching {
        let d = length(max(abs(center - cull.eye.xyz) - extent, vec3<f32>(0.0)));
        if d > cull.reach.y * (1.0 - cull.reach.z * spot(m[3].xyz)) {
            if phase == 2u { atomicAnd(&c_bits[word], ~bit); }
            return;
        }
        far = d > cull.reach.x;
    }
    // Placements that may bend to the terrain (gpu::adapting) move with
    // the ground under them, as gpu.wgsl adapt_to_terrain moves them, so
    // they cull by the box that holds them wherever it lies. With
    // `ground` the piece's origin y and d the ground's range less it over
    // the footprint and the row's origin: riding the ground (`follow`)
    // moves the whole box by d; relative vertices move by up to d again,
    // at most toward it; absolute ones lie within the ground's range.
    var bound = extent;
    var middle = center;
    if m[0].w > 0.5 {
        if cull.ground.w > 0.5 {
            let g = ground_range(min(center.xz - extent.xz, m[3].xz), max(center.xz + extent.xz, m[3].xz));
            let d = g - (m[3].y - m[1].w);
            let follow = select(vec2<f32>(0.0), d, m[2].w > 0.5);
            // 0.02: absolute vertices' lift off the ground. Never less
            // than the old 4 m pad: it also covered what the box misses,
            // such as thinning parts grown past it, so nothing it drew
            // drops out. 1800's tropical cliffs hold rows bending past
            // 8 m, which it culled.
            let lo = min(min(center.y - extent.y + follow.x + min(d.x, 0.0), g.x), center.y - extent.y - 4.0);
            let hi = max(max(center.y + extent.y + follow.y + max(d.y, 0.0), g.y + 0.02), center.y + extent.y + 4.0);
            middle.y = (lo + hi) * 0.5;
            bound.y = (hi - lo) * 0.5;
        } else {
            // No heights to read yet: 4 m covers a 16 m footprint on the
            // game's 24% steepest buildable slope.
            bound.y += 4.0;
        }
        // Levels and texel wants keep measuring from the box padded by
        // those 4 m, as they were tuned: a sculpt should not change what
        // level a placement draws at.
        extent.y += 4.0;
    }
    for (var i = 0; i < 6; i++) {
        let p = cull.planes[i];
        if dot(p.xyz, middle) + p.w + dot(abs(p.xyz), bound) < 0.0 {
            if phase == 2u { atomicAnd(&c_bits[word], ~bit); }
            return;
        }
    }
    // w is linear in position; its minimum over the box is at one corner.
    let w = cull.rows[3];
    let nearest = dot(w.xyz, center) - dot(abs(w.xyz), extent) + w.w;
    // Pixels per metre of the model's own space: a rock placed at 3x has
    // triangles 3x as large on screen, so it keeps each level 3x as far.
    let largest = max(length(m[0].xyz), max(length(m[1].xyz), length(m[2].xyz)));
    var scale = largest;
    if piece.lo.w > 0.5 { scale = 1.0; }
    var ppm = 1e30;
    if nearest > 1e-6 {
        ppm = length(cull.rows[1].xyz) * cull.info.x * 0.5 / nearest * scale;
    }
    var level = piece.draw.y - 1u;
    for (var k = 0u; k < piece.draw.y; k++) {
        if ppm >= piece.levels[k] && !far {
            level = k;
            break;
        }
    }
    let impostor = piece.draw.w != 0u && level + 1u == piece.draw.y;
    // What this placement's textures may sample, wanted whether or not the
    // pyramid hides it: a hidden one shows as the view turns or something
    // in front moves, and a want that came and went with occlusion would
    // keep levels streaming in and out. Impostors sample their atlas.
    // Decals ask in the one phase that sees them.
    if cull.lit.y != 0u && (phase != 1u || decal) && !impostor {
        // Pixels per unit of the mesh's own space along its largest scale,
        // times what thinning grows the parts it keeps by (placed_at).
        var lp = 100.0;
        // Either axis of the target: a shadow fit's texels need not be
        // square in the world.
        let across = max(length(cull.rows[0].xyz), length(cull.rows[1].xyz));
        if nearest > 1e-6 {
            lp = log2(across * cull.info.x * cull.info.w * 0.5 / nearest * largest);
        }
        if piece.thin.w > 0.5 {
            let keep = clamp(pow(ppm / piece.thin.x, 2.0), max(piece.thin.y, 1e-3), 1.0);
            lp += log2(min(sqrt(1.0 / max(keep, 1e-3)), 2.5));
        }
        want_texels(piece.material[level], lp, m[0].w > 0.5);
    }
    if phase == 2u {
        if occluded(middle, bound) {
            atomicAnd(&c_bits[word], ~bit);
            return;
        }
        atomicOr(&c_bits[word], bit);
        // Phase 1 drew it already.
        if was { return; }
    }
    var d = piece.draw.x + level;
    // Draws d - below..=d list the placement: with the arena a thinning
    // bucket's draw has its own triangles alone, past the bucket before's
    // (placed::Arena::pack), so it goes in every bucket up to its own.
    var below = 0u;
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
        below = bucket;
        c_keep[row] = keep;
    }
    if d >= cull.posed.x && d < cull.posed.y && cull.posed.z != 0u {
        mark_posed(middle, bound);
    }
    let first = d - below;
    var indices = 0u;
    for (var k = first; k <= d; k++) {
        let list = c_lists[k].x;
        if list != NO_LIST { indices += atomicLoad(&c_args[list * 5u]); }
    }
    if cull.info.z > 0.5 {
        atomicAdd(&c_totals[level], 1u);
        atomicAdd(&c_totals[4], indices / 3u);
    }
    var entry = row;
    // The mesh's material, not the draw: thinning buckets and pieces
    // repeating a mesh share it, and draw indices took too many bits.
    if cull.counts.y > 0u { entry |= piece.material[level] << cull.counts.y; }
    for (var k = first; k <= d; k++) {
        let list = c_lists[k];
        if list.x == NO_LIST { continue; }
        let slot = atomicAdd(&c_args[list.x * 5u + 1u], 1u);
        c_visible[list.y + slot] = entry;
    }
}

// Sorts each decal draw's visible rows ascending, one workgroup a draw:
// they then blend in row order, whatever order the cull's threads ran in.
// Entries hold the row in their low bits and the draw's one material
// above, so they sort by row. A bitonic network in which every comparator
// puts the smaller at the lower index, over the list padded to a power of
// two: the padding counts as larger than any entry and never moves, so
// comparators reaching past the list are skipped. In place in the list,
// which stays in cache: a few hundred decals a draw is the usual load.
var<workgroup> o_count: u32;

@compute @workgroup_size(256)
fn cs_order_decals(@builtin(workgroup_id) group: vec3<u32>, @builtin(local_invocation_index) t: u32) {
    // Decal slots never share a list (placed::Arena::pack).
    let list = c_lists[cull.lit.z + group.x];
    if t == 0u && list.x != NO_LIST { o_count = atomicLoad(&c_args[list.x * 5u + 1u]); }
    let n = workgroupUniformLoad(&o_count);
    if list.x == NO_LIST || n < 2u { return; }
    let base = list.y;
    var size = 2u;
    while size < n { size *= 2u; }
    for (var k = 2u; k <= size; k *= 2u) {
        for (var j = k / 2u; j > 0u; j /= 2u) {
            for (var p = t; p < size / 2u; p += 256u) {
                var lo: u32;
                var hi: u32;
                if j == k / 2u {
                    // The first step of a merge compares mirrored pairs,
                    // which sorts the two halves' runs into one.
                    lo = p / j * k + p % j;
                    hi = p / j * k + k - 1u - p % j;
                } else {
                    lo = p / j * 2u * j + p % j;
                    hi = lo + j;
                }
                if hi < n {
                    let a = c_visible[base + lo];
                    let b = c_visible[base + hi];
                    if a > b {
                        c_visible[base + lo] = b;
                        c_visible[base + hi] = a;
                    }
                }
            }
            storageBarrier();
        }
    }
}

// A stable share in [0, 1) for the placement standing at `p`.
fn spot(p: vec3<f32>) -> f32 {
    var h = bitcast<u32>(p.x) * 0x9e3779b1u ^ bitcast<u32>(p.z) * 0x85ebca77u ^ bitcast<u32>(p.y);
    h = (h ^ (h >> 16u)) * 0x7feb352du;
    h = (h ^ (h >> 15u)) * 0x846ca68bu;
    h = h ^ (h >> 16u);
    return f32(h >> 8u) / 16777216.0;
}

// Asks for the finest level each texture of `material` could sample at
// `lp`, log2 of the pixels a unit of the mesh's own space spans at most: a
// pixel then spans at least exp2(bias - lp) texels where the sampler picks
// its level. A mesh bent to the ground (`bends`: the placement allows it)
// stretches by the ground's slope, which UVs cannot bound: every level.
fn want_texels(material: u32, lp: f32, bends: bool) {
    let t = c_texels[material];
    let at = (cull.lit.y - 1u) * SLOTS;
    let bent = t.cutout.y == 2u || (t.cutout.y == 1u && bends);
    let pixels = select(lp, 100.0, bent);
    // Shadow maps sample a caster's albedo for its cutout, nothing else.
    let shadow = cull.info.z < 0.5;
    var slots = t.slots;
    // Shadow maps ask for the family slots its coverage samples, too.
    var extras = 0xffu;
    if shadow {
        slots = vec4<u32>(t.cutout.x, 0u, 0u, 0u);
        extras = t.cutout.z;
    }
    for (var k = 0u; k < 12u; k++) {
        var slot = 0u;
        var bias = 0.0;
        if k < 4u {
            slot = slots[k];
            bias = t.bias[k];
        } else {
            let e = k - 4u;
            slot = select(0u, t.extra[e / 4u][e & 3u], ((extras >> e) & 1u) != 0u);
            bias = t.extra_bias[e / 4u][e & 3u];
        }
        if slot == 0u { continue; }
        let want = 32u - u32(clamp(floor(bias - pixels - TEXEL_MARGIN), 0.0, 31.0));
        // Most placements share a few textures: read before the atomic.
        if atomicLoad(&c_wants[at + slot]) < want {
            atomicMax(&c_wants[at + slot], want);
        }
    }
}
