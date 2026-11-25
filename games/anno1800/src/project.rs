// Owns how a 1800 project reaches the game: its mod loader's packaging, its
// maps, and Test in game's maps. 1800's map types are an engine enum, so a
// mod can't add one: layouts and test maps are copies of shipped map
// templates, and the shipped map assets are redirected to them.

use anno_core::fertility;
use anno_core::map::{
    self, LayoutEntry, Map, MapRules, Maps, PlaysAs, Registers, Role, Setup, ShippedMap, Variant,
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
    // The 1800 manifest has no `Difficulty`.
    difficulties: &[],
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
/// The Old World: the one region whose maps come in every setup island
/// size, as its Archipelago map type. The others have one map each.
const OLD_WORLD: &str = "Moderate";
/// The shipped maps a copy may start from, by setup island size.
const VARIANTS: [(&str, &str); 3] = [("easy", "Large"), ("medium", "Medium"), ("hard", "Small")];
/// The smallest class that may take a starter slot: the shipped starters
/// are all Large.
const STARTER_CLASS: u16 = 2;
/// Test maps and layouts go to `{0}{region}{1}/{name}/{name}.a7t` (and
/// `.a7tinfo`, `.a7te`).
const CUSTOM_MAPS: [&str; 2] = ["data/sessions/maps/custom/", ""];
/// The letters a pool map's file stem says its setup with
/// (`moderate_archipel_ls_01`: a Large world of Small islands;
/// `colony01_m_02`: a Medium world, any island size), and the setup words.
const SIZE_LETTERS: [(char, &str); 3] = [('s', "Small"), ('m', "Medium"), ('l', "Large")];

const fn setup(world_size: &'static str, island_size: &'static str) -> Setup {
    Setup {
        island_size,
        world_size,
    }
}

pub(crate) static MAP_RULES: MapRules = MapRules {
    // The Old World's: every world size with every island size.
    setups: &[
        setup("Large", "Large"),
        setup("Large", "Medium"),
        setup("Large", "Small"),
        setup("Medium", "Large"),
        setup("Medium", "Medium"),
        setup("Medium", "Small"),
        setup("Small", "Large"),
        setup("Small", "Medium"),
        setup("Small", "Small"),
    ],
    // Pool `IslandType` indices (map_census): 2 is on decoration islands,
    // 3 on the third parties' and traders'. 4 is on random small slots,
    // a pirate and Arctic scenario islands, 5 on Arctic and scenario
    // islands: they show as their number.
    roles: &[
        (Role::Normal, 0),
        (Role::Start, 1),
        (Role::Decoration, 2),
        (Role::ThirdParty, 3),
    ],
    size_classes: crate::SIZE_CLASSES,
    grid: 8,
    fixed_turns: true,
    pool_slots: true,
    free_starts: false,
    // The least any pool template an asset names has (map_census):
    // footprints overlap by up to 40 m, and stay 104 m off the edge.
    min_starts: 4,
    min_gap: -40,
    min_edge: 104,
    layout_folder: CUSTOM_MAPS,
    registers: Registers::ReplacesShipped,
};

/// A template path's map: the files that differ only in their size token
/// (`moderate_archipel_ls_01` is one of `moderate_archipel_01`'s nine).
struct Family<'a> {
    stem: &'a str,
    /// The folder's name, mostly the file's (not `moderate_continental_01`'s).
    dir: &'a str,
    /// The folder the variants' folders sit in.
    parent: &'a str,
    /// The file name's words, split at `_`.
    words: Vec<&'a str>,
    /// Which word is the size token; none for a map of one file.
    at: Option<usize>,
}

