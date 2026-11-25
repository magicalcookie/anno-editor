// Draws the UI canvas on the GPU into one texture GPUI shows in the same
// frame (`gpui::TextureHandoff`), recorded on a thread of its own. Images
// draw the way the game's widget shader does: one quad per image with its
// affine transform, UV mapping (9-slice, UVScale, UVRotation, UVOffset, wrap
// or clamp), tint, alpha and blend state, and the mask chain it draws
// through evaluated per pixel. Labels draw as glyphs from GPUI's own
// rasterizer, placed where GPUI's elements placed them. The canvas builds a
// `Scene` as it renders; painting turns it into a `Frame` of texture pixels
// for the thread. Images and glyphs are one ordered list, so an image after
// a label covers it, and add and multiply images blend with the labels and
// the page under them.
use gpui_editor_kit::{
    gpu::OnDevice,
    gpui::{self, App, Bounds, Hsla, Pixels, Point, Rgba, SharedString, Window, point, px},
    profile,
    texture::{Block, Compressed, Texture},
};
use std::sync::{Arc, Mutex};

/// Mask layers one draw evaluates; deeper chains drop the outermost.
/// TODO(alex): no shipped screen nests deeper than four; count them.
pub const MAX_MASKS: usize = 6;

/// Whether the canvas draws images here (where GPUI renders with wgpu)
/// rather than as CPU composites. `ANNO_UI_CPU=1` forces the CPU path;
/// tests use it unless `ANNO_UI_GPU=1`.
pub fn enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        let platform = cfg!(any(
            target_os = "linux",
            target_os = "windows",
            target_os = "macos"
        ));
        let forced_cpu = std::env::var_os("ANNO_UI_CPU").is_some();
        let tests_opt_in = !cfg!(test) || std::env::var_os("ANNO_UI_GPU").is_some();
        platform && !forced_cpu && tests_opt_in
    })
}

/// A texture as the canvas draws it: decoded RGBA, and where the file is
/// block-compressed its mip chain as stored, which devices that sample BC
/// take instead. Mips are what the game samples at 1080p from the 4k set:
/// without them fine detail aliases (the bars' marble veins).
#[derive(Debug)]
pub struct Source {
    pub rgba: Arc<Texture>,
    pub bc: Option<Compressed>,
}

/// How a box maps world (design) pixels to texture UV: the inverse of its
/// transform, the box offset and the world scale (so 9-slice borders keep
/// their drawn size), then the image's own UV settings.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Quad {
    /// World pixels to the widget's own pixels.
    pub inv: [f32; 6],
    pub min: [f32; 2],
    pub scale: [f32; 2],
    /// The box in design pixels.
    pub size: [f32; 2],
    /// The ImageAsset's pixel size: UVOffset's unit.
    pub px: [f32; 2],
    pub uv_offset: [f32; 2],
    pub uv_scale: [f32; 2],
    /// UVRotation's cosine and sine.
    pub rotation: [f32; 2],
    /// 9-slice borders in design pixels (l, t, r, b) and the UV centre.
    pub nine: Option<([f32; 4], [[f32; 2]; 2])>,
    pub clamp: bool,
}

impl Quad {
    /// UV at design point `p`, as the fragment shader's `own`, `map_uv`
    /// and `address` find it; None outside the box.
    fn uv_at(&self, p: [f32; 2], dims: [u32; 2]) -> Option<[f32; 2]> {
        let i = self.inv;
        let o = [
            i[0] * p[0] + i[2] * p[1] + i[4],
            i[1] * p[0] + i[3] * p[1] + i[5],
        ];
        let b = [0, 1].map(|a| (o[a] - self.min[a]) * self.scale[a]);
        let slack = 1e-3;
        if (0..2).any(|a| b[a] < -slack || b[a] > self.size[a] + slack) {
            return None;
        }
        let b = [0, 1].map(|a| b[a].clamp(0., self.size[a]));
        let mut uv = [0.; 2];
        for a in 0..2 {
            let size = self.size[a];
            uv[a] = match self.nine {
                Some((m, c)) => {
                    let k = (size / (m[a] + m[a + 2]).max(1e-3)).min(1.);
                    let (lo, hi) = (m[a] * k, m[a + 2] * k);
                    if b[a] < lo {
                        b[a] / lo.max(1e-3) * c[0][a]
                    } else if b[a] > size - hi {
                        c[1][a] + (b[a] - (size - hi)) / hi.max(1e-3) * (1. - c[1][a])
                    } else {
                        c[0][a] + (b[a] - lo) / (size - lo - hi).max(1e-3) * (c[1][a] - c[0][a])
                    }
                }
                None => b[a] / size.max(1e-3),
            };
            uv[a] = (uv[a] - 0.5) * self.uv_scale[a];
        }
        let [c, s] = self.rotation;
        let turned = [c * uv[0] - s * uv[1], s * uv[0] + c * uv[1]];
        let uv = [0, 1].map(|a| turned[a] + 0.5 + self.uv_offset[a] / self.px[a].max(1e-3));
        Some(if self.clamp {
            [0, 1].map(|a| {
                let half = 0.5 / dims[a].max(1) as f32;
                uv[a].clamp(half, 1. - half)
            })
        } else {
            uv
        })
    }

    /// The same box moved `d` design pixels.
    pub(super) fn moved(mut self, d: [f32; 2]) -> Quad {
        let i = self.inv;
        self.inv[4] -= i[0] * d[0] + i[2] * d[1];
        self.inv[5] -= i[1] * d[0] + i[3] * d[1];
        self
    }
}

/// One mask provider of a chain (see `scene::MaskLayer`).
#[derive(Clone, Debug)]
pub struct Mask {
    pub quad: Quad,
    /// None: the box itself covers.
    pub texture: Option<Arc<Source>>,
    pub alpha: f32,
    /// Coverage = smoothstep(edge.0, edge.1, alpha × texel alpha); hard
    /// when they are equal.
    pub edge: (f32, f32),
    pub invert: bool,
    /// Intersects with the masks before it in its run.
    pub intersect: bool,
    /// Multiplies into the coverage on its own (a soft mask); outside its
    /// box it leaves coverage alone when `exclusive`, else hides.
    pub soft: bool,
    pub exclusive: bool,
    /// Which run of the chain: 0 the nearest; runs multiply.
    pub level: u32,
    /// The layout tree index of the widget that provides it.
    pub by: usize,
}

/// One image to draw.
#[derive(Clone, Debug)]
pub struct Sprite {
    /// The untinted texture.
    pub texture: Arc<Source>,
    /// Own pixels to design pixels, and the box in own pixels.
    pub transform: [f32; 6],
    pub local: [f32; 4],
    pub quad: Quad,
    pub tint: [u8; 3],
    pub alpha: f32,
    /// BlendingMode as drawn (inherit resolved).
    pub blend: u32,
    pub masks: Vec<Mask>,
}

impl Sprite {
    /// How opaque the image draws at design point `p` (texture alpha by
    /// its own alpha; masks not counted): 0 outside its box. The canvas
    /// hit-tests by it, so a ring's turned wedges and round strokes catch
    /// the pointer only where they show.
    pub fn alpha_at(&self, p: [f32; 2]) -> f32 {
        let t = &self.texture.rgba;
        let Some(uv) = self.quad.uv_at(p, [t.width, t.height]) else {
            return 0.;
        };
        if t.width == 0 || t.height == 0 {
            return self.alpha;
        }
        // Wrap repeats; nearest texel is enough for picking.
        let at = |v: f32, n: u32| ((v.rem_euclid(1.) * n as f32) as u32).min(n - 1);
        let (x, y) = (at(uv[0], t.width), at(uv[1], t.height));
        let a = t
            .rgba
            .get(((y * t.width + x) * 4 + 3) as usize)
            .copied()
            .unwrap_or(0);
        f32::from(a) / 255. * self.alpha
    }

    /// The same, `k` times as opaque: context outside a template edited
    /// in place.
    pub fn faded(&self, k: f32) -> Sprite {
        let mut s = self.clone();
        s.alpha *= k;
        s
    }

    /// The image moved `d` design pixels, as a move being dragged shows it:
    /// masks whose provider `carried` says moves along (the dragged
    /// container's own) go with it, the rest stay where they are. Round 5
    /// cut the minimap's ring into wedges by leaving its own masks behind.
    pub fn dragged(&self, d: [f32; 2], carried: impl Fn(usize) -> bool) -> Sprite {
        let mut s = self.clone();
        s.transform[4] += d[0];
        s.transform[5] += d[1];
        s.quad = s.quad.moved(d);
        for m in &mut s.masks {
            if carried(m.by) {
                m.quad = m.quad.moved(d);
            }
        }
        s
    }

    /// The image as a resize in progress shows it: its box mapped from
    /// `from` to `to` (design pixels), its masks where they are. Resizing
    /// sets Scale, which scales the texture with the box: tiles and
    /// 9-slice centres follow, borders keep their size.
    pub fn resized(&self, from: [f32; 4], to: [f32; 4]) -> Sprite {
        let k = [to[2] / from[2].max(1e-3), to[3] / from[3].max(1e-3)];
        // Design to design, and back.
        let m = [
            k[0],
            0.,
            0.,
            k[1],
            to[0] - from[0] * k[0],
            to[1] - from[1] * k[1],
        ];
        let back = [
            1. / k[0],
            0.,
            0.,
            1. / k[1],
            from[0] - to[0] / k[0],
            from[1] - to[1] / k[1],
        ];
        let mut s = self.clone();
        s.transform = compose(m, s.transform);
        s.quad.inv = compose(s.quad.inv, back);
        s.quad.size = [s.quad.size[0] * k[0], s.quad.size[1] * k[1]];
        s.quad.scale = [s.quad.scale[0] * k[0], s.quad.scale[1] * k[1]];
        s
    }

