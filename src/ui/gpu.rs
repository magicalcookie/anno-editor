// Draws the UI canvas's images on GPUI's wgpu device, the way the game's
// widget shader does: one quad per image with its affine transform, UV
// mapping (9-slice, UVScale, UVRotation, UVOffset, wrap or clamp), tint,
// alpha and blend state, and the mask chain it draws through evaluated per
// pixel. The scene builds the draw records
// (`Sprite`); the canvas hands consecutive runs of them to `Batch`, so labels
// GPUI draws keep their place in paint order.
use gpui_editor_kit::{
    gpui::{self, Bounds, Pixels, Window},
    texture::{Block, Compressed, Texture},
};
use std::sync::{Arc, Mutex};

/// Mask layers one draw evaluates; deeper chains drop the outermost.
/// TODO(alex): no shipped screen nests deeper than four; count them.
pub const MAX_MASKS: usize = 6;

/// Whether the canvas draws images here (Linux and Windows, where GPUI
/// renders with wgpu) rather than as CPU composites. `ANNO_UI_CPU=1`
/// forces the CPU path; tests use it unless `ANNO_UI_GPU=1`.
pub fn enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        let platform = cfg!(any(target_os = "linux", target_os = "windows"));
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

/// GPU resources for one texture the canvas drew.
struct Uploaded {
    /// Keeps the address the cache is keyed by from being reused.
    _keep: Arc<Source>,
    view: wgpu::TextureView,
    used: u64,
}

struct Device {
    device: wgpu::Device,
    format: wgpu::TextureFormat,
    pipelines: [wgpu::RenderPipeline; 3],
    frame_layout: wgpu::BindGroupLayout,
    texture_layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    blank: wgpu::TextureView,
}

#[derive(Default)]
struct State {
    device: Option<Device>,
    textures: foldhash::HashMap<usize, Uploaded>,
    groups: foldhash::HashMap<[usize; 1 + MAX_MASKS], (wgpu::BindGroup, u64)>,
    /// Batches drawn: the clock textures and bind groups age by.
    draws: u64,
}

/// Textures and pipelines the canvas draws with, kept across frames.
#[derive(Clone, Default)]
pub struct Renderer {
    state: Arc<Mutex<State>>,
}

impl std::fmt::Debug for Renderer {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str("ui::gpu::Renderer")
    }
}

/// Batches a texture or bind group may go unused before it is freed: a
/// screen draws a few hundred batches a frame, so this is some seconds.
const KEEP_DRAWS: u64 = 20_000;
/// Batches between sweeps for textures and bind groups past `KEEP_DRAWS`.
const SWEEP_EVERY: u64 = 64;

