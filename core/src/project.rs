// What a game's projects need: how its mods are packaged and installed, and
// how Test in game puts a project's islands on a map: the test-map plan and
// the template copy 117 and 1800 share. Each game's `Maps::plan` picks its
// templates and fills its slots, with `map::assign`.

use crate::game::{A7mRules, SizeClass};
use crate::map::{Candidate, Fill, assign};
use anno_formats::a7m::template::{MapTemplate, SlotKind};
use anno_formats::vfs::Install;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::borrow::Cow;
use std::collections::BTreeMap;

pub struct Packaging {
    /// The folder the mod loader reads mods from, in the install and in the
    /// user's Documents folder for the game. Not the editor's own project
    /// folder (`<config>/mods`), which only shares the name.
    pub mods_folder: &'static str,
    /// `modinfo.json` `Difficulty` values, the default first; empty for a
    /// loader whose manifest has none. The 117 loader refuses a mod without
    /// one.
    pub difficulties: &'static [&'static str],
    pub guid_range: GuidRange,
    /// The loader's log, as players are told to find it.
    pub loader_log: &'static str,
}

/// Where a project numbers the assets it adds (islands, test map types)
/// until it enters a range reserved in the modders' registry.
pub struct GuidRange {
    /// The first GUID of the space unreserved projects pick from.
    pub first: u32,
    /// GUIDs per project.
    pub size: u32,
    /// How many of a project's GUIDs number its islands; its test-map
    /// assets take the rest.
    pub islands: u32,
    /// How many project ranges the space holds.
    pub count: u32,
    /// Where modders reserve a range.
    pub registry: &'static str,
}

impl GuidRange {
    /// The first GUID of the range picked from `mod_id`: one of `count`, by
    /// the id's FNV-1a hash. Not reserved, so two mods can collide; the
    /// project says so until a reservation replaces it.
    pub fn pick(&self, mod_id: &str) -> u32 {
        let hash = (mod_id.bytes()).fold(0x811c_9dc5u32, |h, b| {
            (h ^ u32::from(b)).wrapping_mul(0x0100_0193)
        });
        self.first + (hash % self.count) * self.size
    }
}

/// A project island as test maps see it.
pub struct MapIsland<'a> {
    /// Virtual path of the island file.
    pub path: &'a str,
    pub region: &'a str,
    /// The placeholder size the game uses, one of `IslandRules::size_classes`.
    pub class: &'a str,
    pub regular: bool,
    pub start: bool,
    pub on_test_map: bool,
    /// Deposit (`Fertility` GUID) per slot object ID.
    pub slot_resources: &'a BTreeMap<u64, u32>,
    /// 1800: the whole fertility list, or None to follow the generator.
    pub fertilities: Option<&'a [u32]>,
    /// The island's `.a7minfo` as the project holds it, read when it has
    /// slot resources; None reads the install's.
    pub info: Option<Cow<'a, [u8]>>,
    /// The island file as the project holds it, read only for games whose
    /// test maps ask (`Maps::reads_islands`); None reads the install's.
    pub file: Option<Cow<'a, [u8]>>,
}

impl MapIsland<'_> {
    /// The island's `.a7minfo`: the project's copy, else the install's.
    pub fn info(&self, install: &Install) -> Result<Cow<'_, [u8]>> {
        let path = format!("{}info", self.path);
        match &self.info {
            Some(b) => Ok(Cow::Borrowed(b)),
            None => Ok(Cow::Owned(
                install
                    .read(&path)
                    .with_context(|| format!("Reading {path}"))?,
            )),
        }
    }

    /// The island file: the project's copy, else the install's.
    pub fn file(&self, install: &Install) -> Result<Cow<'_, [u8]>> {
        match &self.file {
            Some(b) => Ok(Cow::Borrowed(b)),
            None => Ok(Cow::Owned(
                (install.read(self.path)).with_context(|| format!("Reading {}", self.path))?,
            )),
        }
    }
}

/// A test map template: a copy of a shipped one whose slots hold the
/// project's islands. Its files are staged; this is the asset that picks it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MapEntry {
    /// Virtual path of the map; for 117 and 1800 the `.a7t`, with
    /// `.a7tinfo` and `.a7te` next to it.
    pub path: String,
    pub region: String,
    /// The setup's island size option this template answers: `Small`,
    /// `Medium` or `Large`.
    pub island_size: String,
    /// 1800's existing map assets redirected by the test mod. Its map
    /// types are an engine enum, so it cannot register a 117 map type.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub source_guids: Vec<u32>,
}

#[derive(Clone, Debug)]
pub struct PlanSlot {
    pub position: [u32; 2],
    /// Size class: an index into the planning game's
    /// `IslandRules::size_classes`, the pool's `IslandPlaceholderSize` order.
    pub class: u16,
    pub starter: bool,
    pub fill: Fill,
}

/// One generated map.
pub struct MapPlan {
    pub region: String,
    pub island_size: &'static str,
    pub size: [u32; 2],
    pub slots: Vec<PlanSlot>,
    /// The map takes over a shipped map type's templates, so the setup
    /// screen lists it under that type's name (1800's Old World
    /// Archipelago).
    pub relabels_map_type: bool,
    pub entry: MapEntry,
    /// Every file to stage, by virtual path.
    pub files: Vec<(String, Vec<u8>)>,
}

