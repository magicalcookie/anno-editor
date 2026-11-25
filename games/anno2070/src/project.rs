// Owns how a 2070 project reaches the game's maps: its worlds read and
// written as map layouts, and Test in game's world, a shipped scenario's
// world (or a project layout) with the project's islands swapped in.
// Getting the game to list one is registration, which waits for delivery.

use std::sync::Arc;

use anno_core::map::{
    Fill, Map, MapRules, Maps, Registers, Role, Setup, ShippedMap, Slot, Variant,
};
use anno_core::project::{MapEntry, MapIsland, MapPlan, PlanSlot};
use anno_formats::assets::Assets;
use anno_formats::isd::world::{self, Kind, Placed};
use anno_formats::isd::{Doc, Value};
use anno_formats::vfs::Install;
use anyhow::{Context, Result, bail, ensure};

/// The world a test map starts from: scenario 06 (Power games), which is in
/// the mission menu (the spike's proof world). Its other islands, ark
/// slots and players stay, so the scenario still plays.
const TEMPLATE: &str = "data/levels/scenarios/singleplayer/06.www";
/// Where the test map goes, `{}` the project's name for it. Shipped test
/// worlds sit under `data/levels/testworlds/`.
const FOLDER: &str = "data/levels/testworlds";
/// Shipped islands sit at `SeaLevel` 0 above water and −28 to −32 m below;
/// anything under this is an underwater island.
const UNDERWATER_BELOW: f64 = -10.0;

/// Where layouts go: `{0}{name}.www`, beside the test worlds.
const LAYOUTS: [&str; 2] = ["data/levels/testworlds/", ""];

pub(crate) static MAP_RULES: MapRules = MapRules {
    setups: &[Setup::NONE],
    // A world entry's `Type`.
    roles: &[
        (Role::Normal, Kind::Regular as u16),
        (Role::Underwater, Kind::Underwater as u16),
        (Role::Start, Kind::Start as u16),
        (
            Role::ThirdPartyUnderwater,
            Kind::ThirdPartyUnderwater as u16,
        ),
        (Role::ThirdParty, Kind::ThirdPartySurface as u16),
        (Role::Decoration, Kind::Decoration as u16),
    ],
    size_classes: crate::SIZE_CLASSES,
    // Every shipped position is on the 16 m chunk grid.
    grid: 16,
    fixed_turns: true,
    pool_slots: false,
    free_starts: true,
    // The least any shipped scenario or campaign world has (map_census):
    // start entries and arks together, islands overlapping by up to 80 m
    // of their sea margin.
    min_starts: 3,
    min_gap: -80,
    min_edge: 32,
    layout_folder: LAYOUTS,
    registers: Registers::None,
};

// The world entry `Type` a slot writes.
fn kind(slot: &Slot) -> Result<Kind> {
    let raw = match slot.role {
        Role::Other | Role::Marker => slot.raw_type,
        role => MAP_RULES
            .raw_type(role)
            .with_context(|| format!("2070 has no {} islands", role.label()))?,
    };
    Kind::ALL
        .into_iter()
        .find(|&k| k as u16 == raw)
        .with_context(|| format!("world entry Type {raw}"))
}

fn stem(path: &str) -> &str {
    let file = path.rsplit(['/', '\\']).next().unwrap_or(path);
    file.rsplit_once('.').map_or(file, |(stem, _)| stem)
}

pub(crate) struct World;

