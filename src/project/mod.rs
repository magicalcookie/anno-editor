// Owns the mod project: one mod-loader mod folder (`modinfo.json`, loose
// files under `data/`, editor settings in `.anno-editor/`), its edited files
// by virtual path, and the list of recently opened projects. The game
// mounts `<install or Documents>/mods/<ModID>/` over its archives and
// applies ModOps to XML; patch archives in `maindata` are never mounted.
// Shipped files are never written.
pub(crate) mod models;
mod mods;
mod overview;

pub use mods::Undeclared;
pub use overview::{Installed, ProjectUi, plural};

use anno_formats::{
    a7minfo::A7mInfo,
    game::{Game, TestMaps},
    modinfo::{self, ModInfo, id_from_name, valid_id},
    rda,
};
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

#[cfg(test)]
const ASSETS_XML: &str = "data/base/config/export/assets.xml";
/// The setup screen's map type list (asset `DifficultyBalancing`) and the
/// type the play-test type is inserted after ("MapType Random").
const DIFFICULTY_GUID: u32 = 2002450;
const MAP_TYPE_ANCHOR_GUID: u32 = 43181;
/// GUIDs `base + MAP_GUID_OFFSET..` of the project's range go to the
/// play-test map type and its templates; islands take the ones below.
const MAP_GUID_OFFSET: u32 = 90;
/// Languages with a `texts_<lang>.xml`; the map type label is English in all.
const LANGUAGES: [&str; 12] = [
    "brazilian",
    "english",
    "french",
    "german",
    "italian",
    "japanese",
    "korean",
    "polish",
    "russian",
    "simplified_chinese",
    "spanish",
    "traditional_chinese",
];

/// The roles an island can play, as `IslandType` values: a regular island,
/// a player start, or both (every shipped start island is both).
pub const ROLES: [(&str, &str); 3] = [
    ("Regular", "Normal"),
    // Parallel with its neighbours; "island" is said once after the role
    // where it is shown in a sentence.
    ("Start", "Starter"),
    ("Regular or start", "Normal;Starter"),
];

/// How a new island is offered to the game. Set in the wizard, edited in
/// the island's settings, stored per island in project.json.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Listing {
    /// `IslandType`, one of [`ROLES`].
    pub kind: String,
    /// `IslandPlaceholderSize` when set; else it follows the land.
    #[serde(default)]
    pub size_override: Option<String>,
    /// Registered as a `RandomIsland`, so normal games may pick it.
    #[serde(default = "yes")]
    pub in_pool: bool,
    /// Placed on the "Editor islands" test maps.
    #[serde(default = "yes")]
    pub on_test_map: bool,
    /// World Machine export the island imports from.
    #[serde(default)]
    pub world_machine: Option<crate::island::WmLink>,
    /// Deposit (`Fertility` GUID) per slot object ID, for the test map. The
    /// island file has no field for it: the test map's fixed slot gets the
    /// island's fertility list with these in it and forces the slots on.
    #[serde(
        default,
        skip_serializing_if = "BTreeMap::is_empty",
        deserialize_with = "read_slot_resources"
    )]
    pub slot_resources: BTreeMap<u64, u32>,
    /// Where the props Auto cliffs placed stand, so a reopened island still
    /// knows which ones a rerun replaces. The island file has no field for it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub auto_cliffs: Vec<[f32; 3]>,
    /// 1800 test maps: None follows the game generator; Some fixes the whole
    /// fertility list, including an intentionally empty list.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fertilities: Option<Vec<u32>>,
}

fn yes() -> bool {
    true
}

// JSON object keys are strings. serde's flattened Listing uses a generic
// value deserializer, which cannot coerce those keys back into u64 IDs.
fn read_slot_resources<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> std::result::Result<BTreeMap<u64, u32>, D::Error> {
    BTreeMap::<String, u32>::deserialize(d)?
        .into_iter()
        .map(|(id, guid)| {
            id.parse()
                .map(|id| (id, guid))
                .map_err(serde::de::Error::custom)
        })
        .collect()
}

impl Default for Listing {
    fn default() -> Self {
        Self {
            kind: "Normal".to_owned(),
            size_override: None,
            in_pool: true,
            on_test_map: true,
            world_machine: None,
            slot_resources: BTreeMap::new(),
            auto_cliffs: Vec::new(),
            fertilities: None,
        }
    }
}

impl Listing {
    pub fn is_regular(&self) -> bool {
        self.kind.split(';').any(|k| k == "Normal")
    }

    pub fn is_start(&self) -> bool {
        self.kind.split(';').any(|k| k == "Starter")
    }
}

/// A new island the project adds to the game.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct IslandEntry {
    /// Virtual path of the `.a7m`.
    pub path: String,
    pub guid: u32,
    /// `Roman` or `Celtic`.
    pub region: String,
    /// `IslandPlaceholderSize` from the land: `Small`, `Medium`, `Large` or
    /// `XL`. See [`IslandEntry::class`].
    pub size: String,
    #[serde(flatten)]
    pub listing: Listing,
}

impl IslandEntry {
    /// The placeholder size the game uses: the override, else the land's.
    pub fn class(&self) -> &str {
        self.listing.size_override.as_deref().unwrap_or(&self.size)
    }
}

/// A play-test map template: a copy of a shipped one whose slots hold the
/// project's islands. Its files are staged; this is the asset that picks it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MapEntry {
    /// Virtual path of the `.a7t`; `.a7tinfo` and `.a7te` sit next to it.
    pub path: String,
    /// `Roman` or `Celtic`.
    pub region: String,
    /// The setup's island size option this template answers: `Small`,
    /// `Medium` or `Large`.
    pub island_size: String,
    /// 1800's existing map assets redirected by the test mod. Its map
    /// types are an engine enum, so it cannot register a 117 map type.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub source_guids: Vec<u32>,
}

/// Editor settings inside the mod folder. The loader ignores it; install
/// leaves hidden entries out.
const SETTINGS: &str = ".anno-editor/project.json";
/// In a copy installed into the game: the project folder it came from.
const INSTALL_MARK: &str = ".anno-editor-source";
/// Recently opened project folders, newest first, in the config folder.
const RECENT: &str = "projects.json";
const RECENT_KEPT: usize = 12;

/// Comments around the ModOps the editor generates into a mod's XML, so a
/// mod's own patches in the same file survive saves.
const BEGIN: &str = "<!-- anno-editor: generated from the project, replaced on every save -->";
const END: &str = "<!-- anno-editor: end -->";

#[derive(Default, Serialize, Deserialize)]
struct Settings {
    /// Only in the config-folder settings from before projects were folders;
    /// the name lives in modinfo.json now.
    #[serde(default, skip_serializing)]
    name: String,
    #[serde(default)]
    islands: Vec<IslandEntry>,
    /// First GUID handed to new assets.
    #[serde(default)]
    guid_base: Option<u32>,
    /// `guid_base` was entered from a registry reservation, not derived
    /// from the mod id.
    #[serde(default)]
    guid_reserved: bool,
    #[serde(default)]
    maps: Vec<MapEntry>,
}

pub struct Project {
    game: Game,
    /// modinfo.json: id, name, version, dependencies.
    pub info: ModInfo,
    /// Virtual path (normalized, under `data/`) to file bytes.
    // TODO(alex): every file stays in memory and every save rewrites all of
    // them; a mod with large textures wants written-file tracking instead.
    pub files: BTreeMap<String, Vec<u8>>,
    pub islands: Vec<IslandEntry>,
    /// Templates of the "Editor islands" map type; empty means no such type.
    pub maps: Vec<MapEntry>,
    guid_base: Option<u32>,
    guid_reserved: bool,
    folder: PathBuf,
}

/// One row of the project picker: read from the manifest and settings only.
#[derive(Clone, Debug)]
pub struct Summary {
    pub folder: PathBuf,
    pub name: String,
    pub islands: usize,
}

