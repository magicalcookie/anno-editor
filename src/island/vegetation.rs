// Ground vegetation: the engine's prop sets (`prop_sets.xml`) and a CPU
// stand-in for the game's run-time scatter, drawn as a preview layer. The
// game never stores these props; it regenerates them on the GPU from the
// session's `VegetationPropSetName`, the material maps and the island's
// placed props.
//
// 117's traced rule (`data/shaders/fx/prop_generate.fx`, CPropManager).
// 1800 shares the tier format; its preview uses the same approximation:
// 1. A density map: every placed (PropGrid) prop draws a cone, 1 at its
//    centre falling to 0 at its radius; the map is blurred and multiplied
//    by an unblurred coverage mask, so it is 0 away from any prop.
// 2. Per tier (Large, Medium, Small) candidates sit on a hexagonal grid of
//    circles with the tier's `Radius`. A candidate survives when the
//    density lies in `ThresholdMin..=ThresholdMax` (0–255 scaled to 0–1),
//    all four neighbours one radius away are above sea level, and their
//    slopes differ by at most 35 (percent of the radius).
// 3. The materials are composited front to back, last in the material set
//    first (`opacity *= 1 - coverage`); the one with the most visible
//    opacity picks the ground type, and a prop of the tier with that
//    `GroundType` is chosen by `Probability`. The engine's material list
//    order (`cMaterials`) is taken to be the material set's order: shipped
//    sets list beach first, which as a bottom layer matches how they look.
//
// So Large/Medium tiers ring placed trees and rocks, and a Small tier with
// thresholds 0..255 fills every matching material. The preview skips the
// blur and the Vegetate mask (only the Debug set uses it), and reads
// `Probability` as percent of candidates; how the game rolls it was not
// traced.
use super::doc::{Materials, trs};
use crate::terrain::tag;
use anno_formats::game::Ambients;
use anno_formats::vfs::Install;
use anyhow::Result;
use gpui_editor_kit::{heightfield::Heightfield, math::Mat4};
use std::collections::BTreeMap;

/// Preview instances at most. A 512 m island's Small tier alone yields
/// ~100k candidates; past this the preview thins evenly instead of
/// stalling the viewport.
pub const MAX_INSTANCES: usize = 40_000;
/// Cone radius of a placed prop in the density map, per unit of its
/// horizontal scale.
// TODO(alex): the game passes a radius per prop instance
// (`cStaticPropInstanceData.z`); where it comes from (model bounds?) was not
// traced. 4 m makes Large bushes hug trees about as in game screenshots.
const STATIC_RADIUS: f32 = 4.;
/// Density map texels per metre.
const DENSITY_PER_METRE: f32 = 2.;
/// `prop_generate.fx`: neighbour slope differences above this reject a spot.
const MAX_SLOPE_DIFFERENCE: f32 = 35.;

