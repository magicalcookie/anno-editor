// Owns a map layout as the map editor edits it (`Map`), what a game's maps
// are as data (`MapRules`), the capability a game implements to read, write
// and register them (`Maps`), and the layout checks every game shares.
// The variant keeps the file it was read from, and a game's `write`
// re-parses it and applies the slots, so an unedited map writes the bytes it
// was read from.

use crate::game::{Refs, SizeClass};
use crate::island::{Level, Names, Problem};
use crate::project::{MapEntry, MapIsland, MapPlan};
use anno_formats::a7m::template::{MapTemplate, Mines, SlotKind};
use anno_formats::assets::Assets;
use anno_formats::vfs::Install;
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// A map layout: the islands it places, per setup option.
#[derive(Clone, Debug, PartialEq)]
pub struct Map {
    pub name: String,
    /// The region its templates are listed under (117's `Roman`, 1800's
    /// `Moderate`); empty for 2070, whose worlds have none.
    pub region: String,
    /// The first variant's `size` and `playable`, the one the editor opens
    /// on. Each variant has its own: 1800's world sizes differ.
    pub size: [u32; 2],
    pub playable: [i32; 4],
    /// Island paths the slots name, each once; `Slot::island` indexes it.
    pub islands: Vec<String>,
    /// 117 and 1800: one per setup option the layout answers; 2070: one.
    pub variants: Vec<Variant>,
}

impl Map {
    /// Slots per variant. Shipped templates have tens, shipped worlds 27 on
    /// average (1,515 entries in 57). A bound so `u16` slot indices and the
    /// overlap sweep stay small.
    pub const MAX_SLOTS: usize = 512;

    /// The index of island `path` in `islands`, added when new.
    pub fn island(&mut self, path: &str) -> u16 {
        match self.islands.iter().position(|p| p == path) {
            Some(n) => n as u16,
            None => {
                self.islands.push(path.to_owned());
                (self.islands.len() - 1) as u16
            }
        }
    }

    /// The island a slot holds; `None` for a pool slot.
    pub fn island_of(&self, slot: &Slot) -> Option<&str> {
        (slot.island != Slot::POOL).then(|| self.islands[slot.island as usize].as_str())
    }

    /// The map as a new layout called `name` on the same sea: no slots or
    /// islands, one variant per `setups` (a setup the map lacks copies its
    /// first variant's file). Free start points stay: they belong to the
    /// sea's players, not to its islands.
    pub fn emptied(mut self, name: &str, setups: &[Setup]) -> Result<Map> {
        let first = self
            .variants
            .first()
            .context("a map without variants")?
            .clone();
        for setup in setups {
            if !self.variants.iter().any(|v| v.setup == *setup) {
                self.variants.push(Variant {
                    setup: *setup,
                    ..first.clone()
                });
            }
        }
        for v in &mut self.variants {
            v.slots.clear();
        }
        self.islands.clear();
        self.name = name.to_owned();
        Ok(self)
    }

    /// Whether `other` places the same islands the same way: names, sizes,
    /// slots and starts, ignoring the files the variants were read from and
    /// which of their elements a slot came from. A saved and reopened map
    /// equals the one saved in this sense.
    pub fn same_layout(&self, other: &Map) -> bool {
        let slots = |m: &Map, v: &Variant| -> Vec<(Slot, Option<String>)> {
            (v.slots.iter())
                .map(|s| {
                    let island = m.island_of(s).map(str::to_owned);
                    (Slot { source: 0, ..*s }, island)
                })
                .collect()
        };
        self.name == other.name
            && self.region == other.region
            && self.size == other.size
            && self.playable == other.playable
            && self.variants.len() == other.variants.len()
            && (self.variants.iter().zip(&other.variants)).all(|(a, b)| {
                a.setup == b.setup
                    && a.size == b.size
                    && a.playable == b.playable
                    && a.starts == b.starts
                    && a.replaces == b.replaces
                    && slots(self, a) == slots(other, b)
            })
    }
}

/// One file of a layout: the slots it places for one setup option.
#[derive(Clone, Debug, PartialEq)]
pub struct Variant {
    pub setup: Setup,
    /// Metres, fixed by the base session: resizing would mean writing the
    /// `.a7t`, which the editor only copies.
    pub size: [u32; 2],
    /// `x0 y0 x1 y1` in metres; the whole map where the file states none.
    pub playable: [i32; 4],
    pub slots: Vec<Slot>,
    /// Start points apart from slots (2070's `ArkSlots`), metres; empty
    /// elsewhere.
    pub starts: Vec<[i32; 2]>,
    /// The file the variant was read from, unchanged. Writing re-parses it
    /// and applies the slots, so whatever the model doesn't hold (per-slot
    /// difficulty, fertilities, 2070's objects and areas) is carried.
    pub source: Arc<[u8]>,
    /// The shipped stem (virtual path without extension) whose `.a7t` and
    /// `.a7te` the variant copies; 2070: the world it was read from.
    pub base: String,
    /// 1800: the shipped `MapTemplate` assets the variant plays as, whose
    /// templates the project redirects to it. Empty elsewhere.
    pub replaces: Vec<u32>,
}

/// Which setup option a variant answers: the setup screen's island size
/// (117, 1800) and world size (1800). Empty where the file answers every
/// one (1800's New World island sizes) or none (campaign maps, 2070).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Setup {
    pub island_size: &'static str,
    pub world_size: &'static str,
}

