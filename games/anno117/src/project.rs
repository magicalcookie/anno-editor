// Owns how a 117 project reaches the game: its mod loader's packaging, its
// maps (province templates read, written and registered as map types of
// their own), and Test in game's "Editor islands" map type, whose templates
// are copies of the province Default templates, or of a project layout,
// with the project's islands in fixed slots.

use anno_core::fertility;
use anno_core::map::{
    self, LayoutEntry, Map, MapRules, Maps, Registers, Role, Setup, ShippedMap, Variant,
};
use anno_core::project::{
    GuidRange, MapEntry, MapIsland, MapPlan, Packaging, TemplateCopy, candidates, regions,
};
use anno_formats::a7m::a7minfo::A7mInfo;
use anno_formats::a7m::template::{MapTemplate, Mines};
use anno_formats::assets::Assets;
use anno_formats::vfs::Install;
use anyhow::{Context, Result, ensure};

pub(crate) static PACKAGING: Packaging = Packaging {
    mods_folder: "mods",
    // The loader logs an error and refuses a mod without one.
    difficulties: &["unchanged", "easier", "harder", "cheat"],
    // Unreserved projects pick 100 GUIDs from 1.9 billion up, as 1800's do;
    // both games' modders reserve in one registry.
    guid_range: GuidRange {
        first: 1_900_000_000,
        size: 100,
        islands: 90,
        count: 1_000_000,
        registry: "https://github.com/anno-mods/GuidRanges",
    },
    loader_log: "mods/mod-loader.log",
};

/// The setup screen's map type list (asset `DifficultyBalancing`).
const DIFFICULTY_GUID: u32 = 2002450;
/// The map type ours is inserted after ("MapType Random").
const MAP_TYPE_ANCHOR_GUID: u32 = 43181;
/// A map's templates by setup island size, as the file stem's last word
/// names them (`roman_province_default_01_easy`). Test maps copy the
/// Default ones.
const VARIANTS: [(&str, &str); 3] = [("easy", "Large"), ("medium", "Medium"), ("hard", "Small")];
/// The smallest class that may take a starter slot: the shipped starters
/// are all Large or XL.
const STARTER_CLASS: u16 = 2;
/// Test maps and layouts go to `{0}{region}{1}/{name}/{name}.a7t` (and
/// `.a7tinfo`, `.a7te`).
const CUSTOM_MAPS: [&str; 2] = ["data/base/provinces/", "/templates/custom"];

/// Every shipped template's `TemplateSize`: the setup's world size option
/// is 1800's; 117 has one.
const WORLD_SIZE: &str = "Large";

const fn setup(island_size: &'static str) -> Setup {
    Setup {
        island_size,
        world_size: WORLD_SIZE,
    }
}

pub(crate) static MAP_RULES: MapRules = MapRules {
    setups: &[setup("Large"), setup("Medium"), setup("Small")],
    // Pool `IslandType` indices, from the shipped templates (map_census):
    // 2 is on the decoration islands, 3 on the traders, 4 on the pirates.
    // 6 is on two large pool islands of the press-event and a Celtic map,
    // and 5 on none, so they show as their number.
    roles: &[
        (Role::Normal, 0),
        (Role::Start, 1),
        (Role::Decoration, 2),
        (Role::ThirdParty, 3),
        (Role::Pirate, 4),
    ],
    size_classes: crate::SIZE_CLASSES,
    grid: 8,
    fixed_turns: true,
    pool_slots: true,
    free_starts: false,
    // The least any shipped Roman or Celtic pool template has
    // (map_census): footprints may touch, never overlap.
    min_starts: 4,
    min_gap: 0,
    min_edge: 16,
    layout_folder: CUSTOM_MAPS,
    registers: Registers::NewMapType,
};

pub(crate) struct MapType;

