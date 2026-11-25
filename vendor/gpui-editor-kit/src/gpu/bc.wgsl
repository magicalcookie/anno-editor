// Block compression of images the GPU bakes (bc.rs): one 4x4 block per
// invocation, into rows of 16-byte blocks in a buffer that the frame then
// copies into a BC texture. Color endpoints come from the block's extremes
// along its principal axis, refined once by least squares as stb_dxt does
// on the CPU, or from stb_dxt's single-color pair for its mean, whichever
// errs less; single channels take the block's range.

// A rectangle of one layer's mip, as rows of blocks at `offset` words,
// `pitch` words apart. `kind`: 0 BC3 from RGBA, 1 BC5 from RG.
struct Job {
    x: u32,
    y: u32,
    layer: u32,
    mip: u32,
    blocks_x: u32,
    blocks_y: u32,
    offset: u32,
    pitch: u32,
    kind: u32,
    _pad0: u32,
    _pad1: u32,
    _pad2: u32,
}

@group(0) @binding(0) var t_source: texture_2d_array<f32>;
@group(0) @binding(1) var<storage, read> jobs: array<Job>;
@group(0) @binding(2) var<storage, read_write> blocks: array<u32>;

// A 3-bit index into a BC4 block's 48 index bits, which start at bit 16.
fn put3(words: ptr<function, vec2<u32>>, texel: u32, index: u32) {
    let bit = 16u + 3u * texel;
    if bit < 32u {
        (*words).x |= index << bit;
        if bit > 29u { (*words).y |= index >> (32u - bit); }
    } else {
        (*words).y |= index << (bit - 32u);
    }
}

// One channel (0..255) as BC4: the block's extremes as endpoints, the
// larger first for eight levels between them.
fn bc4(values: array<f32, 16>) -> vec2<u32> {
    // Indexed at run time: a local, not the by-value argument.
    var v = values;
    var lo = 255.0;
    var hi = 0.0;
    for (var i = 0u; i < 16u; i++) {
        lo = min(lo, v[i]);
        hi = max(hi, v[i]);
    }
    let e0 = u32(round(hi));
    let e1 = u32(round(lo));
    var words = vec2<u32>(e0 | (e1 << 8u), 0u);
    if e0 <= e1 { return words; }
    let a = f32(e0);
    let b = f32(e1);
    for (var i = 0u; i < 16u; i++) {
        // Level k of 7 from e1 up to e0; BC4 numbers e0 0, e1 1, and the
        // six between from e0's side down.
        let k = u32(clamp(round((v[i] - b) / (a - b) * 7.0), 0.0, 7.0));
        var index = 8u - k;
        if k == 7u { index = 0u; }
        if k == 0u { index = 1u; }
        put3(&words, i, index);
    }
    return words;
}

fn rgb565(c: vec3<f32>) -> u32 {
    let q = vec3<u32>(round(clamp(c, vec3<f32>(0.0), vec3<f32>(255.0)) * vec3<f32>(31.0, 63.0, 31.0) / 255.0));
    return (q.x << 11u) | (q.y << 5u) | q.z;
}

// A 565 color as the decoder widens it to 8 bits.
fn expand565(c: u32) -> vec3<f32> {
    return vec3<f32>(widen((c >> 11u) & 31u, 5u), widen((c >> 5u) & 63u, 6u), widen(c & 31u, 5u));
}

// One channel widened from `bits` as the decoder does.
fn widen(e: u32, bits: u32) -> f32 {
    if bits == 5u { return f32((e << 3u) | (e >> 2u)); }
    return f32((e << 2u) | (e >> 4u));
}

struct Color { words: vec2<u32>, error: f32 }