impl Setup {
    pub const NONE: Setup = Setup {
        island_size: "",
        world_size: "",
    };
}

/// One island position.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Slot {
    /// Min corner, metres. 2070 ships one negative position.
    pub at: [i32; 2],
    /// Index into `Map::islands`, or `POOL` for a slot the game fills.
    pub island: u16,
    pub role: Role,
    /// Index into `IslandRules::size_classes`. A pool slot's footprint is
    /// its class's `slot` side; a fixed slot's is its island's own side.
    pub class: u8,
    /// Quarter turns of a fixed island (`Rotation90`, 2070's `Direction`).
    pub turns: u8,
    /// The game's type id as read, kept for ids `Role` can't name; what
    /// `write` writes for a slot of role `Other`.
    pub raw_type: u16,
    /// The element (117/1800) or world entry (2070) of the variant's
    /// `source` the slot came from; `NEW` for an added slot.
    pub source: u16,
}

impl Slot {
    pub const POOL: u16 = u16::MAX;
    pub const NEW: u16 = u16::MAX;

    pub fn fixed(&self) -> bool {
        self.island != Self::POOL
    }
}

/// What a slot is for. Each game names the ones it has, with its own type
/// ids, in `MapRules::roles`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum Role {
    #[default]
    Normal,
    Start,
    ThirdParty,
    Pirate,
    Volcano,
    Decoration,
    /// 2070.
    Underwater,
    /// 2070.
    ThirdPartyUnderwater,
    /// A position that places no island: 117's and 1800's element type 2.
    Marker,
    /// A type id the game's `MapRules::roles` doesn't name; `Slot::raw_type`
    /// keeps it.
    Other,
}

impl Role {
    pub fn label(self) -> &'static str {
        match self {
            Role::Normal => "Normal",
            Role::Start => "Start",
            Role::ThirdParty => "Third party",
            Role::Pirate => "Pirate",
            Role::Volcano => "Volcano",
            Role::Decoration => "Decoration",
            Role::Underwater => "Underwater",
            Role::ThirdPartyUnderwater => "Third party (underwater)",
            Role::Marker => "Marker",
            Role::Other => "Other",
        }
    }
}

/// What the editor and the checks need of a game's maps.
pub struct MapRules {
    /// The setup options a layout answers, one variant each, the first the
    /// one the editor opens on.
    pub setups: &'static [Setup],
    /// The game's type id per role it has; roles not listed aren't offered.
    pub roles: &'static [(Role, u16)],
    /// The game's size classes, `IslandRules::size_classes`.
    pub size_classes: &'static [SizeClass],
    /// Position snap, metres.
    pub grid: u32,
    /// Fixed islands may turn.
    pub fixed_turns: bool,
    /// Slots the game fills from its pool exist (not 2070's).
    pub pool_slots: bool,
    /// Start points apart from slots exist (2070's arks).
    pub free_starts: bool,
    /// Starts per variant, slots and free starts together: the fewest any
    /// shipped map has.
    pub min_starts: u8,
    /// Metres between two footprints, and between a footprint and the map
    /// edge: the least any shipped map keeps. They stand in for harbour
    /// access and ship paths, which no data here can check.
    pub min_gap: i32,
    pub min_edge: i32,
    /// New layouts go to `{0}{region}{1}/{name}/{name}.a7t`, or the one
    /// `.www` for 2070 (`{0}{name}.www`, the region empty).
    pub layout_folder: [&'static str; 2],
    pub registers: Registers,
}

impl MapRules {
    /// The type id the game writes for `role`.
    pub fn raw_type(&self, role: Role) -> Option<u16> {
        self.roles
            .iter()
            .find(|(r, _)| *r == role)
            .map(|(_, id)| *id)
    }

    /// The role of type id `raw`; `Other` for one the game doesn't name.
    pub fn role(&self, raw: u16) -> Role {
        (self.roles.iter())
            .find(|(_, id)| *id == raw)
            .map_or(Role::Other, |(r, _)| *r)
    }

    /// The class an island of `side` metres needs: the smallest whose slot
    /// holds it, else the largest.
    pub fn class_for(&self, side: u32) -> u8 {
        (self.size_classes.iter().position(|c| side <= c.slot))
            .unwrap_or(self.size_classes.len().saturating_sub(1)) as u8
    }

    /// The side of a slot's footprint: its island's own where `islands`
    /// knows it, else its class's slot.
    fn side(&self, slot: &Slot, facts: Option<IslandFacts>) -> i32 {
        let class = self
            .size_classes
            .get(slot.class as usize)
            .map_or(0, |c| c.slot);
        facts.map_or(class, |f| f.side) as i32
    }
}

/// How the game comes to offer a project's layout.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Registers {
    /// A map type of its own in the setup list, one `MapTemplate` asset per
    /// variant (117).
    NewMapType,
    /// It takes over chosen shipped map assets: the map type is an engine
    /// enum a mod can't extend (1800).
    ReplacesShipped,
    /// Nothing registers it yet; Make test map builds on it (2070).
    None,
}

impl Registers {
    /// The project GUIDs a layout of `variants` variants holds: 117's map
    /// type and one template asset each.
    pub fn guids(self, variants: usize) -> usize {
        match self {
            Registers::NewMapType => 1 + variants,
            Registers::ReplacesShipped | Registers::None => 0,
        }
    }
}

