// Baked particle effects (`.rdp`): a simulation already run and stored
// tick by tick, not emitter rules. Short-closed XML holds the header and the
// materials; binary CDATA holds each particle's lifetime and every tick it
// lives. Parsed into flat per-channel arrays and written back from them,
// byte for byte for every shipped file. docs/particle-format.md has the
// format, the evidence and how the game draws it.
// Engine addresses here are Anno117.exe build 26043928 (Steam).

use anyhow::{Context, Result, bail, ensure};
use std::ops::Range;

/// A baked effect. Ticks are particle-major: particle `p` owns
/// [`Rdp::life`]`(p)` consecutive ticks, one per frame from `starts[p]` to
/// `ends[p]` inclusive, wrapping past `frame_count - 1` to frame 0 (looped
/// effects do this for particles alive across the loop seam).
///
/// Every field is plain data: edit it and [`Rdp::encode`] writes it. Values
/// left as parsed come back as the bytes they were read from.
#[derive(Clone, Debug)]
pub struct Rdp {
    /// Columns of the game's particle textures; it refuses more than 256.
    pub frame_count: u32,
    /// Milliseconds a frame lasts before the cfg's `TimeScale`.
    pub frame_time: f32,
    pub looped: bool,
    /// Darken the side of the cloud away from the sun.
    pub self_occlusion: bool,
    pub lighting: bool,
    /// Sort particles back to front on the CPU (non-additive materials only).
    pub do_sort: bool,
    /// The effect fades in and out over 1.2 s when shown or hidden instead
    /// of dying after one run.
    pub invincible: bool,
    /// `MaxSH` as x, y, z, w: the game reads it; no shader uses it.
    pub max_sh: [f32; 4],
    /// A scale byte of 255 is this.
    pub max_scale: f32,
    /// Positions are stored as -1..1 of `center ± extents`, object space.
    pub center: [f32; 3],
    pub extents: [f32; 3],
    /// The atlas materials crop from when they play no movie; empty when
    /// the file has none.
    pub texture_path: String,
    pub normal_map_path: String,
    /// The game uploads the first eight.
    pub materials: Vec<Material>,

    /// Per particle: first and last frame alive, and its material.
    pub starts: Vec<u16>,
    pub ends: Vec<u16>,
    pub particle_materials: Vec<u8>,

    /// Per tick: object space, metres, before the cfg's transform.
    pub positions: Vec<[f32; 3]>,
    /// Per tick: three angles in radians (multiples of 2π/255) that
    /// [`rotation`] turns into the quaternion the game draws with.
    pub rotations: Vec<[f32; 3]>,
    /// Per tick: quad width, depth and height, metres.
    pub scales: Vec<[f32; 3]>,
    /// Per tick: RGBA 0..1, multiplied with the texture.
    pub colors: Vec<[f32; 4]>,
    /// Per tick: `T_Occlusion`, four bytes in 71 shipped files and empty
    /// otherwise. 117 does not read it; kept so the file stays whole.
    pub occlusion: Vec<[u8; 4]>,

    // The file's element order, whitespace and value texts; empty for an
    // effect made in the editor, which writes the shipped order.
    layout: Vec<Tok>,
    quantized: Quantized,
}

/// One `<Material>`: how a particle using it is drawn. Defaults are the
/// game's for a field the file leaves out.
#[derive(Clone, Debug, PartialEq)]
pub struct Material {
    /// Draw order among the effect's materials.
    pub order: u8,
    /// Colour multiplier `1 + (0.07 b)^2.2` (29 is about 5.8x).
    pub color_boost: u8,
    /// 0: the quad keeps its rotation in object space; 1: it faces the
    /// camera; 2: it faces the camera but stays upright.
    pub view_aligned: u8,
    pub additive: bool,
    pub refractive: bool,
    /// Drawn into the water ripple simulation instead of the scene.
    pub wave_particle: bool,
    pub churn_particle: bool,
    pub opaque_with_depth: bool,
    /// Light from every point light in reach, not the nearest two.
    pub complex_lighting: bool,
    /// Fade alpha to 0.1 between 20 and 500 m from the camera.
    pub distance_fade: bool,
    /// Vertices follow the terrain under the emitter.
    pub terrain_adapt: bool,
    pub disable_view_angle_fade: bool,
    /// Depth offset scale of six-way-lit flipbooks.
    pub depth_scale: f32,
    /// Flipbook frames per second; 0 plays it once over each particle's
    /// life; -1 (the default) shows no flipbook but `crop` of the atlas.
    pub movie_fps: f32,
    pub movie_motion_multiplier: f32,
    /// `cropUVWH`: atlas u, v, width, height.
    pub crop: [f32; 4],
    /// 0: quads; 1 and 2: volume lines (ribbons along a particle's trail).
    pub line_type: u8,
    pub line_length: f32,
    pub line_tiling: f32,
    /// Offset of the quad's pivot, -1..1 of its size.
    pub center_offset: [f32; 2],
    pub blend_tex_intensity: f32,
    pub movie_tex_path: String,
    pub movie_norm_path: String,
    pub movie_motion_path: String,
    pub lightmap_rtbk_path: String,
    pub lightmap_lbtf_path: String,
    pub albedo_ramp_path: String,
    pub emissive_ramp_path: String,
    spelling: Spelling,
}

// How the file wrote a material: its fields' order and texts, and the
// whitespace before its `<Material>`. Not part of its value: two materials
// that draw the same compare equal.
#[derive(Clone, Debug, Default)]
struct Spelling {
    layout: Vec<Tok>,
    lead: Box<[u8]>,
}

impl PartialEq for Spelling {
    fn eq(&self, _: &Spelling) -> bool {
        true
    }
}

impl Default for Material {
    fn default() -> Material {
        Material {
            order: 0,
            color_boost: 0,
            view_aligned: 0,
            additive: false,
            refractive: false,
            wave_particle: false,
            churn_particle: false,
            opaque_with_depth: false,
            complex_lighting: false,
            distance_fade: false,
            terrain_adapt: false,
            disable_view_angle_fade: false,
            depth_scale: 0.,
            movie_fps: -1.,
            movie_motion_multiplier: 1.,
            crop: [0., 0., 1., 1.],
            line_type: 0,
            line_length: 0.,
            line_tiling: 0.,
            center_offset: [0., 0.],
            blend_tex_intensity: 0.,
            movie_tex_path: String::new(),
            movie_norm_path: String::new(),
            movie_motion_path: String::new(),
            lightmap_rtbk_path: String::new(),
            lightmap_lbtf_path: String::new(),
            albedo_ramp_path: String::new(),
            emissive_ramp_path: String::new(),
            spelling: Spelling::default(),
        }
    }
}

/// The particles alive at one moment, as the game's vertex shader sees
/// them. Parallel arrays, one entry per live particle; [`Rdp::frame`]
/// refills them and keeps their capacity.
#[derive(Clone, Debug, Default)]
pub struct Frame {
    /// Index into the effect's particles.
    pub particle: Vec<u32>,
    /// Object space, before the cfg's transform.
    pub position: Vec<[f32; 3]>,
    /// Quaternion x y z w, blended per component and not normalized: the
    /// shader rotates by it as it comes out of the texture.
    pub rotation: Vec<[f32; 4]>,
    pub scale: Vec<[f32; 3]>,
    pub color: Vec<[f32; 4]>,
    pub material: Vec<u8>,
    /// Frames since the particle was born, and its life in frames. A
    /// flipbook with `movie_fps` 0 is at `age / life` of its run.
    pub age: Vec<f32>,
    pub life: Vec<f32>,
}

impl Frame {
    fn clear(&mut self) {
        self.particle.clear();
        self.position.clear();
        self.rotation.clear();
        self.scale.clear();
        self.color.clear();
        self.material.clear();
        self.age.clear();
        self.life.clear();
    }

    pub fn len(&self) -> usize {
        self.particle.len()
    }

    pub fn is_empty(&self) -> bool {
        self.particle.is_empty()
    }
}

// The positions as read, with the box they were stored in. 112 shipped
// files keep values on an axis whose extent is 0 (the game draws them all
// at the centre), so quantizing the decoded floats cannot give those bytes
// back; a position still equal to its decoded original is written as read.
#[derive(Clone, Debug, Default)]
struct Quantized {
    center: [f32; 3],
    extents: [f32; 3],
    positions: Vec<[i16; 3]>,
}

// Element names the reader gives typed values. The top-level ones, then the
// material ones; `Vector` keys hold x/y/z/w children.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
enum Key {
    FrameCount,
    FrameTime,
    Looped,
    ParticleCount,
    SelfOcclusion,
    Lighting,
    DoSort,
    Invincible,
    MaxSh,
    MaxScale,
    Center,
    Extents,
    TexturePath,
    NormalMapPath,
    Materials,
    PStarts,
    PEnds,
    PMaterials,
    TotalTickCount,
    TPositions,
    TRotations,
    TScales,
    TColors,
    TOcclusion,
    // Material fields.
    Order,
    ColorBoost,
    ViewAligned,
    Refractive,
    ComplexLighting,
    DistanceFade,
    WaveParticle,
    OpaqueWithDepth,
    DepthScale,
    ChurnParticle,
    MovieFps,
    MovieMotionMultiplier,
    CropUvwh,
    Additive,
    LineType,
    LineLength,
    LineTiling,
    ParticleCenterOffset,
    TerrainAdapt,
    DisableViewAngleFade,
    BlendTexIntensity,
    MovieTexPath,
    MovieNormPath,
    MovieMotionPath,
    LightmapRtbkPath,
    LightmapLbtfPath,
    AlbedoRampPath,
    EmissiveRampPath,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Uint,
    Float,
    Bool,
    Text,
    // x/y/z/w children of floats.
    Vector,
    Blob,
    Materials,
}