impl MapType {
    /// The file stem of `variant` in a layout saved as `name` in `region`.
    fn stem(region: &str, name: &str, variant: &Variant) -> String {
        let [before, after] = CUSTOM_MAPS;
        let region = region.to_ascii_lowercase();
        let folder = match VARIANTS
            .iter()
            .find(|(_, s)| *s == variant.setup.island_size)
        {
            Some((word, _)) if variant.setup != Setup::NONE => format!("{name}_{word}"),
            _ => name.to_owned(),
        };
        format!("{before}{region}{after}/{folder}/{folder}")
    }
}

/// The regions whose pool maps a layout may start on.
const BASE_REGIONS: [&str; 2] = ["Roman", "Celtic"];

/// A template's region, as `MapTemplate/TemplateRegion` says it: the
/// province folder it sits in (`provinces/egypt/` is `Egypt`).
fn region_of(path: &str) -> String {
    let folder = (path.split("/provinces/").nth(1))
        .and_then(|rest| rest.split('/').next())
        .unwrap_or_default();
    let mut rest = folder.chars();
    rest.next().map_or(String::new(), |first| {
        first.to_ascii_uppercase().to_string() + rest.as_str()
    })
}

/// A template path's map: its stem, its name without the variant word,
/// the folder its variants' folders sit in, and the word. A file's folder
/// is mostly named as the file, but not always (the press-event map's).
struct Family<'a> {
    stem: &'a str,
    name: &'a str,
    parent: &'a str,
    word: Option<&'static str>,
}

impl<'a> Family<'a> {
    fn of(path: &'a str) -> Option<Family<'a>> {
        let stem = [".a7tinfo", ".a7te", ".a7t"]
            .iter()
            .find_map(|ext| path.strip_suffix(ext))
            .unwrap_or(path);
        let (dir, file) = stem.rsplit_once('/')?;
        let parent = dir.rsplit_once('/').map_or("", |(p, _)| p);
        let word = VARIANTS
            .iter()
            .map(|(w, _)| *w)
            .find(|w| file.ends_with(&format!("_{w}")));
        let name = word.map_or(file, |w| &file[..file.len() - w.len() - 1]);
        Some(Family {
            stem,
            name,
            parent,
            word,
        })
    }

    fn setup(&self) -> Setup {
        (VARIANTS.iter())
            .find(|(w, _)| Some(*w) == self.word)
            .map_or(Setup::NONE, |(_, size)| setup(size))
    }

    /// The stem of the variant called `word`: the word swapped in the
    /// folder's name and the file's.
    fn sibling(&self, word: &str) -> String {
        let Some(was) = self.word else {
            return self.stem.to_owned();
        };
        let (dir, file) = self.stem.rsplit_once('/').expect("parsed above");
        let swap = |s: &str| match s.strip_suffix(&format!("_{was}")) {
            Some(head) => format!("{head}_{word}"),
            None => s.to_owned(),
        };
        let dir = dir.rsplit_once('/').map_or(dir, |(_, d)| d);
        format!("{}/{}/{}", self.parent, swap(dir), swap(file))
    }
}

impl Maps for MapType {
    fn rules(&self) -> &'static MapRules {
        &MAP_RULES
    }

    /// Every template the install ships, one entry per map: the three
    /// island-size files of a pool map are one.
    fn shipped(&self, install: &Install, _: Option<&Assets>) -> Vec<ShippedMap> {
        let mut out: Vec<ShippedMap> = Vec::new();
        let mut paths: Vec<String> = (install.search(".a7tinfo", 2000).into_iter())
            .map(|id| install.file_path(id).to_owned())
            .collect();
        paths.sort();
        for path in paths {
            let Some(f) = Family::of(&path) else {
                continue;
            };
            let (name, parent, setup) = (f.name, f.parent, f.setup());
            let a7t = format!("{}.a7t", f.stem);
            if let Some(m) = out.last_mut().filter(|m| {
                m.name == name && Family::of(&m.path).is_some_and(|f| f.parent == parent)
            }) {
                m.setups.push(setup);
                continue;
            }
            let Ok(t) = install.read(&path).and_then(|b| MapTemplate::parse(&b)) else {
                continue;
            };
            let group = (path.split("/templates/").nth(1))
                .and_then(|g| g.split('/').next())
                .unwrap_or_default();
            out.push(ShippedMap {
                path: a7t,
                name: name.to_owned(),
                region: region_of(&path),
                group: group.to_owned(),
                size: t.size,
                setups: vec![setup],
                // The DLC's enlarged copies (`_dlc01expanded`) are the
                // engine's to pick; a layout writes the plain size. Egypt's
                // slots don't fit the size classes the editor knows (its
                // default map overlaps under them), so it is no base yet.
                base: group == "pool"
                    && !f.stem.contains("expanded")
                    && BASE_REGIONS.contains(&region_of(&path).as_str()),
            });
        }
        // In the order `read` gives the variants.
        for m in &mut out {
            m.setups
                .sort_by_key(|s| VARIANTS.iter().position(|(_, size)| *size == s.island_size));
        }
        out
    }

