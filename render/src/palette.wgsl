// Builds a splat's per-tile palettes (a7m.rs `Palette`). The control
// maps give each texel up to eight (layer, weight) slots, which a7m.wgsl's splat
// would have to merge over four texels per pixel. Here each tile of
// TILE² texels, plus BORDER texels all round, picks the eight layers
// with the most weight and writes their weights as eight channels of two
// RGBA8 atlases, and their layer ids, in stack order, to two id textures.
// The fragment shader then interpolates eight channels and loops over the
// tile's layers, with nothing to sort.
const TILE: u32 = 8u;
const BORDER: i32 = 2; // control::PALETTE_BORDER
const FIT: u32 = 12u; // TILE + 2 BORDER: the texels that pick the layers
// control::PALETTE_SIDE: the texels kept, from one before the tile to two
// after, what fs_ground's bicubic reads.
const SIDE: u32 = 11u;
const NONE: u32 = 255u;

struct PaletteGlobals {
    // First tile of the dispatch, control pairs (1 or 2), layer count.
    first: vec2<u32>,
    pairs: u32,
    layers: u32,
    // The control textures hold the map's texels from `window` on (the
    // texels these tiles read); the map's size.
    window: vec2<u32>,
    size: vec2<u32>,
}

@group(0) @binding(0) var<uniform> g: PaletteGlobals;
@group(0) @binding(1) var t_index0: texture_2d<f32>;
@group(0) @binding(2) var t_index1: texture_2d<f32>;
@group(0) @binding(3) var t_weight0: texture_2d<f32>;
@group(0) @binding(4) var t_weight1: texture_2d<f32>;
@group(0) @binding(5) var o_weights0: texture_storage_2d<rgba8unorm, write>;
@group(0) @binding(6) var o_weights1: texture_storage_2d<rgba8unorm, write>;
@group(0) @binding(7) var o_ids0: texture_storage_2d<rgba8unorm, write>;
@group(0) @binding(8) var o_ids1: texture_storage_2d<rgba8unorm, write>;

var<workgroup> totals: array<atomic<u32>, 256>;
var<workgroup> chosen: array<u32, 8>;

// Slot k (0..8) of a control texel: its layer and weight; weight 0 when
// empty or past the layer count.
fn slot(coord: vec2<i32>, k: u32) -> vec2<f32> {
    var layer = 0.0;
    var weight = 0.0;
    if k < 4u {
        layer = textureLoad(t_index0, coord, 0)[k];
        weight = textureLoad(t_weight0, coord, 0)[k];
    } else if g.pairs > 1u {
        layer = textureLoad(t_index1, coord, 0)[k - 4u];
        weight = textureLoad(t_weight1, coord, 0)[k - 4u];
    }
    let id = round(layer * 255.0);
    if id >= f32(g.layers) { weight = 0.0; }
    return vec2<f32>(id, weight);
}

// Control texel `i` of a `side`² region starting `before` texels before
// the tile, clamped to the map, in the control textures' window.
fn texel(tile: vec2<u32>, i: u32, side: u32, before: i32) -> vec2<i32> {
    let last = vec2<i32>(g.size) - 1;
    let at = vec2<i32>(tile * TILE) - before + vec2<i32>(i32(i % side), i32(i / side));
    return clamp(at, vec2<i32>(0), last) - vec2<i32>(g.window);
}

@compute @workgroup_size(64)
fn cs_palette(@builtin(workgroup_id) group: vec3<u32>, @builtin(local_invocation_index) lane: u32) {
    let tile = g.first + group.xy;
    for (var l = lane; l < 256u; l += 64u) { atomicStore(&totals[l], 0u); }
    workgroupBarrier();
    for (var i = lane; i < FIT * FIT; i += 64u) {
        let c = texel(tile, i, FIT, BORDER);
        for (var k = 0u; k < 8u; k++) {
            let s = slot(c, k);
            if s.y > 0.0 { atomicAdd(&totals[u32(s.x)], u32(round(s.y * 255.0))); }
        }
    }
    workgroupBarrier();
    if lane == 0u {
        // The eight heaviest, then in layer order (the stack's).
        var picked = array<u32, 8>(NONE, NONE, NONE, NONE, NONE, NONE, NONE, NONE);
        for (var k = 0u; k < 8u; k++) {
            var best = NONE;
            var most = 0u;
            for (var l = 0u; l < g.layers; l++) {
                let t = atomicLoad(&totals[l]);
                if t > most { most = t; best = l; }
            }
            if best == NONE { break; }
            picked[k] = best;
            atomicStore(&totals[best], 0u);
        }
        for (var a = 1u; a < 8u; a++) {
            var b = a;
            while b > 0u && picked[b - 1u] > picked[b] {
                let t = picked[b];
                picked[b] = picked[b - 1u];
                picked[b - 1u] = t;
                b--;
            }
        }
        for (var k = 0u; k < 8u; k++) { chosen[k] = picked[k]; }
        let ids = vec2<i32>(tile);
        textureStore(o_ids0, ids, vec4<f32>(f32(picked[0]), f32(picked[1]), f32(picked[2]), f32(picked[3])) / 255.0);
        textureStore(o_ids1, ids, vec4<f32>(f32(picked[4]), f32(picked[5]), f32(picked[6]), f32(picked[7])) / 255.0);
    }
    workgroupBarrier();
    let ids = array<u32, 8>(chosen[0], chosen[1], chosen[2], chosen[3], chosen[4], chosen[5], chosen[6], chosen[7]);
    for (var i = lane; i < SIDE * SIDE; i += 64u) {
        let c = texel(tile, i, SIDE, 1);
        var w = array<f32, 8>(0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0);
        for (var k = 0u; k < 8u; k++) {
            let s = slot(c, k);
            if s.y <= 0.0 { continue; }
            for (var j = 0u; j < 8u; j++) {
                if ids[j] == u32(s.x) { w[j] = min(w[j] + s.y, 1.0); }
            }
        }
        let out = vec2<i32>(tile * SIDE) + vec2<i32>(i32(i % SIDE), i32(i / SIDE));
        textureStore(o_weights0, out, vec4<f32>(w[0], w[1], w[2], w[3]));
        textureStore(o_weights1, out, vec4<f32>(w[4], w[5], w[6], w[7]));
    }
}