impl<'a> Family<'a> {
    fn of(path: &'a str) -> Option<Family<'a>> {
        let stem = [".a7tinfo", ".a7te", ".a7t"]
            .iter()
            .find_map(|ext| path.strip_suffix(ext))
            .unwrap_or(path);
        let (dir, file) = stem.rsplit_once('/')?;
        let (parent, dir) = dir.rsplit_once('/').unwrap_or(("", dir));
        let words: Vec<&str> = file.split('_').collect();
        let at = (words.iter().skip(1))
            .rposition(|w| Self::setup_of(w).is_some())
            .map(|i| i + 1);
        Some(Family {
            stem,
            dir,
            parent,
            words,
            at,
        })
    }

    // `ls` → Large world, Small islands; `m` → Medium world, every island
    // size.
    fn setup_of(token: &str) -> Option<Setup> {
        let word = |c| SIZE_LETTERS.iter().find(|(l, _)| *l == c).map(|(_, w)| *w);
        let mut chars = token.chars();
        let world = word(chars.next()?)?;
        let island = match chars.next() {
            Some(c) => word(c)?,
            None => "",
        };
        chars.next().is_none().then_some(setup(world, island))
    }

    fn token(setup: Setup) -> String {
        let letter = |w: &str| SIZE_LETTERS.iter().find(|(_, x)| *x == w).map(|(l, _)| *l);
        letter(setup.world_size)
            .into_iter()
            .chain(letter(setup.island_size))
            .collect()
    }

    fn name(&self) -> String {
        (self.words.iter().enumerate())
            .filter(|&(i, _)| Some(i) != self.at)
            .map(|(_, w)| *w)
            .collect::<Vec<_>>()
            .join("_")
    }

    fn setup(&self) -> Setup {
        self.at
            .and_then(|i| Self::setup_of(self.words[i]))
            .unwrap_or(Setup::NONE)
    }

    /// The stem of the variant answering `setup`.
    fn stem(&self, setup: Setup) -> String {
        let Some(i) = self.at else {
            return self.stem.to_owned();
        };
        let token = Self::token(setup);
        // The token swapped in the file's name, and in the folder's where
        // it stands at the same word.
        let swap = |name: &str| -> String {
            let mut words: Vec<&str> = name.split('_').collect();
            if words.get(i) == Some(&self.words[i]) {
                words[i] = &token;
            }
            words.join("_")
        };
        format!(
            "{}/{}/{}",
            self.parent,
            swap(self.dir),
            swap(&self.words.join("_"))
        )
    }

    /// The setups the family's variants may answer: the token's length
    /// says whether island sizes count.
    fn setups(&self) -> Vec<Setup> {
        let Some(i) = self.at else {
            return vec![Setup::NONE];
        };
        let both = self.words[i].len() == 2;
        let mut out = Vec::new();
        // Largest first, as `MAP_RULES.setups`.
        for &(_, world) in SIZE_LETTERS.iter().rev() {
            for &(_, island) in SIZE_LETTERS.iter().rev() {
                out.push(setup(world, if both { island } else { "" }));
                if !both {
                    break;
                }
            }
        }
        out
    }
}

// A map's region where no asset names it: the folder its maps sit in.
fn region_of(assets: &Assets, a7t: &str) -> Option<String> {
    (assets.iter())
        .filter_map(|a| a.map.as_ref())
        .find(|m| m.path.eq_ignore_ascii_case(a7t))
        .map(|m| m.region.clone())
        .filter(|r| !r.is_empty())
}

fn load_assets(install: &Install) -> Result<Assets> {
    Assets::load(install, &crate::assets::SCHEMA)
}

/// `path`'s map; its variants replace the assets of `assets` that name
/// their files.
fn read(install: &Install, path: &str, assets: Option<&Assets>) -> Result<Map> {
    let f = Family::of(path).with_context(|| format!("{path}: not a template path"))?;
    let stems: Vec<(String, Setup)> = (f.setups().into_iter())
        .map(|s| (f.stem(s), s))
        .filter(|(stem, _)| install.exists(&format!("{stem}.a7tinfo")))
        .collect();
    let replaces = |stem: &str| -> Vec<u32> {
        let a7t = format!("{stem}.a7t");
        (assets.into_iter().flat_map(|a| a.iter()))
            .filter(|a| {
                a.map
                    .as_ref()
                    .is_some_and(|m| m.path.eq_ignore_ascii_case(&a7t))
            })
            .map(|a| a.guid)
            .collect()
    };
    let region = assets
        .and_then(|a| region_of(a, &format!("{}.a7t", stems.first()?.0)))
        .unwrap_or_else(|| {
            // A layout sits in its region's folder.
            let folder = f.parent.rsplit('/').next().unwrap_or_default();
            let mut rest = folder.chars();
            rest.next().map_or(String::new(), |first| {
                first.to_ascii_uppercase().to_string() + rest.as_str()
            })
        });
    let variants = (stems.into_iter())
        .map(|(stem, setup)| {
            let guids = replaces(&stem);
            (stem, setup, guids)
        })
        .collect();
    map::read_templates(install, &MAP_RULES, &f.name(), &region, variants)
}

pub(crate) struct Redirect;

impl Maps for Redirect {
    fn rules(&self) -> &'static MapRules {
        &MAP_RULES
    }

