// Owns the mod project: one mod-loader mod folder (`modinfo.json`, loose
// files under `data/`, editor settings in `.anno-editor/`), its edited files
// by virtual path, and the list of recently opened projects. The game
// mounts `<install or Documents>/mods/<ModID>/` over its archives and
// applies ModOps to XML; patch archives in `maindata` are never mounted.
// Shipped files are never written.
pub(crate) mod models;
mod mods;
mod overview;
mod save;

pub use mods::Undeclared;
pub use overview::{Installed, ProjectUi, plural};

use crate::games::GameExt;
use anno_core::{
    files,
    game::{Game, IslandRules, Module},
    modinfo::{self, ModInfo, id_from_name, valid_id},
};
use anno_formats::{a7m::a7minfo::A7mInfo, rda};
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

#[cfg(test)]
const ASSETS_XML: &str = "data/base/config/export/assets.xml";
/// How a new island is offered to the game. Set in the wizard, edited in
/// the island's settings, stored per island in project.json.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Listing {
    /// The role's value, one of the game's `IslandRules::roles`.
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
    /// What stopped the island leaving the editor at its last save (level
    /// 5, `Islands::validate`): export, test maps and Test in game read it
    /// here rather than load every island again. Listings saved before the
    /// check existed hold none until their island is saved again.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub broken: Vec<anno_core::island::Problem>,
    /// Conversion losses the user accepted, by target game, category and
    /// the source items they took (`anno_core::island::asks`), so
    /// converting the island again asks only about what's newly lost.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub accepted: Vec<anno_core::island::Acceptance>,
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

// Tests name 117's islands; the editor starts a listing from its game's
// first role (`Listing::new`).
#[cfg(test)]
impl Default for Listing {
    fn default() -> Self {
        Self::new(crate::games::rules(Game::Anno117))
    }
}

impl Listing {
    /// A new island's listing: `rules`' first role, in the pool and on the
    /// test map.
    pub fn new(rules: &IslandRules) -> Self {
        Self {
            kind: rules.roles.first().map_or("", |r| r.value).to_owned(),
            size_override: None,
            in_pool: true,
            on_test_map: true,
            world_machine: None,
            slot_resources: BTreeMap::new(),
            auto_cliffs: Vec::new(),
            fertilities: None,
            broken: Vec::new(),
            accepted: Vec::new(),
        }
    }

    /// Whether the island takes the map's other slots in `rules`' roles.
    pub fn is_regular(&self, rules: &IslandRules) -> bool {
        rules.role(&self.kind).is_some_and(|r| r.regular)
    }

    /// Whether the island takes a player's start slot in `rules`' roles.
    pub fn is_start(&self, rules: &IslandRules) -> bool {
        rules.role(&self.kind).is_some_and(|r| r.start)
    }
}

/// An island's name: its file name without the extension.
pub(crate) fn island_name(path: &str) -> &str {
    let file = path.rsplit('/').next().unwrap_or(path);
    file.rsplit_once('.').map_or(file, |(stem, _)| stem)
}

/// A new island the project adds to the game.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct IslandEntry {
    /// Virtual path of the `.a7m`, or of 2070's `.isd`.
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

use anno_core::map::LayoutEntry;
pub use anno_core::project::MapEntry;

/// Editor settings inside the mod folder. The loader ignores it; install
/// leaves hidden entries out.
const SETTINGS: &str = ".anno-editor/project.json";
/// In a copy installed into the game: the project folder it came from.
const INSTALL_MARK: &str = ".anno-editor-source";
/// In the project: what an install is copying (`Project::prepare_install`).
const SNAPSHOT: &str = ".anno-editor/installing";
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
    /// Map layouts made in the Map workspace.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    layouts: Vec<LayoutEntry>,
    /// Particle effects made in the Model workspace, with their emitters.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    effects: Vec<crate::model::effect::Authored>,
}

#[derive(Debug)]
struct CorruptSettings(PathBuf);
impl std::fmt::Display for CorruptSettings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} is unreadable; fix or remove it", self.0.display())
    }
}

pub struct Project {
    pub(crate) module: &'static dyn Module,
    /// modinfo.json: id, name, version, dependencies.
    pub info: ModInfo,
    /// Virtual path (normalized, under `data/`) to file bytes: everything
    /// staged this session, and the small and text files read from the
    /// folder. Ask `has_file`/`read_file` when the file may be in `kept`.
    pub files: BTreeMap<String, Vec<u8>>,
    /// Large binary files (meshes, textures, island archives) read from the
    /// folder and left there: a project with ported models is gigabytes,
    /// and holding it in memory made opening a game take seconds. Saving
    /// links them into the new `data/`. A path is in `files` or `kept`,
    /// never both.
    kept: BTreeMap<String, Kept>,
    pub islands: Vec<IslandEntry>,
    /// Templates of the "Editor islands" map type; empty means no such type.
    pub maps: Vec<MapEntry>,
    /// Map layouts saved from the Map workspace, each with the GUIDs it
    /// holds.
    pub layouts: Vec<LayoutEntry>,
    /// Particle effects made in the Model workspace: their emitters, so
    /// they stay editable after the editor closes.
    pub effects: Vec<crate::model::effect::Authored>,
    guid_base: Option<u32>,
    guid_reserved: bool,
    folder: PathBuf,
    /// Why the most recent project didn't reopen, when `load` fell back to
    /// the default one. The launcher takes it and the editor shows it once.
    pub reopen_failed: Option<String>,
}

/// A file left in the project folder until it is staged over or removed.
#[derive(Clone, Debug)]
struct Kept {
    /// Relative to the project folder, with its case on disk. Normalizes to
    /// its key, so it stays valid across saves (see `write`).
    rel: Box<str>,
    size: u64,
}

