// Converts an island between Anno 1800 and Anno 117, or between two
// regions of one game (Latium and Albion; Old World, New World, Arctic and
// Enbesa). Heights, tint and
// river and lake curves carry over as they are. Ground materials and prop
// models are mapped by role (rock, beach, conifer, bush…) onto what the
// target province ships. Harbours and slots are placed again from the
// target's own records. The Import wizard (`import.rs`) drives it.
use super::catalog::Look;
use super::{
    IslandEditor, Prop,
    catalog::Catalog,
    doc::{self, Materials, Paint},
    harbour::{self, Resource},
    new::LandEnv,
    river, tint,
};
use crate::terrain::Palette;
use anno_core::game::{GroundNames, IslandRules, Module};
use anno_core::harbour::Piece;
use anno_core::island::objects::{Objects, PathPoint, Template};
use anno_core::island::{Island, Layers, Level, Problem, Stack};
use anno_formats::{
    a7m::{grids::Kind, island, tint::TintMap},
    adf::Adf,
    assets::Assets,
    model::Transform,
    rda::Archive,
    vfs::Install,
};
use anyhow::Context as _;
use gpui_editor_kit::history::History;
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

/// What a prop model is, for matching it with one of the other game's.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Role {
    Broadleaf,
    Conifer,
    Palm,
    DeadTree,
    Stump,
    Bush,
    Reed,
    Grass,
    Underwater,
    Cliff,
    Rock,
    Pebbles,
}

impl Role {
    pub fn label(self) -> &'static str {
        match self {
            Role::Broadleaf => "broadleaf tree",
            Role::Conifer => "conifer",
            Role::Palm => "palm",
            Role::DeadTree => "dead tree",
            Role::Stump => "stump",
            Role::Bush => "bush",
            Role::Reed => "reed",
            Role::Grass => "grass & flowers",
            Role::Underwater => "underwater plant",
            Role::Cliff => "cliff",
            Role::Rock => "rock",
            Role::Pebbles => "pebbles",
        }
    }

    /// The nearest role, for when the target province ships none of this
    /// one: Roman islands have no palms, 1800's Arctic no broadleaf trees.
    fn fallback(self) -> Option<Role> {
        Some(match self {
            Role::Palm => Role::Broadleaf,
            Role::Conifer => Role::Broadleaf,
            Role::Broadleaf => Role::Conifer,
            Role::DeadTree => Role::Stump,
            Role::Stump => Role::DeadTree,
            Role::Reed => Role::Bush,
            Role::Grass => Role::Bush,
            Role::Cliff => Role::Rock,
            Role::Rock => Role::Pebbles,
            Role::Pebbles => Role::Rock,
            Role::Bush | Role::Underwater => return None,
        })
    }
}

/// The role of a prop model, from the words in its path. Both games file
/// props under `…/trees/`, `…/bushes/`, `…/rocks/` and name them after what
/// they are, which is enough to sort the few hundred each ships.
pub fn prop_role(path: &str) -> Option<Role> {
    let p = path.to_ascii_lowercase().replace('\\', "/");
    let name = p.rsplit('/').next().unwrap_or(&p);
    let has = |words: &[&str]| words.iter().any(|w| p.contains(w));
    if has(&["placeholder", "/test", "dummy", "/dev/"]) {
        return None;
    }
    Some(
        if has(&["underwater", "seaweed", "coral", "seagrass", "kelp"]) {
            Role::Underwater
        } else if has(&["reed", "cattail"]) {
            Role::Reed
        } else if has(&["/trees/", "/tree_"]) || name.starts_with("tree") || name.contains("palm") {
            if has(&["stump"]) {
                Role::Stump
            } else if has(&["dead"]) {
                Role::DeadTree
            } else if has(&["palm"]) {
                Role::Palm
            } else if has(&[
                "conifer", "fir_", "pine", "spruce", "cypress", "cedar", "larch",
            ]) {
                Role::Conifer
            } else {
                Role::Broadleaf
            }
        } else if has(&["bush", "shrub", "fern", "hedge", "gorse"]) {
            Role::Bush
        } else if has(&["flower", "grass", "/plants/", "plant_"]) {
            Role::Grass
        } else if has(&["cliff", "mountain"]) {
            Role::Cliff
        } else if has(&["pebble", "gravel"]) {
            Role::Pebbles
        } else if has(&["rock", "stone", "boulder"]) {
            Role::Rock
        } else {
            return None;
        },
    )
}

/// 0 for the region's own models, 1 for shared ones, `None` for another
/// region's.
pub fn fit(rules: &IslandRules, region: &str, path: &str) -> Option<u8> {
    let p = path.to_ascii_lowercase();
    let mut foreign = false;
    for (r, words) in rules.region_words {
        if words.iter().any(|w| p.contains(w)) {
            if r.eq_ignore_ascii_case(region) {
                return Some(0);
            }
            foreign = true;
        }
    }
    (!foreign).then_some(1)
}

/// `fit` for a catalog model, by where shipped islands place it as well as
/// by its path: one placed only in other provinces is another region's,
/// whatever folder it sits in.
fn fit_placed(rules: &IslandRules, region: &str, e: &super::catalog::PropEntry) -> Option<u8> {
    let elsewhere =
        !e.regions.is_empty() && !e.regions.iter().any(|r| r.eq_ignore_ascii_case(region));
    if elsewhere {
        return None;
    }
    fit(rules, region, &e.path)
}

/// Models placed fewer times than this over all shipped islands are mostly
/// one-off scenery or leftovers; they make poor stand-ins.
const MIN_USES: u32 = 3;
/// Target models offered per role. Source models of one role spread over
/// these, so a forest of five tree kinds stays mixed.
const CHOICES: usize = 6;

/// The target's models for `role` in `region`, best first: the region's
/// own before shared ones, then by how often shipped islands place them.
pub fn candidates(catalog: &Catalog, rules: &IslandRules, region: &str, role: Role) -> Vec<String> {
    let mut found: Vec<(u8, u32, &str)> = catalog
        .props
        .iter()
        .filter(|e| e.uses >= MIN_USES && prop_role(&e.path) == Some(role))
        .filter_map(|e| Some((fit_placed(rules, region, e)?, e.uses, e.path.as_str())))
        .collect();
    found.sort_by(|a, b| a.0.cmp(&b.0).then(b.1.cmp(&a.1)).then(a.2.cmp(b.2)));
    found
        .into_iter()
        .take(CHOICES)
        .map(|f| f.2.to_owned())
        .collect()
}

/// The target props look matching considers: placed often enough to be
/// real scenery (see `MIN_USES`), from every region. Effects (waterfalls,
/// foam, smoke) are left out: their thumbnails are whatever mesh carries
/// the particles, and a sparse tree once matched a waterfall.
pub fn match_pool(catalog: &Catalog) -> impl Iterator<Item = &str> {
    (catalog.props.iter())
        .filter(|e| e.uses >= MIN_USES && !e.path.contains("/effects/"))
        .map(|e| e.path.as_str())
}

/// How different two props look: 0 for the same, about 1 for unrelated.
/// Silhouette (Jaccard distance of the 8x8 masks), outline raggedness and
/// size (log ratios, so twice as tall counts as much as half) weigh most,
/// then color. Matching ranks by `look_parts` directly; tests compare whole.
#[cfg(test)]
pub fn look_distance(a: &Look, b: &Look) -> f32 {
    let (look, size) = look_parts(a, b);
    look + 0.8 * size
}

/// The two halves of [`look_distance`]: everything but size, and size.
/// Confidence weighs size less, since replacements are scaled to height.
fn look_parts(a: &Look, b: &Look) -> (f32, f32) {
    let shape =
        (a.mask ^ b.mask).count_ones() as f32 / (a.mask | b.mask).count_ones().max(1) as f32;
    let ratio = |x: f32, y: f32| (x.max(0.05) / y.max(0.05)).ln().abs();
    let foot = |l: &Look| l.size[0].max(l.size[2]);
    let size = (ratio(a.size[1], b.size[1]) + 0.5 * ratio(foot(a), foot(b))) / 1.5;
    let delta =
        |p: [f32; 3], q: [f32; 3]| (0..3).map(|i| (p[i] - q[i]).powi(2)).sum::<f32>().sqrt();
    // 50 ΔE: grass green against rock grey.
    let color = (delta(a.top, b.top) + delta(a.bottom, b.bottom)) / 100.;
    // Foliage (0.3 and up) against stone (under 0.1) costs the most; a
    // sparse crown against a dense one should not outweigh the rest.
    let ragged = ((a.ragged - b.ragged).abs() * 2.5).min(0.5);
    (
        shape + 0.8 * color + 0.3 * ratio(a.aspect, b.aspect) + ragged,
        size,
    )
}

/// The target props that look most like `source`, best first, with the
/// confidence of each (see [`confidence`]), from every
/// region: names only nudge (a different kind of prop costs a little, as
/// does another region's), so a dark boulder finds the dark rock even
/// when the two games name them differently.
pub fn look_alikes(
    catalog: &Catalog,
    rules: &IslandRules,
    region: &str,
    source: (&str, &Look),
    look_of: impl Fn(&str) -> Option<Look>,
) -> Vec<(String, f32)> {
    let role = prop_role(source.0);
    // `source` comes placed; targets compare as their islands place them.
    let pool: std::collections::HashSet<&str> = match_pool(catalog).collect();
    // (distance, confidence, path)
    let mut scored: Vec<(f32, f32, &str)> = (catalog.props.iter())
        .filter(|e| pool.contains(e.path.as_str()))
        .filter_map(|e| {
            let other = prop_role(&e.path);
            // Unnamed kinds are mostly building parts: quays, stairs, power
            // lines. Trees and rocks skip them. Otherwise 117's maritime pines
            // matched 1800's harbour beams.
            if role.is_some() && other.is_none() {
                return None;
            }
            let look = look_of(&e.path)?.scaled(e.scale);
            let kind = match (role, other) {
                (Some(a), Some(b)) if a != b => 0.6,
                (None, Some(_)) => 0.3,
                _ => 0.,
            };
            // Never placed in this province: not a stand-in for it, or a
            // region conversion would hand every prop back to itself.
            if !e.regions.is_empty() && fit_placed(rules, region, e).is_none() {
                return None;
            }
            let place = match fit(rules, region, &e.path) {
                Some(0) => 0.,
                Some(_) => 0.03,
                None => 0.1,
            };
            let (alike, size) = look_parts(source.1, &look);
            Some((
                alike + 0.8 * size + kind + place,
                confidence(alike + 0.25 * size + kind),
                e.path.as_str(),
            ))
        })
        .collect();
    scored.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.2.cmp(b.2)));
    scored
        .into_iter()
        .take(CHOICES)
        .map(|(_, sure, p)| (p.to_owned(), sure))
        .collect()
}

/// One source model and what it becomes. `pick` indexes `choices`; `None`
/// drops its instances.
#[derive(Clone, Debug, PartialEq)]
pub struct PropRow {
    pub source: String,
    pub count: usize,
    /// Median vertical scale the source island places it at.
    pub placed: f32,
    pub role: Option<Role>,
    pub choices: Vec<String>,
    pub pick: Option<usize>,
    /// Multiplies each instance's scale so the pick stands as tall as the
    /// source did (see [`height_scale`]); 1 when either look is unknown.
    pub scale: f32,
    /// How alike the best match looks, 0..1 (see [`confidence`]); `None`
    /// when matched by name.
    pub confidence: Option<f32>,
    /// The ported models one source placement becomes, with their offsets.
    /// Filled in by the wizard after the port ran. Empty means place the target
    /// alone.
    pub pieces: Vec<(String, Transform)>,
    /// Effects one source placement becomes, placed as objects. Filled in with
    /// `pieces`.
    pub effects: Vec<(String, Transform)>,
}

/// Best matches below this confidence bring the source game's own model
/// instead, when it can be ported. The import wizard's "Import below" slider
/// changes it. On an 1800 jungle island this brings most large jungle and dead
/// trees; the same palm scores 99%, rocks 60-90%, bushes 35-65%. On a Roman 117
/// island it brings the maritime pines (about 20%), since 1800 has no tree like
/// them.
pub const BRING_BELOW: f32 = 0.35;

/// Confidence of a match from its look distance: 1 for the same model,
/// about 0.5 for a fair stand-in, under 0.2 for a poor one.
pub fn confidence(distance: f32) -> f32 {
    (-distance).exp()
}

impl PropRow {
    /// Whether the row places the source game's own model, ported. A model
    /// kept in a region conversion is not: it is already in the game.
    pub fn brings(&self) -> bool {
        self.target().is_some_and(super::port::is_ported)
    }
}

/// Instance scale factor that makes `target` as tall as `source`, both as
/// models; clamped to 0.25..4 so a mismatch never makes giants.
pub fn height_scale(source: Option<Look>, target: Option<Look>) -> f32 {
    match (source, target) {
        (Some(s), Some(t)) if t.size[1] > 0.01 => (s.size[1] / t.size[1]).clamp(0.25, 4.),
        _ => 1.,
    }
}

impl PropRow {
    pub fn target(&self) -> Option<&str> {
        self.pick
            .and_then(|i| self.choices.get(i))
            .map(String::as_str)
    }
}

/// Asset templates that aren't scenery. People and animals are rigs that carry
/// every tool their animations hold, and markers are the white boxes. A prop
/// can't walk.
const NOT_SCENERY: [&str; 4] = ["Inhabitant", "LandAnimal", "Herd", "PositionMarker"];

/// The model an object places as a prop, as the editor draws it: its own cfg,
/// else its asset's variation. Gameplay buildings only have the variation.
/// Rivers, lakes, coasts and slots convert separately, other curve objects have
/// no single spot, and units aren't scenery.
fn object_model(
    src: &Island,
    from: &dyn Module,
    assets: Option<&Assets>,
    o: &anno_core::island::objects::Object,
) -> Option<String> {
    if assets.is_some_and(|a| NOT_SCENERY.contains(&harbour::template_of(a, o.guid).as_str())) {
        return None;
    }
    if let Some(c) = &o.config {
        return Some(c.replace('\\', "/"));
    }
    let assets = assets?;
    if o.path.is_some()
        || river::water_kind(src.harbour.as_deref(), Some(assets), o.guid).is_some()
        || harbour::piece_of(from, assets, o.guid).is_some()
    {
        return None;
    }
    let cfgs = assets.get(o.guid).map_or(&[][..], |a| a.cfgs.as_slice());
    o.model(cfgs).map(|m| m.replace('\\', "/"))
}

/// Every model the source island places (props, and objects via
/// [`object_model`]) with its count and median vertical scale, most placed
/// first.
pub fn source_models(src: &IslandEditor) -> Vec<(String, usize, f32)> {
    let mut scales: HashMap<String, Vec<f32>> = HashMap::new();
    for p in src.island.doc.props.iter() {
        if let Some(path) = src.island.doc.meshes.get(p.source.file as usize) {
            scales
                .entry(path.clone())
                .or_default()
                .push(p.source.scale[1]);
        }
    }
    for o in src.island.doc.objects.iter() {
        if let Some(model) = object_model(&src.island, src.module(), src.assets.as_deref(), o) {
            scales.entry(model).or_default().push(o.scale[1]);
        }
    }
    let mut out: Vec<_> = (scales.into_iter())
        .map(|(path, mut v)| {
            let n = v.len();
            let mid = n / 2;
            let median = *v.select_nth_unstable_by(mid, f32::total_cmp).1;
            (path, n, median)
        })
        .collect();
    out.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    out
}

/// Matches every source model to one of this game's props. Below `bring_below`
/// (0..1) a look match brings the source model itself, if it can be ported.
pub fn map_props(
    models: &[(String, usize, f32)],
    catalog: &Catalog,
    (from, to): (&'static dyn Module, &'static dyn Module),
    region: &str,
    bring_below: f32,
    source_look: impl Fn(&str) -> Option<Look>,
    target_look: impl Fn(&str) -> Option<Look>,
) -> Vec<PropRow> {
    let mut by_role: HashMap<Role, Vec<String>> = HashMap::new();
    let mut rank: HashMap<Role, usize> = HashMap::new();
    let rules = to
        .islands()
        .expect("a conversion target has islands")
        .rules();
    // What stands in when nothing matches: the source model ported, or
    // within one game (a region conversion) the model itself.
    let original = |source: &str| {
        if from.game() == to.game() {
            Some(source.to_owned())
        } else {
            super::port::portable(from, to, source).then(|| super::port::target(from, to, source))
        }
    };
    models
        .iter()
        .map(|(source, count, placed)| {
            let role = prop_role(source);
            // Decals and effects. Nothing in the target looks like them, and
            // look matching paired burnt ground with rocks.
            if super::port::always_brought(source)
                && let Some(original) = original(source)
            {
                return PropRow {
                    source: source.clone(),
                    count: *count,
                    placed: *placed,
                    role,
                    choices: vec![original],
                    pick: Some(0),
                    scale: 1.,
                    confidence: None,
                    pieces: Vec::new(),
                    effects: Vec::new(),
                };
            }
            // By look when both games' looks are in; by name until then.
            if let Some(look) = source_look(source) {
                let alikes = look_alikes(
                    catalog,
                    rules,
                    region,
                    (source, &look.scaled(*placed)),
                    &target_look,
                );
                if !alikes.is_empty() {
                    let sure = alikes[0].1;
                    let mut choices: Vec<String> = alikes.into_iter().map(|a| a.0).collect();
                    let mut scale = height_scale(Some(look), target_look(&choices[0]));
                    // Nothing alike enough: the original, ported, stands in.
                    if sure < bring_below
                        && let Some(original) = original(source)
                    {
                        choices.retain(|c| *c != original);
                        choices.insert(0, original);
                        scale = 1.;
                    }
                    return PropRow {
                        source: source.clone(),
                        count: *count,
                        placed: *placed,
                        role,
                        scale,
                        choices,
                        pick: Some(0),
                        confidence: Some(sure),
                        pieces: Vec::new(),
                        effects: Vec::new(),
                    };
                }
            }
            let choices = role.map_or_else(Vec::new, |role| {
                by_role
                    .entry(role)
                    .or_insert_with(|| {
                        let mut r = Some(role);
                        let mut c = Vec::new();
                        while let Some(now) = r
                            && c.is_empty()
                        {
                            c = candidates(catalog, rules, region, now);
                            r = now.fallback().filter(|&f| f != role);
                        }
                        c
                    })
                    .clone()
            });
            let pick = role.filter(|_| !choices.is_empty()).map(|role| {
                let n = rank.entry(role).or_default();
                *n += 1;
                (*n - 1) % choices.len()
            });
            PropRow {
                source: source.clone(),
                count: *count,
                placed: *placed,
                role,
                choices,
                pick,
                scale: 1.,
                confidence: None,
                pieces: Vec::new(),
                effects: Vec::new(),
            }
        })
        .collect()
}

/// What a ground material is, for mapping it to the other game's set.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Ground {
    Underwater,
    Sand,
    /// Stone on the ground, laid or strewn: pavements, tiles, stone floors,
    /// boulder rubble like 117's `rocks_sharp_01`, and 1800's `m_stone_03`.
    /// Never a cliff face. Before this kind existed, the pirate village's
    /// rubble became 1800's cracked rock walls and read as bare rock between
    /// the houses.
    Stony,
    Gravel,
    Rock,
    Snow,
    Dry,
    Dirt,
    Grass,
}

impl Ground {
    pub fn label(self) -> &'static str {
        match self {
            Ground::Underwater => "sea floor",
            Ground::Sand => "sand",
            Ground::Stony => "stony",
            Ground::Gravel => "gravel",
            Ground::Rock => "rock",
            Ground::Snow => "snow",
            Ground::Dry => "dry grass",
            Ground::Dirt => "mud",
            Ground::Grass => "grass",
        }
    }

    fn fallback(self) -> Ground {
        match self {
            Ground::Snow | Ground::Gravel => Ground::Rock,
            Ground::Dry => Ground::Grass,
            Ground::Underwater => Ground::Sand,
            Ground::Dirt => Ground::Grass,
            Ground::Grass => Ground::Dirt,
            Ground::Sand => Ground::Gravel,
            Ground::Stony => Ground::Gravel,
            Ground::Rock => Ground::Gravel,
        }
    }
}