    /// Every template a `MapTemplate` asset names, one entry per map: the
    /// world and island sizes of one map type are one.
    fn shipped(&self, install: &Install, assets: Option<&Assets>) -> Vec<ShippedMap> {
        let loaded;
        let assets = match assets {
            Some(a) => a,
            None => match load_assets(install) {
                Ok(a) => {
                    loaded = a;
                    &loaded
                }
                Err(_) => return Vec::new(),
            },
        };
        let mut paths: Vec<(&str, &str)> = (assets.iter())
            .filter_map(|a| a.map.as_ref())
            .filter(|m| {
                m.path.ends_with(".a7t") && install.file(&format!("{}info", m.path)).is_some()
            })
            .map(|m| (m.path.as_str(), m.region.as_str()))
            .collect();
        paths.sort();
        paths.dedup_by(|a, b| a.0 == b.0);
        let mut out: Vec<ShippedMap> = Vec::new();
        for (path, region) in paths {
            let Some(f) = Family::of(path) else { continue };
            let name = f.name();
            if let Some(m) = (out.iter_mut()).find(|m| {
                m.name == name && Family::of(&m.path).is_some_and(|g| g.parent == f.parent)
            }) {
                m.setups.push(f.setup());
                continue;
            }
            let Ok(t) = install
                .read(format!("{path}info"))
                .and_then(|b| MapTemplate::parse(&b))
            else {
                continue;
            };
            let group = ["pool", "campaign"]
                .into_iter()
                .find(|g| path.contains(&format!("/{g}/")))
                .unwrap_or("other");
            out.push(ShippedMap {
                path: path.to_owned(),
                name,
                region: region.to_owned(),
                group: group.to_owned(),
                size: t.size,
                setups: vec![f.setup()],
                // DLC 12's enlarged copies are the engine's to pick; a
                // layout writes the plain size.
                base: group == "pool" && !path.contains("enlarged"),
            });
        }
        // In the order `read` gives the variants.
        for m in &mut out {
            if let Some(order) = Family::of(&m.path).map(|f| f.setups()) {
                m.setups.sort_by_key(|s| order.iter().position(|o| o == s));
            }
        }
        out
    }

    /// A shipped map's variants each replace the assets naming its file;
    /// a project layout's say none here, its settings keep them.
    fn read(&self, install: &Install, path: &str) -> Result<Map> {
        let assets = match path.starts_with(CUSTOM_MAPS[0]) {
            true => None,
            false => Some(load_assets(install)?),
        };
        read(install, path, assets.as_ref())
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
        let [before, after] = CUSTOM_MAPS;
        let region = map.region.to_ascii_lowercase();
        map::write_templates(
            install,
            &MAP_RULES,
            map,
            |v| {
                let token = Family::token(v.setup);
                let folder = match token.is_empty() {
                    true => folder_name.to_owned(),
                    false => format!("{folder_name}_{token}"),
                };
                format!("{before}{region}{after}/{folder}/{folder}")
            },
            Mines::Mapping(&[]),
        )
    }