    /// The same image and masks moved `d` design pixels (moved with its
    /// mask chain, as a relayout found it).
    pub fn moved(&self, d: [f32; 2]) -> Sprite {
        let mut s = self.clone();
        s.transform[4] += d[0];
        s.transform[5] += d[1];
        s.quad = s.quad.moved(d);
        for m in &mut s.masks {
            m.quad = m.quad.moved(d);
        }
        s
    }
}

/// `a` after `b`, as `scene::apply` reads a 2×3 transform.
fn compose(a: [f32; 6], b: [f32; 6]) -> [f32; 6] {
    [
        a[0] * b[0] + a[2] * b[1],
        a[1] * b[0] + a[3] * b[1],
        a[0] * b[2] + a[2] * b[3],
        a[1] * b[2] + a[3] * b[3],
        a[0] * b[4] + a[2] * b[5] + a[4],
        a[1] * b[4] + a[3] * b[5] + a[5],
    ]
}

const BLEND_ADD: u32 = 2;
const BLEND_MULTIPLY: u32 = 3;

/// The pipeline for a blend mode: 0 normal, 1 add, 2 multiply.
fn blend_kind(blend: u32) -> usize {
    match blend {
        BLEND_ADD => 1,
        BLEND_MULTIPLY => 2,
        _ => 0,
    }
}

// GPU layouts. WGSL storage structs align vec4s to 16 bytes.
#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct GpuQuad {
    inv0: [f32; 4],
    // inv[4..6], min
    inv1: [f32; 4],
    // scale, size
    box_: [f32; 4],
    // px, uv offset
    px: [f32; 4],
    // uv scale, rotation
    uv: [f32; 4],
    nine: [f32; 4],
    centre: [f32; 4],
    // nine?, clamp?, textured?, invert?
    flags: [f32; 4],
    // alpha, edge lo, edge hi, intersect?
    cover: [f32; 4],
    // level, soft?, exclusive?
    level: [f32; 4],
}

impl GpuQuad {
    fn of(q: &Quad) -> GpuQuad {
        let (nine, centre) = q.nine.unwrap_or(([0.; 4], [[0.; 2], [1.; 2]]));
        GpuQuad {
            inv0: [q.inv[0], q.inv[1], q.inv[2], q.inv[3]],
            inv1: [q.inv[4], q.inv[5], q.min[0], q.min[1]],
            box_: [q.scale[0], q.scale[1], q.size[0], q.size[1]],
            px: [q.px[0], q.px[1], q.uv_offset[0], q.uv_offset[1]],
            uv: [q.uv_scale[0], q.uv_scale[1], q.rotation[0], q.rotation[1]],
            nine,
            centre: [centre[0][0], centre[0][1], centre[1][0], centre[1][1]],
            flags: [
                f32::from(u8::from(q.nine.is_some())),
                f32::from(u8::from(q.clamp)),
                1.,
                0.,
            ],
            cover: [1., 0., 0., 0.],
            level: [0.; 4],
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct GpuDraw {
    // a, b, c, d of own → design
    t0: [f32; 4],
    // tx, ty, box min
    t1: [f32; 4],
    // box max, 0, 0
    t2: [f32; 4],
    // rgb, alpha
    tint: [f32; 4],
    // first quad, masks, blend kind, 0
    at: [u32; 4],
}

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct GpuView {
    // design → device: scale, offset
    xf: [f32; 4],
    // target size, srgb target?, 0
    target: [f32; 4],
}

const SHADER: &str = r#"
struct View { xf: vec4<f32>, screen: vec4<f32> };
struct Quad {
    inv0: vec4<f32>, inv1: vec4<f32>, box_: vec4<f32>, px: vec4<f32>, uv: vec4<f32>,
    nine: vec4<f32>, centre: vec4<f32>, flags: vec4<f32>, cover: vec4<f32>, level: vec4<f32>,
};
struct Draw { t0: vec4<f32>, t1: vec4<f32>, t2: vec4<f32>, tint: vec4<f32>, at: vec4<u32> };

@group(0) @binding(0) var<uniform> view: View;
@group(0) @binding(1) var<storage, read> draws: array<Draw>;
@group(0) @binding(2) var<storage, read> quads: array<Quad>;
@group(0) @binding(3) var samp: sampler;
@group(1) @binding(0) var image: texture_2d<f32>;
@group(1) @binding(1) var m0: texture_2d<f32>;
@group(1) @binding(2) var m1: texture_2d<f32>;
@group(1) @binding(3) var m2: texture_2d<f32>;
@group(1) @binding(4) var m3: texture_2d<f32>;
@group(1) @binding(5) var m4: texture_2d<f32>;
@group(1) @binding(6) var m5: texture_2d<f32>;

struct Out {
    @builtin(position) pos: vec4<f32>,
    @location(0) world: vec2<f32>,
    @location(1) @interpolate(flat) draw: u32,
};

@vertex
fn vs(@builtin(vertex_index) v: u32, @builtin(instance_index) d: u32) -> Out {
    let dr = draws[d];
    let corner = array(vec2(0., 0.), vec2(1., 0.), vec2(0., 1.), vec2(1., 0.), vec2(1., 1.), vec2(0., 1.))[v];
    let own = mix(dr.t1.zw, dr.t2.xy, corner);
    let world = vec2(dr.t0.x * own.x + dr.t0.z * own.y + dr.t1.x,
                     dr.t0.y * own.x + dr.t0.w * own.y + dr.t1.y);
    let device = world * view.xf.xy + view.xf.zw;
    var o: Out;
    o.pos = vec4(device.x / view.screen.x * 2. - 1., 1. - device.y / view.screen.y * 2., 0., 1.);
    o.world = world;
    o.draw = d;
    return o;
}

// World point `p` in the quad's box, in design pixels from its top-left,
// inside or not.
fn own(q: Quad, p: vec2<f32>) -> vec2<f32> {
    let o = vec2(q.inv0.x * p.x + q.inv0.z * p.y + q.inv1.x, q.inv0.y * p.x + q.inv0.w * p.y + q.inv1.y);
    return (o - q.inv1.zw) * q.box_.xy;
}

// `own`, clamped into the box; x < 0 outside it.
fn local(q: Quad, p: vec2<f32>) -> vec2<f32> {
    let b = own(q, p);
    let size = q.box_.zw;
    let slack = 1e-3;
    if b.x < -slack || b.y < -slack || b.x > size.x + slack || b.y > size.y + slack {
        return vec2(-1., -1.);
    }
    return clamp(b, vec2(0.), size);
}

// As scene::Mapping::uv: stretched or 9-sliced, then UVScale about the
// centre, UVRotation, UVOffset in image pixels.
fn map_uv(q: Quad, b: vec2<f32>) -> vec2<f32> {
    let size = q.box_.zw;
    var uv = vec2(0.);
    for (var a = 0; a < 2; a++) {
        if q.flags.x > 0.5 {
            let lo0 = select(q.nine.y, q.nine.x, a == 0);
            let hi0 = select(q.nine.w, q.nine.z, a == 0);
            let c0 = select(q.centre.y, q.centre.x, a == 0);
            let c1 = select(q.centre.w, q.centre.z, a == 0);
            let k = min(size[a] / max(lo0 + hi0, 1e-3), 1.);
            let lo = lo0 * k;
            let hi = hi0 * k;
            if b[a] < lo {
                uv[a] = b[a] / max(lo, 1e-3) * c0;
            } else if b[a] > size[a] - hi {
                uv[a] = c1 + (b[a] - (size[a] - hi)) / max(hi, 1e-3) * (1. - c1);
            } else {
                uv[a] = c0 + (b[a] - lo) / max(size[a] - lo - hi, 1e-3) * (c1 - c0);
            }
        } else {
            uv[a] = b[a] / max(size[a], 1e-3);
        }
        uv[a] = (uv[a] - 0.5) * q.uv[a];
    }
    let c = q.uv.z;
    let s = q.uv.w;
    let turned = vec2(c * uv.x - s * uv.y, s * uv.x + c * uv.y);
    return turned + 0.5 + q.px.zw / max(q.px.xy, vec2(1e-3));
}

// Clamp addressing keeps the filter off the far edge's texels.
fn address(q: Quad, uv: vec2<f32>, dims: vec2<u32>) -> vec2<f32> {
    if q.flags.y < 0.5 {
        return uv;
    }
    let half = 0.5 / vec2<f32>(dims);
    return clamp(uv, half, 1. - half);
}

fn mask_alpha(i: u32, uv: vec2<f32>) -> f32 {
    switch i {
        case 0u: { return textureSampleLevel(m0, samp, uv, 0.).a; }
        case 1u: { return textureSampleLevel(m1, samp, uv, 0.).a; }
        case 2u: { return textureSampleLevel(m2, samp, uv, 0.).a; }
        case 3u: { return textureSampleLevel(m3, samp, uv, 0.).a; }
        case 4u: { return textureSampleLevel(m4, samp, uv, 0.).a; }
        default: { return textureSampleLevel(m5, samp, uv, 0.).a; }
    }
}

fn mask_dims(i: u32) -> vec2<u32> {
    switch i {
        case 0u: { return textureDimensions(m0); }
        case 1u: { return textureDimensions(m1); }
        case 2u: { return textureDimensions(m2); }
        case 3u: { return textureDimensions(m3); }
        case 4u: { return textureDimensions(m4); }
        default: { return textureDimensions(m5); }
    }
}

// As scene::MaskLayer::coverage.
fn cover(q: Quad, i: u32, p: vec2<f32>) -> f32 {
    let b = local(q, p);
    if b.x < 0. && q.level.y > 0.5 && q.level.z > 0.5 {
        return 1.;
    }
    var c = 0.;
    if b.x >= 0. {
        let alpha = q.cover.x;
        let lo = q.cover.y;
        let hi = q.cover.z;
        if q.flags.z > 0.5 {
            let uv = address(q, map_uv(q, b), mask_dims(i));
            let a = alpha * mask_alpha(i, uv);
            if hi <= lo {
                c = select(0., 1., a >= lo);
            } else {
                c = smoothstep(lo, hi, a);
            }
        } else {
            c = select(0., 1., alpha >= lo || lo <= 0.);
        }
    }
    return select(c, 1. - c, q.flags.w > 0.5);
}

// As scene::Clip::coverage: within a run masks union (or intersect where
// marked), runs multiply.
fn coverage(d: Draw, p: vec2<f32>) -> f32 {
    var total = 1.;
    var own = -1.;
    var level = 0.;
    for (var i = 0u; i < d.at.y; i++) {
        let q = quads[d.at.x + 1u + i];
        if q.level.x != level {
            if own >= 0. { total *= own; }
            own = -1.;
            level = q.level.x;
        }
        let c = cover(q, i, p);
        if q.level.y > 0.5 {
            total *= c;
            continue;
        }
        if own < 0. {
            own = c;
        } else if q.cover.w > 0.5 {
            own = min(own, c);
        } else {
            own = max(own, c);
        }
    }
    if own >= 0. { total *= own; }
    return total;
}

fn to_target(c: vec3<f32>) -> vec3<f32> {
    // An sRGB target encodes what it is given; hand it linear values so it
    // stores the display-space colour the game blends in.
    if view.screen.z > 0.5 {
        return pow(c, vec3(2.2));
    }
    return c;
}

@fragment
fn fs(o: Out) -> @location(0) vec4<f32> {
    let d = draws[o.draw];
    let q = quads[d.at.x];
    // Sampled at the box coordinate clamped, which stays continuous across
    // the box edge: the 2×2 neighbours mips are picked by then see the same
    // UV step, not a jump to the corner (a 1 px outline of the smallest
    // mip around every image).
    let raw = own(q, o.world);
    let size = q.box_.zw;
    let uv = address(q, map_uv(q, clamp(raw, vec2(0.), size)), textureDimensions(image));
    let texel = textureSample(image, samp, uv);
    let slack = 1e-3;
    if any(raw < vec2(-slack)) || any(raw > size + slack) {
        discard;
    }
    let a = texel.a * d.tint.w * coverage(d, o.world);
    let rgb = to_target(texel.rgb * d.tint.rgb);
    if d.at.z == 2u {
        // Multiply: DstColor, InvSrcAlpha over premultiplied colour.
        return vec4(rgb * a, a);
    }
    return vec4(rgb, a);
}
"#;

/// `src` on the device: its BC mip chain where the device samples BC and
/// the blocks fit (whole 4×4 blocks at the top level), else the RGBA.
fn upload(device: &wgpu::Device, queue: &wgpu::Queue, src: &Source) -> wgpu::TextureView {
    let bc = src.bc.as_ref().filter(|c| {
        device
            .features()
            .contains(wgpu::Features::TEXTURE_COMPRESSION_BC)
            && c.width % 4 == 0
            && c.height % 4 == 0
            && !c.mips.is_empty()
    });
    let format = bc.and_then(|c| match c.block {
        Block::Bc1 => Some(wgpu::TextureFormat::Bc1RgbaUnorm),
        Block::Bc2 => Some(wgpu::TextureFormat::Bc2RgbaUnorm),
        Block::Bc3 => Some(wgpu::TextureFormat::Bc3RgbaUnorm),
        Block::Bc7 => Some(wgpu::TextureFormat::Bc7RgbaUnorm),
        // One- and two-channel blocks decode to grey or red-green, as the
        // RGBA does; draw that.
        Block::Bc4 | Block::Bc5 => None,
    });
    let (Some(c), Some(format)) = (bc, format) else {
        return rgba(device, Some(queue), &src.rgba);
    };
    let texture = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("ui image"),
        size: wgpu::Extent3d {
            width: c.width,
            height: c.height,
            depth_or_array_layers: 1,
        },
        mip_level_count: c.mips.len() as u32,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::COPY_DST | wgpu::TextureUsages::TEXTURE_BINDING,
        view_formats: &[],
    });
    for (level, data) in c.mips.iter().enumerate() {
        let (w, h) = ((c.width >> level).max(1), (c.height >> level).max(1));
        // Copies cover whole blocks; mips under 4 texels are one block.
        let (bw, bh) = (w.div_ceil(4), h.div_ceil(4));
        queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &texture,
                mip_level: level as u32,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            data,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(bw * c.block.bytes() as u32),
                rows_per_image: Some(bh),
            },
            wgpu::Extent3d {
                width: bw * 4,
                height: bh * 4,
                depth_or_array_layers: 1,
            },
        );
    }
    texture.create_view(&Default::default())
}

