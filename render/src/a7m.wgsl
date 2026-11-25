// The a7m splat as a ground (a7m.rs): group 3 is the ground's own.
//
// The splat's own uniform (a7m.rs `Params`): the grit as 1 / tiling,
// sharpness, slope and 1 when on; the control maps' texels; the terrain
// grid's origin; 1 with gloss layers, 1 for a plain bake (Bake::Plain);
// and 1 / the grit mask's side in metres. Whether it has normal layers is
// the program's (`ground_normals`).
struct A7mParams {
    grit: vec4<f32>,
    control: vec2<f32>,
    origin: vec2<f32>,
    gloss: f32,
    plain: f32,
    grit_mask: f32,
}
@group(3) @binding(0) var<uniform> a7m: A7mParams;
// Splat layer: reciprocal tiling in world units, blend sharpness (0: none).
// Relief (Relief): floor and ceil in world units, add 1 when the
// layer adds its height to those below.
struct GpuLayer { scale: f32, sharpness: f32, floor: f32, ceil: f32, add: f32, pad: f32 }

@group(3) @binding(1) var<storage, read> b_layers: array<GpuLayer>;
@group(3) @binding(2) var t_layers: texture_2d_array<f32>;
// Splat palettes (palette.wgsl): eight layer weights per control texel in
// per-tile regions of two atlases, and each tile's eight layer ids.
@group(3) @binding(3) var t_palette_weights0: texture_2d<f32>;
@group(3) @binding(4) var t_palette_weights1: texture_2d<f32>;
@group(3) @binding(5) var t_palette_ids0: texture_2d<f32>;
@group(3) @binding(6) var t_palette_ids1: texture_2d<f32>;
// Splat normal maps, one per layer, when ground_normals() is true.
@group(3) @binding(7) var t_layer_normals: texture_2d_array<f32>;
// Splat glossiness maps in red, one per layer, when a7m.gloss is 1.
@group(3) @binding(8) var t_layer_gloss: texture_2d_array<f32>;
// The splat's grit (Grit) when a7m.grit.w is 1: its texture, and
// its mask in red over the terrain from the grid's origin.
@group(3) @binding(9) var t_grit: texture_2d<f32>;
@group(3) @binding(10) var t_grit_mask: texture_2d<f32>;
// The splat's relief height maps (Relief) when relief() is true.
@group(3) @binding(11) var t_layer_heights: texture_2d_array<f32>;
// Nearest, for gathering the palette weights.
@group(3) @binding(12) var s_gather: sampler;
// Layers stack bottom to top in layer index order. Each layer's weight is
// interpolated over the four nearest control texels first and only then
// cut by its image's alpha: alpha = saturate(s (a - 1) + (s + 1) w),
// Anno's BlendSplat, or plain w without sharpness, then smoothed
// (BlendSplatSmooth). Cutting before interpolating left texel-sized
// steps. Under the lowest layer lies the bake's grey base (SPLAT_BASE).
// Weights come from the pixel's palette tile, whose eight channels are the
// same layers on every texel it covers, so they interpolate channel by
// channel. Layers sample with explicit gradients (world xz per pixel):
// this runs in non-uniform control flow, where implicit derivatives are
// undefined, and mips still apply.
//
// Colours, coverage, blend factors, normals and gloss are `half` (16 bits
// where the device has them, kit pass::half_types): they lie in 0..1 or are
// unit vectors. The splat held about 145 registers a thread, which kept the
// opaque pass at a tenth of the warps an RTX 4080 SUPER can hold (Nsight GPU
// Trace, continental_overview at 5K). Coordinates, gradients, relief heights
// (they move geometry) and the projections' |n|^16 (it underflows f16) stay
// f32.
const PALETTE_TILE: f32 = 8.0; // control::PALETTE_TILE
// A tile's region starts a texel before it (control::PALETTE_SIDE).
const PALETTE_BEFORE: i32 = 1;
const PALETTE_SIDE: i32 = 11; // control::PALETTE_SIDE

