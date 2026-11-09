// Converts an island between Anno 1800 and Anno 117. Heights, tint and
// river and lake curves carry over as they are. Ground materials and prop
// models are mapped by role (rock, beach, conifer, bush…) onto what the
// target province ships. Harbours and slots are placed again from the
// target's own records. The Import wizard (`import.rs`) drives it.
use super::catalog::Look;
use super::{
    IslandEditor, Prop,
    catalog::Catalog,
    doc::{self, Materials},
    harbour::{self, Piece, Resource},
    river, tint,
};
use crate::terrain::Palette;
use anno_formats::{game::Game, island, objects::PathPoint, tint::TintMap};
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

/// Path words that tie a model to one region of a game. Models carrying
/// none of the game's words are shared between its regions.
fn region_words(game: Game) -> &'static [(&'static str, &'static [&'static str])] {
    match game {
        Game::Anno117 => &[
            ("roman", &["/roman/", "roman_"]),
            ("celtic", &["/celtic/", "celtic_"]),
        ],
        Game::Anno1800 => &[
            ("moderate", &["moderate"]),
            ("colony01", &["jungle", "colony01", "south_america", "palm"]),
            ("arctic", &["arctic", "snow", "tundra"]),
            (
                "africa",
                &["africa", "desert", "enbesa", "colony02", "savanna"],
            ),
        ],
    }
}

/// 0 for the region's own models, 1 for shared ones, `None` for another
/// region's.
pub fn fit(game: Game, region: &str, path: &str) -> Option<u8> {
    let p = path.to_ascii_lowercase();
    let mut foreign = false;
    for (r, words) in region_words(game) {
        if words.iter().any(|w| p.contains(w)) {
            if r.eq_ignore_ascii_case(region) {
                return Some(0);
            }
            foreign = true;
        }
    }
    (!foreign).then_some(1)
}

/// Models placed fewer times than this over all shipped islands are mostly
/// one-off scenery or leftovers; they make poor stand-ins.
const MIN_USES: u32 = 3;
/// Target models offered per role. Source models of one role spread over
/// these, so a forest of five tree kinds stays mixed.
const CHOICES: usize = 6;