fn stem(file: &str) -> String {
    file.rsplit('/').next().unwrap_or(file).to_ascii_lowercase()
}

/// The kind of a ground material, from its file name. 1800 names read
/// `m_gras_02`, `a_hill_12`, `m_underwater_01`; 117 ones `grass_01_h_dry`,
/// `cliff_limestone_01`, `underwatergrass_01`. The order matters: dry grass
/// is dry before it is grass, and a seagrass is sea floor.
pub fn ground_role(file: &str) -> Ground {
    let name = stem(file);
    let has = |words: &[&str]| words.iter().any(|w| name.contains(w));
    if has(&["underwater", "coral", "seagrass", "seabed"]) {
        Ground::Underwater
    } else if has(&["snow", "ice", "glacier", "frost"]) {
        Ground::Snow
    } else if has(&["beach", "sand", "dune", "shore"]) {
        Ground::Sand
    } else if has(&[
        "pavement",
        "cobble",
        "paving",
        "flagstone",
        "tiles",
        "stone_floor",
        // Boulders strewn on earth, like `rocks_sharp_01`. `rock_limestone_01`
        // is a cliff.
        "rocks_",
    ]) || is_1800_stony(&name)
    {
        Ground::Stony
    } else if has(&["gravel", "gravle", "pebble", "scree"]) {
        Ground::Gravel
    } else if has(&[
        "cliff", "rock", "mountain", "stone", "hill", "lava", "basalt",
    ]) {
        Ground::Rock
    } else if has(&["dry", "savanna", "steppe", "desert"]) {
        Ground::Dry
    } else if has(&[
        "gras", "meadow", "moss", "lawn", "flower", "leaves", "forest",
    ]) {
        Ground::Grass
    } else {
        // mud, dirt, soil, roots, paths
        Ground::Dirt
    }
}

/// 1800 names laid-stone ground with a region letter: `m_stone_03`,
/// `t_stones_02`, `t_path_01`, `a_stone_04`. `m_gras_stone_01` is grass with
/// stones in it, so it doesn't count.
fn is_1800_stony(name: &str) -> bool {
    let mut parts = name.split('_');
    let (Some(region), Some(what)) = (parts.next(), parts.next()) else {
        return false;
    };
    region.len() == 1 && matches!(what, "stone" | "stones" | "path")
}

/// Features painted into a ground texture. 1800's `m_roots_02` is a forest
/// floor full of roots. Such a texture only stands in for a source with the
/// same feature, because matching by color alone turned plain mud paths into
/// roots.
const FEATURES: [&str; 6] = ["root", "leaves", "flower", "moss", "reed", "coral"];

fn features(file: &str) -> u8 {
    let name = stem(file);
    (FEATURES.iter().enumerate())
        .filter(|(_, w)| name.contains(*w))
        .fold(0, |m, (k, _)| m | 1 << k)
}

/// A material as the mapping sees it: id, file, mean texture color.
pub type GroundMaterial = (u8, String, Option<Swatch>);

/// A ground texture as the mapping compares it: mean color and contrast (how
/// much brightness varies, flat sand vs sand strewn with seaweed).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Swatch {
    pub color: [f32; 3],
    pub contrast: f32,
}

impl Swatch {
    #[cfg(test)]
    fn flat(color: [f32; 3]) -> Swatch {
        Swatch {
            color,
            contrast: 0.,
        }
    }
}

/// Source material id → target material id, for every id the source uses.
/// `used` holds each source material with the ground it shows; `target`
/// the target set. Each source material takes the target material of its
/// kind (or the kind's fallback) closest in mean color, so dark tropical
/// rock does not turn into pale limestone when a darker rock exists.
/// Without a color the target game's preferred material of the kind wins
/// (`names`).
pub fn map_materials(
    used: &[(GroundMaterial, f64)],
    target: &[GroundMaterial],
    names: &GroundNames,
) -> BTreeMap<u8, u8> {
    // Per kind: (preferred rank, id, swatch, features), one entry per file.
    type Candidate = (usize, u8, Option<Swatch>, u8);
    let mut kinds: BTreeMap<Ground, Vec<Candidate>> = BTreeMap::new();
    let mut files = std::collections::HashSet::new();
    for (id, file, color) in target {
        if !files.insert(file.as_str()) {
            continue;
        }
        let name = stem(file);
        let g = ground_role(file);
        let rank = (names.preferred(g.label()).iter())
            .position(|p| name.starts_with(p))
            .unwrap_or(usize::MAX);
        kinds
            .entry(g)
            .or_default()
            .push((rank, *id, *color, features(file)));
    }
    for list in kinds.values_mut() {
        list.sort_by_key(|e| (e.0, e.1));
    }
    let candidates = |g: Ground| {
        let mut g = g;
        for _ in 0..4 {
            if let Some(list) = kinds.get(&g) {
                return list.as_slice();
            }
            g = g.fallback();
        }
        &[]
    };
    let distance = |a: Option<Swatch>, b: Option<Swatch>| match (a, b) {
        (Some(a), Some(b)) => swatch_distance(a, b),
        _ => f32::MAX,
    };
    let mut out = BTreeMap::new();
    for ((id, file, color), _) in used {
        let list = candidates(ground_role(file));
        // Skip textures with a feature the source lacks, unless nothing plainer
        // of the kind exists.
        let own = features(file);
        let plain: Vec<&Candidate> = list.iter().filter(|e| e.3 & !own == 0).collect();
        let pool: Vec<&Candidate> = match plain.is_empty() {
            true => list.iter().collect(),
            false => plain,
        };
        let best = pool
            .into_iter()
            .min_by(|a, b| {
                distance(*color, a.2)
                    .total_cmp(&distance(*color, b.2))
                    .then(a.0.cmp(&b.0))
            })
            .map(|e| e.1)
            .or_else(|| target.first().map(|t| t.0));
        if let Some(best) = best {
            out.insert(*id, best);
        }
    }
    out
}

/// How different two ground textures look. Hue and contrast count most and
/// brightness least, because the tint map carries over and fixes brightness
/// anyway. That way 117's plain warm `beach_01` gets 1800's plain warm
/// `m_beach_01`, not the green-speckled `s_beach_02` or the seaweed-strewn
/// `s_beach_11`. Hue is the color divided by its mean; brightness and contrast
/// compare as log ratios.
fn swatch_distance(a: Swatch, b: Swatch) -> f32 {
    let mean = |c: [f32; 3]| ((c[0] + c[1] + c[2]) / 3.).max(1.);
    let (ma, mb) = (mean(a.color), mean(b.color));
    let hue: f32 = (0..3)
        .map(|c| (a.color[c] / ma - b.color[c] / mb).powi(2))
        .sum();
    // +2: below that, contrast is preview noise.
    let contrast = ((a.contrast + 2.) / (b.contrast + 2.)).ln();
    50. * hue + 0.5 * contrast.powi(2) + (ma / mb).ln().powi(2)
}

/// What a source ground material becomes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GroundPick {
    /// One of the target set's materials, by id.
    Material(u8),
    /// The source's own material, ported into the mod (`ground_set`).
    Bring,
}

/// One source ground material (all ids with the same file) and what it becomes.
/// One row of the wizard's Ground table.
#[derive(Clone, Debug, PartialEq)]
pub struct GroundRow {
    pub file: String,
    pub ids: Vec<u8>,
    /// Share of the source's ground it shows, 0..1.
    pub share: f64,
    pub kind: Ground,
    pub pick: GroundPick,
    /// The most alike target material. It's the first pick, and the stand-in if
    /// a brought material fails to port.
    pub auto: u8,
}

/// The source's ground materials, most shown first, each with the target
/// material [`map_materials`] picks for it, by the target game's `names`.
pub fn ground_rows(src: &IslandEditor, target: &Palette, names: &GroundNames) -> Vec<GroundRow> {
    let (Some(sp), Some(sm)) = (&src.palette, &src.island.doc.materials) else {
        return Vec::new();
    };
    let weights = material_weights(sm, &sp.stack);
    let total: f64 = weights.values().sum::<f64>().max(1e-9);
    let used: Vec<(GroundMaterial, f64)> = (weights.iter())
        .filter_map(|(&id, &w)| {
            let m = sp.materials.get(&id)?;
            Some(((id, m.file.clone(), Some(swatch(m))), w))
        })
        .collect();
    let swatches: Vec<GroundMaterial> = (target.materials.iter())
        .map(|(&id, m)| (id, m.file.clone(), Some(swatch(m))))
        .collect();
    let map = map_materials(&used, &swatches, names);
    let mut rows: Vec<GroundRow> = Vec::new();
    for ((id, file, _), w) in &used {
        let Some(&to) = map.get(id) else { continue };
        match rows.iter_mut().find(|r| &r.file == file) {
            Some(r) => {
                r.ids.push(*id);
                r.share += w / total;
            }
            None => rows.push(GroundRow {
                file: file.clone(),
                ids: vec![*id],
                share: w / total,
                kind: ground_role(file),
                pick: GroundPick::Material(to),
                auto: to,
            }),
        }
    }
    rows.sort_by(|a, b| b.share.total_cmp(&a.share).then(a.file.cmp(&b.file)));
    rows
}

/// Maps source material ids to ids in the island's set, from the wizard's rows
/// and the set `brought` describes. A material that failed to port gets its
/// `auto` stand-in.
pub fn ground_map(rows: &[GroundRow], brought: &Brought) -> BTreeMap<u8, u8> {
    let target = |t: u8| brought.target_ids.get(&t).copied().unwrap_or(t);
    let mut out = BTreeMap::new();
    for r in rows {
        let to = match r.pick {
            GroundPick::Material(id) => Some(target(id)),
            GroundPick::Bring => brought.ids.get(&r.file).copied().or(Some(target(r.auto))),
        };
        if let Some(to) = to {
            out.extend(r.ids.iter().map(|&id| (id, to)));
        }
    }
    out
}

/// The ground set a converted island gets (`ground_set`).
#[derive(Default)]
pub struct Brought {
    /// Files to stage: each ported material's xml and textures, then the
    /// island's own material set.
    pub files: Vec<(String, Vec<u8>)>,
    /// The id each brought source material (by file) got in that set.
    pub ids: HashMap<String, u8>,
    /// Target ids that moved in the island's own set. Hand-picked target
    /// materials keep their texture, not their id.
    pub target_ids: HashMap<u8, u8>,
    /// Materials that did not port; their rows take their stand-in.
    pub failed: Vec<String>,
}

/// Gives the converted island its own material set in the mod, and builds its
/// palette right away. Brought textures are read from `from` since the mod
/// isn't mounted yet. With `own` the set holds the source's materials at their
/// ids and in their order, so the ground blends like it did, under the target
/// set's header (the target's terrain shader expects that header). Without
/// `own` it's the target set with the brought materials added on top.
pub fn ground_set(
    from_game: &dyn Module,
    from: &Install,
    install: &Install,
    dst: &mut IslandEditor,
    (target, source): (&str, &str),
    rows: &[GroundRow],
    own: bool,
) -> anyhow::Result<Brought> {
    use super::materials::{SetEntry, set_entries};
    let mut out = Brought::default();
    let target_xml = String::from_utf8_lossy(&install.read(target)?).into_owned();
    let target_entries = set_entries(&target_xml);
    let source_entries = set_entries(&String::from_utf8_lossy(&from.read(source)?));
    // Port each brought material once, by file.
    let mut ported: HashMap<String, String> = HashMap::new();
    for r in rows.iter().filter(|r| r.pick == GroundPick::Bring) {
        match super::port::port_material(from_game, dst.module(), from, &r.file) {
            Ok((path, files)) => {
                out.files.extend(files);
                ported.insert(r.file.clone(), path);
            }
            Err(e) => {
                eprintln!("import: material {}: {e:#}", r.file);
                out.failed.push(r.file.clone());
            }
        }
    }
    // Brought materials in the source's order, each file once, as (entry to
    // write, source file).
    let to = dst.module();
    let mut brought: Vec<(SetEntry, String)> = Vec::new();
    for e in &source_entries {
        if let Some(path) = ported.get(&e.file)
            && !brought.iter().any(|b| b.1 == e.file)
        {
            let ground_type = (to.islands())
                .expect("island workspace")
                .ground_type(&e.ground_type)
                .to_owned();
            let entry = SetEntry {
                file: path.clone(),
                ground_type,
                ..e.clone()
            };
            brought.push((entry, e.file.clone()));
        }
    }
    // (entry, file its textures load from, whether that file is in `from`)
    let mut layers: Vec<(SetEntry, String, bool)> = Vec::new();
    let xml = if own {
        // Brought materials keep their source ids. Target materials, picked by
        // hand or standing in for a failed port, take free ids on top.
        for (e, file) in brought {
            out.ids.insert(file.clone(), e.id);
            layers.push((e, file, true));
        }
        let mut wanted: Vec<u8> = Vec::new();
        for r in rows {
            let t = match r.pick {
                GroundPick::Material(t) => t,
                GroundPick::Bring if !ported.contains_key(&r.file) => r.auto,
                GroundPick::Bring => continue,
            };
            if !wanted.contains(&t) {
                wanted.push(t);
            }
        }
        let mut free = (0u8..32).filter(|id| !out.ids.values().any(|v| v == id));
        for t in wanted {
            let Some(e) = target_entries.iter().find(|e| e.id == t) else {
                continue;
            };
            let id = free
                .next()
                .context("the island's material set is full (32 materials)")?;
            out.target_ids.insert(t, id);
            layers.push((SetEntry { id, ..e.clone() }, e.file.clone(), false));
        }
        let entries: Vec<SetEntry> = layers.iter().map(|l| l.0.clone()).collect();
        super::materials::write_material_set(&target_xml, &entries)?
    } else {
        if brought.is_empty() {
            return Ok(out);
        }
        let entries: Vec<SetEntry> = brought.iter().map(|b| b.0.clone()).collect();
        let (xml, ids) = super::materials::extend_material_set(&target_xml, &entries)?;
        for ((e, file), id) in brought.into_iter().zip(ids) {
            out.ids.insert(file.clone(), id);
            layers.push((SetEntry { id, ..e }, file, true));
        }
        xml
    };
    let name = crate::project::island_name(&dst.path);
    let path = super::port::material_set_path(from_game, dst.module(), name)
        .context("the target's projects have no folder for material sets")?;
    out.files.push((path.clone(), xml.into_bytes()));
    dst.island.material_set = path;
    // The palette starts from the target set when extending, then adds each new
    // layer from wherever its textures are right now.
    let mut palette = match own {
        true => Palette::empty(),
        false => Palette::load(install, None, target)?,
    };
    for (e, file, brought) in &layers {
        let at = if *brought { from } else { install };
        palette.add_material(at, e.id, file, (e.repetition, e.sharpness), &e.ground_type)?;
    }
    dst.thumbs = (palette.materials.iter())
        .map(|(&id, m)| (id, m.preview.render_image()))
        .collect();
    dst.palette = Some(Arc::new(palette));
    Ok(out)
}

/// Mean color and brightness spread of a material's preview.
fn swatch(m: &crate::terrain::Material) -> Swatch {
    Swatch::of(&m.preview.rgba)
}

impl Swatch {
    /// Mean color and brightness spread of RGBA texels.
    pub fn of(rgba: &[u8]) -> Swatch {
        let px = rgba.as_chunks::<4>().0;
        let n = px.len().max(1) as f64;
        let mut sum = [0f64; 3];
        let (mut l1, mut l2) = (0f64, 0f64);
        for p in px {
            for c in 0..3 {
                sum[c] += f64::from(p[c]);
            }
            let l = (f64::from(p[0]) + f64::from(p[1]) + f64::from(p[2])) / 3.;
            l1 += l;
            l2 += l * l;
        }
        let var = (l2 / n - (l1 / n).powi(2)).max(0.);
        Swatch {
            color: sum.map(|v| (v / n) as f32),
            contrast: var.sqrt() as f32,
        }
    }
}

/// How much ground each material id shows over a whole material layer.
/// Paint other than a7m stacks as its own ground does, not by `stack`.
pub(super) fn material_weights(m: &Materials, stack: &Stack) -> BTreeMap<u8, f64> {
    let [w, h] = m.paint.size();
    let mut out = BTreeMap::new();
    let Some(paint) = m.paint.a7m() else {
        for y in 0..h {
            for x in 0..w {
                for &(id, shown) in m.paint.shown(x, y, stack).iter() {
                    *out.entry(id).or_insert(0.) += f64::from(shown);
                }
            }
        }
        return out;
    };
    for y in 0..h {
        for x in 0..w {
            let (ids, weights) = paint.slots(x, y);
            let shown = stack.visible(ids, weights);
            for k in (0..6).filter(|&k| shown[k] > 0.) {
                *out.entry(ids[k]).or_insert(0.) += f64::from(shown[k]);
            }
        }
    }
    out
}

/// A texel's ground as (target id, share shown), shares summing to 1.
type Shares = Vec<(u8, f32)>;

/// The source's materials, plus the mean color (linear light) of every material
/// in the source and target sets.
type Recolor<'a> = (
    &'a Materials,
    &'a Stack,
    HashMap<u8, [f32; 3]>,
    HashMap<u8, [f32; 3]>,
);

/// What conversion reads of a material set: the order and sharpness it
/// stacks paint in, and each material's file and look, so it needs no
/// textures.
pub struct Set {
    pub stack: Stack,
    /// By id, ascending.
    pub materials: Vec<GroundMaterial>,
}

impl Set {
    pub fn of(palette: &Palette) -> Set {
        Set {
            stack: palette.stack.clone(),
            materials: (palette.materials.iter())
                .map(|(&id, m)| (id, m.file.clone(), Some(swatch(m))))
                .collect(),
        }
    }

    fn get(&self, id: u8) -> Option<&GroundMaterial> {
        self.materials.iter().find(|m| m.0 == id)
    }
}

/// Mean colors of a set's materials in linear light, since the terrain shader
/// multiplies by the tint after sRGB decode.
fn linear_colors(set: &Set) -> HashMap<u8, [f32; 3]> {
    (set.materials.iter())
        .filter_map(|(id, _, s)| Some((*id, s.as_ref()?.color.map(|c| (c / 255.).powf(2.2)))))
        .collect()
}

/// A texel's color, its materials' colors weighted by share. Whatever no
/// material covers counts as `rest`.
fn blend(cover: &Shares, colors: &HashMap<u8, [f32; 3]>, rest: [f32; 3]) -> [f32; 3] {
    let mut out = [0f32; 3];
    let mut covered = 0.;
    for &(id, share) in cover {
        let c = colors.get(&id).copied().unwrap_or(rest);
        for k in 0..3 {
            out[k] += share * c[k];
        }
        covered += share;
    }
    let left = (1. - covered).max(0.);
    std::array::from_fn(|k| out[k] + left * rest[k])
}

/// The source materials shown at `q` (metres on the source) with their shares.
/// Coverage is interpolated over the four nearest texels, then stacked the way
/// the game stacks it (`Stack::shares`).
fn source_cover(src: &Materials, from: &Stack, q: [f32; 2]) -> Shares {
    let [sw, sh] = src.paint.size();
    let Some(paint) = src.paint.a7m() else {
        // Other forms' coverage, on corners (2070's metre vertices) or
        // cells, stacked as their own ground stacks it.
        {
            let half = if src.paint.corners() { 0. } else { 0.5 };
            let (u, v) = (q[0] * src.per_metre - half, q[1] * src.per_metre - half);
            let (fx, fy) = (u - u.floor(), v - v.floor());
            let mut cover: Shares = Vec::with_capacity(4 * 24);
            for t in 0..4 {
                let (dx, dy) = (t & 1, t >> 1);
                let tx = (u.floor() as i64 + dx).clamp(0, sw as i64 - 1) as usize;
                let ty = (v.floor() as i64 + dy).clamp(0, sh as i64 - 1) as usize;
                let share = if dx == 1 { fx } else { 1. - fx } * if dy == 1 { fy } else { 1. - fy };
                cover.extend(
                    src.paint
                        .cover(tx, ty)
                        .iter()
                        .map(|&(id, c)| (id, c * share)),
                );
            }
            let n = src.paint.shares(&mut cover).unwrap_or(cover.len());
            cover.truncate(n);
            return cover;
        }
    };
    let (u, v) = (q[0] * src.per_metre - 0.5, q[1] * src.per_metre - 0.5);
    let (fx, fy) = (u - u.floor(), v - v.floor());
    let mut cover: Shares = Vec::with_capacity(24);
    for t in 0..4 {
        let (dx, dy) = (t & 1, t >> 1);
        let tx = (u.floor() as i64 + dx).clamp(0, sw as i64 - 1) as usize;
        let ty = (v.floor() as i64 + dy).clamp(0, sh as i64 - 1) as usize;
        let share = if dx == 1 { fx } else { 1. - fx } * if dy == 1 { fy } else { 1. - fy };
        let (ids, weights) = paint.slots(tx, ty);
        for k in (0..6).filter(|&k| weights[k] > 0) {
            cover.push((ids[k] & 31, f32::from(weights[k]) / 31. * share));
        }
    }
    let n = from.shares(&mut cover);
    cover.truncate(n);
    cover
}

