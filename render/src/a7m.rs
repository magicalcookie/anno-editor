// The 117 and 1800 ground: the ranked splat over the six a7m slots per
// texel, with the material set's grit on top and the island's tint map
// multiplied in, as Anno's terrain shader orders them. Owns the splat's
// device copies (layer arrays, palettes built from the CPU control maps,
// the grit and its mask) and the relief scan that asks for finer tiles.
use std::collections::VecDeque;
use std::sync::{Arc, Mutex, Weak};

pub use anno_core::game::Bake;
use gpui_editor_kit::{
    gpu::{
        self, ControlMap, Kept, LockAny as _, ResidentControl, StreamedTexture,
        terrain::{self, GroundPrepare},
    },
    heightfield::{Grid, Rect},
    texture::Image,
};
use wgpu::util::DeviceExt as _;

/// Tiled terrain layers stacked through control images.
///
/// `indices` and `weights` come in matching pairs of RGBA8 control maps
/// sampled over the terrain's grid: each channel of an index texel names a layer, and
/// the same channel of the weight texel says how much of it covers. One pair
/// gives four layers per texel, two give eight. Layers stack in index
/// order: the lowest present is opaque, each higher one is laid over the
/// ones below at its weight, interpolated between texels (see
/// `sharpness`). A pixel blends at most 12 distinct layers. Layers tile in
/// world X and Z.
/// Control maps take rectangle edits (`ControlMap::write`), so painting
/// shows on the next frame without a new splat.
///
/// Layers share one texture array, so they must reach one size: the smallest
/// layer's. Larger compressed layers start at the mip of that size, so 512²
/// and 1024² layers share a 512² array; RGBA layers have no mips and must be
/// that size. The array keeps the mips every layer has from there down.
/// Compressed layers of one block format stay compressed on the GPU; mixing
/// formats, or mixing in RGBA, decodes them to RGBA band by band on upload.
///
/// GPU memory: `layers × width × height` times 4 bytes for RGBA, 1 for BC2,
/// BC3, BC5 and BC7, ½ for BC1 and BC4, plus a third for mips; then
/// `width × height × 4` per control image (4096² is 64 MB each, so two pairs
/// at that size cost 256 MB). 25 BC7 layers at 512² with mips are 8.7 MB.
/// Everything streams in under the upload budget.
#[derive(Clone, Debug, Default)]
pub struct Splat {
    /// Layer images, at most 256; see above for sizes.
    pub layers: Vec<Image>,
    /// World units one repeat of each layer covers.
    pub tiling: Vec<f32>,
    /// How each layer's image alpha (a height) cuts into its weight where it
    /// is laid over the layers below; larger is crisper. Empty or 0 lays a
    /// layer over at its plain weight.
    pub sharpness: Vec<f32>,
    /// Tangent-space normal maps (RGB, flat is 128,128,255; green along
    /// +Z), one per layer, or empty for none. They stack with the same
    /// alphas as the layers and bend the terrain normal before lighting.
    /// Same size rules as `layers`, in their own array.
    pub normals: Vec<Image>,
    /// Glossiness maps in red (Anno's terrain `_spec`), one per layer or
    /// empty for matte ground. They stack like the layers and give toned
    /// scenes their highlights. Same size rules, their own array.
    pub gloss: Vec<Image>,
    pub indices: Vec<Arc<ControlMap>>,
    pub weights: Vec<Arc<ControlMap>>,
    pub grit: Option<Grit>,
    pub bake: Bake,
    /// The layers' heights, for Anno's material displacement; `None` for
    /// ground without.
    pub relief: Option<Relief>,
}

/// Anno's material displacement (terrain_baking.fx psBakeSplatsBulk,
/// functions_terrain.fxh SampleMaterialDisplacement): each layer's height
/// map, mapped into its floor..ceil range in world units and stacked like
/// the layers, raises or sinks the ground, and the ground is lit from that
/// height rather than from the layers' normal maps. Drawn when `on`; the
/// splat shades the height's slope at each pixel's own size, as the game's
/// bake does at its page texels, so ledges keep their light far off.
#[derive(Clone, Debug, Default)]
pub struct Relief {
    /// Height maps in red, one per layer; same size rules as the layers,
    /// their own array.
    pub heights: Vec<Image>,
    /// Per layer: floor and ceil in world units, and 1 where the layer adds
    /// its height to what lies below (`DisplacementAdd`) instead of
    /// covering it.
    pub ranges: Vec<[f32; 3]>,
    pub on: bool,
}

/// A detail texture laid over the whole splat (Anno's terrain grit,
/// functions_terrain.fxh BlendTerrainGrit): where `mask`'s red is up,
/// `texture` covers the ground, cut by its alpha with `sharpness` as the
/// layers are, and only on ground facing up (`slope` widens that).
#[derive(Clone, Debug)]
pub struct Grit {
    pub texture: Image,
    /// Spans the terrain from its grid's origin, `metres` on each side.
    pub mask: Arc<ControlMap>,
    pub metres: f32,
    /// World units one repeat covers.
    pub tiling: f32,
    pub sharpness: f32,
    pub slope: f32,
}

// Vulkan guarantees 256 array layers, and an index channel holds 0..=255,
// of which palettes keep 255 for "none" (palette.wgsl).
const MAX_LAYERS: usize = 255;