// GetBicubicWeight1D_Hermite (functions.fxh): the cubic convolution kernel
// with a = -0.5.
fn hermite(x: f32) -> f32 {
    let a = -0.5;
    let x2 = x * x;
    let x3 = x2 * x;
    if x <= 1.0 { return (a + 2.0) * x3 - (a + 3.0) * x2 + 1.0; }
    if x < 2.0 { return a * x3 - 5.0 * a * x2 + 8.0 * a * x - 4.0 * a; }
    return 0.0;
}
const NO_LAYER: u32 = 255u;

// A smooth bake lays ground over this grey before its first layer (linear
// 0.2), so where coverage thins it shows through. Stored as sRGB, decoded
// with the layers.
const SPLAT_BASE: vec3<f32> = vec3<f32>(0.4845);

// The base in the splat's blending space: the smooth bake's grey decoded;
// a plain bake clears to white, blended as stored like its layers.
fn splat_base() -> vec3<f32> {
    if plain_bake() { return vec3<f32>(1.0); }
    return decode(SPLAT_BASE);
}

// Layers tile in world XZ on gentle ground. Steeper faces also project
// along X (ZY plane) and Z (XY plane), as Anno's bake does, so cliffs show
// the texture instead of streaks; flat ground samples once.
fn triplanar(n: vec3<f32>) -> vec3<f32> {
    var w = pow(abs(n), vec3<f32>(6.0));
    w = w / (w.x + w.y + w.z);
    // Drop slivers so gentle ground pays for one projection.
    w = select(vec3<f32>(0.0), w, w > vec3<f32>(0.02));
    return w / (w.x + w.y + w.z);
}

// The coordinates, screen gradients and tangent frame of projection p for
// world point `world`: 0 the ZY plane, 1 XZ, 2 XY.
struct Projection {
    at: vec2<f32>,
    dx: vec2<f32>,
    dy: vec2<f32>,
    t: vec3<f32>,
    b: vec3<f32>,
}

// Anno's orientation (terrain_baking.fx SampleTextures): world over the
// repetition with its height negated, then ZY, XZ as (x, -z), and XY, so
// each projection's V runs against up or +Z. The bitangent follows V.
fn projection(p: u32, world: vec3<f32>, dw: array<vec3<f32>, 2>) -> Projection {
    let flip = vec2<f32>(1.0, -1.0);
    if p == 0u {
        return Projection(world.zy * flip, dw[0].zy * flip, dw[1].zy * flip, vec3<f32>(0.0, 0.0, 1.0), vec3<f32>(0.0, -1.0, 0.0));
    }
    if p == 2u {
        return Projection(world.xy * flip, dw[0].xy * flip, dw[1].xy * flip, vec3<f32>(1.0, 0.0, 0.0), vec3<f32>(0.0, -1.0, 0.0));
    }
    return Projection(world.xz * flip, dw[0].xz * flip, dw[1].xz * flip, vec3<f32>(1.0, 0.0, 0.0), vec3<f32>(0.0, 0.0, -1.0));
}

// Whether the splat blends as Bake::Plain does.
fn plain_bake() -> bool { return a7m.plain > 0.5; }

// One layer's color blended over the projections `w` allows, and what
// each projection weighs in it (layer_detail's). Within those, each
// projection weighs as Anno's GetTriPlanarWeights: |n| to the power 16 ×
// its texel's alpha (smooth bake: at least 0.1, less 0.001), so on 45°
// slopes the higher texel wins instead of both smearing.
struct LayerColor {
    color: half4,
    weights: half3,
}