/// Files at least this large stay on disk unless `read_folder` must look
/// inside them.
const KEEP_FROM: u64 = 64 << 10;

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
    pub fn create(
        folder: &Path,
        name: &str,
        id: &str,
        module: &'static dyn Module,
    ) -> Result<Self> {
        ensure!(!name.trim().is_empty(), "the project needs a name");
        ensure!(
            valid_id(id),
            "{id:?} is not a usable mod id: letters, digits, '-', '_' and '.'"
        );
        let mut project = Self::empty(folder, name.trim(), id, module);
        let roots = project.roots()?;
        let _lock = save::lock(folder)?;
        save::recover(folder, roots)?;
        ensure!(
            !folder.join("modinfo.json").exists() && !folder.join(SETTINGS).exists(),
            "{} already holds a mod; open it instead",
            folder.display()
        );

        project.guid_base = project.guid_base();
        project.write_locked(roots)?;
        Ok(project)
    }

    fn empty(folder: &Path, name: &str, id: &str, module: &'static dyn Module) -> Self {
        Self {
            module,
            info: ModInfo::new(id, name),
            files: BTreeMap::new(),
            kept: BTreeMap::new(),
            islands: Vec::new(),
            maps: Vec::new(),
            layouts: Vec::new(),
            effects: Vec::new(),
            guid_base: None,
            guid_reserved: false,
            folder: folder.to_owned(),
            reopen_failed: None,
        }
    }

    /// Opens the manifest, editor settings and game roots in `folder`. A
    /// mod without a manifest gets one on the first save. Unreadable
    /// settings are an error, not an empty project: saving over them would
    /// drop the island list.
    pub fn open(folder: &Path, module: &'static dyn Module) -> Result<Self> {
        ensure!(folder.is_dir(), "{} is not a folder", folder.display());
        let mut project = Self::empty(folder, "", "", module);
        let roots = project.roots()?;
        let _lock = save::lock(folder)?;
        save::recover(folder, roots)?;
        let manifest = folder.join("modinfo.json");
        let info = match std::fs::read(&manifest) {
            Ok(bytes) => {
                let mut info = ModInfo::parse(&bytes, module)
                    .with_context(|| manifest.display().to_string())?;
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
                    == format!("Made with Anno Editor for {}.", module.profile().title)
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
                .with_context(|| CorruptSettings(path.clone()))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Settings::default(),
            Err(e) => return Err(e).with_context(|| path.display().to_string()),
        };
        project.islands = settings.islands;
        project.maps = settings.maps;
        project.layouts = settings.layouts;
        project.effects = settings.effects;
        project.guid_base = settings.guid_base;
        project.guid_reserved = settings.guid_reserved;
        project.info = info;
        project.read_folder()?;
        Ok(project)
    }

    /// The project to start with: the last one opened (see [`remember`]),
    /// else the one the settings from before projects were folders
    /// (`<config>/project.json`) name, moved into its mod folder, else
    /// `<config>/mods/editor`. Corrupt settings at the default folder are set aside
    /// as `project.json.broken`.
    pub fn load(config: &Path, module: &'static dyn Module) -> Result<Self> {
        // Not writable: `write` refuses a game without roots.
        if module.project_roots().is_empty() {
            return Ok(Self::empty(
                &config.join("mods/editor"),
                "editor",
                "editor",
                module,
            ));
        }
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
            let _lock = save::lock(&folder)?;
            save::recover(&folder, module.project_roots())?;
            if !folder.join(SETTINGS).try_exists()? {
                migrate(&folder, &name, &id, &settings, module)?;
                std::fs::rename(&legacy_path, legacy_path.with_extension("json.moved"))?;
                remember(config, &folder);
            }
        }
        // A recent project that can't reopen mustn't lock the game out: carry
        // on with the default project and say why. Its folder stays as it is
        // on disk, recovery evidence included.
        let mut reopen_failed = None;
        if let Some(last) = recent(config).into_iter().next()
            && last != folder
        {
            match Self::open(&last, module) {
                Ok(project) => return Ok(project),
                Err(e) => {
                    reopen_failed = Some(format!("Could not reopen {}: {e:#}", last.display()))
                }
            }
        }
        let mut project = Self::load_default(config, &folder, &name, &id, module)?;
        project.reopen_failed = reopen_failed;
        Ok(project)
    }

    /// The project in the default folder: created empty when there is none
    /// (with a pre-mod-loader patch archive's files, if one is left), and
    /// recovered when its settings are unreadable.
    fn load_default(
        config: &Path,
        folder: &Path,
        name: &str,
        id: &str,
        module: &'static dyn Module,
    ) -> Result<Self> {
        if !folder.is_dir() {
            let mut project = Self::empty(folder, name, id, module);
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
            return Ok(project);
        }
        match Self::open(folder, module) {
            Err(e) if e.is::<CorruptSettings>() => {
                let path = folder.join(SETTINGS);
                let _lock = save::lock(folder)?;
                save::recover(folder, module.project_roots())?;
                // Another writer could have repaired the settings since open released its lock.
                if serde_json::from_slice::<Settings>(&std::fs::read(&path)?).is_err() {
                    std::fs::rename(&path, path.with_extension("json.broken"))?;
                }
                drop(_lock);
                Self::open(folder, module)
            }
            result => result,
        }
    }

    pub fn game(&self) -> Game {
        self.module.game()
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
        let path = rda::normalize_path(path).into_owned();
        self.kept.remove(&path);
        self.files.insert(path, bytes);
    }

    pub fn has_file(&self, path: &str) -> bool {
        self.files.contains_key(path) || self.kept.contains_key(path)
    }

    /// Staged bytes, or the folder's copy of a kept file.
    pub fn read_file(&self, path: &str) -> Option<std::borrow::Cow<'_, [u8]>> {
        if let Some(bytes) = self.files.get(path) {
            return Some(bytes.as_slice().into());
        }
        let kept = self.kept.get(path)?;
        std::fs::read(self.folder.join(&*kept.rel))
            .ok()
            .map(Into::into)
    }

    pub fn file_size(&self, path: &str) -> Option<usize> {
        (self.files.get(path).map(Vec::len))
            .or_else(|| self.kept.get(path).map(|k| k.size as usize))
    }

    pub fn file_count(&self) -> usize {
        self.files.len() + self.kept.len()
    }

    /// Every file path, sorted.
    pub fn file_paths(&self) -> Vec<&str> {
        let mut paths: Vec<&str> = (self.files.keys().chain(self.kept.keys()))
            .map(String::as_str)
            .collect();
        paths.sort_unstable();
        paths
    }

    pub fn remove_file(&mut self, path: &str) -> bool {
        self.files.remove(path).is_some() | self.kept.remove(path).is_some()
    }

    pub fn retain_files(&mut self, mut keep: impl FnMut(&str) -> bool) {
        self.files.retain(|p, _| keep(p));
        self.kept.retain(|p, _| keep(p));
    }

    /// The models among the project's files (see `port::project_models`).
    pub fn models(&self) -> Vec<crate::island::port::ProjectModel> {
        crate::island::port::project_models(self.module, &self.files, |p| self.file_size(p))
    }

    pub fn clear(&mut self) {
        self.files.clear();
        self.kept.clear();
        self.islands.clear();
        self.maps.clear();
        self.layouts.clear();
    }

    /// Replaces the play-test templates; their files are staged by the
    /// caller. Files of templates no longer listed are dropped.
    pub fn set_maps(&mut self, maps: Vec<MapEntry>) {
        let old = std::mem::replace(&mut self.maps, maps);
        for old in &old {
            let stem = old.path.trim_end_matches(".a7t");
            for ext in ["a7t", "a7tinfo", "a7te"] {
                if !self.maps.iter().any(|m| m.path == old.path) {
                    self.remove_file(&format!("{stem}.{ext}"));
                }
            }
        }
    }

    /// The folder holding everything of the island at `path`: its archive,
    /// headers and `_gamedata/<name>/`.
    fn island_dir(path: &str) -> &str {
        path.rsplit_once('/').map_or(path, |(dir, _)| dir)
    }

    /// Whether the game keeps each island in a folder of its own (a7m),
    /// rather than as one file beside others of its kind (2070's `.isd`).
    fn island_folders(&self) -> bool {
        (self.module.islands()).is_some_and(|i| i.codec().folders())
    }

    /// Staged files that belong to the island at `path`: its folder, for a
    /// game that keeps one per island, else the file alone.
    pub fn island_files(&self, path: &str) -> Vec<String> {
        if !self.island_folders() {
            return (self.file_paths().into_iter())
                .filter(|k| *k == path)
                .map(str::to_owned)
                .collect();
        }
        let prefix = format!("{}/", Self::island_dir(path));
        (self.file_paths().into_iter())
            .filter(|k| k.starts_with(&prefix))
            .map(str::to_owned)
            .collect()
    }

    /// Staged files outside every island and play-test template: edited
    /// shipped assets such as UI files.
    pub fn other_files(&self) -> Vec<String> {
        let folders = self.island_folders();
        let dirs: Vec<String> = (self.islands.iter())
            .filter(|_| folders)
            .map(|i| format!("{}/", Self::island_dir(&i.path)))
            .chain(
                self.maps
                    .iter()
                    .map(|m| format!("{}/", Self::island_dir(&m.path))),
            )
            .collect();
        (self.file_paths().into_iter())
            .filter(|k| !dirs.iter().any(|d| k.starts_with(d)))
            .filter(|k| folders || !self.islands.iter().any(|i| i.path == *k))
            .map(str::to_owned)
            .collect()
    }

    /// Drops an island and its files (`island_files`). Returns how many
    /// files went. Play-test maps naming it need rebuilding (see `playtest`).
    pub fn remove_island(&mut self, path: &str) -> usize {
        let files = self.island_files(path);
        for f in &files {
            self.remove_file(f);
        }
        self.islands.retain(|i| i.path != path);
        files.len()
    }

    /// Moves the island at `from` to the new path `to`, with the same
    /// extension: its files move with it, and an a7m island's `.a7minfo`
    /// points at the moved `.a7me`. The island keeps its GUID and
    /// settings.
    pub fn rename_island(&mut self, from: &str, to: &str) -> Result<()> {
        let to = rda::normalize_path(to).into_owned();
        let ext = |p: &str| p.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase());
        ensure!(
            ext(&to).is_some() && ext(&to) == ext(from) && island_name(&to) != "",
            "{to} is not an island path like {from}"
        );
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
        let (old_stem, new_stem) = (island_name(from).to_owned(), island_name(&to).to_owned());
        entry.path = to.clone();
        // Kept files come into memory before anything moves, so a failed
        // read leaves the project as it was: their folder copies keep the
        // old names until the next save, and `Kept::rel` must normalize to
        // its key.
        let moving = self.island_files(from);
        for old in &moving {
            if let Some(kept) = self.kept.get(old) {
                let path = self.folder.join(&*kept.rel);
                let bytes =
                    std::fs::read(&path).with_context(|| format!("Reading {}", path.display()))?;
                self.kept.remove(old);
                self.files.insert(old.clone(), bytes);
            }
        }
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
            let bytes = self.files.remove(&old).expect("listed and read above");
            self.files.insert(format!("{new_dir}/{rest}"), bytes);
        }
        let a7m = (self.module.islands()).is_some_and(|i| i.codec().a7m().is_some());
        let info = format!("{}info", to);
        if let Some(bytes) = self.files.get(&info).filter(|_| a7m) {
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
        let roots = self.roots()?;
        let _lock = save::lock(&self.folder)?;
        save::recover(&self.folder, roots)?;
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
        if self.guid_base.is_none() {
            self.guid_base = base;
        }
        self.info.id = id.to_owned();
        self.folder = new;
        self.write_locked(roots)
    }

    /// Removes `<mods>/<id>` from the install root and from `<game>/mods`,
    /// where installs went before, when it is this mod (same `ModID`).
    /// Returns whether there was one.
    pub fn uninstall(&self, game: &Path, id: &str) -> Result<bool> {
        let (Some(packaging), Some(root)) = (
            self.module.packaging(),
            modinfo::install_root(self.module, game),
        ) else {
            bail!("This game has no project installer");
        };
        let mut removed = false;
        for root in [root, game.join(packaging.mods_folder)] {
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
    /// free one in the project's range; 0 for a game without packaging,
    /// whose projects register no assets.
    // The default range is picked from the mod id, not reserved; publishing
    // needs a range from github.com/anno-mods/GuidRanges, which the
    // Overview's GUID range field sets.
    pub fn island_guid(&self, path: &str) -> u32 {
        if let Some(i) = self.islands.iter().find(|i| i.path == path) {
            return i.guid;
        }
        let (Some(base), Some(packaging)) = (self.guid_base(), self.module.packaging()) else {
            return 0;
        };
        let islands = packaging.guid_range.islands;
        (base..base + islands)
            .find(|g| self.guid_free(*g))
            .unwrap_or_else(|| panic!("the project's {islands} GUIDs are used up"))
    }

    /// No island or layout holds `guid`.
    fn guid_free(&self, guid: u32) -> bool {
        self.islands.iter().all(|i| i.guid != guid)
            && self.layouts.iter().all(|l| !l.guids.contains(&guid))
    }

    /// Adds or updates a map layout, allocating the GUIDs its registration
    /// needs. Layouts take GUIDs from the top of the islands' span down,
    /// islands from the bottom up, so neither moves the other's: a GUID
    /// once stored never changes, or installed saves would lose the map.
    pub fn set_layout(&mut self, mut entry: LayoutEntry, registers: anno_core::map::Registers) {
        // Replaces only the layout at the same path: one of the same name
        // elsewhere is another layout, which the editor refuses to save
        // over before it gets here.
        let at = self.layouts.iter().position(|l| l.path == entry.path);
        if let Some(n) = at {
            let old = self.layouts.remove(n);
            if entry.guids.is_empty() {
                entry.guids = old.guids;
            }
        }
        let want = registers.guids(entry.variants.len());
        if let (Some(base), Some(packaging)) = (self.guid_base(), self.module.packaging()) {
            let islands = packaging.guid_range.islands;
            while entry.guids.len() < want {
                let next = (base..base + islands)
                    .rev()
                    .find(|g| self.guid_free(*g) && !entry.guids.contains(g))
                    .unwrap_or_else(|| panic!("the project's {islands} GUIDs are used up"));
                entry.guids.push(next);
            }
        }
        match at {
            Some(n) => self.layouts.insert(n, entry),
            None => self.layouts.push(entry),
        }
    }

    /// Whether Test in game (Make test map on 2070) builds on the layout at
    /// `path`.
    pub fn set_layout_test(&mut self, path: &str, on: bool) {
        for l in &mut self.layouts {
            l.fill_with_islands = l.path == path && on;
        }
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

    /// The first GUID of the project's range: the reserved one, else the
    /// game's pick from the mod id. None for a game without packaging.
    pub fn guid_base(&self) -> Option<u32> {
        let range = &self.module.packaging()?.guid_range;
        Some(self.guid_base.unwrap_or_else(|| range.pick(self.mod_id())))
    }

    /// The first GUID the test maps own, after the islands': 117's map
    /// type, and the label's text LineId in every game.
    pub fn map_type_guid(&self) -> Option<u32> {
        Some(self.guid_base()? + self.module.packaging()?.guid_range.islands)
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
    /// project's copy (see `owns_install`). The editor runs the two halves
    /// apart, the copy off the UI thread (`Editor::install_project`).
    #[cfg(test)]
    pub fn install(&self, game: &Path) -> Result<PathBuf> {
        self.prepare_install(game)?.run()
    }

    /// Checks, writes the project and snapshots what `write` wrote, for
    /// `Installing::run` to copy into the game. Quick: the snapshot is hard
    /// links, so a save made while the copy runs cannot change what lands
    /// in the game.
    pub fn prepare_install(&self, game: &Path) -> Result<Installing> {
        let packaging = self
            .module
            .packaging()
            .context("This game has no project installer")?;
        if let Some(found) = Game::detect(game) {
            ensure!(
                found == self.game(),
                "this project belongs to {}, not {}",
                self.module.profile().title,
                found.profile().title
            );
        }
        let target = modinfo::install_root(self.module, game)
            .context("This game has no project installer")?
            .join(self.mod_id());
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
        let snapshot = source.join(SNAPSHOT);
        if snapshot.exists() {
            std::fs::remove_dir_all(&snapshot)?;
        }
        link_dir(&source, &snapshot)?;
        // Installs used to go to `<game>/mods`. The loader reads both
        // folders, so our old copy there would load as a second mod.
        let old = game.join(packaging.mods_folder).join(self.mod_id());
        let old = (old != target && old.exists() && self.owns_install(&old)).then_some(old);
        Ok(Installing {
            source: std::fs::canonicalize(&source).unwrap_or(source),
            snapshot,
            target,
            old,
        })
    }

    /// ModOps written from `islands` and `maps` rather than staged: each
    /// file's patches without the `<ModOps>` wrapper (see `merge_modops`).
    fn generated(&self) -> Vec<(String, String)> {
        let mut out = Vec::new();
        // Pools and test maps are asset ModOps, which only a game with a
        // mod loader reads: 2070 has a database but no packaging.
        let Some(database) = self
            .module
            .database()
            .filter(|_| self.module.packaging().is_some())
        else {
            return out;
        };
        if self.islands.iter().any(|i| i.listing.in_pool)
            || !self.maps.is_empty()
            || !self.layouts.is_empty()
        {
            out.push((database.assets_path.to_owned(), self.assets_modops()));
        }
        // Map type names, English in every language: the test maps' type,
        // then each layout that registers a type of its own (117), named
        // as the layout.
        let mut names: Vec<(u32, &str)> = Vec::new();
        if let Some(id) = self.map_type_guid()
            && !self.maps.is_empty()
        {
            names.push((id, "Editor islands"));
        }
        for l in &self.layouts {
            if let Some(&id) = l.guids.first() {
                names.push((id, &l.name));
            }
        }
        if !names.is_empty() {
            let key = database.text_key;
            let xml: String = (names.iter())
                .map(|(id, name)| {
                    format!(
                        "  <ModOp Type=\"add\" Path=\"/TextExport/Texts\">\n    <Text>\n      <{key}>{id}</{key}>\n      <Text>{}</Text>\n    </Text>\n  </ModOp>\n",
                        xml_text(name)
                    )
                })
                .collect();
            for &(_, path) in database.languages {
                out.push((path.to_owned(), xml.clone()));
            }
        }
        out
    }

    fn assets_modops(&self) -> String {
        let mut xml = String::new();
        if let Some(ty) = self.map_type_guid()
            && (!self.maps.is_empty() || !self.layouts.is_empty())
        {
            xml += &self.map_modops(ty);
        }
        let Some(rules) = self.module.islands().map(|i| i.rules()) else {
            return xml;
        };
        if !self.islands.iter().any(|i| i.listing.in_pool) {
            return xml;
        }
        let anchor = rules.pool_anchor_guid;
        xml += &format!("  <ModOp Type=\"addNextSibling\" GUID=\"{anchor}\">\n");
        for i in self.islands.iter().filter(|i| i.listing.in_pool) {
            let name = island_name(&i.path);
            xml += &format!(
                "    <Asset>\n      <Template>RandomIsland</Template>\n      <Values>\n        <Standard>\n          <GUID>{}</GUID>\n          <Name>{name}</Name>\n        </Standard>\n        <RandomIsland>\n          <FilePath>{}</FilePath>\n          <IslandRegion>{}</IslandRegion>\n          <IslandDifficulty>Normal;Hard</IslandDifficulty>\n          <IslandType>{}</IslandType>\n          {}\n        </RandomIsland>\n      </Values>\n    </Asset>\n",
                i.guid,
                i.path,
                i.region,
                i.listing.kind,
                if rules.placeholder_size {
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

    /// The game's ModOps for the layouts and the test maps, these
    /// numbered from `first_guid`.
    fn map_modops(&self, first_guid: u32) -> String {
        (self.module.islands())
            .and_then(|i| i.maps())
            .map_or_else(String::new, |m| {
                m.register(&self.layouts, &self.maps, first_guid)
            })
    }

    /// Stages `data/` from the folder. XML the editor generated is cut out
    /// of its file between the markers; a file left with nothing else is
    /// not staged. Files at generated paths without markers were written
    /// before the markers existed and are regenerated, not staged.
    fn read_folder(&mut self) -> Result<()> {
        let generated: Vec<String> = self.generated().into_iter().map(|(p, _)| p).collect();
        let root = self.folder.clone();
        let mut stack = Vec::new();
        let roots = self.roots()?;
        for island in &self.islands {
            let (name, _) = save::split_path(&island.path, roots)?;
            ensure!(
                root.join(name).is_dir(),
                "missing project root {name} for {}",
                island.path
            );
        }
        for name in roots {
            let dir = root.join(name);
            if dir.try_exists()? {
                stack.push(dir);
            }
        }
        while let Some(dir) = stack.pop() {
            let entries =
                std::fs::read_dir(&dir).with_context(|| format!("Reading {}", dir.display()))?;
            for entry in entries {
                let path = entry?.path();
                // Follows links, as reading the file does.
                let meta = std::fs::metadata(&path)?;
                if meta.is_dir() {
                    stack.push(path);
                    continue;
                }
                let on_disk = path
                    .strip_prefix(&root)?
                    .to_string_lossy()
                    .replace('\\', "/");
                let rel = rda::normalize_path(&on_disk).into_owned();
                // Text is read now: XML for the generated block, the game's
                // model files for what they reference (`models`, which reads
                // only `files`), headers for renames and play tests.
                let text = [".xml", ".a7minfo", ".a7tinfo"]
                    .iter()
                    .any(|ext| rel.ends_with(ext))
                    || files::kind_of(self.module, &rel).is_some_and(|k| k.deps.is_some());
                let size = meta.len();
                if !text && size >= KEEP_FROM {
                    self.files.remove(&rel);
                    self.kept.insert(
                        rel,
                        Kept {
                            rel: on_disk.into(),
                            size,
                        },
                    );
                    continue;
                }
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
fn migrate(
    folder: &Path,
    name: &str,
    id: &str,
    settings: &Settings,
    module: &dyn Module,
) -> Result<()> {
    let roots = module.project_roots();
    for island in &settings.islands {
        let (root, _) = save::split_path(&island.path, roots)?;
        ensure!(
            folder.join(root).is_dir(),
            "missing project root {root} for {}",
            island.path
        );
    }
    std::fs::create_dir_all(folder.join(".anno-editor"))?;
    let manifest = folder.join("modinfo.json");
    if !manifest.exists() {
        save::atomic(&manifest, ModInfo::new(id, name).encode(module).as_bytes())?;
    }
    let moved = Settings {
        name: String::new(),
        islands: settings.islands.clone(),
        guid_base: settings.guid_base,
        guid_reserved: settings.guid_reserved,
        maps: settings.maps.clone(),
        layouts: settings.layouts.clone(),
        effects: settings.effects.clone(),
    };
    save::atomic(&folder.join(SETTINGS), &serde_json::to_vec_pretty(&moved)?)
}

/// `text` as XML character data.
fn xml_text(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
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
    // A copy written after our file last changed holds what it holds:
    // install copies, and saves link kept files without touching them, so
    // only files a save rewrote are read, and those are the small ones.
    let same = |ours: &Path, theirs: &Path| {
        let (Ok(a), Ok(b)) = (std::fs::metadata(ours), std::fs::metadata(theirs)) else {
            return false;
        };
        if a.len() != b.len() {
            return false;
        }
        match (a.modified(), b.modified()) {
            (Ok(changed), Ok(copied)) if changed <= copied => true,
            _ => std::fs::read(ours).ok() == std::fs::read(theirs).ok(),
        }
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

/// A written project on its way into the game (`Project::prepare_install`).
pub struct Installing {
    /// The project folder, as the install mark names it.
    source: PathBuf,
    snapshot: PathBuf,
    target: PathBuf,
    /// Our copy in `<game>/mods`, where installs used to go.
    old: Option<PathBuf>,
}

impl Installing {
    /// Brings the game's copy in line with the snapshot: files it already
    /// has unchanged stay, changed ones are copied, ones the project no
    /// longer has go. Deleting and copying all of it took 2.8 GB of an
    /// 1800 project across drives for every install. Slow when much
    /// changed, so the editor runs it off the UI thread.
    pub fn run(self) -> Result<PathBuf> {
        let done = (|| -> Result<PathBuf> {
            sync_dir(&self.snapshot, &self.target)?;
            let mark = self.source.to_string_lossy();
            std::fs::write(self.target.join(INSTALL_MARK), mark.as_bytes())?;
            if let Some(old) = &self.old {
                std::fs::remove_dir_all(old)?;
            }
            Ok(self.target.clone())
        })();
        let _ = std::fs::remove_dir_all(&self.snapshot);
        done
    }
}

/// Hard links of `from`'s files under `to`, without hidden entries; a copy
/// where a link cannot be made.
fn link_dir(from: &Path, to: &Path) -> Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        if entry.file_name().to_string_lossy().starts_with('.') {
            continue;
        }
        let target = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            link_dir(&entry.path(), &target)?;
        } else if std::fs::hard_link(entry.path(), &target).is_err() {
            std::fs::copy(entry.path(), &target)
                .with_context(|| format!("Copying to {}", target.display()))?;
        }
    }
    Ok(())
}

/// Makes `to` hold what `from` holds, leaving hidden entries on both sides
/// alone. A file stays when its size and modification time match, which
/// copies made here keep from their source (within a microsecond, for file
/// systems that round). Copies, never links: a link would let an edit to
/// the game's copy change the project.
fn sync_dir(from: &Path, to: &Path) -> Result<()> {
    use std::collections::HashSet;
    std::fs::create_dir_all(to)?;
    let hidden = |name: &std::ffi::OsStr| name.to_string_lossy().starts_with('.');
    let mut wanted = HashSet::new();
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let name = entry.file_name();
        if hidden(&name) {
            continue;
        }
        let target = to.join(&name);
        wanted.insert(name);
        let there = std::fs::symlink_metadata(&target).ok();
        if entry.file_type()?.is_dir() {
            if there.as_ref().is_some_and(|m| !m.is_dir()) {
                std::fs::remove_file(&target)?;
            }
            sync_dir(&entry.path(), &target)?;
            continue;
        }
        let meta = entry.metadata()?;
        let modified = meta.modified()?;
        let same = there.as_ref().is_some_and(|t| {
            t.is_file()
                && t.len() == meta.len()
                && t.modified().ok().is_some_and(|m| {
                    let gap = m
                        .duration_since(modified)
                        .or_else(|e| Ok::<_, ()>(e.duration()));
                    gap.is_ok_and(|g| g < std::time::Duration::from_micros(1))
                })
        });
        if same {
            continue;
        }
        if there.as_ref().is_some_and(|m| m.is_dir()) {
            std::fs::remove_dir_all(&target)?;
        }
        std::fs::copy(entry.path(), &target)
            .with_context(|| format!("Copying to {}", target.display()))?;
        std::fs::File::options()
            .write(true)
            .open(&target)?
            .set_modified(modified)?;
    }
    for entry in std::fs::read_dir(to)? {
        let entry = entry?;
        let name = entry.file_name();
        if hidden(&name) || wanted.contains(&name) {
            continue;
        }
        if entry.file_type()?.is_dir() {
            std::fs::remove_dir_all(entry.path())?;
        } else {
            std::fs::remove_file(entry.path())?;
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
pub fn known(config: &Path, module: &dyn Module) -> Vec<Summary> {
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
                .and_then(|b| ModInfo::parse(&b, module).ok())
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
        // Files are stamped as an install then an edit leave them: the copy
        // at install time, the project's after it when edited since.
        let installed = std::time::SystemTime::now() - std::time::Duration::from_secs(60);
        let write = |root: &std::path::Path, rel: &str, body: &str, at| {
            let path = root.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, body).unwrap();
            let file = std::fs::File::options().write(true).open(&path).unwrap();
            file.set_modified(at).unwrap();
        };
        let before = installed - std::time::Duration::from_secs(60);
        let after = installed + std::time::Duration::from_secs(30);
        write(project.path(), "modinfo.json", "{}", before);
        write(copy.path(), "modinfo.json", "{}", installed);
        write(
            project.path(),
            "data/islands/cove.a7m",
            "new terrain",
            after,
        );
        write(
            copy.path(),
            "data/islands/cove.a7m",
            "old terrain",
            installed,
        );
        // Rewritten by a save with the same bytes: read, and the same.
        write(project.path(), "data/same.xml", "z", after);
        write(copy.path(), "data/same.xml", "z", installed);
        write(project.path(), "data/new.xml", "x", after);
        write(copy.path(), "data/gone.xml", "y", installed);
        write(project.path(), ".editor/settings.json", "hidden", after);
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
        let mut project = super::Project::load(config.path(), &anno1800::MODULE).unwrap();
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
        let back = super::Project::load(config.path(), &anno1800::MODULE).unwrap();
        assert_eq!(back.info.name, project.info.name);
        assert_eq!(back.islands, project.islands);
    }

    use super::{ASSETS_XML, IslandEntry, Listing, Project, rda};

    fn island(project: &mut Project, name: &str) -> String {
        let path = format!("data/base/provinces/roman/islands/custom/{name}/{name}.a7m");
        let stem = path.trim_end_matches(".a7m");
        let dir = format!("data/base/provinces/roman/islands/custom/{name}");
        project.stage(&path, b"archive".to_vec());
        let info = anno_formats::a7m::a7minfo::A7mInfo::new([128, 128], &format!("{stem}.a7me"));
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
        let mut project = Project::load(config.path(), &anno117::MODULE).unwrap();
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
        let info = anno_formats::a7m::a7minfo::A7mInfo::parse(
            &project.files[&format!("{dir}/magna.a7minfo")],
        )
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
    fn large_files_stay_on_disk_across_reopen_save_and_rename() {
        let config = tempfile::tempdir().unwrap();
        let mut project = Project::load(config.path(), &anno117::MODULE).unwrap();
        let old = island(&mut project, "nova");
        let big: Vec<u8> = (0..super::KEEP_FROM as usize + 1)
            .map(|i| i as u8)
            .collect();
        project.stage(&old, big.clone());
        let texture = "data/graphics/x/maps/x_diff_0.dds";
        project.stage(texture, big.clone());
        let folder = project.write().unwrap();
        // A file whose name has capitals on disk stays where it is.
        std::fs::rename(
            folder.join(texture),
            folder.join("data/graphics/x/maps/X_Diff_0.dds"),
        )
        .unwrap();

        let mut back = Project::load(config.path(), &anno117::MODULE).unwrap();
        assert!(!back.files.contains_key(texture), "left on disk");
        assert!(back.has_file(texture));
        assert_eq!(back.file_size(texture), Some(big.len()));
        assert_eq!(back.read_file(texture).unwrap().as_ref(), big.as_slice());
        assert_eq!(back.file_count(), project.file_count());

        back.write().unwrap();
        assert_eq!(back.read_file(texture).unwrap().as_ref(), big.as_slice());
        let reopened = Project::load(config.path(), &anno117::MODULE).unwrap();
        assert_eq!(reopened.read_file(&old).unwrap().as_ref(), big.as_slice());

        let new = "data/base/provinces/roman/islands/custom/magna/magna.a7m";
        back.rename_island(&old, new).unwrap();
        assert_eq!(back.read_file(new).unwrap().as_ref(), big.as_slice());
        assert!(!back.has_file(&old));
        back.write().unwrap();
        assert_eq!(std::fs::read(folder.join(new)).unwrap(), big);
        assert!(!folder.join(&old).exists());

        assert!(back.remove_file(texture));
        back.write().unwrap();
        assert!(!folder.join("data/graphics/x/maps/X_Diff_0.dds").exists());
    }

    #[test]
    fn a_folder_named_after_the_id_follows_it() {
        let config = tempfile::tempdir().unwrap();
        let mut project = Project::load(config.path(), &anno117::MODULE).unwrap();
        island(&mut project, "nova");
        let old = project.write().unwrap();
        let guid = project.map_type_guid();
        project.info.name = "Latium Isles".into();
        let new = project.set_mod_id("latium-isles").unwrap();
        assert!(new.join("modinfo.json").exists() && !old.exists());
        assert_eq!(project.map_type_guid(), guid, "GUIDs stay with the project");
        let back = Project::open(&new, &anno117::MODULE).unwrap();
        assert_eq!(back.info.name, "Latium Isles");
        assert_eq!(back.mod_id(), "latium-isles");
        assert_eq!(back.islands.len(), 1);
        assert_eq!(back.files.len(), project.files.len());
        // A folder with another name (a repository) stays put.
        let repo = config.path().join("my-repo");
        let mut other = Project::create(&repo, "Other", "other", &anno117::MODULE).unwrap();
        assert_eq!(other.set_mod_id("other-2").unwrap(), repo);

        // Uninstall only removes this mod's folder.
        let game = tempfile::tempdir().unwrap();
        assert!(!project.uninstall(game.path(), project.mod_id()).unwrap());
        project.install(game.path()).unwrap();
        assert!(project.uninstall(game.path(), project.mod_id()).unwrap());
        assert!(!game.path().join("mods/latium-isles").exists());
    }

    // A game the registry doesn't know names its islands' folder, pool
    // anchor and size class from its own rules: through 117's the entry
    // would anchor on 42366 under data/base/provinces/ and say Large.
    #[test]
    fn a_skeleton_pool_island_takes_its_own_folder_anchor_and_size_class() {
        use crate::games::fixtures::SKELETON;
        use anno_core::game::Module;
        let dir = tempfile::tempdir().unwrap();
        let mut p =
            Project::create(&dir.path().join("reefs"), "Reefs", "reefs", &SKELETON).unwrap();
        let rules = SKELETON.islands().unwrap().rules();
        let region = rules.island_region("content/isles/reef/coral/coral.a7m");
        assert_eq!(
            (region, rules.island_region("elsewhere.a7m")),
            ("Reef", "Shoal")
        );
        let path = crate::island::island_path(SKELETON.islands().unwrap(), region, "Coral Isle");
        assert_eq!(path, "content/isles/Reef/made/coral_isle.ski");
        assert_eq!(rules.province_short("reef"), "Coral Sea");
        // A new island takes the skeleton's first role, and its roles say
        // which take a start slot.
        let listing = Listing::new(rules);
        assert_eq!(listing.kind, "Lagoon");
        assert!(listing.is_regular(rules) && !listing.is_start(rules));
        let landing = Listing {
            kind: "Landing".into(),
            ..Listing::new(rules)
        };
        assert!(landing.is_start(rules));
        assert!(!landing.is_start(crate::games::rules(anno_core::game::Game::Anno117)));
        p.register_island(IslandEntry {
            path,
            guid: 1,
            region: region.into(),
            size: rules.size_class(150).into(),
            listing,
        });
        let xml = p.assets_modops();
        assert!(xml.contains("<IslandType>Lagoon</IslandType>"), "{xml}");
        assert!(xml.contains("GUID=\"910100\""), "{xml}");
        assert!(
            xml.contains("<IslandPlaceholderSize>Atoll</IslandPlaceholderSize>"),
            "{xml}"
        );
    }

    #[test]
    fn islands_out_of_the_pool_get_no_pool_entry() {
        let config = tempfile::tempdir().unwrap();
        // project.json as written before island settings existed.
        let island = config.path().join("mods/old/data/x");
        std::fs::create_dir_all(&island).unwrap();
        std::fs::write(island.join("a.a7m"), b"island").unwrap();
        std::fs::write(
            config.path().join("project.json"),
            r#"{"name":"old","islands":[{"path":"data/x/a.a7m","guid":7,"region":"Roman","size":"Small","kind":"Normal"}]}"#,
        )
        .unwrap();
        let mut project = Project::load(config.path(), &anno117::MODULE).unwrap();
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
            Project::load(config.path(), &anno117::MODULE)
                .unwrap()
                .islands,
            project.islands
        );
    }

    #[test]
    fn a_project_guid_range_comes_from_the_game() {
        use crate::games::fixtures::{PACKAGING, PartialGame};
        let dir = tempfile::tempdir().unwrap();
        let game = &PartialGame::Packaged;
        let mut p = Project::create(&dir.path().join("a"), "Ranged", "ranged", game).unwrap();
        let base = p.guid_base().unwrap();
        assert_eq!(base, PACKAGING.guid_range.pick("ranged"));
        assert!(
            (7_000_000..7_000_500).contains(&base) && base % 50 == 0,
            "{base}"
        );
        assert_eq!(p.island_guid("data/x/x.a7m"), base);
        // The game's difficulties, its first the default.
        let manifest = |p: &Project| -> serde_json::Value {
            let bytes = std::fs::read(p.folder().join("modinfo.json")).unwrap();
            serde_json::from_slice(&bytes).unwrap()
        };
        assert_eq!(manifest(&p)["Difficulty"], "calm");
        p.info.difficulty = "wild".into();
        p.write().unwrap();
        assert_eq!(manifest(&p)["Difficulty"], "wild");
        // Without packaging a project adds no assets: no range, no GUIDs,
        // no loader fields.
        let bare = &PartialGame::Islands;
        let p = Project::create(&dir.path().join("b"), "Bare", "bare", bare).unwrap();
        assert_eq!(p.guid_base(), None);
        assert_eq!(p.island_guid("data/x/x.a7m"), 0);
        assert!(manifest(&p).get("Difficulty").is_none());
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
        let mut project = Project::create(&folder, "Ranged", "ranged", &anno117::MODULE).unwrap();
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
        let back = Project::open(&project.write().unwrap(), &anno117::MODULE).unwrap();
        assert_eq!(
            back.guid_base(),
            Some(1_938_924_000),
            "the range is written"
        );
        assert!(back.guid_reserved(), "and marked as reserved");
    }

    #[test]
    fn writes_a_mod_folder_that_reads_back() {
        let config = tempfile::tempdir().unwrap();
        let folder = config.path().join("mods/my-islands");
        let mut project =
            Project::create(&folder, "My Islands!", "my-islands", &anno117::MODULE).unwrap();
        assert!(Project::create(&folder, "Again", "again", &anno117::MODULE).is_err());
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
        let back = Project::open(&folder, &anno117::MODULE).unwrap();
        assert_eq!(back.info.name, "My Islands!");
        assert_eq!(back.files.len(), 1);
        assert_eq!(back.files["data/a/b.txt"], b"hello");
        assert_eq!(back.islands, project.islands);
    }

    // A second install copies only what changed: unchanged files stay the
    // same files on disk, a changed one is replaced, a dropped one goes,
    // and the project's install snapshot is gone afterwards.
    #[cfg(unix)]
    #[test]
    fn installing_again_copies_only_what_changed() {
        use std::os::unix::fs::MetadataExt;
        let game = tempfile::tempdir().unwrap();
        let folder = tempfile::tempdir().unwrap();
        let mut project = Project::load(folder.path(), &anno117::MODULE).unwrap();
        project.stage("data/same.txt", b"same".to_vec());
        project.stage("data/changes.txt", b"before".to_vec());
        project.stage("data/goes.txt", b"gone soon".to_vec());
        let copy = project.install(game.path()).unwrap();
        let inode = |p: &str| std::fs::metadata(copy.join(p)).unwrap().ino();
        let same = inode("data/same.txt");

        project.stage("data/changes.txt", b"after!".to_vec());
        project.remove_file("data/goes.txt");
        assert_eq!(project.install(game.path()).unwrap(), copy);
        assert_eq!(
            inode("data/same.txt"),
            same,
            "unchanged file not copied again"
        );
        assert_eq!(
            std::fs::read(copy.join("data/changes.txt")).unwrap(),
            b"after!"
        );
        assert!(!copy.join("data/goes.txt").exists());
        assert!(project.owns_install(&copy));
        assert!(!folder.path().join(super::SNAPSHOT).exists());
    }

    #[test]
    fn install_keeps_another_project_with_the_same_id() {
        let game = tempfile::tempdir().unwrap();
        let (a, b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let mut first = Project::load(a.path(), &anno117::MODULE).unwrap();
        first.stage("data/a.txt", b"mine".to_vec());
        let copy = first.install(game.path()).unwrap();
        // A fresh project gets the same default id.
        let second = Project::load(b.path(), &anno117::MODULE).unwrap();
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
        let project = Project::load(config.path(), &anno117::MODULE).unwrap();
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
        let project = Project::load(config.path(), &anno117::MODULE).unwrap();
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
        let mut project = Project::load(config.path(), &anno1800::MODULE).unwrap();
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
        let back = Project::load(config.path(), &anno1800::MODULE).unwrap();
        assert_eq!(back.files, project.files);
        // A project must not accidentally install into the other game.
        let wrong = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(wrong.path().join("Bin/Win64")).unwrap();
        std::fs::create_dir(wrong.path().join("maindata")).unwrap();
        let mut archive = rda::Packer::new(rda::PackOptions::default());
        archive.add("data/fixture.txt", b"fixture").unwrap();
        std::fs::write(
            wrong.path().join("maindata/data0.rda"),
            archive.encode().unwrap(),
        )
        .unwrap();
        std::fs::write(wrong.path().join("Bin/Win64/Anno117.exe"), []).unwrap();
        assert!(project.install(wrong.path()).is_err());
        assert!(!wrong.path().join("mods").exists());
    }

    #[test]
    fn saving_rebuilds_data_and_leaves_the_rest_of_the_folder() {
        let dir = tempfile::tempdir().unwrap();
        let folder = dir.path().join("repo");
        let mut project = Project::create(&folder, "Repo", "repo", &anno117::MODULE).unwrap();
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
        let mut project = Project::load(config.path(), &anno117::MODULE).unwrap();
        let own = "<ModOps>\n  <ModOp Type=\"merge\" GUID=\"1\" Path=\"/Values\"><X>1</X></ModOp>\n</ModOps>\n";
        project.stage(ASSETS_XML, own.as_bytes().to_vec());
        island(&mut project, "nova");
        let folder = project.write().unwrap();
        let written = std::fs::read_to_string(folder.join(ASSETS_XML)).unwrap();
        assert!(written.contains("<X>1</X>") && written.contains("RandomIsland"));
        // Reopened, the mod's part is staged and the generated part is not.
        let mut back = Project::load(config.path(), &anno117::MODULE).unwrap();
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
        let mut project = Project::load(config.path(), &anno117::MODULE).unwrap();
        island(&mut project, "nova");
        let folder = project.write().unwrap();
        std::fs::write(folder.join(super::SETTINGS), "{ not json").unwrap();
        assert!(Project::open(&folder, &anno117::MODULE).is_err());
        let back = Project::load(config.path(), &anno117::MODULE).unwrap();
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
        let root = config.path().join("mods/old-isles/data/x");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("a.a7m"), b"island").unwrap();
        let project = Project::load(config.path(), &anno117::MODULE).unwrap();
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
        let again = Project::load(config.path(), &anno117::MODULE).unwrap();
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
        let a = Project::create(&outside.path().join("a"), "A", "a", &anno117::MODULE).unwrap();
        let b = Project::create(&config.path().join("mods/b"), "B", "b", &anno117::MODULE).unwrap();
        super::remember(config.path(), &a.folder());
        let listed: Vec<String> = super::known(config.path(), &anno117::MODULE)
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