    fn read(&self, install: &Install, path: &str) -> Result<Map> {
        let f = Family::of(path).with_context(|| format!("{path}: not a template path"))?;
        let variants = match f.word {
            Some(_) => (VARIANTS.iter())
                .map(|(word, size)| (f.sibling(word), setup(size), Vec::new()))
                .filter(|(stem, ..)| install.exists(&format!("{stem}.a7tinfo")))
                .collect(),
            None => vec![(f.stem.to_owned(), Setup::NONE, Vec::new())],
        };
        map::read_templates(install, &MAP_RULES, f.name, &region_of(path), variants)
    }

    fn write(
        &self,
        install: &Install,
        map: &Map,
        folder_name: &str,
    ) -> Result<Vec<(String, Vec<u8>)>> {
        ensure!(
            !map.region.is_empty(),
            "{}: a layout without a region",
            map.name
        );
        map::write_templates(
            install,
            &MAP_RULES,
            map,
            |v| Self::stem(&map.region, folder_name, v),
            Mines::Activation(&[]),
        )
    }

    fn new_map(&self, install: &Install, base: &str, name: &str) -> Result<Map> {
        self.read(install, base)?.emptied(name, MAP_RULES.setups)
    }

    /// Per layout, its map type, its entry in the setup list and one
    /// `MapTemplate` per variant; the test maps' "Editor islands" type the
    /// same way. A layout whose GUIDs aren't allocated yet is left out.
    fn register(&self, layouts: &[LayoutEntry], tests: &[MapEntry], test_guid: u32) -> String {
        let mut xml = String::new();
        for l in layouts {
            if l.guids.len() < Registers::NewMapType.guids(l.variants.len()) {
                continue;
            }
            xml += &map_type(&l.name, l.guids[0], l.variants.iter().zip(&l.guids[1..]));
        }
        if !tests.is_empty() {
            let guids: Vec<u32> = (1..=tests.len() as u32).map(|i| test_guid + i).collect();
            xml += &map_type("Editor islands", test_guid, tests.iter().zip(&guids));
        }
        xml
    }