fn layer_color(layer: u32, scale: f32, world: vec3<f32>, dw: array<vec3<f32>, 2>, n: vec3<f32>, w: vec3<f32>) -> LayerColor {
    var texels: array<half4, 3>;
    var height = vec3<f32>(0.0);
    for (var p = 0u; p < 3u; p++) {
        if w[p] <= 0.0 { continue; }
        let q = projection(p, world, dw);
        let t = textureSampleGrad(t_layers, s_albedo, q.at * scale, layer, q.dx * scale, q.dy * scale);
        if plain_bake() {
            height[p] = pow(abs(n[p]), 16.0 * t.a);
        } else {
            height[p] = max(pow(abs(n[p]), 16.0 * max(t.a, 0.1)) - 0.001, 0.0);
        }
        // A smooth bake blends decoded, as sampling through sRGB views
        // would; a plain one blends as stored and splat() decodes the
        // result.
        // TODO: filtering still runs on the stored values; an sRGB view of
        // the layer array would filter decoded too.
        var rgb = t.rgb;
        if !plain_bake() { rgb = decode(rgb); }
        texels[p] = half4(half3(rgb), half(t.a));
    }
    var weights = w;
    let sum = height.x + height.y + height.z;
    if sum > 0.0 { weights = height / sum; }
    var out = LayerColor(half4(0.0), half3(weights));
    for (var p = 0u; p < 3u; p++) {
        if out.weights[p] <= 0.0 { continue; }
        out.color += out.weights[p] * texels[p];
    }
    return out;
}

// One layer's bent world normal, gloss and height, blended with the
// projection weights layer_color found. Only asked where the layer shows:
// these are most of a live pixel's samples, and a layer its cut hides
// (alpha 0, common along sharp layer borders) would add nothing.
struct LayerDetail {
    normal: half3,
    gloss: half,
    height: f32,
}

fn layer_detail(layer: u32, scale: f32, world: vec3<f32>, dw: array<vec3<f32>, 2>, n: vec3<f32>, weights: half3, normals: bool, gloss: bool, heights: bool) -> LayerDetail {
    var out = LayerDetail(half3(0.0), half(0.0), 0.0);
    let nh = half3(n);
    for (var p = 0u; p < 3u; p++) {
        if weights[p] <= 0.0 { continue; }
        let q = projection(p, world, dw);
        if normals {
            let ts = half3(textureSampleGrad(t_layer_normals, s_albedo, q.at * scale, layer, q.dx * scale, q.dy * scale).xyz) * 2.0 - 1.0;
            let qt = half3(q.t);
            let qb = half3(q.b);
            let tt = normalize(qt - nh * dot(nh, qt));
            let bb = normalize(qb - nh * dot(nh, qb) - tt * dot(tt, qb));
            out.normal += weights[p] * (tt * ts.x + bb * ts.y + nh * max(ts.z, 0.05));
        }
        if gloss {
            out.gloss += weights[p] * half(textureSampleGrad(t_layer_gloss, s_albedo, q.at * scale, layer, q.dx * scale, q.dy * scale).r);
        }
        if heights {
            // Two pixels' worth: the slope comes from neighbouring pixels,
            // and heights sharper than that only make it flicker.
            let g = RELIEF_FOOTPRINT * scale;
            out.height += f32(weights[p]) * textureSampleGrad(t_layer_heights, s_albedo, q.at * scale, layer, q.dx * g, q.dy * g).r;
        }
    }
    return out;
}

// The footprint relief heights are filtered over, in pixels: the game
// shades from a filtered normal map; slopes from heights sharper than the
// pixels they span came out as dark grain.
// Whether the splat draws its relief (Relief::on, with heights).


// Metres a ground pixel covers past which the splat drops its layers'
// normal maps and samples each layer from one projection only: their relief
// is below a pixel there. 0.1 ms of a 5K overview (2.28 to 2.18).
const FAR_GROUND: f32 = 0.12;

// Where a smooth bake's pixel reads its tile's layer weights: atlas texel
// `at`, and the bicubic weights of the texels from `at` - 1 along each
// axis, the first three of four (each axis's sum is 1).
struct Cover {
    at: vec2<f32>,
    wx: vec3<f32>,
    wy: vec3<f32>,
}