/// A reserved range's first GUID as the registry writes it, with or
/// without separators; `None` unless it is a number from 1,000,000 up.
pub fn parse_guid_base(text: &str) -> Option<u32> {
    let digits: String = (text.chars())
        .filter(|c| !matches!(c, ',' | '.' | ' ' | '_'))
        .collect();
    digits.parse::<u32>().ok().filter(|&b| b >= 1_000_000)
}

impl Project {
    /// A new, empty project in `folder`, written at once. Its GUID range is
    /// fixed now, so changing the id later keeps the GUIDs.
    pub fn create(folder: &Path, name: &str, id: &str, game: Game) -> Result<Self> {
        ensure!(!name.trim().is_empty(), "the project needs a name");
        ensure!(
            valid_id(id),
            "{id:?} is not a usable mod id: letters, digits, '-', '_' and '.'"
        );
        ensure!(
            !folder.join("modinfo.json").exists() && !folder.join(SETTINGS).exists(),
            "{} already holds a mod; open it instead",
            folder.display()
        );
        let mut project = Self::empty(folder, name.trim(), id, game);
        project.guid_base = Some(project.guid_base());
        project.write()?;
        Ok(project)
    }

    fn empty(folder: &Path, name: &str, id: &str, game: Game) -> Self {
        Self {
            game,
            info: ModInfo::new(id, name),
            files: BTreeMap::new(),
            islands: Vec::new(),
            maps: Vec::new(),
            guid_base: None,
            guid_reserved: false,
            folder: folder.to_owned(),
        }
    }