    fn plan(
        &self,
        install: &Install,
        assets: Option<&Assets>,
        islands: &[MapIsland],
        mod_id: &str,
        on: Option<&Map>,
    ) -> Result<Vec<MapPlan>> {
        // (template, base stem, region, island size, variant word)
        let mut copies = Vec::new();
        match on {
            Some(map) => {
                let name = map
                    .name
                    .to_ascii_lowercase()
                    .replace(|c: char| !c.is_ascii_alphanumeric(), "_");
                for v in &map.variants {
                    let word = (VARIANTS.iter())
                        .find(|(_, s)| *s == v.setup.island_size)
                        .map_or("", |(w, _)| w);
                    let template = map::template_of(&MAP_RULES, map, v, Mines::Activation(&[]))?;
                    copies.push((
                        template,
                        v.base.clone(),
                        map.region.clone(),
                        v.setup.island_size,
                        format!("{name}_{word}"),
                    ));
                }
            }
            None => {
                for region in regions(islands) {
                    let lower = region.to_ascii_lowercase();
                    for (variant, island_size) in VARIANTS {
                        let name = format!("{lower}_province_default_01_{variant}");
                        let base =
                            format!("data/base/provinces/{lower}/templates/pool/{name}/{name}");
                        let path = format!("{base}.a7tinfo");
                        let template = MapTemplate::parse(
                            &install
                                .read(&path)
                                .with_context(|| format!("Reading {path}"))?,
                        )?;
                        copies.push((
                            template,
                            base,
                            region.to_owned(),
                            island_size,
                            variant.to_owned(),
                        ));
                    }
                }
            }
        }
        let mut out = Vec::new();
        for (template, base, region, island_size, variant) in copies {
            let candidates = candidates(crate::SIZE_CLASSES, islands, &region);
            let copy = TemplateCopy {
                classes: crate::SIZE_CLASSES,
                a7m: &crate::A7M,
                template,
                base: &base,
                region: &region,
                island_size,
                variant: &variant,
                folder: CUSTOM_MAPS,
                starter_class: STARTER_CLASS,
                source_guids: Vec::new(),
                relabels_map_type: false,
            };
            out.push(copy.plan(
                install,
                islands,
                &candidates,
                mod_id,
                |t, n, island, start| {
                    let (fertilities, active) = resources(install, assets, island, start)?;
                    t.fix(
                        n,
                        island.path,
                        Some(&fertilities[..]),
                        Mines::Activation(&active),
                    )
                },
            )?);
        }
        Ok(out)
    }
}

/// A map type called `label` numbered `ty`, its entry in the setup list,
/// and one `MapTemplate` per `(template, GUID)`. Templates carry no
/// `EnlargeDLC`, so DLC owners get the same file. Wiggling is off: it
/// shifts slots, and fixed islands were placed to fit their slot.
fn map_type<'a>(
    label: &str,
    ty: u32,
    templates: impl Iterator<Item = (&'a MapEntry, &'a u32)>,
) -> String {
    let mut xml = format!(
        "  <ModOp Type=\"addNextSibling\" GUID=\"{MAP_TYPE_ANCHOR_GUID}\">\n    <Asset>\n      <Template>MapTemplateType</Template>\n      <Values>\n        <Standard>\n          <GUID>{ty}</GUID>\n          <Name>MapType {label}</Name>\n        </Standard>\n        <MapTemplateType>\n          <Name>{ty}</Name>\n        </MapTemplateType>\n      </Values>\n    </Asset>\n"
    );
    for (m, guid) in templates {
        xml += &format!(
            "    <Asset>\n      <Template>MapTemplate</Template>\n      <Values>\n        <Standard>\n          <GUID>{guid}</GUID>\n          <Name>Map Template {} {label} - {}</Name>\n        </Standard>\n        <MapTemplate>\n          <IslandSize>{}</IslandSize>\n          <TemplateFilename>{}</TemplateFilename>\n          <TemplateSize>{WORLD_SIZE}</TemplateSize>\n          <IsUsedByMapGenerator>1</IsUsedByMapGenerator>\n          <TemplateRegion>{}</TemplateRegion>\n          <MapTemplateType>{ty}</MapTemplateType>\n          <WiggleIterationCount>0</WiggleIterationCount>\n        </MapTemplate>\n      </Values>\n    </Asset>\n",
            m.region, m.island_size, m.island_size, m.path, m.region
        );
    }
    xml += "  </ModOp>\n";
    xml += &format!(
        "  <ModOp Type=\"add\" GUID=\"{DIFFICULTY_GUID}\" Path=\"/Values/DifficultySettings/MapTemplateTypes\">\n    <Item>\n      <MapType>{ty}</MapType>\n    </Item>\n  </ModOp>\n"
    );
    xml
}