    /// A new layout keeps the base map's variants, and the assets they
    /// replace: it plays as the map it was made on until told otherwise.
    fn new_map(&self, install: &Install, base: &str, name: &str) -> Result<Map> {
        let map = self.read(install, base)?;
        let setups: Vec<Setup> = map.variants.iter().map(|v| v.setup).collect();
        map.emptied(name, &setups)
    }

    /// The `MapTemplate` assets of the map's region and the variant's
    /// setup, pool maps first. The variant's base sea fixes its world size
    /// and the region's look, and its slot classes were made for its
    /// island size, so a map of another size or region isn't offered;
    /// `register` would redirect any GUID listed.
    fn plays_as(&self, assets: &Assets, map: &Map, variant: &Variant) -> Vec<PlaysAs> {
        let setup = variant.setup;
        // An asset that leaves a size out takes the template's, Small:
        // shipped Small-world assets (moderate_archipel_sl_01's) write no
        // `TemplateSize`.
        let size = |w: &str| if w.is_empty() { "Small" } else { w }.to_owned();
        let fits = |want: &str, has: &str| want.is_empty() || size(has) == want;
        let mut out: Vec<(bool, PlaysAs)> = (assets.iter())
            .filter_map(|a| Some((a, a.map.as_ref()?)))
            .filter(|(_, m)| {
                m.path.ends_with(".a7t")
                    && m.region.eq_ignore_ascii_case(&map.region)
                    && fits(setup.world_size, &m.size)
                    && fits(setup.island_size, &m.island_size)
            })
            .map(|(a, m)| {
                let stem = m.path.rsplit('/').next().unwrap_or(&m.path);
                let kind = match m.map_type.is_empty() {
                    true => a.name.as_str(),
                    false => m.map_type.as_str(),
                };
                let label = format!("{kind} · {}", stem.trim_end_matches(".a7t"));
                (
                    !m.path.contains("/pool/"),
                    PlaysAs {
                        guid: a.guid,
                        label,
                    },
                )
            })
            .collect();
        out.sort_by(|a, b| (a.0, &a.1.label, a.1.guid).cmp(&(b.0, &b.1.label, b.1.guid)));
        out.into_iter().map(|(_, p)| p).collect()
    }

    /// Each redirected map asset's templates point at its copy, a test
    /// map's or a layout variant's, and with an Old World test map the
    /// Archipelago type shows our label.
    fn register(&self, layouts: &[LayoutEntry], tests: &[MapEntry], test_guid: u32) -> String {
        let mut xml = String::new();
        // A map without the GUIDs it replaces redirects nothing; `plan`
        // always records them.
        let variants = layouts.iter().flat_map(|l| &l.variants);
        for map in tests
            .iter()
            .chain(variants)
            .filter(|m| !m.source_guids.is_empty())
        {
            let ids = (map.source_guids.iter())
                .map(u32::to_string)
                .collect::<Vec<_>>()
                .join(",");
            xml += &format!(
                "  <ModOp Type=\"merge\" GUID=\"{ids}\" Path=\"/Values/MapTemplate\">\n    <TemplateFilename>{}</TemplateFilename>\n    <EnlargedTemplateFilename>{}</EnlargedTemplateFilename>\n  </ModOp>\n",
                map.path, map.path
            );
        }
        if tests.iter().any(|m| m.region == OLD_WORLD) {
            xml += &format!(
                "  <ModOp Type=\"replace\" GUID=\"{DIFFICULTY_GUID}\" Path=\"/Values/DifficultySettings/MapType/Archipelago/ValueName\"><ValueName>{test_guid}</ValueName></ModOp>\n"
            );
        }
        xml
    }