impl MapPlan {
    pub fn ours(&self) -> usize {
        (self.slots.iter())
            .filter(|s| matches!(s.fill, Fill::Ours(_)))
            .count()
    }

    pub fn starts_on_ours(&self) -> bool {
        (self.slots.iter()).any(|s| s.starter && matches!(s.fill, Fill::Ours(_)))
    }
}

/// The regions of the islands on the test map, sorted, each once.
pub fn regions<'a>(islands: &[MapIsland<'a>]) -> Vec<&'a str> {
    let mut out: Vec<&str> = (islands.iter())
        .filter(|i| i.on_test_map)
        .map(|i| i.region)
        .collect();
    out.sort();
    out.dedup();
    out
}

/// The islands of `region` on the test map whose size class the templates
/// know; `classes` is the game's `IslandRules::size_classes`.
pub fn candidates(classes: &[SizeClass], islands: &[MapIsland], region: &str) -> Vec<Candidate> {
    (islands.iter().enumerate())
        .filter(|(_, i)| i.region == region && i.on_test_map)
        .filter_map(|(n, i)| {
            Some(Candidate {
                island: n,
                class: classes.iter().position(|c| c.name == i.class)? as u16,
                regular: i.regular,
                start: i.start,
            })
        })
        .collect()
}

/// A map template copied as a test map: a shipped one, or a project
/// layout's variant.
pub struct TemplateCopy<'a> {
    /// The game's size classes, `IslandRules::size_classes`.
    pub classes: &'static [SizeClass],
    /// The game's a7m rules: which random slots are Normal and Starter.
    pub a7m: &'static A7mRules,
    /// The template to fill: a shipped one as parsed, or a layout's
    /// variant with its slots applied (`map::template_of`).
    pub template: MapTemplate,
    /// Virtual path, without extension, of the shipped template whose
    /// `.a7t` and `.a7te` the copy takes.
    pub base: &'a str,
    pub region: &'a str,
    pub island_size: &'static str,
    /// Names the copy: `<mod id>_editor_islands_<variant>`.
    pub variant: &'a str,
    /// Where test maps go: `{0}{region}{1}/{name}/{name}.a7t`.
    pub folder: [&'a str; 2],
    pub starter_class: u16,
    pub source_guids: Vec<u32>,
    pub relabels_map_type: bool,
}

impl TemplateCopy<'_> {
    /// The copy with `candidates` assigned to its random slots, `fix`
    /// writing each assigned island (by index into `islands`) into its slot
    /// and whether the slot is a start. The `.a7t` (session gamedata, no
    /// island references) and the editor-only `.a7te` are copied as shipped.
    pub fn plan(
        self,
        install: &Install,
        islands: &[MapIsland],
        candidates: &[Candidate],
        mod_id: &str,
        mut fix: impl FnMut(&mut MapTemplate, usize, &MapIsland, bool) -> Result<()>,
    ) -> Result<MapPlan> {
        let source = self.base;
        let mut template = self.template;
        let types = &self.a7m.pool_types;
        let keys: Vec<(u16, Option<bool>)> = (template.slots.iter())
            .map(|s| match s.kind {
                SlotKind::Random { size, island_type } => {
                    let role = if island_type == types.normal {
                        Some(false)
                    } else if island_type == types.starter {
                        Some(true)
                    } else {
                        None
                    };
                    (size, role)
                }
                _ => (0, None),
            })
            .collect();
        let picks = assign(&keys, candidates, self.starter_class);
        let mut slots = Vec::new();
        for (n, pick) in picks.iter().enumerate() {
            let (class, role) = keys[n];
            let starter = role == Some(true);
            let fill = match (pick, &template.slots[n].kind) {
                (Some(i), _) => Fill::Ours(*i),
                (None, SlotKind::Random { .. }) => Fill::Pool,
                _ => Fill::Vanilla,
            };
            slots.push(PlanSlot {
                position: template.slots[n].position,
                class: class.min(self.classes.len().saturating_sub(1) as u16),
                starter,
                fill,
            });
            if let Some(i) = pick {
                fix(&mut template, n, &islands[*i], starter)?;
            }
        }
        let [before, after] = self.folder;
        let lower = self.region.to_ascii_lowercase();
        let name = format!(
            "{}_editor_islands_{}",
            mod_id.replace('-', "_"),
            self.variant
        );
        let stem = format!("{before}{lower}{after}/{name}/{name}");
        let mut files = vec![(format!("{stem}.a7tinfo"), template.encode())];
        for ext in ["a7t", "a7te"] {
            files.push((
                format!("{stem}.{ext}"),
                install
                    .read(format!("{source}.{ext}"))
                    .with_context(|| format!("Reading {source}.{ext}"))?,
            ));
        }
        Ok(MapPlan {
            region: self.region.to_owned(),
            island_size: self.island_size,
            size: template.size,
            slots,
            relabels_map_type: self.relabels_map_type,
            entry: MapEntry {
                path: format!("{stem}.a7t"),
                region: self.region.to_owned(),
                island_size: self.island_size.to_owned(),
                source_guids: self.source_guids,
            },
            files,
        })
    }
}