impl Splat {
    /// Fails where the splat can't draw: counts that disagree, layers that
    /// can't share an array, control maps of different sizes.
    pub fn validate(&self) -> anyhow::Result<()> {
        use anyhow::ensure;
        let count = self.layers.len();
        ensure!(
            (1..=MAX_LAYERS).contains(&count),
            "Splat needs 1 to {MAX_LAYERS} layers, has {count}"
        );
        for layer in &self.layers {
            layer.validate()?;
        }
        StreamedTexture::check(&self.layers)?;
        if !self.normals.is_empty() {
            ensure!(
                self.normals.len() == count,
                "Splat normals are empty or one per layer"
            );
            for normal in &self.normals {
                normal.validate()?;
            }
            StreamedTexture::check(&self.normals)?;
        }
        if !self.gloss.is_empty() {
            ensure!(
                self.gloss.len() == count,
                "Splat gloss maps are empty or one per layer"
            );
            for gloss in &self.gloss {
                gloss.validate()?;
            }
            StreamedTexture::check(&self.gloss)?;
        }
        if let Some(relief) = &self.relief {
            ensure!(
                relief.heights.len() == count && relief.ranges.len() == count,
                "Splat relief needs a height map and a range per layer"
            );
            for height in &relief.heights {
                height.validate()?;
            }
            StreamedTexture::check(&relief.heights)?;
            ensure!(
                relief.ranges.iter().flatten().all(|v| v.is_finite()),
                "Splat relief ranges must be finite"
            );
        }
        if let Some(grit) = &self.grit {
            grit.texture.validate()?;
            ensure!(
                grit.metres > 0. && grit.tiling > 0. && grit.sharpness >= 0.,
                "Splat grit needs a positive size and tiling"
            );
        }
        ensure!(
            self.tiling.len() == count && self.tiling.iter().all(|t| t.is_finite() && *t > 0.),
            "Splat needs one positive tiling per layer"
        );
        ensure!(
            self.sharpness.is_empty()
                || self.sharpness.len() == count
                    && self.sharpness.iter().all(|s| s.is_finite() && *s >= 0.),
            "Splat sharpness is empty or one non-negative value per layer"
        );
        ensure!(
            (1..=2).contains(&self.indices.len()) && self.weights.len() == self.indices.len(),
            "Splat needs one or two index/weight pairs"
        );
        let control = self.indices[0].size();
        for map in self.indices.iter().chain(&self.weights) {
            ensure!(map.size() == control, "Splat control maps differ in size");
        }
        Ok(())
    }
}

/// The ground drawing `splat`, which must validate (`Splat::validate`),
/// for a terrain of its grid. Make it once per splat and keep it: the kit
/// keeps a mounted ground, and its pages, while the factory and terrain
/// stay the same, so one per scene would remount and rebake every page on
/// each camera or layer change.
pub fn ground(splat: &Arc<Splat>) -> gpu::GroundDraw {
    let splat = splat.clone();
    // The splat's device copies outlive a mount: the editor hands the view
    // a new terrain over the same splat
    // after each stroke, and a ground made afresh then rebuilt every
    // palette tile. One per device and grid, while a mounted ground holds
    // it.
    let residents: Mutex<Vec<Weak<Resident>>> = Mutex::new(Vec::new());
    gpu::GroundDraw::Surface(Arc::new(move |init| {
        let find = |residents: &mut Vec<Weak<Resident>>| {
            residents.retain(|r| r.strong_count() > 0);
            (residents.iter())
                .filter_map(Weak::upgrade)
                .find(|r| r.device == *init.device && r.grid == init.grid)
        };
        let found = find(&mut residents.lock_any());
        // Made outside the lock: on a lost device it panics in wgpu, which
        // must not leave the list poisoned for the next device's mount.
        let resident = found.unwrap_or_else(|| {
            let made = Arc::new(Resident::new(init.device, splat.clone(), init.grid));
            let mut residents = residents.lock_any();
            // Another view's mount may have made one meanwhile.
            find(&mut residents).unwrap_or_else(|| {
                residents.push(Arc::downgrade(&made));
                made
            })
        });
        Box::new(A7mGround {
            resident,
            rough: Mutex::new(Vec::new()),
        })
    }))
}

/// Both games' terrain shaders define `BRDF_DISABLE_SPEC_ON_ZERO_GLOSS`
/// (1800 terrain.fxh; 117 tags it GBUFFER_MATERIAL_ID_TERRAIN, which
/// deferred_shading.fx gives the same flag): matte ground keeps neither
/// the sun's highlight nor the sky's reflection, which turned dark soil
/// grey toward the horizon.
const GROUND_SPECULAR: gpu::Specular = gpu::Specular::MATTE_FADES;

/// Builds the ground's programs as the window's device is made
/// (`gpu::warm_passes`), so the first island opened doesn't compile them. All four variants:
/// whether a set has normal maps and relief is known only once it loads.
/// Under the model family's lighting, the set 117's and 1800's islands
/// draw with (`a7m_models::families`).
pub fn warm(gpu: &gpu::Warm<'_>) {
    let variants = [false, true].map(|relief| [None, Some(NORMAL_FOOTPRINT)].map(|n| (relief, n)));
    let families = crate::a7m_models::families();
    gpu.ground(
        &families,
        wgsl(),
        &layout(gpu.device),
        false,
        GROUND_SPECULAR,
        variants.as_flattened(),
    );
}

/// a7m.wgsl with the tint decode it shares with the models' lighting
/// (`TINT_DECODE`), as `a7m_tint`.
fn wgsl() -> &'static str {
    static WGSL: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    WGSL.get_or_init(|| {
        format!(
            "{}\nfn a7m_tint(texel: vec4<f32>) -> vec3<f32> {{\n{TINT_DECODE}}}\n",
            include_str!("a7m.wgsl")
        )
    })
}

/// How the island's tint map decodes into the multiplier the ground and
/// its models take, as the body of a WGSL function of `texel: vec4<f32>`
/// returning `vec3<f32>`: the texel filtered as stored, then
/// UncompressTintColor as the game's compiled terrain shader runs it
/// (docs/island-tint.md): RGB holds Cb, Y and Cr, chroma as a signed square
/// root around 0.5, JPEG YCbCr with Cb and Cr halved, then (2 rgb)^2.2, so
/// 0.5 grey is x1. The models take it through their families' lighting
/// (`a7m_models::families`).
pub const TINT_DECODE: &str = "    let t = texel.rgb;
    let cb = (2.0 * t.r - 1.0) * abs(2.0 * t.r - 1.0);
    let cr = (2.0 * t.b - 1.0) * abs(2.0 * t.b - 1.0);
    let y = t.g;
    let rgb = vec3<f32>(y + 0.701 * cr, y - 0.172 * cb - 0.357 * cr, y + 0.886 * cb);
    return pow(2.0 * max(rgb, vec3<f32>(0.0)), vec3<f32>(2.2));
";

/// Metres per pixel up to which the splat reads its layers' normal maps
/// (a7m.wgsl FAR_GROUND).
const NORMAL_FOOTPRINT: f32 = 0.12;