/// The target's models for `role` in `region`, best first: the region's
/// own before shared ones, then by how often shipped islands place them.
pub fn candidates(catalog: &Catalog, game: Game, region: &str, role: Role) -> Vec<String> {
    let mut found: Vec<(u8, u32, &str)> = catalog
        .props
        .iter()
        .filter(|e| e.uses >= MIN_USES && prop_role(&e.path) == Some(role))
        .filter_map(|e| Some((fit(game, region, &e.path)?, e.uses, e.path.as_str())))
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
    game: Game,
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
            let look = look_of(&e.path)?.scaled(e.scale);
            let other = prop_role(&e.path);
            let kind = match (role, other) {
                (Some(a), Some(b)) if a != b => 0.6,
                // Unnamed kinds are mostly building parts (quays, stairs).
                (Some(_), None) | (None, Some(_)) => 0.3,
                _ => 0.,
            };
            let place = match fit(game, region, &e.path) {
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
}

/// Best matches below this confidence bring the source game's own model
/// instead, when it can be ported (`port::portable`). On an 1800 jungle
/// island that brings most large jungle trees, dead trees and an
/// underwater plant whose best 117 match was a stair piece; the same palm
/// scores 99%, rocks 60-90%, bushes 35-65%.
pub const BRING_BELOW: f32 = 0.35;

/// Confidence of a match from its look distance: 1 for the same model,
/// about 0.5 for a fair stand-in, under 0.2 for a poor one.
pub fn confidence(distance: f32) -> f32 {
    (-distance).exp()
}

impl PropRow {
    /// Whether the row places the source game's own model, ported.
    pub fn brings(&self) -> bool {
        self.target()
            .is_some_and(|t| t.starts_with(super::port::FOLDER))
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

/// Every model the source island places (props and decoration objects),
/// with its instance count and median vertical scale, most placed first.
pub fn source_models(src: &IslandEditor) -> Vec<(String, usize, f32)> {
    let mut scales: HashMap<String, Vec<f32>> = HashMap::new();
    for p in src.doc.props.iter() {
        if let Some(path) = src.doc.meshes.get(p.source.file as usize) {
            scales
                .entry(path.clone())
                .or_default()
                .push(p.source.scale[1]);
        }
    }
    for o in src.doc.objects.iter() {
        if let Some(c) = &o.config {
            scales
                .entry(c.replace('\\', "/"))
                .or_default()
                .push(o.scale[1]);
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

pub fn map_props(
    models: &[(String, usize, f32)],
    catalog: &Catalog,
    (from, game): (Game, Game),
    region: &str,
    source_look: impl Fn(&str) -> Option<Look>,
    target_look: impl Fn(&str) -> Option<Look>,
) -> Vec<PropRow> {
    let mut by_role: HashMap<Role, Vec<String>> = HashMap::new();
    let mut rank: HashMap<Role, usize> = HashMap::new();
    models
        .iter()
        .map(|(source, count, placed)| {
            let role = prop_role(source);
            // By look when both games' looks are in; by name until then.
            if let Some(look) = source_look(source) {
                let alikes = look_alikes(
                    catalog,
                    game,
                    region,
                    (source, &look.scaled(*placed)),
                    &target_look,
                );
                if !alikes.is_empty() {
                    let sure = alikes[0].1;
                    let mut choices: Vec<String> = alikes.into_iter().map(|a| a.0).collect();
                    let mut scale = height_scale(Some(look), target_look(&choices[0]));
                    // Nothing alike enough: the original, ported, stands in.
                    if sure < BRING_BELOW && super::port::portable(from, game, source) {
                        choices.insert(0, super::port::target(source));
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
                            c = candidates(catalog, game, region, now);
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
            }
        })
        .collect()
}

/// What a ground material is, for mapping it to the other game's set.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Ground {
    Underwater,
    Sand,
    Gravel,
    Rock,
    Snow,
    Dry,
    Dirt,
    Grass,
}

impl Ground {
    fn fallback(self) -> Ground {
        match self {
            Ground::Snow | Ground::Gravel => Ground::Rock,
            Ground::Dry => Ground::Grass,
            Ground::Underwater => Ground::Sand,
            Ground::Dirt => Ground::Grass,
            Ground::Grass => Ground::Dirt,
            Ground::Sand => Ground::Gravel,
            Ground::Rock => Ground::Gravel,
        }
    }

    /// Which of the target's materials of this kind come first. Only 117
    /// names: they follow the World Machine template's channels, which
    /// were picked from what retail islands use. 1800 sets keep id order.
    fn preferred(self) -> &'static [&'static str] {
        match self {
            Ground::Rock => &["cliff_limestone", "celtic_cliffs_05", "rock_limestone"],
            Ground::Sand => &["beach_04", "beach_01", "sand_01"],
            Ground::Gravel => &["gravel_01", "river_pebbles", "gravel_round"],
            Ground::Underwater => &["underwatergrass", "beach_04"],
            Ground::Dirt => &["dirt_01", "mud_01"],
            Ground::Grass => &["grass_01_e", "grass_01.", "grass_01_f_dark"],
            Ground::Dry => &["grass_01_h_dry", "grass_01_c_dry"],
            Ground::Snow => &[],
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

/// A material as the mapping sees it: id, file, mean texture color.
pub type GroundMaterial = (u8, String, Option<[f32; 3]>);

/// Source material id → target material id, for every id the source uses.
/// `used` holds each source material with the ground it shows; `target`
/// the target set. Each source material takes the target material of its
/// kind (or the kind's fallback) closest in mean color, so dark tropical
/// rock does not turn into pale limestone when a darker rock exists.
/// Without a color the kind's preferred material wins.
pub fn map_materials(
    used: &[(GroundMaterial, f64)],
    target: &[GroundMaterial],
) -> BTreeMap<u8, u8> {
    // Per kind: (preferred rank, id, color), one entry per file.
    type Candidate = (usize, u8, Option<[f32; 3]>);
    let mut kinds: BTreeMap<Ground, Vec<Candidate>> = BTreeMap::new();
    let mut files = std::collections::HashSet::new();
    for (id, file, color) in target {
        if !files.insert(file.as_str()) {
            continue;
        }
        let name = stem(file);
        let g = ground_role(file);
        let rank = (g.preferred().iter())
            .position(|p| name.starts_with(p))
            .unwrap_or(usize::MAX);
        kinds.entry(g).or_default().push((rank, *id, *color));
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
    let distance = |a: Option<[f32; 3]>, b: Option<[f32; 3]>| match (a, b) {
        (Some(a), Some(b)) => (0..3).map(|c| (a[c] - b[c]).powi(2)).sum::<f32>(),
        _ => f32::MAX,
    };
    let mut out = BTreeMap::new();
    for ((id, file, color), _) in used {
        let list = candidates(ground_role(file));
        let best = list
            .iter()
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

/// Mean color of a material's preview.
fn mean_color(m: &crate::terrain::Material) -> [f32; 3] {
    let t = &m.preview;
    let mut sum = [0f64; 3];
    for px in t.rgba.as_chunks::<4>().0 {
        for c in 0..3 {
            sum[c] += f64::from(px[c]);
        }
    }
    let n = (t.rgba.len() / 4).max(1) as f64;
    sum.map(|v| (v / n) as f32)
}

/// How much ground each material id shows over a whole material layer.
fn material_weights(m: &Materials, palette: &Palette) -> BTreeMap<u8, f64> {
    let [w, h] = m.alphas.size();
    let mut out = BTreeMap::new();
    for y in 0..h {
        for x in 0..w {
            let ids = island::unpack_slots(m.ids.get(x / 2, y / 2));
            let shown = palette.visible(ids, island::unpack_slots(m.alphas.get(x, y)));
            for k in (0..6).filter(|&k| shown[k] > 0.) {
                *out.entry(ids[k]).or_insert(0.) += f64::from(shown[k]);
            }
        }
    }
    out
}

/// A texel's ground as (target id, share shown), shares summing to 1.
type Shares = Vec<(u8, f32)>;

/// Rewrites `dst` from `src` through `map` so the target shows what the
/// source showed. Weights are coverages the game lays over each other in
/// set order (`Palette::stack`), so the source is sampled the way the game
/// samples it (each material's coverage interpolated over the four nearest
/// texels, then stacked), mapped, and re-encoded for the target's order.
/// Nearest sampling made every source texel a block of target texels with
/// stair-step edges. Ground off the source (a larger target island) becomes
/// `sea`.
///
/// Every texel keeps one base material at full weight under the rest, as
/// shipped islands keep a beach: where neighbouring texels' layers only
/// partly cover, the game's bake shows its white clear color unless a base
/// lies under everything.
fn transfer_materials(
    (src, from): (&Materials, &Palette),
    (dst, to): (&mut Materials, &Palette),
    map: &BTreeMap<u8, u8>,
    offset: f32,
    src_size: f32,
    sea: u8,
) {
    let [w, h] = dst.alphas.size();
    let per = dst.per_metre;
    let [sw, sh] = src.alphas.size();
    let base = (map.values().chain([&sea]).copied())
        .min_by_key(|&id| to.rank(id).unwrap_or(u8::MAX))
        .unwrap_or(sea);
    let texel = |x: usize, y: usize| -> Shares {
        let q = [
            (x as f32 + 0.5) / per - offset,
            (y as f32 + 0.5) / per - offset,
        ];
        if !(0.0..src_size).contains(&q[0]) || !(0.0..src_size).contains(&q[1]) {
            return vec![(sea, 1.)];
        }
        let (u, v) = (q[0] * src.per_metre - 0.5, q[1] * src.per_metre - 0.5);
        let (fx, fy) = (u - u.floor(), v - v.floor());
        let mut cover: Vec<(u8, f32)> = Vec::with_capacity(24);
        for t in 0..4 {
            let (dx, dy) = (t & 1, t >> 1);
            let tx = (u.floor() as i64 + dx).clamp(0, sw as i64 - 1) as usize;
            let ty = (v.floor() as i64 + dy).clamp(0, sh as i64 - 1) as usize;
            let share = if dx == 1 { fx } else { 1. - fx } * if dy == 1 { fy } else { 1. - fy };
            let ids = island::unpack_slots(src.ids.get(tx / 2, ty / 2));
            let weights = island::unpack_slots(src.alphas.get(tx, ty));
            for k in (0..6).filter(|&k| weights[k] > 0) {
                cover.push((ids[k] & 31, f32::from(weights[k]) / 31. * share));
            }
        }
        let n = from.shares(&mut cover);
        let mut out: Shares = Vec::new();
        for &(id, shown) in &cover[..n] {
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
            dst.ids.set(bx, by, island::pack_slots(block));
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
                dst.alphas.set(x, y, island::pack_slots(packed));
            }
        }
    }
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
    pub props_dropped: usize,
    /// Source material file → target material file.
    pub materials: Vec<(String, String)>,
    pub rivers: usize,
    pub lakes: usize,
    pub harbours: usize,
    pub slots: usize,
    /// Slot kinds left out, with counts.
    pub slots_dropped: BTreeMap<String, usize>,
}

impl Report {
    pub fn summary(&self) -> String {
        let mut s = format!(
            "{} props ({} dropped), {} materials mapped, {} rivers, {} lakes, {} harbours, {} slots",
            self.props,
            self.props_dropped,
            self.materials.len(),
            self.rivers,
            self.lakes,
            self.harbours,
            self.slots
        );
        for (kind, n) in &self.slots_dropped {
            s += &format!(
                "; {} left out",
                crate::project::plural(*n, &format!("{kind} slot"), &format!("{kind} slots"))
            );
        }
        s
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
    for o in src.doc.objects.iter() {
        let Some(kind) = assets
            .and_then(|a| harbour::piece_of(a, o.guid))
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

/// Builds `src`'s island into `dst`, a blank island of the target game
/// (`IslandEditor::create` with `Start::Sea`) whose harbour records and
/// assets are loaded. `src` needs its assets for telling slots, coasts and
/// water apart. A smaller source sits centred on the target; a larger one
/// loses its rim. Leaves `dst` with a fresh history.
pub fn convert(
    src: &IslandEditor,
    dst: &mut IslandEditor,
    rows: &[PropRow],
    slots: &[SlotRow],
    opts: &Options,
) -> Report {
    let mut report = Report::default();
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
    let heights = &mut Arc::make_mut(&mut dst.doc.field).heights;
    for z in 0..g.depth {
        for x in 0..g.width {
            let [wx, wz] = g.world(x, z);
            heights[z * g.width + x] = src.doc.field.height([clamp(wx), clamp(wz)]);
        }
    }
    dst.derive_land();

    if opts.materials
        && let (Some(sp), Some(dp), Some(sm)) =
            (&src.palette, dst.palette.clone(), &src.doc.materials)
        && let Some(dm) = &mut dst.doc.materials
    {
        let used: Vec<(GroundMaterial, f64)> = material_weights(sm, sp)
            .into_iter()
            .filter_map(|(id, w)| {
                let m = sp.materials.get(&id)?;
                Some(((id, m.file.clone(), Some(mean_color(m))), w))
            })
            .collect();
        let target: Vec<GroundMaterial> = (dp.materials.iter())
            .map(|(&id, m)| (id, m.file.clone(), Some(mean_color(m))))
            .collect();
        let map = map_materials(&used, &target);
        let sea = map_materials(&[((0, "underwater".into(), None), 1.)], &target)[&0];
        transfer_materials((sm, sp), (dm, &dp), &map, offset, src_size, sea);
        for ((id, file, _), _) in &used {
            if let Some(m) = map.get(id).and_then(|t| dp.materials.get(t)) {
                report.materials.push((stem(file), stem(&m.file)));
            }
        }
    }

    if opts.tint
        && let Some(t) = &src.doc.tint
    {
        dst.doc.tint = Some(Arc::new(TintMap::from_fn(
            tint::WORK_LEVEL,
            dst.world(),
            |x, z| tint::sample(t, clamp(x), clamp(z)),
        )));
    }

    let src_assets = src.assets.as_deref();
    let piece = |guid: u32| src_assets.and_then(|a| harbour::piece_of(a, guid));
    let moved = |path: &[PathPoint]| -> Vec<PathPoint> {
        path.iter()
            .map(|p| PathPoint {
                p: to_dst(p.p),
                ..*p
            })
            .collect()
    };

    if opts.water
        && let Some(lib) = dst.harbour.clone()
    {
        for o in src.doc.objects.iter() {
            let (Some(lake), Some(path)) = (src.water_kind(o.guid), o.path.as_deref()) else {
                continue;
            };
            if !path.iter().all(|p| inside(p.p)) {
                continue;
            }
            let curve = moved(path);
            let what = if lake { Piece::Lake } else { Piece::River };
            let Some(store) = &mut dst.object_store else {
                break;
            };
            store.objects.clone_from(&dst.doc.objects);
            let areas =
                (dst.doc.grids.as_ref()).and_then(|g| g.layer(anno_formats::grids::Kind::AreaIds));
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
                continue;
            }
            dst.start_coast = p == Piece::StartCoast;
            if dst.push_harbour(moved(path)) {
                report.harbours += 1;
            }
        }
        dst.start_coast = false;
    }

    if opts.slots
        && let Some(lib) = dst.harbour.clone()
    {
        for o in src.doc.objects.iter() {
            let Some(kind) = piece(o.guid).and_then(slot_of) else {
                continue;
            };
            if !inside(o.position) {
                continue;
            }
            let target = (slots.iter())
                .find(|r| r.kind == kind)
                .and_then(|r| r.target)
                .filter(|&(r, fixed)| lib.guid(r.pieces(fixed).0).is_some());
            let Some((resource, fixed)) = target else {
                *report.slots_dropped.entry(slot_label(kind)).or_default() += 1;
                continue;
            };
            dst.resource = resource;
            dst.fixed_slots = fixed;
            if dst.push_slot(to_dst(o.position)) {
                report.slots += 1;
            }
        }
        dst.resource = Resource::Mine;
        dst.fixed_slots = false;
    }

    if opts.props {
        let target: HashMap<&str, (&str, f32)> = rows
            .iter()
            .filter_map(|r| Some((r.source.as_str(), (r.target()?, r.scale))))
            .collect();
        let place = |dst: &mut IslandEditor, path: &str, record: island::Prop| {
            let Some(&(to, scale)) = target.get(path).filter(|_| inside(record.position)) else {
                return false;
            };
            let file = dst.mesh_index(to);
            let source = island::Prop {
                file,
                position: to_dst(record.position),
                scale: record.scale.map(|v| v * scale),
                ..record
            };
            Arc::make_mut(&mut dst.doc.props).push(Prop {
                id: dst.next_id,
                matrix: doc::trs(source.position, source.rotation, source.scale),
                source,
                auto_cliff: false,
            });
            dst.next_id += 1;
            true
        };
        for p in src.doc.props.iter() {
            let Some(path) = src.doc.meshes.get(p.source.file as usize) else {
                continue;
            };
            if place(dst, path, p.source.clone()) {
                report.props += 1;
            } else {
                report.props_dropped += 1;
            }
        }
        // Decoration objects become plain props of the mapped model.
        for o in src.doc.objects.iter() {
            let Some(config) = &o.config else { continue };
            let record = island::Prop {
                file: 0,
                position: o.position,
                rotation: o.rotation,
                scale: o.scale,
                color: [1.; 4],
                adapt_terrain_height: None,
            };
            if place(dst, &config.replace('\\', "/"), record) {
                report.props += 1;
            } else {
                report.props_dropped += 1;
            }
        }
    }

    // Tree cells, forest chunks, environment and herds follow the new
    // props and terrain; buildable land follows the slope.
    dst.derive_grids(None);
    dst.history = History::new(false);
    dst.ground_stale = true;
    dst.splat_stale = true;
    dst.selection.clear();
    dst.rebuild_tiles(None);
    report
}

#[cfg(test)]
mod tests {
    use super::{Ground, Options, Role, convert, ground_role, map_materials, map_props, prop_role};
    use crate::island::test_support::{
        assets, create, harbour_library, install, install_1800, saved, staged,
    };
    use crate::island::{IslandEditor, Start, catalog::Catalog, harbour, harbour::Resource, new};
    use anno_formats::game::Game;
    use anno_formats::vfs::Install;
    use std::sync::Arc;

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
        let mid = Some([80., 90., 40.]);
        let light = Some([140., 150., 90.]);
        let rock = Some([120., 110., 100.]);
        let used = [
            ((10, "m_gras_02.xml".to_owned(), mid), 900.),
            ((19, "m_gras_05.xml".to_owned(), light), 500.),
            (
                (2, "m_mountain_03.xml".to_owned(), Some([60., 55., 50.])),
                300.,
            ),
            ((3, "m_underwater_01.xml".to_owned(), None), 200.),
            ((9, "c_snow_01.xml".to_owned(), Some([230.; 3])), 10.),
        ];
        let target = [
            (0, "mud_01.xml".to_owned(), Some([110., 100., 80.])),
            (1, "grass_01_b_light.xml".to_owned(), light),
            (4, "grass_01_e.xml".to_owned(), mid),
            (6, "cliff_limestone_01.xml".to_owned(), rock),
            (8, "cliff_limestone_01.xml".to_owned(), rock),
            (
                5,
                "rock_limestone_01.xml".to_owned(),
                Some([100., 90., 80.]),
            ),
            (7, "beach_04.xml".to_owned(), Some([136., 116., 93.])),
        ];
        let map = map_materials(&used, &target);
        // Each takes the closest color of its kind.
        assert_eq!(map[&10], 4);
        assert_eq!(map[&19], 1);
        assert_eq!(map[&2], 5);
        // No sea-floor material in the set: sand stands in; snow → the
        // lightest rock, listed once however often the set repeats it.
        assert_eq!(map[&3], 7);
        assert_eq!(map[&9], 6);
    }

    /// `path` from `src` converted into a new island of `region` on `dst`.
    fn run(
        src_install: &Install,
        dst_install: &Install,
        path: &str,
        region: &str,
        remap: impl Fn(&mut Vec<super::SlotRow>),
    ) -> (IslandEditor, IslandEditor, super::Report) {
        let src_assets = assets(src_install);
        let mut src = IslandEditor::load(src_install, path).unwrap();
        src.assets = Some(src_assets.clone());
        src.harbour = harbour::Library::load(src_install, &src_assets, &src.region())
            .ok()
            .map(Arc::new);
        let size = src.world()[0];
        let dst_assets = assets(dst_install);
        let skeletons = new::skeletons(dst_install, Some(&dst_assets));
        let skeleton = super::pick_size(&skeletons, region, size).unwrap().clone();
        let mut dst = create(dst_install, skeleton, Start::Sea, "Converted");
        dst.harbour = Some(harbour_library(dst_install, &dst_assets, &dst));
        dst.assets = Some(dst_assets);
        let catalog = Catalog::build(dst_install).unwrap();
        let rows = map_props(
            &super::source_models(&src),
            &catalog,
            (src_install.game(), dst_install.game()),
            region,
            |_| None,
            |_| None,
        );
        let mut slots = super::map_slots(&src, dst.harbour.as_deref().unwrap());
        remap(&mut slots);
        let report = convert(&src, &mut dst, &rows, &slots, &Options::default());
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
                src.doc.field.height(q),
                dst.doc.field.height([q[0] + offset, q[1] + offset]),
            );
            assert!((a - b).abs() < 0.05, "height {a} became {b}");
        }
        assert!(
            report.props * 10 >= src.doc.props.len() * 7,
            "{}",
            report.summary()
        );
        for p in dst.doc.props.iter() {
            let mesh = &dst.doc.meshes[p.source.file as usize];
            assert!(dst_install.file(mesh).is_some(), "{mesh} not shipped");
            assert!(
                super::fit(dst_install.game(), region, mesh).is_some(),
                "{mesh} is another region’s"
            );
        }
        // Land ground maps to land materials, deep water to sea floor or sand.
        let palette = dst.palette.as_ref().unwrap();
        let m = dst.doc.materials.as_ref().unwrap();
        let kind =
            |p: [f32; 2]| super::ground_role(&palette.materials[&m.shown(p, Some(palette))].file);
        let mid = dst.world()[0] as f32 / 2.;
        let deep = [offset + 2., offset + 2.];
        assert!(dst.doc.field.height(deep) < -2.);
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
            .filter(|&p| dst.doc.field.height(p) > 3.)
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
            .doc
            .objects
            .iter()
            .filter(|o| src.water_kind(o.guid).is_some())
            .count();
        assert_eq!(report.rivers + report.lakes, src_water);
        // Saves and reads back.
        let (_config, project) = staged(dst_install, &mut dst);
        let island = saved(&project, &dst.path);
        assert_eq!(island.props().len(), dst.doc.props.len());
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
        let (src, dst, report) = run(&a1800, &a117, &path, "roman", |rows| {
            let row = rows.iter_mut().find(|r| r.kind == clay).unwrap();
            assert_eq!(row.target, None);
            row.target = Some((Resource::Mine, false));
        });
        assert!(!report.slots_dropped.contains_key(&super::slot_label(clay)));
        assert_eq!(report.slots_dropped.get("Oil"), Some(&9));
        assert_eq!(report.slots, 8 + 6, "{}", report.summary());
        assert!(report.rivers > 0, "{}", report.summary());
        // The river's surface stands over its bed, as it did in 1800.
        for o in (dst.doc.objects.iter()).filter(|o| dst.water_kind(o.guid) == Some(false)) {
            let points = o.path.as_deref().unwrap();
            let wet = (points.iter())
                .filter(|p| p.p[1] > dst.doc.field.height([p.p[0], p.p[2]]))
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
    fn jungle_props_match_by_look() {
        use crate::island::catalog::Thumbs;
        let (Some(a1800), Some(a117)) = (install_1800(), install()) else {
            return;
        };
        let path = a1800
            .search("colony01_l_06_river_01.a7m", 10)
            .into_iter()
            .map(|id| a1800.file_path(id).to_owned())
            .find(|p| p.ends_with(".a7m"))
            .unwrap();
        let src = IslandEditor::load(&a1800, &path).unwrap();
        let models = super::source_models(&src);
        let catalog = Catalog::build(&a117).unwrap();
        let (sd, td) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let mut source = Thumbs::start(Arc::new(a1800), sd.path().to_owned());
        let mut target = Thumbs::start(Arc::new(a117), td.path().to_owned());
        source.request(models.iter().map(|m| m.0.as_str()));
        target.request(super::match_pool(&catalog));
        while source.pending() || target.pending() {
            std::thread::sleep(std::time::Duration::from_millis(20));
            source.receive();
            target.receive();
        }
        let rows = map_props(
            &models,
            &catalog,
            (Game::Anno1800, Game::Anno117),
            "roman",
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

    #[test]
    fn converts_a_117_island_into_1800() {
        let (Some(a1800), Some(a117)) = (install_1800(), install()) else {
            return;
        };
        let path = "data/base/provinces/roman/islands/pool/roman_island_medium_01/roman_island_medium_01.a7m";
        let (src, dst, report) = run(&a117, &a1800, path, "moderate", |_| {});
        check(&src, dst, &report, "moderate", &a1800);
        assert!(report.rivers > 0, "{}", report.summary());
    }
}