fn rgba(device: &wgpu::Device, queue: Option<&wgpu::Queue>, t: &Texture) -> wgpu::TextureView {
    let size = wgpu::Extent3d {
        width: t.width,
        height: t.height,
        depth_or_array_layers: 1,
    };
    let texture = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("ui image"),
        size,
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8Unorm,
        usage: wgpu::TextureUsages::COPY_DST | wgpu::TextureUsages::TEXTURE_BINDING,
        view_formats: &[],
    });
    if let Some(queue) = queue {
        queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            &t.rgba,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(t.width * 4),
                rows_per_image: Some(t.height),
            },
            size,
        );
    }
    texture.create_view(&Default::default())
}

// ---- What the canvas draws, as it renders ----

/// A label as the canvas placed it: the box GPUI's elements drew it in, in
/// logical pixels from the canvas pane's corner (`canvas::render`).
#[derive(Clone, Debug)]
pub struct Label {
    pub at: Point<Pixels>,
    pub size: [f32; 2],
    /// The box of the mask chain it draws inside (`Node::clip`).
    pub clip: Option<(Point<Pixels>, [f32; 2])>,
    pub text: SharedString,
    pub font: Option<SharedString>,
    /// In logical pixels; no line height takes the editor text style's.
    pub font_size: f32,
    pub line_height: Option<f32>,
    pub color: [u8; 3],
    /// The drop shadow under it: colour, alpha, offset in logical pixels.
    pub shadow: Option<([u8; 3], f32, [f32; 2])>,
    /// AlignmentX and AlignmentY, HorizontalOverflow (`Node`).
    pub align: [u32; 2],
    pub overflow: u32,
    pub opacity: f32,
}

/// A solid box: an image without its texture, the page under the screens.
#[derive(Clone, Debug)]
pub struct Fill {
    pub at: Point<Pixels>,
    pub size: [f32; 2],
    pub clip: Option<(Point<Pixels>, [f32; 2])>,
    /// Straight alpha, opacity included.
    pub color: Rgba,
    /// A border inside the box: width in logical pixels, and colour.
    pub border: Option<(f32, Rgba)>,
}

enum Item {
    Sprite(Arc<Sprite>),
    Label(Label),
    Fill(Fill),
}

/// What the canvas draws on the GPU, in paint order.
pub struct Scene {
    items: Vec<Item>,
    /// Design point (0, 0) in logical pixels from the pane's corner, and
    /// logical pixels per design pixel.
    origin: [f32; 2],
    zoom: f32,
}

impl Scene {
    pub fn new(origin: [f32; 2], zoom: f32) -> Scene {
        Scene {
            items: Vec::new(),
            origin,
            zoom,
        }
    }

    pub fn sprite(&mut self, sprite: Arc<Sprite>) {
        self.items.push(Item::Sprite(sprite));
    }

    pub fn label(&mut self, label: Label) {
        self.items.push(Item::Label(label));
    }

    pub fn fill(&mut self, fill: Fill) {
        self.items.push(Item::Fill(fill));
    }
}

// ---- The frame the thread records, in texture pixels ----

/// One frame of the canvas texture: images, then glyphs and boxes
/// ("marks"), drawn in `order`.
struct Frame {
    size: [u32; 2],
    /// The window's size and the texture's corner in it, in device
    /// pixels: images rasterize in window coordinates, through a viewport
    /// moved by the corner, so their edges and filtering land on the very
    /// values they did when they drew into the window.
    window: [u32; 2],
    corner: [i32; 2],
    /// Design pixels to window device pixels: scale, offset.
    xf: [f32; 4],
    sprites: Vec<Arc<Sprite>>,
    marks: Vec<Mark>,
    /// Runs of sprites (true) or marks, each to its end in its own list.
    order: Vec<(bool, u32)>,
}

impl PartialEq for Frame {
    fn eq(&self, o: &Frame) -> bool {
        self.size == o.size
            && self.window == o.window
            && self.corner == o.corner
            && self.xf == o.xf
            && self.order == o.order
            && self.marks == o.marks
            && self.sprites.len() == o.sprites.len()
            && (self.sprites.iter().zip(&o.sprites)).all(|(a, b)| Arc::ptr_eq(a, b))
    }
}

/// A glyph or a solid box, in texture pixels.
#[derive(Clone, PartialEq)]
struct Mark {
    /// The glyph, hung from `at` by its raster bounds as GPUI hangs it;
    /// none: a box from `at`, `size` across.
    glyph: Option<gpui::RenderGlyphParams>,
    at: [i32; 2],
    size: [i32; 2],
    /// Drawn only inside: x0, y0, x1, y1.
    clip: [i32; 4],
    /// Straight alpha, as GPUI's sprite shaders take it.
    color: [f32; 4],
}