/// World units the stacked relief must span for ground to take the kit's
/// finer patch levels (`Ground::rough`). The game tessellates its terrain
/// patches by a granularity baked from the height's roughness
/// (terrain_baking.fx csGenerateGranularityFromHeight), which leaves flat
/// ground at one quad; on 117's Roman set grass, sand, dirt and pebbles
/// span at most 0.08 and rock, cliffs and gravel 0.17 to 0.95.
const RELIEF_ROUGH: f32 = 0.1;

/// Control texels a frame builds palette tiles from, at most: the first
/// build of a 2048² island takes eight frames, each writing 8 MB of its
/// control maps; a brush dab's few thousand build at once.
const PALETTE_BAND: usize = 1 << 19;

/// The splat's uniform (a7m.wgsl `A7mParams`).
#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct Params {
    grit: [f32; 4],
    control: [f32; 2],
    origin: [f32; 2],
    gloss: f32,
    plain: f32,
    grit_mask: f32,
    _pad: f32,
}

/// Splat layer: reciprocal tiling in world units, blend sharpness, and the
/// relief's floor, ceil and add (a7m.wgsl `GpuLayer`).
#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct GpuLayer {
    scale: f32,
    sharpness: f32,
    floor: f32,
    ceil: f32,
    add: f32,
    _pad: f32,
}

/// One layer's part in the relief scan.
struct LayerRelief {
    /// World units between its floor and ceil.
    span: f32,
    adds: bool,
    sharpness: f32,
}

/// A scanned area's answer, kept until paint inside it changes.
struct RoughArea {
    rect: Rect,
    witness: Option<[u32; 2]>,
    serials: [u64; 4],
}

/// What changed for pages, in the grid's UVs (palette tiles) or in metres
/// from its origin (the grit mask).
#[derive(Clone, Copy)]
enum Edit {
    Uv([f32; 4]),
    Metres([f32; 4]),
}

/// A mount of the splat's ground: its resident copy and the relief scan's
/// answers so far.
struct A7mGround {
    resident: Arc<Resident>,
    rough: Mutex<Vec<RoughArea>>,
}

/// The splat on one device, shared by the grounds mounted over it.
struct Resident {
    device: wgpu::Device,
    splat: Arc<Splat>,
    grid: Grid,
    layout: wgpu::BindGroupLayout,
    group: wgpu::BindGroup,
    /// Layers, then normals, gloss and relief heights where the splat has
    /// them, and the grit texture: all streamed before the ground draws.
    textures: Vec<Arc<StreamedTexture>>,
    grit_mask: Option<ResidentControl>,
    palette: Palette,
    relief: Vec<LayerRelief>,
    /// Layers whose relief moves the shadow caster, one bit each (see
    /// `Resident::new`). All clear without relief.
    displacing: [u64; 4],
    state: Mutex<State>,
    /// Streamed and every palette tile built once: the ground draws.
    ready: std::sync::atomic::AtomicBool,
}

struct State {
    streamed: bool,
    /// The edit serial of each of the splat's index then weight maps the
    /// palette has taken in.
    synced: Vec<u64>,
    /// Control texels whose palette tiles are still to build (all of them
    /// at first), and whether every tile has been built once.
    dirty: Option<Rect>,
    built: bool,
    serial: u64,
    /// The last edits, for `edits`; older serials report the whole grid.
    log: VecDeque<Edit>,
    /// Per palette tile, row-major, one bit each: whether a displacing
    /// layer lies on the tile. Set until a build reads the tile, so a tile
    /// not yet read counts as one that displaces.
    displaced: Vec<u64>,
    /// Builds that could move the displaced surface (`Ground::relief_moves`).
    relief_moves: u64,
}

impl State {
    fn push(&mut self, edit: Edit) {
        self.serial += 1;
        if self.log.len() == 64 {
            self.log.pop_front();
        }
        self.log.push_back(edit);
    }
}

/// The splat's group 3 layout, one per device so warmed programs match
/// (`Warm::ground`).
fn layout(d: &wgpu::Device) -> wgpu::BindGroupLayout {
    gpu::per_device(d, "a7m ground", || {
        use wgpu::ShaderStages as S;
        let entry = |binding, ty| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: S::VERTEX_FRAGMENT,
            ty,
            count: None,
        };
        let texture = |view_dimension| wgpu::BindingType::Texture {
            sample_type: wgpu::TextureSampleType::Float { filterable: true },
            view_dimension,
            multisampled: false,
        };
        let (d2, array) = (
            wgpu::TextureViewDimension::D2,
            wgpu::TextureViewDimension::D2Array,
        );
        let buffer = |ty| wgpu::BindingType::Buffer {
            ty,
            has_dynamic_offset: false,
            min_binding_size: None,
        };
        let mut entries = vec![
            entry(0, buffer(wgpu::BufferBindingType::Uniform)),
            entry(
                1,
                buffer(wgpu::BufferBindingType::Storage { read_only: true }),
            ),
            entry(2, texture(array)),
        ];
        entries.extend((3..=6).map(|b| entry(b, texture(d2))));
        entries.extend([
            entry(7, texture(array)),
            entry(8, texture(array)),
            entry(9, texture(d2)),
            entry(10, texture(d2)),
            entry(11, texture(array)),
            entry(
                12,
                wgpu::BindingType::Sampler(wgpu::SamplerBindingType::NonFiltering),
            ),
        ]);
        d.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("a7m ground"),
            entries: &entries,
        })
    })
}

/// A texture array over `images`, shared with any other ground holding
/// the same images on this device: a splat rebuilt over the same layers
/// (the Relief toggle, undoing paint) keeps them uploaded instead of
/// streaming the set's 80 MB again. Weak, so arrays go with their grounds.
fn array(device: &wgpu::Device, images: &[Image], bc: bool) -> Arc<StreamedTexture> {
    type Arrays = Vec<(wgpu::Device, Vec<usize>, Weak<StreamedTexture>)>;
    static ARRAYS: Mutex<Arrays> = Mutex::new(Vec::new());
    let key: Vec<usize> = images
        .iter()
        .map(|image| match image {
            Image::Rgba(t) => Arc::as_ptr(t) as *const () as usize,
            Image::Compressed(c) => Arc::as_ptr(c) as *const () as usize,
            Image::Releasable(r) => Arc::as_ptr(r) as *const () as usize,
        })
        .collect();
    let find = |arrays: &mut Arrays| {
        arrays.retain(|(_, _, a)| a.strong_count() > 0);
        (arrays.iter())
            .find(|(d, k, _)| d == device && *k == key)
            .and_then(|(_, _, a)| a.upgrade())
    };
    if let Some(found) = find(&mut ARRAYS.lock_any()) {
        return found;
    }
    // Made outside the lock, as `ground`'s residents are: wgpu panics on a
    // lost device.
    let made = Arc::new(
        StreamedTexture::new(device, images.to_vec(), true, bc).expect("validated splat layers"),
    );
    let mut arrays = ARRAYS.lock_any();
    if let Some(found) = find(&mut arrays) {
        return found;
    }
    arrays.push((device.clone(), key.clone(), Arc::downgrade(&made)));
    made
}