// The endpoint pair of `bits` whose two-thirds point lands nearest `v`
// (0..255), as stb_dxt's single-color tables: a flat block keeps its color
// instead of rounding to 565's, up to 6 of 255 off. As there, a pair also
// pays 3% of its spread: GPUs interpolate less exactly than (2a + b) / 3.
fn single(v: f32, bits: u32) -> vec2<u32> {
    let top = (1u << bits) - 1u;
    var best = vec2<u32>(0u);
    var least = 1e30;
    for (var e0 = 0u; e0 <= top; e0++) {
        let x0 = widen(e0, bits);
        let guess = i32(round((3.0 * v - 2.0 * x0) * f32(top) / 255.0));
        for (var k = -1; k <= 1; k++) {
            let e1 = u32(clamp(guess + k, 0, i32(top)));
            let x1 = widen(e1, bits);
            let e = abs((2.0 * x0 + x1) / 3.0 - v) + 0.03 * abs(x0 - x1);
            if e < least {
                least = e;
                best = vec2<u32>(e0, e1);
            }
        }
    }
    return best;
}

// Every texel at the two-thirds point of the single-color endpoints for
// `mean`, and the squared error.
fn bc1_flat(colors: array<vec3<f32>, 16>, mean: vec3<f32>) -> Color {
    var c = colors;
    let r = single(mean.x, 5u);
    let g = single(mean.y, 6u);
    let b = single(mean.z, 5u);
    let q0 = (r.x << 11u) | (g.x << 5u) | b.x;
    let q1 = (r.y << 11u) | (g.y << 5u) | b.y;
    let p = (2.0 * expand565(q0) + expand565(q1)) / 3.0;
    var out: Color;
    out.words = vec2<u32>(q0 | (q1 << 16u), 0xaaaaaaaau);
    out.error = 0.0;
    for (var i = 0u; i < 16u; i++) {
        let d = c[i] - p;
        out.error += dot(d, d);
    }
    return out;
}

// The color block for endpoints `e0`, `e1` (0..255) and its squared error.
// BC3's color blocks always have four colors, whichever endpoint is
// larger: 0 c0, 1 c1, 2 and 3 the thirds between, from c0's side.
fn bc1_indices(colors: array<vec3<f32>, 16>, e0: vec3<f32>, e1: vec3<f32>) -> Color {
    var c = colors;
    let q0 = rgb565(e0);
    let q1 = rgb565(e1);
    var out: Color;
    out.words = vec2<u32>(q0 | (q1 << 16u), 0u);
    out.error = 0.0;
    let p0 = expand565(q0);
    let p1 = expand565(q1);
    var palette = array<vec3<f32>, 4>(p0, p1, (2.0 * p0 + p1) / 3.0, (p0 + 2.0 * p1) / 3.0);
    for (var i = 0u; i < 16u; i++) {
        var best = 0u;
        var least = 1e30;
        for (var k = 0u; k < 4u; k++) {
            let d = c[i] - palette[k];
            let e = dot(d, d);
            if e < least {
                least = e;
                best = k;
            }
        }
        out.words.y |= best << (2u * i);
        out.error += least;
    }
    return out;
}