    // Redirected maps are found, and their slots checked, in assets.xml, so
    // planning loads it when the editor has none yet.
    fn plan(
        &self,
        install: &Install,
        assets: Option<&Assets>,
        islands: &[MapIsland],
        mod_id: &str,
        on: Option<&Map>,
    ) -> Result<Vec<MapPlan>> {
        let loaded;
        let assets = match assets {
            Some(a) => a,
            None => {
                loaded = load_assets(install)?;
                &loaded
            }
        };
        // (template, base stem, region, island size, variant, replaced, relabels)
        let mut copies = Vec::new();
        match on {
            Some(map) => {
                let name = map
                    .name
                    .to_ascii_lowercase()
                    .replace(|c: char| !c.is_ascii_alphanumeric(), "_");
                for v in &map.variants {
                    let template = map::template_of(&MAP_RULES, map, v, Mines::Mapping(&[]))?;
                    let variant = format!("{name}_{}", Family::token(v.setup));
                    copies.push((
                        template,
                        v.base.clone(),
                        map.region.clone(),
                        v.setup.island_size,
                        variant,
                        v.replaces.clone(),
                        false,
                    ));
                }
            }
            None => {
                for region in regions(islands) {
                    for (variant, island_size) in VARIANTS {
                        if region != OLD_WORLD && island_size != "Medium" {
                            continue;
                        }
                        let shipped: Vec<_> = (assets.iter())
                            .filter_map(|a| a.map.as_ref().map(|m| (a.guid, m)))
                            .filter(|(_, m)| {
                                m.region == region
                                    && !m.path.contains("/scenario")
                                    // The New World's other maps are its campaign's.
                                    && (region != "Colony01" || m.path.contains("/pool/"))
                                    && (region != OLD_WORLD
                                        || (m.map_type == "Archipelago" && m.island_size == island_size))
                            })
                            .collect();
                        let base = (shipped.iter())
                            .find(|(_, m)| m.size == "Large")
                            .or_else(|| shipped.first())
                            .with_context(|| format!("no shipped {region} test-map template"))?
                            .1
                            .path
                            .trim_end_matches(".a7t")
                            .to_owned();
                        let path = format!("{base}.a7tinfo");
                        let template = MapTemplate::parse(
                            &install
                                .read(&path)
                                .with_context(|| format!("Reading {path}"))?,
                        )?;
                        let guids = shipped.iter().map(|(g, _)| *g).collect();
                        copies.push((
                            template,
                            base,
                            region.to_owned(),
                            island_size,
                            variant.to_owned(),
                            guids,
                            region == OLD_WORLD,
                        ));
                    }
                }
            }
        }
        let mut out = Vec::new();
        for (template, base, region, island_size, variant, source_guids, relabels_map_type) in
            copies
        {
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
                source_guids,
                relabels_map_type,
            };
            out.push(
                copy.plan(install, islands, &candidates, mod_id, |t, n, island, _| {
                    fix(install, assets, t, n, island)
                })?,
            );
        }
        Ok(out)
    }
}

/// Writes `island` into slot `n`: its random slots get the assigned
/// deposits (else the first the slot allows), checked against the slot
/// assets, and a fixed fertility list replaces the generator's when set.
fn fix(
    install: &Install,
    assets: &Assets,
    template: &mut MapTemplate,
    n: usize,
    island: &MapIsland,
) -> Result<()> {
    let mut mapping = Vec::new();
    if !island.slot_resources.is_empty() {
        let info = A7mInfo::parse(&island.info(install)?)?;
        for group in info.random_slots() {
            if !(assets.get(group.guid)).is_some_and(|a| a.slot_type.as_deref() == Some("Random")) {
                continue;
            }
            let replacements = assets.slot_replacements(group.guid, island.region);
            for object in group.objects {
                let chosen = (island.slot_resources.get(&object.object_id).copied())
                    .or_else(|| replacements.first().map(|a| a.guid))
                    .unwrap_or(0);
                ensure!(
                    chosen == 0 || replacements.iter().any(|a| a.guid == chosen),
                    "slot {}: resource {chosen} is not a replacement for {}",
                    object.object_id,
                    group.guid
                );
                mapping.push((object.object_id, chosen));
            }
        }
    }
    if let Some(fertilities) = island.fertilities {
        let allowed = fertility::choices(assets, island.region);
        for guid in fertilities {
            ensure!(
                allowed.iter().any(|f| f.guid == *guid),
                "fertility {guid} is not available in {}",
                island.region
            );
        }
    }
    template.fix(n, island.path, island.fertilities, Mines::Mapping(&mapping))
}