impl Maps for World {
    fn rules(&self) -> &'static MapRules {
        &MAP_RULES
    }

    /// Every world the install ships, scenario 06 first: the one checked
    /// in game.
    fn shipped(&self, install: &Install, _: Option<&Assets>) -> Vec<ShippedMap> {
        let mut out: Vec<ShippedMap> = (install.search(".www", 1000).into_iter())
            .filter_map(|id| {
                let path = install.file_path(id).to_owned();
                let size = world::size(&Doc::parse(&install.read_file(id).ok()?).ok()?).ok()?;
                let group = path.rsplit('/').nth(1).unwrap_or_default().to_owned();
                Some(ShippedMap {
                    name: stem(&path).to_owned(),
                    region: String::new(),
                    group,
                    size,
                    setups: vec![Setup::NONE],
                    base: true,
                    path,
                })
            })
            .collect();
        out.sort_by(|a, b| (a.path != TEMPLATE, &a.path).cmp(&(b.path != TEMPLATE, &b.path)));
        out
    }

    fn read(&self, install: &Install, path: &str) -> Result<Map> {
        let bytes: Arc<[u8]> = install
            .read(path)
            .with_context(|| format!("Reading {path}"))?
            .into();
        let doc = Doc::parse(&bytes).with_context(|| format!("Reading {path}"))?;
        let placed = world::read(&doc).with_context(|| format!("Reading {path}"))?;
        ensure!(
            placed.len() <= Map::MAX_SLOTS,
            "{path}: {} islands",
            placed.len()
        );
        let size = world::size(&doc)?;
        let mut map = Map {
            name: stem(path).to_owned(),
            region: String::new(),
            size,
            playable: [0, 0, size[0] as i32, size[1] as i32],
            islands: Vec::new(),
            variants: Vec::new(),
        };
        let slots = (placed.iter().enumerate())
            .map(|(n, p)| Slot {
                at: [p.x, p.z],
                island: map.island(&p.file),
                role: MAP_RULES.role(p.kind as u16),
                class: MAP_RULES.class_for(p.side),
                turns: p.direction,
                raw_type: p.kind as u16,
                source: n as u16,
            })
            .collect();
        map.variants.push(Variant {
            setup: Setup::NONE,
            size,
            playable: map.playable,
            slots,
            starts: world::arks(&doc)?,
            source: bytes,
            base: path.to_owned(),
            replaces: Vec::new(),
        });
        Ok(map)
    }

    /// The one world: its source with the slots placed by
    /// `world::write_by_source`, so an entry keeps its carried children
    /// and an unchanged map writes its bytes. A slot whose island changed
    /// takes the new island's side and file stem as its name.
    fn write(
        &self,
        install: &Install,
        map: &Map,
        folder_name: &str,
    ) -> Result<Vec<(String, Vec<u8>)>> {
        let [v] = &map.variants[..] else {
            bail!(
                "{}: a world has one variant, not {}",
                map.name,
                map.variants.len()
            );
        };
        let mut doc = Doc::parse(&v.source)?;
        let was = world::read(&doc)?;
        // Kept entries in their order, new ones after (`write_by_source`).
        let mut slots: Vec<&Slot> = v.slots.iter().collect();
        slots.sort_by_key(|s| s.source);
        let mut list = Vec::with_capacity(slots.len());
        for s in slots {
            let file = (map.island_of(s))
                .with_context(|| format!("{}: 2070 has no pool slots", map.name))?;
            let source = (s.source != Slot::NEW).then_some(s.source as usize);
            let old = match source {
                Some(i) => Some(
                    was.get(i)
                        .with_context(|| format!("{}: no world entry {i}", map.name))?,
                ),
                None => None,
            };
            let same = old.filter(|o| o.file.eq_ignore_ascii_case(file));
            let (side, name) = match same {
                Some(o) => (o.side, o.name.clone()),
                None => {
                    let bytes = install
                        .read(file)
                        .with_context(|| format!("Reading {file}"))?;
                    (sides(&bytes)?[0], stem(file).to_owned())
                }
            };
            let [x, z] = s.at;
            list.push((
                source,
                Placed {
                    file: file.to_owned(),
                    name,
                    x,
                    z,
                    direction: s.turns,
                    side,
                    kind: kind(s)?,
                },
            ));
        }
        world::write_by_source(&mut doc, &list)?;
        if world::arks(&doc)? != v.starts {
            world::write_arks(&mut doc, &v.starts)?;
        }
        let [before, after] = LAYOUTS;
        Ok(vec![(
            format!("{before}{folder_name}{after}.www"),
            doc.encode(),
        )])
    }

    /// A world with its islands gone and its ark slots kept.
    fn new_map(&self, install: &Install, base: &str, name: &str) -> Result<Map> {
        self.read(install, base)?.emptied(name, MAP_RULES.setups)
    }

    // Nothing registers a world yet: that is delivery's.

    // A world entry's side must be the island's own `Width`.
    fn reads_islands(&self) -> bool {
        true
    }

    /// Each island on the test map takes the template's smallest free slot
    /// of its kind that holds it, centred on the slot and turned as the
    /// slot was: a start island a start slot (else a regular one), a
    /// regular island a regular one, an underwater island an underwater
    /// one. Decoration and third-party islands stay.
    fn plan(
        &self,
        install: &Install,
        _: Option<&Assets>,
        islands: &[MapIsland],
        mod_id: &str,
        on: Option<&Map>,
    ) -> Result<Vec<MapPlan>> {
        // A layout is the base as it would save.
        let (base, bytes) = match on {
            Some(map) => (
                map.name.clone(),
                self.write(install, map, &map.name)?.remove(0).1,
            ),
            None => (
                TEMPLATE.to_owned(),
                install
                    .read(TEMPLATE)
                    .with_context(|| format!("Reading {TEMPLATE}"))?,
            ),
        };
        let mut doc = Doc::parse(&bytes).with_context(|| format!("Reading {base}"))?;
        let shipped = world::read(&doc).with_context(|| format!("Reading {base}"))?;
        let mut placed = shipped.clone();
        let mut fill = vec![Fill::Vanilla; shipped.len()];
        let mut taken = vec![0u64; shipped.len().div_ceil(64)];
        let key = mod_id.replace('-', "_");
        for (n, island) in islands.iter().enumerate().filter(|(_, i)| i.on_test_map) {
            let (side, underwater) = measure(&island.file(install)?)
                .with_context(|| format!("Reading {}", island.path))?;
            let kinds: &[Kind] = match (underwater, island.start, island.regular) {
                (true, ..) => &[Kind::Underwater],
                (false, true, true) => &[Kind::Start, Kind::Regular],
                (false, true, false) => &[Kind::Start],
                (false, false, _) => &[Kind::Regular],
            };
            let Some(slot) = kinds.iter().find_map(|&kind| {
                (0..shipped.len())
                    .filter(|&s| taken[s / 64] >> (s % 64) & 1 == 0)
                    .filter(|&s| shipped[s].kind == kind && shipped[s].side >= side)
                    .min_by_key(|&s| shipped[s].side)
            }) else {
                bail!(
                    "{}: {base} has no free {} slot for a {side} m island",
                    island.path,
                    if underwater { "underwater" } else { "surface" }
                );
            };
            taken[slot / 64] |= 1 << (slot % 64);
            let was = &shipped[slot];
            // Shipped positions are on the 16 m chunk grid; stay on it.
            let inset = ((was.side - side) / 2 / 16 * 16) as i32;
            let stem = (std::path::Path::new(island.path).file_stem())
                .and_then(|s| s.to_str())
                .unwrap_or(island.path);
            // The lead's rule: an island keeps the text key a shipped world
            // gives it (every shipped `Name` is the file's stem), and a new
            // one gets a project key, whose texts delivery writes.
            let same = |p: &Placed| p.file.replace('\\', "/").eq_ignore_ascii_case(island.path);
            let name = match shipped.iter().find(|p| same(p)) {
                Some(p) => p.name.clone(),
                None if install.file(island.path).is_some() => stem.to_owned(),
                None => format!("{key}_{stem}"),
            };
            placed[slot] = Placed {
                file: island.path.to_owned(),
                name,
                x: was.x + inset,
                z: was.z + inset,
                direction: was.direction,
                side,
                kind: was.kind,
            };
            fill[slot] = Fill::Ours(n);
        }
        world::write(&mut doc, &placed)?;
        let config = doc.root("WorldConfig").context("no <WorldConfig>")?;
        let size = |name| match doc.child(config, name).map(|c| doc.value(c)) {
            Some(Value::Text(t)) => std::str::from_utf8(t).ok()?.parse::<u32>().ok(),
            _ => None,
        };
        let size = [size("Width").unwrap_or(0), size("Height").unwrap_or(0)];
        let classes = crate::SIZE_CLASSES;
        let slots = (placed.iter().zip(fill))
            .map(|(p, fill)| PlanSlot {
                position: [p.x.max(0) as u32, p.z.max(0) as u32],
                class: (classes.iter().position(|c| p.side <= c.slot))
                    .unwrap_or(classes.len().saturating_sub(1)) as u16,
                starter: p.kind == Kind::Start,
                fill,
            })
            .collect();
        let path = format!("{FOLDER}/{key}_editor_islands.www");
        Ok(vec![MapPlan {
            region: String::new(),
            // A world has no setup island size.
            island_size: "",
            size,
            slots,
            relabels_map_type: false,
            entry: MapEntry {
                path: path.clone(),
                region: String::new(),
                island_size: String::new(),
                source_guids: Vec::new(),
            },
            files: vec![(path, doc.encode())],
        }])
    }
}