    /// Opens the mod in `folder`: manifest, editor settings and `data/`. A
    /// mod without a manifest gets one on the first save. Unreadable
    /// settings are an error, not an empty project: saving over them would
    /// drop the island list.
    pub fn open(folder: &Path, game: Game) -> Result<Self> {
        ensure!(folder.is_dir(), "{} is not a folder", folder.display());
        let manifest = folder.join("modinfo.json");
        let info = match std::fs::read(&manifest) {
            Ok(bytes) => {
                let mut info =
                    ModInfo::parse(&bytes, game).with_context(|| manifest.display().to_string())?;
                if info.id.is_empty() {
                    info.id = folder_id(folder);
                }
                if info.name.is_empty() {
                    info.name = info.id.clone();
                }
                // The line earlier versions stored as every new mod's
                // description reads as the author's own to mod managers:
                // shown empty, under its placeholder, and written out empty
                // at the next save.
                if info.description
                    == format!("Made with Anno Editor for {}.", game.profile().title)
                {
                    info.description.clear();
                }
                info
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let id = folder_id(folder);
                ModInfo::new(&id, &id)
            }
            Err(e) => return Err(e).with_context(|| manifest.display().to_string()),
        };
        let path = folder.join(SETTINGS);
        let settings = match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice::<Settings>(&bytes)
                .with_context(|| format!("{} is unreadable; fix or remove it", path.display()))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Settings::default(),
            Err(e) => return Err(e).with_context(|| path.display().to_string()),
        };
        let mut project = Self {
            islands: settings.islands,
            maps: settings.maps,
            guid_base: settings.guid_base,
            guid_reserved: settings.guid_reserved,
            ..Self::empty(folder, "", "", game)
        };
        project.info = info;
        project.read_folder()?;
        Ok(project)
    }

    /// The project to start with: the last one opened (see [`remember`]),
    /// else the one the settings from before projects were folders
    /// (`<config>/project.json`) name, moved into its mod folder, else
    /// `<config>/mods/editor`. Never fails: corrupt settings are set aside
    /// as `project.json.broken`.
    pub fn load(config: &Path, game: Game) -> Self {
        let legacy_path = config.join("project.json");
        let legacy = std::fs::read(&legacy_path)
            .ok()
            .and_then(|b| serde_json::from_slice::<Settings>(&b).ok());
        let name = legacy
            .as_ref()
            .map(|s| s.name.trim().to_owned())
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| "editor".to_owned());
        let id = Some(id_from_name(&name))
            .filter(|id| !id.is_empty())
            .unwrap_or_else(|| "editor".to_owned());
        let folder = config.join("mods").join(&id);
        if let Some(settings) = legacy
            && !folder.join(SETTINGS).exists()
        {
            match migrate(&folder, &name, &id, &settings, game) {
                Ok(()) => {
                    let _ = std::fs::rename(&legacy_path, legacy_path.with_extension("json.moved"));
                    remember(config, &folder);
                }
                Err(e) => eprintln!(
                    "Project settings not moved into {}: {e:#}",
                    folder.display()
                ),
            }
        }
        if let Some(last) = recent(config).into_iter().next() {
            match Self::open(&last, game) {
                Ok(project) => return project,
                Err(e) => eprintln!("Not reopening {}: {e:#}", last.display()),
            }
        }
        if !folder.is_dir() {
            let mut project = Self::empty(&folder, &name, &id, game);
            // Before the mod-loader switch: a patch archive, read once and
            // written as a folder on the next save.
            let archive = config
                .join("mods")
                .join(format!("zz_patchfiles_999_{name}.rda"));
            if archive.exists() {
                match rda::Archive::open(&archive).and_then(|a| {
                    a.entries()
                        .iter()
                        .map(|e| Ok((e.key().into_owned(), a.read(e)?)))
                        .collect::<Result<BTreeMap<_, _>>>()
                }) {
                    Ok(files) => project.files = files,
                    Err(e) => eprintln!("Ignoring unreadable archive {}: {e:#}", archive.display()),
                }
            }
            return project;
        }
        match Self::open(&folder, game) {
            Ok(project) => project,
            Err(e) => {
                eprintln!("{e:#}");
                let settings = folder.join(SETTINGS);
                let _ = std::fs::rename(&settings, settings.with_extension("json.broken"));
                Self::open(&folder, game).unwrap_or_else(|e| {
                    eprintln!("{e:#}");
                    Self::empty(&folder, &name, &id, game)
                })
            }
        }
    }

    pub fn game(&self) -> Game {
        self.game
    }

    /// The mod-loader id, also the folder name in the game's `mods/`.
    pub fn mod_id(&self) -> &str {
        &self.info.id
    }

    /// The mod folder this project is.
    pub fn folder(&self) -> PathBuf {
        self.folder.clone()
    }

    pub fn stage(&mut self, path: &str, bytes: Vec<u8>) {
        self.files
            .insert(rda::normalize_path(path).into_owned(), bytes);
    }

    pub fn clear(&mut self) {
        self.files.clear();
        self.islands.clear();
        self.maps.clear();
    }

    /// Replaces the play-test templates; their files are staged by the
    /// caller. Files of templates no longer listed are dropped.
    pub fn set_maps(&mut self, maps: Vec<MapEntry>) {
        for old in &self.maps {
            let stem = old.path.trim_end_matches(".a7t");
            for ext in ["a7t", "a7tinfo", "a7te"] {
                if !maps.iter().any(|m| m.path == old.path) {
                    self.files.remove(&format!("{stem}.{ext}"));
                }
            }
        }
        self.maps = maps;
    }

    /// The folder holding everything of the island at `path`: its archive,
    /// headers and `_gamedata/<name>/`.
    fn island_dir(path: &str) -> &str {
        path.rsplit_once('/').map_or(path, |(dir, _)| dir)
    }

    /// Staged files that belong to the island at `path`.
    pub fn island_files(&self, path: &str) -> Vec<String> {
        let prefix = format!("{}/", Self::island_dir(path));
        self.files
            .keys()
            .filter(|k| k.starts_with(&prefix))
            .cloned()
            .collect()
    }

    /// Staged files outside every island folder and play-test template:
    /// edited shipped assets such as UI files.
    pub fn other_files(&self) -> Vec<String> {
        let dirs: Vec<String> = self
            .islands
            .iter()
            .map(|i| format!("{}/", Self::island_dir(&i.path)))
            .chain(
                self.maps
                    .iter()
                    .map(|m| format!("{}/", Self::island_dir(&m.path))),
            )
            .collect();
        self.files
            .keys()
            .filter(|k| !dirs.iter().any(|d| k.starts_with(d)))
            .cloned()
            .collect()
    }

    /// Drops an island and every file in its folder. Returns how many files
    /// went. Play-test maps naming it need rebuilding (see `playtest`).
    pub fn remove_island(&mut self, path: &str) -> usize {
        let files = self.island_files(path);
        for f in &files {
            self.files.remove(f);
        }
        self.islands.retain(|i| i.path != path);
        files.len()
    }

    /// Moves the island at `from` to the new `.a7m` path `to`: its files
    /// move with the folder, and the `.a7minfo` points at the moved
    /// `.a7me`. The island keeps its GUID and settings.
    pub fn rename_island(&mut self, from: &str, to: &str) -> Result<()> {
        let to = rda::normalize_path(to).into_owned();
        ensure!(to.ends_with(".a7m"), "{to} is not an island path");
        ensure!(
            !self.islands.iter().any(|i| i.path == to),
            "the project already has {to}"
        );
        let entry = self
            .islands
            .iter_mut()
            .find(|i| i.path == from)
            .with_context(|| format!("{from} is not in the project"))?;
        let (old_dir, new_dir) = (
            Self::island_dir(from).to_owned(),
            Self::island_dir(&to).to_owned(),
        );
        let stem = |p: &str| {
            p.rsplit('/')
                .next()
                .unwrap_or("")
                .trim_end_matches(".a7m")
                .to_owned()
        };
        let (old_stem, new_stem) = (stem(from), stem(&to));
        entry.path = to.clone();
        let moving: Vec<String> = self
            .files
            .keys()
            .filter(|k| k.starts_with(&format!("{old_dir}/")))
            .cloned()
            .collect();
        for old in moving {
            let rest = &old[old_dir.len() + 1..];
            // `<stem>.a7m`, `<stem>.a7minfo`, `_gamedata/<stem>/…` carry the
            // name; other files keep theirs.
            let rest = if let Some(tail) = rest.strip_prefix(&format!("_gamedata/{old_stem}/")) {
                format!("_gamedata/{new_stem}/{tail}")
            } else if let Some(ext) = rest.strip_prefix(&format!("{old_stem}.")) {
                format!("{new_stem}.{ext}")
            } else {
                rest.to_owned()
            };
            let bytes = self.files.remove(&old).expect("listed above");
            self.files.insert(format!("{new_dir}/{rest}"), bytes);
        }
        let info = format!("{}info", to);
        if let Some(bytes) = self.files.get(&info) {
            let mut header = A7mInfo::parse(bytes).context("the island’s .a7minfo")?;
            header.source_map = to.replace(".a7m", ".a7me").replace('/', "\\");
            self.files.insert(info, header.encode());
        }
        Ok(())
    }

    /// Changes the mod id. A folder named after the old id is renamed with
    /// it; any other folder (a repository, say) stays where it is. The GUID
    /// range stays: it was picked from the old id.
    pub fn set_mod_id(&mut self, id: &str) -> Result<PathBuf> {
        let id = id.trim();
        ensure!(
            valid_id(id),
            "{id:?} is not a usable mod id: letters, digits, '-', '_' and '.'"
        );
        if id == self.info.id {
            return Ok(self.folder());
        }
        let base = self.guid_base();
        let old = self.folder.clone();
        let follows = old.file_name().is_some_and(|n| *n == *self.info.id);
        let new = if follows {
            old.with_file_name(id)
        } else {
            old.clone()
        };
        if new != old {
            ensure!(
                !new.exists(),
                "{} already exists; pick another id",
                new.display()
            );
            if old.exists() {
                std::fs::rename(&old, &new)
                    .with_context(|| format!("Moving {} to {}", old.display(), new.display()))?;
            }
        }
        self.guid_base.get_or_insert(base);
        self.info.id = id.to_owned();
        self.folder = new;
        self.write()
    }

    /// Removes `<mods>/<id>` from the install root and from `<game>/mods`,
    /// where installs went before, when it is this mod (same `ModID`).
    /// Returns whether there was one.
    pub fn uninstall(&self, game: &Path, id: &str) -> Result<bool> {
        let mut removed = false;
        for root in [modinfo::install_root(self.game, game), game.join("mods")] {
            let target = root.join(id);
            if !target.exists() {
                continue;
            }
            let info = std::fs::read(target.join("modinfo.json")).unwrap_or_default();
            let found = serde_json::from_slice::<serde_json::Value>(&info)
                .ok()
                .and_then(|v| v.get("ModID")?.as_str().map(str::to_owned));
            ensure!(
                found.as_deref() == Some(id),
                "{} is not this mod; left alone",
                target.display()
            );
            std::fs::remove_dir_all(&target)?;
            removed = true;
        }
        Ok(removed)
    }

    /// Adds or updates a pool island.
    pub fn register_island(&mut self, entry: IslandEntry) {
        match self.islands.iter_mut().find(|i| i.path == entry.path) {
            Some(old) => *old = entry,
            None => self.islands.push(entry),
        }
    }

    /// The GUID an island at `path` uses: its existing one, else the next
    /// free one in the project's range.
    // The default range is picked from the mod id, not reserved; publishing
    // needs a range from github.com/anno-mods/GuidRanges, which the
    // Overview's GUID range field sets.
    pub fn island_guid(&self, path: &str) -> u32 {
        if let Some(i) = self.islands.iter().find(|i| i.path == path) {
            return i.guid;
        }
        let base = self.guid_base();
        (base..base + MAP_GUID_OFFSET)
            .find(|g| self.islands.iter().all(|i| i.guid != *g))
            .expect("fewer than 90 islands per project")
    }

    /// First GUID of the project's range.
    /// Sets the first GUID of the project's range, as reserved in the
    /// registry. Islands already in the project keep their GUIDs; new ones
    /// are numbered from here. Written with the project.
    pub fn set_guid_base(&mut self, base: u32) {
        self.guid_base = Some(base);
        self.guid_reserved = true;
    }

    /// Whether the range was entered from a reservation; until then it is
    /// picked from the mod id and may collide with another mod.
    pub fn guid_reserved(&self) -> bool {
        self.guid_reserved
    }

    pub fn guid_base(&self) -> u32 {
        self.guid_base.unwrap_or_else(|| {
            let hash = self.mod_id().bytes().fold(0x811c_9dc5u32, |h, b| {
                (h ^ u32::from(b)).wrapping_mul(0x0100_0193)
            });
            1_900_000_000 + (hash % 1_000_000) * 100
        })
    }

    /// The play-test map type; also its label's text LineId.
    pub fn map_type_guid(&self) -> u32 {
        self.guid_base() + MAP_GUID_OFFSET
    }

    /// Writes the project into its folder: `modinfo.json`, the editor
    /// settings, and `data/` with every staged file and the generated
    /// ModOps. `data/` is built next to the old one and swapped in, so a
    /// failed save leaves the last one intact; nothing else in the folder
    /// is touched.
    pub fn write(&self) -> Result<PathBuf> {
        ensure!(
            self.game.profile().test_maps != TestMaps::Redirect
                || self.maps.iter().all(|m| !m.source_guids.is_empty()),
            "1800 test maps need the shipped template GUIDs they replace"
        );
        if let Some(path) = self.files.keys().find(|p| !p.starts_with("data/")) {
            bail!("{path} is outside data/; the mod loader only reads data/");
        }
        let folder = &self.folder;
        std::fs::create_dir_all(folder)
            .with_context(|| format!("Creating {}", folder.display()))?;
        let fresh = folder.join(".data.writing");
        if fresh.exists() {
            std::fs::remove_dir_all(&fresh)?;
        }
        let put = |path: &str, bytes: &[u8]| -> Result<()> {
            let target = fresh.join(&path["data/".len()..]);
            std::fs::create_dir_all(target.parent().context("file has a folder")?)?;
            std::fs::write(&target, bytes).with_context(|| format!("Writing {}", target.display()))
        };
        let generated = self.generated();
        for (path, bytes) in &self.files {
            if !generated.iter().any(|(p, _)| p == path) {
                put(path, bytes)?;
            }
        }
        for (path, body) in &generated {
            let own = self.files.get(path).map(Vec::as_slice);
            put(
                path,
                merge_modops(own, body)
                    .with_context(|| path.clone())?
                    .as_bytes(),
            )?;
        }
        std::fs::create_dir_all(&fresh)?;
        let data = folder.join("data");
        let old = folder.join(".data.old");
        if old.exists() {
            std::fs::remove_dir_all(&old)?;
        }
        if data.exists() {
            std::fs::rename(&data, &old)?;
        }
        if let Err(e) = std::fs::rename(&fresh, &data) {
            if old.exists() {
                let _ = std::fs::rename(&old, &data);
            }
            return Err(e).with_context(|| format!("Replacing {}", data.display()));
        }
        if old.exists() {
            std::fs::remove_dir_all(&old).with_context(|| format!("Removing {}", old.display()))?;
        }
        write_atomic(
            &folder.join("modinfo.json"),
            self.info.encode(self.game).as_bytes(),
        )?;
        let settings = Settings {
            name: String::new(),
            islands: self.islands.clone(),
            guid_base: self.guid_base,
            guid_reserved: self.guid_reserved,
            maps: self.maps.clone(),
        };
        let path = folder.join(SETTINGS);
        std::fs::create_dir_all(path.parent().context("settings have a folder")?)?;
        write_atomic(&path, &serde_json::to_vec_pretty(&settings)?)?;
        Ok(folder.clone())
    }

    /// Whether `copy` (a folder in the game's `mods/`) is this project's
    /// installed copy and may be replaced. Every new project starts with
    /// the id "editor", so a matching ModID proves nothing: a copy is ours
    /// when its install mark names this project's folder. Copies installed
    /// before the mark existed pass when their manifest is ours byte for
    /// byte, which a project that never saved has none of.
    pub fn owns_install(&self, copy: &Path) -> bool {
        let here = std::fs::canonicalize(&self.folder).unwrap_or_else(|_| self.folder.clone());
        match std::fs::read(copy.join(INSTALL_MARK)) {
            Ok(mark) => Path::new(String::from_utf8_lossy(&mark).trim()) == here,
            Err(_) => {
                let ours = std::fs::read(self.folder.join("modinfo.json"));
                let theirs = std::fs::read(copy.join("modinfo.json"));
                matches!((ours, theirs), (Ok(a), Ok(b)) if a == b)
            }
        }
    }

    /// Writes the project and copies it into `<mods>/<ModID>/` under
    /// `modinfo::install_root`, without hidden entries (editor settings,
    /// `.git`). An existing folder there is replaced only if it is this
    /// project's copy (see `owns_install`).
    pub fn install(&self, game: &Path) -> Result<PathBuf> {
        if let Some(found) = Game::detect(game) {
            ensure!(
                found == self.game,
                "this project belongs to {}, not {}",
                self.game.profile().title,
                found.profile().title
            );
        }
        let target = modinfo::install_root(self.game, game).join(self.mod_id());
        ensure!(
            target != self.folder,
            "the project folder is the game’s copy; nothing to install"
        );
        // Before writing: a project that never saved has no manifest yet,
        // and `owns_install` must not find the one `write` is about to make.
        if target.exists() && !self.owns_install(&target) {
            bail!(
                "{} exists and is not this project's copy; change the mod id or move it away",
                target.display()
            );
        }
        let source = self.write()?;
        if target.exists() {
            std::fs::remove_dir_all(&target)?;
        }
        copy_dir(&source, &target)?;
        let here = std::fs::canonicalize(&source).unwrap_or(source);
        std::fs::write(target.join(INSTALL_MARK), here.to_string_lossy().as_bytes())?;
        // Installs used to go to `<game>/mods`. The loader reads both
        // folders, so our old copy there would load as a second mod.
        let old = game.join("mods").join(self.mod_id());
        if old != target && old.exists() && self.owns_install(&old) {
            std::fs::remove_dir_all(&old)?;
        }
        Ok(target)
    }

    /// ModOps written from `islands` and `maps` rather than staged: each
    /// file's patches without the `<ModOps>` wrapper (see `merge_modops`).
    fn generated(&self) -> Vec<(String, String)> {
        let mut out = Vec::new();
        let profile = self.game.profile();
        if self.islands.iter().any(|i| i.listing.in_pool) || !self.maps.is_empty() {
            out.push((profile.assets_path.to_owned(), self.assets_modops()));
        }
        if !self.maps.is_empty() {
            let id = self.map_type_guid();
            let key = profile.text_id;
            let xml = format!(
                "  <ModOp Type=\"add\" Path=\"/TextExport/Texts\">\n    <Text>\n      <{key}>{id}</{key}>\n      <Text>Editor islands</Text>\n    </Text>\n  </ModOp>\n"
            );
            for lang in LANGUAGES {
                out.push((profile.texts_path.replace("english", lang), xml.clone()));
            }
        }
        out
    }

    fn assets_modops(&self) -> String {
        let mut xml = String::new();
        if !self.maps.is_empty() {
            xml += &self.map_modops();
        }
        if !self.islands.iter().any(|i| i.listing.in_pool) {
            return xml;
        }
        let profile = self.game.profile();
        let anchor = profile.pool_anchor_guid;
        xml += &format!("  <ModOp Type=\"addNextSibling\" GUID=\"{anchor}\">\n");
        for i in self.islands.iter().filter(|i| i.listing.in_pool) {
            let name = i
                .path
                .rsplit('/')
                .next()
                .unwrap_or("island")
                .trim_end_matches(".a7m");
            xml += &format!(
                "    <Asset>\n      <Template>RandomIsland</Template>\n      <Values>\n        <Standard>\n          <GUID>{}</GUID>\n          <Name>{name}</Name>\n        </Standard>\n        <RandomIsland>\n          <FilePath>{}</FilePath>\n          <IslandRegion>{}</IslandRegion>\n          <IslandDifficulty>Normal;Hard</IslandDifficulty>\n          <IslandType>{}</IslandType>\n          {}\n        </RandomIsland>\n      </Values>\n    </Asset>\n",
                i.guid,
                i.path,
                i.region,
                i.listing.kind,
                if profile.placeholder_size {
                    format!(
                        "<IslandPlaceholderSize>{}</IslandPlaceholderSize>",
                        i.class()
                    )
                } else {
                    String::new()
                }
            );
        }
        xml += "  </ModOp>\n";
        xml
    }

    /// The "Editor islands" map type, its entry in the setup list, and one
    /// `MapTemplate` per staged template. Templates carry no `EnlargeDLC`, so
    /// DLC owners get the same file. Wiggling is off: it shifts slots, and
    /// fixed islands were placed to fit the slot they replace.
    fn map_modops(&self) -> String {
        if self.game.profile().test_maps == TestMaps::Redirect {
            let mut xml = String::new();
            for map in &self.maps {
                let ids = map
                    .source_guids
                    .iter()
                    .map(u32::to_string)
                    .collect::<Vec<_>>()
                    .join(",");
                xml += &format!(
                    "  <ModOp Type=\"merge\" GUID=\"{ids}\" Path=\"/Values/MapTemplate\">\n    <TemplateFilename>{}</TemplateFilename>\n    <EnlargedTemplateFilename>{}</EnlargedTemplateFilename>\n  </ModOp>\n",
                    map.path, map.path
                );
            }
            if self.maps.iter().any(|m| m.region == "Moderate") {
                let id = self.map_type_guid();
                xml += &format!(
                    "  <ModOp Type=\"replace\" GUID=\"2002450\" Path=\"/Values/DifficultySettings/MapType/Archipelago/ValueName\"><ValueName>{id}</ValueName></ModOp>\n"
                );
            }
            return xml;
        }
        let ty = self.map_type_guid();
        let mut xml = format!(
            "  <ModOp Type=\"addNextSibling\" GUID=\"{MAP_TYPE_ANCHOR_GUID}\">\n    <Asset>\n      <Template>MapTemplateType</Template>\n      <Values>\n        <Standard>\n          <GUID>{ty}</GUID>\n          <Name>MapType Editor islands</Name>\n        </Standard>\n        <MapTemplateType>\n          <Name>{ty}</Name>\n        </MapTemplateType>\n      </Values>\n    </Asset>\n"
        );
        for (i, m) in self.maps.iter().enumerate() {
            let guid = ty + 1 + i as u32;
            xml += &format!(
                "    <Asset>\n      <Template>MapTemplate</Template>\n      <Values>\n        <Standard>\n          <GUID>{guid}</GUID>\n          <Name>Map Template {} Editor islands - {}</Name>\n        </Standard>\n        <MapTemplate>\n          <IslandSize>{}</IslandSize>\n          <TemplateFilename>{}</TemplateFilename>\n          <TemplateSize>Large</TemplateSize>\n          <IsUsedByMapGenerator>1</IsUsedByMapGenerator>\n          <TemplateRegion>{}</TemplateRegion>\n          <MapTemplateType>{ty}</MapTemplateType>\n          <WiggleIterationCount>0</WiggleIterationCount>\n        </MapTemplate>\n      </Values>\n    </Asset>\n",
                m.region, m.island_size, m.island_size, m.path, m.region
            );
        }
        xml += "  </ModOp>\n";
        xml += &format!(
            "  <ModOp Type=\"add\" GUID=\"{DIFFICULTY_GUID}\" Path=\"/Values/DifficultySettings/MapTemplateTypes\">\n    <Item>\n      <MapType>{ty}</MapType>\n    </Item>\n  </ModOp>\n"
        );
        xml
    }

    /// Stages `data/` from the folder. XML the editor generated is cut out
    /// of its file between the markers; a file left with nothing else is
    /// not staged. Files at generated paths without markers were written
    /// before the markers existed and are regenerated, not staged.
    fn read_folder(&mut self) -> Result<()> {
        let generated: Vec<String> = self.generated().into_iter().map(|(p, _)| p).collect();
        let root = self.folder.clone();
        let mut stack = vec![root.join("data")];
        while let Some(dir) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries {
                let path = entry?.path();
                if path.is_dir() {
                    stack.push(path);
                    continue;
                }
                let rel = path
                    .strip_prefix(&root)?
                    .to_string_lossy()
                    .replace('\\', "/");
                let rel = rda::normalize_path(&rel).into_owned();
                let bytes = std::fs::read(&path)?;
                if rel.ends_with(".xml") {
                    match strip_generated(&bytes) {
                        Some(rest) if is_empty_modops(&rest) => continue,
                        Some(rest) => {
                            self.stage(&rel, rest.into_bytes());
                            continue;
                        }
                        None if generated.contains(&rel) => continue,
                        None => {}
                    }
                }
                self.stage(&rel, bytes);
            }
        }
        Ok(())
    }
}