/// What a fixed slot holding `island` carries for its resources:
/// `FertilityGuids` and the slot object IDs to force on. Islands without
/// assigned resources keep both empty, leaving them to the map generator.
/// Assignments are checked against the saved `.a7minfo` slot list, the one
/// the generator reads.
fn resources(
    install: &Install,
    assets: Option<&Assets>,
    island: &MapIsland,
    start: bool,
) -> Result<(Vec<u32>, Vec<u64>)> {
    let (Some(assets), false) = (assets, island.slot_resources.is_empty()) else {
        return Ok(Default::default());
    };
    let info = A7mInfo::parse(&island.info(install)?)
        .with_context(|| format!("Reading {}info", island.path))?;
    let mut slot_types: Vec<&str> = Vec::new();
    let mut active = Vec::new();
    let mut assigned = Vec::new();
    for group in &info.random_slots() {
        slot_types.extend(fertility::slot_type_of(assets, group.guid));
        for o in &group.objects {
            if let Some(&g) = island.slot_resources.get(&o.object_id) {
                active.push(o.object_id);
                assigned.push(g);
            }
        }
    }
    let list = fertility::island_list(assets, island.region, start, &slot_types, &assigned);
    Ok((list, active))
}

#[cfg(test)]
mod tests {
    use anno_core::fertility::*;
    use anno_formats::assets::Assets;
    use anno_formats::vfs::Install;

    fn assets() -> Option<Assets> {
        let Some(dir) = anno_formats::vfs::install_dir("ANNO117_DIR") else {
            eprintln!("skip: ANNO117_DIR unset");
            return None;
        };
        let install = Install::open(dir, crate::ARCHIVE_FAMILIES).unwrap();
        Some(Assets::load(&install, &crate::assets::SCHEMA).unwrap())
    }

    #[test]
    fn slot_kinds_offer_the_regions_deposits() {
        let Some(assets) = assets() else { return };
        let guids = |region, kind| -> Vec<u32> {
            let mut g: Vec<u32> = deposits(&assets, region, kind)
                .iter()
                .map(|d| d.guid)
                .collect();
            g.sort();
            g
        };
        // Iron, copper, tin, granite, silver; birds and beavers.
        assert_eq!(guids("Celtic", "Mountain"), [4049, 4063, 4064, 4066, 8487]);
        assert_eq!(guids("Celtic", "Marsh"), [2219, 4082]);
        assert!(guids("Celtic", "River").is_empty());
        // Iron, marble, minerals; sturgeon and gold.
        assert_eq!(guids("Roman", "Mountain"), [4049, 4053, 4062]);
        assert_eq!(guids("Roman", "River"), [8577, 32027]);
        assert!(
            deposits(&assets, "Celtic", "Mountain")
                .iter()
                .all(|d| !d.label.is_empty() && !d.label.starts_with("Deposit"))
        );
        assert_eq!(slot_type_of(&assets, 38473), Some("Mountain"));
        assert_eq!(slot_type_of(&assets, 5282), Some("Marsh"));
    }

    #[test]
    fn an_assigned_deposit_joins_a_full_island_list() {
        let Some(assets) = assets() else { return };
        assert!(island_list(&assets, "Celtic", false, &["Mountain"], &[]).is_empty());
        let list = island_list(&assets, "Celtic", false, &["Mountain"], &[4066]);
        assert!(list.contains(&4066), "{list:?}");
        assert!(list.windows(2).all(|w| w[0] < w[1]), "ascending: {list:?}");
        assert!(list.len() >= 4, "crops and fish too: {list:?}");
        // No marsh slots on the island: no birds or beavers.
        assert!(!list.contains(&2219) && !list.contains(&4082), "{list:?}");
        let marsh = island_list(&assets, "Celtic", false, &["Mountain", "Marsh"], &[2219]);
        assert!(marsh.contains(&2219));
    }

    mod maps {
        use super::super::{MAP_RULES, MapType};
        use anno_core::map::{self, IslandFacts, LayoutEntry, Map, Maps, Role, Slot};
        use anno_core::project::MapEntry;
        use anno_formats::a7m::a7minfo::A7mInfo;
        use anno_formats::a7m::template::{MapTemplate, SlotKind};
        use anno_formats::vfs::Install;
        use std::collections::BTreeMap;

        const DEFAULT: &str = "data/base/provinces/roman/templates/pool/roman_province_default_01_easy/roman_province_default_01_easy.a7t";

        fn install() -> Option<Install> {
            let Some(dir) = anno_formats::vfs::install_dir("ANNO117_DIR") else {
                eprintln!("skip: ANNO117_DIR unset");
                return None;
            };
            Some(Install::open(dir, crate::ARCHIVE_FAMILIES).unwrap())
        }