/// Whether island `path` in `install` lies under water.
pub(crate) fn underwater(install: &Install, path: &str) -> Result<bool> {
    Ok(measure(&install.read(path)?)?.1)
}

/// An island file's side in metres, and whether it lies under water.
fn measure(bytes: &[u8]) -> Result<(u32, bool)> {
    let doc = Doc::parse(bytes)?;
    let side = number(&doc, "Width").context("no <Width>")?;
    Ok((side as u32, lies_under_water(&doc)))
}

/// An island file's `Width` and `Height`, metres.
pub(crate) fn sides(bytes: &[u8]) -> Result<[u32; 2]> {
    let doc = Doc::parse(bytes)?;
    let side = |name| number(&doc, name).with_context(|| format!("no <{name}>"));
    Ok([side("Width")? as u32, side("Height")? as u32])
}

/// Whether island `doc` lies under water, by its `SeaLevel`.
pub(crate) fn lies_under_water(doc: &Doc) -> bool {
    sea_level(doc) < UNDERWATER_BELOW
}

/// Island `doc`'s `SeaLevel`, metres: 0 above water.
pub(crate) fn sea_level(doc: &Doc) -> f64 {
    number(doc, "SeaLevel").unwrap_or(0.0)
}

fn number(doc: &Doc, name: &str) -> Option<f64> {
    match doc.root(name).map(|r| doc.value(r)) {
        Some(Value::Text(t)) => std::str::from_utf8(t).ok()?.parse::<f64>().ok(),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use anno_formats::isd::world::{self, Kind, Placed};
    use anno_formats::isd::{Doc, NodeId, Value};
    use anno_formats::vfs::{FileId, Install};

    fn install() -> Option<Install> {
        let Some(dir) = anno_formats::vfs::install_dir(crate::PROFILE.env_var) else {
            eprintln!("skip: ANNO2070_DIR unset");
            return None;
        };
        Some(Install::open(&dir, crate::ARCHIVE_FAMILIES).unwrap())
    }

    fn worlds(install: &Install) -> Vec<(String, Vec<u8>)> {
        (0..install.file_count())
            .map(|i| FileId(i as u32))
            .filter(|&id| install.file_path(id).ends_with(".www"))
            .map(|id| {
                let path = install.file_path(id).to_owned();
                (path, install.read_file(id).unwrap())
            })
            .collect()
    }

    // Every `IslandConfig` anywhere in the document, its fields looked up by
    // name: no knowledge of the k/v list the reader walks.
    fn walk(doc: &Doc) -> Vec<Placed> {
        let field = |c: NodeId, name: &str| -> String {
            let id = doc.children(c).find(|&n| doc.name(n) == name.as_bytes());
            match id.map(|n| doc.value(n)) {
                Some(Value::Text(t)) => String::from_utf8(t.to_vec()).unwrap(),
                Some(Value::Empty) => String::new(),
                other => panic!("{name}: {other:?}"),
            }
        };
        let mut out = Vec::new();
        let mut stack: Vec<NodeId> = doc.roots().collect();
        stack.reverse();
        while let Some(n) = stack.pop() {
            if doc.name(n) == b"IslandConfig" {
                let num = |name: &str| field(n, name).parse::<i64>().unwrap();
                out.push(Placed {
                    file: field(n, "Filename"),
                    name: field(n, "Name"),
                    x: num("PositionX") as i32,
                    z: num("PositionZ") as i32,
                    direction: num("Direction") as u8,
                    side: num("Width") as u32,
                    kind: match num("Type") {
                        0 => Kind::Regular,
                        1 => Kind::Underwater,
                        2 => Kind::Start,
                        3 => Kind::ThirdPartyUnderwater,
                        4 => Kind::ThirdPartySurface,
                        5 => Kind::Decoration,
                        other => panic!("Type {other}"),
                    },
                });
                continue;
            }
            let mut kids: Vec<NodeId> = doc.children(n).collect();
            kids.reverse();
            stack.extend(kids);
        }
        out
    }

    // Needs Anno 2070: every shipped world's islands read the same as a
    // plain walk finds them, and writing that list back changes no byte.
    #[test]
    fn every_shipped_world_reads_back_the_islands_it_places() {
        let Some(install) = install() else { return };
        let worlds = worlds(&install);
        assert_eq!(worlds.len(), 57);
        let mut placed = 0;
        for (path, bytes) in &worlds {
            let mut doc = Doc::parse(bytes).unwrap();
            let islands = world::read(&doc).unwrap_or_else(|e| panic!("{path}: {e:#}"));
            assert_eq!(islands, walk(&doc), "{path}");
            placed += islands.len();
            world::write(&mut doc, &islands).unwrap();
            assert!(
                doc.encode() == *bytes,
                "{path} changed by placing its own list"
            );
        }
        assert_eq!(placed, 1515);
    }

    // Needs Anno 2070: in every shipped world, swapping the middle island
    // for another changes the bytes of that entry only.
    #[test]
    fn swapping_an_island_in_a_shipped_world_changes_only_its_entry() {
        let Some(install) = install() else { return };
        for (path, bytes) in worlds(&install) {
            let mut doc = Doc::parse(&bytes).unwrap();
            let mut islands = world::read(&doc).unwrap();
            let n = islands.len() / 2;
            islands[n] = Placed {
                file: "data/levels/islands/normal/editor_test.isd".into(),
                name: "editor_test".into(),
                x: islands[n].x + 16,
                ..islands[n].clone()
            };
            world::write(&mut doc, &islands).unwrap();
            let written = doc.encode();
            assert_eq!(
                world::read(&Doc::parse(&written).unwrap()).unwrap(),
                islands
            );
            // The entry's bytes run from its n-th `<IslandConfig>` to the
            // close after it; the rest is compared as it was.
            let span = |b: &[u8]| {
                let open = b"<IslandConfig>";
                let close = b"</IslandConfig>\r\n";
                let start = (0..b.len())
                    .filter(|&i| b[i..].starts_with(open))
                    .nth(n)
                    .unwrap();
                let end = (start..b.len())
                    .find(|&i| b[i..].starts_with(close))
                    .unwrap()
                    + close.len();
                (start, end)
            };
            let ((s0, e0), (s1, e1)) = (span(&bytes), span(&written));
            assert!(bytes[..s0] == written[..s1], "{path}: before the entry");
            assert!(bytes[e0..] == written[e1..], "{path}: after the entry");
            assert!(bytes[s0..e0] != written[s1..e1]);
        }
    }

    // Needs Anno 2070: a project with a regular island, a start island, an
    // edited shipped one and one kept off the test map plans one world:
    // scenario 06's, with the three placed in slots that hold them and the
    // rest as shipped.
    #[test]
    fn a_plan_stages_one_world_that_places_the_project_islands() {
        use anno_core::map::{Fill, Maps};
        use anno_core::project::MapIsland;
        use std::collections::BTreeMap;

        let Some(install) = install() else { return };
        let read = |p: &str| install.read(p).unwrap();
        let none = BTreeMap::new();
        let island = |path, bytes: Option<Vec<u8>>, start, on_test_map| MapIsland {
            path,
            region: "",
            class: "",
            regular: true,
            start,
            on_test_map,
            slot_resources: &none,
            fertilities: None,
            info: None,
            file: bytes.map(Into::into),
        };
        let islands = [
            island(
                "data/levels/islands/normal/reef.isd",
                Some(read("data/levels/islands/normal/n_m02.isd")),
                false,
                true,
            ),
            island(
                "addondata/levels/islands/normal/haven.isd",
                Some(read("data/levels/islands/normal/n_l07.isd")),
                true,
                true,
            ),
            island(
                "data/levels/islands/normal/hidden.isd",
                Some(read("data/levels/islands/normal/n_s02.isd")),
                false,
                false,
            ),
            // An edited shipped island, read from the install.
            island("data/levels/Islands/normal/N_S04.isd", None, false, true),
        ];
        let maps = super::World;
        let plans = maps
            .plan(&install, None, &islands, "golden-isles", None)
            .unwrap();
        assert_eq!(plans.len(), 1);
        let plan = &plans[0];
        let path = "data/levels/testworlds/golden_isles_editor_islands.www";
        assert_eq!(plan.entry.path, path);
        assert_eq!(plan.files.len(), 1);
        assert_eq!(plan.files[0].0, path);
        assert_eq!(maps.register(&[], std::slice::from_ref(&plan.entry), 0), "");
        assert_eq!(plan.ours(), 3);
        assert!(plan.starts_on_ours());

        let bytes = &plan.files[0].1;
        let doc = Doc::parse(bytes).unwrap();
        assert!(doc.encode() == *bytes);
        let back = world::read(&doc).unwrap();
        let template = world::read(&Doc::parse(&read(super::TEMPLATE)).unwrap()).unwrap();
        assert_eq!(back.len(), template.len());
        assert_eq!(plan.slots.len(), back.len());
        let (mut seen, mut start) = (Vec::new(), None);
        for (n, (slot, (p, was))) in plan
            .slots
            .iter()
            .zip(back.iter().zip(&template))
            .enumerate()
        {
            let Fill::Ours(i) = slot.fill else {
                assert_eq!(p, was, "entry {n} stays as shipped");
                continue;
            };
            let side = super::measure(&islands[i].file(&install).unwrap())
                .unwrap()
                .0;
            assert_eq!(p.file, islands[i].path);
            assert_eq!(p.side, side);
            assert!(was.side >= side, "{}: slot {n} holds it", p.file);
            assert!(p.x >= was.x && p.x + side as i32 <= was.x + was.side as i32);
            assert!(p.z >= was.z && p.z + side as i32 <= was.z + was.side as i32);
            assert_eq!(p.direction, was.direction);
            assert_eq!(p.kind, was.kind);
            if p.kind == world::Kind::Start {
                start = Some(i);
            }
            seen.push((i, p.name.as_str()));
        }
        seen.sort();
        assert_eq!(
            seen,
            [
                (0, "golden_isles_reef"),
                (1, "golden_isles_haven"),
                (3, "N_S04")
            ]
        );
        assert_eq!(start, Some(1), "the start island takes a start slot");
    }

    // Needs Anno 2070: every shipped world, read as a map and written back
    // unedited, gives its shipped bytes; the map's slots are its entries
    // and its starts its ark slots.
    #[test]
    fn every_shipped_map_round_trips_through_the_model() {
        use anno_core::map::Maps;
        let Some(install) = install() else { return };
        let maps = super::World;
        let shipped = maps.shipped(&install, None);
        assert_eq!(shipped.len(), 57);
        assert_eq!(shipped[0].path, super::TEMPLATE);
        for (path, bytes) in worlds(&install) {
            let map = maps.read(&install, &path).unwrap();
            let files = maps.write(&install, &map, "round_trip").unwrap();
            assert_eq!(files.len(), 1);
            assert!(files[0].1 == bytes, "{path} written as shipped");
            let doc = Doc::parse(&bytes).unwrap();
            let placed = walk(&doc);
            let v = &map.variants[0];
            assert_eq!(v.slots.len(), placed.len(), "{path}");
            for (s, p) in v.slots.iter().zip(&placed) {
                assert_eq!(
                    (s.at, map.island_of(s), s.turns, s.raw_type),
                    (
                        [p.x, p.z],
                        Some(p.file.as_str()),
                        p.direction,
                        p.kind as u16
                    ),
                    "{path}"
                );
            }
            assert_eq!(v.starts, world::arks(&doc).unwrap());
        }
    }

    // Needs Anno 2070: what the shipped scenario and campaign worlds
    // measure, pinned in `MAP_RULES`, and every slot on the 16 m grid.
    // Test, benchmark, DLC and world-event worlds aren't measured.
    #[test]
    fn map_census() {
        use anno_core::map::{self, IslandFacts, Maps};
        let Some(install) = install() else { return };
        let maps = super::World;
        let facts = |path: &str| {
            let (side, underwater) = super::measure(&install.read(path).ok()?).ok()?;
            Some(IslandFacts {
                side,
                regular: true,
                start: true,
                underwater,
                broken: false,
            })
        };
        let refs = anno_core::game::Refs {
            install: &install,
            project: &|_| false,
            assets: None,
        };
        let (mut starts, mut gap, mut edge) = (usize::MAX, i32::MAX, i32::MAX);
        for s in maps.shipped(&install, None) {
            let m = maps.read(&install, &s.path).unwrap();
            for slot in &m.variants[0].slots {
                assert_eq!(slot.at.map(|p| p % 16), [0, 0], "{}", s.path);
            }
            let measured = map::measure(&super::MAP_RULES, &m, &facts);
            eprintln!("{}: {measured:?}", s.path);
            // The sudoku world is a puzzle: one island, no start.
            let played = s.path.contains("/scenarios/") || s.path.contains("/campaign/");
            if !played || s.name == "sudoku" {
                continue;
            }
            (starts, gap, edge) = (
                starts.min(measured.starts),
                gap.min(measured.gap),
                edge.min(measured.edge),
            );
            let problems: Vec<_> = (map::check(&super::MAP_RULES, &m, &refs, &facts).into_iter())
                .filter(|p| p.level == anno_core::island::Level::Broken)
                .collect();
            let slots = &m.variants[0].slots;
            let named: Vec<String> = (problems.iter())
                .flat_map(|p| p.items.iter().map(move |&i| (p, i as usize)))
                .map(|(p, i)| {
                    let slot = &slots[i & 0xffff];
                    let island = m.island_of(slot).unwrap_or("pool");
                    format!("{}: {:?} {island} {:?}", p.what, slot.role, facts(island))
                })
                .collect();
            assert!(problems.is_empty(), "{}: {named:#?}", s.path);
        }
        eprintln!("census: starts {starts}, gap {gap}, edge {edge}");
        let r = &super::MAP_RULES;
        assert_eq!(
            (usize::from(r.min_starts), r.min_gap, r.min_edge),
            (starts, gap, edge)
        );
    }

    // Needs Anno 2070: a new world on scenario 06 keeps its ark slots and
    // places what it is given, each entry with a shipped entry's fields,
    // and moves an ark.
    #[test]
    fn a_new_world_places_its_slots_like_a_shipped_one() {
        use anno_core::map::{Maps, Role, Slot};
        let Some(install) = install() else { return };
        let maps = super::World;
        let mut map = maps
            .new_map(&install, super::TEMPLATE, "Ring of Gold")
            .unwrap();
        let shipped = Doc::parse(&install.read(super::TEMPLATE).unwrap()).unwrap();
        let arks = world::arks(&shipped).unwrap();
        assert!(map.variants[0].slots.is_empty());
        assert_eq!(map.variants[0].starts, arks);
        let start = map.island("data/levels/islands/normal/n_l07.isd");
        let under = map.island("data/levels/islands/underwater/u_s06.isd");
        let slot = |at, role, island, turns| Slot {
            at,
            island,
            role,
            class: 2,
            turns,
            raw_type: 0,
            source: Slot::NEW,
        };
        let v = &mut map.variants[0];
        v.slots = vec![
            slot([320, 320], Role::Start, start, 1),
            slot([960, 640], Role::Underwater, under, 0),
        ];
        v.starts[0] = [400, 400];
        let files = maps.write(&install, &map, "ring_of_gold").unwrap();
        assert_eq!(files[0].0, "data/levels/testworlds/ring_of_gold.www");
        let doc = Doc::parse(&files[0].1).unwrap();
        let placed = world::read(&doc).unwrap();
        let want: Vec<(i32, i32, u8, Kind, &str)> = vec![
            (320, 320, 1, Kind::Start, "n_l07"),
            (960, 640, 0, Kind::Underwater, "u_s06"),
        ];
        let got: Vec<(i32, i32, u8, Kind, &str)> = placed
            .iter()
            .map(|p| (p.x, p.z, p.direction, p.kind, p.name.as_str()))
            .collect();
        assert_eq!(got, want);
        let mut moved = arks.clone();
        moved[0] = [400, 400];
        assert_eq!(world::arks(&doc).unwrap(), moved);
        // Field names of each new entry, as a shipped entry has them.
        // The n-th `IslandConfig` in document order.
        let fields = |doc: &Doc, n: usize| -> Vec<Vec<u8>> {
            let mut stack: Vec<NodeId> = doc.roots().collect();
            stack.reverse();
            let mut configs = Vec::new();
            while let Some(id) = stack.pop() {
                if doc.name(id) == b"IslandConfig" {
                    configs.push(id);
                    continue;
                }
                let mut kids: Vec<NodeId> = doc.children(id).collect();
                kids.reverse();
                stack.extend(kids);
            }
            doc.children(configs[n])
                .map(|c| doc.name(c).to_vec())
                .collect()
        };
        let shape = fields(&shipped, 0);
        assert_eq!(fields(&doc, 0), shape);
        assert_eq!(fields(&doc, 1), shape);
    }

    // Needs Anno 2070: an island seen from above has land in the middle
    // and clear sea at the corners; an underwater one shows its plateau.
    #[test]
    fn top_views_show_land_and_clear_sea() {
        let Some(install) = install() else { return };
        for path in [
            "data/levels/islands/normal/n_l07.isd",
            "data/levels/islands/underwater/u_s06.isd",
        ] {
            let view = crate::island::top_view(&install, path, 64).unwrap();
            assert_eq!(
                (view.width, view.height, view.rgba.len()),
                (64, 64, 64 * 64 * 4)
            );
            let alpha = |x: usize, y: usize| view.rgba[(y * 64 + x) * 4 + 3];
            assert_eq!(alpha(0, 0), 0, "{path}: a corner is clear sea");
            let opaque = (0..64 * 64).filter(|n| view.rgba[n * 4 + 3] == 255).count();
            assert!(opaque > 64 * 64 / 10, "{path}: {opaque} land texels");
        }
    }
}