// (name, key, kind), top-level first. The order is the shipped one, which an
// effect made in the editor is written in.
const TOP: &[(&str, Key, Kind)] = &[
    ("FrameCount", Key::FrameCount, Kind::Uint),
    ("FrameTime", Key::FrameTime, Kind::Float),
    ("Looped", Key::Looped, Kind::Bool),
    ("ParticleCount", Key::ParticleCount, Kind::Uint),
    ("SelfOcclusion", Key::SelfOcclusion, Kind::Bool),
    ("Lighting", Key::Lighting, Kind::Bool),
    ("DoSort", Key::DoSort, Kind::Bool),
    ("Invincible", Key::Invincible, Kind::Bool),
    ("MaxSH", Key::MaxSh, Kind::Vector),
    ("MaxScale", Key::MaxScale, Kind::Float),
    ("Center", Key::Center, Kind::Vector),
    ("Extents", Key::Extents, Kind::Vector),
    ("TexturePath", Key::TexturePath, Kind::Text),
    ("NormalMapPath", Key::NormalMapPath, Kind::Text),
    ("Materials", Key::Materials, Kind::Materials),
    ("P_Starts", Key::PStarts, Kind::Blob),
    ("P_Ends", Key::PEnds, Kind::Blob),
    ("P_Materials", Key::PMaterials, Kind::Blob),
    ("TotalTickCount", Key::TotalTickCount, Kind::Uint),
    ("T_Positions", Key::TPositions, Kind::Blob),
    ("T_Rotations", Key::TRotations, Kind::Blob),
    ("T_Scales", Key::TScales, Kind::Blob),
    ("T_Colors", Key::TColors, Kind::Blob),
    ("T_Occlusion", Key::TOcclusion, Kind::Blob),
];

const MATERIAL: &[(&str, Key, Kind)] = &[
    ("Order", Key::Order, Kind::Uint),
    ("ColorBoost", Key::ColorBoost, Kind::Uint),
    ("ViewAligned", Key::ViewAligned, Kind::Uint),
    ("Refractive", Key::Refractive, Kind::Bool),
    ("ComplexLighting", Key::ComplexLighting, Kind::Bool),
    ("DistanceFade", Key::DistanceFade, Kind::Bool),
    ("WaveParticle", Key::WaveParticle, Kind::Bool),
    ("OpaqueWithDepth", Key::OpaqueWithDepth, Kind::Bool),
    ("DepthScale", Key::DepthScale, Kind::Float),
    ("ChurnParticle", Key::ChurnParticle, Kind::Bool),
    ("MovieFPS", Key::MovieFps, Kind::Float),
    (
        "MovieMotionMultiplier",
        Key::MovieMotionMultiplier,
        Kind::Float,
    ),
    ("cropUVWH", Key::CropUvwh, Kind::Vector),
    ("Additive", Key::Additive, Kind::Bool),
    ("LineType", Key::LineType, Kind::Uint),
    ("LineLength", Key::LineLength, Kind::Float),
    ("LineTiling", Key::LineTiling, Kind::Float),
    (
        "ParticleCenterOffset",
        Key::ParticleCenterOffset,
        Kind::Vector,
    ),
    ("TerrainAdapt", Key::TerrainAdapt, Kind::Bool),
    (
        "DisableViewAngleFade",
        Key::DisableViewAngleFade,
        Kind::Bool,
    ),
    ("BlendTexIntensity", Key::BlendTexIntensity, Kind::Float),
    ("MovieTexPath", Key::MovieTexPath, Kind::Text),
    ("MovieNormPath", Key::MovieNormPath, Kind::Text),
    ("MovieMotionPath", Key::MovieMotionPath, Kind::Text),
    ("LightmapRTBkPath", Key::LightmapRtbkPath, Kind::Text),
    ("LightmapLBtFPath", Key::LightmapLbtfPath, Kind::Text),
    ("AlbedoRampPath", Key::AlbedoRampPath, Kind::Text),
    ("EmissiveRampPath", Key::EmissiveRampPath, Kind::Text),
];

// How many of TOP/MATERIAL a new effect or material writes in full; the
// rest only when set. Firefly_swarm_01's material is the model.
const TOP_ALWAYS: usize = 12;
const MATERIAL_ALWAYS: usize = 18;