impl Frame {
    fn push(&mut self, sprites: bool) {
        let end = if sprites {
            self.sprites.len()
        } else {
            self.marks.len()
        } as u32;
        match self.order.last_mut() {
            Some((kind, at)) if *kind == sprites => *at = end,
            _ => self.order.push((sprites, end)),
        }
    }
}

/// `v` rounded to a whole number, halves toward zero, as GPUI snaps.
fn round_half_toward_zero(v: f32) -> f32 {
    (v.abs() - 0.5).ceil().copysign(v)
}

fn intersect(a: [i32; 4], b: [i32; 4]) -> [i32; 4] {
    [
        a[0].max(b[0]),
        a[1].max(b[1]),
        a[2].min(b[2]),
        a[3].min(b[3]),
    ]
}

/// Lays labels and boxes out as GPUI laid out the elements the canvas drew
/// them with (`canvas::render`), in window device pixels: authored lengths
/// round to device pixels before layout, measured text rounds up, flex
/// centring adds fractions, and each box's absolute edges round, halves
/// toward zero (gpui's taffy.rs). Masks cover whole pixels, as GPUI clips.
struct Placer {
    sf: f32,
    /// The pane's corner and the texture's, in window device pixels.
    pane: [f32; 2],
    texture: [i32; 2],
    /// What the canvas shows, in window device pixels: the page within
    /// the pane, as GPUI clips the canvas.
    page: [i32; 4],
}

impl Placer {
    /// An authored length in device pixels.
    fn len(&self, v: f32) -> f32 {
        round_half_toward_zero(v * self.sf)
    }

    /// The box at `at` of `size` (logical, from the pane's corner), inside
    /// the clip box: its corner in device pixels, and the mask so far.
    fn place(
        &self,
        at: Point<Pixels>,
        clip: Option<(Point<Pixels>, [f32; 2])>,
    ) -> ([f32; 2], [i32; 4]) {
        let p = [f32::from(at.x), f32::from(at.y)];
        match clip {
            Some((q, [w, h])) => {
                let q = [f32::from(q.x), f32::from(q.y)];
                let c = [self.pane[0] + self.len(q[0]), self.pane[1] + self.len(q[1])];
                let mask = self.cover([c[0], c[1], c[0] + self.len(w), c[1] + self.len(h)]);
                let el = [c[0] + self.len(p[0] - q[0]), c[1] + self.len(p[1] - q[1])];
                (el, intersect(self.page, mask))
            }
            None => (
                [self.pane[0] + self.len(p[0]), self.pane[1] + self.len(p[1])],
                self.page,
            ),
        }
    }

    /// The device pixels a box's bounds cover, as `Window` turns a content
    /// mask (logical) into a clip: floored near edges, ceiled far ones.
    fn cover(&self, r: [f32; 4]) -> [i32; 4] {
        let sf = self.sf;
        [
            ((r[0] / sf) * sf).floor() as i32,
            ((r[1] / sf) * sf).floor() as i32,
            ((r[2] / sf) * sf).ceil() as i32,
            ((r[3] / sf) * sf).ceil() as i32,
        ]
    }

    /// `r` in window device pixels, moved into the texture.
    fn local(&self, r: [i32; 4]) -> [i32; 4] {
        let [x, y] = self.texture;
        [r[0] - x, r[1] - y, r[2] - x, r[3] - y]
    }
}

/// Straight-alpha RGBA as GPUI's shaders turn its colour back from HSLA.
fn straight(color: Hsla) -> [f32; 4] {
    let c = Rgba::from(color);
    [c.r, c.g, c.b, c.a]
}

impl Frame {
    /// A label's glyphs, laid out as its elements were: the label box, its
    /// drop shadow's layer and then its own, each a flex row that aligns
    /// one text element (or a column of them, one a line, aligned per
    /// line) without wrapping.
    fn label(&mut self, l: &Label, at: &Placer, style: &gpui::TextStyle, window: &Window) {
        let sf = at.sf;
        let (el, mut mask) = at.place(l.at, l.clip);
        let size = [at.len(l.size[0]), at.len(l.size[1])];
        // HorizontalOverflow 2-4 hide what runs out of the box. 4 asks for
        // an ellipsis, but GPUI's text element in a flex row never cut the
        // line short (the row sizes it whole), so it is clipped like 2 and 3.
        // TODO(alex): end a cut line with "…" as the game does.
        let hidden = matches!(l.overflow, 2..=4);
        if hidden {
            mask = intersect(
                mask,
                at.cover([el[0], el[1], el[0] + size[0], el[1] + size[1]]),
            );
        }
        let mut font = style.font();
        if let Some(family) = &l.font {
            font.family = family.clone();
        }
        let font_size = px(l.font_size);
        let line_height = match l.line_height {
            Some(h) => px(h),
            None => (style.line_height).to_pixels(font_size.into(), window.rem_size()),
        };
        let line_height = window.pixel_snap(line_height);
        let multi = l.text.contains('\n');
        let lines: Vec<SharedString> = if multi {
            l.text
                .lines()
                .map(|t| SharedString::from(t.to_owned()))
                .collect()
        } else {
            vec![l.text.clone()]
        };
        let shaped: Vec<gpui::ShapedLine> = (lines.iter())
            .map(|t| {
                let run = gpui::TextRun {
                    len: t.len(),
                    font: font.clone(),
                    color: Hsla::default(),
                    background_color: None,
                    underline: None,
                    strikethrough: None,
                };
                window
                    .text_system()
                    .shape_line(t.clone(), font_size, &[run], None)
            })
            .collect();
        // Each text element measures to its line's width rounded up to a
        // logical and then a device pixel, one line high.
        let widths: Vec<f32> = (shaped.iter())
            .map(|s| (f32::from(s.width.max(px(0.)).ceil()) * sf).ceil())
            .collect();
        let line_dev = (f32::from(line_height) * sf).ceil();
        let item = [
            widths.iter().copied().fold(0., f32::max),
            line_dev * shaped.len() as f32,
        ];
        let across = |free: f32, align: u32| match align {
            1 => free / 2.,
            2 => free,
            _ => 0.,
        };
        let down = |free: f32| match l.align[1] {
            1 => free / 2.,
            2 | 3 => free,
            _ => 0.,
        };
        let rgba = |[r, g, b]: [u8; 3], alpha: f32| -> Hsla {
            let a = (alpha.clamp(0., 1.) * 255.) as u32;
            gpui::rgba(u32::from(r) << 24 | u32::from(g) << 16 | u32::from(b) << 8 | a).into()
        };
        let main = (rgba(l.color, 1.), [0., 0.]);
        let shadow = (l.shadow).map(|(c, a, [x, y])| (rgba(c, a), [at.len(x), at.len(y)]));
        for (color, offset) in shadow.into_iter().chain([main]) {
            let layer = [el[0] + offset[0], el[1] + offset[1]];
            let mut mask = mask;
            if hidden {
                let x = at.cover([layer[0], 0., layer[0] + size[0], 0.]);
                mask[0] = mask[0].max(x[0]);
                mask[2] = mask[2].min(x[2]);
            }
            let clip = at.local(mask);
            if clip[0] >= clip[2] || clip[1] >= clip[3] {
                continue;
            }
            let drawn = straight(color.opacity(l.opacity));
            let corner = [
                layer[0] + across(size[0] - item[0], l.align[0]),
                layer[1] + down(size[1] - item[1]),
            ];
            for (k, line) in shaped.iter().enumerate() {
                // A line of several: placed in the column, aligned as the
                // label aligns across.
                let raw = match multi {
                    true => [
                        corner[0] + across(item[0] - widths[k], l.align[0]),
                        corner[1] + line_dev * k as f32,
                    ],
                    false => corner,
                };
                let origin = raw.map(|v| round_half_toward_zero(v) / sf);
                let padding = (line_height - line.ascent - line.descent) / 2.;
                let baseline = px(origin[1]) + padding + line.ascent;
                // As GPUI's paint_line steps from glyph to glyph.
                let mut x = px(origin[0]);
                let mut prev = px(0.);
                for run in &line.runs {
                    for glyph in &run.glyphs {
                        x += glyph.position.x - prev;
                        prev = glyph.position.x;
                        // TODO(alex): emoji draw as GPUI's colour sprites;
                        // no game label has one.
                        if glyph.is_emoji {
                            continue;
                        }
                        let origin = point(x, baseline + glyph.position.y);
                        let (params, hang) =
                            window.glyph_raster(origin, run.font_id, glyph.id, font_size, color);
                        let hang = [hang.x.0 as i32, hang.y.0 as i32];
                        self.marks.push(Mark {
                            glyph: Some(params),
                            at: [hang[0] - at.texture[0], hang[1] - at.texture[1]],
                            size: [0, 0],
                            clip,
                            color: drawn,
                        });
                    }
                }
            }
        }
        self.push(false);
    }

    /// A box, and the border inside it, as GPUI draws a quad: snapped to
    /// device pixels, the border at least one pixel.
    fn fill(&mut self, f: &Fill, at: &Placer) {
        let (el, mask) = at.place(f.at, f.clip);
        let r = [
            el[0] as i32,
            el[1] as i32,
            (el[0] + at.len(f.size[0])) as i32,
            (el[1] + at.len(f.size[1])) as i32,
        ];
        let clip = at.local(mask);
        let mut push = |r: [i32; 4], c: Rgba| {
            let r = at.local(r);
            self.marks.push(Mark {
                glyph: None,
                at: [r[0], r[1]],
                size: [r[2] - r[0], r[3] - r[1]],
                clip,
                color: [c.r, c.g, c.b, c.a],
            })
        };
        match f.border {
            Some((width, border)) => {
                let w = if width == 0. {
                    0
                } else {
                    (round_half_toward_zero(width * at.sf) as i32).max(1)
                };
                push(r, border);
                push([r[0] + w, r[1] + w, r[2] - w, r[3] - w], f.color);
            }
            None => push(r, f.color),
        }
        self.push(false);
    }
}