// Layer `k`'s weight at the pixel under GetMaterialCoverageBicubic. Its
// channel alone is gathered, a 2x2 block at a time, and only for the
// layers the compositing reaches: loading all 16 texels' eight channels up
// front was most of what the coverage cost, and the compositing often
// stops after one or two layers.
fn layer_cover(c: Cover, k: u32) -> f32 {
    let texel = 1.0 / vec2<f32>(textureDimensions(t_palette_weights0));
    let wx = vec4<f32>(c.wx, 1.0 - c.wx.x - c.wx.y - c.wx.z);
    let wy = vec4<f32>(c.wy, 1.0 - c.wy.x - c.wy.y - c.wy.z);
    // Blocks from `at` - 1 and `at` + 1 on each axis, each gathered at the
    // corner its texels share, in gather order: (x, y + 1), (x + 1, y + 1),
    // (x + 1, y), (x, y).
    let g0 = gather(k, c.at * texel);
    let g1 = gather(k, (c.at + vec2<f32>(2.0, 0.0)) * texel);
    let g2 = gather(k, (c.at + vec2<f32>(0.0, 2.0)) * texel);
    let g3 = gather(k, (c.at + 2.0) * texel);
    let rows = vec4<f32>(
        dot(vec4<f32>(g0.w, g0.z, g1.w, g1.z), wx),
        dot(vec4<f32>(g0.x, g0.y, g1.x, g1.y), wx),
        dot(vec4<f32>(g2.w, g2.z, g3.w, g3.z), wx),
        dot(vec4<f32>(g2.x, g2.y, g3.x, g3.y), wx),
    );
    return dot(rows, wy);
}

// Channel `k` of the eight layer weights over the 2x2 texels around `at`.
fn gather(k: u32, at: vec2<f32>) -> vec4<f32> {
    switch k {
        case 0u: { return textureGather(0, t_palette_weights0, s_gather, at); }
        case 1u: { return textureGather(1, t_palette_weights0, s_gather, at); }
        case 2u: { return textureGather(2, t_palette_weights0, s_gather, at); }
        case 3u: { return textureGather(3, t_palette_weights0, s_gather, at); }
        case 4u: { return textureGather(0, t_palette_weights1, s_gather, at); }
        case 5u: { return textureGather(1, t_palette_weights1, s_gather, at); }
        case 6u: { return textureGather(2, t_palette_weights1, s_gather, at); }
        default: { return textureGather(3, t_palette_weights1, s_gather, at); }
    }
}

// Layer `k` of a tile's eight (its ids packed a byte each).
fn layer_id(ids: vec2<u32>, k: u32) -> u32 {
    return (ids[k >> 2u] >> ((k & 3u) * 8u)) & 255u;
}