/// The settings from before projects were folders, moved into `folder`,
/// with a manifest when it has none.
fn migrate(folder: &Path, name: &str, id: &str, settings: &Settings, game: Game) -> Result<()> {
    std::fs::create_dir_all(folder.join(".anno-editor"))?;
    let manifest = folder.join("modinfo.json");
    if !manifest.exists() {
        write_atomic(&manifest, ModInfo::new(id, name).encode(game).as_bytes())?;
    }
    let moved = Settings {
        name: String::new(),
        islands: settings.islands.clone(),
        guid_base: settings.guid_base,
        guid_reserved: settings.guid_reserved,
        maps: settings.maps.clone(),
    };
    write_atomic(&folder.join(SETTINGS), &serde_json::to_vec_pretty(&moved)?)
}

/// An id for a folder without a manifest: its name, as the 1800 loader does.
fn folder_id(folder: &Path) -> String {
    folder
        .file_name()
        .map(|n| id_from_name(&n.to_string_lossy()))
        .filter(|id| !id.is_empty())
        .unwrap_or_else(|| "editor".to_owned())
}

/// `body` between the markers inside the mod's own `own` file, before its
/// closing tag, or in a file of its own.
fn merge_modops(own: Option<&[u8]>, body: &str) -> Result<String> {
    let block = format!("  {BEGIN}\n{body}  {END}\n");
    let Some(own) = own else {
        return Ok(format!("<ModOps>\n{block}</ModOps>\n"));
    };
    let own = String::from_utf8_lossy(own);
    let at = own
        .rfind("</ModOps>")
        .context("the mod’s own file has no </ModOps> to add the editor’s patches to")?;
    Ok(format!("{}{block}{}", &own[..at], &own[at..]))
}