// ---- The texture, and the thread that records it ----

/// How long after a frame asks for the canvas GPUI waits for it at most;
/// then it shows the last one and asks again. A moving canvas records in
/// about a millisecond; a screen's first frame uploads its textures.
const DEADLINE: std::time::Duration = std::time::Duration::from_millis(10);

/// The format the canvas draws in; GPUI draws the texture into its own.
const FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;

/// The canvas's GPU side: the texture GPUI shows and the thread that draws
/// it. One per UI editor; clones share it.
#[derive(Clone, Default)]
pub struct Renderer(Arc<Pane>);

impl std::fmt::Debug for Renderer {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str("ui::gpu::Renderer")
    }
}

struct Pane {
    handoff: Arc<gpui::TextureHandoff>,
    /// The newest frame painted, for the thread.
    frame: Arc<Mutex<Option<Arc<Frame>>>>,
    thread: Mutex<Option<std::thread::JoinHandle<()>>>,
}

impl Default for Pane {
    fn default() -> Self {
        Self {
            handoff: Arc::new(gpui::TextureHandoff::new(DEADLINE)),
            frame: Arc::default(),
            thread: Mutex::default(),
        }
    }
}

impl Drop for Pane {
    /// Ends the thread. It may be recording; exit joins it rather than the
    /// UI thread waiting here.
    fn drop(&mut self) {
        self.handoff.close();
        let thread = self.thread.get_mut().unwrap_or_else(|e| e.into_inner());
        if let Some(thread) = thread.take() {
            gpui_editor_kit::gpu::join_at_exit(thread);
        }
    }
}

impl Renderer {
    /// An element that paints `scene` (see `paint`); place it where the
    /// canvas's widgets go, at the pane's corner.
    pub fn element(&self, scene: Scene) -> gpui::AnyElement {
        use gpui::{IntoElement, Styled};
        let renderer = self.clone();
        gpui::canvas(
            |_, _, _| {},
            move |bounds, _, window, cx| renderer.paint(scene, bounds, window, cx),
        )
        .absolute()
        .top_0()
        .left_0()
        .size_full()
        .into_any_element()
    }

    /// Paints `scene` as the canvas texture, over what the canvas shows:
    /// its content mask, the page within the pane. `pane`: the pane's
    /// bounds. Lays the labels out and hands the frame to the thread,
    /// which records it while GPUI paints the rest of the window.
    pub fn paint(&self, scene: Scene, pane: Bounds<Pixels>, window: &mut Window, cx: &mut App) {
        let _span = profile::span("ui canvas paint");
        let sf = window.scale_factor();
        let mask = window.content_mask().bounds;
        let view = window.viewport_size();
        let page = [
            (f32::from(mask.origin.x) * sf).floor().max(0.) as i32,
            (f32::from(mask.origin.y) * sf).floor().max(0.) as i32,
            (f32::from(mask.origin.x + mask.size.width) * sf)
                .ceil()
                .min((f32::from(view.width) * sf).round()) as i32,
            (f32::from(mask.origin.y + mask.size.height) * sf)
                .ceil()
                .min((f32::from(view.height) * sf).round()) as i32,
        ];
        if page[2] <= page[0] || page[3] <= page[1] {
            return;
        }
        // On the window's own grid of 64 pixels: the GPU shades pixels in
        // 2×2 quads and picks mips from their differences, so quads grouped
        // otherwise sampled other mips along images' edges; and rasterizers
        // work in tiles, whose origin moved interpolation by a rounding.
        let texture = [page[0] & !63, page[1] & !63];
        let at = Placer {
            sf,
            pane: [
                (f32::from(pane.origin.x) * sf).round(),
                (f32::from(pane.origin.y) * sf).round(),
            ],
            texture,
            page,
        };
        let size = [(page[2] - texture[0]) as u32, (page[3] - texture[1]) as u32];
        let shift = |o: Pixels, d: f32| (f32::from(o) + d) * sf;
        let mut frame = Frame {
            size,
            window: [
                (f32::from(view.width) * sf).round() as u32,
                (f32::from(view.height) * sf).round() as u32,
            ],
            corner: texture,
            xf: [
                scene.zoom * sf,
                scene.zoom * sf,
                shift(pane.origin.x, scene.origin[0]),
                shift(pane.origin.y, scene.origin[1]),
            ],
            sprites: Vec::new(),
            marks: Vec::new(),
            order: Vec::new(),
        };
        let style = window.text_style();
        for item in scene.items {
            match item {
                Item::Sprite(sprite) => {
                    frame.sprites.push(sprite);
                    frame.push(true);
                }
                Item::Label(label) => frame.label(&label, &at, &style, window),
                Item::Fill(fill) => frame.fill(&fill, &at),
            }
        }
        let pane = &self.0;
        let fresh = {
            let mut slot = pane.frame.lock().unwrap_or_else(|e| e.into_inner());
            let fresh = slot.as_deref() != Some(&frame);
            if fresh {
                *slot = Some(Arc::new(frame));
            }
            fresh
        };
        let mut thread = pane.thread.lock().unwrap_or_else(|e| e.into_inner());
        if thread.is_none() {
            let (handoff, slot) = (pane.handoff.clone(), pane.frame.clone());
            let text = cx.text_system().clone();
            *thread = (std::thread::Builder::new().name("ui canvas".into()))
                .spawn(move || produce(&handoff, &slot, &text))
                .ok();
        }
        drop(thread);
        let bounds = Bounds::new(
            point(px(texture[0] as f32 / sf), px(texture[1] as f32 / sf)),
            gpui::size(px(size[0] as f32 / sf), px(size[1] as f32 / sf)),
        );
        window.paint_gpu_texture(bounds, pane.handoff.clone());
        // Painted after this frame's ask went out: ask for what changed.
        if fresh {
            pane.handoff.ask();
        }
    }
}

/// The canvas thread: records each frame a window asks for into the
/// texture, until the canvas is dropped. A frame the texture already shows
/// is not recorded again.
fn produce(
    handoff: &gpui::TextureHandoff,
    slot: &Mutex<Option<Arc<Frame>>>,
    text: &gpui::TextSystem,
) {
    let mut held = OnDevice::<Device>::default();
    let mut shown: Option<Arc<Frame>> = None;
    // A device a frame failed on, lost under it: no more frames there.
    let mut failed: Option<wgpu::Device> = None;
    while let Some((asked, gpu)) = handoff.wait() {
        // Exiting: a compile or submit must not run under the driver's
        // teardown.
        let Some(_busy) = gpui_editor_kit::gpu::busy() else {
            break;
        };
        let frame = slot.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let same = (shown.as_ref().zip(frame.as_ref())).is_some_and(|(a, b)| Arc::ptr_eq(a, b))
            && held.get(&gpu.device).is_some();
        let recorded = match &frame {
            Some(frame) if !same && failed.as_ref() != Some(&gpu.device) => {
                let recorded = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    let device = held.get_or_insert_with(&gpu.device, || Device::new(&gpu.device));
                    Some(device.record(&gpu, frame, text))
                }));
                recorded.unwrap_or_else(|_| {
                    eprintln!(
                        "ui canvas: a frame failed, as on a lost device; no more frames on it"
                    );
                    failed = Some(gpu.device.clone());
                    None
                })
            }
            _ => None,
        };
        let mut submitted = false;
        handoff.submit(asked, || {
            let (commands, texture) = recorded?;
            gpu.queue.submit([commands]);
            submitted = true;
            Some(texture)
        });
        if submitted {
            shown = frame;
            if let Some(timer) = held.get_mut(&gpu.device).and_then(|d| d.timer.as_mut()) {
                timer.read_back();
            }
        }
    }
}

/// GPU resources for one texture the canvas drew.
struct Uploaded {
    /// Keeps the address the cache is keyed by from being reused.
    _keep: Arc<Source>,
    view: wgpu::TextureView,
    used: u64,
}

/// Frames recorded a texture or bind group may go unused before it is
/// freed: some seconds of a canvas being moved.
const KEEP_FRAMES: u64 = 600;
/// Frames between sweeps for textures and bind groups past `KEEP_FRAMES`.
const SWEEP_EVERY: u64 = 64;
/// The glyph atlas's side: 4 MiB of coverage, a few thousand glyphs.
const ATLAS: u32 = 2048;

/// Where a glyph is in the atlas, and its raster bounds: the offset from
/// the pixel it hangs from, and its size.
#[derive(Clone, Copy)]
struct Tile {
    at: [u32; 2],
    offset: [i32; 2],
    size: [u32; 2],
}

/// The glyphs the canvas drew, rasterized once. Shelves of glyphs of
/// about one height; when it is full it starts over.
struct Atlas {
    texture: wgpu::Texture,
    view: wgpu::TextureView,
    /// Top, height and the next free x of each shelf.
    shelves: Vec<[u32; 3]>,
    top: u32,
    /// None: a glyph with nothing to draw (a space).
    glyphs: foldhash::HashMap<gpui::RenderGlyphParams, Option<Tile>>,
}