/// A 1×1 stand-in for a map the splat lacks; the shader never reads it.
fn blank(d: &wgpu::Device, layers: bool) -> wgpu::TextureView {
    let texture = d.create_texture(&wgpu::TextureDescriptor {
        label: Some("a7m blank"),
        size: wgpu::Extent3d {
            width: 1,
            height: 1,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8Unorm,
        usage: wgpu::TextureUsages::TEXTURE_BINDING,
        view_formats: &[],
    });
    texture.create_view(&wgpu::TextureViewDescriptor {
        dimension: Some(if layers {
            wgpu::TextureViewDimension::D2Array
        } else {
            wgpu::TextureViewDimension::D2
        }),
        ..Default::default()
    })
}

impl Resident {
    fn new(d: &wgpu::Device, splat: Arc<Splat>, grid: Grid) -> Self {
        let bc = gpu::bc_supported(d);
        let info: Vec<GpuLayer> = (splat.tiling.iter().enumerate())
            .map(|(i, tiling)| {
                let range = (splat.relief.as_ref())
                    .and_then(|r| r.ranges.get(i).copied())
                    .unwrap_or_default();
                GpuLayer {
                    scale: 1. / tiling,
                    sharpness: splat.sharpness.get(i).copied().unwrap_or(0.),
                    floor: range[0],
                    ceil: range[1],
                    add: range[2],
                    _pad: 0.,
                }
            })
            .collect();
        let control = splat.indices[0].size();
        let mut params = Params {
            grit: [0.; 4],
            control: control.map(|v| v as f32),
            origin: grid.origin,
            gloss: f32::from(u8::from(!splat.gloss.is_empty())),
            plain: match splat.bake {
                Bake::Smooth => 0.,
                Bake::Plain => 1.,
            },
            grit_mask: 0.,
            _pad: 0.,
        };
        if let Some(g) = &splat.grit {
            params.grit = [1. / g.tiling, g.sharpness, g.slope, 1.];
            params.grit_mask = 1. / g.metres;
        }
        let layers = array(d, &splat.layers, bc);
        let normals = (!splat.normals.is_empty()).then(|| array(d, &splat.normals, bc));
        let gloss = (!splat.gloss.is_empty()).then(|| array(d, &splat.gloss, bc));
        let heights = splat.relief.as_ref().map(|r| array(d, &r.heights, bc));
        let grit = splat.grit.as_ref().map(|g| {
            let texture = StreamedTexture::new(d, vec![g.texture.clone()], false, bc)
                .expect("validated grit texture");
            let mask = ResidentControl::new(d, "a7m grit mask", g.mask.clone(), Kept::Red);
            (Arc::new(texture), mask)
        });
        let palette = Palette::new(d, control);
        let layout = layout(d);
        let uniform = d.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("a7m ground"),
            contents: bytemuck::bytes_of(&params),
            usage: wgpu::BufferUsages::UNIFORM,
        });
        let layer_info = d.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("a7m layers"),
            contents: bytemuck::cast_slice(&info),
            usage: wgpu::BufferUsages::STORAGE,
        });
        let [blank_array, blank_2d] = [true, false].map(|layers| blank(d, layers));
        let view = |t: &Option<Arc<StreamedTexture>>, blank: &wgpu::TextureView| {
            t.as_ref().map_or_else(|| blank.clone(), |t| t.view())
        };
        let views = [
            (2, layers.view()),
            (3, palette.weights[0].clone()),
            (4, palette.weights[1].clone()),
            (5, palette.ids[0].clone()),
            (6, palette.ids[1].clone()),
            (7, view(&normals, &blank_array)),
            (8, view(&gloss, &blank_array)),
            (9, view(&grit.as_ref().map(|g| g.0.clone()), &blank_2d)),
            (
                10,
                grit.as_ref()
                    .map_or_else(|| blank_2d.clone(), |g| g.1.view.clone()),
            ),
            (11, view(&heights, &blank_array)),
        ];
        let mut entries = vec![
            wgpu::BindGroupEntry {
                binding: 0,
                resource: uniform.as_entire_binding(),
            },
            wgpu::BindGroupEntry {
                binding: 1,
                resource: layer_info.as_entire_binding(),
            },
        ];
        entries.extend(views.iter().map(|(binding, view)| wgpu::BindGroupEntry {
            binding: *binding,
            resource: wgpu::BindingResource::TextureView(view),
        }));
        // Nearest: the palette weights are only gathered, which filters
        // nothing; the albedo sampler they took before is anisotropic.
        let gather = gpu::per_device(d, "a7m gather", || {
            d.create_sampler(&wgpu::SamplerDescriptor {
                label: Some("a7m gather"),
                ..Default::default()
            })
        });
        entries.push(wgpu::BindGroupEntry {
            binding: 12,
            resource: wgpu::BindingResource::Sampler(&gather),
        });
        let group = d.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("a7m ground"),
            layout: &layout,
            entries: &entries,
        });
        let relief = (splat.relief.iter())
            .filter(|r| r.on)
            .flat_map(|r| &r.ranges)
            .enumerate()
            .map(|(i, r)| LayerRelief {
                span: r[1] - r[0],
                adds: r[2] > 0.5,
                sharpness: splat.sharpness.get(i).copied().unwrap_or(0.),
            })
            .collect();
        // Layers that raise or sink the ground by RELIEF_ROUGH or more move
        // the shadow caster. Every layer of 117's Roman set displaces some:
        // grass, sand, dirt and pebbles by 0.08 at most. Counting those, every
        // dab redrew the shadow map and its cascades (4 ms a frame on the
        // pirate island). Now paint among them alone leaves the shadow maps
        // until something else redraws them: a sculpt, rougher paint, a new
        // scene, or the view leaving its cascades.
        let mut displacing = [0u64; 4];
        let ranges = (splat.relief.iter())
            .filter(|r| r.on)
            .flat_map(|r| &r.ranges);
        for (i, r) in ranges.enumerate().take(256) {
            if r[0].abs().max(r[1].abs()) >= RELIEF_ROUGH {
                displacing[i / 64] |= 1 << (i % 64);
            }
        }
        let (grit_texture, grit_mask) = grit.map_or((None, None), |(t, m)| (Some(t), Some(m)));
        let textures = std::iter::once(layers)
            .chain(normals)
            .chain(gloss)
            .chain(heights)
            .chain(grit_texture)
            .collect();
        let synced = (splat.indices.iter().chain(&splat.weights))
            .map(|m| m.edits_since(None).0)
            .collect();
        let whole = palette.whole();
        let tiles = palette.tiles.map(|v| v as usize);
        Self {
            device: d.clone(),
            splat,
            grid,
            layout,
            group,
            textures,
            grit_mask,
            palette,
            relief,
            displacing,
            state: Mutex::new(State {
                streamed: false,
                synced,
                dirty: Some(whole),
                built: false,
                serial: 0,
                log: VecDeque::new(),
                displaced: if displacing == [0; 4] {
                    Vec::new()
                } else {
                    vec![u64::MAX; (tiles[0] * tiles[1]).div_ceil(64)]
                },
                relief_moves: 0,
            }),
            ready: Default::default(),
        }
    }

    /// The grit mask and textures first, as the kit streams a mesh's, then
    /// the palette. True once nothing remains.
    fn prepare(&self, frame: &mut GroundPrepare<'_>) -> bool {
        let mut state = self.state.lock_any();
        if !state.streamed {
            // A handle, so the budget can be charged beside it.
            let queue = frame.queue().clone();
            if let Some(mask) = &self.grit_mask
                && !mask.fill(&queue, frame.budget)
            {
                return false;
            }
            for texture in &self.textures {
                if !texture.stream(&queue, frame.budget) {
                    return false;
                }
            }
            state.streamed = true;
        }
        let pending = self.sync_palette(&mut state, frame);
        if state.built {
            self.ready.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        !pending
    }

    /// Takes in paint and grit edits and builds the palette tiles they
    /// reach, a band of rows a frame within the budget. True while some
    /// remain.
    fn sync_palette(&self, state: &mut State, frame: &mut GroundPrepare<'_>) -> bool {
        let mut dirty = state.dirty.take();
        let maps = self.splat.indices.iter().chain(&self.splat.weights);
        for (map, synced) in maps.zip(state.synced.iter_mut()) {
            let (serial, rects) = map.edits_since(Some(*synced));
            *synced = serial;
            for rect in rects {
                dirty = Some(dirty.map_or(rect, |d| d.union(rect)));
            }
        }
        let can_sync_grit = !frame.budget.spent();
        let grit = (self.grit_mask.as_ref())
            .filter(|_| can_sync_grit)
            .and_then(|mask| Some((mask.sync(frame.queue())?, mask.source.size())));
        if let Some((rect, size)) = grit {
            frame
                .budget
                .charge((rect.x1 - rect.x0) * (rect.z1 - rect.z0));
            let metres = self.splat.grit.as_ref().map_or(0., |g| g.metres);
            let [w, h] = size.map(|v| v as f32);
            state.push(Edit::Metres([
                (rect.x0 as f32 - 1.) / w * metres,
                (rect.z0 as f32 - 1.) / h * metres,
                (rect.x1 as f32 + 1.) / w * metres,
                (rect.z1 as f32 + 1.) / h * metres,
            ]));
        }
        if frame.budget.spent() {
            state.dirty = dirty;
            return dirty.is_some() || (!can_sync_grit && self.grit_mask.is_some());
        }
        let size = self.splat.indices[0].size();
        // A palette window includes the tiles on either side and their
        // filter border.
        let pad = PALETTE_PAD;
        let rest = dirty.and_then(|d| {
            let bytes = self.splat.indices.len() * 8;
            let width = (d.x1 - d.x0 + pad).min(size[0]);
            let rows = (PALETTE_BAND / (d.x1 - d.x0).max(1))
                .min((frame.budget.room() / bytes / width).saturating_sub(pad))
                .max(PALETTE_TILE);
            (d.z1 - d.z0 > rows).then(|| Rect {
                z0: d.z0 + rows,
                ..d
            })
        });
        let band = dirty.map(|d| Rect {
            z1: rest.map_or(d.z1, |r| r.z0),
            ..d
        });
        state.dirty = rest;
        if rest.is_none() {
            state.built = true;
        }
        if let Some(rect) = band {
            // Bicubic coverage reads two texels round each, and the
            // palette tiles under those can change their layers across
            // the tile.
            let [w, h] = size.map(|v| v as f32);
            let tile = PALETTE_TILE as f32;
            let down = |v: usize| ((v as f32 - 2.) / tile).floor() * tile;
            let up = |v: usize| ((v as f32 + 2.) / tile).ceil() * tile;
            state.push(Edit::Uv([
                down(rect.x0) / w,
                down(rect.z0) / h,
                up(rect.x1) / w,
                up(rect.z1) / h,
            ]));
            let controls = [self.splat.indices.as_slice(), self.splat.weights.as_slice()];
            let layers = self.splat.layers.len() as u32;
            let read = self.palette.build(frame, controls, layers, rect);
            if let Some(read) = read
                && self.relief_moved(state, rect, &read)
            {
                state.relief_moves += 1;
            }
            frame.budget.charge(
                (rect.x1 - rect.x0 + pad).min(size[0])
                    * (rect.z1 - rect.z0 + pad).min(size[1])
                    * self.splat.indices.len()
                    * 8,
            );
        }
        rest.is_some()
    }

    /// Takes in which palette tiles under `rect`, whose palettes were just
    /// built from `read`, hold a displacing layer. True when the build can
    /// have moved the displaced surface: a displacing layer lay or lies
    /// within reach of `rect`, its bicubic weights and the palette windows
    /// round its tiles. Paint elsewhere leaves the shadow maps as drawn.
    fn relief_moved(&self, state: &mut State, rect: Rect, read: &Window) -> bool {
        if state.displaced.is_empty() {
            return false;
        }
        let tile = PALETTE_TILE;
        let [tw, th] = self.palette.tiles.map(|v| v as usize);
        let tiles = |pad: usize| {
            let lo = |v: usize| v.saturating_sub(pad) / tile;
            let hi = |v: usize, n: usize| (v + pad).div_ceil(tile).min(n);
            [lo(rect.x0), lo(rect.z0), hi(rect.x1, tw), hi(rect.z1, th)]
        };
        let any = |bits: &[u64], [x0, z0, x1, z1]: [usize; 4]| {
            (z0..z1)
                .any(|z| (x0..x1).any(|x| bits[(z * tw + x) / 64] >> ((z * tw + x) % 64) & 1 != 0))
        };
        let reach = tiles(PALETTE_PAD / 2);
        let before = any(&state.displaced, reach);
        let [x0, z0, x1, z1] = tiles(0);
        let w = read.rect;
        let width = w.x1 - w.x0;
        let pairs = read.read.len() / 2;
        let displaces = |id: u8| self.displacing[usize::from(id) / 64] >> (id % 64) & 1 != 0;
        for tz in z0..z1 {
            for tx in x0..x1 {
                // The window holds every tile under `rect` with its border.
                let (cx, cz) = (tx * tile, tz * tile);
                let xs = cx.max(w.x0)..(cx + tile).min(w.x1);
                let found = (cz.max(w.z0)..(cz + tile).min(w.z1)).any(|z| {
                    xs.clone().any(|x| {
                        let t = ((z - w.z0) * width + x - w.x0) * 4;
                        (0..pairs).any(|k| {
                            let (ids, weights) = (&read.read[k], &read.read[pairs + k]);
                            (t..t + 4).any(|c| weights[c] > 0 && displaces(ids[c]))
                        })
                    })
                });
                let (word, bit) = ((tz * tw + tx) / 64, (tz * tw + tx) % 64);
                if found {
                    state.displaced[word] |= 1 << bit;
                } else {
                    state.displaced[word] &= !(1 << bit);
                }
            }
        }
        before || any(&state.displaced, reach)
    }

    fn displacement(&self) -> f32 {
        let Some(relief) = self.splat.relief.as_ref().filter(|r| r.on) else {
            return 0.;
        };
        let (mut cover, mut adds) = (0f32, 0f32);
        for r in &relief.ranges {
            let most = r[0].abs().max(r[1].abs());
            if r[2] > 0.5 {
                adds += most;
            } else {
                cover = cover.max(most);
            }
        }
        cover + adds
    }

    /// The first texel in `r` whose stacked relief spans RELIEF_ROUGH, if
    /// any.
    fn scan(splat: &Splat, layers: &[LayerRelief], r: Rect) -> Option<[u32; 2]> {
        let pairs: Vec<_> = splat.indices.iter().zip(&splat.weights).collect();
        if pairs.is_empty() || r.is_empty() {
            return None;
        }
        // Row by row: copied a byte at a time through an iterator, this
        // took three quarters of the scan.
        let reads: Vec<(Vec<u8>, Vec<u8>)> = (pairs.iter())
            .map(|(ids, w)| (ids.rows(r), w.rows(r)))
            .collect();
        let texels = (r.x1 - r.x0) * (r.z1 - r.z0);
        let alpha = |layer: &LayerRelief, weight: u8| {
            (layer.sharpness * -0.5 + (layer.sharpness + 1.) * (f32::from(weight) / 255.))
                .clamp(0., 1.)
        };
        let mut present: Vec<(u8, u8)> = Vec::with_capacity(4 * pairs.len());
        let width = r.x1 - r.x0;
        (0..texels)
            .find(|&t| {
                // What shows through is at most one, so the spans at their
                // alphas bound the stack: most texels stop here, unsorted.
                let mut bound = 0.;
                for (ids, w) in &reads {
                    for c in t * 4..t * 4 + 4 {
                        if w[c] > 0
                            && let Some(layer) = layers.get(usize::from(ids[c]))
                        {
                            bound += layer.span.max(0.) * alpha(layer, w[c]);
                        }
                    }
                }
                if bound < RELIEF_ROUGH {
                    return false;
                }
                present.clear();
                for (ids, w) in &reads {
                    for c in t * 4..t * 4 + 4 {
                        if w[c] > 0 {
                            present.push((ids[c], w[c]));
                        }
                    }
                }
                present.sort_unstable_by(|a, b| b.0.cmp(&a.0));
                let (mut span, mut through) = (0., 1.);
                for &(id, weight) in &present {
                    let Some(layer) = layers.get(usize::from(id)) else {
                        continue;
                    };
                    let alpha = alpha(layer, weight);
                    span += layer.span * alpha * through;
                    if !layer.adds {
                        through *= 1. - alpha;
                    }
                }
                span >= RELIEF_ROUGH
            })
            .map(|t| [(r.x0 + t % width) as u32, (r.z0 + t / width) as u32])
    }

    /// The control texels under world `area`, with the bicubic's reach.
    fn control_rect(&self, area: [f32; 4]) -> Rect {
        let size = self.splat.indices[0].size();
        let extent = self.grid.size();
        let texel = |i: usize| {
            (area[i] - self.grid.origin[i % 2]) / extent[i % 2].max(self.grid.spacing)
                * size[i % 2] as f32
                - 0.5
        };
        let lo = |i| ((texel(i).floor() - 2.) as usize).min(size[i]);
        let hi = |i| ((texel(i + 2).ceil() + 3.) as usize).min(size[i]);
        Rect {
            x0: lo(0),
            z0: lo(1),
            x1: hi(0),
            z1: hi(1),
        }
    }
}