/// A shipped layout, as Open shipped map and New map list it.
#[derive(Clone, Debug, PartialEq)]
pub struct ShippedMap {
    /// What `Maps::read` opens: one variant's `.a7t`, or the `.www`.
    pub path: String,
    /// The file stem without the variant suffix (`roman_province_default_01`).
    pub name: String,
    pub region: String,
    /// What the game groups it under: 117's `pool`, `campaign`, `events`;
    /// 2070's `scenarios`, `campaign`, `testworlds`.
    pub group: String,
    pub size: [u32; 2],
    /// The setup options its variants answer.
    pub setups: Vec<Setup>,
    /// A new layout may start on its sea: 117's pool templates, 1800's
    /// generator maps, every 2070 world.
    pub base: bool,
}

/// A project's layout, as its settings keep it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LayoutEntry {
    pub name: String,
    /// The first variant's `.a7t`, or the `.www`.
    pub path: String,
    pub region: String,
    /// Per variant: its `.a7t`, island size and, on 1800, the shipped
    /// assets it replaces.
    pub variants: Vec<MapEntry>,
    /// The project GUIDs it holds once allocated (`Registers::guids`): 117's
    /// map type, then one per variant. They never move.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub guids: Vec<u32>,
    /// Test in game fills its pool slots with the project's islands.
    #[serde(default)]
    pub fill_with_islands: bool,
}

/// A shipped map asset a variant may play as (1800's `MapTemplate`s).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlaysAs {
    pub guid: u32,
    /// "Archipelago · moderate_archipel_ll_01".
    pub label: String,
}

/// What the checks need of an island a fixed slot names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IslandFacts {
    /// Metres.
    pub side: u32,
    pub regular: bool,
    pub start: bool,
    /// Lies wholly under the sea (2070).
    pub underwater: bool,
    /// The island's last save listed level-5 problems.
    pub broken: bool,
}

/// A game's maps: reading, writing and registering layouts, and Test in
/// game's test maps. Reached through `Islands::maps`.
pub trait Maps: Sync {
    fn rules(&self) -> &'static MapRules;

    /// Shipped layouts Open shipped map and New map list.
    fn shipped(&self, _install: &Install, _assets: Option<&Assets>) -> Vec<ShippedMap> {
        Vec::new()
    }
    /// A shipped or project map with every variant: 117 reads the
    /// `_easy/_medium/_hard` siblings, 1800 the assets sharing a file stem,
    /// 2070 one world.
    fn read(&self, _install: &Install, path: &str) -> Result<Map> {
        bail!("{path}: this game's maps can't be read yet")
    }
    /// Every file `map` needs, by virtual path, its stem `folder_name`:
    /// 117/1800 each variant's `.a7tinfo` written, `.a7t` and `.a7te` copied
    /// from its base; 2070 the one `.www`.
    fn write(
        &self,
        _install: &Install,
        map: &Map,
        _folder_name: &str,
    ) -> Result<Vec<(String, Vec<u8>)>> {
        bail!("{}: this game's maps can't be written yet", map.name)
    }
    /// An empty layout on `base`'s sea (a `ShippedMap::path`): no slots,
    /// the base's size and playable area, one variant per
    /// `MapRules::setups`.
    fn new_map(&self, _install: &Install, base: &str, _name: &str) -> Result<Map> {
        bail!("{base}: this game can't make new maps yet")
    }
    /// The shipped map assets `variant` of `map` may play as, for
    /// `Variant::replaces`; empty where layouts replace no shipped map
    /// (`Registers::ReplacesShipped` games list them).
    fn plays_as(&self, _assets: &Assets, _map: &Map, _variant: &Variant) -> Vec<PlaysAs> {
        Vec::new()
    }
    /// The asset ModOps that make the game offer `layouts` and `tests` (the
    /// test maps, numbered from `test_guid`), without `<ModOps>`.
    fn register(&self, _layouts: &[LayoutEntry], _tests: &[MapEntry], _test_guid: u32) -> String {
        String::new()
    }
    /// What stops `map` leaving the editor (level 5), and what it should
    /// warn of. By default [`check`].
    fn validate(
        &self,
        map: &Map,
        refs: &Refs,
        islands: &dyn Fn(&str) -> Option<IslandFacts>,
    ) -> Vec<Problem> {
        check(self.rules(), map, refs, islands)
    }

    /// Whether `plan` reads the island files themselves, so the caller
    /// fills [`MapIsland::file`].
    fn reads_islands(&self) -> bool {
        false
    }
    /// Every test map for `islands` (the project's, in its order), without
    /// touching the project. `assets` resolves assigned slot resources.
    /// `on`, a project layout, is filled instead of the game's default
    /// base.
    fn plan(
        &self,
        _install: &Install,
        _assets: Option<&Assets>,
        _islands: &[MapIsland],
        _mod_id: &str,
        _on: Option<&Map>,
    ) -> Result<Vec<MapPlan>> {
        bail!("this game has no test maps")
    }
}

/// A slot's footprint, `x0 z0 x1 z1` in metres, and its index.
type Footprint = (i32, i32, i32, i32, usize);

/// The footprints of `variant`'s slots, markers left out.
fn footprints(
    rules: &MapRules,
    map: &Map,
    variant: &Variant,
    islands: &dyn Fn(&str) -> Option<IslandFacts>,
) -> Vec<Footprint> {
    (variant.slots.iter().enumerate())
        .filter(|(_, s)| s.role != Role::Marker)
        .map(|(n, slot)| {
            let side = rules.side(slot, map.island_of(slot).and_then(islands));
            let [x0, z0] = slot.at;
            (x0, z0, x0 + side, z0 + side, n)
        })
        .collect()
}