impl Atlas {
    fn new(device: &wgpu::Device) -> Atlas {
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("ui canvas glyphs"),
            size: wgpu::Extent3d {
                width: ATLAS,
                height: ATLAS,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::R8Unorm,
            usage: wgpu::TextureUsages::COPY_DST | wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        });
        Atlas {
            view: texture.create_view(&Default::default()),
            texture,
            shelves: Vec::new(),
            top: 0,
            glyphs: Default::default(),
        }
    }

    fn clear(&mut self) {
        self.shelves.clear();
        self.top = 0;
        self.glyphs.clear();
    }

    /// Room for `w` by `h`: on a shelf no more than a quarter taller, or a
    /// new one.
    fn room(&mut self, w: u32, h: u32) -> Option<[u32; 2]> {
        if w > ATLAS {
            return None;
        }
        let fits = |s: &[u32; 3]| s[1] >= h && s[1] <= h + h / 4 + 1 && s[2] + w <= ATLAS;
        if let Some(shelf) = self.shelves.iter_mut().find(|s| fits(s)) {
            let at = [shelf[2], shelf[0]];
            shelf[2] += w + 1;
            return Some(at);
        }
        if self.top + h > ATLAS {
            return None;
        }
        self.shelves.push([self.top, h, w + 1]);
        self.top += h + 1;
        Some([0, self.top - h - 1])
    }

    /// The glyph's tile, rasterizing it on first sight; None for nothing
    /// to draw, Err when the atlas is full.
    fn tile(
        &mut self,
        params: &gpui::RenderGlyphParams,
        text: &gpui::TextSystem,
        queue: &wgpu::Queue,
    ) -> Result<Option<Tile>, ()> {
        if let Some(tile) = self.glyphs.get(params) {
            return Ok(*tile);
        }
        let raster = (text.raster_bounds(params).ok())
            .filter(|b| b.size.width.0 > 0 && b.size.height.0 > 0)
            .and_then(|b| Some((b, text.rasterize_glyph(params).ok()?)));
        let Some((bounds, (size, bytes))) = raster else {
            self.glyphs.insert(params.clone(), None);
            return Ok(None);
        };
        let [w, h] = [size.width.0 as u32, size.height.0 as u32];
        if bytes.len() < (w * h) as usize {
            self.glyphs.insert(params.clone(), None);
            return Ok(None);
        }
        let at = self.room(w, h).ok_or(())?;
        queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &self.texture,
                mip_level: 0,
                origin: wgpu::Origin3d {
                    x: at[0],
                    y: at[1],
                    z: 0,
                },
                aspect: wgpu::TextureAspect::All,
            },
            &bytes,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(w),
                rows_per_image: Some(h),
            },
            wgpu::Extent3d {
                width: w,
                height: h,
                depth_or_array_layers: 1,
            },
        );
        let tile = Tile {
            at,
            offset: [bounds.origin.x.0, bounds.origin.y.0],
            size: [w, h],
        };
        self.glyphs.insert(params.clone(), Some(tile));
        Ok(Some(tile))
    }
}

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct GpuMark {
    // x0, y0, x1, y1 in texture pixels, clipped
    rect: [i32; 4],
    // atlas texel of rect's corner, solid?, 0
    tile: [i32; 4],
    color: [f32; 4],
}

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct MarkView {
    // target size, grayscale enhanced contrast, 0
    size: [f32; 4],
    gamma: [f32; 4],
}

/// Glyphs and boxes: coverage from the atlas, corrected as GPUI's
/// monochrome sprite shader corrects it (shaders.wgsl), blended over.
const MARKS: &str = r#"
struct View { size: vec4<f32>, gamma: vec4<f32> };
struct Mark { rect: vec4<i32>, tile: vec4<i32>, color: vec4<f32> };

@group(0) @binding(0) var<uniform> view: View;
@group(0) @binding(1) var<storage, read> marks: array<Mark>;
@group(0) @binding(2) var atlas: texture_2d<f32>;

struct Out {
    @builtin(position) pos: vec4<f32>,
    @location(0) @interpolate(flat) mark: u32,
};

@vertex
fn vs(@builtin(vertex_index) v: u32, @builtin(instance_index) m: u32) -> Out {
    let r = vec4<f32>(marks[m].rect);
    let corner = array(vec2(0., 0.), vec2(1., 0.), vec2(0., 1.), vec2(1., 0.), vec2(1., 1.), vec2(0., 1.))[v];
    let p = mix(r.xy, r.zw, corner);
    var o: Out;
    o.pos = vec4(p.x / view.size.x * 2. - 1., 1. - p.y / view.size.y * 2., 0., 1.);
    o.mark = m;
    return o;
}

fn color_brightness(color: vec3<f32>) -> f32 {
    return dot(color, vec3<f32>(0.30, 0.59, 0.11));
}

fn light_on_dark_contrast(k: f32, color: vec3<f32>) -> f32 {
    return k * saturate(4.0 * (0.75 - color_brightness(color)));
}

fn enhance_contrast(alpha: f32, k: f32) -> f32 {
    return alpha * (k + 1.0) / (alpha * k + 1.0);
}

fn apply_alpha_correction(a: f32, b: f32, g: vec4<f32>) -> f32 {
    let brightness_adjustment = g.x * b + g.y;
    let correction = brightness_adjustment * a + (g.z * b + g.w);
    return a + a * (1.0 - a) * correction;
}

@fragment
fn fs(o: Out) -> @location(0) vec4<f32> {
    let m = marks[o.mark];
    var coverage = 1.;
    if m.tile.z == 0 {
        let texel = m.tile.xy + vec2<i32>(floor(o.pos.xy)) - m.rect.xy;
        let sample = textureLoad(atlas, texel, 0).r;
        let k = light_on_dark_contrast(view.size.z, m.color.rgb);
        coverage = apply_alpha_correction(enhance_contrast(sample, k), color_brightness(m.color.rgb), view.gamma);
    }
    return vec4(m.color.rgb, m.color.a * coverage);
}
"#;

/// Timestamps around the canvas's pass, read back as `profile` spans
/// ("ui canvas gpu") when profiling is on.
struct Timer {
    set: wgpu::QuerySet,
    resolve: wgpu::Buffer,
    read: wgpu::Buffer,
    /// 0 free, 1 recorded, 2 mapping, 3 mapped.
    state: Arc<std::sync::atomic::AtomicU8>,
    period: f32,
}

impl Timer {
    fn new(device: &wgpu::Device, queue: &wgpu::Queue) -> Option<Timer> {
        if !profile::enabled() || !device.features().contains(wgpu::Features::TIMESTAMP_QUERY) {
            return None;
        }
        let buffer = |usage| {
            device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("ui canvas timing"),
                size: 16,
                usage,
                mapped_at_creation: false,
            })
        };
        Some(Timer {
            set: device.create_query_set(&wgpu::QuerySetDescriptor {
                label: Some("ui canvas timing"),
                ty: wgpu::QueryType::Timestamp,
                count: 2,
            }),
            resolve: buffer(wgpu::BufferUsages::QUERY_RESOLVE | wgpu::BufferUsages::COPY_SRC),
            read: buffer(wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST),
            state: Arc::default(),
            period: queue.get_timestamp_period(),
        })
    }

    /// Counts the last frame's time if it has come back; whether this
    /// frame can be timed.
    fn ready(&self) -> bool {
        use std::sync::atomic::Ordering;
        match self.state.load(Ordering::Acquire) {
            0 => true,
            3 => {
                let t: [u64; 2] = {
                    let range = self.read.slice(..).get_mapped_range();
                    range.map_or([0; 2], |r| bytemuck::pod_read_unaligned(&r[..16]))
                };
                self.read.unmap();
                let ns = t[1].saturating_sub(t[0]) as f64 * f64::from(self.period);
                profile::add("ui canvas gpu", std::time::Duration::from_nanos(ns as u64));
                self.state.store(0, Ordering::Release);
                true
            }
            _ => false,
        }
    }

    /// After the frame's submit: maps what it resolved.
    fn read_back(&mut self) {
        use std::sync::atomic::Ordering;
        if self.state.load(Ordering::Acquire) != 1 {
            return;
        }
        self.state.store(2, Ordering::Release);
        let state = self.state.clone();
        self.read
            .slice(..)
            .map_async(wgpu::MapMode::Read, move |r| {
                state.store(if r.is_ok() { 3 } else { 0 }, Ordering::Release);
            });
    }
}

/// What the canvas thread keeps on one device.
struct Device {
    pipelines: [wgpu::RenderPipeline; 3],
    frame_layout: wgpu::BindGroupLayout,
    texture_layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    blank: wgpu::TextureView,
    mark_pipeline: wgpu::RenderPipeline,
    marks_layout: wgpu::BindGroupLayout,
    atlas: Atlas,
    textures: foldhash::HashMap<usize, Uploaded>,
    groups: foldhash::HashMap<[usize; 1 + MAX_MASKS], (wgpu::BindGroup, u64)>,
    /// The texture GPUI shows, its size rounded up to 64 pixels so a
    /// dock edge being dragged does not make one each frame.
    output: Option<(wgpu::TextureView, [u32; 2])>,
    timer: Option<Timer>,
    /// Frames recorded: the clock textures and bind groups age by.
    frames: u64,
}