impl terrain::Ground for A7mGround {
    fn wgsl(&self) -> &str {
        wgsl()
    }
    fn specular(&self) -> gpu::Specular {
        GROUND_SPECULAR
    }
    fn layout(&self) -> &wgpu::BindGroupLayout {
        &self.resident.layout
    }
    fn bind_group(&self) -> Option<&wgpu::BindGroup> {
        let r = &self.resident;
        r.ready
            .load(std::sync::atomic::Ordering::Relaxed)
            .then_some(&r.group)
    }
    fn prepare(&mut self, frame: &mut GroundPrepare<'_>) -> bool {
        self.resident.prepare(frame)
    }
    fn edits(&self, since: u64, out: &mut Vec<[f32; 4]>) -> u64 {
        let state = self.resident.state.lock_any();
        let count = state.serial.saturating_sub(since);
        let [x, z] = self.resident.grid.origin;
        let [w, h] = self.resident.grid.size();
        if count > state.log.len() as u64 {
            out.push([x, z, x + w, z + h]);
        } else {
            for &edit in state.log.iter().skip(state.log.len() - count as usize) {
                out.push(match edit {
                    Edit::Metres([x0, z0, x1, z1]) => [x + x0, z + z0, x + x1, z + z1],
                    Edit::Uv([x0, z0, x1, z1]) => [x + x0 * w, z + z0 * h, x + x1 * w, z + z1 * h],
                });
            }
        }
        state.serial
    }
    fn relief_moves(&self) -> Option<u64> {
        Some(self.resident.state.lock_any().relief_moves)
    }
    fn relief_bound(&self) -> f32 {
        self.resident.displacement()
    }
    fn has_relief(&self) -> bool {
        self.resident.splat.relief.as_ref().is_some_and(|r| r.on)
    }
    fn normal_footprint(&self) -> Option<f32> {
        (!self.resident.splat.normals.is_empty()).then_some(NORMAL_FOOTPRINT)
    }
    fn rough(&self, area: [f32; 4]) -> bool {
        let r = &self.resident;
        if r.relief.is_empty() {
            return false;
        }
        let rect = r.control_rect(area);
        let splat = &r.splat;
        let mut cache = self.rough.lock_any();
        let mut serials = [0; 4];
        let key = |r: Rect| (r.z0, r.x0, r.z1, r.x1);
        let index = cache.binary_search_by_key(&key(rect), |entry| key(entry.rect));
        let previous = index.ok().map(|i| &cache[i]);
        let mut dirty: Option<Rect> = None;
        let witness = previous.as_ref().and_then(|p| p.witness);
        let mut hit = false;
        for (i, m) in splat.indices.iter().chain(&splat.weights).enumerate() {
            let (serial, edits) = m.edits_since(previous.as_ref().map(|p| p.serials[i]));
            serials[i] = serial;
            for edit in edits {
                let overlap = Rect {
                    x0: edit.x0.max(rect.x0),
                    x1: edit.x1.min(rect.x1),
                    z0: edit.z0.max(rect.z0),
                    z1: edit.z1.min(rect.z1),
                };
                if !overlap.is_empty() {
                    dirty = Some(dirty.map_or(overlap, |r| r.union(overlap)));
                    hit |= witness.is_some_and(|[x, z]| {
                        (overlap.x0..overlap.x1).contains(&(x as usize))
                            && (overlap.z0..overlap.z1).contains(&(z as usize))
                    });
                }
            }
        }
        let scan = |rect| Resident::scan(splat, &r.relief, rect);
        let found = if previous.is_none() {
            scan(rect)
        } else if let Some(d) = dirty.filter(|_| witness.is_none() || hit) {
            scan(d).or_else(|| if hit && rect != d { scan(rect) } else { None })
        } else {
            witness
        };
        let entry = RoughArea {
            rect,
            serials,
            witness: found,
        };
        match index {
            Ok(i) => cache[i] = entry,
            Err(i) if cache.len() < 4096 => cache.insert(i, entry),
            // Queries may be arbitrary rectangles. Bound retained scan results.
            Err(_) => {
                cache.clear();
                cache.push(entry);
            }
        }
        found.is_some()
    }
}