/// One prop a tier can place.
#[derive(Clone, Debug, PartialEq)]
pub struct SetProp {
    /// Model cfg, forward slashes.
    pub file: String,
    pub ground_type: String,
    /// Percent of candidates on that ground type.
    pub probability: f32,
    pub scale: [f32; 2],
    /// Only placed under the Vegetate mask; the preview skips these.
    pub vegetate: bool,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Tier {
    pub radius: f32,
    /// Density window, 0–1.
    pub threshold: [f32; 2],
    pub props: Vec<SetProp>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct PropSet {
    pub name: String,
    /// Large, Medium, Small.
    pub tiers: [Tier; 3],
    /// Ground types with a GPU grass material (blade shader); not previewed.
    pub grass: Vec<String>,
}

impl PropSet {
    /// Whether any tier grows something on `ground_type`.
    pub fn grows_on(&self, ground_type: &str) -> bool {
        self.tiers
            .iter()
            .flat_map(|t| &t.props)
            .any(|p| p.probability > 0. && matches(p, ground_type))
    }

    /// Up to `n` model cfgs this preset grows most of, by summed odds:
    /// what it looks like, for the preset picker's pictures.
    pub fn main_plants(&self, n: usize) -> Vec<&str> {
        let mut odds: Vec<(&str, f32)> = Vec::new();
        for p in self.tiers.iter().flat_map(|t| &t.props) {
            if p.vegetate || p.probability <= 0. {
                continue;
            }
            match odds.iter_mut().find(|(f, _)| *f == p.file) {
                Some((_, o)) => *o += p.probability,
                None => odds.push((&p.file, p.probability)),
            }
        }
        odds.sort_by(|a, b| b.1.total_cmp(&a.1));
        odds.into_iter().take(n).map(|(f, _)| f).collect()
    }
}

fn matches(p: &SetProp, ground_type: &str) -> bool {
    !p.vegetate && (p.ground_type == ground_type || p.ground_type == "All")
}

pub struct PropSets {
    pub sets: Vec<PropSet>,
}

impl PropSets {
    pub fn load(install: &Install) -> Result<Self> {
        let bytes = install.read(install.game().profile().prop_sets_path)?;
        Ok(Self::parse(&String::from_utf8_lossy(&bytes)))
    }

    /// Parses Anno's short-closed XML. Tier fields come before the first
    /// `<Prop>` of their section; the `<Grass>` section follows the tiers.
    pub fn parse(xml: &str) -> Self {
        let sets = xml
            .split("<PropSet>")
            .skip(1)
            .filter_map(|block| {
                let name = tag(block, "Name")?.to_owned();
                let section = |open: &str| -> &str {
                    let Some(start) = block.find(open) else {
                        return "";
                    };
                    let rest = &block[start + open.len()..];
                    let end = ["<Large>", "<Medium>", "<Small>", "<Grass>"]
                        .iter()
                        .filter_map(|m| rest.find(m))
                        .min()
                        .unwrap_or(rest.len());
                    &rest[..end]
                };
                let tier = |open: &str| {
                    let text = section(open);
                    let head = text.split("<Prop>").next().unwrap_or("");
                    let number = |s: &str, name: &str, default: f32| {
                        tag(s, name)
                            .and_then(|v| v.trim_end_matches('f').parse().ok())
                            .unwrap_or(default)
                    };
                    Tier {
                        radius: number(head, "Radius", 1.).max(0.1),
                        threshold: [
                            number(head, "ThresholdMin", 0.) / 255.,
                            number(head, "ThresholdMax", 255.) / 255.,
                        ],
                        props: text
                            .split("<Prop>")
                            .skip(1)
                            .filter_map(|p| {
                                Some(SetProp {
                                    file: tag(p, "FileName")?.replace('\\', "/"),
                                    ground_type: tag(p, "GroundType")?.to_owned(),
                                    // Parser defaults: 100,
                                    // 0.5, 1.5.
                                    probability: number(p, "Probability", 100.),
                                    scale: [
                                        number(p, "MinScaleFactor", 0.5),
                                        number(p, "MaxScaleFactor", 1.5),
                                    ],
                                    vegetate: number(p, "Vegetate", 0.) != 0.,
                                })
                            })
                            .collect(),
                    }
                };
                let grass = section("<Grass>")
                    .split("<GrassMaterial>")
                    .skip(1)
                    .filter_map(|g| tag(g, "GroundType").map(str::to_owned))
                    .collect();
                Some(PropSet {
                    name,
                    tiers: [tier("<Large>"), tier("<Medium>"), tier("<Small>")],
                    grass,
                })
            })
            .collect();
        Self { sets }
    }

    pub fn get(&self, name: &str) -> Option<&PropSet> {
        self.sets.iter().find(|s| s.name == name)
    }

    /// The region's shipped family, plus the engine's default and no props.
    pub fn choices(&self, region: &str) -> Vec<&str> {
        let family = match region {
            "Celtic" => "ls_wetlands",
            "Egypt" => "ls_egypt",
            "Roman" => "ls_heartlands",
            "Moderate" => "Moderate",
            "Colony01" => "Colony",
            "Arctic" => "Arctic",
            "Africa" => "Colony02",
            _ => return Vec::new(),
        };
        self.sets
            .iter()
            .map(|s| s.name.as_str())
            .filter(|n| {
                (*n == family || n.starts_with(&format!("{family}_")))
                    || matches!(*n, "None" | "Default")
            })
            .collect()
    }
}

/// Instances of the scattered props, grouped by model.
#[derive(Clone)]
pub struct Preview {
    pub models: BTreeMap<String, Vec<Mat4>>,
    /// Candidates before thinning to the bound, as of the last full scatter.
    pub candidates: usize,
    /// The even thinning's cut (`roll >> 32` at most), kept so a rescatter
    /// of a window thins as the full scatter did.
    keep: u64,
}

impl Preview {
    pub fn len(&self) -> usize {
        self.models.values().map(Vec::len).sum()
    }
}

/// What the scatter reads from the island.
pub struct Ground<'a> {
    pub field: &'a Heightfield,
    pub materials: &'a Materials,
    /// (material id, `GroundType`) in material set order.
    pub layers: &'a [(u8, String)],
    /// Placed props: ground position (x, z) and horizontal scale.
    pub statics: &'a [([f32; 2], f32)],
}

impl Ground<'_> {
    /// The ground type the game reads at `p`: the material with the most
    /// visible opacity, compositing from the end of the set (nearest texel;
    /// the game blends the four around it).
    pub fn ground_type(&self, p: [f32; 2]) -> Option<&str> {
        let (ids, weights) = self.materials.slots(p);
        let mut opacity = 1.;
        let mut best: Option<(&str, f32)> = None;
        for (id, ground_type) in self.layers.iter().rev() {
            let coverage: f32 = (0..6)
                .filter(|&k| ids[k] == *id)
                .map(|k| f32::from(weights[k]) / 31.)
                .sum::<f32>()
                .min(1.);
            if coverage == 0. {
                continue;
            }
            let visible = opacity * coverage;
            if best.is_none_or(|(_, b)| visible > b) {
                best = Some((ground_type, visible));
            }
            opacity *= 1. - coverage;
        }
        best.map(|(g, _)| g)
    }
}