        // The model's slots, walked against the template's own.
        fn same_slots(map: &Map, slots: &[Slot], t: &MapTemplate, path: &str) {
            assert_eq!(t.slots.len(), slots.len(), "{path}");
            for (n, (a, b)) in t.slots.iter().zip(slots).enumerate() {
                assert_eq!(a.position.map(|v| v as i32), b.at, "{path} slot {n}");
                match &a.kind {
                    SlotKind::Random { size, island_type } => {
                        assert_eq!(
                            (b.island, b.class as u16, b.raw_type),
                            (Slot::POOL, *size, *island_type)
                        );
                    }
                    SlotKind::Fixed {
                        path: island,
                        island_type,
                    } => {
                        assert_eq!(map.island_of(b), Some(island.as_str()), "{path} slot {n}");
                        assert_eq!((b.turns, b.raw_type), (a.turns, *island_type));
                    }
                    SlotKind::Other(_) => assert_eq!(b.role, Role::Marker),
                }
            }
        }

        // Needs Anno 117: every shipped template, read as a map and written
        // back unedited, gives its shipped bytes, and the map's slots are
        // the template's. Every template is in exactly one listed map.
        #[test]
        fn every_shipped_map_round_trips_through_the_model() {
            let Some(install) = install() else { return };
            let maps = MapType;
            let shipped = maps.shipped(&install, None);
            let mut bases = Vec::new();
            for s in &shipped {
                let map = maps.read(&install, &s.path).unwrap();
                assert_eq!(
                    map.variants.iter().map(|v| v.setup).collect::<Vec<_>>(),
                    s.setups,
                    "{}",
                    s.path
                );
                let files = maps.write(&install, &map, "round_trip").unwrap();
                let mut files = files.iter();
                for v in &map.variants {
                    for ext in ["a7tinfo", "a7t", "a7te"] {
                        let path = format!("{}.{ext}", v.base);
                        let Ok(shipped) = install.read(&path) else {
                            assert_eq!(ext, "a7te", "{path}");
                            continue;
                        };
                        let (to, bytes) = files.next().unwrap();
                        assert!(
                            to.ends_with(ext) && shipped == *bytes,
                            "{path} written as shipped"
                        );
                    }
                    same_slots(
                        &map,
                        &v.slots,
                        &MapTemplate::parse(&v.source).unwrap(),
                        &v.base,
                    );
                    bases.push(v.base.clone());
                }
            }
            bases.sort();
            let mut all: Vec<String> = (install.search(".a7tinfo", 2000).into_iter())
                .map(|id| {
                    install
                        .file_path(id)
                        .trim_end_matches(".a7tinfo")
                        .to_owned()
                })
                .collect();
            all.sort();
            assert_eq!(bases, all);
        }