// RGB (0..255) as BC3's color block: endpoints at the texels' extremes
// along their principal axis, then once more from the least-squares
// endpoints for the indices that gave, and the single-color pair for their
// mean; the best of the three.
fn bc1(colors: array<vec3<f32>, 16>) -> vec2<u32> {
    var c = colors;
    var mean = vec3<f32>(0.0);
    var lo = vec3<f32>(255.0);
    var hi = vec3<f32>(0.0);
    for (var i = 0u; i < 16u; i++) {
        mean += c[i];
        lo = min(lo, c[i]);
        hi = max(hi, c[i]);
    }
    mean /= 16.0;
    // Covariance: xx, xy, xz, yy, yz, zz.
    var cov = array<f32, 6>(0.0, 0.0, 0.0, 0.0, 0.0, 0.0);
    for (var i = 0u; i < 16u; i++) {
        let d = c[i] - mean;
        cov[0] += d.x * d.x;
        cov[1] += d.x * d.y;
        cov[2] += d.x * d.z;
        cov[3] += d.y * d.y;
        cov[4] += d.y * d.z;
        cov[5] += d.z * d.z;
    }
    // The principal axis by power iteration from the bounding box's
    // diagonal.
    var axis = hi - lo;
    for (var k = 0; k < 8; k++) {
        let next = vec3<f32>(
            cov[0] * axis.x + cov[1] * axis.y + cov[2] * axis.z,
            cov[1] * axis.x + cov[3] * axis.y + cov[4] * axis.z,
            cov[2] * axis.x + cov[4] * axis.y + cov[5] * axis.z,
        );
        let m = max(max(abs(next.x), abs(next.y)), abs(next.z));
        if m < 1e-6 { break; }
        axis = next / m;
    }
    if dot(axis, axis) < 1e-12 { axis = vec3<f32>(1.0); }
    axis = normalize(axis);
    var t0 = 1e30;
    var t1 = -1e30;
    for (var i = 0u; i < 16u; i++) {
        let t = dot(c[i] - mean, axis);
        t0 = min(t0, t);
        t1 = max(t1, t);
    }
    var best = bc1_flat(c, mean);
    let spread = bc1_indices(c, mean + axis * t1, mean + axis * t0);
    if spread.error < best.error { best = spread; }
    // Least squares: each texel's index weighs c0 by w and c1 by 1 - w.
    if (spread.words.x & 0xffffu) != (spread.words.x >> 16u) {
        var weights = array<f32, 4>(1.0, 0.0, 2.0 / 3.0, 1.0 / 3.0);
        var aa = 0.0;
        var ab = 0.0;
        var bb = 0.0;
        var ax = vec3<f32>(0.0);
        var bx = vec3<f32>(0.0);
        for (var i = 0u; i < 16u; i++) {
            let w = weights[(spread.words.y >> (2u * i)) & 3u];
            aa += w * w;
            ab += w * (1.0 - w);
            bb += (1.0 - w) * (1.0 - w);
            ax += w * c[i];
            bx += (1.0 - w) * c[i];
        }
        let det = aa * bb - ab * ab;
        if abs(det) > 1e-6 {
            let e0 = (bb * ax - ab * bx) / det;
            let e1 = (aa * bx - ab * ax) / det;
            let refined = bc1_indices(c, e0, e1);
            if refined.error < best.error { best = refined; }
        }
    }
    return best.words;
}

@compute @workgroup_size(8, 8)
fn cs_encode(@builtin(global_invocation_id) id: vec3<u32>) {
    let job = jobs[id.z];
    if id.x >= job.blocks_x || id.y >= job.blocks_y { return; }
    let last = vec2<i32>(textureDimensions(t_source, job.mip)) - 1;
    let corner = vec2<i32>(i32(job.x), i32(job.y)) + vec2<i32>(id.xy) * 4;
    var texels: array<vec4<f32>, 16>;
    for (var i = 0u; i < 16u; i++) {
        let at = min(corner + vec2<i32>(i32(i & 3u), i32(i >> 2u)), last);
        texels[i] = round(textureLoad(t_source, at, job.layer, job.mip) * 255.0);
    }
    var words: vec4<u32>;
    if job.kind == 0u {
        var a: array<f32, 16>;
        var c: array<vec3<f32>, 16>;
        for (var i = 0u; i < 16u; i++) {
            a[i] = texels[i].a;
            c[i] = texels[i].rgb;
        }
        // BC3: the alpha block, then the color block.
        words = vec4<u32>(bc4(a), bc1(c));
    } else {
        var u: array<f32, 16>;
        var v: array<f32, 16>;
        for (var i = 0u; i < 16u; i++) {
            u[i] = texels[i].r;
            v[i] = texels[i].g;
        }
        words = vec4<u32>(bc4(u), bc4(v));
    }
    let at = job.offset + id.y * job.pitch + id.x * 4u;
    blocks[at] = words.x;
    blocks[at + 1u] = words.y;
    blocks[at + 2u] = words.z;
    blocks[at + 3u] = words.w;
}