/// Scatters `set` over the island like the game would (see the module
/// top), deterministic for the same input, at most `max` instances.
pub fn scatter(set: &PropSet, ground: &Ground, max: usize) -> Preview {
    let size = ground.field.grid.size();
    let mut candidates = candidates(set, ground, [0., 0., size[0], size[1]]);
    let total = candidates.len();
    let mut keep = u64::MAX;
    if total > max {
        // Even thinning: keep a hash-chosen share, then trim the rest.
        keep = (max as f64 / total as f64 * u32::MAX as f64) as u64;
        candidates.retain(|c| (c.roll >> 32) <= keep);
        candidates.truncate(max);
    }
    let mut preview = Preview {
        models: BTreeMap::new(),
        candidates: total,
        keep,
    };
    preview.grow(set, candidates);
    preview
}

/// Scatters `set` again where the island changed under each of `changed`
/// (world `[x0, z0, x1, z1]`), leaving the rest as it is. Every candidate reads
/// only the ground within a tier radius of it, so the window grows by the
/// largest radius. A dab on a 512 m island rescatters in well under a
/// millisecond where the whole island takes 60 ms.
///
/// The thinning share stays the full scatter's, and the bound is not
/// re-applied: a stroke that grows many new candidates can pass `max`
/// until the next full scatter.
pub fn rescatter(preview: &mut Preview, set: &PropSet, ground: &Ground, changed: &[[f32; 4]]) {
    let reach = (set.tiers.iter())
        .filter(|t| !t.props.is_empty())
        .map(|t| t.radius)
        .fold(0f32, f32::max);
    // One window after another: a later one that overlaps an earlier one
    // drops what that grew inside it and grows the same again.
    for c in changed {
        let window = [c[0] - reach, c[1] - reach, c[2] + reach, c[3] + reach];
        for ms in preview.models.values_mut() {
            ms.retain(|m| !inside(window, [m[12], m[14]]));
        }
        let mut candidates = candidates(set, ground, window);
        candidates.retain(|c| (c.roll >> 32) <= preview.keep);
        preview.grow(set, candidates);
    }
}

/// Half-open, as both the scatter and the rescatter's removal read it: an
/// instance stands exactly on its candidate's point.
fn inside(w: [f32; 4], p: [f32; 2]) -> bool {
    p[0] >= w[0] && p[0] < w[2] && p[1] >= w[1] && p[1] < w[3]
}

struct Candidate {
    prop: (usize, usize),
    at: [f32; 3],
    roll: u64,
}