// --- Palettes -----------------------------------------------------------------

/// Control texels per palette tile side; palette.wgsl `TILE`, a7m.wgsl
/// `PALETTE_TILE`. Small, so a tile seldom holds more than its eight
/// layers: at 64 texels (24 m on a 768 m island) 4.5% of an overview's
/// pixels changed against merging every layer per pixel, at 8 0.3%. 2048²
/// control maps make 256² tiles, 63 MB of weights with their borders.
const PALETTE_TILE: usize = 8;
/// Texels round each tile whose weight counts toward its eight layers.
const PALETTE_BORDER: usize = 2;
/// Texels each tile's region keeps: the bicubic coverage reads one texel
/// before and two after the pair it sits between, so a tile's reach one
/// before it and two after. The whole border kept took 75 MB on the
/// continental island.
const PALETTE_SIDE: u32 = (PALETTE_TILE + 3) as u32;
/// A build rounds each edge out to a tile and reads the filter border.
const PALETTE_PAD: usize = 2 * (PALETTE_TILE + 4);

/// The control texels a palette build read: its window, and each index
/// then weight map's RGBA rows over it.
struct Window {
    rect: Rect,
    read: Vec<Vec<u8>>,
}

/// One device's palettes for a splat: per tile its eight heaviest layers'
/// ids (two RGBA8 texels) and their weights over the tile and its border
/// (two RGBA8 atlases of `PALETTE_SIDE`² regions). The splat reads these
/// instead of the control maps. Layers past a tile's eight drop there.
///
/// The control maps stay on the CPU: each build writes the texels it reads
/// into a window of scratch textures, gone after the frame. Kept on the
/// GPU they took 64 MiB on the continental island (four 2048² maps) for
/// what only builds read.
struct Palette {
    weights: [wgpu::TextureView; 2],
    ids: [wgpu::TextureView; 2],
    tiles: [u32; 2],
}