// How far apart two footprints are: the larger of the x and z gaps, so
// below `min_gap` they are closer than it on both axes.
fn gap(a: &Footprint, b: &Footprint) -> i32 {
    let dx = (b.0 - a.2).max(a.0 - b.2);
    let dz = (b.1 - a.3).max(a.1 - b.3);
    dx.max(dz)
}

/// What [`check`] compares with `min_starts`, `min_gap` and `min_edge`,
/// measured on one map: the fewest starts of a variant, the least gap
/// between two footprints and the least distance from a footprint to the
/// map edge. The census tests pin each game's rules from these over its
/// shipped maps.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Measure {
    pub starts: usize,
    pub gap: i32,
    pub edge: i32,
}

pub fn measure(
    rules: &MapRules,
    map: &Map,
    islands: &dyn Fn(&str) -> Option<IslandFacts>,
) -> Measure {
    let mut m = Measure {
        starts: usize::MAX,
        gap: i32::MAX,
        edge: i32::MAX,
    };
    for variant in &map.variants {
        let [w, h] = variant.size.map(|s| s as i32);
        let starts = variant.starts.len()
            + (variant.slots.iter())
                .filter(|s| s.role == Role::Start)
                .count();
        m.starts = m.starts.min(starts);
        let boxes = footprints(rules, map, variant, islands);
        for (i, a) in boxes.iter().enumerate() {
            m.edge = m.edge.min(a.0.min(a.1).min(w - a.2).min(h - a.3));
            for b in &boxes[i + 1..] {
                m.gap = m.gap.min(gap(a, b));
            }
        }
    }
    m
}

/// Slot `slot` of variant `variant` as a [`Problem`] item.
pub fn slot_item(variant: usize, slot: usize) -> u64 {
    ((variant as u64) << 16) | slot as u64
}

/// The `(variant, slot)` a [`slot_item`] names.
pub fn item_slot(item: u64) -> (usize, usize) {
    ((item >> 16) as usize, (item & 0xffff) as usize)
}

/// `map`'s problems under `rules`: level 5 ([`Level::Broken`]) for what
/// stops it leaving the editor, level 4 for a setup option no variant
/// answers, which only warns until an in-game check says whether the setup
/// screen copes. Each problem's `names` says what its items are: slots
/// ([`slot_item`]), variants by index, or the setups the whole map lacks.
pub fn check(
    rules: &MapRules,
    map: &Map,
    refs: &Refs,
    islands: &dyn Fn(&str) -> Option<IslandFacts>,
) -> Vec<Problem> {
    // One list of items per row of the design's table, in its order.
    const ROWS: [(&str, Level, Names); 10] = [
        ("Slots outside the map", Level::Broken, Names::Slots),
        (
            "Slots outside the playable area",
            Level::Broken,
            Names::Slots,
        ),
        ("Slots too close to another", Level::Broken, Names::Slots),
        (
            "Slots too close to the map edge",
            Level::Broken,
            Names::Slots,
        ),
        (
            "Islands too big for their slot",
            Level::Broken,
            Names::Slots,
        ),
        ("Islands the install lacks", Level::Broken, Names::Slots),
        (
            "Islands whose last save is broken",
            Level::Broken,
            Names::Slots,
        ),
        (
            "Islands that can't play their slot's role",
            Level::Broken,
            Names::Slots,
        ),
        (
            "Variants with too few starts",
            Level::Broken,
            Names::Variants,
        ),
        (
            "Setup options no variant answers",
            Level::Gameplay,
            Names::Setups,
        ),
    ];
    let mut items: [Vec<u64>; 10] = Default::default();
    for (v, variant) in map.variants.iter().enumerate() {
        let [w, h] = variant.size.map(|s| s as i32);
        let [px0, pz0, px1, pz1] = variant.playable;
        let item = |s: usize| slot_item(v, s);
        let mut starts = variant.starts.len();
        for (n, slot) in variant.slots.iter().enumerate() {
            starts += usize::from(slot.role == Role::Start);
            let Some(path) = map.island_of(slot) else {
                continue;
            };
            if !refs.has(path) {
                items[5].push(item(n));
            }
            let Some(f) = islands(path) else { continue };
            if f.broken {
                items[6].push(item(n));
            }
            if slot.class < rules.class_for(f.side) {
                items[4].push(item(n));
            }
            let under = matches!(slot.role, Role::Underwater | Role::ThirdPartyUnderwater);
            // Not Normal: a shipped 2070 science scenario places
            // underwater islands in regular entries.
            let surface = matches!(slot.role, Role::Start | Role::ThirdParty);
            if (slot.role == Role::Start && !f.start)
                || (under && !f.underwater)
                || (surface && f.underwater)
            {
                items[7].push(item(n));
            }
        }
        let mut boxes = footprints(rules, map, variant, islands);
        for &(x0, z0, x1, z1, n) in &boxes {
            // Off the map says it all; on it, off the playable area and
            // near the edge are told apart.
            if x0 < 0 || z0 < 0 || x1 > w || z1 > h {
                items[0].push(item(n));
                continue;
            }
            if x0.min(z0).min(w - x1).min(h - z1) < rules.min_edge {
                items[3].push(item(n));
            }
            if x0 < px0 || z0 < pz0 || x1 > px1 || z1 > pz1 {
                items[1].push(item(n));
            }
        }
        // Sort and sweep on x: n ≤ `Map::MAX_SLOTS`, and only boxes whose x
        // spans come within the gap are compared.
        boxes.sort_unstable_by_key(|b| b.0);
        let mut close = Vec::new();
        for (i, a) in boxes.iter().enumerate() {
            for b in &boxes[i + 1..] {
                if b.0 - a.2 >= rules.min_gap {
                    break;
                }
                if gap(a, b) < rules.min_gap {
                    close.extend([a.4, b.4]);
                }
            }
        }
        close.sort_unstable();
        close.dedup();
        items[2].extend(close.into_iter().map(item));
        if starts < usize::from(rules.min_starts) {
            items[8].push(v as u64);
        }
    }
    for (n, setup) in rules.setups.iter().enumerate() {
        if !map.variants.iter().any(|v| v.setup == *setup) {
            items[9].push(n as u64);
        }
    }
    (ROWS.iter().zip(items))
        .filter(|(_, items)| !items.is_empty())
        .map(|(&(what, level, names), mut items)| {
            items.sort_unstable();
            Problem {
                level,
                what: what.to_owned(),
                count: items.len(),
                items,
                names,
            }
        })
        .collect()
}