/// The candidates that survive inside `window` (world `[x0, z0, x1, z1]`).
fn candidates(set: &PropSet, ground: &Ground, window: [f32; 4]) -> Vec<Candidate> {
    let size = ground.field.grid.size();
    let density = Density::new(ground.statics, size, window);
    let mut out = Vec::new();
    for (t, tier) in set.tiers.iter().enumerate() {
        if tier.props.is_empty() {
            continue;
        }
        let r = tier.radius;
        let (dx, dz) = (2. * r, 3f32.sqrt() * r);
        let (nx, nz) = ((size[0] / dx) as u64, (size[1] / dz) as u64);
        // Grid indices the window can hold, a step wider each side;
        // `inside` decides.
        let span = |lo: f32, hi: f32, step: f32, n: u64| {
            let a = ((lo - r) / step).floor().max(1.) as u64 - 1;
            let b = (((hi - r) / step).ceil().max(0.) as u64 + 1).min(n);
            a..b
        };
        for iz in span(window[1], window[3], dz, nz) {
            let z = iz as f32 * dz + r;
            let shift = if iz % 2 == 1 { r } else { 0. };
            for ix in span(window[0] - shift, window[2] - shift, dx, nx) {
                let x = ix as f32 * dx + r + shift;
                if x >= size[0] || !inside(window, [x, z]) {
                    continue;
                }
                let d = density.at([x, z]);
                if d < tier.threshold[0] || d > tier.threshold[1] {
                    continue;
                }
                let Some(y) = settle(ground.field, [x, z], r) else {
                    continue;
                };
                let Some(ground_type) = ground.ground_type([x, z]) else {
                    continue;
                };
                let roll = hash(t as u64, ix, iz);
                let Some(p) = pick(tier, ground_type, roll) else {
                    continue;
                };
                out.push(Candidate {
                    prop: (t, p),
                    at: [x, y, z],
                    roll,
                });
            }
        }
    }
    out
}

impl Preview {
    fn grow(&mut self, set: &PropSet, candidates: Vec<Candidate>) {
        for c in candidates {
            let p = &set.tiers[c.prop.0].props[c.prop.1];
            let unit = |shift: u32| ((c.roll >> shift) & 0xffff) as f32 / 65535.;
            let s = p.scale[0] + (p.scale[1] - p.scale[0]) * unit(8);
            let yaw = unit(24) * std::f32::consts::TAU;
            let q = [0., (yaw / 2.).sin(), 0., (yaw / 2.).cos()];
            self.models
                .entry(p.file.clone())
                .or_default()
                .push(trs(c.at, q, [s; 3]));
        }
    }
}

/// The prop of `tier` for `ground_type` that `roll` lands on, if any.
/// Probabilities are percent; a ground type whose props sum below 100
/// leaves the rest of its candidates empty.
fn pick(tier: &Tier, ground_type: &str, roll: u64) -> Option<usize> {
    let fitting = || {
        tier.props
            .iter()
            .enumerate()
            .filter(|(_, p)| matches(p, ground_type))
    };
    let sum: f32 = fitting().map(|(_, p)| p.probability.max(0.)).sum();
    if sum <= 0. {
        return None;
    }
    let mut u = (roll & 0xffff) as f32 / 65536. * sum.max(100.);
    for (i, p) in fitting() {
        u -= p.probability.max(0.);
        if u < 0. {
            return Some(i);
        }
    }
    None
}

/// Ground height at `p` if the game would place a prop there: no neighbour
/// one radius away under water, no sharp break in slope.
fn settle(field: &Heightfield, p: [f32; 2], r: f32) -> Option<f32> {
    let hc = field.height(p);
    let h =
        [[-r, 0.], [r, 0.], [0., -r], [0., r]].map(|o| field.height([p[0] + o[0], p[1] + o[1]]));
    if h.iter().any(|&v| v < 0.) {
        return None;
    }
    let slope = h.map(|v| 100. / r * (v - hc).abs());
    slope[1..]
        .iter()
        .all(|s| (s - slope[0]).abs() <= MAX_SLOPE_DIFFERENCE)
        .then_some(hc)
}

/// The prop density map: cones around placed props, max-blended. Holds
/// only the texels of a window, addressed as the whole island's are, so a
/// rescatter reads the values a full scatter would.
struct Density {
    /// The island's texels across and down; `at` clamps to them.
    width: usize,
    height: usize,
    /// The window's first texel and its texels across and down.
    origin: [usize; 2],
    span: [usize; 2],
    texels: Vec<f32>,
}