fn palette_pipeline(d: &wgpu::Device) -> (wgpu::BindGroupLayout, wgpu::ComputePipeline) {
    gpu::per_device(d, "a7m splat palette", || {
        use wgpu::ShaderStages as S;
        let entry = |binding, ty| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: S::COMPUTE,
            ty,
            count: None,
        };
        let read = wgpu::BindingType::Texture {
            sample_type: wgpu::TextureSampleType::Float { filterable: false },
            view_dimension: wgpu::TextureViewDimension::D2,
            multisampled: false,
        };
        let written = wgpu::BindingType::StorageTexture {
            access: wgpu::StorageTextureAccess::WriteOnly,
            format: wgpu::TextureFormat::Rgba8Unorm,
            view_dimension: wgpu::TextureViewDimension::D2,
        };
        let mut entries = vec![entry(
            0,
            wgpu::BindingType::Buffer {
                ty: wgpu::BufferBindingType::Uniform,
                has_dynamic_offset: false,
                min_binding_size: None,
            },
        )];
        entries.extend((1..=4).map(|b| entry(b, read)));
        entries.extend((5..=8).map(|b| entry(b, written)));
        let layout = d.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("a7m splat palette"),
            entries: &entries,
        });
        let module = d.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("a7m splat palette"),
            source: wgpu::ShaderSource::Wgsl(include_str!("palette.wgsl").into()),
        });
        let pipeline = d.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
            label: Some("a7m splat palette"),
            layout: Some(&d.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("a7m splat palette"),
                bind_group_layouts: &[Some(&layout)],
                immediate_size: 0,
            })),
            module: &module,
            entry_point: Some("cs_palette"),
            compilation_options: Default::default(),
            cache: None,
        });
        (layout, pipeline)
    })
}