// Test in game's slot filling, which a layout's pool slots share.

#[derive(Clone, Debug, PartialEq)]
pub enum Fill {
    /// One of the project's islands, by index into the planned islands.
    Ours(usize),
    /// Left to the vanilla pool.
    Pool,
    /// A vanilla fixed island or a non-island element.
    Vanilla,
}

/// An island the test map may use.
#[derive(Clone, Copy)]
pub struct Candidate {
    /// Index into the planned islands.
    pub island: usize,
    pub class: u16,
    pub regular: bool,
    pub start: bool,
}

/// Which island each slot gets, by `(class, Some(starter))` for a random
/// Normal or Starter slot: the largest island of the matching role that
/// fits the slot class, rotating through ties so several islands share the
/// map. Starter slots only take classes from `starter_class` up. Slots
/// without a role (decoration, third parties, volcanoes) stay vanilla.
pub fn assign(
    slots: &[(u16, Option<bool>)],
    islands: &[Candidate],
    starter_class: u16,
) -> Vec<Option<usize>> {
    let classes = (islands.iter())
        .map(|c| c.class as usize + 1)
        .max()
        .unwrap_or(0);
    let mut turn = vec![[0usize; 2]; classes];
    slots
        .iter()
        .map(|&(class, starter)| {
            let start = starter?;
            let min = if start { starter_class } else { 0 };
            let fits = |c: &&Candidate| {
                (if start { c.start } else { c.regular }) && c.class >= min && c.class <= class
            };
            let best = islands.iter().filter(fits).map(|c| c.class).max()?;
            let ties: Vec<usize> = islands
                .iter()
                .filter(fits)
                .filter(|c| c.class == best)
                .map(|c| c.island)
                .collect();
            let turn = &mut turn[best as usize][start as usize];
            let pick = ties[*turn % ties.len()];
            *turn += 1;
            Some(pick)
        })
        .collect()
}

// 117's and 1800's maps: their `.a7tinfo` is one codec.

/// The map called `name` in `region` whose variants are `(stem, setup,
/// replaces)`, each read from `{stem}.a7tinfo` in `install`, its `.a7t` and
/// `.a7te` beside it.
pub fn read_templates(
    install: &Install,
    rules: &MapRules,
    name: &str,
    region: &str,
    variants: Vec<(String, Setup, Vec<u32>)>,
) -> Result<Map> {
    let mut map = Map {
        name: name.to_owned(),
        region: region.to_owned(),
        size: [0; 2],
        playable: [0; 4],
        islands: Vec::new(),
        variants: Vec::with_capacity(variants.len()),
    };
    let side = |path: &str| -> Option<u32> {
        let info = install.read(format!("{path}info")).ok()?;
        Some(
            anno_formats::a7m::a7minfo::A7mInfo::parse(&info)
                .ok()?
                .map_size[0],
        )
    };
    for (stem, setup, replaces) in variants {
        let path = format!("{stem}.a7tinfo");
        let bytes: Arc<[u8]> = install
            .read(&path)
            .with_context(|| format!("Reading {path}"))?
            .into();
        let t = MapTemplate::parse(&bytes).with_context(|| format!("Reading {path}"))?;
        ensure!(
            t.slots.len() <= Map::MAX_SLOTS,
            "{path}: {} slots",
            t.slots.len()
        );
        let [w, h] = t.size;
        let playable = t.playable.unwrap_or([0, 0, w, h]).map(|v| v as i32);
        if map.variants.is_empty() {
            (map.size, map.playable) = (t.size, playable);
        }
        let slots = template_slots(rules, &mut map, &t, &side);
        map.variants.push(Variant {
            setup,
            size: t.size,
            playable,
            slots,
            starts: Vec::new(),
            source: bytes,
            base: stem,
            replaces,
        });
    }
    ensure!(!map.variants.is_empty(), "{name}: no template found");
    Ok(map)
}