/// Rewrites `dst` from `src` through `map` so the target shows what the
/// source showed. Weights are coverages the game lays over each other in
/// set order (`Stack`), so the source is sampled the way the game
/// samples it (each material's coverage interpolated over the four nearest
/// texels, or 2070's metre vertices, then stacked), mapped, merged where
/// two source materials map to one target, and re-encoded for the target's
/// form and order (`encode_a7m`, `encode_layers`). Nearest sampling made
/// every source texel a block of target texels with stair-step edges.
/// Ground off the source (a larger target island) becomes `sea`.
///
/// Every texel keeps one base material at full weight under the rest, as
/// shipped islands keep a beach: where neighbouring texels' layers only
/// partly cover, the game's bake shows its white clear color unless a base
/// lies under everything.
///
/// Gives how many target texels lost ground a 2070 chunk had no room
/// for, a level-1 loss.
pub(super) fn transfer_materials(
    (src, from): (&Materials, &Stack),
    (dst, to): (&mut Materials, &Stack),
    map: &BTreeMap<u8, u8>,
    offset: f32,
    src_size: f32,
    sea: u8,
) -> usize {
    let per = dst.per_metre;
    let base = (map.values().chain([&sea]).copied())
        .min_by_key(|&id| to.rank(id).unwrap_or(u8::MAX))
        .unwrap_or(sea);
    // The target's shares at island metres `q` on the target. The source's
    // far edge is on it: 2070's last vertices sit exactly there, and a7m
    // texel centres never do.
    let target = |q: [f32; 2]| -> Shares {
        let q = [q[0] - offset, q[1] - offset];
        if !(0.0..=src_size).contains(&q[0]) || !(0.0..=src_size).contains(&q[1]) {
            return vec![(sea, 1.)];
        }
        let cover = source_cover(src, from, q);
        let mut out: Shares = Vec::new();
        for &(id, shown) in &cover {
            let id = map.get(&id).copied().unwrap_or(sea);
            match out.iter_mut().find(|e| e.0 == id) {
                Some(e) => e.1 += shown,
                None => out.push((id, shown)),
            }
        }
        if out.is_empty() {
            out.push((sea, 1.));
        }
        out
    };
    match &mut dst.paint {
        Paint::A7m(paint) => {
            // Texel centres.
            let texel =
                |x: usize, y: usize| target([(x as f32 + 0.5) / per, (y as f32 + 0.5) / per]);
            encode_a7m(paint, to, base, texel);
            0
        }
        // Metre vertices.
        Paint::Layers(layers) => encode_layers(layers, base, |x, z| {
            target([x as f32 / per, z as f32 / per])
        }),
        Paint::Own(own) => own.form_mut().fill(per, base, &target),
    }
}

/// Writes `texel`'s shares into a7m paint: per 2x2 block the base and the
/// five ids showing most across it, then each texel's weights inverting
/// `to`'s ranks and sharpness, bottom to top.
fn encode_a7m(
    paint: &mut anno_core::island::A7m,
    to: &Stack,
    base: u8,
    texel: impl Fn(usize, usize) -> Shares,
) {
    let [w, h] = paint.alphas.size();
    for by in 0..h.div_ceil(2) {
        for bx in 0..w.div_ceil(2) {
            let cells: Vec<((usize, usize), Shares)> = [(0, 0), (1, 0), (0, 1), (1, 1)]
                .iter()
                .map(|(dx, dy)| (bx * 2 + dx, by * 2 + dy))
                .filter(|&(x, y)| x < w && y < h)
                .map(|(x, y)| ((x, y), texel(x, y)))
                .collect();
            let mut totals: Shares = Vec::new();
            for (_, t) in &cells {
                for &(id, v) in t.iter().filter(|e| e.0 != base) {
                    match totals.iter_mut().find(|e| e.0 == id) {
                        Some(e) => e.1 += v,
                        None => totals.push((id, v)),
                    }
                }
            }
            totals.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
            totals.truncate(5);
            let used: Vec<u8> = [base]
                .into_iter()
                .chain(totals.iter().map(|t| t.0))
                .collect();
            let mut block = [0u8; 6];
            let mut spare = (0u8..32).filter(|id| !used.contains(id));
            for (k, slot) in block.iter_mut().enumerate() {
                *slot = match used.get(k) {
                    Some(&id) => id,
                    None => spare.next().expect("32 ids, at most 6 used"),
                };
            }
            paint.ids.set(bx, by, island::pack_slots(block));
            // Slots bottom to top in the target's stack; the base is lowest.
            let mut order: Vec<usize> = (0..used.len()).collect();
            order.sort_by_key(|&k| to.rank(block[k]).unwrap_or(u8::MAX));
            for ((x, y), t) in cells {
                let share =
                    |k: usize| -> f32 { t.iter().filter(|e| e.0 == block[k]).map(|e| e.1).sum() };
                // A layer showing v of the texel over c below it, where c
                // already includes v, is laid over at alpha v / c.
                let mut packed = [0u8; 6];
                let mut below = 0.;
                for &k in &order {
                    let v = share(k);
                    if block[k] == base {
                        packed[k] = 31;
                        below += v;
                        continue;
                    }
                    if v <= 0. {
                        continue;
                    }
                    below += v;
                    packed[k] = to.coverage_for(block[k], v / below);
                }
                paint.alphas.set(x, y, island::pack_slots(packed));
            }
        }
    }
}

/// Writes `vertex`'s shares into 2070 paint, chunk by chunk. `base` lies
/// opaque under every chunk, so neighbours agree and the shader's opaque
/// first layer never shows through. Above it each vertex's ids take the
/// coverage that lays them over what is below at the alpha their share
/// needs, inverting the registry's ranks and 2070's sharpness
/// ([`Stack::coverage`]).
///
/// A chunk holds layer 0, which the format puts first, the base, and the
/// ids showing most over it, up to the 24 the game draws. A vertex keeps
/// only ids every chunk holding it keeps, so shared edges agree; the rest
/// of its ground falls to the base. Gives how many vertices lost ground so.
fn encode_layers(
    layers: &mut Layers,
    base: u8,
    vertex: impl Fn(usize, usize) -> Shares + Sync,
) -> usize {
    use anno_formats::isd::terrain::{Terrain, chunks_at};
    let registry = layers.registry().clone();
    let stack = registry.stack();
    let [cw, ch] = layers.chunks();
    let (w, h) = (cw * Terrain::CHUNK + 1, ch * Terrain::CHUNK + 1);
    // Ids the stack can't rank (scatter, unregistered) draw no ground.
    let ranked = |id: u8| stack.rank(id).is_some();
    let base = if ranked(base) {
        base
    } else {
        stack.first().copied().unwrap_or(0)
    };
    let threads = std::thread::available_parallelism().map_or(4, |n| n.get());
    let rows = h.div_ceil(threads).max(1);
    let shares: Vec<Shares> = std::thread::scope(|scope| {
        let parts: Vec<_> = (0..h)
            .step_by(rows)
            .map(|z0| {
                let vertex = &vertex;
                scope.spawn(move || {
                    let mut out = Vec::with_capacity(rows * w);
                    for z in z0..(z0 + rows).min(h) {
                        for x in 0..w {
                            let mut v = vertex(x, z);
                            v.retain(|e| e.1 > 0. && ranked(e.0));
                            out.push(v);
                        }
                    }
                    out
                })
            })
            .collect();
        (parts.into_iter())
            .flat_map(|p| p.join().expect("vertex shares"))
            .collect()
    });

    let mut kept: Vec<Vec<u8>> = Vec::with_capacity(cw * ch);
    for k in 0..cw * ch {
        let (x0, z0) = (k % cw * Terrain::CHUNK, k / cw * Terrain::CHUNK);
        let mut shown: Vec<(u8, f32)> = Vec::new();
        for z in z0..=z0 + Terrain::CHUNK {
            for &(id, v) in shares[z * w + x0..=z * w + x0 + Terrain::CHUNK]
                .iter()
                .flatten()
            {
                match shown.iter_mut().find(|e| e.0 == id) {
                    Some(e) => e.1 += v,
                    None => shown.push((id, v)),
                }
            }
        }
        shown.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
        let mut ids = vec![0, base];
        for (id, _) in shown {
            if ids.len() == Terrain::MAX_LAYERS {
                break;
            }
            ids.push(id);
        }
        ids.sort_unstable();
        ids.dedup();
        kept.push(ids);
    }

    let mut lost = 0;
    let mut coverage: Vec<Vec<(u8, u8)>> = Vec::with_capacity(w * h);
    for z in 0..h {
        for x in 0..w {
            let held =
                |id: &u8| chunks_at((cw, ch), x, z).all(|k| kept[k].binary_search(id).is_ok());
            let all = &shares[z * w + x];
            let mut v: Shares = all.iter().filter(|e| held(&e.0)).copied().collect();
            if all.len() != v.len() {
                lost += 1;
            }
            // Top down, each id lays over what's left below it at the
            // alpha that shows its share; the base takes the rest.
            v.retain(|e| e.0 != base);
            v.sort_by_key(|e| std::cmp::Reverse(stack.rank(e.0)));
            let mut out = vec![(base, 255)];
            let mut below = 1f32;
            for &(id, share) in &v {
                let alpha = if below > 0. {
                    (share / below).min(1.)
                } else {
                    0.
                };
                below = (below - share).max(0.);
                let c = (stack.coverage(id, alpha) * 255.).round() as u8;
                if c > 0 {
                    out.push((id, c));
                }
            }
            coverage.push(out);
        }
    }

    for (k, ids) in kept.iter().enumerate() {
        let (x0, z0) = (k % cw * Terrain::CHUNK, k / cw * Terrain::CHUNK);
        let mut planes = vec![0u8; ids.len() * Terrain::PLANE];
        for n in 0..Terrain::PLANE {
            let (x, z) = (x0 + n % Terrain::PLANE_SIDE, z0 + n / Terrain::PLANE_SIDE);
            for &(id, c) in &coverage[z * w + x] {
                if let Ok(l) = ids.binary_search(&id) {
                    planes[l * Terrain::PLANE + n] = c;
                }
            }
        }
        // A layer nothing in the chunk shows isn't worth a place; a
        // neighbour reads its missing plane as 0. Layer 0 stays first.
        let (mut held, mut kept_planes) = (Vec::with_capacity(ids.len()), Vec::new());
        for (l, &id) in ids.iter().enumerate() {
            let plane = &planes[l * Terrain::PLANE..(l + 1) * Terrain::PLANE];
            if id == 0 || plane.iter().any(|&c| c > 0) {
                held.push(id);
                kept_planes.extend_from_slice(plane);
            }
        }
        layers
            .set(k, &held, &kept_planes)
            .expect("ascending ids from 0, at most 24, a plane each");
    }
    lost
}

/// What to carry over.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Options {
    pub materials: bool,
    pub tint: bool,
    pub props: bool,
    pub water: bool,
    pub harbours: bool,
    pub slots: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            materials: true,
            tint: true,
            props: true,
            water: true,
            harbours: true,
            slots: true,
        }
    }
}

/// What a conversion did, for the status line and the wizard.
#[derive(Clone, Debug, Default)]
pub struct Report {
    pub props: usize,
    /// Particle effects placed as effect objects.
    pub effects: usize,
    /// Source material file → target material file.
    pub materials: Vec<(String, String)>,
    pub rivers: usize,
    pub lakes: usize,
    pub harbours: usize,
    pub slots: usize,
    /// What conversion left out or cropped, with counts.
    pub losses: BTreeMap<Lost, usize>,
    /// The source objects (or props) each loss took, by id, where it took
    /// any: what an acceptance lists, so converting the same island again
    /// asks nothing new.
    pub items: BTreeMap<Lost, Vec<u64>>,
}

/// Something conversion leaves out, by kind; its loss-table level is
/// [`Lost::level`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Lost {
    /// A prop or decoration object with no stand-in in the target and no
    /// port of its own.
    PropDropped,
    /// A slot of a kind (`SlotKind`) the target province places none of.
    SlotDropped(Resource, bool),
    /// A river path past the target's edge.
    RiverCropped,
    /// A lake path past the target's edge.
    LakeCropped,
    /// A slot past the target's edge.
    SlotCropped,
    /// A harbour or start coast past the target's edge.
    HarbourCropped,
    /// The ground paint: one side has no material set to map it through.
    Paint,
    /// A vertex's ground past the 24 layers its 2070 chunk draws, given to
    /// the chunk's base instead; counted per vertex.
    ChunkFull,
    /// A harbour or start coast the target places no records for (2070,
    /// whose harbours are walls of `harboursystem` handles).
    HarbourDropped,
    /// A river or lake the target places no records for (2070, which
    /// carries its own water as loaded).
    WaterDropped,
}

impl Lost {
    pub fn level(self) -> Level {
        match self {
            Lost::PropDropped => Level::Decoration,
            Lost::RiverCropped | Lost::LakeCropped | Lost::WaterDropped => Level::Shape,
            Lost::SlotDropped(..)
            | Lost::SlotCropped
            | Lost::HarbourCropped
            | Lost::HarbourDropped => Level::Gameplay,
            Lost::Paint | Lost::ChunkFull => Level::Look,
        }
    }

    /// `n` of it in words, for the status line: "2 Oil slots left out".
    fn said(self, n: usize) -> String {
        let (one, many) = self.words();
        crate::project::plural(n, &one, &many)
    }

    /// Singular and plural, for the status line and acceptances.
    fn words(self) -> (String, String) {
        let (one, many) = match self {
            Lost::SlotDropped(resource, fixed) => {
                let kind = slot_label((resource, fixed));
                return (
                    format!("{kind} slot left out"),
                    format!("{kind} slots left out"),
                );
            }
            Lost::PropDropped => ("prop left out", "props left out"),
            Lost::RiverCropped => ("river cropped", "rivers cropped"),
            Lost::LakeCropped => ("lake cropped", "lakes cropped"),
            Lost::SlotCropped => ("slot cropped", "slots cropped"),
            Lost::HarbourCropped => ("harbour cropped", "harbours cropped"),
            Lost::Paint => ("ground paint left out", "ground paint left out"),
            Lost::ChunkFull => (
                "vertex past a chunk's 24 ground layers",
                "vertices past a chunk's 24 ground layers",
            ),
            Lost::HarbourDropped => ("harbour left out", "harbours left out"),
            Lost::WaterDropped => ("river or lake left out", "rivers and lakes left out"),
        };
        (one.to_owned(), many.to_owned())
    }
}

impl Report {
    pub fn summary(&self) -> String {
        let mut s = format!(
            "{} props ({} dropped), {} materials mapped, {} rivers, {} lakes, {} harbours, {} slots",
            self.props,
            self.lost(Lost::PropDropped),
            self.materials.len(),
            self.rivers,
            self.lakes,
            self.harbours,
            self.slots
        );
        if self.effects > 0 {
            s += &format!(
                ", {}",
                crate::project::plural(self.effects, "effect", "effects")
            );
        }
        // Props dropped are said with the props above.
        for (&lost, &n) in self.losses.iter().filter(|(l, _)| **l != Lost::PropDropped) {
            s += &format!("; {}", lost.said(n));
        }
        s
    }

    /// How many of `what` the conversion lost.
    pub fn lost(&self, what: Lost) -> usize {
        self.losses.get(&what).copied().unwrap_or(0)
    }

    /// Each kind of loss with its level and count, which decide what the
    /// user is asked (`Level::policy`).
    pub fn problems(&self) -> Vec<Problem> {
        (self.losses.iter())
            .map(|(&lost, &count)| Problem {
                level: lost.level(),
                what: lost.words().1,
                count,
                items: {
                    let mut items = self.items.get(&lost).cloned().unwrap_or_default();
                    items.sort_unstable();
                    items.dedup();
                    items
                },
                names: Default::default(),
            })
            .collect()
    }

    fn lose(&mut self, what: Lost, item: Option<u64>) {
        *self.losses.entry(what).or_default() += 1;
        if let Some(item) = item {
            self.items.entry(what).or_default().push(item);
        }
    }
}

/// The target size for an island `size` metres across: the smallest of the
/// region's sizes that holds it, else the largest (which crops the rim).
pub fn pick_size<'a>(
    skeletons: &'a [super::Skeleton],
    region: &str,
    size: u32,
) -> Option<&'a super::Skeleton> {
    let of_region = || skeletons.iter().filter(|s| s.region == region);
    of_region()
        .filter(|s| s.world[0] >= size)
        .min_by_key(|s| s.world[0])
        .or_else(|| of_region().max_by_key(|s| s.world[0]))
}

/// A kind of slot: what it yields, and whether it is fixed (always active)
/// rather than one the map generator may pick.
pub type SlotKind = (Resource, bool);

pub fn slot_label((resource, fixed): SlotKind) -> String {
    let name = Resource::ALL
        .iter()
        .find(|(r, _)| *r == resource)
        .map_or("?", |(_, n)| n);
    if fixed {
        format!("{name} (fixed)")
    } else {
        name.to_owned()
    }
}

/// One kind of slot on the source island and what it becomes; `None`
/// leaves its slots out.
#[derive(Clone, Debug, PartialEq)]
pub struct SlotRow {
    pub kind: SlotKind,
    pub count: usize,
    pub target: Option<SlotKind>,
}

/// The slot kinds `lib`'s province can place.
pub fn slot_kinds(lib: &harbour::Library) -> Vec<SlotKind> {
    Resource::ALL
        .iter()
        .flat_map(|&(r, _)| [(r, false), (r, true)])
        .filter(|&(r, fixed)| (!fixed || r.has_fixed()) && lib.guid(r.pieces(fixed).0).is_some())
        .collect()
}

/// The source island's slot kinds with first targets: the same kind where
/// the province has it, a random slot for a fixed one it lacks, else left
/// out (Albion has no clay or oil, the Old World no river slots).
pub fn map_slots(src: &IslandEditor, lib: &harbour::Library) -> Vec<SlotRow> {
    let available = slot_kinds(lib);
    let mut rows: Vec<SlotRow> = Vec::new();
    let assets = src.assets.as_deref();
    for o in src.island.doc.objects.iter() {
        let Some(kind) = assets
            .and_then(|a| harbour::piece_of(src.module(), a, o.guid))
            .and_then(slot_of)
        else {
            continue;
        };
        match rows.iter_mut().find(|r| r.kind == kind) {
            Some(r) => r.count += 1,
            None => rows.push(SlotRow {
                kind,
                count: 1,
                target: [kind, (kind.0, false)]
                    .into_iter()
                    .find(|k| available.contains(k)),
            }),
        }
    }
    rows.sort_by(|a, b| {
        b.count
            .cmp(&a.count)
            .then(slot_label(a.kind).cmp(&slot_label(b.kind)))
    });
    rows
}

/// The slot an object of `piece` stands for.
fn slot_of(piece: Piece) -> Option<SlotKind> {
    Some(match piece {
        Piece::MineSlot => (Resource::Mine, false),
        Piece::FixedMineSlot => (Resource::Mine, true),
        Piece::ClaySlot => (Resource::Clay, false),
        Piece::OilSlot => (Resource::Oil, false),
        Piece::GasSlot => (Resource::Gas, false),
        Piece::NewWorldMineSlot => (Resource::NewWorldMine, false),
        Piece::RiverSlot => (Resource::River, false),
        Piece::FixedRiverSlot => (Resource::River, true),
        Piece::MarshSlot => (Resource::Marsh, false),
        _ => return None,
    })
}