impl Device {
    fn new(device: &wgpu::Device, format: wgpu::TextureFormat) -> Device {
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
        let frame_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("ui canvas frame"),
            entries: &[
                entry(
                    0,
                    both,
                    wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                ),
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
        // Multiply (DstColor, InvSrcAlpha, the shader premultiplying).
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
        let pipelines = blends.map(|blend| {
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some("ui canvas"),
                layout: Some(&layout),
                vertex: wgpu::VertexState {
                    module: &module,
                    entry_point: Some("vs"),
                    buffers: &[],
                    compilation_options: Default::default(),
                },
                fragment: Some(wgpu::FragmentState {
                    module: &module,
                    entry_point: Some("fs"),
                    targets: &[Some(wgpu::ColorTargetState {
                        format,
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
        });
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
            device: device.clone(),
            format,
            pipelines,
            frame_layout,
            texture_layout,
            sampler,
            blank,
        }
    }
}

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

/// A run of sprites the canvas draws between two of GPUI's own elements.
#[derive(Debug)]
pub struct Batch {
    renderer: Renderer,
    sprites: Vec<Arc<Sprite>>,
    /// Design pixels to device pixels: scale, offset.
    xf: [f32; 4],
}

impl Batch {
    /// Paints `sprites` over `bounds` (the canvas pane), where design point
    /// (0, 0) is at `origin` (logical pixels from the pane's corner) and one
    /// design pixel is `zoom` logical pixels.
    pub fn paint(
        renderer: &Renderer,
        sprites: Vec<Arc<Sprite>>,
        origin: [f32; 2],
        zoom: f32,
        bounds: Bounds<Pixels>,
        window: &mut Window,
    ) {
        if sprites.is_empty() {
            return;
        }
        let sf = window.scale_factor();
        let at = |o: Pixels, d: f32| (f32::from(o) + d) * sf;
        let batch = Batch {
            renderer: renderer.clone(),
            sprites,
            xf: [
                zoom * sf,
                zoom * sf,
                at(bounds.origin.x, origin[0]),
                at(bounds.origin.y, origin[1]),
            ],
        };
        // GPUI clips a canvas to its bounds as well as the content mask, and
        // the element can be smaller than what it draws (the game view's
        // other screens sit in a page-sized box shifted to the pane's
        // corner): cover the window and leave clipping to the mask.
        let whole = Bounds::new(
            gpui::point(gpui::px(0.), gpui::px(0.)),
            window.viewport_size(),
        );
        window.paint_gpu_canvas(whole, Arc::new(batch));
    }
}

impl gpui::GpuCanvas for Batch {
    fn draw(&self, frame: &mut gpui::GpuCanvasFrame) {
        let mut state = self
            .renderer
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let state = &mut *state;
        state.draws += 1;
        let now = state.draws;
        let fresh = state
            .device
            .as_ref()
            .is_none_or(|d| d.device != *frame.device || d.format != frame.format);
        if fresh {
            state.textures.clear();
            state.groups.clear();
            state.device = Some(Device::new(frame.device, frame.format));
        }
        let dev = state.device.as_ref().expect("made above");
        let device = frame.device;
        // Draw records, quads and bind groups, in sprite order.
        let mut draws: Vec<GpuDraw> = Vec::with_capacity(self.sprites.len());
        let mut quads: Vec<GpuQuad> = Vec::with_capacity(self.sprites.len() * 2);
        let mut groups: Vec<[usize; 1 + MAX_MASKS]> = Vec::with_capacity(self.sprites.len());
        for s in &self.sprites {
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
            // Textures this batch needs, uploaded on first sight.
            let textures =
                std::iter::once(&s.texture).chain(masks.iter().filter_map(|m| m.texture.as_ref()));
            for t in textures {
                state
                    .textures
                    .entry(Arc::as_ptr(t) as usize)
                    .or_insert_with(|| Uploaded {
                        _keep: t.clone(),
                        view: upload(device, frame.queue, t),
                        used: now,
                    })
                    .used = now;
            }
            groups.push(key);
        }
        for key in &groups {
            if let Some(g) = state.groups.get_mut(key) {
                g.1 = now;
                continue;
            }
            let view = |k: usize| match k {
                0 => &dev.blank,
                k => &state.textures[&k].view,
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
                layout: &dev.texture_layout,
                entries: &entries,
            });
            state.groups.insert(*key, (group, now));
        }
        let buffer = |label, usage, bytes: &[u8]| {
            let b = device.create_buffer(&wgpu::BufferDescriptor {
                label: Some(label),
                size: (bytes.len() as u64).max(16),
                usage: usage | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            frame.queue.write_buffer(&b, 0, bytes);
            b
        };
        let view = GpuView {
            xf: self.xf,
            target: [
                frame.size[0] as f32,
                frame.size[1] as f32,
                f32::from(u8::from(frame.format.remove_srgb_suffix() != frame.format)),
                0.,
            ],
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
            layout: &dev.frame_layout,
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
                    resource: wgpu::BindingResource::Sampler(&dev.sampler),
                },
            ],
        });
        {
            let mut pass = frame
                .encoder
                .begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("ui canvas"),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: frame.target,
                        resolve_target: None,
                        ops: wgpu::Operations {
                            load: wgpu::LoadOp::Load,
                            store: wgpu::StoreOp::Store,
                        },
                        depth_slice: None,
                    })],
                    depth_stencil_attachment: None,
                    timestamp_writes: None,
                    occlusion_query_set: None,
                    multiview_mask: None,
                });
            let [sx, sy, sw, sh] = scissor(frame.clip, frame.size);
            if sw == 0 || sh == 0 {
                return;
            }
            pass.set_scissor_rect(sx, sy, sw, sh);
            pass.set_bind_group(0, &frame_group, &[]);
            // Neighbours with one blend and one set of textures (a list's
            // slots, a bar's tiles) draw as one instanced call: the shader
            // finds each one's record by instance index.
            let mut bound = None;
            let mut i = 0;
            while i < draws.len() {
                let kind = draws[i].at[2] as usize;
                let end = (i + 1..draws.len())
                    .find(|&j| draws[j].at[2] as usize != kind || groups[j] != groups[i])
                    .unwrap_or(draws.len());
                if bound != Some(kind) {
                    pass.set_pipeline(&dev.pipelines[kind]);
                    bound = Some(kind);
                }
                pass.set_bind_group(1, &state.groups[&groups[i]].0, &[]);
                pass.draw(0..6, i as u32..end as u32);
                i = end;
            }
        }
        // Now and then, not per batch: a sweep walks every cached texture.
        if now.is_multiple_of(SWEEP_EVERY) {
            state.textures.retain(|_, t| now - t.used < KEEP_DRAWS);
            let textures = &state.textures;
            state.groups.retain(|k, g| {
                now - g.1 < KEEP_DRAWS && k.iter().all(|&t| t == 0 || textures.contains_key(&t))
            });
        }
    }
}

fn scissor(clip: Bounds<gpui::ScaledPixels>, size: [u32; 2]) -> [u32; 4] {
    let f = |v: gpui::ScaledPixels| f64::from(v) as f32;
    let x = (f(clip.origin.x).max(0.) as u32).min(size[0]);
    let y = (f(clip.origin.y).max(0.) as u32).min(size[1]);
    let w = (f(clip.size.width).max(0.) as u32).min(size[0] - x);
    let h = (f(clip.size.height).max(0.) as u32).min(size[1] - y);
    [x, y, w, h]
}

#[cfg(test)]
mod tests {
    // The shader parses and validates as wgpu would take it; a bad one
    // invalidates the whole frame GPUI draws it in (a black window).
    #[test]
    fn shader_validates() {
        let module = naga::front::wgsl::parse_str(super::SHADER).unwrap_or_else(|e| {
            panic!("{}", e.emit_to_string(super::SHADER));
        });
        naga::valid::Validator::new(
            naga::valid::ValidationFlags::all(),
            naga::valid::Capabilities::default(),
        )
        .validate(&module)
        .unwrap_or_else(|e| panic!("{}", e.emit_to_string(super::SHADER)));
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