/// The file without the generated block (its whole lines), or `None` when
/// it has none.
fn strip_generated(bytes: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(bytes);
    let begin = text.find(BEGIN)?;
    let end = text[begin..].find(END)? + begin + END.len();
    let start = text[..begin].rfind('\n').map_or(0, |i| i + 1);
    let stop = text[end..].find('\n').map_or(text.len(), |i| end + i + 1);
    Some(format!("{}{}", &text[..start], &text[stop..]))
}

fn is_empty_modops(xml: &str) -> bool {
    let bare: String = xml.chars().filter(|c| !c.is_whitespace()).collect();
    bare.is_empty() || bare == "<ModOps></ModOps>"
}

/// Writes next to `path` and renames over it, so a crash leaves the old file.
fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = path.with_extension("writing");
    std::fs::write(&tmp, bytes).with_context(|| format!("Writing {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("Writing {}", path.display()))
}

/// Copies a mod folder, leaving out hidden entries: editor settings, `.git`,
/// half-written `data/`.
/// How a file in the project differs from the game's installed copy.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Change {
    Added,
    Changed,
    Removed,
}

/// Files that differ between the project folder and the game's copy at
/// `copy`, by relative path, compared by content, without hidden entries
/// (as `install` copies). Sorted, at most `limit` (mods carry tens of
/// files; the list says what an update brings, not a full audit).
pub fn changes_since(project: &Path, copy: &Path, limit: usize) -> Vec<(String, Change)> {
    fn walk(root: &Path, dir: &Path, out: &mut BTreeMap<String, PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for e in entries.flatten() {
            if e.file_name().to_string_lossy().starts_with('.') {
                continue;
            }
            let path = e.path();
            if path.is_dir() {
                walk(root, &path, out);
            } else if let Ok(rel) = path.strip_prefix(root) {
                out.insert(rel.to_string_lossy().replace('\\', "/"), path);
            }
        }
    }
    let (mut ours, mut theirs) = (BTreeMap::new(), BTreeMap::new());
    walk(project, project, &mut ours);
    walk(copy, copy, &mut theirs);
    let same = |a: &Path, b: &Path| {
        let size = |p: &Path| std::fs::metadata(p).map(|m| m.len()).ok();
        size(a) == size(b) && std::fs::read(a).ok() == std::fs::read(b).ok()
    };
    let mut out = Vec::new();
    for (rel, path) in &ours {
        match theirs.get(rel) {
            None => out.push((rel.clone(), Change::Added)),
            Some(other) if !same(path, other) => out.push((rel.clone(), Change::Changed)),
            _ => {}
        }
    }
    out.extend(
        theirs
            .keys()
            .filter(|rel| !ours.contains_key(*rel))
            .map(|rel| (rel.clone(), Change::Removed)),
    );
    out.sort();
    out.truncate(limit);
    out
}

fn copy_dir(from: &Path, to: &Path) -> Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        if entry.file_name().to_string_lossy().starts_with('.') {
            continue;
        }
        let target = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir(&entry.path(), &target)?;
        } else {
            std::fs::copy(entry.path(), &target)
                .with_context(|| format!("Copying to {}", target.display()))?;
        }
    }
    Ok(())
}

/// Project folders opened before, newest first, that still exist.
pub fn recent(config: &Path) -> Vec<PathBuf> {
    std::fs::read(config.join(RECENT))
        .ok()
        .and_then(|b| serde_json::from_slice::<Vec<PathBuf>>(&b).ok())
        .unwrap_or_default()
        .into_iter()
        .filter(|p| p.is_dir())
        .collect()
}