/// An empty RGBA8 texture for the palette's atlases and windows.
fn rgba8(
    d: &wgpu::Device,
    label: &str,
    size: [u32; 2],
    usage: wgpu::TextureUsages,
) -> wgpu::Texture {
    d.create_texture(&wgpu::TextureDescriptor {
        label: Some(label),
        size: wgpu::Extent3d {
            width: size[0],
            height: size[1],
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8Unorm,
        usage,
        view_formats: &[],
    })
}

impl Palette {
    fn new(d: &wgpu::Device, control: [usize; 2]) -> Self {
        let tiles = control.map(|c| c.div_ceil(PALETTE_TILE).max(1) as u32);
        let usage = wgpu::TextureUsages::STORAGE_BINDING | wgpu::TextureUsages::TEXTURE_BINDING;
        let atlas = tiles.map(|t| t * PALETTE_SIDE);
        let view = |label, size| rgba8(d, label, size, usage).create_view(&Default::default());
        Self {
            weights: [
                view("splat palette weights", atlas),
                view("splat palette weights", atlas),
            ],
            ids: [
                view("splat palette ids", tiles),
                view("splat palette ids", tiles),
            ],
            tiles,
        }
    }

    /// Rebuilds the tiles `rect` (control texels) reaches, border included,
    /// from `indices` and `weights` (one or two of each), in the frame's
    /// encoder.
    fn build(
        &self,
        frame: &mut GroundPrepare<'_>,
        controls: [&[Arc<ControlMap>]; 2],
        layers: u32,
        rect: Rect,
    ) -> Option<Window> {
        let tile = PALETTE_TILE;
        // A texel also sits in the borders of the tiles next to it.
        let b = PALETTE_BORDER;
        let first = [
            rect.x0.saturating_sub(b) / tile,
            rect.z0.saturating_sub(b) / tile,
        ];
        let end = [(rect.x1 + b).div_ceil(tile), (rect.z1 + b).div_ceil(tile)];
        let end = [
            end[0].min(self.tiles[0] as usize),
            end[1].min(self.tiles[1] as usize),
        ];
        if end[0] <= first[0] || end[1] <= first[1] {
            return None;
        }
        let [indices, weights] = controls;
        // The control texels those tiles read, clamped to the map.
        let size = indices[0].size();
        let window = Rect {
            x0: (first[0] * tile).saturating_sub(b),
            z0: (first[1] * tile).saturating_sub(b),
            x1: (end[0] * tile + b).min(size[0]),
            z1: (end[1] * tile + b).min(size[1]),
        };
        let d = frame.device();
        let globals: [u32; 8] = [
            first[0] as u32,
            first[1] as u32,
            indices.len() as u32,
            layers,
            window.x0 as u32,
            window.z0 as u32,
            size[0] as u32,
            size[1] as u32,
        ];
        let globals = d.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("splat palette"),
            contents: bytemuck::cast_slice(&globals),
            usage: wgpu::BufferUsages::UNIFORM,
        });
        let extent = [window.x1 - window.x0, window.z1 - window.z0].map(|v| v as u32);
        let mut read = Vec::with_capacity(4);
        let mut scratch = |map: &ControlMap| {
            let rows = map.rows(window);
            let usage = wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST;
            let texture = rgba8(d, "splat control window", extent, usage);
            frame.queue().write_texture(
                wgpu::TexelCopyTextureInfo {
                    texture: &texture,
                    mip_level: 0,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::All,
                },
                &rows,
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(extent[0] * 4),
                    rows_per_image: Some(extent[1]),
                },
                wgpu::Extent3d {
                    width: extent[0],
                    height: extent[1],
                    depth_or_array_layers: 1,
                },
            );
            read.push(rows);
            texture.create_view(&Default::default())
        };
        // A single pair binds its maps twice; the shader reads only the
        // first.
        let mut pair = |maps: &[Arc<ControlMap>]| {
            let first = scratch(&maps[0]);
            let second = maps.get(1).map_or_else(|| first.clone(), |m| scratch(m));
            [first, second]
        };
        let [i0, i1] = pair(indices);
        let [w0, w1] = pair(weights);
        drop(pair);
        let views = [&i0, &i1, &w0, &w1];
        let outputs = [
            &self.weights[0],
            &self.weights[1],
            &self.ids[0],
            &self.ids[1],
        ];
        let mut entries = vec![wgpu::BindGroupEntry {
            binding: 0,
            resource: globals.as_entire_binding(),
        }];
        for (i, v) in views.into_iter().chain(outputs).enumerate() {
            entries.push(wgpu::BindGroupEntry {
                binding: i as u32 + 1,
                resource: wgpu::BindingResource::TextureView(v),
            });
        }
        let pipeline = palette_pipeline(d);
        let group = d.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("splat palette"),
            layout: &pipeline.0,
            entries: &entries,
        });
        let mut pass = frame
            .encoder
            .begin_compute_pass(&wgpu::ComputePassDescriptor {
                label: Some("splat palette"),
                timestamp_writes: None,
            });
        pass.set_pipeline(&pipeline.1);
        pass.set_bind_group(0, &group, &[]);
        pass.dispatch_workgroups((end[0] - first[0]) as u32, (end[1] - first[1]) as u32, 1);
        Some(Window { rect: window, read })
    }

    fn whole(&self) -> Rect {
        Rect {
            x0: 0,
            z0: 0,
            x1: self.tiles[0] as usize * PALETTE_TILE,
            z1: self.tiles[1] as usize * PALETTE_TILE,
        }
    }
}

#[cfg(test)]
mod tests;