#[cfg(test)]
mod tests {
    use super::{MAP_RULES, Redirect};
    use anno_core::map::{self, IslandFacts, LayoutEntry, Map, Maps, Role, Slot};
    use anno_core::project::MapEntry;
    use anno_formats::a7m::a7minfo::A7mInfo;
    use anno_formats::a7m::template::{MapTemplate, SlotKind};
    use anno_formats::assets::Assets;
    use anno_formats::vfs::Install;
    use std::collections::BTreeMap;

    fn install() -> Option<(Install, Assets)> {
        let Some(dir) = anno_formats::vfs::install_dir("ANNO1800_DIR") else {
            eprintln!("skip: ANNO1800_DIR unset");
            return None;
        };
        let install = Install::open(dir, crate::ARCHIVE_FAMILIES).unwrap();
        let assets = super::load_assets(&install).unwrap();
        Some((install, assets))
    }

    // Every shipped template's map, once each.
    fn every_map(install: &Install, assets: &Assets) -> Vec<Map> {
        let mut paths: Vec<String> = (install.search(".a7tinfo", 2000).into_iter())
            .map(|id| install.file_path(id).to_owned())
            .collect();
        paths.sort();
        let mut seen: Vec<String> = Vec::new();
        let mut out = Vec::new();
        for path in paths {
            if seen.iter().any(|b| path.starts_with(&format!("{b}."))) {
                continue;
            }
            let m = super::read(install, &path, Some(assets)).unwrap();
            seen.extend(m.variants.iter().map(|v| v.base.clone()));
            out.push(m);
        }
        out
    }

    // Needs Anno 1800: every shipped template, read as a map and written
    // back unedited, gives its shipped bytes; the map's slots are the
    // template's, and the variants an asset names replace it.
    #[test]
    fn every_shipped_map_round_trips_through_the_model() {
        let Some((install, assets)) = install() else {
            return;
        };
        let maps = Redirect;
        let all = every_map(&install, &assets);
        let mut bases = Vec::new();
        for m in &all {
            let files = maps.write(&install, m, "round_trip").unwrap();
            let mut files = files.iter();
            for v in &m.variants {
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
                let t = MapTemplate::parse(&v.source).unwrap();
                assert_eq!(t.slots.len(), v.slots.len());
                for (n, (a, b)) in t.slots.iter().zip(&v.slots).enumerate() {
                    assert_eq!(a.position.map(|p| p as i32), b.at, "{} slot {n}", v.base);
                    match &a.kind {
                        SlotKind::Random { size, island_type } => {
                            assert_eq!(
                                (b.island, b.class as u16, b.raw_type),
                                (Slot::POOL, *size, *island_type)
                            );
                        }
                        SlotKind::Fixed { path, island_type } => {
                            assert_eq!(m.island_of(b), Some(path.as_str()));
                            assert_eq!((b.turns, b.raw_type), (a.turns, *island_type));
                        }
                        SlotKind::Other(_) => assert_eq!(b.role, Role::Marker),
                    }
                }
                let a7t = format!("{}.a7t", v.base);
                let named = (assets.iter()).any(|a| {
                    a.map
                        .as_ref()
                        .is_some_and(|x| x.path.eq_ignore_ascii_case(&a7t))
                });
                assert_eq!(named, !v.replaces.is_empty(), "{a7t}");
                bases.push(v.base.clone());
            }
        }
        bases.sort();
        let mut want: Vec<String> = (install.search(".a7tinfo", 2000).into_iter())
            .map(|id| {
                install
                    .file_path(id)
                    .trim_end_matches(".a7tinfo")
                    .to_owned()
            })
            .collect();
        want.sort();
        assert_eq!(bases, want);
        // The Old World's Archipelago: nine variants, one per setup.
        let archipelago = all
            .iter()
            .find(|m| m.name == "moderate_archipel_01")
            .unwrap();
        assert_eq!(archipelago.variants.len(), MAP_RULES.setups.len());
    }