fn lookup(table: &[(&'static str, Key, Kind)], name: &[u8]) -> Option<(&'static str, Key, Kind)> {
    table.iter().copied().find(|(n, ..)| n.as_bytes() == name)
}

fn entry(key: Key) -> (&'static str, Key, Kind) {
    TOP.iter()
        .chain(MATERIAL)
        .copied()
        .find(|e| e.1 == key)
        .expect("every key is in a table")
}

// One piece of an element's body, in file order.
#[derive(Clone, Debug)]
enum Tok {
    // Whitespace between elements (four shipped files have stray CRLFs).
    Gap(Box<[u8]>),
    // `<Name>text</>` of a typed value: the text as read, written again
    // while the value still parses to the same thing.
    Leaf(Key, Box<[u8]>),
    // `<Name>` of a vector; `Comp`s and gaps follow, then `End`.
    Vector(Key),
    // `<x>text</>`: axis 0..3 is x, y, z, w.
    Comp(u8, Box<[u8]>),
    End,
    Blob(Key),
    // `<Materials>` from `Rdp::materials`, with the whitespace before its
    // close.
    Materials(Box<[u8]>),
    // An element this reader does not know, verbatim with its close.
    Raw(Box<[u8]>),
}

const CDATA: &[u8] = b"CDATA[";
const CLOSE: &[u8] = b"</>\r\n";
// 1/32767 as the game multiplies a stored position by it.
const POSITION_UNIT: f32 = 0.000_030_518_51;
// The game raises a smaller extent to this on load.
const MIN_EXTENT: f32 = 0.000_001;
const ANGLE_UNIT: f32 = 6.283_185_5;

impl Default for Rdp {
    /// An empty effect, for authoring: one frame of 30 fps, no materials or
    /// particles.
    fn default() -> Rdp {
        Rdp {
            frame_count: 1,
            frame_time: 33.333_332,
            looped: false,
            self_occlusion: false,
            lighting: false,
            do_sort: false,
            invincible: false,
            max_sh: [0.; 4],
            max_scale: 1.,
            center: [0.; 3],
            extents: [1.; 3],
            texture_path: String::new(),
            normal_map_path: String::new(),
            materials: Vec::new(),
            starts: Vec::new(),
            ends: Vec::new(),
            particle_materials: Vec::new(),
            positions: Vec::new(),
            rotations: Vec::new(),
            scales: Vec::new(),
            colors: Vec::new(),
            occlusion: Vec::new(),
            layout: Vec::new(),
            quantized: Quantized::default(),
        }
    }
}

impl Rdp {
    pub fn parse(bytes: &[u8]) -> Result<Rdp> {
        let mut rdp = Rdp {
            invincible: false,
            frame_count: 0,
            extents: [0.; 3],
            max_scale: 0.,
            ..Rdp::default()
        };
        let mut r = Reader { b: bytes, at: 0 };
        let mut blobs: Vec<(Key, &[u8])> = Vec::new();
        let mut seen = 0u64;
        let (mut particle_count, mut tick_count) = (None, None);
        loop {
            if let Some(gap) = r.gap() {
                rdp.layout.push(Tok::Gap(gap));
            }
            if r.at == bytes.len() {
                break;
            }
            let start = r.at;
            let name = r.open()?;
            let Some((name, key, kind)) = lookup(TOP, name) else {
                r.skip_body()?;
                rdp.layout.push(Tok::Raw(bytes[start..r.at].into()));
                continue;
            };
            seen |= 1 << key as u8;
            match kind {
                Kind::Blob => {
                    blobs.push((key, r.blob().with_context(|| format!("<{name}>"))?));
                    rdp.layout.push(Tok::Blob(key));
                }
                Kind::Vector => {
                    rdp.layout.push(Tok::Vector(key));
                    let comps = r
                        .vector(&mut rdp.layout)
                        .with_context(|| format!("<{name}>"))?;
                    for (axis, raw) in comps {
                        rdp.set_comp(key, axis, raw)
                            .with_context(|| format!("<{name}>"))?;
                    }
                }
                Kind::Materials => {
                    let tail = r.materials(&mut rdp.materials)?;
                    rdp.layout.push(Tok::Materials(tail));
                }
                _ => {
                    let raw = r.text().with_context(|| format!("<{name}>"))?;
                    let v = parse_val(kind, raw).with_context(|| format!("<{name}>"))?;
                    match key {
                        Key::ParticleCount => particle_count = Some(v.uint()),
                        Key::TotalTickCount => tick_count = Some(v.uint()),
                        _ => rdp.set(key, v),
                    }
                    rdp.layout.push(Tok::Leaf(key, raw.into()));
                }
            }
        }
        // What the game's loader insists on (0x14262aed0 and the two
        // after it); without one it drops the effect.
        for key in [
            Key::FrameCount,
            Key::FrameTime,
            Key::Looped,
            Key::ParticleCount,
            Key::SelfOcclusion,
            Key::Lighting,
            Key::MaxScale,
            Key::TotalTickCount,
            Key::Materials,
            Key::PStarts,
            Key::PEnds,
            Key::PMaterials,
            Key::TPositions,
            Key::TRotations,
            Key::TScales,
            Key::TColors,
        ] {
            ensure!(seen & (1 << key as u8) != 0, "no <{}>", entry(key).0);
        }
        ensure!(rdp.frame_count > 0, "FrameCount 0");
        rdp.decode_blobs(&blobs)?;
        if let Some(n) = particle_count {
            ensure!(
                n as usize == rdp.starts.len(),
                "ParticleCount {n}, P_Starts holds {}",
                rdp.starts.len()
            );
        }
        if let Some(n) = tick_count {
            ensure!(
                n as usize == rdp.positions.len(),
                "TotalTickCount {n}, T_Positions holds {}",
                rdp.positions.len()
            );
        }
        rdp.check()?;
        Ok(rdp)
    }

    fn decode_blobs(&mut self, blobs: &[(Key, &[u8])]) -> Result<()> {
        let get = |key: Key| blobs.iter().find(|b| b.0 == key).map(|b| b.1);
        let n = get(Key::PStarts).unwrap_or_default().len() / 2;
        let words = |b: &[u8]| -> Vec<u16> {
            b.as_chunks::<2>()
                .0
                .iter()
                .map(|c| u16::from_le_bytes(*c))
                .collect()
        };
        for (key, len) in [
            (Key::PStarts, 2 * n),
            (Key::PEnds, 2 * n),
            (Key::PMaterials, n),
        ] {
            let b = get(key).unwrap_or_default();
            ensure!(
                b.len() == len,
                "<{}> holds {} bytes for {n} particles",
                entry(key).0,
                b.len()
            );
        }
        self.starts = words(get(Key::PStarts).unwrap_or_default());
        self.ends = words(get(Key::PEnds).unwrap_or_default());
        self.particle_materials = get(Key::PMaterials).unwrap_or_default().to_vec();
        let ticks: usize = (0..n).map(|p| self.life(p) as usize).sum();
        for (key, size) in [
            (Key::TPositions, 6),
            (Key::TRotations, 3),
            (Key::TScales, 3),
            (Key::TColors, 4),
            (Key::TOcclusion, 4),
        ] {
            if let Some(b) = get(key) {
                ensure!(
                    b.len() == ticks * size,
                    "<{}> holds {} bytes for {ticks} ticks",
                    entry(key).0,
                    b.len()
                );
            }
        }
        let (center, extents) = (self.center, self.extents.map(|e| e.max(MIN_EXTENT)));
        let q: Vec<[i16; 3]> = (get(Key::TPositions)
            .unwrap_or_default()
            .as_chunks::<6>()
            .0
            .iter())
        .map(|c| [0, 1, 2].map(|k| i16::from_le_bytes([c[2 * k], c[2 * k + 1]])))
        .collect();
        self.positions = q.iter().map(|q| dequantize(*q, center, extents)).collect();
        self.quantized = Quantized {
            center: self.center,
            extents: self.extents,
            positions: q,
        };
        self.rotations = (get(Key::TRotations)
            .unwrap_or_default()
            .as_chunks::<3>()
            .0
            .iter())
        .map(|c| c.map(|v| v as f32 * ANGLE_UNIT / 255.))
        .collect();
        let max_scale = self.max_scale;
        self.scales = (get(Key::TScales)
            .unwrap_or_default()
            .as_chunks::<3>()
            .0
            .iter())
        .map(|c| c.map(|v| v as f32 / 255. * max_scale))
        .collect();
        // Stored B, G, R, A: the game reads each as a little-endian
        // D3DCOLOR.
        self.colors = (get(Key::TColors)
            .unwrap_or_default()
            .as_chunks::<4>()
            .0
            .iter())
        .map(|c| [c[2], c[1], c[0], c[3]].map(|v| v as f32 / 255.))
        .collect();
        self.occlusion = get(Key::TOcclusion)
            .unwrap_or_default()
            .as_chunks::<4>()
            .0
            .to_vec();
        Ok(())
    }

    /// Frames particle `p` lives: `ends[p] - starts[p] + 1`, through the
    /// loop seam when the end is the smaller.
    pub fn life(&self, p: usize) -> u32 {
        let (s, e) = (self.starts[p] as i64, self.ends[p] as i64);
        ((e - s).rem_euclid(self.frame_count.max(1) as i64) + 1) as u32
    }

    /// Each particle's ticks in the per-tick arrays, in particle order.
    pub fn tick_ranges(&self) -> impl Iterator<Item = Range<usize>> + '_ {
        let mut at = 0;
        (0..self.starts.len()).map(move |p| {
            let r = at..at + self.life(p) as usize;
            at = r.end;
            r
        })
    }

    /// Seconds one run takes before the cfg's `TimeScale`.
    pub fn duration(&self) -> f32 {
        self.frame_count as f32 * self.frame_time / 1000.
    }

    /// Reorders particles, and their ticks, by material. The game sorts the
    /// particle list by material before it lays the ticks out, so ticks of a
    /// file not in this order land on the wrong particles (ten shipped 117
    /// effects, all smoke and fire with two materials). Stable.
    pub fn sort_by_material(&mut self) {
        let mut order: Vec<usize> = (0..self.starts.len()).collect();
        order.sort_by_key(|&p| self.particle_materials[p]);
        if order.iter().enumerate().all(|(i, &p)| i == p) {
            return;
        }
        let ranges: Vec<Range<usize>> = self.tick_ranges().collect();
        fn runs<T: Copy>(v: &[T], order: &[usize], ranges: &[Range<usize>]) -> Vec<T> {
            match v.len() == ranges.last().map_or(0, |r| r.end) {
                true => order
                    .iter()
                    .flat_map(|&p| v[ranges[p].clone()].iter().copied())
                    .collect(),
                // Not one per tick: the caller is mid-edit; leave it.
                false => v.to_vec(),
            }
        }
        self.positions = runs(&self.positions, &order, &ranges);
        self.rotations = runs(&self.rotations, &order, &ranges);
        self.scales = runs(&self.scales, &order, &ranges);
        self.colors = runs(&self.colors, &order, &ranges);
        self.occlusion = runs(&self.occlusion, &order, &ranges);
        self.quantized.positions = runs(&self.quantized.positions, &order, &ranges);
        self.starts = order.iter().map(|&p| self.starts[p]).collect();
        self.ends = order.iter().map(|&p| self.ends[p]).collect();
        self.particle_materials = order.iter().map(|&p| self.particle_materials[p]).collect();
    }

    // What the game and the sampler rely on; encode refuses anything else.
    fn check(&self) -> Result<()> {
        let n = self.starts.len();
        ensure!(
            self.ends.len() == n && self.particle_materials.len() == n,
            "{n} starts, {} ends, {} particle materials",
            self.ends.len(),
            self.particle_materials.len()
        );
        // The born texture stores the start frame in a byte.
        ensure!(
            (1..=256).contains(&self.frame_count),
            "FrameCount {} (the game loads 1..=256)",
            self.frame_count
        );
        for p in 0..n {
            ensure!(
                (self.starts[p] as u32) < self.frame_count
                    && (self.ends[p] as u32) < self.frame_count,
                "particle {p} lives {}..={} of {} frames",
                self.starts[p],
                self.ends[p],
                self.frame_count
            );
            ensure!(
                (self.particle_materials[p] as usize) < self.materials.len(),
                "particle {p} has material {} of {}",
                self.particle_materials[p],
                self.materials.len()
            );
        }
        let ticks: usize = (0..n).map(|p| self.life(p) as usize).sum();
        for (name, len) in [
            ("positions", self.positions.len()),
            ("rotations", self.rotations.len()),
            ("scales", self.scales.len()),
            ("colors", self.colors.len()),
        ] {
            ensure!(len == ticks, "{len} {name} for {ticks} ticks");
        }
        ensure!(
            self.occlusion.is_empty() || self.occlusion.len() == ticks,
            "{} occlusion entries for {ticks} ticks",
            self.occlusion.len()
        );
        Ok(())
    }

    /// The file's bytes. Fails when the arrays disagree (particle arrays of
    /// different lengths, ticks that do not add up to the lifetimes, a
    /// material index past the list), on an effect with no material, and on
    /// a `frame_count` outside 1..=256.
    pub fn encode(&self) -> Result<Vec<u8>> {
        self.check()?;
        ensure!(!self.materials.is_empty(), "an effect needs a material");
        let ticks = self.positions.len();
        let mut out = Vec::with_capacity(4096 + ticks * 20);
        let fresh;
        let layout = match self.layout.is_empty() {
            true => {
                // The header, the materials (paths go in before them when
                // set), then the arrays; T_Occlusion only when set, last.
                let arrays = TOP.iter().position(|e| e.1 == Key::PStarts).unwrap();
                let mut toks = canonical(&TOP[..TOP_ALWAYS]);
                toks.push(Tok::Materials(Box::default()));
                toks.extend(canonical(&TOP[arrays..TOP.len() - 1]));
                fresh = toks;
                &fresh
            }
            false => &self.layout,
        };
        let mut seen = 0u64;
        let mut vector = Key::Center;
        for tok in layout {
            match tok {
                Tok::Gap(g) | Tok::Raw(g) => out.extend_from_slice(g),
                Tok::Leaf(key, raw) => {
                    seen |= 1 << *key as u8;
                    let v = match key {
                        Key::ParticleCount => Val::Uint(self.starts.len() as u32),
                        Key::TotalTickCount => Val::Uint(ticks as u32),
                        _ => self.get(*key),
                    };
                    leaf(&mut out, entry(*key).0, raw, v, entry(*key).2);
                }
                Tok::Vector(key) => {
                    seen |= 1 << *key as u8;
                    vector = *key;
                    open(&mut out, entry(*key).0);
                }
                Tok::Comp(axis, raw) => {
                    let v = Val::Float(self.comp(vector, *axis));
                    leaf(&mut out, axis_name(*axis), raw, v, Kind::Float);
                }
                Tok::End => out.extend_from_slice(CLOSE),
                Tok::Blob(key) => {
                    seen |= 1 << *key as u8;
                    // Occlusion is optional: a cleared one drops its element.
                    if *key == Key::TOcclusion && self.occlusion.is_empty() {
                        continue;
                    }
                    self.blob(&mut out, *key);
                }
                Tok::Materials(tail) => {
                    seen |= 1 << Key::Materials as u8;
                    // Fields the file leaves out but the effect now sets go
                    // in before the materials; the game looks them up by
                    // name.
                    self.missing(&mut out, seen);
                    open(&mut out, "Materials");
                    for m in &self.materials {
                        m.encode(&mut out);
                    }
                    out.extend_from_slice(tail);
                    out.extend_from_slice(CLOSE);
                }
            }
        }
        if seen & (1 << Key::TOcclusion as u8) == 0 && !self.occlusion.is_empty() {
            self.blob(&mut out, Key::TOcclusion);
        }
        Ok(out)
    }

    // Header fields absent from the layout whose value is not the default.
    fn missing(&self, out: &mut Vec<u8>, seen: u64) {
        let default = Rdp::default();
        for &(name, key, kind) in TOP {
            // The counts are written where the layout has them, which is
            // always: the reader insists on both.
            if seen & (1 << key as u8) != 0
                || matches!(key, Key::ParticleCount | Key::TotalTickCount)
            {
                continue;
            }
            match kind {
                Kind::Uint | Kind::Float | Kind::Bool | Kind::Text => {
                    if self.get(key) != default.get(key) {
                        leaf(out, name, b"", self.get(key), kind);
                    }
                }
                Kind::Vector => {
                    let n = vector_len(key);
                    if (0..n).any(|a| self.comp(key, a).to_bits() != default.comp(key, a).to_bits())
                    {
                        open(out, name);
                        for a in vector_order(n) {
                            leaf(
                                out,
                                axis_name(a),
                                b"",
                                Val::Float(self.comp(key, a)),
                                Kind::Float,
                            );
                        }
                        out.extend_from_slice(CLOSE);
                    }
                }
                Kind::Blob | Kind::Materials => {}
            }
        }
    }

    fn blob(&self, out: &mut Vec<u8>, key: Key) {
        open(out, entry(key).0);
        out.extend_from_slice(CDATA);
        let len_at = out.len();
        out.extend_from_slice(&[0; 4]);
        match key {
            Key::PStarts => self
                .starts
                .iter()
                .for_each(|v| out.extend_from_slice(&v.to_le_bytes())),
            Key::PEnds => self
                .ends
                .iter()
                .for_each(|v| out.extend_from_slice(&v.to_le_bytes())),
            Key::PMaterials => out.extend_from_slice(&self.particle_materials),
            Key::TPositions => {
                let q = &self.quantized;
                let same_box = q.center.map(f32::to_bits) == self.center.map(f32::to_bits)
                    && q.extents.map(f32::to_bits) == self.extents.map(f32::to_bits);
                let extents = self.extents.map(|e| e.max(MIN_EXTENT));
                for (i, p) in self.positions.iter().enumerate() {
                    let orig = q.positions.get(i).copied().filter(|&o| {
                        same_box
                            && dequantize(o, self.center, extents).map(f32::to_bits)
                                == p.map(f32::to_bits)
                    });
                    let v = orig.unwrap_or_else(|| quantize(*p, self.center, extents));
                    v.iter()
                        .for_each(|c| out.extend_from_slice(&c.to_le_bytes()));
                }
            }
            Key::TRotations => {
                for r in &self.rotations {
                    out.extend(
                        r.map(|a| ((a / ANGLE_UNIT * 255.).round() as i64).rem_euclid(256) as u8),
                    );
                }
            }
            Key::TScales => {
                for s in &self.scales {
                    out.extend(s.map(|v| match self.max_scale > 0. {
                        true => unorm8(v / self.max_scale),
                        false => 0,
                    }));
                }
            }
            Key::TColors => {
                for c in &self.colors {
                    out.extend([c[2], c[1], c[0], c[3]].map(unorm8));
                }
            }
            Key::TOcclusion => self.occlusion.iter().for_each(|o| out.extend_from_slice(o)),
            _ => unreachable!("not a blob: {key:?}"),
        }
        let len = (out.len() - len_at - 4) as u32;
        out[len_at..len_at + 4].copy_from_slice(&len.to_le_bytes());
        out.push(b']');
        out.extend_from_slice(CLOSE);
    }

    fn get(&self, key: Key) -> Val<'_> {
        match key {
            Key::FrameCount => Val::Uint(self.frame_count),
            Key::FrameTime => Val::Float(self.frame_time),
            Key::Looped => Val::Bool(self.looped),
            Key::SelfOcclusion => Val::Bool(self.self_occlusion),
            Key::Lighting => Val::Bool(self.lighting),
            Key::DoSort => Val::Bool(self.do_sort),
            Key::Invincible => Val::Bool(self.invincible),
            Key::MaxScale => Val::Float(self.max_scale),
            Key::TexturePath => Val::Text(&self.texture_path),
            Key::NormalMapPath => Val::Text(&self.normal_map_path),
            Key::ParticleCount => Val::Uint(self.starts.len() as u32),
            Key::TotalTickCount => Val::Uint(self.positions.len() as u32),
            _ => unreachable!("not a header value: {key:?}"),
        }
    }

    fn set(&mut self, key: Key, v: Val) {
        match key {
            Key::FrameCount => self.frame_count = v.uint(),
            Key::FrameTime => self.frame_time = v.float(),
            Key::Looped => self.looped = v.bool(),
            Key::SelfOcclusion => self.self_occlusion = v.bool(),
            Key::Lighting => self.lighting = v.bool(),
            Key::DoSort => self.do_sort = v.bool(),
            Key::Invincible => self.invincible = v.bool(),
            Key::MaxScale => self.max_scale = v.float(),
            Key::TexturePath => self.texture_path = v.text(),
            Key::NormalMapPath => self.normal_map_path = v.text(),
            _ => unreachable!("not a header value: {key:?}"),
        }
    }

    fn comp(&self, key: Key, axis: u8) -> f32 {
        let a = axis as usize;
        match key {
            Key::MaxSh => self.max_sh[a],
            Key::Center => self.center.get(a).copied().unwrap_or(0.),
            Key::Extents => self.extents.get(a).copied().unwrap_or(0.),
            _ => unreachable!("not a header vector: {key:?}"),
        }
    }

    fn set_comp(&mut self, key: Key, axis: u8, raw: &[u8]) -> Result<()> {
        let v = parse_val(Kind::Float, raw)?.float();
        let a = axis as usize;
        match key {
            Key::MaxSh => self.max_sh[a] = v,
            Key::Center if a < 3 => self.center[a] = v,
            Key::Extents if a < 3 => self.extents[a] = v,
            _ => bail!("no {} in <{}>", axis_name(axis), entry(key).0),
        }
        Ok(())
    }

    // The sign the game's texture gives tick `at`'s quaternion against
    // [`rotation`] of it, in the run of ticks starting at `first`. From the
    // second tick on, the game flips each one whose x, y, z point away from
    // the (flipped) one before (0x14262c050), so the blend between them takes
    // the short way: a tick's sign is the one before's, negated when the raw
    // pair's x, y, z dot is negative, and + when it is exactly 0 (a tick
    // without rotation has x, y, z of 0). Walks back to the nearest such
    // reset or the first tick.
    fn run_sign(&self, first: usize, at: usize) -> f32 {
        let (mut sign, mut t) = (1., at);
        let mut r = rotation(self.rotations[at]);
        while t > first {
            let prev = rotation(self.rotations[t - 1]);
            let d = dot3(prev, r);
            if d == 0. {
                break;
            }
            if d < 0. {
                sign = -sign;
            }
            (r, t) = (prev, t - 1);
        }
        sign
    }

    /// The live particles at `time` seconds of effect time (real seconds
    /// divided by the cfg's `TimeScale`), the way the game's vertex shader
    /// samples its textures: blended between the two nearest frames, each
    /// particle shifted back by up to one frame so the ones born on one
    /// frame spread over it. A looped effect wraps; a one-shot one is empty
    /// before 0 and from its [`Rdp::duration`] on, when the game removes it.
    /// Rotations come out up to sign, which rotates the same. Does not
    /// allocate once `out` has held the effect's particles.
    pub fn frame(&self, time: f32, out: &mut Frame) {
        out.clear();
        let duration = self.duration();
        let n = self
            .starts
            .len()
            .min(self.ends.len())
            .min(self.particle_materials.len());
        if duration.is_nan() || duration <= 0. || n == 0 || self.frame_count > 256 {
            return;
        }
        let mut age = time / duration;
        if self.looped {
            age -= age.floor();
        } else if !(0. ..1.).contains(&age) {
            return;
        }
        let f = self.frame_count as f32;
        let frame = age * f;
        // Particles born on each frame, and how many of those come before
        // the current one: the born texture's green channel.
        let mut born = [0u16; 256];
        for &s in &self.starts[..n] {
            born[s as usize & 255] += 1;
        }
        let mut rank = [0u16; 256];
        let mut tick = 0usize;
        for p in 0..n {
            let life = self.life(p) as usize;
            let ticks = tick..tick + life;
            tick = ticks.end;
            let start = self.starts[p] as usize;
            let k = rank[start & 255];
            rank[start & 255] += 1;
            if ticks.end > self.positions.len()
                || ticks.end > self.rotations.len()
                || ticks.end > self.scales.len()
                || ticks.end > self.colors.len()
            {
                return;
            }
            // Stored as a byte: (k / born * 255) truncated.
            let lerp = (k as f32 / born[start & 255] as f32 * 255.)
                .min(255.)
                .floor()
                / 255.;
            let at = frame - (1. - lerp);
            // The born texture holds the life in a byte too; 256 reads 255.
            let span = life.min(255) as f32;
            let alive_for = (at - start as f32 + f).rem_euclid(f);
            if alive_for >= span {
                continue;
            }
            // Texel space: column c holds frame c; blend c0 and the next.
            // c0 is the particle's tick floor(alive_for), alive by the test
            // above; c1 is the next tick, the first again across the seam
            // of a particle alive all loop, or a column it is dead in.
            let count = self.frame_count as usize;
            let u = at.rem_euclid(f);
            let w = u - u.floor();
            let k0 = (alive_for.floor() as usize).min(life - 1);
            let k1 = (k0 + 1) % count;
            let a = ticks.start + k0;
            let b = (k1 < life).then(|| ticks.start + k1);
            let lerp3 = |x: [f32; 3], y: [f32; 3]| [0, 1, 2].map(|i| x[i] + (y[i] - x[i]) * w);
            let lerp4 = |x: [f32; 4], y: [f32; 4]| [0, 1, 2, 3].map(|i| x[i] + (y[i] - x[i]) * w);
            // Both ends as the shader reads them, up to one shared sign
            // (which rotates the same).
            let q0 = rotation(self.rotations[a]);
            let (position, q1, scale, color) = match b {
                Some(b) => (
                    self.positions[b],
                    {
                        let r = rotation(self.rotations[b]);
                        let d = dot3(q0, r);
                        let sign = match (b == a + 1, d) {
                            (true, d) if d < 0. => -1.,
                            (true, d) if d > 0. => 1.,
                            // Not flipped: the game keeps it as built, so its
                            // sign against q0 is q0's own.
                            (true, _) => self.run_sign(ticks.start, a),
                            // Across the seam to the run's first tick, which
                            // is never flipped.
                            (false, _) => self.run_sign(ticks.start, a),
                        };
                        r.map(|c| c * sign)
                    },
                    self.scales[b],
                    self.colors[b],
                ),
                // A dead column has scale and colour 0 (0x14262c5f0,
                // 0x14262c9a0). Its position and rotation are whatever the
                // previous texture left in the game's scratch buffer; hold
                // the last live tick's instead.
                None => (self.positions[a], q0, [0.; 3], [0.; 4]),
            };
            out.particle.push(p as u32);
            out.position.push(lerp3(self.positions[a], position));
            out.rotation.push(lerp4(q0, q1));
            out.scale.push(lerp3(self.scales[a], scale));
            out.color.push(lerp4(self.colors[a], color));
            out.material.push(self.particle_materials[p]);
            out.age.push(alive_for);
            out.life.push(span);
        }
    }
}