/// The object a placed effect copies. 1800 stands a bare cfg up with the
/// generic `TestData_Simple` asset, which two shipped islands place; we read it
/// from the first island the catalog found it on. `None` if the target has
/// none, and then effects are left out.
pub fn effect_template(install: &Install, catalog: &Catalog) -> Option<Template> {
    let entry = catalog.decorations.first()?;
    let read = || -> anyhow::Result<Option<Template>> {
        let archive = Archive::parse(install.read(&entry.island)?, &entry.island)?;
        let store = Objects::read(&Adf::parse(&archive.read_path("gamedata.data")?)?)?;
        Ok((store.objects.iter())
            .position(|o| o.guid == entry.guid && o.config.is_some())
            .map(|i| store.template(i)))
    };
    read()
        .map_err(|e| eprintln!("import: effect object from {}: {e:#}", entry.island))
        .ok()
        .flatten()
}

/// What conversion reads besides the two islands and the wizard's choices.
pub struct Context<'a> {
    /// Source, target.
    pub games: [&'static dyn Module; 2],
    /// Each side's material set, source and target, when it loaded.
    pub sets: [Option<Set>; 2],
    /// The source game's assets, for telling its slots, coasts and water
    /// apart.
    pub assets: Option<&'a Assets>,
    /// The ground and sea environment the target's land grids take.
    pub land: LandEnv,
    /// The target's generic object effects become (`effect_template`).
    pub effect_object: Option<&'a Template>,
    /// The target's install and database, which a target whose codec saves
    /// its objects (2070) copies their records from.
    pub target: Option<(&'a Install, &'a Assets)>,
}

impl IslandEditor {
    /// Builds `src`'s island into this one with [`convert`], then redoes
    /// what the editor keeps beside the island: grids that follow the new
    /// props and terrain, the terrain tiles, and a fresh history.
    pub fn convert_from(
        &mut self,
        src: &IslandEditor,
        rows: &[PropRow],
        slots: &[SlotRow],
        opts: &Options,
        effect_object: Option<&Template>,
        ground: Option<&BTreeMap<u8, u8>>,
    ) -> Report {
        let with = Context {
            games: [src.module(), self.module()],
            sets: [src.ground_set(), self.ground_set()],
            assets: src.assets.as_deref(),
            land: self.land_environment(),
            effect_object,
            target: self.install.as_deref().zip(self.assets.as_deref()),
        };
        let report = convert(
            &src.island,
            &mut self.island,
            &with,
            rows,
            slots,
            opts,
            ground,
        );
        // Tree cells, forest chunks, environment and herds follow the new
        // props and terrain; buildable land follows the slope.
        self.derive_grids(None);
        self.history = History::new(false);
        self.ground_stale = true;
        self.splat_stale = true;
        self.scene_stale = true;
        self.selection.clear();
        self.curve_point = None;
        self.drawn_look = None;
        self.rebuild_tiles(None);
        // A game's own ground draws the new paint at once, as an undo does.
        let [w, d] = self.island.doc.field.grid.size();
        if self.own_ground.is_some() {
            self.show_paint([0., 0., w, d]);
        }
        report
    }

    /// What conversion reads of this island's ground: its palette, or the
    /// colour ids of the game's own ground with their looks, stacked as
    /// its paint stacks them.
    pub fn ground_set(&self) -> Option<Set> {
        if let Some(palette) = &self.palette {
            return Some(Set::of(palette));
        }
        let stack = self.island.doc.materials.as_ref()?.paint.stack()?;
        Some(Set {
            stack: stack.clone(),
            materials: self.own_materials.clone(),
        })
    }
}

/// Builds `src` into `dst`, a blank island of the target game
/// (`IslandEditor::create` with `Start::Sea`) whose harbour records are
/// loaded. A smaller source sits centred on the target; a larger one loses
/// its rim. Derives `dst`'s land and areas, which harbours and slots are
/// placed by; the gameplay grids, tiles and history are the editor's to
/// redo (`IslandEditor::convert_from`).
pub fn convert(
    src: &Island,
    dst: &mut Island,
    with: &Context,
    rows: &[PropRow],
    slots: &[SlotRow],
    opts: &Options,
    ground: Option<&BTreeMap<u8, u8>>,
) -> Report {
    let mut report = Report::default();
    let [src_set, dst_set] = &with.sets;
    let dst_game = with.games[1];
    let islands = dst_game.islands();
    let names = islands.map_or(&GroundNames::NONE, |i| &i.rules().ground_names);
    // A target whose own codec saves its objects (2070) takes buildings,
    // decoration and slots as copies of its records, and has no harbour or
    // water records the later games' tools could place.
    let native = dst.saves_through_codec();
    let mut records = Records::new(dst);
    // The coastline GUIDs harbour pads follow: without records, no harbour
    // is placed, so there are none.
    let coasts: Vec<u32> = (dst.harbour.iter())
        .flat_map(|lib| [Piece::Coast, Piece::StartCoast].map(|p| lib.guid(p)))
        .flatten()
        .collect();
    let src_size = src.doc.field.grid.size()[0];
    let offset = (dst.doc.field.grid.size()[0] - src_size) / 2.;
    let to_dst = |p: [f32; 3]| [p[0] + offset, p[1], p[2] + offset];
    // A cropped source loses what lies past the target's edge.
    let dst_size = dst.doc.field.grid.size()[0];
    let inside = |p: [f32; 3]| {
        let q = to_dst(p);
        (0.0..dst_size).contains(&q[0]) && (0.0..dst_size).contains(&q[2])
    };
    // Off the source, its edge continues: shipped islands end in open sea.
    let clamp = |v: f32| (v - offset).clamp(0., src_size);

    let g = dst.doc.field.grid;
    // 2070 blocks the ground new heights lift off buildable land, as its
    // sculpts do (`grids::block_lifted`).
    let was = super::grids::lift_rule(&dst.doc).then(|| dst.doc.field.clone());
    let heights = &mut Arc::make_mut(&mut dst.doc.field).heights;
    for z in 0..g.depth {
        for x in 0..g.width {
            let [wx, wz] = g.world(x, z);
            heights[z * g.width + x] = src.doc.field.height([clamp(wx), clamp(wz)]);
        }
    }
    if let Some(was) = was {
        let cells = gpui_editor_kit::heightfield::Rect {
            x0: 0,
            z0: 0,
            x1: g.width - 1,
            z1: g.depth - 1,
        };
        super::grids::block_lifted(&mut dst.doc, |x, z| was.at(x, z), cells);
    }
    // Areas first: harbours and slots take their object area from them.
    super::new::derive_land(&mut dst.doc, with.land);
    harbour::refresh_areas(&mut dst.doc, &coasts, true);

    // Source materials and the tint correction of each (see below). Within
    // one game the target province's own colors are the point, so nothing
    // is pulled back toward the source: Albion's rocks under Latium's
    // correction came out purple.
    let mut recolor: Option<Recolor> = None;
    let correct = with.games[0].game() != dst_game.game();
    if opts.materials
        && let (Some(sp), Some(dp), Some(sm)) = (src_set, dst_set, &src.doc.materials)
        && let Some(dm) = &mut dst.doc.materials
    {
        let used: Vec<(GroundMaterial, f64)> = material_weights(sm, &sp.stack)
            .into_iter()
            .filter_map(|(id, w)| Some((sp.get(id)?.clone(), w)))
            .collect();
        // The wizard's choices, else each material's closest look.
        let map = ground
            .cloned()
            .unwrap_or_else(|| map_materials(&used, &dp.materials, names));
        if correct {
            recolor = Some((sm, &sp.stack, linear_colors(sp), linear_colors(dp)));
        }
        let sea = map_materials(
            &[((0, "underwater".into(), None), 1.)],
            &dp.materials,
            names,
        )[&0];
        let full = transfer_materials(
            (sm, &sp.stack),
            (dm, &dp.stack),
            &map,
            offset,
            src_size,
            sea,
        );
        if full > 0 {
            *report.losses.entry(Lost::ChunkFull).or_default() += full;
        }
        for ((id, file, _), _) in &used {
            if let Some(m) = map.get(id).and_then(|&t| dp.get(t)) {
                report.materials.push((stem(file), stem(&m.1)));
            }
        }
    } else if opts.materials && src.doc.materials.is_some() && dst.doc.materials.is_some() {
        report.lose(Lost::Paint, None);
    }

    // Painted tint times a correction toward the source's color: the source
    // blend at this spot divided by the target blend. Stand-ins never match
    // exactly, and the base layer under every block shows through at edges.
    // Linear light, clamped so a bad stand-in doesn't get smeared over.
    let painted = src.doc.tint.as_ref().filter(|_| opts.tint);
    // 2070 has no tint map: its paint is the whole look.
    let tinted = (dst.doc.materials.as_ref()).is_none_or(|m| m.paint.a7m().is_some());
    if tinted && (painted.is_some() || recolor.is_some()) {
        let neutral =
            anno_formats::a7m::tint::TintMap::multiplier(anno_formats::a7m::tint::TintMap::NEUTRAL);
        dst.doc.tint = Some(Arc::new(TintMap::from_fn(
            tint::WORK_LEVEL,
            dst.doc.field.grid.size().map(|v| v.round() as u32),
            |x, z| {
                let base = painted.map_or(neutral, |t| tint::sample(t, clamp(x), clamp(z)));
                let (Some((sm, sp, from, to)), Some(dm), Some(dp)) =
                    (&recolor, &dst.doc.materials, dst_set)
                else {
                    return base;
                };
                let q = [x - offset, z - offset];
                if !(0.0..src_size).contains(&q[0]) || !(0.0..src_size).contains(&q[1]) {
                    return base;
                }
                let want = blend(&source_cover(sm, sp, q), from, [0.2; 3]);
                let got = blend(&source_cover(dm, &dp.stack, [x, z]), to, [0.2; 3]);
                std::array::from_fn(|c| {
                    base[c] * ((want[c] + 1e-3) / (got[c] + 1e-3)).clamp(0.4, 2.5)
                })
            },
        )));
    }

    let src_assets = with.assets;
    let piece = |guid: u32| src_assets.and_then(|a| harbour::piece_of(with.games[0], a, guid));
    let moved = |path: &[PathPoint]| -> Vec<PathPoint> {
        path.iter()
            .map(|p| PathPoint {
                p: to_dst(p.p),
                ..*p
            })
            .collect()
    };

    if opts.water && native {
        for o in src.doc.objects.iter() {
            let water = river::water_kind(src.harbour.as_deref(), src_assets, o.guid);
            if water.is_some() && o.path.is_some() {
                report.lose(Lost::WaterDropped, Some(o.id));
            }
        }
    } else if opts.water
        && let Some(lib) = dst.harbour.clone()
    {
        for o in src.doc.objects.iter() {
            let water = river::water_kind(src.harbour.as_deref(), src_assets, o.guid);
            let (Some(lake), Some(path)) = (water, o.path.as_deref()) else {
                continue;
            };
            if !path.iter().all(|p| inside(p.p)) {
                report.lose(
                    if lake {
                        Lost::LakeCropped
                    } else {
                        Lost::RiverCropped
                    },
                    Some(o.id),
                );
                continue;
            }
            let curve = moved(path);
            let what = if lake { Piece::Lake } else { Piece::River };
            let Some(store) = &mut dst.object_store else {
                break;
            };
            store.objects.clone_from(&dst.doc.objects);
            let areas = (dst.doc.grids.as_ref())
                .and_then(|g| g.layer(anno_formats::a7m::grids::Kind::AreaIds));
            let Some(mut placed) = lib.place(store, what, areas, curve[0].p) else {
                continue;
            };
            if let Some(g) = &mut dst.doc.grids {
                river::paint_curve(Arc::make_mut(g), &curve, lake, true);
            }
            placed.path = Some(curve);
            Arc::make_mut(&mut dst.doc.objects).push(placed);
            if lake {
                report.lakes += 1;
            } else {
                report.rivers += 1;
            }
        }
    }

    if opts.harbours {
        for o in src.doc.objects.iter() {
            let (Some(p @ (Piece::Coast | Piece::StartCoast)), Some(path)) =
                (piece(o.guid), o.path.as_deref())
            else {
                continue;
            };
            if !path.iter().all(|p| inside(p.p)) {
                report.lose(Lost::HarbourCropped, Some(o.id));
                continue;
            }
            // 2070 lays its harbours as 1 m tiles along the coast, by its
            // own rules; a coast with no water to face drops it.
            if native {
                let coast: Vec<[f32; 2]> = moved(path).iter().map(|p| [p.p[0], p.p[2]]).collect();
                let codec = islands.and_then(|i| i.codec().own());
                let laid = (with.target.zip(codec))
                    .and_then(|((install, _), c)| c.harbour_along(install, dst, &coast));
                match laid {
                    Some(Ok(tiles)) if !tiles.is_empty() => {
                        let last = tiles.iter().map(|o| o.id).max().unwrap_or(0);
                        records.next = records.next.max(last + 1);
                        Arc::make_mut(&mut dst.doc.objects).extend(tiles);
                        report.harbours += 1;
                    }
                    Some(Err(e)) => {
                        eprintln!("convert: harbour: {e:#}");
                        report.lose(Lost::HarbourDropped, Some(o.id));
                    }
                    _ => report.lose(Lost::HarbourDropped, Some(o.id)),
                }
                continue;
            }
            dst.start_coast = p == Piece::StartCoast;
            if dst.add_harbour(moved(path)).is_ok() {
                harbour::refresh_areas(&mut dst.doc, &coasts, false);
                report.harbours += 1;
            }
        }
        dst.start_coast = false;
    }

    if opts.slots && native {
        for o in src.doc.objects.iter() {
            let Some(kind) = piece(o.guid).and_then(slot_of) else {
                continue;
            };
            if !inside(o.position) {
                report.lose(Lost::SlotCropped, Some(o.id));
                continue;
            }
            let model = (islands.and_then(|i| i.codec().own())).and_then(|c| c.slot_model(kind.0));
            let at = Transform {
                position: to_dst(o.position),
                rotation: o.rotation,
                scale: [1.; 3],
            };
            match model.is_some_and(|m| records.place(dst, with, m, at)) {
                true => report.slots += 1,
                false => report.lose(Lost::SlotDropped(kind.0, kind.1), Some(o.id)),
            }
        }
    } else if opts.slots
        && let Some(lib) = dst.harbour.clone()
        && let Some(islands) = islands
        && let Some(harbours) = islands.harbours()
    {
        for o in src.doc.objects.iter() {
            let Some(kind) = piece(o.guid).and_then(slot_of) else {
                continue;
            };
            if !inside(o.position) {
                report.lose(Lost::SlotCropped, Some(o.id));
                continue;
            }
            let target = (slots.iter())
                .find(|r| r.kind == kind)
                .and_then(|r| r.target)
                .filter(|&(r, fixed)| lib.guid(r.pieces(fixed).0).is_some());
            let Some((resource, fixed)) = target else {
                report.lose(Lost::SlotDropped(kind.0, kind.1), Some(o.id));
                continue;
            };
            dst.resource = resource;
            dst.fixed_slots = fixed;
            // Turned the way shipped slots of the kind face (`push_slot`).
            let at = to_dst(o.position);
            let river = (dst.doc.grids.as_ref()).and_then(|g| g.layer(Kind::River));
            let step = islands.rules().slot_yaw_step;
            let yaw = harbour::slot_yaw(&dst.doc.field, river, resource, [at[0], at[2]], step);
            if dst.add_slot(at, yaw, harbours).is_ok() {
                report.slots += 1;
            }
        }
        dst.resource = Resource::Mine;
        dst.fixed_slots = false;
    }

    if opts.props {
        let target: HashMap<&str, &PropRow> = (rows.iter())
            .filter(|r| r.target().is_some())
            .map(|r| (r.source.as_str(), r))
            .collect();
        let mut effects_placed = 0;
        let mut place = |dst: &mut Island, path: &str, record: island::Prop| {
            let Some(row) = target.get(path).filter(|_| inside(record.position)) else {
                return false;
            };
            let at = Transform {
                position: record.position,
                rotation: record.rotation,
                scale: record.scale.map(|v| v * row.scale),
            };
            // Marked as brought, but the port never ran or came back empty. The
            // model path would point at nothing.
            if row.brings() && row.pieces.is_empty() && row.effects.is_empty() {
                return false;
            }
            let single = [(
                row.target().unwrap_or_default().to_owned(),
                Transform::IDENTITY,
            )];
            // A brought row places what its port wrote, which for an effect
            // is no model at all; its target was never written.
            let pieces = if row.pieces.is_empty() && !row.brings() {
                &single[..]
            } else {
                &row.pieces[..]
            };
            // A ported group, like a house with its torches. Each piece goes at
            // the instance's transform composed with the piece's offset.
            let mut placed = 0;
            for (model, offset) in pieces {
                let t = at.then(offset);
                let t = Transform {
                    position: to_dst(t.position),
                    ..t
                };
                // A building or decoration model there is a record of its
                // asset, not a prop.
                let object = native
                    && (with.target).is_some_and(|(_, a)| {
                        islands.is_some_and(|i| {
                            i.codec().own().is_some_and(|c| !c.places_as_prop(a, model))
                        })
                    });
                if object {
                    placed += usize::from(records.place(dst, with, model, t));
                    continue;
                }
                if native && !records.prop(dst, with, model) {
                    continue;
                }
                placed += 1;
                let source = island::Prop {
                    file: dst.doc.mesh_index(model),
                    position: t.position,
                    rotation: t.rotation,
                    scale: t.scale,
                    ..record.clone()
                };
                dst.doc.props.push(Prop {
                    id: dst.next_id,
                    matrix: doc::trs(source.position, source.rotation, source.scale),
                    source,
                    auto_cliff: false,
                });
                dst.next_id += 1;
            }
            // Effects become objects: the target's generic cfg object, pointed
            // at the effect. Kept upright, turned by yaw only.
            if let (Some(tpl), Some(store)) = (with.effect_object, &mut dst.object_store) {
                for (effect, offset) in &row.effects {
                    let t = at.then(offset);
                    store.objects.clone_from(&dst.doc.objects);
                    let mut o = store.add_template(tpl, harbour::BASE as u16, to_dst(t.position));
                    o.config = Some(effect.replace('/', "\\"));
                    o.set_direction(-2. * t.rotation[1].atan2(t.rotation[3]));
                    o.scale = [t.scale[1]; 3];
                    Arc::make_mut(&mut dst.doc.objects).push(o);
                    effects_placed += 1;
                    placed += 1;
                }
            }
            // On 2070, where none of its models has a record to copy, it
            // is lost.
            !native || placed > 0
        };
        for p in src.doc.props.iter() {
            let Some(path) = src.doc.meshes.get(p.source.file as usize) else {
                continue;
            };
            if place(dst, path, p.source.clone()) {
                report.props += 1;
            } else {
                report.lose(Lost::PropDropped, Some(p.id));
            }
        }
        // Objects with a model (decorations and gameplay buildings) become
        // plain props of the mapped model.
        for o in src.doc.objects.iter() {
            let Some(model) = object_model(src, with.games[0], src_assets, o) else {
                continue;
            };
            let record = island::Prop {
                file: 0,
                position: o.position,
                rotation: o.rotation,
                scale: o.scale,
                color: [1.; 4],
                adapt_terrain_height: None,
            };
            if place(dst, &model, record) {
                report.props += 1;
            } else {
                report.lose(Lost::PropDropped, Some(o.id));
            }
        }
        report.effects = effects_placed;
    }

    report
}

/// New objects of a target whose codec saves its objects (2070), as Place
/// makes them: a copy of a record of the asset that draws a model, one
/// found per model and reused.
struct Records {
    /// Model to the object a new one copies; `None` where the target has no
    /// record of it.
    made: HashMap<String, Option<anno_core::island::objects::Object>>,
    /// Prop models new to the target, and whether a record saves them.
    props: HashMap<String, bool>,
    /// The id the next new object takes; the codec numbers it on save.
    next: u64,
}

impl Records {
    fn new(dst: &Island) -> Records {
        let next = dst.doc.objects.iter().map(|o| o.id).max().unwrap_or(0) + 1;
        Records {
            made: HashMap::new(),
            props: HashMap::new(),
            next,
        }
    }