        // Needs Anno 117: the shipped pool templates set `MAP_RULES`' census
        // values and pass every check, and the type ids it names sit on the
        // islands their roles say.
        #[test]
        fn map_census() {
            let Some(install) = install() else { return };
            let maps = MapType;
            let side = |path: &str| -> Option<u32> {
                Some(
                    A7mInfo::parse(&install.read(format!("{path}info")).ok()?)
                        .ok()?
                        .map_size[0],
                )
            };
            let facts = |path: &str| {
                Some(IslandFacts {
                    side: side(path)?,
                    regular: true,
                    start: true,
                    underwater: false,
                    broken: false,
                })
            };
            let refs = anno_core::game::Refs {
                install: &install,
                project: &|_| false,
                assets: None,
            };
            let (mut starts, mut gap, mut edge) = (usize::MAX, i32::MAX, i32::MAX);
            let mut ids = BTreeMap::new();
            let mut difficulty = BTreeMap::new();
            let mut third_parties = BTreeMap::new();
            for s in maps.shipped(&install, None) {
                let m = maps.read(&install, &s.path).unwrap();
                for v in &m.variants {
                    let t = MapTemplate::parse(&v.source).unwrap();
                    *third_parties.entry(t.third_parties).or_insert(0) += 1;
                    for slot in &t.slots {
                        *difficulty.entry(slot.difficulty).or_insert(0) += 1;
                        if let SlotKind::Fixed { path, island_type } = &slot.kind {
                            let folder =
                                path.split('/').rev().nth(2).unwrap_or_default().to_owned();
                            let name = path.rsplit('/').next().unwrap_or_default().to_owned();
                            ids.entry((*island_type, folder))
                                .or_insert_with(Vec::new)
                                .push(name);
                        }
                    }
                    for slot in &v.slots {
                        assert_eq!(
                            slot.at.map(|p| p % MAP_RULES.grid as i32),
                            [0, 0],
                            "{} on the grid",
                            v.base
                        );
                    }
                }
                if !s.base {
                    continue;
                }
                let measured = map::measure(&MAP_RULES, &m, &facts);
                (starts, gap, edge) = (
                    starts.min(measured.starts),
                    gap.min(measured.gap),
                    edge.min(measured.edge),
                );
                let problems: Vec<_> = (map::check(&MAP_RULES, &m, &refs, &facts).into_iter())
                    .filter(|p| p.level == anno_core::island::Level::Broken)
                    .collect();
                assert!(problems.is_empty(), "{}: {problems:?}", s.path);
            }
            eprintln!("census: starts {starts}, gap {gap}, edge {edge}");
            eprintln!(
                "difficulty ids {difficulty:?}; RandomlyPlacedThirdParties entries {third_parties:?}"
            );
            for ((id, folder), names) in &ids {
                eprintln!("fixed type {id} in {folder}: {names:?}");
            }
            assert_eq!(
                (
                    usize::from(MAP_RULES.min_starts),
                    MAP_RULES.min_gap,
                    MAP_RULES.min_edge
                ),
                (starts, gap, edge)
            );
            // The roles named by their islands: the shipped third-party
            // islands play them wherever they are placed. (Campaign and
            // Egypt islands play them too, under names of their own.)
            for ((id, folder), names) in &ids {
                let role = MAP_RULES.role(*id);
                for name in names.iter().filter(|_| folder == "3rdparty") {
                    if name.contains("trader") {
                        assert_eq!(role, Role::ThirdParty, "{name}");
                    } else if name.contains("pirate") {
                        assert_eq!(role, Role::Pirate, "{name}");
                    }
                }
                if names.iter().all(|n| n.contains("deco")) && folder == "campaign" {
                    assert_eq!(role, Role::Decoration, "{names:?}");
                }
            }
        }