    // Needs Anno 1800: the shipped pool maps set `MAP_RULES`' census values
    // and pass every check; prints what the type ids sit on.
    #[test]
    fn map_census() {
        let Some((install, assets)) = install() else {
            return;
        };
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
        for m in every_map(&install, &assets) {
            for v in &m.variants {
                let t = MapTemplate::parse(&v.source).unwrap();
                *third_parties.entry(t.third_parties).or_insert(0) += 1;
                for slot in &t.slots {
                    *difficulty.entry(slot.difficulty).or_insert(0) += 1;
                    let key = match &slot.kind {
                        SlotKind::Fixed { path, island_type } => (
                            *island_type,
                            path.rsplit('/').next().unwrap_or_default().to_owned(),
                        ),
                        SlotKind::Random { size, island_type } => {
                            (*island_type, format!("random size {size}"))
                        }
                        SlotKind::Other(_) => continue,
                    };
                    *ids.entry(key).or_insert(0) += 1;
                }
            }
            // The pool maps the game plays: an asset names each variant.
            // `moderate_ss_01` and its kin, which none names, have fewer
            // starts and a slot off the map.
            let played = |v: &map::Variant| {
                v.base.contains("/pool/") && !v.base.contains("enlarged") && !v.replaces.is_empty()
            };
            if !m.variants.iter().all(played) {
                continue;
            }
            for v in &m.variants {
                for slot in &v.slots {
                    assert_eq!(
                        slot.at.map(|p| p % MAP_RULES.grid as i32),
                        [0, 0],
                        "{} on the grid",
                        v.base
                    );
                }
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
            assert!(problems.is_empty(), "{}: {problems:?}", m.name);
        }
        eprintln!("census: starts {starts}, gap {gap}, edge {edge}");
        eprintln!(
            "difficulty ids {difficulty:?}; RandomlyPlacedThirdParties entries {third_parties:?}"
        );
        for ((id, on), n) in &ids {
            eprintln!("type {id} on {on}: {n}");
        }
        assert_eq!(
            (
                usize::from(MAP_RULES.min_starts),
                MAP_RULES.min_gap,
                MAP_RULES.min_edge
            ),
            (starts, gap, edge)
        );
    }

    // Needs Anno 1800: a new layout on the Old World's Large Archipelago
    // keeps its variants and the assets they replace, writes templates that
    // read back as set, and registers as merges naming exactly those
    // assets.
    #[test]
    fn a_new_layout_replaces_the_maps_it_was_made_on() {
        let Some((install, _)) = install() else {
            return;
        };
        let maps = Redirect;
        let base =
            "data/sessions/maps/pool/moderate/moderate_archipel_ll_01/moderate_archipel_ll_01.a7t";
        let mut map = maps.new_map(&install, base, "Ring of Gold").unwrap();
        assert_eq!(map.region, "Moderate");
        assert_eq!(map.variants.len(), 9);
        assert!(
            map.variants
                .iter()
                .all(|v| v.slots.is_empty() && !v.replaces.is_empty())
        );
        for v in &mut map.variants {
            v.slots = vec![Slot {
                at: [400, 400],
                island: Slot::POOL,
                role: Role::Start,
                class: 2,
                turns: 0,
                raw_type: 0,
                source: Slot::NEW,
            }];
        }
        let files = maps.write(&install, &map, "ring_of_gold").unwrap();
        let ll = (map.variants.iter().position(|v| v.base.ends_with("_ll_01"))).unwrap();
        assert_eq!(
            files[3 * ll].0,
            "data/sessions/maps/custom/moderate/ring_of_gold_ll/ring_of_gold_ll.a7tinfo"
        );
        for (v, files) in map.variants.iter().zip(files.chunks(3)) {
            let t = MapTemplate::parse(&files[0].1).unwrap();
            assert_eq!(t.slots.len(), 1);
            assert_eq!(
                t.slots[0].kind,
                SlotKind::Random {
                    size: 2,
                    island_type: 1
                }
            );
            assert!(files[1].1 == install.read(format!("{}.a7t", v.base)).unwrap());
        }
        let entry = LayoutEntry {
            name: map.name.clone(),
            path: files[1].0.clone(),
            region: map.region.clone(),
            variants: (map.variants.iter().zip(files.chunks(3)))
                .map(|(v, f)| MapEntry {
                    path: f[1].0.clone(),
                    region: map.region.clone(),
                    island_size: v.setup.island_size.into(),
                    source_guids: v.replaces.clone(),
                })
                .collect(),
            guids: Vec::new(),
            fill_with_islands: false,
        };
        let xml = maps.register(std::slice::from_ref(&entry), &[], 0);
        let mut named: Vec<u32> = (xml.split("GUID=\"").skip(1))
            .flat_map(|rest| rest[..rest.find('"').unwrap()].split(','))
            .map(|g| g.parse().unwrap())
            .collect();
        named.sort();
        let mut want: Vec<u32> = map
            .variants
            .iter()
            .flat_map(|v| v.replaces.clone())
            .collect();
        want.sort();
        assert_eq!(named, want);
        assert!(
            !xml.contains("Archipelago"),
            "a layout leaves the map type's label"
        );
    }

    // Needs Anno 1800: each variant of an Old World layout may play as the
    // Old World maps of its own world and island size, which include the
    // ones it was made on and others; nothing of another size or region is
    // offered.
    #[test]
    fn a_variant_plays_as_the_regions_maps_of_its_setup() {
        let Some((install, assets)) = install() else {
            return;
        };
        let maps = Redirect;
        let base =
            "data/sessions/maps/pool/moderate/moderate_archipel_ll_01/moderate_archipel_ll_01.a7t";
        let map = maps.new_map(&install, base, "Ring of Gold").unwrap();
        let mut others = 0;
        for v in &map.variants {
            let offered = maps.plays_as(&assets, &map, v);
            let guids: Vec<u32> = offered.iter().map(|p| p.guid).collect();
            let settings = |g: &u32| {
                (assets.iter())
                    .find(|a| a.guid == *g)
                    .and_then(|a| a.map.clone())
            };
            assert!(
                v.replaces.iter().all(|g| guids.contains(g)),
                "{:?}: {:?} offers {offered:?}",
                v.setup,
                v.replaces.iter().map(settings).collect::<Vec<_>>()
            );
            for p in &offered {
                let m = (assets.iter())
                    .find(|a| a.guid == p.guid)
                    .and_then(|a| a.map.as_ref())
                    .unwrap();
                let size = |w: &str| if w.is_empty() { "Small" } else { w }.to_owned();
                assert_eq!(
                    (m.region.as_str(), size(&m.size), size(&m.island_size)),
                    (
                        "Moderate",
                        v.setup.world_size.to_owned(),
                        v.setup.island_size.to_owned()
                    ),
                    "{}",
                    p.label
                );
            }
            let extra: Vec<&anno_core::map::PlaysAs> = (offered.iter())
                .filter(|p| !v.replaces.contains(&p.guid))
                .collect();
            eprintln!(
                "{:?}: plays as {:?}, may also {extra:?}",
                v.setup, v.replaces
            );
            others += extra.len();
        }
        assert!(others > 0, "some variant may play as another map");
    }
}