// `dw` is how far `world` moves per pixel across and down the screen (per
// texel across and down a ground page when baking one).
fn splat(uv: vec2<f32>, world: vec3<f32>, geometry: vec3<f32>, dw: array<vec3<f32>, 2>) -> GroundSample {
    let far = max(length(dw[0]), length(dw[1])) > FAR_GROUND;
    var w = triplanar(geometry);
    // Relief keeps the blend far off too: one projection switching to the
    // next steps the height between pixels, and its slope there spiked into
    // dark grain.
    if far && !relief() {
        let a = abs(geometry);
        w = select(select(vec3<f32>(0.0, 0.0, 1.0), vec3<f32>(0.0, 1.0, 0.0), a.y >= a.z), vec3<f32>(1.0, 0.0, 0.0), a.x > a.y && a.x > a.z);
    }
    // Anno's material maps hold texel i's value at i, not i + 0.5, and the
    // last row and column repeat past the map (terrain_baking.fx
    // psBakeSplatsBulk).
    let texels = min(uv * a7m.control, a7m.control - 1.0);
    let tiles = vec2<i32>(textureDimensions(t_palette_ids0));
    let tile = clamp(vec2<i32>(floor(texels / PALETTE_TILE)), vec2<i32>(0), tiles - 1);
    // In the tile's region, which starts PALETTE_BEFORE texels early.
    let local = texels - vec2<f32>(tile) * PALETTE_TILE + f32(PALETTE_BEFORE);
    let base = floor(local);
    let f = local - base;
    let b = vec2<i32>(base) + tile * PALETTE_SIDE;
    // Stored over 255 (palette.wgsl), so packing rounds them back to bytes.
    let ids = vec2<u32>(
        pack4x8unorm(textureLoad(t_palette_ids0, tile, 0)),
        pack4x8unorm(textureLoad(t_palette_ids1, tile, 0)),
    );
    // The tile's layers stop at the first NO_LAYER, and every texel read
    // here lies in the tile's own region.
    var count = 8u;
    for (var k = 0u; k < 8u; k++) {
        if layer_id(ids, k) == NO_LAYER {
            count = k;
            break;
        }
    }
    let plain = plain_bake();
    // Smooth: 4x4 texels under Hermite weights (a = -0.5), normalised by
    // their sum, so coverage is C1 continuous; layer_cover reads them.
    var wx = vec4<f32>(hermite(1.0 + f.x), hermite(f.x), hermite(1.0 - f.x), hermite(2.0 - f.x));
    var wy = vec4<f32>(hermite(1.0 + f.y), hermite(f.y), hermite(1.0 - f.y), hermite(2.0 - f.y));
    wx /= wx.x + wx.y + wx.z + wx.w;
    wy /= wy.x + wy.y + wy.z + wy.w;
    let cover = Cover(vec2<f32>(b), wx.xyz, wy.xyz);
    // Plain: its 2x2 texels at and after, bilinear, loaded once for every
    // layer. Gathered per layer instead, 1800's moderate view drew 0.15 ms
    // slower.
    var flat = array<half4, 2>(half4(0.0), half4(0.0));
    if plain {
        let second = count > 4u;
        for (var y = 0; y < 2; y++) {
            for (var x = 0; x < 2; x++) {
                let at = b + vec2<i32>(x, y);
                let w = half(select(1.0 - f.x, f.x, x == 1) * select(1.0 - f.y, f.y, y == 1));
                flat[0] += half4(textureLoad(t_palette_weights0, at, 0)) * w;
                if second {
                    flat[1] += half4(textureLoad(t_palette_weights1, at, 0)) * w;
                }
            }
        }
    }
    // Compiled out without normal layers rather than skipped by a uniform:
    // the bent normal's frame and sums held the splat at 72 registers a
    // thread against 56 without (RTX 4080 SUPER), for every ground pixel,
    // and dropping them took the ground from 1.98 to 1.78 ms on pirate and
    // 2.37 to 2.11 ms on continental_overview at 5K.
    let normals = ground_normals() && !far;
    let glossy = a7m.gloss > 0.5;
    // Composited top down with what still shows through below: the same
    // as laying each layer over those beneath, but a layer that covers the
    // pixel (full grass) ends the loop, so most pixels sample one or two
    // layers instead of every one present: at a 5K viewport the splat went
    // from 1.37 to 1.20 ms with the same pixels.
    let heights = relief();
    var color = half4(0.0);
    var normal = half3(0.0);
    var gloss = half(0.0);
    var height = 0.0;
    var through = half(1.0);
    // What still shows of the heights below: an adding layer
    // (DisplacementAdd) lays its height on them without hiding them.
    var through_height = half(1.0);
    for (var k = i32(count) - 1; k >= 0; k--) {
        let i = u32(k);
        var weight = flat[i >> 2u][i & 3u];
        if !plain { weight = half(layer_cover(cover, i)); }
        if weight <= 0.0 { continue; }
        let layer = layer_id(ids, i);
        let info = b_layers[layer];
        let one = layer_color(layer, info.scale, world, dw, geometry, w);
        let texel = one.color;
        // BlendSplat; a smooth bake then eases it (BlendSplatSmooth), so
        // layer borders have no crease where the cut saturates.
        let sharpness = half(info.sharpness);
        var alpha = saturate(sharpness * (saturate(texel.a) - 1.0) + (sharpness + 1.0) * weight);
        if !plain { alpha = alpha * alpha * (3.0 - 2.0 * alpha); }
        color += texel * (alpha * through);
        if alpha > 0.0 {
            let detail = layer_detail(layer, info.scale, world, dw, geometry, one.weights, normals, glossy, heights);
            gloss += detail.gloss * (alpha * through);
            if normals { normal += detail.normal * (alpha * through); }
            // psBakeSplatsBulk: the height mapped into the layer's range.
            if heights { height += mix(info.floor, info.ceil, detail.height) * f32(alpha * through_height); }
        }
        if heights && info.add < 0.5 { through_height *= 1.0 - alpha; }
        through *= 1.0 - alpha;
        if through < 1.0 / 512.0 && (!heights || through_height < 1.0 / 512.0) { break; }
    }
    // What the layers leave uncovered is the bake's base, matte and facing
    // the way the ground does.
    color += half4(half3(splat_base()), 1.0) * through;
    var out = GroundSample(vec4<f32>(color), vec3<f32>(normal), f32(gloss), height, 0.0);
    // A plain bake holds stored values: decoded with gamma 2.2 once blended.
    if plain && toned() {
        out.color = vec4<f32>(pow(max(out.color.rgb, vec3<f32>(0.0)), vec3<f32>(2.2)), out.color.a);
    }
    if normals { out.normal += geometry * f32(through); }
    if !normals { out.normal = geometry; }
    if dot(out.normal, out.normal) < 1e-12 { out.normal = geometry; }
    out.normal = normalize(out.normal);
    return out;
}