    /// Whether a new prop drawing `model` can save on `dst`, a target whose
    /// codec saves its records (2070): a kind the file holds saves as a
    /// copy of its record; a new kind asks the game for a shipped record
    /// of its asset, which `dst` keeps for its save (`Island::foreign`).
    /// False where no island ships one: the prop is lost here, counted
    /// with the props dropped, and never fails the save.
    fn prop(&mut self, dst: &mut Island, with: &Context, model: &str) -> bool {
        if let Some(&saves) = self.props.get(model) {
            return saves;
        }
        if dst.doc.meshes.iter().any(|m| m == model) {
            return true;
        }
        let (Some((install, assets)), Some(islands)) = (with.target, with.games[1].islands())
        else {
            return false;
        };
        let codec = islands.codec().own();
        let saves = match codec.and_then(|c| c.object_for(install, assets, dst, model)) {
            Some(Ok(o)) => {
                super::keep_foreign(dst, &o);
                true
            }
            Some(Err(e)) => {
                eprintln!("convert: {model}: {e:#}");
                false
            }
            None => true,
        };
        self.props.insert(model.to_owned(), saves);
        saves
    }

    /// Puts a new object of the asset drawing `model` on `dst` at `t`;
    /// false where the target has no record of it to copy.
    fn place(&mut self, dst: &mut Island, with: &Context, model: &str, t: Transform) -> bool {
        let (Some((install, assets)), Some(islands)) = (with.target, with.games[1].islands())
        else {
            return false;
        };
        let template = (self.made.entry(model.to_owned())).or_insert_with(|| {
            match (islands.codec().own()).and_then(|c| c.object_for(install, assets, dst, model)) {
                Some(Ok(o)) => Some(o),
                Some(Err(e)) => {
                    eprintln!("convert: {model}: {e:#}");
                    None
                }
                None => None,
            }
        });
        let Some(template) = template else {
            return false;
        };
        let mut o = template.copied(self.next);
        self.next += 1;
        o.position = t.position;
        o.rotation = t.rotation;
        o.direction = -2. * t.rotation[1].atan2(t.rotation[3]);
        o.scale = t.scale;
        Arc::make_mut(&mut dst.doc.objects).push(o);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::{
        Ground, Options, Role, Swatch, convert, fit, ground_role, map_materials, map_props,
        prop_role,
    };
    use crate::island::test_support::{
        assets, create, harbour_library, install, install_1800, saved, staged,
    };
    use crate::island::{
        IslandEditor, Start, catalog::Catalog, harbour, harbour::Resource, new, port,
    };
    use anno_core::game::Game;
    use anno_core::harbour::Piece;
    use anno_core::island::{Materials, Paint, Stack};
    use anno_formats::vfs::Install;
    use std::collections::BTreeMap;
    use std::sync::Arc;

    // --- retarget into and out of 2070 paint ---

    /// A 2070 registry of `(id, SplatID)` colour textures, plus scatter id
    /// 60, which no ground maps to.
    fn layer_registry(textures: &[(u8, u16)]) -> Arc<anno_core::island::Registry> {
        let mut xml: String = (textures.iter())
            .map(|&(id, rank)| {
                format!(
                    "<Texture><GUID>{id}</GUID><SplatID>{rank}</SplatID>\
                     <DiffuseTexture>t{id}_diff.dds</DiffuseTexture></Texture>"
                )
            })
            .collect();
        xml += "<Texture><GUID>60</GUID><SplatID>2</SplatID></Texture>";
        let ground = anno_formats::isd::terrain::GroundTextures::parse(&xml).unwrap();
        Arc::new(anno_core::island::Registry::new(ground, 4.).unwrap())
    }

    /// The known field: three grounds whose shares vary smoothly across
    /// the island and sum to 1, as `(base, middle, top)` at metres (x, z).
    fn field(x: f32, z: f32) -> [f32; 3] {
        let top = 0.45 + 0.4 * (x / 9.).sin();
        let middle = (1. - top) * (0.5 + 0.4 * (z / 7.).cos());
        [1. - top - middle, middle, top]
    }

    /// `shares` merged by id, for comparing with the field.
    fn share_of(shares: &[(u8, f32)], id: u8) -> f32 {
        shares.iter().filter(|e| e.0 == id).map(|e| e.1).sum()
    }

    /// A 2070 island `chunks` across with no paint, as `Materials`.
    fn layers_island(chunks: usize, registry: Arc<anno_core::island::Registry>) -> Materials {
        let layers = anno_core::island::Layers::new([chunks; 2], registry);
        Materials::from_paint(Paint::Layers(layers), 1.)
    }

    /// An a7m island `texels` across at `per_metre`, all zero, as `Materials`.
    fn a7m_island(texels: usize, per_metre: f32) -> Materials {
        let grid = |side: usize| anno_formats::a7m::island::Grid {
            width: side,
            height: side,
            data: vec![0u32; side * side],
        };
        let a7m = anno_core::island::A7m {
            ids: anno_core::island::Layer::new(&grid(texels / 2)),
            alphas: anno_core::island::Layer::new(&grid(texels)),
        };
        Materials::from_paint(Paint::A7m(a7m), per_metre)
    }

    /// The largest difference between `field` and what `m` shows, by the
    /// `stack` that draws it, over its finest map's texels, where `ids`
    /// name the field's three grounds in `m`. 2070 paint sits on metre
    /// vertices; a7m texels are sampled at their centres.
    fn worst(m: &Materials, stack: &Stack, ids: [u8; 3]) -> f32 {
        let [w, h] = m.paint.size();
        let centre = if m.paint.a7m().is_some() { 0.5 } else { 0. };
        let mut worst = 0f32;
        for z in 0..h {
            for x in 0..w {
                let cover = m.paint.shown(x, z, stack);
                let n = cover.len();
                let q = [x as f32 + centre, z as f32 + centre].map(|v| v / m.per_metre);
                let want = field(q[0], q[1]);
                for (k, &id) in ids.iter().enumerate() {
                    worst = worst.max((share_of(&cover[..n], id) - want[k]).abs());
                }
            }
        }
        worst
    }

    // The 2070 inverse: a known share field written into `Layers` and read
    // back the way the ground stacks it (`Stack::shares` at sharpness 5)
    // comes back within 0.03 on every vertex. Each painted chunk starts
    // with layer 0 and holds the base opaque everywhere; the scatter id is
    // never written.
    #[test]
    fn layers_paint_takes_a_known_share_field_back_within_tolerance() {
        let registry = layer_registry(&[(0, 0), (5, 1), (2, 3), (9, 7)]);
        let mut m = layers_island(4, registry.clone());
        let Paint::Layers(layers) = &mut m.paint else {
            unreachable!()
        };
        let lost = super::encode_layers(layers, 5, |x, z| {
            let f = field(x as f32, z as f32);
            vec![(5, f[0]), (2, f[1]), (9, f[2])]
        });
        assert_eq!(lost, 0);
        let worst = worst(&m, registry.stack(), [5, 2, 9]);
        eprintln!("layers: worst share error {worst:.4}");
        assert!(worst < 0.03, "{worst}");
        let Paint::Layers(layers) = &m.paint else {
            unreachable!()
        };
        for k in 0..16 {
            assert_eq!(layers.ids(k).first(), Some(&0), "chunk {k}");
            assert!(!layers.ids(k).contains(&60));
            let base = layers.ids(k).iter().position(|&id| id == 5).unwrap();
            assert!(layers.plane(k, base).iter().all(|&c| c == 255));
        }
    }

    // A chunk whose vertices show more grounds than the 24 layers 2070
    // draws keeps the 24 showing most, gives the rest of each such vertex
    // to the base, counts those vertices as lost, and still agrees with its
    // neighbours along every shared edge.
    #[test]
    fn a_full_chunk_keeps_24_layers_and_its_edges() {
        let textures: Vec<(u8, u16)> = (0..=30).map(|id| (id, u16::from(id))).collect();
        let registry = layer_registry(&textures);
        let mut m = layers_island(3, registry);
        let Paint::Layers(layers) = &mut m.paint else {
            unreachable!()
        };
        // Every vertex half the base, half one of 30 grounds in turn.
        let lost = super::encode_layers(layers, 1, |x, z| {
            vec![(1, 0.5), (((x * 7 + z * 3) % 29 + 2) as u8, 0.5)]
        });
        assert!(lost > 0);
        let side = 3 * 16 + 1;
        for k in 0..9 {
            assert!(layers.ids(k).len() <= 24, "chunk {k}");
        }
        // Every chunk holding a vertex holds the same coverage there.
        for z in 0..side {
            for x in 0..side {
                let held: Vec<_> = anno_formats::isd::terrain::chunks_at((3, 3), x, z)
                    .map(|k| {
                        let at = (z - k / 3 * 16) * 17 + x - k % 3 * 16;
                        let mut v: Vec<(u8, u8)> = (layers.ids(k).iter().enumerate())
                            .map(|(l, &id)| (id, layers.plane(k, l)[at]))
                            .filter(|e| e.1 > 0)
                            .collect();
                        v.sort();
                        v
                    })
                    .collect();
                assert!(held.windows(2).all(|w| w[0] == w[1]), "vertex {x},{z}");
            }
        }
    }

    // The a7m inverse, the other way: the same field into six slots per
    // block at 5-bit weights comes back within 0.07 on every texel. At
    // sharpness 1 a weight step moves alpha by 2/31, so up to 0.032 a
    // layer, and two layers above the base add up.
    #[test]
    fn a7m_paint_takes_a_known_share_field_back_within_tolerance() {
        let mut stack = Stack::default();
        for id in [1, 2, 3] {
            stack.push(id, 1.);
        }
        let mut m = a7m_island(128, 2.);
        let Paint::A7m(paint) = &mut m.paint else {
            unreachable!()
        };
        super::encode_a7m(paint, &stack, 1, |x, y| {
            let f = field((x as f32 + 0.5) / 2., (y as f32 + 0.5) / 2.);
            vec![(1, f[0]), (2, f[1]), (3, f[2])]
        });
        let worst = worst(&m, &stack, [1, 2, 3]);
        eprintln!("a7m: worst share error {worst:.4}");
        assert!(worst < 0.07, "{worst}");
    }

    // Both directions through `transfer_materials`, as a conversion runs
    // it: the field in a7m paint retargets into 2070 paint through a map
    // and back into a fresh a7m island through the inverse map. Each step
    // shows the field within its tolerance: 0.06 in 2070 paint (the a7m
    // weights' error plus the inverse's), 0.1 back in a7m.
    #[test]
    fn paint_retargets_into_2070_and_back_within_tolerance() {
        let mut stack = Stack::default();
        for id in [1, 2, 3] {
            stack.push(id, 1.);
        }
        let registry = layer_registry(&[(0, 0), (5, 1), (2, 3), (9, 7)]);
        let mut src = a7m_island(128, 2.);
        let Paint::A7m(paint) = &mut src.paint else {
            unreachable!()
        };
        super::encode_a7m(paint, &stack, 1, |x, y| {
            let f = field((x as f32 + 0.5) / 2., (y as f32 + 0.5) / 2.);
            vec![(1, f[0]), (2, f[1]), (3, f[2])]
        });
        // Island sizes match: 64 m, no offset.
        let mut mid = layers_island(4, registry.clone());
        let into = BTreeMap::from([(1, 5), (2, 2), (3, 9)]);
        let lost = super::transfer_materials(
            (&src, &stack),
            (&mut mid, registry.stack()),
            &into,
            0.,
            64.,
            5,
        );
        assert_eq!(lost, 0);
        let there = worst(&mid, registry.stack(), [5, 2, 9]);
        let mut back = a7m_island(128, 2.);
        let out = BTreeMap::from([(5, 1), (2, 2), (9, 3)]);
        let lost = super::transfer_materials(
            (&mid, registry.stack()),
            (&mut back, &stack),
            &out,
            0.,
            64.,
            1,
        );
        assert_eq!(lost, 0);
        let again = worst(&back, &stack, [1, 2, 3]);
        eprintln!("into 2070: worst {there:.4}; back: worst {again:.4}");
        assert!(there < 0.06, "{there}");
        assert!(again < 0.1, "{again}");
    }

    // Needs Anno 117 and 2070: a small 117 pool island converted onto
    // shipped n_m02 maps its ground into 2070's colour layers, losing
    // none, and saves through 2070's codec. The save reopens with exactly
    // the paint the conversion made, which shows several grounds.
    #[test]
    fn a_117_island_converted_to_2070_saves_and_reopens_with_its_paint() {
        let (Some(a117), Some(dir)) = (install(), anno_formats::vfs::install_dir("ANNO2070_DIR"))
        else {
            return;
        };
        let families = crate::games::profile(Game::Anno2070).archive_families;
        let a2070 = Install::open(dir, families).unwrap();
        let src = IslandEditor::load(
            crate::games::module(Game::Anno117),
            &a117,
            crate::island::test_support::SMALL,
        )
        .unwrap();
        let path = "data/levels/islands/normal/n_m02.isd";
        let mut dst =
            IslandEditor::load(crate::games::module(Game::Anno2070), &a2070, path).unwrap();
        let opts = Options {
            materials: true,
            tint: false,
            props: false,
            water: false,
            harbours: false,
            slots: false,
        };
        let report = dst.convert_from(&src, &[], &[], &opts, None, None);
        eprintln!("{}", report.summary());
        assert!(!report.materials.is_empty(), "{}", report.summary());
        assert!(!report.losses.contains_key(&super::Lost::Paint));
        assert!(dst.island.doc.tint.is_none(), "2070 has no tint");
        let paint = |e: &IslandEditor| e.island.doc.materials.as_ref().unwrap().paint.clone();
        let converted = paint(&dst);
        let Paint::Layers(layers) = &converted else {
            panic!("2070 paint")
        };
        let [w, h] = layers.chunks();
        let mut shown = std::collections::BTreeSet::new();
        for z in 0..h * 16 + 1 {
            for x in 0..w * 16 + 1 {
                shown.insert(converted.dominant(x, z));
            }
        }
        assert!(shown.len() >= 3, "{shown:?}");

        let (_config, mut project) = staged(&a2070, &mut dst);
        let back = crate::island::test_support::reopen(&a2070, &mut project, path);
        assert!(
            paint(&back) == converted,
            "reopens with the converted paint"
        );
    }

    // Needs Anno 117 and 2070: a small 117 pool island converted onto
    // n_m02, cleared as a new island is. Its trees become a 2070 tree n_m02
    // never had, its bushes 2070 bushes (n_m02 has no grass list), its
    // rocks the rock n_m02 holds, and its other models copies of one of
    // n_m02's buildings, all but one model, which maps to nothing 2070
    // draws. Its mine slots become 2070 mine bases, and its harbours 2070
    // harbour tiles along their coast. The report counts the props left out
    // and the slots, harbours and water 2070 has no records for. Saved and reopened, the island holds exactly what was placed,
    // each record with the GUID its model maps to, where it was put.
    #[test]
    fn a_117_island_converted_to_2070_saves_and_reopens_with_its_objects() {
        use anno_core::game::Module;
        use anno_formats::isd::objects::Section as List;
        let (Some(a117), Some(dir)) = (install(), anno_formats::vfs::install_dir("ANNO2070_DIR"))
        else {
            return;
        };
        let module = &anno2070::MODULE;
        let a2070 = Arc::new(Install::open(dir, module.profile().archive_families).unwrap());
        let db = Arc::new(
            anno_formats::assets::Assets::load(&a2070, module.database().unwrap()).unwrap(),
        );
        let mut src = IslandEditor::load(
            crate::games::module(Game::Anno117),
            &a117,
            crate::island::test_support::SMALL,
        )
        .unwrap();
        src.assets = Some(assets(&a117));
        let path = "data/levels/islands/normal/n_m02.isd";
        let mut dst = IslandEditor::load_with(
            crate::games::module(Game::Anno2070),
            &a2070,
            path,
            Some((a2070.clone(), db.clone())),
        )
        .unwrap();
        let shipped = {
            let doc = anno_formats::isd::Doc::parse(&a2070.read(path).unwrap()).unwrap();
            anno_formats::isd::objects::read(&doc, path).unwrap()
        };
        dst.island.doc.props.clear();
        Arc::make_mut(&mut dst.island.doc.objects).clear();

        let template = |guid: u32| db.get(guid).map_or("", |a| a.template.as_str());
        let cfg = |guid: u32| db.get(guid).unwrap().cfgs[0].replace('\\', "/");
        let on_island = |guid: u32| shipped.iter().any(|o| o.guid == guid);
        let tree = (db.iter())
            .find(|a| a.template == "Tree" && !a.cfgs.is_empty() && !on_island(a.guid))
            .unwrap()
            .guid;
        let rock = (shipped.iter())
            .find(|o| {
                matches!(
                    template(o.guid),
                    "NatureMountainObject" | "SimpleStoneObject"
                )
            })
            .unwrap()
            .guid;
        let (bush, building) = (315, shipped[0].guid);
        assert!(
            !on_island(bush) && shipped[0].section == anno_formats::isd::objects::Section::Handle
        );
        let nothing = "data/graphics/nothing/nothing.cfg";

        let models = super::source_models(&src);
        let target = |k: usize, path: &str| match prop_role(path) {
            Some(Role::Broadleaf | Role::Conifer | Role::Palm | Role::DeadTree) => cfg(tree),
            Some(Role::Bush | Role::Grass | Role::Reed) => cfg(bush),
            Some(Role::Rock | Role::Pebbles | Role::Cliff) => cfg(rock),
            // The first model of no role maps to nothing 2070 draws.
            _ if k
                == models
                    .iter()
                    .position(|m| prop_role(&m.0).is_none())
                    .unwrap() =>
            {
                nothing.to_owned()
            }
            _ => cfg(building),
        };
        let rows: Vec<super::PropRow> = (models.iter().enumerate())
            .map(|(k, (path, count, placed))| super::PropRow {
                source: path.clone(),
                count: *count,
                placed: *placed,
                role: prop_role(path),
                choices: vec![target(k, path)],
                pick: Some(0),
                scale: 1.,
                confidence: None,
                pieces: Vec::new(),
                effects: Vec::new(),
            })
            .collect();
        let expect = |want: &str| -> usize {
            (rows.iter())
                .filter(|r| r.choices[0] == want)
                .map(|r| r.count)
                .sum()
        };
        let report = dst.convert_from(&src, &rows, &[], &Options::default(), None, None);
        eprintln!("{}", report.summary());

        // What the source holds that 2070 has records for, and not.
        let src_db = src.assets.as_deref().unwrap();
        let piece = |guid| harbour::piece_of(src.module(), src_db, guid);
        let slots: Vec<_> = (src.island.doc.objects.iter())
            .filter_map(|o| piece(o.guid).and_then(super::slot_of))
            .collect();
        let mines = (slots.iter())
            .filter(|(r, _)| matches!(r, Resource::Mine | Resource::NewWorldMine | Resource::River))
            .count();
        let harbours = (src.island.doc.objects.iter())
            .filter(|o| matches!(piece(o.guid), Some(Piece::Coast | Piece::StartCoast)))
            .count();
        assert!(
            !slots.is_empty() && harbours > 0,
            "{slots:?}, {harbours} harbours"
        );
        assert_eq!(report.slots, mines);
        assert_eq!(
            (report.losses.iter())
                .filter(|(l, _)| matches!(l, super::Lost::SlotDropped(..)))
                .map(|(_, n)| n)
                .sum::<usize>(),
            slots.len() - mines
        );
        // Harbours become 2070's harbour tiles where the coast has water to
        // face; the rest are dropped.
        let dropped = report.losses.get(&super::Lost::HarbourDropped).copied();
        assert_eq!(report.harbours + dropped.unwrap_or(0), harbours);
        assert!(report.harbours > 0, "{}", report.summary());
        assert_eq!(report.lost(super::Lost::PropDropped), expect(nothing));
        assert!(expect(&cfg(tree)) > 0 && expect(&cfg(bush)) > 0 && expect(&cfg(building)) > 0);

        let (_config, project) = staged(&a2070, &mut dst);
        let saved = anno_formats::isd::Doc::parse(&project.files[path]).unwrap();
        let back = anno_formats::isd::objects::read(&saved, path).unwrap();
        let largest = shipped.iter().map(|o| o.id).max().unwrap();
        assert!(
            back.iter().all(|o| o.id > largest),
            "n_m02's own records went"
        );
        let count = |guid: u32, list: List| {
            (back.iter())
                .filter(|o| o.guid == guid && o.section == list)
                .count()
        };
        assert_eq!(count(tree, List::Nature), expect(&cfg(tree)));
        assert_eq!(count(bush, List::Grass), expect(&cfg(bush)));
        assert_eq!(count(rock, List::Nature), expect(&cfg(rock)));
        assert_eq!(count(building, List::Handle), expect(&cfg(building)));
        assert_eq!(count(335, List::Handle) + count(350, List::Handle), mines);
        assert_eq!(
            back.len(),
            dst.island.doc.props.len() + dst.island.doc.objects.len()
        );
        // Each where the conversion put it, to the file's 1/4096 m.
        let stored = |p: [f32; 3]| p.map(|v| ((v * 4096.).round() as i64 as f32 / 4096.).to_bits());
        let mut placed: Vec<[u32; 3]> = (dst.island.doc.props.iter())
            .map(|p| stored([p.matrix[12], p.matrix[13], p.matrix[14]]))
            .chain(dst.island.doc.objects.iter().map(|o| stored(o.position)))
            .collect();
        let mut read: Vec<[u32; 3]> = back.iter().map(|o| o.position.map(f32::to_bits)).collect();
        placed.sort_unstable();
        read.sort_unstable();
        assert!(
            placed == read,
            "the records stand where the conversion put them"
        );
    }

    // Needs Anno 117 and 2070: a small 117 pool island converted onto the
    // first shipped 2070 island without trees or rocks. Its trees, bushes
    // and rocks become 2070 trees, bushes and rocks the island never had and
    // holds no record of their list to copy, so each kind copies a shipped
    // record of its asset, found at convert time as Place finds one. The
    // save, which used to fail, reopens with every one in its asset's list.
    #[test]
    fn a_117_island_converts_onto_a_2070_island_without_trees_and_saves() {
        use anno_core::game::Module;
        use anno_formats::isd::objects::Section as List;
        let (Some(a117), Some(dir)) = (install(), anno_formats::vfs::install_dir("ANNO2070_DIR"))
        else {
            return;
        };
        let module = &anno2070::MODULE;
        let a2070 = Arc::new(Install::open(dir, module.profile().archive_families).unwrap());
        let db = Arc::new(
            anno_formats::assets::Assets::load(&a2070, module.database().unwrap()).unwrap(),
        );
        let read = |path: &str| {
            let doc = anno_formats::isd::Doc::parse(&a2070.read(path).unwrap()).unwrap();
            anno_formats::isd::objects::read(&doc, path).unwrap()
        };
        let path = (0..a2070.file_count())
            .map(|i| {
                a2070
                    .file_path(anno_formats::vfs::FileId(i as u32))
                    .to_owned()
            })
            .filter(|p| p.ends_with(".isd"))
            .find(|p| read(p).iter().all(|o| o.section != List::Nature))
            .unwrap();
        let mut src = IslandEditor::load(
            crate::games::module(Game::Anno117),
            &a117,
            crate::island::test_support::SMALL,
        )
        .unwrap();
        src.assets = Some(assets(&a117));
        let mut dst = IslandEditor::load_with(
            crate::games::module(Game::Anno2070),
            &a2070,
            &path,
            Some((a2070.clone(), db.clone())),
        )
        .unwrap();
        let shipped = read(&path);

        let first = |template: &str| {
            (db.iter())
                .find(|a| a.template == template && !a.cfgs.is_empty())
                .unwrap()
                .guid
        };
        let (tree, rock, bush) = (first("Tree"), first("NatureMountainObject"), 315);
        let cfg = |guid: u32| db.get(guid).unwrap().cfgs[0].replace('\\', "/");
        let nothing = "data/graphics/nothing/nothing.cfg".to_owned();
        let target = |path: &str| match prop_role(path) {
            Some(Role::Broadleaf | Role::Conifer | Role::Palm | Role::DeadTree) => cfg(tree),
            Some(Role::Bush | Role::Grass | Role::Reed) => cfg(bush),
            Some(Role::Rock | Role::Pebbles | Role::Cliff) => cfg(rock),
            _ => nothing.clone(),
        };
        let rows: Vec<super::PropRow> = (super::source_models(&src).iter())
            .map(|(path, count, placed)| super::PropRow {
                source: path.clone(),
                count: *count,
                placed: *placed,
                role: prop_role(path),
                choices: vec![target(path)],
                pick: Some(0),
                scale: 1.,
                confidence: None,
                pieces: Vec::new(),
                effects: Vec::new(),
            })
            .collect();
        let expect = |want: &str| -> usize {
            (rows.iter())
                .filter(|r| r.choices[0] == want)
                .map(|r| r.count)
                .sum()
        };
        let opts = Options {
            slots: false,
            harbours: false,
            water: false,
            ..Options::default()
        };
        let report = dst.convert_from(&src, &rows, &[], &opts, None, None);
        eprintln!("{path}: {}", report.summary());
        assert_eq!(report.lost(super::Lost::PropDropped), expect(&nothing));
        assert!(expect(&cfg(tree)) > 0 && expect(&cfg(rock)) > 0 && expect(&cfg(bush)) > 0);
        assert!(!dst.island.foreign.is_empty());

        let (_config, project) = staged(&a2070, &mut dst);
        let saved = anno_formats::isd::Doc::parse(&project.files[path.as_str()]).unwrap();
        let back = anno_formats::isd::objects::read(&saved, &path).unwrap();
        let count = |guid: u32, list: List| {
            (back.iter())
                .filter(|o| o.guid == guid && o.section == list)
                .count()
        };
        let on_island = |guid: u32| shipped.iter().filter(|o| o.guid == guid).count();
        assert_eq!(
            count(tree, List::Nature),
            expect(&cfg(tree)) + on_island(tree)
        );
        assert_eq!(
            count(rock, List::Nature),
            expect(&cfg(rock)) + on_island(rock)
        );
        assert_eq!(
            count(bush, List::Grass),
            expect(&cfg(bush)) + on_island(bush)
        );
    }

    #[test]
    fn props_sort_into_roles_by_path() {
        let cases = [
            (
                "data/graphics/props/terrain_props/vegetation/trees/moderate/tree_fir_02_var0.prp",
                Some(Role::Conifer),
            ),
            (
                "data/graphics/props/terrain_props/vegetation/trees/jungle/tree_jungle_palm_04_03.prp",
                Some(Role::Palm),
            ),
            (
                "data/graphics/props/terrain_props/vegetation/trees/dead/tree_dead_stump_top_03.prp",
                Some(Role::Stump),
            ),
            (
                "data/graphics/props/terrain_props/vegetation/bushes/bush_jungle_03_medium_mountain.prp",
                Some(Role::Bush),
            ),
            (
                "data/graphics/props/terrain_props/vegetation/water/underwater_plant_02.prp",
                Some(Role::Underwater),
            ),
            (
                "data/graphics/props/terrain_props/rocks/rocks_pebbles/pebbles_03.prp",
                Some(Role::Pebbles),
            ),
            (
                "data/base/graphics/roman/environment/props/vegetation/trees/tree_laurus_01/tree_laurus_01_b.cfg",
                Some(Role::Broadleaf),
            ),
            (
                "data/base/graphics/roman/environment/props/vegetation/trees/tree_maritime_pine_01/tree_maritime_pine_01_k.cfg",
                Some(Role::Conifer),
            ),
            (
                "data/base/graphics/celtic/environment/props/vegetation/reed/reed_03_b/reed_03_b_marshveg.cfg",
                Some(Role::Reed),
            ),
            (
                "data/base/graphics/roman/environment/props/rocks/roman_rock_02/roman_rock_02_b.cfg",
                Some(Role::Rock),
            ),
            (
                "data/base/graphics/celtic/environment/props/rocks/celtic_mountain_element_01/celtic_mountain_element_01.cfg",
                Some(Role::Cliff),
            ),
            ("data/dev/graphics/tools/templates/empty.cfg", None),
        ];
        for (path, role) in cases {
            assert_eq!(prop_role(path), role, "{path}");
        }
    }

    // The pirate island's paths, with the measured values. Cobblestone should
    // get 1800's laid stone, not a cliff. Plain mud should get plain mud, not
    // the roots that are closest in color. Flat warm sand should beat seaweed
    // and green speckles.
    #[test]
    fn paths_and_beaches_keep_their_look() {
        assert_eq!(ground_role("pavement_02_b.xml"), Ground::Stony);
        assert_eq!(ground_role("square_stone_floor_01_a.xml"), Ground::Stony);
        assert_eq!(ground_role("m_stone_03.xml"), Ground::Stony);
        assert_eq!(ground_role("t_path_01.xml"), Ground::Stony);
        // Grass with stones in it is not a road.
        assert_ne!(ground_role("m_gras_stone_01.xml"), Ground::Stony);
        assert_eq!(ground_role("rocks_sharp_01.xml"), Ground::Stony);
        assert_eq!(ground_role("rock_limestone_01.xml"), Ground::Rock);
        let sw = |color: [f32; 3], contrast: f32| Some(Swatch { color, contrast });
        let used = [
            (
                (1, "pavement_02_b.xml".to_owned(), sw([56., 49., 46.], 4.)),
                1.,
            ),
            (
                (
                    2,
                    "mud_01_darker_dummy.xml".to_owned(),
                    sw([58., 53., 48.], 1.4),
                ),
                1.,
            ),
            (
                (3, "beach_01.xml".to_owned(), sw([101., 93., 86.], 3.7)),
                1.,
            ),
        ];
        let target = [
            (10, "a_hill_12.xml".to_owned(), sw([76., 76., 76.], 6.)),
            (11, "m_stone_03.xml".to_owned(), sw([80., 81., 82.], 7.7)),
            (12, "m_roots_02.xml".to_owned(), sw([56., 47., 35.], 6.)),
            (13, "m_mud_01.xml".to_owned(), sw([72., 58., 39.], 2.6)),
            (14, "m_mud_01b.xml".to_owned(), sw([78., 73., 58.], 3.)),
            (15, "s_beach_02.xml".to_owned(), sw([87., 89., 80.], 11.)),
            (16, "s_beach_11.xml".to_owned(), sw([123., 118., 110.], 22.)),
            (
                17,
                "m_beach_01.xml".to_owned(),
                sw([130., 126., 118.], 10.5),
            ),
        ];
        let map = map_materials(
            &used,
            &target,
            &crate::games::rules(anno_core::game::Game::Anno1800).ground_names,
        );
        assert_eq!(map[&1], 11);
        assert!(matches!(map[&2], 13 | 14), "{}", map[&2]);
        assert_eq!(map[&3], 17);
        // Roots still stand in for roots.
        let roots = [(
            (4, "forest_roots_01.xml".to_owned(), sw([56., 47., 35.], 6.)),
            1.,
        )];
        assert_eq!(
            map_materials(
                &roots,
                &target,
                &crate::games::rules(anno_core::game::Game::Anno1800).ground_names
            )[&4],
            12
        );
    }

    #[test]
    fn ground_materials_map_by_kind_and_color() {
        assert_eq!(
            ground_role("landscape/terrain/moderate/m_gras_02.xml"),
            Ground::Grass
        );
        assert_eq!(ground_role("texture_pool/a_hill_12.xml"), Ground::Rock);
        assert_eq!(ground_role("texture_pool/c_gravle_01.xml"), Ground::Gravel);
        assert_eq!(
            ground_role("moderate/m_underwater_01.xml"),
            Ground::Underwater
        );
        assert_eq!(ground_role("grass/grass_01_h_dry.xml"), Ground::Dry);
        assert_eq!(ground_role("cliff/cliff_limestone_01.xml"), Ground::Rock);
        let mid = Some(Swatch::flat([80., 90., 40.]));
        let light = Some(Swatch::flat([140., 150., 90.]));
        let rock = Some(Swatch::flat([120., 110., 100.]));
        let used = [
            ((10, "m_gras_02.xml".to_owned(), mid), 900.),
            ((19, "m_gras_05.xml".to_owned(), light), 500.),
            (
                (
                    2,
                    "m_mountain_03.xml".to_owned(),
                    Some(Swatch::flat([60., 55., 50.])),
                ),
                300.,
            ),
            ((3, "m_underwater_01.xml".to_owned(), None), 200.),
            (
                (9, "c_snow_01.xml".to_owned(), Some(Swatch::flat([230.; 3]))),
                10.,
            ),
        ];
        let target = [
            (
                0,
                "mud_01.xml".to_owned(),
                Some(Swatch::flat([110., 100., 80.])),
            ),
            (1, "grass_01_b_light.xml".to_owned(), light),
            (4, "grass_01_e.xml".to_owned(), mid),
            (6, "cliff_limestone_01.xml".to_owned(), rock),
            (8, "cliff_limestone_01.xml".to_owned(), rock),
            (
                5,
                "rock_limestone_01.xml".to_owned(),
                Some(Swatch::flat([100., 90., 80.])),
            ),
            (
                7,
                "beach_04.xml".to_owned(),
                Some(Swatch::flat([136., 116., 93.])),
            ),
        ];
        let map = map_materials(
            &used,
            &target,
            &crate::games::rules(anno_core::game::Game::Anno117).ground_names,
        );
        // Each takes the closest color of its kind.
        assert_eq!(map[&10], 4);
        assert_eq!(map[&19], 1);
        assert_eq!(map[&2], 5);
        // No sea-floor material in the set: sand stands in; snow → the
        // lightest rock, listed once however often the set repeats it.
        assert_eq!(map[&3], 7);
        assert_eq!(map[&9], 6);
    }

    // Ground converts through the set's draw order alone (`Stack`), with no
    // textures: laid into a blank map of the same set, each material shows
    // as much as it did, and two ids mapped onto one show as their sum.
    #[test]
    fn ground_converts_through_the_stack_alone() {
        use super::{material_weights, transfer_materials};
        use crate::island::doc::{A7m, Layer, Materials, Paint};
        use anno_core::island::Stack;
        use anno_formats::a7m::island::{Grid, pack_slots};
        use gpui_editor_kit::heightfield::Brush;
        use std::collections::BTreeMap;
        // 32 m at 2 texels per metre, all id 0 at full weight.
        let blank = || {
            let grid = |w: usize, word: u32| Grid {
                width: w,
                height: w,
                data: vec![word; w * w],
            };
            Materials::from_paint(
                Paint::A7m(A7m {
                    ids: Layer::new(&grid(32, pack_slots([0, 1, 2, 3, 4, 5]))),
                    alphas: Layer::new(&grid(64, pack_slots([31, 0, 0, 0, 0, 0]))),
                }),
                2.,
            )
        };
        let mut stack = Stack::default();
        for id in [0, 3, 7] {
            stack.push(id, 0.5);
        }
        let mut src = blank();
        let brush = Brush {
            radius: 6.,
            strength: 1.,
            ..Brush::default()
        };
        assert!(src.paint([10., 10.], &brush, 3));
        assert!(src.paint([21., 20.], &brush, 7));
        let before = material_weights(&src, &stack);
        let total: f64 = before.values().sum();
        let converted = |map: &BTreeMap<u8, u8>| {
            let mut dst = blank();
            let full = transfer_materials((&src, &stack), (&mut dst, &stack), map, 0., 32., 0);
            assert_eq!(full, 0);
            material_weights(&dst, &stack)
        };
        let after = converted(&BTreeMap::from([(0, 0), (3, 3), (7, 7)]));
        for (id, w) in &before {
            let got = after.get(id).copied().unwrap_or(0.);
            assert!(
                (got - w).abs() < total * 0.01,
                "{id}: {w:.0} became {got:.0}"
            );
        }
        let merged = converted(&BTreeMap::from([(0, 0), (3, 3), (7, 3)]));
        let both = before[&3] + before[&7];
        assert!(
            (merged[&3] - both).abs() < total * 0.01,
            "{both:.0} became {:.0}",
            merged[&3]
        );
        assert!(merged.get(&7).copied().unwrap_or(0.) < total * 0.001);
    }

    const PIRATE: &str = "data/base/provinces/celtic/islands/3rdparty/celtic_island_3rdparty_pirate_01/celtic_island_3rdparty_pirate_01.a7m";

    /// Rendered looks of `models` (source game) and of `catalog`'s match pool
    /// (target game). Keep the returned folders alive while using the looks.
    fn looks(
        from: Install,
        models: &[(String, usize, f32)],
        to: Install,
        catalog: &Catalog,
    ) -> (
        [tempfile::TempDir; 2],
        crate::island::catalog::Thumbs,
        crate::island::catalog::Thumbs,
    ) {
        use crate::island::catalog::Thumbs;
        let dirs = [tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap()];
        // In each game's own look, as the import wizard renders them.
        let thumbs = |install: Install, dir: &tempfile::TempDir| {
            let look = crate::games::module(crate::games::of(&install)).model_look();
            Thumbs::new(Arc::new(install), dir.path().to_owned(), look)
        };
        let mut source = thumbs(from, &dirs[0]);
        let mut target = thumbs(to, &dirs[1]);
        source.request(models.iter().map(|m| m.0.as_str()));
        target.request(super::match_pool(catalog));
        while source.pending() || target.pending() {
            std::thread::sleep(std::time::Duration::from_millis(20));
            source.receive();
            target.receive();
        }
        (dirs, source, target)
    }

    /// The 117 pirate island with its assets, plus a new Old World island of
    /// the same size, named `name`, to convert it into.
    fn pirate_and_new(a117: &Install, a1800: &Install, name: &str) -> (IslandEditor, IslandEditor) {
        let mut src =
            IslandEditor::load(crate::games::module(crate::games::of(&a117)), a117, PIRATE)
                .unwrap();
        src.assets = Some(assets(a117));
        let dst_assets = assets(a1800);
        let skeletons = new::skeletons(
            crate::games::module(crate::games::of(&a1800)),
            a1800,
            Some(&dst_assets),
        );
        let skeleton = super::pick_size(&skeletons, "moderate", src.world()[0])
            .unwrap()
            .clone();
        let mut dst = create(a1800, skeleton, Start::Sea, name);
        dst.island.harbour = Some(harbour_library(a1800, &dst_assets, &dst));
        dst.assets = Some(dst_assets);
        (src, dst)
    }

    /// `path` from `src` converted into a new island of `region` on `dst`.
    /// With `bring`, every model that can be ported is brought, like the wizard
    /// at 100%.
    fn run(
        src_install: &Install,
        dst_install: &Install,
        path: &str,
        region: &str,
        bring: bool,
        remap: impl Fn(&mut Vec<super::SlotRow>),
    ) -> (IslandEditor, IslandEditor, super::Report) {
        let src_assets = assets(src_install);
        let mut src = IslandEditor::load(
            crate::games::module(crate::games::of(&src_install)),
            src_install,
            path,
        )
        .unwrap();
        src.assets = Some(src_assets.clone());
        src.island.harbour = harbour::Library::load(
            crate::games::module(crate::games::of(&src_install)),
            src_install,
            &src_assets,
            &src.region(),
        )
        .ok()
        .map(Arc::new);
        let size = src.world()[0];
        let dst_assets = assets(dst_install);
        let skeletons = new::skeletons(
            crate::games::module(crate::games::of(&dst_install)),
            dst_install,
            Some(&dst_assets),
        );
        let skeleton = super::pick_size(&skeletons, region, size).unwrap().clone();
        let mut dst = create(dst_install, skeleton, Start::Sea, "Converted");
        dst.island.harbour = Some(harbour_library(dst_install, &dst_assets, &dst));
        dst.assets = Some(dst_assets);
        let catalog = Catalog::build(
            crate::games::rules(crate::games::of(&dst_install)),
            dst_install,
        )
        .unwrap();
        let mut rows = map_props(
            &super::source_models(&src),
            &catalog,
            (
                crate::games::module(crate::games::of(&src_install)),
                crate::games::module(crate::games::of(&dst_install)),
            ),
            region,
            super::BRING_BELOW,
            |_| None,
            |_| None,
        );
        let (from, to) = (
            crate::games::module(crate::games::of(&src_install)),
            crate::games::module(crate::games::of(&dst_install)),
        );
        for r in rows
            .iter_mut()
            .filter(|r| bring && port::portable(from, to, &r.source))
        {
            let Ok(p) = port::port(from, to, src_install, dst_install, &r.source) else {
                continue;
            };
            r.choices.insert(0, port::target(from, to, &r.source));
            (r.pick, r.scale, r.pieces, r.effects) = (Some(0), 1., p.pieces, p.effects);
        }
        let mut slots = super::map_slots(&src, dst.island.harbour.as_deref().unwrap());
        remap(&mut slots);
        let effect = super::effect_template(dst_install, &catalog);
        let report = dst.convert_from(
            &src,
            &rows,
            &slots,
            &Options::default(),
            effect.as_ref(),
            None,
        );
        eprintln!("{path} → {region}: {}", report.summary());
        (src, dst, report)
    }

    fn check(
        src: &IslandEditor,
        mut dst: IslandEditor,
        report: &super::Report,
        region: &str,
        dst_install: &Install,
    ) {
        let offset = (dst.world()[0] as f32 - src.world()[0] as f32) / 2.;
        for p in [[0.5, 0.5], [0.3, 0.6], [0.7, 0.4]] {
            let q = p.map(|v| v * src.world()[0] as f32);
            let (a, b) = (
                src.island.doc.field.height(q),
                dst.island.doc.field.height([q[0] + offset, q[1] + offset]),
            );
            assert!((a - b).abs() < 0.05, "height {a} became {b}");
        }
        assert!(
            report.props * 10 >= src.island.doc.props.len() * 7,
            "{}",
            report.summary()
        );
        for p in dst.island.doc.props.iter() {
            let mesh = &dst.island.doc.meshes[p.source.file as usize];
            assert!(dst_install.file(mesh).is_some(), "{mesh} not shipped");
            assert!(
                super::fit(
                    crate::games::rules(crate::games::of(&dst_install)),
                    region,
                    mesh
                )
                .is_some(),
                "{mesh} is another region’s"
            );
        }
        // Land ground maps to land materials, deep water to sea floor or sand.
        let palette = dst.palette.as_ref().unwrap();
        let m = dst.island.doc.materials.as_ref().unwrap();
        let kind = |p: [f32; 2]| {
            super::ground_role(&palette.materials[&super::doc::shown(m, p, Some(palette))].file)
        };
        let mid = dst.world()[0] as f32 / 2.;
        let deep = [offset + 2., offset + 2.];
        assert!(dst.island.doc.field.height(deep) < -2.);
        assert!(
            matches!(kind(deep), Ground::Underwater | Ground::Sand),
            "{:?}",
            kind(deep)
        );
        let land = (0..400)
            .map(|k| {
                [
                    mid + (k % 20) as f32 * 4. - 40.,
                    mid + (k / 20) as f32 * 4. - 40.,
                ]
            })
            .filter(|&p| dst.island.doc.field.height(p) > 3.)
            .collect::<Vec<_>>();
        assert!(land.iter().all(|&p| kind(p) != Ground::Underwater));
        // Land shows what it showed in the source game, judged by the stacked
        // ground: neither island is mostly beach. Reading weights as shares
        // made 1800's New World forest floor (beach under grass and rock)
        // beach, and the converted island white.
        let sand = land.iter().filter(|&&p| kind(p) == Ground::Sand).count();
        assert!(
            sand * 4 < land.len(),
            "{sand} of {} land samples sand",
            land.len()
        );
        let src_water = src
            .island
            .doc
            .objects
            .iter()
            .filter(|o| src.water_kind(o.guid).is_some())
            .count();
        assert_eq!(report.rivers + report.lakes, src_water);
        // Saves and reads back.
        let (_config, project) = staged(dst_install, &mut dst);
        let island = saved(&project, &dst.path);
        assert_eq!(island.props().len(), dst.island.doc.props.len());
    }

    // Conversion needs no editor: run on the bare islands it builds what the
    // editor's conversion builds (heights, ground, props, objects, report),
    // and the editor adds only what it keeps beside the island, such as a
    // fresh history.
    #[test]
    fn islands_convert_without_an_editor() {
        let (Some(a1800), Some(a117)) = (install_1800(), install()) else {
            return;
        };
        // A pool island: harbours, slots and rivers to place.
        let path = "data/base/provinces/roman/islands/pool/roman_island_medium_01/roman_island_medium_01.a7m";
        let mut src = IslandEditor::load(crate::games::module(Game::Anno117), &a117, path).unwrap();
        src.assets = Some(assets(&a117));
        let dst_assets = assets(&a1800);
        let skeletons = new::skeletons(
            crate::games::module(Game::Anno1800),
            &a1800,
            Some(&dst_assets),
        );
        let skeleton = super::pick_size(&skeletons, "moderate", src.world()[0]).unwrap();
        let target = |name: &str| {
            let mut dst = create(&a1800, skeleton.clone(), Start::Sea, name);
            dst.island.harbour = Some(harbour_library(&a1800, &dst_assets, &dst));
            dst.assets = Some(dst_assets.clone());
            dst
        };
        let (mut by_editor, mut bare) = (target("By editor"), target("Bare"));
        let lib = by_editor.island.harbour.clone().unwrap();
        let slots = super::map_slots(&src, &lib);
        let opts = Options::default();
        let report = by_editor.convert_from(&src, &[], &slots, &opts, None, None);
        let with = super::Context {
            games: [src.module(), bare.module()],
            sets: [
                src.palette.as_deref().map(super::Set::of),
                bare.palette.as_deref().map(super::Set::of),
            ],
            assets: src.assets.as_deref(),
            land: bare.land_environment(),
            effect_object: None,
            target: None,
        };
        let alone = convert(
            &src.island,
            &mut bare.island,
            &with,
            &[],
            &slots,
            &opts,
            None,
        );
        assert_eq!(report.summary(), alone.summary());
        assert!(
            report.harbours > 0 && report.slots > 0 && report.rivers > 0,
            "{}",
            report.summary()
        );
        let (a, b) = (&by_editor.island.doc, &bare.island.doc);
        assert_eq!(a.field.heights, b.field.heights);
        assert!(a.materials.as_ref().unwrap().paint == b.materials.as_ref().unwrap().paint);
        assert_eq!(
            a.tint.as_ref().map(|t| &t.texels),
            b.tint.as_ref().map(|t| &t.texels)
        );
        let placed = |d: &crate::island::doc::Doc| -> Vec<(u64, u32, u16, [f32; 3])> {
            (d.objects.iter())
                .map(|o| (o.id, o.guid, o.area, o.position))
                .collect()
        };
        assert_eq!(placed(a), placed(b));
        assert_eq!(a.props.len(), b.props.len());
        assert!(!by_editor.history.can_undo());
    }

    // A crop counts what it drops. One slot, one river or lake and one
    // harbour moved past the source's edge are past the target's too; each
    // is reported once, at its loss-table level, and nothing else is.
    #[test]
    fn a_crop_reports_each_slot_river_and_harbour_it_drops() {
        use super::Lost;
        use anno_core::island::Level;
        let Some(a1800) = install_1800() else {
            return;
        };
        let path = a1800
            .search("moderate_m_01_river_01.a7m", 10)
            .into_iter()
            .map(|id| a1800.file_path(id).to_owned())
            .find(|p| p.ends_with(".a7m"))
            .unwrap();
        let assets = assets(&a1800);
        let mut src =
            IslandEditor::load(crate::games::module(Game::Anno1800), &a1800, &path).unwrap();
        src.assets = Some(assets.clone());
        src.island.harbour = Some(harbour_library(&a1800, &assets, &src));
        let skeletons = new::skeletons(crate::games::module(Game::Anno1800), &a1800, Some(&assets));
        let skeleton = super::pick_size(&skeletons, "moderate", src.world()[0]).unwrap();
        let mut dst = create(&a1800, skeleton.clone(), Start::Sea, "Cropped");
        dst.island.harbour = Some(harbour_library(&a1800, &assets, &dst));
        dst.assets = Some(assets.clone());
        let (src_size, dst_size) = (src.world()[0] as f32, dst.world()[0] as f32);
        assert!(dst_size >= src_size, "the target holds the source");
        // Far enough west to stay past the edge on the larger target.
        let west = src_size + (dst_size - src_size) / 2. + 20.;
        let lib = src.island.harbour.clone().unwrap();
        let find = |src: &IslandEditor,
                    is: &dyn Fn(&anno_core::island::objects::Object) -> bool| {
            src.island.doc.objects.iter().position(is).unwrap()
        };
        let slot = find(&src, &|o| {
            harbour::piece_of(src.module(), &assets, o.guid)
                .and_then(super::slot_of)
                .is_some()
        });
        let water = find(&src, &|o| {
            o.path.is_some() && src.water_kind(o.guid).is_some()
        });
        let lake = src.water_kind(src.island.doc.objects[water].guid) == Some(true);
        let coast = [lib.guid(Piece::Coast), lib.guid(Piece::StartCoast)];
        let harbour_at = find(&src, &|o| o.path.is_some() && coast.contains(&Some(o.guid)));
        let objects = Arc::make_mut(&mut src.island.doc.objects);
        objects[slot].position[0] -= west;
        for k in [water, harbour_at] {
            for p in objects[k].path.as_mut().unwrap() {
                p.p[0] -= west;
            }
        }
        let slots = super::map_slots(&src, dst.island.harbour.as_deref().unwrap());
        let report = dst.convert_from(&src, &[], &slots, &Options::default(), None, None);
        let water = if lake {
            Lost::LakeCropped
        } else {
            Lost::RiverCropped
        };
        let want = [
            (water, 1),
            (Lost::SlotCropped, 1),
            (Lost::HarbourCropped, 1),
        ];
        // Props and slots it never had stand-ins for are other losses.
        let cropped: std::collections::BTreeMap<_, _> = (report.losses.iter())
            .filter(|(l, _)| !matches!(l, Lost::PropDropped | Lost::SlotDropped(..)))
            .map(|(&l, &n)| (l, n))
            .collect();
        assert_eq!(cropped, want.into_iter().collect(), "{}", report.summary());
        assert_eq!(water.level(), Level::Shape);
        assert_eq!(Lost::SlotCropped.level(), Level::Gameplay);
        assert_eq!(Lost::HarbourCropped.level(), Level::Gameplay);
        assert!(
            report.summary().contains("1 slot cropped"),
            "{}",
            report.summary()
        );
    }

    #[test]
    fn converts_an_1800_island_into_117() {
        let (Some(a1800), Some(a117)) = (install_1800(), install()) else {
            return;
        };
        let path = a1800
            .search("moderate_m_01_river_01.a7m", 10)
            .into_iter()
            .map(|id| a1800.file_path(id).to_owned())
            .find(|p| p.ends_with(".a7m"))
            .unwrap();
        // Latium has no clay: by default those slots are left out; here
        // they become mines.
        let clay = (Resource::Clay, false);
        let (src, dst, report) = run(&a1800, &a117, &path, "roman", false, |rows| {
            let row = rows.iter_mut().find(|r| r.kind == clay).unwrap();
            assert_eq!(row.target, None);
            row.target = Some((Resource::Mine, false));
        });
        use super::Lost;
        assert_eq!(report.lost(Lost::SlotDropped(clay.0, clay.1)), 0);
        let oil = Lost::SlotDropped(Resource::Oil, false);
        assert_eq!(report.lost(oil), 9);
        // Dropped slots are gameplay losses, confirmed once per kind.
        let problem = (report.problems().into_iter())
            .find(|p| p.what == "Oil slots left out")
            .expect("the oil slots are a problem");
        assert_eq!(
            (problem.level, problem.count),
            (anno_core::island::Level::Gameplay, 9)
        );
        assert_eq!(
            problem.level.policy(),
            anno_core::island::Policy::ConfirmEach
        );
        assert_eq!(report.slots, 8 + 6, "{}", report.summary());
        assert!(report.rivers > 0, "{}", report.summary());
        // The river's surface stands over its bed, as it did in 1800.
        for o in (dst.island.doc.objects.iter()).filter(|o| dst.water_kind(o.guid) == Some(false)) {
            let points = o.path.as_deref().unwrap();
            let wet = (points.iter())
                .filter(|p| p.p[1] > dst.island.doc.field.height([p.p[0], p.p[2]]))
                .count();
            eprintln!("river: {wet} of {} points above the ground", points.len());
            assert!(wet * 10 >= points.len() * 8);
        }
        assert!(
            report.harbours > 0 && report.slots > 0,
            "{}",
            report.summary()
        );
        check(&src, dst, &report, "roman", &a117);
    }

    // Needs both games: what a conversion asks follows the loss policy, an
    // acceptance kept with the island stops the same losses asking again,
    // and a slot lost anew asks again.
    #[test]
    fn accepted_losses_ask_again_only_for_newly_lost_items() {
        use anno_core::island::{Acceptance, Level, Problem, accept, asks};
        let (Some(a1800), Some(a117)) = (install_1800(), install()) else {
            return;
        };
        let path = (a1800.search("moderate_m_01_river_01.a7m", 10).into_iter())
            .map(|id| a1800.file_path(id).to_owned())
            .find(|p| p.ends_with(".a7m"))
            .unwrap();
        let (mut src, _, _) = run(&a1800, &a117, &path, "roman", false, |_| {});
        let assets_117 = assets(&a117);
        let skeletons = new::skeletons(
            crate::games::module(Game::Anno117),
            &a117,
            Some(&assets_117),
        );
        let skeleton = super::pick_size(&skeletons, "roman", src.world()[0])
            .unwrap()
            .clone();
        // Converted into a fresh island each time, as Import does.
        let convert = |src: &IslandEditor| -> Vec<Problem> {
            let mut dst = create(&a117, skeleton.clone(), Start::Sea, "Converted");
            dst.island.harbour = Some(harbour_library(&a117, &assets_117, &dst));
            dst.assets = Some(assets_117.clone());
            let slots = super::map_slots(src, dst.island.harbour.as_deref().unwrap());
            dst.convert_from(src, &[], &slots, &Options::default(), None, None)
                .problems()
        };
        let oil = "Oil slots left out";
        let questions = |accepted: &[Acceptance], problems: &[Problem]| -> Vec<Vec<String>> {
            (asks(accepted, Game::Anno117, problems).into_iter())
                .map(|q| q.into_iter().map(|p| p.what.clone()).collect())
                .collect()
        };

        let first = convert(&src);
        let asked = questions(&[], &first);
        // Gameplay losses ask one category at a time; reported ones don't ask.
        assert!(asked.contains(&vec![oil.to_owned()]), "{asked:?}");
        for p in first.iter().filter(|p| p.level <= Level::Decoration) {
            assert!(
                !asked.iter().flatten().any(|w| *w == p.what),
                "{} asked",
                p.what
            );
        }
        let mut accepted = Vec::new();
        for q in asks(&[], Game::Anno117, &first) {
            accept(&mut accepted, Game::Anno117, &q);
        }
        // Kept with the island, through its listing's JSON.
        let accepted: Vec<Acceptance> =
            serde_json::from_str(&serde_json::to_string(&accepted).unwrap()).unwrap();
        // The same island converted again loses the same items: no question.
        let again = convert(&src);
        assert_eq!(questions(&accepted, &again), Vec::<Vec<String>>::new());
        // Accepted for 117 only.
        assert!(!asks(&accepted, Game::Anno1800, &again).is_empty());

        // One more oil slot on the source, lost anew: its category asks again.
        let lib = src.island.harbour.clone().unwrap();
        let oil_guid = lib.guid(Piece::OilSlot).unwrap();
        let next = src.island.doc.objects.iter().map(|o| o.id).max().unwrap() + 1;
        let copy = (src.island.doc.objects.iter())
            .find(|o| o.guid == oil_guid)
            .unwrap()
            .copied(next);
        Arc::make_mut(&mut src.island.doc.objects).push(copy);
        let more = convert(&src);
        let lost = more.iter().find(|p| p.what == oil).unwrap();
        assert!(lost.items.contains(&next));
        assert_eq!(questions(&accepted, &more), vec![vec![oil.to_owned()]]);
    }

    fn look(height: f32, mask: u64, ragged: f32, color: [f32; 3]) -> super::Look {
        super::Look {
            size: [height * 0.6, height, height * 0.6],
            top: color,
            bottom: color,
            aspect: 1.5,
            ragged,
            mask,
        }
    }

    #[test]
    fn looks_compare_by_shape_size_and_color() {
        // A tall leafy crown on a trunk, and a squat solid boulder.
        let crown = 0x0018_183C_7EFF_FF7E;
        let boulder = 0xFFFF_FFFF_FF7E_0000;
        let green = [40., -20., 25.];
        let grey = [55., 0., 3.];
        let tree = look(6., crown, 0.4, green);
        let similar_tree = look(5., crown | 1 << 40, 0.35, [45., -18., 22.]);
        let rock = look(6., boulder, 0.08, grey);
        let d = super::look_distance;
        assert!(d(&tree, &tree) < 1e-6);
        assert!(d(&tree, &similar_tree) < d(&tree, &rock));
        // Size: a 6 m tree is nearer a 5 m one than a 1 m one of the
        // same look.
        let sapling = look(1., crown, 0.4, green);
        assert!(d(&tree, &similar_tree) < d(&tree, &sapling));
        assert!(d(&tree, &sapling) > 0.5);
        // Scaling a look scales its size only.
        assert_eq!(tree.scaled(2.).size[1], 12.);
        assert_eq!(super::height_scale(Some(tree), Some(sapling)), 4.);
        assert_eq!(super::height_scale(Some(tree), Some(similar_tree)), 1.2);
        assert_eq!(super::height_scale(None, Some(tree)), 1.);
    }

    // Needs both games: an 1800 jungle island's props match 117 props by
    // look: palms stay palms, the big jungle trees (taller than any 117
    // tree) become trees rather than the big rocks their size suggests,
    // rocks stay rocks, and the scale keeps their height.
    #[test]
    fn a_latium_island_converts_to_albion() {
        let Some(a117) = install() else {
            return;
        };
        let path = "data/base/provinces/roman/islands/pool/roman_island_medium_01/roman_island_medium_01.a7m";
        let (src, dst, report) = run(&a117, &a117, path, "celtic", false, |_| {});
        assert_eq!(dst.region().to_ascii_lowercase(), "celtic");
        assert_eq!(dst.world(), src.world());
        // Every prop is Albion's or shared; nothing Roman-only stayed, and
        // nothing was ported within one game.
        let roman: Vec<&str> = (dst.island.doc.props.iter())
            .filter_map(|p| {
                dst.island
                    .doc
                    .meshes
                    .get(p.source.file as usize)
                    .map(String::as_str)
            })
            .filter(|m| fit(crate::games::rules(Game::Anno117), "celtic", m).is_none())
            .collect();
        assert!(
            roman.is_empty(),
            "Roman props left: {:?}",
            &roman[..roman.len().min(5)]
        );
        // Nor one only Latium islands place, whatever folder it is in
        // (the maritime pines live in the shared library).
        let catalog = Catalog::build(crate::games::rules(crate::games::of(&a117)), &a117).unwrap();
        let latium_only: Vec<&str> = (dst.island.doc.props.iter())
            .filter_map(|p| dst.island.doc.meshes.get(p.source.file as usize))
            .filter_map(|m| catalog.props.iter().find(|e| e.path == *m))
            .filter(|e| !port::always_brought(&e.path) && e.regions == ["roman"])
            .map(|e| e.path.as_str())
            .collect();
        assert!(
            latium_only.is_empty(),
            "Latium-only props: {:?}",
            &latium_only[..latium_only.len().min(5)]
        );
        assert!(!dst.island.doc.props.is_empty());
        assert!(report.summary().len() > 0);
        // Ground from Albion's set.
        let set = dst.island.material_set.to_ascii_lowercase();
        assert!(set.contains("celtic"), "material set {set}");
    }

    #[test]
    fn an_old_world_island_converts_to_the_new_world() {
        let Some(a1800) = install_1800() else {
            return;
        };
        let path = crate::island::test_support::MODERATE_M_01;
        let (_, dst, _) = run(&a1800, &a1800, path, "colony01", false, |_| {});
        assert_eq!(dst.region().to_ascii_lowercase(), "colony01");
        let foreign: Vec<&str> = (dst.island.doc.props.iter())
            .filter_map(|p| {
                dst.island
                    .doc
                    .meshes
                    .get(p.source.file as usize)
                    .map(String::as_str)
            })
            // Decals and effects carry over as they are: nothing looks alike.
            .filter(|m| {
                !port::always_brought(m)
                    && fit(crate::games::rules(Game::Anno1800), "colony01", m).is_none()
            })
            .collect();
        assert!(
            foreign.is_empty(),
            "Old World props left: {:?}",
            &foreign[..foreign.len().min(5)]
        );
    }

    #[test]
    fn within_one_game_a_poor_match_keeps_its_model() {
        let Some(a117) = install() else {
            return;
        };
        let catalog = Catalog::build(crate::games::rules(crate::games::of(&a117)), &a117).unwrap();
        let tree = "data/base/graphics/roman/environment/props/vegetation/trees/x/roman_tree.cfg";
        let look = |_: &str| Some(look(10., 0xff, 0.2, [0.3, 0.5, 0.2]));
        // Every look is "alike" at 100% and none at 0%: the bar decides.
        let rows = map_props(
            &[(tree.to_owned(), 4, 1.)],
            &catalog,
            (
                crate::games::module(Game::Anno117),
                crate::games::module(Game::Anno117),
            ),
            "celtic",
            1.01,
            look,
            look,
        );
        assert_eq!(rows[0].target(), Some(tree));
        assert!(!rows[0].brings());
    }

    #[test]
    fn jungle_props_match_by_look() {
        let (Some(a1800), Some(a117)) = (install_1800(), install()) else {
            return;
        };
        let path = a1800
            .search("colony01_l_06_river_01.a7m", 10)
            .into_iter()
            .map(|id| a1800.file_path(id).to_owned())
            .find(|p| p.ends_with(".a7m"))
            .unwrap();
        let src = IslandEditor::load(
            crate::games::module(crate::games::of(&a1800)),
            &a1800,
            &path,
        )
        .unwrap();
        let models = super::source_models(&src);
        let catalog = Catalog::build(crate::games::rules(crate::games::of(&a117)), &a117).unwrap();
        let (_dirs, source, target) = looks(a1800, &models, a117, &catalog);
        let rows = map_props(
            &models,
            &catalog,
            (
                crate::games::module(Game::Anno1800),
                crate::games::module(Game::Anno117),
            ),
            "roman",
            super::BRING_BELOW,
            |p| source.look(p),
            |p| target.look(p),
        );
        let role_of = |r: &super::PropRow| r.target().and_then(prop_role);
        let trees = [Role::Broadleaf, Role::Conifer, Role::Palm];
        for r in &rows {
            let name = r.source.rsplit('/').next().unwrap();
            let to = r.target().unwrap_or("-");
            // The regular palms; "special" ones are bent or fallen shapes.
            // 117 has the same palm, so it is never brought over.
            if name.starts_with("tree_jungle_palm_0") {
                assert!(to.contains("palm") && !r.brings(), "{name} -> {to}");
            }
            // Brought over below the confidence bar, else a 117 tree kept
            // at the jungle tree's height.
            assert_eq!(
                r.brings(),
                r.confidence.is_some_and(|c| c < super::BRING_BELOW),
                "{name}"
            );
            if (name.starts_with("tree_jungle_very_large") || name.starts_with("tree_jungle_large"))
                && !r.brings()
            {
                assert!(
                    role_of(r).is_some_and(|t| trees.contains(&t)),
                    "{name} -> {to}"
                );
                // Height kept: the 117 trees are smaller models.
                assert!(r.scale > 1.2, "{name} -> {to} x{}", r.scale);
            }
            if name.starts_with("rock_jungle") {
                assert!(
                    matches!(role_of(r), Some(Role::Rock | Role::Cliff)),
                    "{name} -> {to}"
                );
            }
        }
        assert!(rows.iter().all(|r| r.pick.is_some()));
        let brought: Vec<&str> = (rows.iter().filter(|r| r.brings()))
            .map(|r| r.source.rsplit('/').next().unwrap())
            .collect();
        eprintln!("brought over: {brought:?}");
        // Most large jungle trees: 117 has no tree as tall and alike.
        assert!(brought.iter().any(|n| n.starts_with("tree_jungle_large")));
    }

    // Needs both games. A Roman island's props match 1800 props by look.
    // Nothing named as a tree or rock ends up on a building part (the maritime
    // pines used to become harbour beams), and the pines, which 1800 has
    // nothing like, come over as ported prps at their own size.
    #[test]
    fn roman_props_match_or_port_into_1800() {
        let (Some(a1800), Some(a117)) = (install_1800(), install()) else {
            return;
        };
        let path = "data/base/provinces/roman/islands/pool/roman_island_medium_01/roman_island_medium_01.a7m";
        let src =
            IslandEditor::load(crate::games::module(crate::games::of(&a117)), &a117, path).unwrap();
        let models = super::source_models(&src);
        let catalog =
            Catalog::build(crate::games::rules(crate::games::of(&a1800)), &a1800).unwrap();
        let (_dirs, source, target) = looks(a117, &models, a1800, &catalog);
        let rows = map_props(
            &models,
            &catalog,
            (
                crate::games::module(Game::Anno117),
                crate::games::module(Game::Anno1800),
            ),
            "moderate",
            super::BRING_BELOW,
            |p| source.look(p),
            |p| target.look(p),
        );
        for r in rows.iter().filter(|r| r.role.is_some()) {
            let to = r.target().unwrap_or("-");
            assert!(
                r.brings() || r.target().and_then(prop_role).is_some(),
                "{} -> {to}",
                r.source
            );
        }
        let pines: Vec<_> = (rows.iter())
            .filter(|r| r.source.contains("tree_maritime_pine"))
            .collect();
        assert!(!pines.is_empty());
        for r in pines {
            assert!(r.brings(), "{} -> {:?}", r.source, r.target());
            assert!(r.target().unwrap().ends_with(".prp"));
            assert_eq!(r.scale, 1.);
        }
        // The wizard's slider. At 0 nothing is brought; at 1 every look match
        // that can be ported is, at its own size.
        let at = |below: f32| {
            map_props(
                &models,
                &catalog,
                (
                    crate::games::module(Game::Anno117),
                    crate::games::module(Game::Anno1800),
                ),
                "moderate",
                below,
                |p| source.look(p),
                |p| target.look(p),
            )
        };
        // Decals and effects are brought at any setting, since nothing in 1800
        // looks like them.
        assert!(
            at(0.)
                .iter()
                .filter(|r| r.confidence.is_some())
                .all(|r| !r.brings())
        );
        let all = at(1.);
        for r in all.iter().filter(|r| r.confidence.is_some()) {
            let portable =
                crate::island::port::portable(&anno117::MODULE, &anno1800::MODULE, &r.source);
            assert_eq!(r.brings(), portable, "{}", r.source);
        }
        assert!(
            all.iter().filter(|r| r.brings()).count() > rows.iter().filter(|r| r.brings()).count()
        );
    }

    #[test]
    fn converts_a_117_island_into_1800() {
        let (Some(a1800), Some(a117)) = (install_1800(), install()) else {
            return;
        };
        let path = "data/base/provinces/roman/islands/pool/roman_island_medium_01/roman_island_medium_01.a7m";
        let (src, dst, report) = run(&a117, &a1800, path, "moderate", false, |_| {});
        check(&src, dst, &report, "moderate", &a1800);
        assert!(report.rivers > 0, "{}", report.summary());
    }

    // Needs both games. The pirate island's houses, wall ends and stumps are
    // built from several meshes. Brought over, each placement becomes all of
    // its pieces, at the house's spot and rotated with it.
    #[test]
    fn grouped_models_come_over_whole() {
        let (Some(a1800), Some(a117)) = (install_1800(), install()) else {
            return;
        };
        let path = PIRATE;
        let (src, dst, report) = run(&a117, &a1800, path, "moderate", true, |_| {});
        let house = "house_celtic_mid_long_wooden_01";
        let placed = |doc: &crate::island::doc::Doc| {
            (doc.props.iter())
                .filter(|p| doc.meshes[p.source.file as usize].contains(house))
                .count()
        };
        let objects = (src.island.doc.objects.iter())
            .filter(|o| o.config.as_ref().is_some_and(|c| c.contains(house)))
            .count();
        let houses = placed(&src.island.doc) + objects;
        assert!(houses > 0);
        let pieces = port::port(
            &anno117::MODULE,
            &anno1800::MODULE,
            &a117,
            &a1800,
            &format!("data/base/graphics/library/modular_level_kits/manx_island/{house}.cfg"),
        )
        .unwrap()
        .pieces;
        // Each house's own mesh and its torches all land near it.
        let torches = (dst.island.doc.props.iter())
            .filter(|p| dst.island.doc.meshes[p.source.file as usize].contains("torch"))
            .count();
        assert!(torches >= houses, "{torches} torches for {houses} houses");
        assert!(
            dst.island.doc.props.len() > src.island.doc.props.len(),
            "{}",
            report.summary()
        );
        assert!(pieces.len() > 1);
        let ported = (dst.island.doc.props.iter())
            .filter(|p| {
                crate::island::port::is_ported(&dst.island.doc.meshes[p.source.file as usize])
            })
            .count();
        eprintln!("{ported} ported props; {}", report.summary());

        // The settlement's own buildings are objects placed by GUID, so their
        // model comes from the asset's variations. The six timber houses
        // (`voada_main`), the townhall, harbour houses, kontor and towers come
        // over; villagers, sheep and marker boxes don't.
        let models = super::source_models(&src);
        let row = |name: &str| models.iter().find(|m| m.0.contains(name)).map(|m| m.1);
        assert_eq!(row("voada_main.cfg"), Some(6));
        for building in [
            "voada_celtic_townhall",
            "voada_kontor",
            "harbor_house_01",
            "roman_tower_wood_01",
        ] {
            assert!(row(building).is_some(), "{building} is not a row");
        }
        // Placed by GUID as units: 66 villagers, sheep, marker boxes.
        for unit in [
            "resident_tier01_lean_m_01",
            "/wildlife/sheep/",
            "ui/placeholder/",
        ] {
            assert!(row(unit).is_none(), "{unit} became a row");
        }
        // A villager placed as decoration is still a row, but its rig's tools
        // (hammer, broom, buckets) must not end up standing around it.
        let villager = "data/base/graphics/celtic/units/inhabitants/residents/resident_tier02_plus_m_01/resident_tier02_plus_m_01.cfg";
        assert!(port::port(&anno117::MODULE, &anno1800::MODULE, &a117, &a1800, villager).is_err());
        let placed_in_dst = |name: &str| {
            (dst.island.doc.props.iter())
                .filter(|p| dst.island.doc.meshes[p.source.file as usize].contains(name))
                .count()
        };
        assert!(placed_in_dst("voada_main") >= 6);
        assert!(placed_in_dst("voada_celtic_townhall") > 0);
        // Decals come over as 1800 decal prps: burnt ground placed on its own,
        // and the soil under every wheat field from its projector box.
        assert!(placed_in_dst("burnt_decal_05") > 0);
        assert!(placed_in_dst("wheat_field_module_01x01_v1_bright_decal0") >= 396);
        // Torch fire and smoke become 1800 effect objects.
        let effect = |name: &str| {
            (dst.island.doc.objects.iter())
                .filter(|o| o.config.as_ref().is_some_and(|c| c.contains(name)))
                .count()
        };
        assert!(effect("fps_torch") > 100, "{}", report.summary());
        assert!(effect("fire_smoke_01") > 0, "{}", report.summary());
        assert_eq!(report.effects, effect("effects"));
    }

    // Needs both games. The converted ground shows the colors the source
    // showed. A texel's color is its materials' mean colors by share, times the
    // tint, in linear light. The textures differ (plain 1800 sand for 117's
    // plain sand) and the tint makes up the rest.
    #[test]
    fn converted_ground_keeps_its_colors() {
        let (Some(a1800), Some(a117)) = (install_1800(), install()) else {
            return;
        };
        let path = PIRATE;
        let (src, dst, _) = run(&a117, &a1800, path, "moderate", false, |_| {});
        let shown = |isl: &IslandEditor, q: [f32; 2]| -> [f32; 3] {
            let (m, p) = (
                isl.island.doc.materials.as_ref().unwrap(),
                isl.palette.as_ref().unwrap(),
            );
            let mut out = [0f32; 3];
            for (id, share) in super::source_cover(m, &p.stack, q) {
                let c = super::swatch(&p.materials[&id]).color;
                for k in 0..3 {
                    out[k] += share * (c[k] / 255.).powf(2.2);
                }
            }
            let t = isl
                .island
                .doc
                .tint
                .as_ref()
                .map_or([1.; 3], |t| crate::island::tint::sample(t, q[0], q[1]));
            std::array::from_fn(|k| out[k] * t[k])
        };
        let offset = (dst.world()[0] as f32 - src.world()[0] as f32) / 2.;
        let (mut err, mut n) = (0f32, 0);
        let size = src.world()[0] as f32;
        for i in 0..40 {
            for j in 0..40 {
                let q = [(i as f32 + 0.5) * size / 40., (j as f32 + 0.5) * size / 40.];
                if src.island.doc.field.height(q) < 1. {
                    continue;
                }
                let (a, b) = (shown(&src, q), shown(&dst, [q[0] + offset, q[1] + offset]));
                let la = (a[0] + a[1] + a[2]).max(1e-3);
                err += (0..3).map(|k| (a[k] - b[k]).abs()).sum::<f32>() / la;
                n += 1;
            }
        }
        let mean = err / n as f32;
        eprintln!("{n} land samples, mean color error {:.1}%", mean * 100.);
        assert!(mean < 0.10, "mean color error {:.1}%", mean * 100.);
    }

    // Needs both games. The pirate island's cobblestone is brought instead of
    // matched. Its xml and textures go into the mod, the island gets its own
    // copy of the Old World set with the cobblestone added, the ground shows it
    // where the source did, and after reopening from the mod the palette still
    // has it.
    #[test]
    fn a_brought_ground_material_lands_in_the_islands_own_set() {
        use crate::island::test_support::reopen;
        let (Some(a1800), Some(a117)) = (install_1800(), install()) else {
            return;
        };
        let (src, mut dst) = pirate_and_new(&a117, &a1800, "Brought ground");
        let set = dst.island.material_set.clone();
        let mut rows = super::ground_rows(
            &src,
            dst.palette.as_ref().unwrap(),
            &dst.rules().ground_names,
        );
        let cobble = rows
            .iter()
            .position(|r| r.file.contains("pavement_02_b"))
            .unwrap();
        rows[cobble].pick = super::GroundPick::Bring;
        let source_set = src.island.material_set.clone();
        let brought = super::ground_set(
            crate::games::module(crate::games::of(&a117)),
            &a117,
            &a1800,
            &mut dst,
            (&set, &source_set),
            &rows,
            false,
        )
        .unwrap();
        let id = brought.ids[&rows[cobble].file];
        assert!(dst.palette.as_ref().unwrap().materials.contains_key(&id));
        let map = super::ground_map(&rows, &brought);
        dst.convert_from(&src, &[], &[], &Options::default(), None, Some(&map));
        let own = dst.island.material_set.clone();
        assert!(
            own.starts_with("data/editor/anno117/") && own != set,
            "{own}"
        );
        // Cobblestone shows wherever the source had it.
        let shown = super::material_weights(
            dst.island.doc.materials.as_ref().unwrap(),
            &dst.palette.as_ref().unwrap().stack,
        );
        assert!(shown.get(&id).is_some_and(|&w| w > 0.), "{shown:?}");
        // Saved with its files, the island reads its set and the texture from
        // the mod.
        let (_config, mut project) = staged(&a1800, &mut dst);
        for (file, bytes) in brought.files {
            project.stage(&file, bytes);
        }
        let again = reopen(&a1800, &mut project, &dst.path);
        assert_eq!(
            again.island.a7m().unwrap().material_set_file_name(),
            Some(&*own)
        );
        let m = &again.palette.as_ref().unwrap().materials[&id];
        assert!(m.file.starts_with("data/editor/anno117/"), "{}", m.file);
    }

    // Needs both games. The pirate island keeps its 117 ground. Its own set has
    // every 117 material it shows, at the 117 id and in 117's order so it
    // stacks and blends the same, under the Old World header. The ground layer
    // keeps every id, and after reopening from the mod the island reads exactly
    // those materials.
    #[test]
    fn an_island_keeps_its_own_ground_in_its_own_set() {
        use crate::island::test_support::reopen;
        let (Some(a1800), Some(a117)) = (install_1800(), install()) else {
            return;
        };
        let (src, mut dst) = pirate_and_new(&a117, &a1800, "Own ground");
        let target_set = dst.island.material_set.clone();
        let mut rows = super::ground_rows(
            &src,
            dst.palette.as_ref().unwrap(),
            &dst.rules().ground_names,
        );
        for r in &mut rows {
            r.pick = super::GroundPick::Bring;
        }
        let source_set = src.island.material_set.clone();
        let built = super::ground_set(
            crate::games::module(crate::games::of(&a117)),
            &a117,
            &a1800,
            &mut dst,
            (&target_set, &source_set),
            &rows,
            true,
        )
        .unwrap();
        assert!(built.failed.is_empty(), "{:?}", built.failed);
        // Each shown 117 material keeps its id, in 117's stack order.
        let (sp, dp) = (src.palette.as_ref().unwrap(), dst.palette.clone().unwrap());
        for r in &rows {
            let id = built.ids[&r.file];
            assert!(r.ids.contains(&id), "{} {:?} -> {id}", r.file, r.ids);
            assert!(dp.materials.contains_key(&id));
        }
        let rank = |p: &crate::terrain::Palette, id: u8| p.rank(id).unwrap();
        let dp = dp.as_ref();
        let mut ids: Vec<u8> = built.ids.values().copied().collect();
        ids.sort_by_key(|&id| rank(sp, id));
        assert!(ids.windows(2).all(|w| rank(dp, w[0]) < rank(dp, w[1])));
        let map = super::ground_map(&rows, &built);
        dst.convert_from(&src, &[], &[], &Options::default(), None, Some(&map));
        // The set has the target's header and the source's materials.
        let set_file = dst.island.material_set.clone();
        let xml = String::from_utf8(
            built
                .files
                .iter()
                .find(|f| f.0 == set_file)
                .unwrap()
                .1
                .clone(),
        )
        .unwrap();
        let header = String::from_utf8(a1800.read(&target_set).unwrap()).unwrap();
        let start = |x: &str| x[..x.find("<Material>").unwrap()].to_owned();
        assert_eq!(start(&xml), start(&header));
        assert_eq!(
            super::super::materials::set_entries(&xml).len(),
            rows.iter()
                .map(|r| &r.file)
                .collect::<std::collections::HashSet<_>>()
                .len()
        );
        // The ground shows the same materials as the source, in the same
        // shares.
        let shown = |isl: &IslandEditor| {
            let w = super::material_weights(
                isl.island.doc.materials.as_ref().unwrap(),
                &isl.palette.as_ref().unwrap().stack,
            );
            let total: f64 = w.values().sum();
            let p = isl.palette.as_ref().unwrap();
            let mut by: std::collections::BTreeMap<String, f64> = Default::default();
            for (id, v) in w {
                let name = p.materials[&id].file.rsplit('/').next().unwrap().to_owned();
                *by.entry(name).or_default() += v / total;
            }
            by
        };
        let (a, b) = (shown(&src), shown(&dst));
        for (name, share) in &a {
            let got = b.get(name).copied().unwrap_or(0.);
            assert!(
                (got - share).abs() < 0.02,
                "{name}: {share:.3} became {got:.3}"
            );
        }
        // Saved with its files, the island reads its own set and textures from
        // the mod.
        let (_config, mut project) = staged(&a1800, &mut dst);
        for (file, bytes) in built.files {
            project.stage(&file, bytes);
        }
        let again = reopen(&a1800, &mut project, &dst.path);
        assert_eq!(
            again.island.a7m().unwrap().material_set_file_name(),
            Some(&*set_file)
        );
        let ap = again.palette.as_ref().unwrap();
        assert_eq!(ap.materials.len(), dp.materials.len());
        assert!(
            ap.materials
                .values()
                .all(|m| m.file.starts_with("data/editor/anno117/"))
        );
    }
}