impl Density {
    fn new(statics: &[([f32; 2], f32)], size: [f32; 2], window: [f32; 4]) -> Self {
        let width = (size[0] * DENSITY_PER_METRE).ceil() as usize + 1;
        let height = (size[1] * DENSITY_PER_METRE).ceil() as usize + 1;
        // Texels `at` rounds window points to.
        let texel =
            |v: f32, n: usize| ((v * DENSITY_PER_METRE).round().max(0.) as usize).min(n - 1);
        let origin = [texel(window[0], width), texel(window[1], height)];
        let end = [texel(window[2], width) + 1, texel(window[3], height) + 1];
        let span = [end[0] - origin[0], end[1] - origin[1]];
        let mut texels = vec![0f32; span[0] * span[1]];
        for &(p, scale) in statics {
            let r = STATIC_RADIUS * scale.max(0.1);
            let cover = |c: f32, lo: usize, hi: usize| {
                let a = (((c - r) * DENSITY_PER_METRE).floor().max(0.) as usize).max(lo);
                let b = (((c + r) * DENSITY_PER_METRE).ceil().max(0.) as usize).min(hi);
                a..b.max(a)
            };
            for ty in cover(p[1], origin[1], end[1]) {
                for tx in cover(p[0], origin[0], end[0]) {
                    let (x, z) = (tx as f32 / DENSITY_PER_METRE, ty as f32 / DENSITY_PER_METRE);
                    let v = 1. - (x - p[0]).hypot(z - p[1]) / r;
                    let t = &mut texels[(ty - origin[1]) * span[0] + tx - origin[0]];
                    *t = t.max(v);
                }
            }
        }
        Self {
            width,
            height,
            origin,
            span,
            texels,
        }
    }

    /// Density at `p`, which must lie in the window.
    fn at(&self, p: [f32; 2]) -> f32 {
        let x = ((p[0] * DENSITY_PER_METRE).round().max(0.) as usize).min(self.width - 1);
        let z = ((p[1] * DENSITY_PER_METRE).round().max(0.) as usize).min(self.height - 1);
        let (x, z) = (x - self.origin[0], z - self.origin[1]);
        debug_assert!(x < self.span[0] && z < self.span[1]);
        self.texels[z * self.span[0] + x]
    }
}

/// Reach of a placed prop's density cone, for a prop of horizontal scale
/// `scale`: moving it changes the vegetation that far around.
pub fn prop_reach(scale: f32) -> f32 {
    STATIC_RADIUS * scale.max(0.1)
}