/// The quaternion (x, y, z, w) the game builds from a tick's three angles:
/// q = qy(-y) · qz(-z) · qx(-x), i.e. the inverse of rotating about y, then
/// z, then x. Not sign-normalized; [`Rdp::frame`] blends consecutive ticks
/// the way the game does.
pub fn rotation(angles: [f32; 3]) -> [f32; 4] {
    let [x, y, z] = angles.map(|a| a * 0.5);
    let (sx, cx) = x.sin_cos();
    let (sy, cy) = y.sin_cos();
    let (sz, cz) = z.sin_cos();
    [
        sz * sy * cx - cz * cy * sx,
        sz * cy * sx - sy * cz * cx,
        -cy * sz * cx - sy * cz * sx,
        sz * sy * sx + cz * cy * cx,
    ]
}

fn dot3(a: [f32; 4], b: [f32; 4]) -> f32 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

fn dequantize(q: [i16; 3], center: [f32; 3], extents: [f32; 3]) -> [f32; 3] {
    [0, 1, 2].map(|k| q[k] as f32 * POSITION_UNIT * extents[k] + center[k])
}

fn quantize(p: [f32; 3], center: [f32; 3], extents: [f32; 3]) -> [i16; 3] {
    [0, 1, 2].map(|k| {
        ((p[k] - center[k]) / extents[k] * 32767.)
            .round()
            .clamp(-32767., 32767.) as i16
    })
}