        // Needs Anno 117: a new layout on the Default map, given a start, a
        // pool and a fixed slot, writes templates whose slots read back as
        // set beside the base's own `.a7t` and `.a7te`, and registers as a
        // map type with a shipped `MapTemplate`'s fields.
        #[test]
        fn a_new_layout_writes_and_registers_like_a_shipped_one() {
            let Some(install) = install() else { return };
            let maps = MapType;
            let mut map = maps.new_map(&install, DEFAULT, "Ring of Gold").unwrap();
            let setups: Vec<&str> = map.variants.iter().map(|v| v.setup.island_size).collect();
            assert_eq!(setups, ["Large", "Medium", "Small"]);
            assert!(map.variants.iter().all(|v| v.slots.is_empty()));
            let island = "data/base/provinces/roman/islands/pool/roman_island_large_01/roman_island_large_01.a7m";
            let fixed = map.island(island);
            let slot = |at, role, class, island, turns| Slot {
                at,
                island,
                role,
                class,
                turns,
                raw_type: 0,
                source: Slot::NEW,
            };
            for v in &mut map.variants {
                v.slots = vec![
                    slot([200, 200], Role::Start, 2, Slot::POOL, 0),
                    slot([800, 800], Role::Normal, 1, Slot::POOL, 0),
                    slot([1200, 400], Role::Normal, 2, fixed, 1),
                ];
            }
            let files = maps.write(&install, &map, "ring_of_gold").unwrap();
            let paths: Vec<&str> = files.iter().map(|(p, _)| p.as_str()).collect();
            assert_eq!(
                paths[..3],
                [
                    "data/base/provinces/roman/templates/custom/ring_of_gold_easy/ring_of_gold_easy.a7tinfo",
                    "data/base/provinces/roman/templates/custom/ring_of_gold_easy/ring_of_gold_easy.a7t",
                    "data/base/provinces/roman/templates/custom/ring_of_gold_easy/ring_of_gold_easy.a7te",
                ]
            );
            for (v, files) in map.variants.iter().zip(files.chunks(3)) {
                let t = MapTemplate::parse(&files[0].1).unwrap();
                let kinds: Vec<(&SlotKind, [u32; 2], u8)> = t
                    .slots
                    .iter()
                    .map(|s| (&s.kind, s.position, s.turns))
                    .collect();
                assert_eq!(
                    kinds,
                    [
                        (
                            &SlotKind::Random {
                                size: 2,
                                island_type: 1
                            },
                            [200, 200],
                            0
                        ),
                        (
                            &SlotKind::Random {
                                size: 1,
                                island_type: 0
                            },
                            [800, 800],
                            0
                        ),
                        (
                            &SlotKind::Fixed {
                                path: island.into(),
                                island_type: 0
                            },
                            [1200, 400],
                            1
                        ),
                    ]
                );
                assert_eq!(t.size, map.size);
                assert!(files[1].1 == install.read(format!("{}.a7t", v.base)).unwrap());
                assert!(files[2].1 == install.read(format!("{}.a7te", v.base)).unwrap());
            }

            let entry = LayoutEntry {
                name: map.name.clone(),
                path: files[1].0.clone(),
                region: "Roman".into(),
                variants: (map.variants.iter().zip(files.chunks(3)))
                    .map(|(v, f)| MapEntry {
                        path: f[1].0.clone(),
                        region: "Roman".into(),
                        island_size: v.setup.island_size.into(),
                        source_guids: Vec::new(),
                    })
                    .collect(),
                guids: vec![1_900_000_089, 1_900_000_088, 1_900_000_087, 1_900_000_086],
                fill_with_islands: false,
            };
            let xml = maps.register(std::slice::from_ref(&entry), &[], 0);
            let assets: Vec<&str> = xml.split("<Asset>").skip(1).collect();
            assert_eq!(assets.len(), 4, "{xml}");
            assert_eq!(
                anno_formats::xml::tag(assets[0], "Name"),
                Some("MapType Ring of Gold")
            );
            for (a, v) in assets[1..].iter().zip(&entry.variants) {
                for (field, want) in [
                    ("IslandSize", v.island_size.as_str()),
                    ("TemplateFilename", v.path.as_str()),
                    ("TemplateSize", "Large"),
                    ("IsUsedByMapGenerator", "1"),
                    ("TemplateRegion", "Roman"),
                    ("MapTemplateType", "1900000089"),
                ] {
                    assert_eq!(
                        anno_formats::xml::tag(a, field),
                        Some(want),
                        "{field} in {a}"
                    );
                }
            }
            assert!(xml.contains("<MapType>1900000089</MapType>"));
            // Not allocated yet: nothing.
            let bare = LayoutEntry {
                guids: Vec::new(),
                ..entry
            };
            assert_eq!(maps.register(&[bare], &[], 0), "");

            // Test in game on the layout fills its pool slots.
            let none = BTreeMap::new();
            let ours = anno_core::project::MapIsland {
                path: "data/base/provinces/roman/islands/custom/ours/ours.a7m",
                region: "Roman",
                class: "Large",
                regular: true,
                start: true,
                on_test_map: true,
                slot_resources: &none,
                fertilities: None,
                info: None,
                file: None,
            };
            let plans = maps
                .plan(
                    &install,
                    None,
                    std::slice::from_ref(&ours),
                    "golden-isles",
                    Some(&map),
                )
                .unwrap();
            assert_eq!(plans.len(), 3);
            assert!(plans.iter().all(|p| p.ours() >= 1 && p.starts_on_ours()));
        }
    }
}