/// The slots of template `t`, adding its fixed islands to `map`. `side`
/// gives a fixed island's side for its class; an island it can't measure
/// takes the largest.
pub fn template_slots(
    rules: &MapRules,
    map: &mut Map,
    t: &MapTemplate,
    side: &dyn Fn(&str) -> Option<u32>,
) -> Vec<Slot> {
    let largest = rules.size_classes.len().saturating_sub(1) as u8;
    (t.slots.iter().enumerate())
        .map(|(n, s)| {
            let at = s.position.map(|v| v as i32);
            let source = n as u16;
            match &s.kind {
                SlotKind::Random { size, island_type } => Slot {
                    at,
                    island: Slot::POOL,
                    role: rules.role(*island_type),
                    class: (*size).min(u16::from(u8::MAX)) as u8,
                    turns: 0,
                    raw_type: *island_type,
                    source,
                },
                SlotKind::Fixed { path, island_type } => Slot {
                    at,
                    island: map.island(path),
                    role: rules.role(*island_type),
                    class: side(path).map_or(largest, |side| rules.class_for(side)),
                    turns: s.turns,
                    raw_type: *island_type,
                    source,
                },
                SlotKind::Other(kind) => Slot {
                    at,
                    island: Slot::POOL,
                    role: Role::Marker,
                    class: 0,
                    turns: 0,
                    raw_type: *kind as u16,
                    source,
                },
            }
        })
        .collect()
}

/// The template `variant` of `map` writes: its source with the slots
/// applied. Slots keep the element they came from, elements no slot names
/// go, and new slots append elements shaped as shipped ones; a slot made
/// fixed takes its island as `fix` writes it, its mine slots as `mines`
/// says (left to the generator).
pub fn template_of(
    rules: &MapRules,
    map: &Map,
    variant: &Variant,
    mines: Mines,
) -> Result<MapTemplate> {
    ensure!(
        variant.slots.len() <= Map::MAX_SLOTS,
        "{}: {} slots, more than {}",
        map.name,
        variant.slots.len(),
        Map::MAX_SLOTS
    );
    let mut t = MapTemplate::parse(&variant.source)?;
    let playable = variant.playable.map(|v| v.max(0) as u32);
    if t.playable.is_some_and(|was| was != playable) {
        t.set_playable(playable)?;
    }
    let mut kept = vec![0u64; t.slots.len().div_ceil(64)];
    for slot in variant.slots.iter().filter(|s| s.source != Slot::NEW) {
        let n = slot.source as usize;
        ensure!(
            n < t.slots.len() && kept[n / 64] >> (n % 64) & 1 == 0,
            "{}: slot source {n} is not an element of the template, or named twice",
            map.name
        );
        kept[n / 64] |= 1 << (n % 64);
        apply_slot(rules, map, &mut t, n, slot, mines)?;
    }
    // Backwards, so the indices still to remove stay put.
    for n in (0..t.slots.len()).rev() {
        if kept[n / 64] >> (n % 64) & 1 == 0 {
            t.remove(n)?;
        }
    }
    for slot in variant.slots.iter().filter(|s| s.source == Slot::NEW) {
        let at = corner(map, slot)?;
        let n = match slot.role {
            Role::Marker => t.add_marker(at)?,
            _ => t.add_random(at, slot.class.into(), raw_type(rules, slot))?,
        };
        apply_slot(rules, map, &mut t, n, slot, mines)?;
    }
    Ok(t)
}

/// Every file `map` needs: per variant, at `stem(variant)`, its
/// `.a7tinfo` written and its base's `.a7t` copied, and its `.a7te` where
/// the base has one (117's Egypt templates have none).
pub fn write_templates(
    install: &Install,
    rules: &MapRules,
    map: &Map,
    stem: impl Fn(&Variant) -> String,
    mines: Mines,
) -> Result<Vec<(String, Vec<u8>)>> {
    let mut files = Vec::with_capacity(3 * map.variants.len());
    for variant in &map.variants {
        let stem = stem(variant);
        let t = template_of(rules, map, variant, mines)?;
        files.push((format!("{stem}.a7tinfo"), t.encode()));
        for ext in ["a7t", "a7te"] {
            let from = format!("{}.{ext}", variant.base);
            if ext == "a7te" && install.file(&from).is_none() {
                continue;
            }
            let bytes = install
                .read(&from)
                .with_context(|| format!("Reading {from}"))?;
            files.push((format!("{stem}.{ext}"), bytes));
        }
    }
    Ok(files)
}

// Roles the game names write their own id; the rest what was read.
fn raw_type(rules: &MapRules, slot: &Slot) -> u16 {
    match slot.role {
        Role::Other | Role::Marker => slot.raw_type,
        role => rules.raw_type(role).unwrap_or(slot.raw_type),
    }
}

fn corner(map: &Map, slot: &Slot) -> Result<[u32; 2]> {
    let [x, z] = slot.at;
    ensure!(
        x >= 0 && z >= 0,
        "{}: a slot at {x}, {z}, off the map",
        map.name
    );
    Ok([x as u32, z as u32])
}