fn unorm8(v: f32) -> u8 {
    (v * 255.).round().clamp(0., 255.) as u8
}

impl Material {
    fn encode(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.spelling.lead);
        open(out, "Material");
        let fresh;
        let layout = match self.spelling.layout.is_empty() {
            true => {
                fresh = canonical(&MATERIAL[..MATERIAL_ALWAYS]);
                &fresh
            }
            false => &self.spelling.layout,
        };
        let mut seen = 0u64;
        let mut vector = Key::CropUvwh;
        for tok in layout {
            match tok {
                Tok::Gap(g) | Tok::Raw(g) => out.extend_from_slice(g),
                Tok::Leaf(key, raw) => {
                    seen |= 1 << (*key as u8 - Key::Order as u8);
                    leaf(out, entry(*key).0, raw, self.get(*key), entry(*key).2);
                }
                Tok::Vector(key) => {
                    seen |= 1 << (*key as u8 - Key::Order as u8);
                    vector = *key;
                    open(out, entry(*key).0);
                }
                Tok::Comp(axis, raw) => {
                    let v = Val::Float(self.comp(vector, *axis));
                    leaf(out, axis_name(*axis), raw, v, Kind::Float);
                }
                Tok::End => out.extend_from_slice(CLOSE),
                Tok::Blob(_) | Tok::Materials(_) => unreachable!("not in a material"),
            }
        }
        // Set fields the file left out, at the end: the game finds them by
        // name.
        let default = Material::default();
        for &(name, key, kind) in MATERIAL {
            if seen & (1 << (key as u8 - Key::Order as u8)) != 0 {
                continue;
            }
            if kind == Kind::Vector {
                let n = vector_len(key);
                if (0..n).any(|a| self.comp(key, a).to_bits() != default.comp(key, a).to_bits()) {
                    open(out, name);
                    for a in vector_order(n) {
                        leaf(
                            out,
                            axis_name(a),
                            b"",
                            Val::Float(self.comp(key, a)),
                            Kind::Float,
                        );
                    }
                    out.extend_from_slice(CLOSE);
                }
            } else if self.get(key) != default.get(key) {
                leaf(out, name, b"", self.get(key), kind);
            }
        }
        out.extend_from_slice(CLOSE);
    }

    fn get(&self, key: Key) -> Val<'_> {
        match key {
            Key::Order => Val::Uint(self.order as u32),
            Key::ColorBoost => Val::Uint(self.color_boost as u32),
            Key::ViewAligned => Val::Uint(self.view_aligned as u32),
            Key::Refractive => Val::Bool(self.refractive),
            Key::ComplexLighting => Val::Bool(self.complex_lighting),
            Key::DistanceFade => Val::Bool(self.distance_fade),
            Key::WaveParticle => Val::Bool(self.wave_particle),
            Key::OpaqueWithDepth => Val::Bool(self.opaque_with_depth),
            Key::DepthScale => Val::Float(self.depth_scale),
            Key::ChurnParticle => Val::Bool(self.churn_particle),
            Key::MovieFps => Val::Float(self.movie_fps),
            Key::MovieMotionMultiplier => Val::Float(self.movie_motion_multiplier),
            Key::Additive => Val::Bool(self.additive),
            Key::LineType => Val::Uint(self.line_type as u32),
            Key::LineLength => Val::Float(self.line_length),
            Key::LineTiling => Val::Float(self.line_tiling),
            Key::TerrainAdapt => Val::Bool(self.terrain_adapt),
            Key::DisableViewAngleFade => Val::Bool(self.disable_view_angle_fade),
            Key::BlendTexIntensity => Val::Float(self.blend_tex_intensity),
            Key::MovieTexPath => Val::Text(&self.movie_tex_path),
            Key::MovieNormPath => Val::Text(&self.movie_norm_path),
            Key::MovieMotionPath => Val::Text(&self.movie_motion_path),
            Key::LightmapRtbkPath => Val::Text(&self.lightmap_rtbk_path),
            Key::LightmapLbtfPath => Val::Text(&self.lightmap_lbtf_path),
            Key::AlbedoRampPath => Val::Text(&self.albedo_ramp_path),
            Key::EmissiveRampPath => Val::Text(&self.emissive_ramp_path),
            _ => unreachable!("not a material value: {key:?}"),
        }
    }

    fn set(&mut self, key: Key, v: Val) {
        // Byte fields: the game reads them as bytes.
        let byte = |v: Val| v.uint().min(255) as u8;
        match key {
            Key::Order => self.order = byte(v),
            Key::ColorBoost => self.color_boost = byte(v),
            Key::ViewAligned => self.view_aligned = byte(v),
            Key::Refractive => self.refractive = v.bool(),
            Key::ComplexLighting => self.complex_lighting = v.bool(),
            Key::DistanceFade => self.distance_fade = v.bool(),
            Key::WaveParticle => self.wave_particle = v.bool(),
            Key::OpaqueWithDepth => self.opaque_with_depth = v.bool(),
            Key::DepthScale => self.depth_scale = v.float(),
            Key::ChurnParticle => self.churn_particle = v.bool(),
            Key::MovieFps => self.movie_fps = v.float(),
            Key::MovieMotionMultiplier => self.movie_motion_multiplier = v.float(),
            Key::Additive => self.additive = v.bool(),
            Key::LineType => self.line_type = byte(v),
            Key::LineLength => self.line_length = v.float(),
            Key::LineTiling => self.line_tiling = v.float(),
            Key::TerrainAdapt => self.terrain_adapt = v.bool(),
            Key::DisableViewAngleFade => self.disable_view_angle_fade = v.bool(),
            Key::BlendTexIntensity => self.blend_tex_intensity = v.float(),
            Key::MovieTexPath => self.movie_tex_path = v.text(),
            Key::MovieNormPath => self.movie_norm_path = v.text(),
            Key::MovieMotionPath => self.movie_motion_path = v.text(),
            Key::LightmapRtbkPath => self.lightmap_rtbk_path = v.text(),
            Key::LightmapLbtfPath => self.lightmap_lbtf_path = v.text(),
            Key::AlbedoRampPath => self.albedo_ramp_path = v.text(),
            Key::EmissiveRampPath => self.emissive_ramp_path = v.text(),
            _ => unreachable!("not a material value: {key:?}"),
        }
    }

    fn comp(&self, key: Key, axis: u8) -> f32 {
        let a = axis as usize;
        match key {
            Key::CropUvwh => self.crop[a],
            Key::ParticleCenterOffset => self.center_offset.get(a).copied().unwrap_or(0.),
            _ => unreachable!("not a material vector: {key:?}"),
        }
    }

    fn set_comp(&mut self, key: Key, axis: u8, raw: &[u8]) -> Result<()> {
        let v = parse_val(Kind::Float, raw)?.float();
        let a = axis as usize;
        match key {
            Key::CropUvwh => self.crop[a] = v,
            Key::ParticleCenterOffset if a < 2 => self.center_offset[a] = v,
            _ => bail!("no {} in <{}>", axis_name(axis), entry(key).0),
        }
        Ok(())
    }
}

// A typed value as the file spells it: what a leaf's text parses to.
#[derive(Clone, Copy, Debug)]
enum Val<'a> {
    Uint(u32),
    Float(f32),
    Bool(bool),
    Text(&'a str),
}

impl PartialEq for Val<'_> {
    fn eq(&self, other: &Val) -> bool {
        match (self, other) {
            (Val::Uint(a), Val::Uint(b)) => a == b,
            // By bits: -0 and 0 are different texts.
            (Val::Float(a), Val::Float(b)) => a.to_bits() == b.to_bits(),
            (Val::Bool(a), Val::Bool(b)) => a == b,
            (Val::Text(a), Val::Text(b)) => a == b,
            _ => false,
        }
    }
}

impl Val<'_> {
    fn uint(self) -> u32 {
        match self {
            Val::Uint(v) => v,
            _ => unreachable!(),
        }
    }
    fn float(self) -> f32 {
        match self {
            Val::Float(v) => v,
            _ => unreachable!(),
        }
    }
    fn bool(self) -> bool {
        match self {
            Val::Bool(v) => v,
            _ => unreachable!(),
        }
    }
    fn text(self) -> String {
        match self {
            Val::Text(v) => v.to_owned(),
            _ => unreachable!(),
        }
    }
}