impl Device {
    fn new(device: &wgpu::Device) -> Device {
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("ui canvas"),
            source: wgpu::ShaderSource::Wgsl(SHADER.into()),
        });
        let entry = |binding, visibility, ty| wgpu::BindGroupLayoutEntry {
            binding,
            visibility,
            ty,
            count: None,
        };
        let both = wgpu::ShaderStages::VERTEX_FRAGMENT;
        let storage = wgpu::BindingType::Buffer {
            ty: wgpu::BufferBindingType::Storage { read_only: true },
            has_dynamic_offset: false,
            min_binding_size: None,
        };
        let uniform = wgpu::BindingType::Buffer {
            ty: wgpu::BufferBindingType::Uniform,
            has_dynamic_offset: false,
            min_binding_size: None,
        };
        let frame_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("ui canvas frame"),
            entries: &[
                entry(0, both, uniform),
                entry(1, both, storage),
                entry(2, both, storage),
                entry(
                    3,
                    wgpu::ShaderStages::FRAGMENT,
                    wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                ),
            ],
        });
        let texture = wgpu::BindingType::Texture {
            sample_type: wgpu::TextureSampleType::Float { filterable: true },
            view_dimension: wgpu::TextureViewDimension::D2,
            multisampled: false,
        };
        let entries: Vec<_> = (0..=MAX_MASKS as u32)
            .map(|b| entry(b, wgpu::ShaderStages::FRAGMENT, texture))
            .collect();
        let texture_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("ui canvas textures"),
            entries: &entries,
        });
        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("ui canvas"),
            bind_group_layouts: &[Some(&frame_layout), Some(&texture_layout)],
            immediate_size: 0,
        });
        let keep_alpha = wgpu::BlendComponent {
            src_factor: wgpu::BlendFactor::Zero,
            dst_factor: wgpu::BlendFactor::One,
            operation: wgpu::BlendOperation::Add,
        };
        // phoenix_gui.fx state blocks: Standard, Add (SrcAlpha, One),
        // Multiply (DstColor, InvSrcAlpha, the shader premultiplying). The
        // texture's alpha composites "over", so the page stays opaque and
        // GPUI shows what was drawn on it unchanged.
        let blends = [
            wgpu::BlendState::ALPHA_BLENDING,
            wgpu::BlendState {
                color: wgpu::BlendComponent {
                    src_factor: wgpu::BlendFactor::SrcAlpha,
                    dst_factor: wgpu::BlendFactor::One,
                    operation: wgpu::BlendOperation::Add,
                },
                alpha: keep_alpha,
            },
            wgpu::BlendState {
                color: wgpu::BlendComponent {
                    src_factor: wgpu::BlendFactor::Dst,
                    dst_factor: wgpu::BlendFactor::OneMinusSrcAlpha,
                    operation: wgpu::BlendOperation::Add,
                },
                alpha: keep_alpha,
            },
        ];
        let pipeline = |label, layout: &wgpu::PipelineLayout, module, blend| {
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some(label),
                layout: Some(layout),
                vertex: wgpu::VertexState {
                    module,
                    entry_point: Some("vs"),
                    buffers: &[],
                    compilation_options: Default::default(),
                },
                fragment: Some(wgpu::FragmentState {
                    module,
                    entry_point: Some("fs"),
                    targets: &[Some(wgpu::ColorTargetState {
                        format: FORMAT,
                        blend: Some(blend),
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                    compilation_options: Default::default(),
                }),
                primitive: wgpu::PrimitiveState::default(),
                depth_stencil: None,
                multisample: Default::default(),
                multiview_mask: None,
                cache: None,
            })
        };
        let pipelines = blends.map(|blend| pipeline("ui canvas", &layout, &module, blend));
        let marks_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("ui canvas marks"),
            entries: &[
                entry(0, both, uniform),
                entry(1, both, storage),
                entry(
                    2,
                    wgpu::ShaderStages::FRAGMENT,
                    wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: false },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                ),
            ],
        });
        let marks_module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("ui canvas marks"),
            source: wgpu::ShaderSource::Wgsl(MARKS.into()),
        });
        let mark_pipeline = pipeline(
            "ui canvas marks",
            &device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("ui canvas marks"),
                bind_group_layouts: &[Some(&marks_layout)],
                immediate_size: 0,
            }),
            &marks_module,
            wgpu::BlendState::ALPHA_BLENDING,
        );
        // Repeat: wrapping images wrap, clamped ones clamp in the shader.
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("ui canvas"),
            address_mode_u: wgpu::AddressMode::Repeat,
            address_mode_v: wgpu::AddressMode::Repeat,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            mipmap_filter: wgpu::MipmapFilterMode::Linear,
            ..Default::default()
        });
        // Unused texture slots; never sampled for what they show.
        let blank = rgba(
            device,
            None,
            &Texture::from_rgba(1, 1, vec![255; 4]).expect("a 1×1 texture"),
        );
        Device {
            pipelines,
            frame_layout,
            texture_layout,
            sampler,
            blank,
            mark_pipeline,
            marks_layout,
            atlas: Atlas::new(device),
            textures: Default::default(),
            groups: Default::default(),
            output: None,
            timer: None,
            frames: 0,
        }
    }

    /// Records `frame` into the texture GPUI shows: the commands to submit
    /// and the texture.
    fn record(
        &mut self,
        gpu: &gpui::HandoffGpu,
        frame: &Frame,
        text: &gpui::TextSystem,
    ) -> (wgpu::CommandBuffer, gpui::HandoffTexture) {
        let _span = profile::span("ui canvas record");
        let (device, queue) = (&gpu.device, &gpu.queue);
        self.frames += 1;
        let now = self.frames;
        if self.timer.is_none() && self.frames == 1 {
            self.timer = Timer::new(device, queue);
        }
        let room = frame.size.map(|v| v.max(1).next_multiple_of(64));
        if self.output.as_ref().is_none_or(|(_, size)| *size != room) {
            let texture = device.create_texture(&wgpu::TextureDescriptor {
                label: Some("ui canvas"),
                size: wgpu::Extent3d {
                    width: room[0],
                    height: room[1],
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: FORMAT,
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT
                    | wgpu::TextureUsages::TEXTURE_BINDING,
                view_formats: &[],
            });
            self.output = Some((texture.create_view(&Default::default()), room));
        }
        let marks = self.marks(frame, text, queue);
        let (draws, quads, groups) = self.sprites(frame, device, queue, now);
        let buffer = |label, usage, bytes: &[u8]| {
            let b = device.create_buffer(&wgpu::BufferDescriptor {
                label: Some(label),
                size: (bytes.len() as u64).max(16),
                usage: usage | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            queue.write_buffer(&b, 0, bytes);
            b
        };
        let view = GpuView {
            xf: frame.xf,
            target: [frame.window[0] as f32, frame.window[1] as f32, 0., 0.],
        };
        let view_buf = buffer(
            "ui canvas view",
            wgpu::BufferUsages::UNIFORM,
            bytemuck::bytes_of(&view),
        );
        let draw_buf = buffer(
            "ui canvas draws",
            wgpu::BufferUsages::STORAGE,
            bytemuck::cast_slice(&draws),
        );
        let quad_buf = buffer(
            "ui canvas quads",
            wgpu::BufferUsages::STORAGE,
            bytemuck::cast_slice(&quads),
        );
        let frame_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("ui canvas frame"),
            layout: &self.frame_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: view_buf.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: draw_buf.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: quad_buf.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 3,
                    resource: wgpu::BindingResource::Sampler(&self.sampler),
                },
            ],
        });
        let (gamma, contrast) = gpui::grayscale_glyph_correction();
        let mark_view = MarkView {
            size: [frame.size[0] as f32, frame.size[1] as f32, contrast, 0.],
            gamma,
        };
        let mark_view_buf = buffer(
            "ui canvas marks view",
            wgpu::BufferUsages::UNIFORM,
            bytemuck::bytes_of(&mark_view),
        );
        let mark_buf = buffer(
            "ui canvas marks",
            wgpu::BufferUsages::STORAGE,
            bytemuck::cast_slice(&marks.0),
        );
        let marks_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("ui canvas marks"),
            layout: &self.marks_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: mark_view_buf.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: mark_buf.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::TextureView(&self.atlas.view),
                },
            ],
        });
        let timer = self.timer.as_ref().filter(|t| t.ready());
        let (target, _) = self.output.as_ref().expect("made above");
        let mut encoder = device.create_command_encoder(&Default::default());
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("ui canvas"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: target,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                        store: wgpu::StoreOp::Store,
                    },
                    depth_slice: None,
                })],
                depth_stencil_attachment: None,
                timestamp_writes: timer.map(|t| wgpu::RenderPassTimestampWrites {
                    query_set: &t.set,
                    beginning_of_pass_write_index: Some(0),
                    end_of_pass_write_index: Some(1),
                }),
                occlusion_query_set: None,
                multiview_mask: None,
            });
            pass.set_scissor_rect(0, 0, frame.size[0], frame.size[1]);
            let [x, y] = frame.corner.map(|v| -(v as f32));
            let [w, h] = frame.window.map(|v| v as f32);
            let (mut sprite, mut mark) = (0u32, 0u32);
            for &(sprites, end) in &frame.order {
                if !sprites {
                    // Marks clipped away entirely were dropped; draw what
                    // is left of the run.
                    let (from, to) = (marks.1[mark as usize], marks.1[end as usize]);
                    if to > from {
                        let [w, h] = frame.size.map(|v| v as f32);
                        pass.set_viewport(0., 0., w, h, 0., 1.);
                        pass.set_pipeline(&self.mark_pipeline);
                        pass.set_bind_group(0, &marks_group, &[]);
                        pass.draw(0..6, from..to);
                    }
                    mark = end;
                    continue;
                }
                // Neighbours with one blend and one set of textures (a
                // list's slots, a bar's tiles) draw as one instanced call:
                // the shader finds each one's record by instance index.
                pass.set_viewport(x, y, w, h, 0., 1.);
                pass.set_bind_group(0, &frame_group, &[]);
                let mut i = sprite as usize;
                let end = end as usize;
                while i < end {
                    let kind = draws[i].at[2] as usize;
                    let next = (i + 1..end)
                        .find(|&j| draws[j].at[2] as usize != kind || groups[j] != groups[i])
                        .unwrap_or(end);
                    pass.set_pipeline(&self.pipelines[kind]);
                    pass.set_bind_group(1, &self.groups[&groups[i]].0, &[]);
                    pass.draw(0..6, i as u32..next as u32);
                    i = next;
                }
                sprite = end as u32;
            }
        }
        if let Some(t) = timer {
            encoder.resolve_query_set(&t.set, 0..2, &t.resolve, 0);
            encoder.copy_buffer_to_buffer(&t.resolve, 0, &t.read, 0, 16);
            t.state.store(1, std::sync::atomic::Ordering::Release);
        }
        // Now and then, not per frame: a sweep walks every cached texture.
        if now.is_multiple_of(SWEEP_EVERY) {
            self.textures.retain(|_, t| now - t.used < KEEP_FRAMES);
            let textures = &self.textures;
            self.groups.retain(|k, g| {
                now - g.1 < KEEP_FRAMES && k.iter().all(|&t| t == 0 || textures.contains_key(&t))
            });
        }
        let texture = gpui::HandoffTexture {
            device: device.clone(),
            view: target.clone(),
            size: frame.size,
        };
        (encoder.finish(), texture)
    }

    /// The frame's marks as drawn, clipped, and for each mark of the frame
    /// the index its drawn ones start at (one past the end at the end).
    /// Glyphs not seen before are rasterized into the atlas; when it is
    /// full it starts over once, and what still does not fit is left out.
    fn marks(
        &mut self,
        frame: &Frame,
        text: &gpui::TextSystem,
        queue: &wgpu::Queue,
    ) -> (Vec<GpuMark>, Vec<u32>) {
        let mut tiles = Vec::with_capacity(frame.marks.len());
        for round in 0..2 {
            tiles.clear();
            let mut full = false;
            for m in &frame.marks {
                tiles.push(match &m.glyph {
                    None => None,
                    Some(params) => match self.atlas.tile(params, text, queue) {
                        Ok(tile) => tile,
                        Err(()) => {
                            full = true;
                            None
                        }
                    },
                });
            }
            if !full {
                break;
            }
            if round == 0 {
                self.atlas.clear();
            } else {
                eprintln!("ui canvas: the frame's glyphs do not fit the atlas; some are left out");
            }
        }
        let mut out = Vec::with_capacity(frame.marks.len());
        let mut starts = Vec::with_capacity(frame.marks.len() + 1);
        for (m, tile) in frame.marks.iter().zip(&tiles) {
            starts.push(out.len() as u32);
            let (rect, tile) = match (&m.glyph, tile) {
                (None, _) => (
                    [m.at[0], m.at[1], m.at[0] + m.size[0], m.at[1] + m.size[1]],
                    [0, 0, 1, 0],
                ),
                (Some(_), Some(t)) => {
                    let at = [m.at[0] + t.offset[0], m.at[1] + t.offset[1]];
                    let rect = [
                        at[0],
                        at[1],
                        at[0] + t.size[0] as i32,
                        at[1] + t.size[1] as i32,
                    ];
                    (rect, [t.at[0] as i32 - at[0], t.at[1] as i32 - at[1], 0, 0])
                }
                (Some(_), None) => continue,
            };
            let clipped = intersect(rect, m.clip);
            if clipped[0] >= clipped[2] || clipped[1] >= clipped[3] {
                continue;
            }
            // The atlas texel of the clipped corner.
            let tile = [tile[0] + clipped[0], tile[1] + clipped[1], tile[2], tile[3]];
            out.push(GpuMark {
                rect: clipped,
                tile,
                color: m.color,
            });
        }
        starts.push(out.len() as u32);
        (out, starts)
    }

    /// The frame's sprites as draw records, quads and texture sets, with
    /// their textures uploaded on first sight.
    fn sprites(
        &mut self,
        frame: &Frame,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        now: u64,
    ) -> (Vec<GpuDraw>, Vec<GpuQuad>, Vec<[usize; 1 + MAX_MASKS]>) {
        let mut draws: Vec<GpuDraw> = Vec::with_capacity(frame.sprites.len());
        let mut quads: Vec<GpuQuad> = Vec::with_capacity(frame.sprites.len() * 2);
        let mut groups: Vec<[usize; 1 + MAX_MASKS]> = Vec::with_capacity(frame.sprites.len());
        for s in &frame.sprites {
            let first = quads.len() as u32;
            quads.push(GpuQuad::of(&s.quad));
            let masks = &s.masks[..s.masks.len().min(MAX_MASKS)];
            let mut key = [0usize; 1 + MAX_MASKS];
            key[0] = Arc::as_ptr(&s.texture) as usize;
            for (k, m) in masks.iter().enumerate() {
                let mut q = GpuQuad::of(&m.quad);
                q.flags[2] = f32::from(u8::from(m.texture.is_some()));
                q.flags[3] = f32::from(u8::from(m.invert));
                q.cover = [
                    m.alpha,
                    m.edge.0,
                    m.edge.1,
                    f32::from(u8::from(m.intersect)),
                ];
                q.level = [
                    m.level as f32,
                    f32::from(u8::from(m.soft)),
                    f32::from(u8::from(m.exclusive)),
                    0.,
                ];
                quads.push(q);
                key[1 + k] = m.texture.as_ref().map_or(0, |t| Arc::as_ptr(t) as usize);
            }
            let t = s.transform;
            draws.push(GpuDraw {
                t0: [t[0], t[1], t[2], t[3]],
                t1: [t[4], t[5], s.local[0], s.local[1]],
                t2: [s.local[2], s.local[3], 0., 0.],
                tint: [
                    f32::from(s.tint[0]) / 255.,
                    f32::from(s.tint[1]) / 255.,
                    f32::from(s.tint[2]) / 255.,
                    s.alpha,
                ],
                at: [first, masks.len() as u32, blend_kind(s.blend) as u32, 0],
            });
            let textures =
                std::iter::once(&s.texture).chain(masks.iter().filter_map(|m| m.texture.as_ref()));
            for t in textures {
                self.textures
                    .entry(Arc::as_ptr(t) as usize)
                    .or_insert_with(|| Uploaded {
                        _keep: t.clone(),
                        view: upload(device, queue, t),
                        used: now,
                    })
                    .used = now;
            }
            groups.push(key);
        }
        for key in &groups {
            if let Some(g) = self.groups.get_mut(key) {
                g.1 = now;
                continue;
            }
            let view = |k: usize| match k {
                0 => &self.blank,
                k => &self.textures[&k].view,
            };
            let entries: Vec<_> = key
                .iter()
                .enumerate()
                .map(|(b, &k)| wgpu::BindGroupEntry {
                    binding: b as u32,
                    resource: wgpu::BindingResource::TextureView(view(k)),
                })
                .collect();
            let group = device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("ui canvas textures"),
                layout: &self.texture_layout,
                entries: &entries,
            });
            self.groups.insert(*key, (group, now));
        }
        (draws, quads, groups)
    }
}