/// splitmix64 over a tier and grid cell: the same island always scatters
/// the same way.
fn hash(tier: u64, x: u64, z: u64) -> u64 {
    let mut v = tier
        .wrapping_mul(0x9e37_79b9_7f4a_7c15)
        .wrapping_add(x << 32 ^ z)
        .wrapping_add(0x9e37_79b9_7f4a_7c15);
    v = (v ^ (v >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    v = (v ^ (v >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    v ^ (v >> 31)
}

/// Ambient presets an island of `region` can pick: the install's
/// `ambientsettings/amb_<region>_*` and `amb_default_*`, without benchmark,
/// interior and test scenes. Shipped islands use `amb_roman_01`,
/// `amb_celtic_01`/`_03` and a few one-offs.
pub fn ambients(install: &Install, region: &str) -> Vec<String> {
    let kind = install.game().profile().ambients;
    let prefix = match kind {
        Ambients::Files => format!("amb_{}_", region.to_ascii_lowercase()),
        Ambients::Named => match region {
            "Colony01" => "south_america_caribic_".into(),
            "Arctic" => "dlc03_".into(),
            "Africa" => "colony_02".into(),
            _ => "moderate_".into(),
        },
    };
    let mut out: Vec<String> = install
        .search("config/engine/ambientsettings/", 2000)
        .into_iter()
        .filter_map(|id| {
            let name = install.file_name(id).strip_suffix(".xml")?.to_owned();
            let skip = [
                "benchmark",
                "_int_",
                "test",
                "ign_",
                "senate",
                "_bw",
                "sepia",
            ];
            if !(name.starts_with(&prefix) || name.starts_with("amb_default_"))
                || skip.iter().any(|s| name.contains(s))
            {
                return None;
            }
            match kind {
                Ambients::Files => Some(name),
                Ambients::Named => {
                    if !install.file_path(id).contains("/ambientsettings/local/")
                        || ["_old", "_movie", "_gc_", "_ls_"]
                            .iter()
                            .any(|s| name.contains(s))
                    {
                        return None;
                    }
                    let bytes = install.read_file(id).ok()?;
                    tag(&String::from_utf8_lossy(&bytes), "Name").map(str::to_owned)
                }
            }
        })
        .collect();
    out.sort();
    out.dedup();
    out
}

/// How an ambient preset lights the island, in the game's linear units:
/// `LightColor` is the sun's radiance, `GroundColorAmbient` the light from
/// below, `Exposure` scales before tone mapping, `SunAO` is how much
/// occlusion maps shade the sun, and `MaxSunElevation` (radians) is how high
/// the sun stands. The sky's own light and look come from the game's
/// atmosphere model, which this does not run; `sky` (its light) and
/// `zenith`/`horizon` (how it looks) are fixed stand-ins, the horizon a
/// pale haze as in game.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Ambient {
    pub sun: [f32; 3],
    pub sky: [f32; 3],
    pub zenith: [f32; 3],
    pub horizon: [f32; 3],
    pub ground: [f32; 3],
    pub exposure: f32,
    pub sun_occlusion: f32,
    pub elevation: f32,
}

impl Default for Ambient {
    /// `amb_roman_01` and `amb_celtic_01`, which differ only in `SunAO`.
    fn default() -> Self {
        Self {
            sun: [4.197, 3.597, 3.198],
            sky: [1.0, 1.12, 1.3],
            zenith: [0.32, 0.58, 1.15],
            horizon: [1.05, 1.2, 1.4],
            ground: [0.2; 3],
            exposure: 0.55,
            sun_occlusion: 0.16,
            elevation: 0.65,
        }
    }
}

impl Ambient {
    /// The 117 presets among `names` that have a file, with defaults for
    /// what each leaves out. Empty for 1800, whose presets light by time of
    /// day curves this does not read.
    pub fn read_all<'a>(
        install: &Install,
        names: impl IntoIterator<Item = &'a String>,
    ) -> Vec<(String, Ambient)> {
        if install.game().profile().ambients != Ambients::Files {
            return Vec::new();
        }
        let files = install.search("config/engine/ambientsettings/", 2000);
        (names.into_iter())
            .filter_map(|name| {
                let file = format!("{name}.xml");
                let id = (files.iter())
                    .find(|&&id| install.file_name(id).eq_ignore_ascii_case(&file))?;
                Some((name.clone(), Self::read(install, *id)?))
            })
            .collect()
    }

    fn read(install: &Install, id: anno_formats::vfs::FileId) -> Option<Ambient> {
        let xml = String::from_utf8_lossy(&install.read_file(id).ok()?).into_owned();
        let number = |name: &str| tag(&xml, name)?.parse::<f32>().ok();
        // `<LightColor><x>4.197</><y>…</><z>…</></>`
        let color = |name: &str| -> Option<[f32; 3]> {
            let open = format!("<{name}>");
            let rest = &xml[xml.find(&open)? + open.len()..];
            let mut out = [0.; 3];
            for (v, axis) in out.iter_mut().zip(["x", "y", "z"]) {
                *v = tag(rest, axis)?.parse().ok()?;
            }
            Some(out)
        };
        let d = Ambient::default();
        Some(Ambient {
            sun: color("LightColor").unwrap_or(d.sun),
            ground: color("GroundColorAmbient").unwrap_or(d.ground),
            exposure: number("Exposure").unwrap_or(d.exposure),
            sun_occlusion: number("SunAO").unwrap_or(d.sun_occlusion),
            elevation: number("MaxSunElevation").unwrap_or(d.elevation),
            ..d
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{Ground, MAX_INSTANCES, PropSets, scatter};
    use crate::island::doc::{Doc, Materials};
    use crate::terrain::tag;
    use anno_formats::{island::Island, rda, session::SessionSettings, vfs::Install};

    #[test]
    fn installed_1800_regional_vegetation_previews_and_saves() {
        use crate::island::IslandEditor;
        let Some(dir) = std::env::var_os("ANNO1800_DIR") else {
            return;
        };
        let install = Install::open(dir).unwrap();
        let sets = PropSets::load(&install).unwrap();
        for (region, name, ambient) in [
            ("Moderate", "Moderate", "Moderate_01"),
            ("Colony01", "Colony", "south_america_caribic_01"),
            ("Arctic", "Arctic", "DLC03_01"),
            ("Africa", "Colony02", "Colony_02"),
        ] {
            assert!(sets.choices(region).contains(&name));
            assert!(sets.choices(region).contains(&"None"));
            assert!(
                super::ambients(&install, region).contains(&ambient.to_owned()),
                "{region}: ambient"
            );
            if region == "Colony01" {
                assert!(!sets.choices(region).contains(&"Colony02"));
            }
            let path = install
                .search(".a7m", 100_000)
                .into_iter()
                .find_map(|id| {
                    let path = install.file_path(id);
                    if !path.ends_with(".a7m") || install.game().island_region(path) != region {
                        return None;
                    }
                    let archive =
                        rda::Archive::from_bytes(install.read_file(id).ok()?, path).ok()?;
                    let island = Island::parse(
                        &archive.read_path("gamedata.data").ok()?,
                        &archive.read_path("rd3d.data").ok()?,
                    )
                    .ok()?;
                    (SessionSettings::read(island.gamedata())
                        .ok()?
                        .vegetation_prop_set
                        == name
                        && island.props().len() > 20)
                        .then(|| path.to_owned())
                })
                .unwrap();
            let mut editor = IslandEditor::load(&install, &path).unwrap();
            editor.layers.vegetation = true;
            let preview = editor.vegetation_preview().unwrap();
            assert!(
                preview.len() > 0 && preview.len() <= MAX_INSTANCES,
                "{region}: {} props",
                preview.len()
            );
            for cfg in preview.models.keys().take(3) {
                let model = anno_formats::model::Model::load(&install, cfg).unwrap();
                assert!(
                    model.parts.iter().any(|p| p.mesh.is_some()),
                    "{region}: {cfg}"
                );
            }
            editor.edit_session(|s| s.vegetation_prop_set = "None".into());
            assert_eq!(editor.vegetation_preview().unwrap().len(), 0);
            let mut source = editor.source.clone();
            editor.doc.write_into(&mut source).unwrap();
            let (gd, rd) = source.to_members().unwrap();
            let saved = Island::parse(&gd, &rd).unwrap();
            assert_eq!(
                SessionSettings::read(saved.gamedata())
                    .unwrap()
                    .vegetation_prop_set,
                "None"
            );
            assert!(editor.undo(false));
            assert_eq!(
                editor.vegetation_preview().unwrap().models,
                preview.models,
                "{region}: undo"
            );
        }
    }

    #[test]
    fn parses_tiers_props_and_grass() {
        let xml = "<PropSets><GroundTypes><GroundType><Name>All</></></>\
            <PropSet><Name>a</><Large><Radius>1</><ThresholdMin>100</><ThresholdMax>255</>\
            <Prop><FileName>x\\big.cfg</><GroundType>Grass</><Probability>30</></></>\
            <Small><Radius>0.5</><ThresholdMin>0</><ThresholdMax>255</>\
            <Prop><FileName>x\\small.cfg</><GroundType>Soil</><Probability>12</>\
            <MinScaleFactor>1</><MaxScaleFactor>2</></></>\
            <Grass><GrassMaterial><GroundType>Grass</><BladeHeight>0.3f</></></></>\
            <PropSet><Name>None</></></>";
        let sets = PropSets::parse(xml);
        assert_eq!(sets.sets.len(), 2);
        let a = sets.get("a").unwrap();
        assert_eq!(a.tiers[0].radius, 1.);
        assert!((a.tiers[0].threshold[0] - 100. / 255.).abs() < 1e-6);
        assert_eq!(a.tiers[0].props[0].file, "x/big.cfg");
        assert_eq!(a.tiers[0].props[0].scale, [0.5, 1.5], "parser defaults");
        assert!(a.tiers[1].props.is_empty());
        assert_eq!(a.tiers[2].props[0].scale, [1., 2.]);
        assert_eq!(a.grass, ["Grass"]);
        assert!(a.grows_on("Soil") && !a.grows_on("Rock"));
        assert!(
            sets.get("None")
                .unwrap()
                .tiers
                .iter()
                .all(|t| t.props.is_empty())
        );
    }

    // Shipped roman island with its own set: instances land only on
    // materials whose ground type the set grows on, two runs agree, and the
    // bound holds. The chosen set survives a save.
    #[test]
    fn preview_follows_ground_types_and_set_survives_save() {
        let Some(dir) = std::env::var_os("ANNO117_DIR") else {
            eprintln!("skip: ANNO117_DIR unset");
            return;
        };
        let install = Install::open(dir).unwrap();
        let sets = PropSets::load(&install).unwrap();
        for name in ["ls_heartlands_01", "ls_wetlands_01", "None"] {
            assert!(sets.get(name).is_some(), "{name}");
        }
        assert!(sets.choices("Roman").contains(&"ls_heartlands_01_arid"));
        assert!(!sets.choices("Roman").contains(&"ls_wetlands_01"));

        let path = install
            .search("roman_island_medium_01.a7m", 4)
            .into_iter()
            .map(|id| install.file_path(id).to_owned())
            .find(|p| p.ends_with(".a7m"))
            .unwrap();
        let archive =
            rda::Archive::from_bytes(install.read(&path).unwrap(), path.as_str()).unwrap();
        let member = |n: &str| archive.read_path(n).unwrap();
        let mut island = Island::parse(&member("gamedata.data"), &member("rd3d.data")).unwrap();
        let materials = Materials::load_level0(&install, &path, &island).unwrap().0;
        let mut doc = Doc::from_island(&island, Some(materials)).unwrap();
        let set = sets.get(&doc.session.vegetation_prop_set).unwrap();

        // Ground types straight from the material set xml, so the test
        // does not depend on textures decoding.
        let xml = String::from_utf8_lossy(&install.read(island.material_set_file_name()).unwrap())
            .into_owned();
        let layers: Vec<(u8, String)> = xml
            .split("<Material>")
            .skip(1)
            .filter_map(|b| {
                Some((
                    tag(b, "ID")?.parse().ok()?,
                    tag(b, "GroundType")?.to_owned(),
                ))
            })
            .collect();
        let statics: Vec<([f32; 2], f32)> = doc
            .props
            .iter()
            .map(|p| ([p.matrix[12], p.matrix[14]], 1.))
            .collect();
        let ground = Ground {
            field: &doc.field,
            materials: doc.materials.as_ref().unwrap(),
            layers: &layers,
            statics: &statics,
        };
        let a = scatter(set, &ground, MAX_INSTANCES);
        assert!(a.len() > 1000, "{} instances", a.len());
        assert!(a.len() <= MAX_INSTANCES);
        for m in a.models.values().flatten() {
            let gt = ground.ground_type([m[12], m[14]]).unwrap();
            assert!(set.grows_on(gt), "instance on {gt} at {:?}", [m[12], m[14]]);
            assert!(m[13] >= 0., "under water");
        }
        let b = scatter(set, &ground, MAX_INSTANCES);
        assert_eq!(a.models, b.models, "deterministic");
        let small = scatter(set, &ground, 500);
        assert!(small.len() <= 500 && small.len() > 250, "{}", small.len());
        assert_eq!(small.candidates, a.candidates);
        // An empty set scatters nothing.
        assert_eq!(
            scatter(sets.get("None").unwrap(), &ground, MAX_INSTANCES).len(),
            0
        );

        doc.session.vegetation_prop_set = "ls_heartlands_01_volcanic".into();
        doc.write_into(&mut island).unwrap();
        let (gd, rd) = island.to_members().unwrap();
        let reread = Island::parse(&gd, &rd).unwrap();
        assert_eq!(
            SessionSettings::read(reread.gamedata())
                .unwrap()
                .vegetation_prop_set,
            "ls_heartlands_01_volcanic"
        );
    }
}

#[cfg(test)]
mod ambient_tests {
    use super::Ambient;
    use anno_formats::vfs::Install;

    // Needs the game: the presets differ in exposure and sun occlusion.
    #[test]
    fn ambient_presets_read_their_light() {
        let Some(dir) = std::env::var_os("ANNO117_DIR") else {
            eprintln!("skipping: ANNO117_DIR unset");
            return;
        };
        let install = Install::open(dir).unwrap();
        let names = ["amb_celtic_03", "amb_roman_01", "no_such_preset"].map(String::from);
        let read = Ambient::read_all(&install, &names);
        let get = |name: &str| read.iter().find(|(n, _)| n == name).map(|(_, a)| *a);
        let celtic = get("amb_celtic_03").unwrap();
        assert_eq!((celtic.exposure, celtic.sun_occlusion), (0.65, 0.198));
        assert!((celtic.sun[0] - 4.197).abs() < 1e-3 && celtic.ground == [0.2; 3]);
        assert!((get("amb_roman_01").unwrap().elevation - 0.644444).abs() < 1e-5);
        assert!(get("no_such_preset").is_none());
    }
}