fn parse_val(kind: Kind, raw: &[u8]) -> Result<Val<'_>> {
    let s = std::str::from_utf8(raw).context("text is not UTF-8")?;
    let t = s.trim();
    Ok(match kind {
        // Byte and count fields: 124 shipped materials write ColorBoost as
        // "0.000000", which the game's integer reader takes as 0.
        Kind::Uint => {
            let v: f64 = t.parse().with_context(|| format!("not a number: {t:?}"))?;
            ensure!((0. ..4_294_967_296.).contains(&v), "out of range: {t:?}");
            Val::Uint(v as u32)
        }
        Kind::Float => Val::Float(t.parse().with_context(|| format!("not a number: {t:?}"))?),
        Kind::Bool => {
            let v: f64 = t.parse().with_context(|| format!("not a flag: {t:?}"))?;
            Val::Bool(v != 0.)
        }
        Kind::Text => Val::Text(s),
        _ => unreachable!(),
    })
}

// Writes `<name>text</>` with the text as read while it still means `v`.
fn leaf(out: &mut Vec<u8>, name: &str, raw: &[u8], v: Val, kind: Kind) {
    open(out, name);
    match parse_val(kind, raw).ok() == Some(v) {
        true => out.extend_from_slice(raw),
        false => match v {
            Val::Uint(v) => out.extend_from_slice(v.to_string().as_bytes()),
            Val::Float(v) => out.extend_from_slice(float_text(v).as_bytes()),
            Val::Bool(v) => out.push(b'0' + v as u8),
            Val::Text(v) => out.extend_from_slice(v.as_bytes()),
        },
    }
    out.extend_from_slice(CLOSE);
}

// Shortest text that reads back as the same f32, exponent form for the very
// small and large (shipped files have "5.877472e-39").
fn float_text(v: f32) -> String {
    let a = v.abs();
    match a != 0. && !(1e-5..1e9).contains(&a) {
        true => format!("{v:e}"),
        false => format!("{v}"),
    }
}

fn open(out: &mut Vec<u8>, name: &str) {
    out.push(b'<');
    out.extend_from_slice(name.as_bytes());
    out.push(b'>');
}

fn axis_name(axis: u8) -> &'static str {
    ["x", "y", "z", "w"][axis as usize]
}

fn vector_len(key: Key) -> u8 {
    match key {
        Key::MaxSh | Key::CropUvwh => 4,
        Key::ParticleCenterOffset => 2,
        _ => 3,
    }
}

// Shipped files list four-component vectors w first.
fn vector_order(n: u8) -> Vec<u8> {
    match n {
        4 => vec![3, 0, 1, 2],
        n => (0..n).collect(),
    }
}

// Elements with empty texts, for an effect or material made in the editor;
// the texts are then written from the values.
fn canonical(entries: &[(&str, Key, Kind)]) -> Vec<Tok> {
    let mut out = Vec::new();
    for &(_, key, kind) in entries {
        match kind {
            Kind::Vector => {
                out.push(Tok::Vector(key));
                out.extend(
                    vector_order(vector_len(key))
                        .into_iter()
                        .map(|a| Tok::Comp(a, Box::default())),
                );
                out.push(Tok::End);
            }
            Kind::Blob => out.push(Tok::Blob(key)),
            Kind::Materials => out.push(Tok::Materials(Box::default())),
            _ => out.push(Tok::Leaf(key, Box::default())),
        }
    }
    out
}