#[cfg(test)]
mod tests {
    // The shaders parse and validate as wgpu would take them; a bad one
    // leaves the canvas empty.
    #[test]
    fn shader_validates() {
        for shader in [super::SHADER, super::MARKS] {
            let module = naga::front::wgsl::parse_str(shader).unwrap_or_else(|e| {
                panic!("{}", e.emit_to_string(shader));
            });
            naga::valid::Validator::new(
                naga::valid::ValidationFlags::all(),
                naga::valid::Capabilities::default(),
            )
            .validate(&module)
            .unwrap_or_else(|e| panic!("{}", e.emit_to_string(shader)));
        }
    }
}

#[cfg(test)]
mod resize_tests {
    use super::{Quad, Source, Sprite};
    use gpui_editor_kit::texture::Texture;
    use std::sync::Arc;

    // A resize preview maps the old box onto the new one: the texel under
    // a corner (or the middle) of the old box is under the same corner of
    // the new box, and the drawn box moves with it.
    #[test]
    fn resized_sprites_map_the_old_box_onto_the_new() {
        let sprite = Sprite {
            texture: Arc::new(Source {
                rgba: Arc::new(Texture::from_rgba(1, 1, vec![255; 4]).unwrap()),
                bc: None,
            }),
            // Own pixels: a 10×20 box drawn at 100, 50, scaled 2×.
            transform: [2., 0., 0., 2., 100., 50.],
            local: [0., 0., 10., 20.],
            quad: Quad {
                inv: [0.5, 0., 0., 0.5, -50., -25.],
                min: [0.; 2],
                scale: [2.; 2],
                size: [20., 40.],
                px: [10., 20.],
                uv_offset: [0.; 2],
                uv_scale: [1.; 2],
                rotation: [1., 0.],
                nine: None,
                clamp: true,
            },
            tint: [255; 3],
            alpha: 1.,
            blend: 0,
            masks: Vec::new(),
        };
        let (from, to) = ([100., 50., 20., 40.], [90., 60., 40., 20.]);
        let s = sprite.resized(from, to);
        let apply = crate::ui::scene::apply;
        // Own corners land on the new box's corners.
        assert_eq!(apply(s.transform, [0., 0.]), [90., 60.]);
        assert_eq!(apply(s.transform, [10., 20.]), [130., 80.]);
        // And back: the new box's middle reads the old middle's texel.
        let mid_new = [110., 70.];
        let mid_old = [110., 70.];
        assert_eq!(apply(s.quad.inv, mid_new), apply(sprite.quad.inv, mid_old));
        assert_eq!(s.quad.size, [40., 20.]);
    }
}