// The splat's grit over `sample` (BlendTerrainGrit): the mask's red under
// the point, cut by the grit texel's alpha as a layer is, and only where
// the ground faces up: (n.y + slope)^16, so about 0.8 and up at Anno's 0.2.
// Grit is matte. `dw` as for `splat`.
fn grit(sample: GroundSample, world: vec3<f32>, dw: array<vec3<f32>, 2>) -> GroundSample {
    if a7m.grit.w < 0.5 { return sample; }
    let uv = world.xz * a7m.grit.x;
    let duv = array<vec2<f32>, 2>(dw[0].xz * a7m.grit.x, dw[1].xz * a7m.grit.x);
    // Clamped to the edge texels' centres: the sampler repeats, and the
    // mask must not wrap to its far side.
    let half = 0.5 / vec2<f32>(textureDimensions(t_grit_mask));
    let at = clamp((world.xz - a7m.origin) * a7m.grit_mask, half, 1.0 - half);
    var cover = textureSampleLevel(t_grit_mask, s_albedo, at, 0.0).r;
    if cover <= 0.0 { return sample; }
    let texel = textureSampleGrad(t_grit, s_albedo, uv, duv[0], duv[1]);
    let s = a7m.grit.y;
    cover = saturate(s * (saturate(texel.a) - 1.0) + (s + 1.0) * cover);
    cover *= pow(saturate(sample.normal.y + a7m.grit.z), 16.0);
    var out = sample;
    out.color = vec4<f32>(mix(sample.color.rgb, decode(texel.rgb), cover), sample.color.a);
    out.gloss = mix(sample.gloss, 0.0, cover);
    return out;
}

// Tint multiplies the decoded splat after the blend and grit, clamped,
// before lighting, as Anno's terrain does. The decode is a7m.rs
// TINT_DECODE, appended as `a7m_tint`.
fn ground(uv: vec2<f32>, world: vec3<f32>, n: vec3<f32>, dw: array<vec3<f32>, 2>) -> GroundSample {
    var sample = grit(splat(uv, world, n, dw), world, dw);
    if has_ground_tint() {
        sample.color = vec4<f32>(min(sample.color.rgb * a7m_tint(ground_tint_texel(world)), vec3<f32>(1.0)), sample.color.a);
    }
    return sample;
}

fn ground_displacement(uv: vec2<f32>, world: vec3<f32>, n: vec3<f32>, dw: array<vec3<f32>, 2>) -> f32 {
    return splat(uv, world, n, dw).height;
}