// A cursor over the file. Every close is `</>` and CRLF in shipped files;
// whitespace between elements comes back as gaps.
struct Reader<'a> {
    b: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    fn gap(&mut self) -> Option<Box<[u8]>> {
        let start = self.at;
        while self.at < self.b.len() && matches!(self.b[self.at], b' ' | b'\t' | b'\r' | b'\n') {
            self.at += 1;
        }
        (self.at > start).then(|| self.b[start..self.at].into())
    }

    // `<Name>`, returning the name.
    fn open(&mut self) -> Result<&'a [u8]> {
        let rest = &self.b[self.at..];
        ensure!(
            rest.first() == Some(&b'<') && rest.get(1) != Some(&b'/'),
            "byte {}: expected an element",
            self.at
        );
        let Some(gt) = rest.iter().position(|&c| c == b'>') else {
            bail!("byte {}: unterminated tag", self.at);
        };
        self.at += gt + 1;
        Ok(&rest[1..gt])
    }

    fn close(&mut self) -> Result<()> {
        ensure!(
            self.b[self.at..].starts_with(CLOSE),
            "byte {}: expected </> and CRLF",
            self.at
        );
        self.at += CLOSE.len();
        Ok(())
    }

    fn at_close(&self) -> bool {
        self.b[self.at..].starts_with(b"</")
    }

    // A text body and its close.
    fn text(&mut self) -> Result<&'a [u8]> {
        let rest = &self.b[self.at..];
        let Some(lt) = rest.iter().position(|&c| c == b'<') else {
            bail!("byte {}: unterminated text", self.at);
        };
        self.at += lt;
        ensure!(
            self.at_close(),
            "byte {}: expected a value, found an element",
            self.at
        );
        self.close()?;
        Ok(&rest[..lt])
    }

    // `CDATA[<u32 length><bytes>]` and its close.
    fn blob(&mut self) -> Result<&'a [u8]> {
        let rest = &self.b[self.at..];
        let Some(body) = rest.strip_prefix(CDATA) else {
            bail!("byte {}: expected CDATA", self.at);
        };
        let Some(len) = body
            .first_chunk::<4>()
            .map(|n| u32::from_le_bytes(*n) as usize)
        else {
            bail!("byte {}: CDATA without a length", self.at);
        };
        ensure!(
            body.get(4 + len) == Some(&b']'),
            "byte {}: CDATA of {len} bytes does not end in ]",
            self.at
        );
        self.at += CDATA.len() + 4 + len + 1;
        self.close()?;
        Ok(&body[4..4 + len])
    }

    // A vector's children (`<x>…</>`, gaps) as tokens, its close included;
    // returns each component's axis and text.
    fn vector(&mut self, toks: &mut Vec<Tok>) -> Result<Vec<(u8, &'a [u8])>> {
        let mut comps = Vec::new();
        loop {
            if let Some(g) = self.gap() {
                toks.push(Tok::Gap(g));
            }
            if self.at_close() {
                self.close()?;
                toks.push(Tok::End);
                return Ok(comps);
            }
            let name = self.open()?;
            let axis = match name {
                b"x" => 0,
                b"y" => 1,
                b"z" => 2,
                b"w" => 3,
                _ => bail!(
                    "byte {}: <{}> in a vector",
                    self.at,
                    String::from_utf8_lossy(name)
                ),
            };
            let raw = self.text()?;
            comps.push((axis, raw));
            toks.push(Tok::Comp(axis, raw.into()));
        }
    }

    // `<Materials>`' children, its close included; returns the whitespace
    // before that close.
    fn materials(&mut self, out: &mut Vec<Material>) -> Result<Box<[u8]>> {
        loop {
            let gap = self.gap().unwrap_or_default();
            if self.at_close() {
                self.close()?;
                return Ok(gap);
            }
            let name = self.open()?;
            ensure!(
                name == b"Material",
                "byte {}: <{}> in <Materials>",
                self.at,
                String::from_utf8_lossy(name)
            );
            let mut m = Material::default();
            m.spelling.lead = gap;
            self.material(&mut m)
                .with_context(|| format!("material {}", out.len()))?;
            out.push(m);
        }
    }

    fn material(&mut self, m: &mut Material) -> Result<()> {
        loop {
            if let Some(g) = self.gap() {
                m.spelling.layout.push(Tok::Gap(g));
            }
            if self.at_close() {
                return self.close();
            }
            let start = self.at;
            let name = self.open()?;
            let Some((name, key, kind)) = lookup(MATERIAL, name) else {
                self.skip_body()?;
                m.spelling
                    .layout
                    .push(Tok::Raw(self.b[start..self.at].into()));
                continue;
            };
            if kind == Kind::Vector {
                m.spelling.layout.push(Tok::Vector(key));
                let comps = self
                    .vector(&mut m.spelling.layout)
                    .with_context(|| format!("<{name}>"))?;
                for (axis, raw) in comps {
                    m.set_comp(key, axis, raw)
                        .with_context(|| format!("<{name}>"))?;
                }
            } else {
                let raw = self.text().with_context(|| format!("<{name}>"))?;
                let v = parse_val(kind, raw).with_context(|| format!("<{name}>"))?;
                m.set(key, v);
                m.spelling.layout.push(Tok::Leaf(key, raw.into()));
            }
        }
    }

    // Past an element's body and close, whatever it holds.
    fn skip_body(&mut self) -> Result<()> {
        if self.b[self.at..].starts_with(CDATA) {
            self.blob()?;
            return Ok(());
        }
        let rest = &self.b[self.at..];
        let lt = rest.iter().position(|&c| c == b'<').unwrap_or(rest.len());
        if rest[lt..].starts_with(b"</") {
            self.at += lt;
            return self.close();
        }
        loop {
            self.gap();
            if self.at_close() {
                return self.close();
            }
            self.open()?;
            self.skip_body()?;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vfs::{FileId, Install};

    // Two particles over four looped frames: particle 0 lives frames 0..=2,
    // particle 1 is born on frame 3 and lives across the seam to frame 1.
    fn sample() -> Vec<u8> {
        let mut b = Vec::new();
        b.extend(b"<FrameCount>4</>\r\n<FrameTime>125.000000</>\r\n<Looped>1</>\r\n");
        b.extend(
            b"<ParticleCount>2</>\r\n<SelfOcclusion>0</>\r\n<Lighting>1</>\r\n<DoSort>0</>\r\n",
        );
        b.extend(b"<Invincible>1</>\r\n<MaxScale>2</>\r\n");
        b.extend(b"<Center><x>1</>\r\n<y>0</>\r\n<z>0</>\r\n</>\r\n");
        b.extend(b"<Extents><x>2</>\r\n<y>0.000000</>\r\n<z>1</>\r\n</>\r\n");
        b.extend(b"<Materials><Material>\r\n<Order>0</>\r\n<ColorBoost>0.000000</>\r\n");
        b.extend(
            b"<ViewAligned>1</>\r\n<cropUVWH><w>1</>\r\n<x>0</>\r\n<y>0</>\r\n<z>1</>\r\n</>\r\n",
        );
        b.extend(b"<Additive>1</>\r\n<MovieTexPath>data\\a\\b_diff.psd</>\r\n<Unknown><Deep>3</>\r\n</>\r\n");
        b.extend(b"\r\n</>\r\n</>\r\n");
        let blob = |b: &mut Vec<u8>, name: &str, bytes: &[u8]| {
            b.extend(format!("<{name}>CDATA[").bytes());
            b.extend((bytes.len() as u32).to_le_bytes());
            b.extend(bytes);
            b.extend(b"]</>\r\n");
        };
        blob(&mut b, "P_Starts", &[0, 0, 3, 0]);
        blob(&mut b, "P_Ends", &[2, 0, 1, 0]);
        blob(&mut b, "P_Materials", &[0, 0]);
        b.extend(b"<TotalTickCount>6</>\r\n");
        // Six ticks: x runs -32767..32767 on particle 0; y holds data on a
        // zero-extent axis, as 112 shipped files do.
        let pos: [[i16; 3]; 6] = [
            [-32767, 5, 0],
            [0, -7, 0],
            [32767, 9, 32767],
            [100, 0, 0],
            [200, 0, 0],
            [300, 0, 0],
        ];
        blob(
            &mut b,
            "T_Positions",
            &pos.iter()
                .flatten()
                .flat_map(|v| v.to_le_bytes())
                .collect::<Vec<_>>(),
        );
        blob(
            &mut b,
            "T_Rotations",
            &[0, 0, 0, 0, 64, 0, 0, 128, 0, 0, 0, 0, 0, 0, 0, 1, 2, 3],
        );
        blob(
            &mut b,
            "T_Scales",
            &[
                0, 0, 0, 255, 255, 255, 255, 255, 255, 51, 51, 51, 102, 102, 102, 0, 0, 0,
            ],
        );
        // B G R A.
        let colors = [
            [0, 0, 255, 0],
            [0, 0, 255, 255],
            [0, 0, 255, 0],
            [255, 0, 0, 255],
            [255, 0, 0, 255],
            [255, 0, 0, 255],
        ];
        blob(&mut b, "T_Colors", &colors.concat());
        b
    }

    #[test]
    fn decodes_the_channels() {
        let rdp = Rdp::parse(&sample()).unwrap();
        assert_eq!(
            (rdp.frame_count, rdp.frame_time, rdp.looped),
            (4, 125., true)
        );
        assert!(rdp.lighting && rdp.invincible && !rdp.do_sort);
        assert_eq!(rdp.duration(), 0.5);
        assert_eq!(
            (rdp.starts.clone(), rdp.ends.clone()),
            (vec![0, 3], vec![2, 1])
        );
        assert_eq!((rdp.life(0), rdp.life(1)), (3, 3));
        assert_eq!(rdp.tick_ranges().collect::<Vec<_>>(), [0..3, 3..6]);
        // -1..1 of center ± extents; the zero extent reads as the game's
        // minimum, so its stray data stays at the centre.
        let near = |a: [f32; 3], b: [f32; 3]| (0..3).all(|k| (a[k] - b[k]).abs() < 1e-6);
        assert!(
            near(rdp.positions[0], [-1., 0., 0.]),
            "{:?}",
            rdp.positions[0]
        );
        assert!(
            near(rdp.positions[2], [3., 0., 1.]),
            "{:?}",
            rdp.positions[2]
        );
        assert!(rdp.positions[1][1] != 0. && rdp.positions[1][1].abs() < 1e-6);
        assert_eq!(rdp.scales[1], [2.; 3]);
        assert_eq!(rdp.colors[1], [1., 0., 0., 1.]);
        assert_eq!(rdp.colors[3], [0., 0., 1., 1.]);
        assert_eq!(rdp.rotations[1][1], 64. * ANGLE_UNIT / 255.);
        let m = &rdp.materials[0];
        assert_eq!((m.view_aligned, m.color_boost, m.additive), (1, 0, true));
        assert_eq!(m.crop, [0., 0., 1., 1.]);
        assert_eq!(m.movie_tex_path, "data\\a\\b_diff.psd");
        assert_eq!(m.movie_fps, -1., "absent: the game's default");
    }

    #[test]
    fn encodes_byte_for_byte() {
        let bytes = sample();
        assert_eq!(Rdp::parse(&bytes).unwrap().encode().unwrap(), bytes);
    }

    #[test]
    fn edits_read_back() {
        let mut rdp = Rdp::parse(&sample()).unwrap();
        rdp.frame_time = 50.;
        rdp.looped = false;
        rdp.do_sort = true;
        rdp.texture_path = "data/x/atlas.psd".into();
        rdp.max_sh = [1., 2., 3., 4.];
        rdp.materials[0].additive = false;
        rdp.materials[0].crop = [0.5, 0.25, 0.5, 0.75];
        rdp.materials[0].distance_fade = true;
        rdp.materials[0].movie_fps = 0.;
        rdp.positions[3] = [2., 0., -0.5];
        rdp.colors[4] = [0.2, 0.4, 0.6, 0.8];
        rdp.rotations[5] = [1., 2., 3.];
        rdp.scales[4] = [1., 0.5, 0.];
        let back = Rdp::parse(&rdp.encode().unwrap()).unwrap();
        assert_eq!(
            (back.frame_time, back.looped, back.do_sort),
            (50., false, true)
        );
        assert_eq!(back.texture_path, "data/x/atlas.psd");
        assert_eq!(back.max_sh, [1., 2., 3., 4.]);
        let m = &back.materials[0];
        assert!(!m.additive && m.distance_fade);
        assert_eq!((m.crop, m.movie_fps), ([0.5, 0.25, 0.5, 0.75], 0.));
        let close =
            |a: &[f32], b: &[f32], eps: f32| a.iter().zip(b).all(|(a, b)| (a - b).abs() <= eps);
        assert!(
            close(&back.positions[3], &[2., 0., -0.5], 1e-4),
            "{:?}",
            back.positions[3]
        );
        assert!(close(&back.colors[4], &[0.2, 0.4, 0.6, 0.8], 0.5 / 255.));
        assert!(close(&back.rotations[5], &[1., 2., 3.], ANGLE_UNIT / 255.));
        // A step of the scale byte is MaxScale / 255.
        assert!(close(&back.scales[4], &[1., 0.5, 0.], 2. / 255.));
        // Untouched ticks, the stray zero-axis data among them, unchanged.
        let orig = Rdp::parse(&sample()).unwrap();
        assert_eq!(back.positions[..3], orig.positions[..3]);
        assert_eq!(back.quantized.positions[1], [0, -7, 0]);
    }

    #[test]
    fn authors_a_new_effect() {
        let mut rdp = Rdp {
            frame_count: 3,
            looped: true,
            materials: vec![Material {
                view_aligned: 1,
                movie_tex_path: "data/fx/spark_diff.psd".into(),
                ..Material::default()
            }],
            starts: vec![0],
            ends: vec![1],
            particle_materials: vec![0],
            positions: vec![[0.5, 0., 0.], [0., 0.25, 0.]],
            rotations: vec![[0.; 3]; 2],
            scales: vec![[1.; 3]; 2],
            colors: vec![[1.; 4]; 2],
            ..Rdp::default()
        };
        let bytes = rdp.encode().unwrap();
        let text = String::from_utf8_lossy(&bytes);
        assert!(
            text.starts_with("<FrameCount>3</>\r\n<FrameTime>33.333332</>\r\n<Looped>1</>\r\n")
        );
        assert!(text.contains("<cropUVWH><w>1</>\r\n<x>0</>\r\n<y>0</>\r\n<z>1</>\r\n</>\r\n"));
        let back = Rdp::parse(&bytes).unwrap();
        assert_eq!(back.materials[0], rdp.materials[0].clone());
        // Quantized to a 32767th of the extent.
        let d = (back.positions[1][1] - 0.25).abs();
        assert!(d > 0. && d <= 0.5 / 32767., "{d}");
        assert_eq!(back.encode().unwrap(), bytes);
        // The arrays must agree.
        rdp.positions.pop();
        assert!(rdp.encode().is_err());
    }

    // An independent composition of axis rotations, against the game's
    // closed form.
    #[test]
    fn rotation_is_y_z_x_inverse() {
        let mul = |a: [f32; 4], b: [f32; 4]| {
            let ([ax, ay, az, aw], [bx, by, bz, bw]) = (a, b);
            [
                aw * bx + ax * bw + ay * bz - az * by,
                aw * by - ax * bz + ay * bw + az * bx,
                aw * bz + ax * by - ay * bx + az * bw,
                aw * bw - ax * bx - ay * by - az * bz,
            ]
        };
        let axis = |i: usize, a: f32| {
            let mut q = [0., 0., 0., (a / 2.).cos()];
            q[i] = (a / 2.).sin();
            q
        };
        for angles in [[0.3, 1.1, 2.0], [5.0, 0.2, 4.4], [0., 0., 1.]] {
            let [x, y, z] = angles;
            let q = mul(mul(axis(1, -y), axis(2, -z)), axis(0, -x));
            let g = rotation(angles);
            let same = |s: f32| (0..4).all(|i| (q[i] - s * g[i]).abs() < 1e-6);
            assert!(same(1.) || same(-1.), "{angles:?}: {q:?} vs {g:?}");
        }
    }

    // Frames are 0.125 s. Both particles are alone on their birth frame, so
    // each samples one frame behind the clock (the born texture's spread is
    // 0 for the first particle of a frame).
    #[test]
    fn frames_follow_the_game_textures() {
        let rdp = Rdp::parse(&sample()).unwrap();
        let near = |a: f32, b: f32| (a - b).abs() < 1e-5;
        let mut f = Frame::default();
        // Clock 1.5, sampled at 0.5: particle 0 halfway between its first
        // two ticks; particle 1 (born on frame 3) halfway between its ticks
        // 1 and 2, across the loop seam.
        rdp.frame(1.5 * 0.125, &mut f);
        assert_eq!(f.particle, [0, 1]);
        assert!(near(f.position[0][0], 0.), "{:?}", f.position[0]);
        assert_eq!(f.scale[0], [1.; 3]);
        assert_eq!(f.color[0], [1., 0., 0., 0.5]);
        assert_eq!((f.age[0], f.life[0]), (0.5, 3.));
        assert_eq!(f.age[1], 1.5);
        assert!(near(f.scale[1][0], 0.4), "{:?}", f.scale[1]);
        // Looped: one run later is the same picture.
        let mut g = Frame::default();
        rdp.frame(1.5 * 0.125 + rdp.duration(), &mut g);
        assert_eq!(g.particle, f.particle);
        assert!(near(g.position[0][0], f.position[0][0]));
        // Clock 3.5, sampled at 2.5: particle 0 between its last tick and a
        // column it is dead in, so half its scale at its last position;
        // particle 1 is not born again until frame 3 is sampled.
        rdp.frame(3.5 * 0.125, &mut f);
        assert_eq!(f.particle, [0]);
        assert_eq!(f.scale[0], [1.; 3]);
        assert!(near(f.position[0][0], 3.) && near(f.position[0][2], 1.));
        // Past the end it wraps to the start: particle 1 only.
        rdp.frame(4.5 * 0.125, &mut f);
        assert_eq!((f.particle.clone(), f.age.clone()), (vec![1], vec![0.5]));
        // A one-shot effect is over after one run, and not started before.
        let once = Rdp {
            looped: false,
            ..rdp.clone()
        };
        once.frame(once.duration() + 0.01, &mut f);
        assert!(f.is_empty());
        once.frame(-0.01, &mut f);
        assert!(f.is_empty());
    }

    // The second particle born on a frame samples half a frame later than
    // the first: the game spreads a frame's births over it.
    #[test]
    fn births_on_one_frame_spread_over_it() {
        let mut rdp = Rdp::parse(&sample()).unwrap();
        rdp.starts[1] = 0;
        rdp.ends[1] = 2;
        rdp.positions.copy_within(0..3, 3);
        let mut f = Frame::default();
        rdp.frame(1.5 * 0.125, &mut f);
        // The born texture keeps 1/2 as the byte 127.
        assert_eq!(f.age[0], 0.5);
        assert!((f.age[1] - (0.5 + 127. / 255.)).abs() < 1e-6, "{:?}", f.age);
    }

    // The game flips a tick's quaternion when its x, y, z point away from
    // the previous tick's, w left out of the test.
    #[test]
    fn rotation_flips_like_the_game() {
        let mut rdp = Rdp::parse(&sample()).unwrap();
        rdp.rotations[0] = [0.5, 0., 0.];
        rdp.rotations[1] = [0., 1., 1.];
        let (q0, q1) = (rotation(rdp.rotations[0]), rotation(rdp.rotations[1]));
        assert!(q0[0] * q1[0] + q0[1] * q1[1] + q0[2] * q1[2] < 0.);
        let mut f = Frame::default();
        rdp.frame(1.5 * 0.125, &mut f);
        let want: [f32; 4] = [0, 1, 2, 3].map(|i| (q0[i] - q1[i]) * 0.5);
        assert!(
            (0..4).all(|i| (f.rotation[0][i] - want[i]).abs() < 1e-6),
            "{:?} vs {want:?}",
            f.rotation[0]
        );
        // A pure x rotation never flips, so 10 and 250 steps blend through
        // nearly nothing, as in the game.
        rdp.rotations[0] = [10. * ANGLE_UNIT / 255., 0., 0.];
        rdp.rotations[1] = [250. * ANGLE_UNIT / 255., 0., 0.];
        rdp.frame(1.5 * 0.125, &mut f);
        let len = f.rotation[0].iter().map(|c| c * c).sum::<f32>().sqrt();
        assert!(len < 0.2, "{:?}", f.rotation[0]);
    }

    // Against the game's loop run forward over every tick: one particle
    // alive all 64 frames of a looped effect (so the seam blends its last
    // tick into its first), a third of its ticks without rotation and some
    // about one axis only, where the flip test is exactly 0.
    #[test]
    fn rotation_signs_match_the_forward_chain() {
        let mut seed = 0x2545_f491_u32;
        let mut next = || {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            seed
        };
        let rotations: Vec<[f32; 3]> = (0..64)
            .map(|_| {
                let b = [0, 1, 2].map(|_| (next() % 256) as f32 * ANGLE_UNIT / 255.);
                match next() % 6 {
                    0 | 1 => [0.; 3],
                    2 => [b[0], 0., 0.],
                    3 => [0., b[1], 0.],
                    _ => b,
                }
            })
            .collect();
        let rdp = Rdp {
            frame_count: 64,
            frame_time: 125.,
            looped: true,
            materials: vec![Material::default()],
            starts: vec![0],
            ends: vec![63],
            particle_materials: vec![0],
            positions: vec![[0.; 3]; 64],
            rotations: rotations.clone(),
            scales: vec![[1.; 3]; 64],
            colors: vec![[1.; 4]; 64],
            ..Rdp::default()
        };
        let mut chain = vec![rotation(rotations[0])];
        for r in &rotations[1..] {
            let (q, r) = (*chain.last().unwrap(), rotation(*r));
            chain.push(if dot3(q, r) < 0. { r.map(|c| -c) } else { r });
        }
        let mut f = Frame::default();
        for i in 0..512 {
            rdp.frame(i as f32 * 0.125 / 8., &mut f);
            let (k0, w) = (f.age[0].floor() as usize, f.age[0].fract());
            let (a, b) = (chain[k0], chain[(k0 + 1) % 64]);
            let want: [f32; 4] = [0, 1, 2, 3].map(|c| a[c] + (b[c] - a[c]) * w);
            let got = f.rotation[0];
            let same = |s: f32| (0..4).all(|c| (got[c] - s * want[c]).abs() < 1e-5);
            assert!(same(1.) || same(-1.), "moment {i}: {got:?} vs {want:?}");
        }
    }

    #[test]
    fn sorts_ticks_with_their_particles() {
        let mut rdp = Rdp::parse(&sample()).unwrap();
        rdp.materials.push(Material::default());
        rdp.particle_materials = vec![1, 0];
        let p1 = rdp.positions[3..6].to_vec();
        rdp.sort_by_material();
        assert_eq!(
            (rdp.starts.clone(), rdp.particle_materials.clone()),
            (vec![3, 0], vec![0, 1])
        );
        assert_eq!(rdp.positions[..3], p1[..]);
        let back = Rdp::parse(&rdp.encode().unwrap()).unwrap();
        assert_eq!(back.positions, rdp.positions);
    }

    #[test]
    fn rejects_inconsistent_files() {
        let bytes = sample();
        let s = String::from_utf8_lossy(&bytes).replace("<TotalTickCount>6", "<TotalTickCount>7");
        assert!(Rdp::parse(s.as_bytes()).is_err());
        let s = String::from_utf8_lossy(&bytes).replace("<FrameCount>4", "<FrameCount>3");
        assert!(
            Rdp::parse(s.as_bytes()).is_err(),
            "particle 1 starts on frame 3"
        );
    }

    // Every shipped .rdp, shadowed copies included: parse, write back the
    // same bytes, then change one value of each kind and read it back.
    #[test]
    fn shipped_effects_round_trip() {
        for var in ["ANNO117_DIR", "ANNO1800_DIR"] {
            let Some(dir) = crate::vfs::install_dir(var) else {
                eprintln!("skip: {var} unset");
                continue;
            };
            let install = Install::open(dir, Install::DATA_FIRST).unwrap();
            let (mut files, mut copies) = (0, 0);
            let mut frame = Frame::default();
            let mut slowest = (std::time::Duration::ZERO, String::new(), 0);
            for i in 0..install.file_count() {
                let id = FileId(i as u32);
                if !install.file_name(id).ends_with(".rdp") {
                    continue;
                }
                files += 1;
                let path = install.file_path(id);
                for layer in install.layers(id).iter().filter(|l| !l.is_tombstone()) {
                    copies += 1;
                    let bytes = install.read_layer(id, layer).unwrap();
                    let rdp = Rdp::parse(&bytes).unwrap_or_else(|e| panic!("{path}: {e:#}"));
                    let out = rdp.encode().unwrap();
                    assert!(out == bytes, "{path}: encode differs");
                    let (most, took) = play(&path, &rdp, &mut frame);
                    if took > slowest.0 {
                        slowest = (took, path.clone(), most);
                    }
                    edit_and_read_back(&path, rdp);
                }
            }
            eprintln!(
                "{var}: {files} effects, {copies} copies; slowest frame {:?} ({}, {} live)",
                slowest.0, slowest.1, slowest.2
            );
            assert!(files > 300, "{var}: {files} effects");
        }
    }

    // Plays an effect at 32 moments over a run and a quarter: every value
    // finite, no more particles than it has. Returns the most alive at once
    // and the mean time per frame.
    fn play(path: &str, rdp: &Rdp, f: &mut Frame) -> (usize, std::time::Duration) {
        let (mut most, t) = (0, std::time::Instant::now());
        for i in 0..32 {
            rdp.frame(rdp.duration() * i as f32 / 25., f);
            most = most.max(f.len());
            assert!(f.len() <= rdp.starts.len(), "{path}");
            let values = (f.position.iter().flatten())
                .chain(f.rotation.iter().flatten())
                .chain(f.scale.iter().flatten())
                .chain(f.color.iter().flatten());
            assert!(values.copied().all(f32::is_finite), "{path}: moment {i}");
        }
        (most, t.elapsed() / 32)
    }

    fn edit_and_read_back(path: &str, mut rdp: Rdp) {
        rdp.frame_time *= 2.;
        rdp.looped = !rdp.looped;
        let m = &mut rdp.materials[0];
        m.additive = !m.additive;
        m.color_boost = 17;
        m.crop[2] = 0.375;
        let last = rdp.positions.len() - 1;
        let target = [0, 1, 2].map(|k| rdp.center[k] + 0.5 * rdp.extents[k]);
        rdp.positions[last] = target;
        rdp.colors[0] = [0.2, 0.4, 0.6, 0.8];
        let back = Rdp::parse(&rdp.encode().unwrap()).unwrap_or_else(|e| panic!("{path}: {e:#}"));
        assert_eq!(back.frame_time, rdp.frame_time, "{path}");
        assert_eq!(back.looped, rdp.looped, "{path}");
        assert_eq!(back.materials[0], rdp.materials[0], "{path}");
        for (k, want) in target.into_iter().enumerate() {
            let step = rdp.extents[k].max(MIN_EXTENT) / 32767.;
            let d = (back.positions[last][k] - want).abs();
            assert!(d <= step + want.abs() * 1e-6, "{path}: axis {k} off by {d}");
        }
        assert_eq!(
            back.colors[0],
            [51., 102., 153., 204.].map(|v| v / 255.),
            "{path}"
        );
        assert_eq!(back.positions[..last], rdp.positions[..last], "{path}");
    }
}