// Makes element `n` of `t` what `slot` says, changing only what differs.
fn apply_slot(
    rules: &MapRules,
    map: &Map,
    t: &mut MapTemplate,
    n: usize,
    slot: &Slot,
    mines: Mines,
) -> Result<()> {
    let (class, raw) = (u16::from(slot.class), raw_type(rules, slot));
    match (map.island_of(slot), &t.slots[n].kind) {
        (Some(path), SlotKind::Fixed { .. }) => {
            t.set_island(n, path)?;
            t.set_fixed_type(n, raw)?;
        }
        (Some(path), kind) => {
            if matches!(kind, SlotKind::Other(_)) {
                t.unfix(n, class, raw)?;
            }
            t.set_random(n, class, raw)?;
            t.fix(n, path, None, mines)?;
        }
        (None, SlotKind::Other(_)) if slot.role == Role::Marker => {}
        (None, SlotKind::Random { .. }) => t.set_random(n, class, raw)?,
        (None, _) => t.unfix(n, class, raw)?,
    }
    if slot.fixed() {
        t.set_turns(n, slot.turns)?;
    }
    t.set_position(n, corner(map, slot)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::game::{Refs, SizeClass};
    use anno_formats::vfs::Install;

    // Starter slots take Large (class 2) and up, as 117's and 1800's do.
    #[test]
    fn slots_take_the_largest_island_that_fits() {
        // (class, role): Small normal, Medium normal ×2, Large starter,
        // decoration.
        let slots = [
            (0, Some(false)),
            (1, Some(false)),
            (1, Some(false)),
            (2, Some(true)),
            (1, None),
        ];
        let island = |island, class, regular, start| Candidate {
            island,
            class,
            regular,
            start,
        };
        // Two regular Medium islands and one Small that is both.
        let islands = [
            island(0, 1, true, false),
            island(1, 1, true, false),
            island(2, 0, true, true),
        ];
        assert_eq!(
            assign(&slots, &islands, 2),
            [Some(2), Some(0), Some(1), None, None],
            "Medium islands alternate; a start slot needs a Large one"
        );
        let start_only = [island(7, 2, false, true)];
        assert_eq!(
            assign(&slots, &start_only, 2),
            [None, None, None, Some(7), None],
            "a start-only island stays out of regular slots"
        );
        let regular = [island(7, 2, true, false)];
        assert_eq!(
            assign(&slots, &regular, 2),
            [None, None, None, None, None],
            "a regular Large island is no start, and too big for Medium"
        );
    }

    const LARGE: Setup = Setup {
        island_size: "Large",
        world_size: "Large",
    };

    static RULES: MapRules = MapRules {
        setups: &[
            LARGE,
            Setup {
                island_size: "Small",
                world_size: "Large",
            },
        ],
        roles: &[(Role::Normal, 0), (Role::Start, 1), (Role::Underwater, 7)],
        size_classes: &[
            SizeClass {
                name: "Small",
                slot: 100,
                start: false,
            },
            SizeClass {
                name: "Large",
                slot: 200,
                start: true,
            },
        ],
        grid: 8,
        fixed_turns: true,
        pool_slots: true,
        free_starts: false,
        min_starts: 1,
        min_gap: 10,
        min_edge: 20,
        layout_folder: ["maps/", ""],
        registers: Registers::NewMapType,
    };

    fn slot(at: [i32; 2], role: Role, class: u8, island: u16) -> Slot {
        Slot {
            at,
            island,
            role,
            class,
            turns: 0,
            raw_type: 0,
            source: Slot::NEW,
        }
    }

    // A 1000 m map, playable 10 m in, with a start slot clear of
    // everything; `slots` come after it.
    fn map(slots: &[Slot]) -> Map {
        let mut all = vec![slot([400, 400], Role::Start, 1, Slot::POOL)];
        all.extend_from_slice(slots);
        Map {
            name: "test".into(),
            region: "Roman".into(),
            size: [1000; 2],
            playable: [10, 10, 990, 990],
            islands: vec!["ours.a7m".into(), "gone.a7m".into()],
            variants: vec![Variant {
                setup: LARGE,
                size: [1000; 2],
                playable: [10, 10, 990, 990],
                slots: all,
                starts: Vec::new(),
                source: Arc::from(&[][..]),
                base: String::new(),
                replaces: Vec::new(),
            }],
        }
    }

    // What each check row finds in `map`, by its `what`, with what its
    // items name and the items.
    fn problems(map: &Map, facts: IslandFacts) -> Vec<(String, Level, Names, Vec<u64>)> {
        // An install of one file, which no slot names.
        let dir = tempfile::tempdir().unwrap();
        let mut archive = anno_formats::rda::Packer::new(Default::default());
        archive.add("data/x.txt", b"x").unwrap();
        std::fs::create_dir(dir.path().join("maindata")).unwrap();
        std::fs::write(
            dir.path().join("maindata/data0.rda"),
            archive.encode().unwrap(),
        )
        .unwrap();
        let install = Install::open(dir.path(), &["data"]).unwrap();
        let refs = Refs {
            install: &install,
            project: &|p| p == "ours.a7m",
            assets: None,
        };
        let islands = |_: &str| Some(facts);
        check(&RULES, map, &refs, &islands)
            .into_iter()
            .map(|p| (p.what, p.level, p.names, p.items))
            .collect()
    }

    const FINE: IslandFacts = IslandFacts {
        side: 90,
        regular: true,
        start: true,
        underwater: false,
        broken: false,
    };

    // The missing Small variant only warns.
    fn missing_setup() -> (String, Level, Names, Vec<u64>) {
        (
            "Setup options no variant answers".into(),
            Level::Gameplay,
            Names::Setups,
            vec![1],
        )
    }

    #[test]
    fn a_clear_map_only_warns_of_the_setup_it_lacks() {
        let m = map(&[slot([100, 100], Role::Normal, 0, 0)]);
        assert_eq!(problems(&m, FINE), [missing_setup()]);
    }

    #[test]
    fn each_check_finds_its_slots() {
        let one = |what: &str, slots: &[Slot], facts: IslandFacts, items: &[u64]| {
            let found = problems(&map(slots), facts);
            assert_eq!(
                found,
                [
                    (what.to_owned(), Level::Broken, Names::Slots, items.to_vec()),
                    missing_setup()
                ],
                "{what}"
            );
        };
        let normal = |at, class| slot(at, Role::Normal, class, Slot::POOL);
        one(
            "Slots outside the map",
            &[normal([950, 100], 0)],
            FINE,
            &[1],
        );
        one(
            "Slots too close to the map edge",
            &[normal([15, 500], 0)],
            FINE,
            &[1],
        );
        one(
            "Slots too close to another",
            &[normal([100, 100], 0), normal([205, 150], 0)],
            FINE,
            &[1, 2],
        );
        one(
            "Islands too big for their slot",
            &[slot([100, 100], Role::Normal, 0, 0)],
            IslandFacts { side: 150, ..FINE },
            &[1],
        );
        one(
            "Islands the install lacks",
            &[slot([100, 100], Role::Normal, 0, 1)],
            FINE,
            &[1],
        );
        one(
            "Islands whose last save is broken",
            &[slot([100, 100], Role::Normal, 0, 0)],
            IslandFacts {
                broken: true,
                ..FINE
            },
            &[1],
        );
        one(
            "Islands that can't play their slot's role",
            &[slot([100, 100], Role::Start, 1, 0)],
            IslandFacts {
                start: false,
                ..FINE
            },
            &[1],
        );
        one(
            "Islands that can't play their slot's role",
            &[slot([100, 100], Role::Underwater, 0, 0)],
            FINE,
            &[1],
        );

        // A second variant's slot items carry its index; a problem of the
        // whole variant names the variant, not its first slot.
        let mut m = map(&[]);
        m.variants.push(Variant {
            setup: RULES.setups[1],
            playable: [10, 10, 600, 990],
            slots: vec![normal([700, 100], 0)],
            ..m.variants[0].clone()
        });
        assert_eq!(
            problems(&m, FINE),
            [
                (
                    "Slots outside the playable area".into(),
                    Level::Broken,
                    Names::Slots,
                    vec![slot_item(1, 0)]
                ),
                (
                    "Variants with too few starts".into(),
                    Level::Broken,
                    Names::Variants,
                    vec![1]
                ),
            ]
        );
    }

    // A map whose first variant has a slot off the map and whose second
    // has no start: the slot problem names that slot, the starts problem
    // the second variant, and neither reads as the other.
    #[test]
    fn problems_say_whether_they_name_a_slot_a_variant_or_the_map() {
        let mut m = map(&[slot([950, 100], Role::Normal, 0, Slot::POOL)]);
        m.variants.push(Variant {
            setup: RULES.setups[1],
            slots: Vec::new(),
            ..m.variants[0].clone()
        });
        let found = problems(&m, FINE);
        let named = |names| {
            (found.iter())
                .filter(|p| p.2 == names)
                .map(|p| (p.0.as_str(), p.3.clone()))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            named(Names::Slots),
            [("Slots outside the map", vec![slot_item(0, 1)])]
        );
        assert_eq!(item_slot(slot_item(0, 1)), (0, 1));
        assert_eq!(
            named(Names::Variants),
            [("Variants with too few starts", vec![1])]
        );
        assert!(named(Names::Setups).is_empty(), "both setups answered");
        m.variants.pop();
        m.variants[0].slots.truncate(1);
        assert_eq!(
            problems(&m, FINE),
            [missing_setup()],
            "the whole map lacks setup 1"
        );
    }

    // Islands' saved listings hold problems from before `names` existed:
    // they read as objects, and an object problem still saves without it.
    #[test]
    fn saved_problems_read_without_names() {
        let old = r#"{"level":"Broken","what":"Unbuildable","count":1}"#;
        let p: Problem = serde_json::from_str(old).unwrap();
        assert_eq!(p.names, Names::Objects);
        assert_eq!(serde_json::to_string(&p).unwrap(), old);
        let slots = Problem {
            names: Names::Slots,
            items: vec![slot_item(2, 3)],
            ..p
        };
        let saved = serde_json::to_string(&slots).unwrap();
        assert!(saved.contains(r#""names":"slots""#), "{saved}");
        assert_eq!(serde_json::from_str::<Problem>(&saved).unwrap(), slots);
    }

    #[test]
    fn measure_gives_what_check_compares() {
        let m = map(&[
            slot([100, 100], Role::Normal, 0, Slot::POOL),
            slot([230, 150], Role::Normal, 0, 0),
        ]);
        let facts = |_: &str| Some(IslandFacts { side: 50, ..FINE });
        assert_eq!(
            measure(&RULES, &m, &facts),
            Measure {
                starts: 1,
                // The fixed island's own 50 m to the pool slot's 100 m.
                gap: 30,
                edge: 100,
            }
        );
    }

    // An unedited map writes the elements it read; `same_layout` ignores
    // where slots came from.
    #[test]
    fn a_reopened_map_has_the_same_layout() {
        let mut a = map(&[slot([100, 100], Role::Normal, 0, 0)]);
        let mut b = a.clone();
        b.variants[0].source = Arc::from(&[1u8][..]);
        b.variants[0].slots[1].source = 3;
        assert!(a.same_layout(&b));
        a.variants[0].slots[1].at[0] += 8;
        assert!(!a.same_layout(&b));
        let empty = b.clone().emptied("new", RULES.setups).unwrap();
        assert_eq!(empty.variants.len(), 2);
        assert!(empty.variants.iter().all(|v| v.slots.is_empty()));
        assert!(empty.islands.is_empty() && empty.name == "new");
    }
}
