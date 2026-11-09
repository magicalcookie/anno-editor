// The selection's outline: a band where pixels of marked instances meet
// unmarked ones, read from the opaque pass's pick handles. Drawn over the
// frame with alpha blending.
//
// b_marks: [placed words, 0xRRGGBBAA, width px, 0], then one bit per placed
// row (handles with the high bit set), then one bit per batch handle - 1.

@group(0) @binding(0) var t_ids: texture_2d<u32>;
@group(0) @binding(1) var<storage, read> b_marks: array<u32>;

struct Out {
    @builtin(position) clip: vec4<f32>,
}

@vertex fn vs_outline(@builtin(vertex_index) vertex: u32) -> Out {
    let p = vec2<f32>(f32((vertex << 1u) & 2u), f32(vertex & 2u)) * 2.0 - 1.0;
    var out: Out;
    out.clip = vec4<f32>(p, 0.0, 1.0);
    return out;
}

fn bit(word: u32, i: u32) -> bool {
    if word >= arrayLength(&b_marks) {
        return false;
    }
    return ((b_marks[word] >> (i & 31u)) & 1u) == 1u;
}

fn marked(handle: u32) -> bool {
    if handle == 0u {
        return false;
    }
    if (handle & 0x80000000u) != 0u {
        let row = handle & 0x7fffffffu;
        if (row >> 5u) >= b_marks[0] {
            return false;
        }
        return bit(4u + (row >> 5u), row);
    }
    let i = handle - 1u;
    return bit(4u + b_marks[0] + (i >> 5u), i);
}

fn at(p: vec2<i32>, last: vec2<i32>) -> bool {
    return marked(textureLoad(t_ids, clamp(p, vec2<i32>(0), last), 0).x);
}

// How far a gap inside a model may be: a pixel with marked pixels within
// this reach in most directions lies inside the model's shape.
const ENCLOSED: i32 = 16;

// Whether `p` lies in a gap of the model's own shape (between a bush's
// leaf clumps) rather than outside it: marked pixels within ENCLOSED in at
// least 6 of 8 directions. With all of 4 required, gaps open on one side
// between clumps still took the band, and a sparse bush read as orange
// loops; this merges the clumps into one shape with one ring.
fn enclosed(p: vec2<i32>, last: vec2<i32>) -> bool {
    let dirs = array<vec2<i32>, 8>(
        vec2<i32>(1, 0), vec2<i32>(-1, 0), vec2<i32>(0, 1), vec2<i32>(0, -1),
        vec2<i32>(1, 1), vec2<i32>(-1, -1), vec2<i32>(1, -1), vec2<i32>(-1, 1),
    );
    var hits = 0;
    for (var i = 0; i < 8; i++) {
        for (var k = 1; k <= ENCLOSED; k++) {
            if at(p + dirs[i] * k, last) {
                hits++;
                break;
            }
        }
        // Three misses already rule it out.
        if i + 1 - hits >= 3 {
            return false;
        }
    }
    return hits >= 6;
}

// The selection's own pixels take a light wash of the colour, so a sparse
// bush in tall grass reads as picked and not only its box; an unmarked pixel
// within `width` of a marked one takes the colour, and the pixel past that
// a dark keyline.
@fragment fn fs_outline(in: Out) -> @location(0) vec4<f32> {
    let p = vec2<i32>(in.clip.xy);
    let last = vec2<i32>(textureDimensions(t_ids)) - 1;
    let bits = b_marks[1];
    let color = vec3<f32>(f32(bits >> 24u), f32((bits >> 16u) & 255u), f32((bits >> 8u) & 255u)) / 255.0;
    if at(p, last) {
        // Light, so the model keeps its own colours: at 0.3 a picked bush
        // read as orange. The band and the box carry the selection.
        return vec4<f32>(color, 0.12);
    }
    // The band, then a dark keyline past it: orange alone sank into
    // straw and gorse. Kept thin: on a sparse bush every gap lies within a
    // few pixels of a leaf, and a 3 px band with a 2 px keyline filled the
    // whole bush orange.
    // The band starts OFFSET px out from the model: gaps narrower than
    // twice that between leaf clumps stay unbanded and the clumps read as
    // one shape (a cheap closing), where a band on the pixels themselves
    // traced every clump as a loop of specks.
    let offset = 3.0;
    let band = f32(max(b_marks[2], 1u)) + offset;
    // Two pixels of keyline, the outer one soft: with one, the orange
    // band vanished into ochre marsh grass of the same hue.
    let width = i32(band) + 2;
    var near = 1e9;
    for (var y = -width; y <= width; y++) {
        for (var x = -width; x <= width; x++) {
            let d = length(vec2<f32>(f32(x), f32(y)));
            if d > f32(width) + 0.5 || d >= near {
                continue;
            }
            if at(p + vec2<i32>(x, y), last) {
                near = d;
            }
        }
    }
    if near > f32(width) + 0.5 || near <= offset + 0.5 || enclosed(p, last) {
        discard;
    }
    if near > band + 0.5 {
        return vec4<f32>(0.04, 0.04, 0.04, 0.85 * clamp(f32(width) + 0.5 - near, 0.0, 1.0));
    }
    return vec4<f32>(color, f32(bits & 255u) / 255.0);
}