/// Puts `folder` first in the recent list.
pub fn remember(config: &Path, folder: &Path) {
    let mut list = recent(config);
    list.retain(|p| p != folder);
    list.insert(0, folder.to_owned());
    list.truncate(RECENT_KEPT);
    let written = std::fs::create_dir_all(config).and_then(|()| {
        std::fs::write(
            config.join(RECENT),
            serde_json::to_vec_pretty(&list).expect("paths serialize"),
        )
    });
    if let Err(e) = written {
        eprintln!("Recent projects not saved: {e}");
    }
}

/// Every project the picker offers: recent folders first, then mod folders
/// under `<config>/mods` not among them.
pub fn known(config: &Path, game: Game) -> Vec<Summary> {
    let mut folders = recent(config);
    if let Ok(entries) = std::fs::read_dir(config.join("mods")) {
        let mut more: Vec<PathBuf> = entries
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.is_dir() && !folders.contains(p))
            .filter(|p| p.join("modinfo.json").is_file() || p.join(SETTINGS).is_file())
            .collect();
        more.sort();
        folders.extend(more);
    }
    folders
        .into_iter()
        .map(|folder| {
            let name = std::fs::read(folder.join("modinfo.json"))
                .ok()
                .and_then(|b| ModInfo::parse(&b, game).ok())
                .map(|i| if i.name.is_empty() { i.id } else { i.name })
                .filter(|n| !n.is_empty())
                .unwrap_or_else(|| folder_id(&folder));
            let islands = std::fs::read(folder.join(SETTINGS))
                .ok()
                .and_then(|b| serde_json::from_slice::<Settings>(&b).ok())
                .map_or(0, |s| s.islands.len());
            Summary {
                name,
                islands,
                folder,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    #[test]
    fn changes_since_lists_new_changed_and_removed_files_but_not_hidden_ones() {
        let (project, copy) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let write = |root: &std::path::Path, rel: &str, body: &str| {
            let path = root.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, body).unwrap();
        };
        write(project.path(), "modinfo.json", "{}");
        write(copy.path(), "modinfo.json", "{}");
        write(project.path(), "data/islands/cove.a7m", "new terrain");
        write(copy.path(), "data/islands/cove.a7m", "old terrain");
        write(project.path(), "data/new.xml", "x");
        write(copy.path(), "data/gone.xml", "y");
        write(project.path(), ".editor/settings.json", "hidden");
        assert_eq!(
            super::changes_since(project.path(), copy.path(), 10),
            [
                ("data/gone.xml".to_owned(), super::Change::Removed),
                ("data/islands/cove.a7m".to_owned(), super::Change::Changed),
                ("data/new.xml".to_owned(), super::Change::Added),
            ]
        );
        assert_eq!(
            super::changes_since(project.path(), copy.path(), 1).len(),
            1
        );
    }

    #[test]
    fn settings_with_resource_assignments_reload() {
        let config = tempfile::tempdir().unwrap();
        let mut project = super::Project::load(config.path(), anno_formats::game::Game::Anno1800);
        project.info.name = "Resource reload".into();
        project.islands.push(super::IslandEntry {
            path: "data/sessions/islands/custom/reload/reload.a7m".into(),
            guid: 2000000001,
            region: "Moderate".into(),
            size: "Small".into(),
            listing: super::Listing {
                slot_resources: std::collections::BTreeMap::from([(u64::MAX - 1, 1010501)]),
                fertilities: Some(vec![1010569]),
                ..Default::default()
            },
        });
        project.write().unwrap();
        let bytes = std::fs::read(project.folder().join(super::SETTINGS)).unwrap();
        let _: super::Settings = serde_json::from_slice(&bytes).unwrap();
        let back = super::Project::load(config.path(), anno_formats::game::Game::Anno1800);
        assert_eq!(back.info.name, project.info.name);
        assert_eq!(back.islands, project.islands);
    }

    use super::{ASSETS_XML, Game, IslandEntry, Listing, Project, rda};

    fn island(project: &mut Project, name: &str) -> String {
        let path = format!("data/base/provinces/roman/islands/custom/{name}/{name}.a7m");
        let stem = path.trim_end_matches(".a7m");
        let dir = format!("data/base/provinces/roman/islands/custom/{name}");
        project.stage(&path, b"archive".to_vec());
        let info = anno_formats::a7minfo::A7mInfo::new([128, 128], &format!("{stem}.a7me"));
        project.stage(&format!("{stem}.a7minfo"), info.encode());
        project.stage(&format!("{stem}.a7me"), b"<a7me/>".to_vec());
        project.stage(
            &format!("{dir}/_gamedata/{name}/mapimage.png"),
            b"png".to_vec(),
        );
        project.stage(
            &format!("{dir}/_gamedata/{name}/meshes/2/0x0.tmc"),
            b"tmc".to_vec(),
        );
        project.register_island(IslandEntry {
            guid: project.island_guid(&path),
            path: path.clone(),
            region: "Roman".into(),
            size: "Small".into(),
            listing: Listing::default(),
        });
        path
    }

    #[test]
    fn renaming_an_island_moves_its_files_and_header() {
        let config = tempfile::tempdir().unwrap();
        let mut project = Project::load(config.path(), anno_formats::game::Game::Anno117);
        let old = island(&mut project, "nova");
        let other = island(&mut project, "parva");
        project.stage("data/ui/x.xml", b"ui".to_vec());
        let guid = project.islands[0].guid;
        let new = "data/base/provinces/roman/islands/custom/magna/magna.a7m";
        project.rename_island(&old, new).unwrap();
        assert_eq!(project.islands[0].path, new);
        assert_eq!(project.islands[0].guid, guid, "keeps its GUID");
        let dir = "data/base/provinces/roman/islands/custom/magna";
        let mut files = project.island_files(new);
        files.sort();
        assert_eq!(
            files,
            [
                format!("{dir}/_gamedata/magna/mapimage.png"),
                format!("{dir}/_gamedata/magna/meshes/2/0x0.tmc"),
                format!("{dir}/magna.a7m"),
                format!("{dir}/magna.a7me"),
                format!("{dir}/magna.a7minfo"),
            ]
        );
        assert!(project.island_files(&old).is_empty());
        let info =
            anno_formats::a7minfo::A7mInfo::parse(&project.files[&format!("{dir}/magna.a7minfo")])
                .unwrap();
        assert_eq!(
            info.source_map,
            format!("{dir}/magna.a7me").replace('/', "\\")
        );
        assert_eq!(project.island_files(&other).len(), 5, "other islands stay");
        assert_eq!(project.other_files(), ["data/ui/x.xml"]);
        assert!(project.rename_island(new, &other).is_err(), "no clobbering");

        assert_eq!(project.remove_island(new), 5);
        assert_eq!(project.islands.len(), 1);
        assert!(project.island_files(new).is_empty());
    }

    #[test]
    fn a_folder_named_after_the_id_follows_it() {
        let config = tempfile::tempdir().unwrap();
        let mut project = Project::load(config.path(), anno_formats::game::Game::Anno117);
        island(&mut project, "nova");
        let old = project.write().unwrap();
        let guid = project.map_type_guid();
        project.info.name = "Latium Isles".into();
        let new = project.set_mod_id("latium-isles").unwrap();
        assert!(new.join("modinfo.json").exists() && !old.exists());
        assert_eq!(project.map_type_guid(), guid, "GUIDs stay with the project");
        let back = Project::open(&new, Game::Anno117).unwrap();
        assert_eq!(back.info.name, "Latium Isles");
        assert_eq!(back.mod_id(), "latium-isles");
        assert_eq!(back.islands.len(), 1);
        assert_eq!(back.files.len(), project.files.len());
        // A folder with another name (a repository) stays put.
        let repo = config.path().join("my-repo");
        let mut other = Project::create(&repo, "Other", "other", Game::Anno117).unwrap();
        assert_eq!(other.set_mod_id("other-2").unwrap(), repo);

        // Uninstall only removes this mod's folder.
        let game = tempfile::tempdir().unwrap();
        assert!(!project.uninstall(game.path(), project.mod_id()).unwrap());
        project.install(game.path()).unwrap();
        assert!(project.uninstall(game.path(), project.mod_id()).unwrap());
        assert!(!game.path().join("mods/latium-isles").exists());
    }

    #[test]
    fn islands_out_of_the_pool_get_no_pool_entry() {
        let config = tempfile::tempdir().unwrap();
        // project.json as written before island settings existed.
        std::fs::write(
            config.path().join("project.json"),
            r#"{"name":"old","islands":[{"path":"data/x/a.a7m","guid":7,"region":"Roman","size":"Small","kind":"Normal"}]}"#,
        )
        .unwrap();
        let mut project = Project::load(config.path(), anno_formats::game::Game::Anno117);
        assert_eq!(project.islands[0].listing, Listing::default());
        project.register_island(IslandEntry {
            path: "data/x/b.a7m".into(),
            guid: 8,
            region: "Roman".into(),
            size: "Small".into(),
            listing: Listing {
                kind: "Normal;Starter".into(),
                size_override: Some("Large".into()),
                ..Listing::default()
            },
        });
        let folder = project.write().unwrap();
        let assets =
            std::fs::read_to_string(folder.join("data/base/config/export/assets.xml")).unwrap();
        assert!(assets.contains("<IslandType>Normal;Starter</IslandType>"));
        assert!(
            assets.contains("<IslandPlaceholderSize>Large</IslandPlaceholderSize>"),
            "the override wins"
        );

        project
            .islands
            .iter_mut()
            .for_each(|i| i.listing.in_pool = false);
        let folder = project.write().unwrap();
        assert!(
            !folder.join("data/base/config/export/assets.xml").exists(),
            "nothing to register"
        );
        assert_eq!(
            Project::load(config.path(), anno_formats::game::Game::Anno117).islands,
            project.islands
        );
    }

    #[test]
    fn guid_bases_parse_as_the_registry_writes_them() {
        use super::parse_guid_base;
        assert_eq!(parse_guid_base("1938924000"), Some(1_938_924_000));
        assert_eq!(parse_guid_base("1,938,924,000"), Some(1_938_924_000));
        assert_eq!(parse_guid_base("1 938 924 000"), Some(1_938_924_000));
        // Too small to be a reservation, or not a number.
        assert_eq!(parse_guid_base("42"), None);
        assert_eq!(parse_guid_base("abc"), None);
    }

    #[test]
    fn a_reserved_range_numbers_new_islands_and_keeps_old_ones() {
        let config = tempfile::tempdir().unwrap();
        let folder = config.path().join("mods/ranged");
        let mut project = Project::create(&folder, "Ranged", "ranged", Game::Anno117).unwrap();
        let old = "data/base/provinces/roman/islands/custom/old/old.a7m";
        let kept = project.island_guid(old);
        project.register_island(IslandEntry {
            path: old.into(),
            guid: kept,
            region: "Roman".into(),
            size: "Medium".into(),
            listing: Listing::default(),
        });
        project.set_guid_base(1_938_924_000);
        let new = "data/base/provinces/roman/islands/custom/new/new.a7m";
        assert_eq!(project.island_guid(new), 1_938_924_000);
        assert_eq!(project.island_guid(old), kept, "an island keeps its GUID");
        let back = Project::open(&project.write().unwrap(), Game::Anno117).unwrap();
        assert_eq!(back.guid_base(), 1_938_924_000, "the range is written");
        assert!(back.guid_reserved(), "and marked as reserved");
    }

    #[test]
    fn writes_a_mod_folder_that_reads_back() {
        let config = tempfile::tempdir().unwrap();
        let folder = config.path().join("mods/my-islands");
        let mut project =
            Project::create(&folder, "My Islands!", "my-islands", Game::Anno117).unwrap();
        assert!(Project::create(&folder, "Again", "again", Game::Anno117).is_err());
        project.stage("data/a/b.txt", b"hello".to_vec());
        let path = "data/base/provinces/roman/islands/custom/nova/nova.a7m";
        let guid = project.island_guid(path);
        project.register_island(IslandEntry {
            path: path.into(),
            guid,
            region: "Roman".into(),
            size: "Medium".into(),
            listing: Listing::default(),
        });
        assert_eq!(project.island_guid(path), guid, "an island keeps its GUID");
        let folder = project.write().unwrap();
        let info: serde_json::Value =
            serde_json::from_slice(&std::fs::read(folder.join("modinfo.json")).unwrap()).unwrap();
        assert_eq!(info["Anno"], 8);
        assert_eq!(info["ModID"], "my-islands");
        let assets =
            std::fs::read_to_string(folder.join("data/base/config/export/assets.xml")).unwrap();
        assert!(assets.contains(&format!("<GUID>{guid}</GUID>")));
        assert!(assets.contains(&format!("<FilePath>{path}</FilePath>")));

        // A second save replaces the folder; a reload sees the same project.
        project.write().unwrap();
        let back = Project::open(&folder, Game::Anno117).unwrap();
        assert_eq!(back.info.name, "My Islands!");
        assert_eq!(back.files.len(), 1);
        assert_eq!(back.files["data/a/b.txt"], b"hello");
        assert_eq!(back.islands, project.islands);
    }

    #[test]
    fn install_keeps_another_project_with_the_same_id() {
        let game = tempfile::tempdir().unwrap();
        let (a, b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let mut first = Project::load(a.path(), Game::Anno117);
        first.stage("data/a.txt", b"mine".to_vec());
        let copy = first.install(game.path()).unwrap();
        // A fresh project gets the same default id.
        let second = Project::load(b.path(), Game::Anno117);
        assert_eq!(second.mod_id(), first.mod_id());
        assert!(!second.owns_install(&copy));
        assert!(second.install(game.path()).is_err());
        assert_eq!(std::fs::read(copy.join("data/a.txt")).unwrap(), b"mine");
        // The first still updates its own copy.
        assert!(first.owns_install(&copy));
        first.install(game.path()).unwrap();
    }

    #[cfg(not(windows))]
    #[test]
    fn install_moves_the_copy_into_the_user_folder() {
        let config = tempfile::tempdir().unwrap();
        let lib = tempfile::tempdir().unwrap();
        let game = lib.path().join("steamapps/common/Anno 117 - Pax Romana");
        std::fs::create_dir_all(&game).unwrap();
        let project = Project::load(config.path(), Game::Anno117);
        // Nothing of the prefix exists yet; install makes the folders.
        let new = project.install(&game).unwrap();
        let documents = lib
            .path()
            .join("steamapps/compatdata/3274580/pfx/drive_c/users/steamuser/Documents");
        assert!(new.starts_with(documents.join("Anno 117 - Pax Romana/mods")));
        assert!(new.join("modinfo.json").exists());
        // A copy from before installs moved, with its install mark.
        let old = game.join("mods").join(project.mod_id());
        std::fs::create_dir_all(old.parent().unwrap()).unwrap();
        std::fs::rename(&new, &old).unwrap();
        assert_eq!(project.install(&game).unwrap(), new);
        assert!(!old.exists(), "the old copy would load as a second mod");
        assert!(project.uninstall(&game, project.mod_id()).unwrap());
        assert!(!new.exists());
    }

    #[test]
    fn install_refuses_to_replace_another_mod() {
        let config = tempfile::tempdir().unwrap();
        let game = tempfile::tempdir().unwrap();
        let project = Project::load(config.path(), anno_formats::game::Game::Anno117);
        let other = game.path().join("mods").join(project.mod_id());
        std::fs::create_dir_all(&other).unwrap();
        std::fs::write(other.join("modinfo.json"), r#"{"ModID":"someone-else"}"#).unwrap();
        assert!(project.install(game.path()).is_err());
        std::fs::remove_dir_all(&other).unwrap();
        let installed = project.install(game.path()).unwrap();
        assert!(installed.join("modinfo.json").exists());
        // Reinstalling over our own copy works.
        project.install(game.path()).unwrap();
    }

    #[test]
    fn anno1800_exports_loose_files_with_its_own_game_metadata() {
        let config = tempfile::tempdir().unwrap();
        let mut project = Project::load(config.path(), Game::Anno1800);
        project.stage("data/ui/studio/generated/example", b"PHX example".to_vec());
        let folder = project.write().unwrap();
        let meta: serde_json::Value =
            serde_json::from_slice(&std::fs::read(folder.join("modinfo.json")).unwrap()).unwrap();
        assert_eq!(meta["Anno"], 7);
        assert_eq!(meta["ModID"], "editor");
        assert_eq!(
            std::fs::read(folder.join("data/ui/studio/generated/example")).unwrap(),
            b"PHX example"
        );
        assert!(!folder.join(ASSETS_XML).exists());
        let back = Project::load(config.path(), Game::Anno1800);
        assert_eq!(back.files, project.files);
        // A project must not accidentally install into the other game.
        let wrong = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(wrong.path().join("Bin/Win64")).unwrap();
        std::fs::create_dir(wrong.path().join("maindata")).unwrap();
        let mut archive = rda::Packer::new(rda::PackOptions::default());
        archive.add("data/fixture.txt", b"fixture").unwrap();
        archive
            .write(&wrong.path().join("maindata/data0.rda"))
            .unwrap();
        std::fs::write(wrong.path().join("Bin/Win64/Anno117.exe"), []).unwrap();
        assert!(project.install(wrong.path()).is_err());
        assert!(!wrong.path().join("mods").exists());
    }

    #[test]
    fn saving_rebuilds_data_and_leaves_the_rest_of_the_folder() {
        let dir = tempfile::tempdir().unwrap();
        let folder = dir.path().join("repo");
        let mut project = Project::create(&folder, "Repo", "repo", Game::Anno117).unwrap();
        std::fs::create_dir_all(folder.join(".git")).unwrap();
        std::fs::write(folder.join(".git/HEAD"), "ref").unwrap();
        std::fs::write(folder.join("README.md"), "mine").unwrap();
        std::fs::create_dir_all(folder.join("data/stale")).unwrap();
        std::fs::write(folder.join("data/stale/old.txt"), "gone").unwrap();
        project.files.clear();
        project.stage("data/a.txt", b"a".to_vec());
        project.write().unwrap();
        assert!(folder.join(".git/HEAD").exists() && folder.join("README.md").exists());
        assert!(
            !folder.join("data/stale").exists(),
            "data/ is the project’s"
        );
        let info: serde_json::Value =
            serde_json::from_slice(&std::fs::read(folder.join("modinfo.json")).unwrap()).unwrap();
        assert_eq!(info["Difficulty"], "unchanged", "the 117 loader needs it");

        // Installing leaves hidden entries (settings, .git) out.
        let game = tempfile::tempdir().unwrap();
        let installed = project.install(game.path()).unwrap();
        assert!(installed.join("data/a.txt").exists());
        assert!(installed.join("README.md").exists());
        assert!(!installed.join(".anno-editor").exists() && !installed.join(".git").exists());
    }

    #[test]
    fn a_mods_own_modops_survive_next_to_the_generated_ones() {
        let config = tempfile::tempdir().unwrap();
        let mut project = Project::load(config.path(), Game::Anno117);
        let own = "<ModOps>\n  <ModOp Type=\"merge\" GUID=\"1\" Path=\"/Values\"><X>1</X></ModOp>\n</ModOps>\n";
        project.stage(ASSETS_XML, own.as_bytes().to_vec());
        island(&mut project, "nova");
        let folder = project.write().unwrap();
        let written = std::fs::read_to_string(folder.join(ASSETS_XML)).unwrap();
        assert!(written.contains("<X>1</X>") && written.contains("RandomIsland"));
        // Reopened, the mod's part is staged and the generated part is not.
        let mut back = Project::load(config.path(), Game::Anno117);
        assert_eq!(back.files[ASSETS_XML], own.as_bytes());
        back.write().unwrap();
        let again = std::fs::read_to_string(folder.join(ASSETS_XML)).unwrap();
        assert_eq!(again, written, "saves do not stack generated blocks");
        // Without islands only the mod's own file remains.
        back.islands.clear();
        back.files.retain(|p, _| p == ASSETS_XML);
        back.write().unwrap();
        assert_eq!(
            std::fs::read_to_string(folder.join(ASSETS_XML)).unwrap(),
            own
        );
    }

    #[test]
    fn unreadable_settings_refuse_to_open_and_are_set_aside_by_load() {
        let config = tempfile::tempdir().unwrap();
        let mut project = Project::load(config.path(), Game::Anno117);
        island(&mut project, "nova");
        let folder = project.write().unwrap();
        std::fs::write(folder.join(super::SETTINGS), "{ not json").unwrap();
        assert!(Project::open(&folder, Game::Anno117).is_err());
        let back = Project::load(config.path(), Game::Anno117);
        assert!(back.islands.is_empty());
        assert!(
            folder.join(".anno-editor/project.json.broken").exists(),
            "kept for repair"
        );
    }

    #[test]
    fn settings_from_the_config_folder_move_into_the_mod() {
        let config = tempfile::tempdir().unwrap();
        std::fs::write(
            config.path().join("project.json"),
            r#"{"name":"Old Isles","islands":[{"path":"data/x/a.a7m","guid":7,"region":"Roman","size":"Small","kind":"Normal"}]}"#,
        )
        .unwrap();
        let project = Project::load(config.path(), Game::Anno117);
        assert_eq!(project.mod_id(), "old-isles");
        assert_eq!(project.info.name, "Old Isles");
        assert_eq!(project.islands.len(), 1);
        assert!(
            config
                .path()
                .join("mods/old-isles/.anno-editor/project.json")
                .exists()
        );
        assert!(
            !config.path().join("project.json").exists(),
            "moved, not copied"
        );
        let again = Project::load(config.path(), Game::Anno117);
        assert_eq!(
            (again.mod_id(), again.islands.len()),
            ("old-isles", 1),
            "reopened"
        );
    }

    #[test]
    fn the_picker_lists_recent_projects_then_the_config_mods() {
        let config = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let a = Project::create(&outside.path().join("a"), "A", "a", Game::Anno117).unwrap();
        let b = Project::create(&config.path().join("mods/b"), "B", "b", Game::Anno117).unwrap();
        super::remember(config.path(), &a.folder());
        let listed: Vec<String> = super::known(config.path(), Game::Anno117)
            .into_iter()
            .map(|s| s.name)
            .collect();
        assert_eq!(listed, ["A", "B"]);
        super::remember(config.path(), &b.folder());
        assert_eq!(super::recent(config.path()), [b.folder(), a.folder()]);
    }
}

/// A folder for display: the home folder as `~`, and a game install's
/// Steam library prefix dropped, since that part is the same for every
/// mod and is what gets cut off.
pub fn short_path(folder: &Path) -> String {
    let full = folder.display().to_string();
    if let Some(i) = full.find("/steamapps/common/") {
        return full[i + "/steamapps/common/".len()..].to_owned();
    }
    // The user mods folder in a Proton prefix: "Documents/Anno 117 - …/mods".
    if let Some(i) = full.find("/drive_c/users/steamuser/") {
        return full[i + "/drive_c/users/steamuser/".len()..].to_owned();
    }
    match std::env::var("HOME") {
        Ok(home) if !home.is_empty() && full.starts_with(&home) => {
            format!("~{}", &full[home.len()..])
        }
        _ => full,
    }
}

/// A count with thousands grouped: `280,255`.
pub fn count(n: usize) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod count_tests {
    #[test]
    fn groups_thousands() {
        assert_eq!(super::count(0), "0");
        assert_eq!(super::count(999), "999");
        assert_eq!(super::count(3853), "3,853");
        assert_eq!(super::count(280255), "280,255");
        assert_eq!(super::count(1234567), "1,234,567");
    }
}
