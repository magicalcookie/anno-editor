// Owns `modinfo.json`, the mod loader's manifest, and finding the mods a
// player has installed. Spec: https://jakobharder.github.io/anno-mod-loader/modinfo/
//
// Fields the editor does not edit are kept as read, so a mod made elsewhere
// keeps its languages, GameSetup and development settings.
use crate::game::{Dependencies, Module};
use anyhow::{Context, Result, bail};
use serde_json::{Map, Value, json};
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, PartialEq)]
pub struct ModInfo {
    pub id: String,
    /// `ModName.English`.
    pub name: String,
    /// `MAJOR.minor.patch`.
    pub version: String,
    pub creator: String,
    /// `Description.English`.
    pub description: String,
    /// `Category.English`.
    pub category: String,
    /// One of the game's `Packaging::difficulties`; empty for its default,
    /// or for a game whose manifest has none.
    pub difficulty: String,
    /// Mod ids this one needs: 117 `Dependencies.Require`, 1800
    /// `ModDependencies`.
    pub requires: Vec<String>,
    /// Everything as read, written back under the edited fields.
    raw: Map<String, Value>,
}

impl ModInfo {
    pub fn new(id: &str, name: &str) -> Self {
        Self {
            id: id.to_owned(),
            name: name.to_owned(),
            version: "1.0.0".to_owned(),
            creator: String::new(),
            // Empty, the field's placeholder asks for one: a default line
            // stored as the value shipped to mod managers unnoticed.
            description: String::new(),
            category: "Misc".to_owned(),
            difficulty: String::new(),
            requires: Vec::new(),
            raw: Map::new(),
        }
    }

    pub fn parse(bytes: &[u8], module: &dyn Module) -> Result<Self> {
        let profile = module.profile();
        let Value::Object(raw) =
            serde_json::from_slice(strip_bom(bytes)).context("modinfo.json is not JSON")?
        else {
            bail!("modinfo.json is not a JSON object");
        };
        let text = |key: &str| -> String {
            match raw.get(key) {
                Some(Value::String(s)) => s.clone(),
                Some(Value::Object(by_lang)) => by_lang
                    .get("English")
                    .or_else(|| by_lang.values().next())
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
                _ => String::new(),
            }
        };
        let ids = |v: Option<&Value>| -> Vec<String> {
            v.and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(str::to_owned))
                        .collect()
                })
                .unwrap_or_default()
        };
        let requires = match profile.dependencies {
            Dependencies::Require => ids(raw.get("Dependencies").and_then(|d| d.get("Require"))),
            Dependencies::ModDependencies => ids(raw.get("ModDependencies")),
            Dependencies::None => Vec::new(),
        };
        Ok(Self {
            id: text("ModID"),
            name: text("ModName"),
            version: text("Version"),
            creator: text("CreatorName"),
            description: text("Description"),
            category: text("Category"),
            // A mod without one loads as the default once written.
            difficulty: match text("Difficulty") {
                d if d.is_empty() => difficulties(module)
                    .first()
                    .copied()
                    .unwrap_or("")
                    .to_owned(),
                d => d,
            },
            requires,
            raw,
        })
    }

    /// The manifest with the edited fields over what was read. Localized
    /// fields keep their other languages.
    pub fn encode(&self, module: &dyn Module) -> String {
        let profile = module.profile();
        let mut out = self.raw.clone();
        let localized =
            |out: &mut Map<String, Value>, key: &str, value: &str| match out.get_mut(key) {
                Some(Value::Object(by_lang)) => {
                    by_lang.insert("English".into(), value.into());
                }
                _ => {
                    out.insert(key.into(), json!({ "English": value }));
                }
            };
        out.insert("Anno".into(), profile.modinfo_anno.into());
        out.insert("ModID".into(), self.id.as_str().into());
        out.insert("Version".into(), self.version.as_str().into());
        localized(&mut out, "ModName", &self.name);
        localized(&mut out, "Category", &self.category);
        localized(&mut out, "Description", &self.description);
        if self.creator.is_empty() {
            out.remove("CreatorName");
        } else {
            out.insert("CreatorName".into(), self.creator.as_str().into());
        }
        let requires: Vec<Value> = self.requires.iter().map(|r| r.as_str().into()).collect();
        if let Some(&default) = difficulties(module).first() {
            let difficulty = if self.difficulty.is_empty() {
                default
            } else {
                &self.difficulty
            };
            out.insert("Difficulty".into(), difficulty.into());
        }
        match profile.dependencies {
            Dependencies::Require => {
                let deps = out
                    .entry("Dependencies")
                    .or_insert_with(|| Value::Object(Map::new()));
                if let Value::Object(deps) = deps {
                    if requires.is_empty() {
                        deps.remove("Require");
                    } else {
                        deps.insert("Require".into(), requires.into());
                    }
                }
                if out
                    .get("Dependencies")
                    .and_then(Value::as_object)
                    .is_some_and(Map::is_empty)
                {
                    out.remove("Dependencies");
                }
            }
            Dependencies::ModDependencies => {
                if requires.is_empty() {
                    out.remove("ModDependencies");
                } else {
                    out.insert("ModDependencies".into(), requires.into());
                }
            }
            Dependencies::None => {}
        }
        serde_json::to_string_pretty(&Value::Object(out)).expect("modinfo serializes")
    }
}

/// The game's `Difficulty` values, default first; none without packaging.
pub fn difficulties(module: &dyn Module) -> &'static [&'static str] {
    module.packaging().map_or(&[], |p| p.difficulties)
}

fn strip_bom(bytes: &[u8]) -> &[u8] {
    bytes.strip_prefix(b"\xef\xbb\xbf").unwrap_or(bytes)
}

/// A mod id from a display name: lowercase letters, digits and dashes.
pub fn id_from_name(name: &str) -> String {
    let id: String = name
        .trim()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    let mut out = String::with_capacity(id.len());
    for c in id.trim_matches('-').chars() {
        if !(c == '-' && out.ends_with('-')) {
            out.push(c);
        }
    }
    out
}

/// Whether `id` is safe as a mod id and folder name.
pub fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && !id.starts_with(['.', '-'])
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))
}

/// Where a mod was found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    /// The install's mods folder.
    Game,
    /// The mods folder in `Documents/<game>`.
    Documents,
    /// mod.io's download folder.
    ModIo,
}

impl Source {
    pub fn label(self) -> &'static str {
        match self {
            Self::Game => "game’s mods folder",
            Self::Documents => "Documents",
            Self::ModIo => "mod.io",
        }
    }
}

/// One installed mod.
#[derive(Clone, Debug)]
pub struct Installed {
    pub info: ModInfo,
    pub dir: PathBuf,
    pub source: Source,
    /// The mod loader skips folders whose name starts with `-`.
    pub enabled: bool,
}

/// Folders the mod loader searches, in its order; none for a game without
/// one. Under Proton the Windows user folders live in the game's prefix next
/// to `steamapps/common`.
pub fn roots(module: &dyn Module, install: &Path) -> Vec<(Source, PathBuf)> {
    let Some(packaging) = module.packaging() else {
        return Vec::new();
    };
    let profile = module.profile();
    let mods = install.join(packaging.mods_folder);
    let mut out = vec![
        (Source::Game, mods.clone()),
        (Source::Game, mods.join(".ubi")),
    ];
    out.extend(user_root(module, install).map(|r| (Source::Documents, r)));
    let public = if cfg!(windows) {
        std::env::var_os("PUBLIC").map(PathBuf::from)
    } else {
        proton_users(profile, install).map(|u| u.join("Public"))
    };
    // mod.io's own download layout, not the loader's folder name.
    let modio = format!("mod.io/{}/mods", profile.modio_game);
    out.extend(public.map(|p| (Source::ModIo, p.join(modio))));
    out
}

/// The loader's per-user mods folder in `Documents/<game>`; none for a game
/// without a loader.
// TODO(alex): Windows can redirect Documents (OneDrive); the loader asks the
// shell for the known folder, this assumes `%USERPROFILE%\Documents`.
pub fn user_root(module: &dyn Module, install: &Path) -> Option<PathBuf> {
    let packaging = module.packaging()?;
    let profile = module.profile();
    let documents = if cfg!(windows) {
        std::env::var_os("USERPROFILE").map(|p| Path::new(&p).join("Documents"))
    } else {
        proton_users(profile, install).map(|u| u.join("steamuser/Documents"))
    };
    documents.map(|d| d.join(profile.documents_folder).join(packaging.mods_folder))
}

/// Where the editor installs mods: the user folder, made on install if
/// missing. The install's mods folder only when there is no user folder to
/// name: no `USERPROFILE`, or a Linux install outside `steamapps/common`.
/// None for a game without a loader.
pub fn install_root(module: &dyn Module, install: &Path) -> Option<PathBuf> {
    let packaging = module.packaging()?;
    user_root(module, install).or_else(|| Some(install.join(packaging.mods_folder)))
}

/// `drive_c/users` in the game's Proton prefix, for an install under
/// `steamapps/common`.
fn proton_users(profile: &crate::game::Profile, install: &Path) -> Option<PathBuf> {
    let steamapps = install
        .parent()
        .filter(|p| p.file_name().is_some_and(|n| n == "common"))
        .and_then(Path::parent)?;
    Some(
        steamapps
            .join("compatdata")
            .join(profile.steam_app_id)
            .join("pfx/drive_c/users"),
    )
}

/// Every mod in `roots`, and mods nested one level inside them, first one
/// per id. Unreadable manifests are skipped with a note on stderr.
pub fn installed(module: &dyn Module, roots: &[(Source, PathBuf)]) -> Vec<Installed> {
    let mut out: Vec<Installed> = Vec::new();
    for (source, root) in roots {
        let Ok(entries) = std::fs::read_dir(root) else {
            continue;
        };
        let mut dirs: Vec<PathBuf> = entries
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.is_dir())
            .collect();
        dirs.sort();
        for dir in dirs {
            let name = dir.file_name().map(|n| n.to_string_lossy().into_owned());
            let name = name.unwrap_or_default();
            if name.starts_with('.') {
                continue;
            }
            let enabled = !name.starts_with('-');
            let mut found = vec![dir.clone()];
            // Nested mods sit in subfolders with their own manifest.
            if let Ok(inner) = std::fs::read_dir(&dir) {
                found.extend(
                    inner
                        .filter_map(|e| e.ok().map(|e| e.path()))
                        .filter(|p| p.join("modinfo.json").is_file()),
                );
            }
            for dir in found {
                let Ok(bytes) = std::fs::read(dir.join("modinfo.json")) else {
                    continue;
                };
                let info = match ModInfo::parse(&bytes, module) {
                    Ok(mut info) => {
                        // 1800 falls back to the folder name.
                        if info.id.is_empty() {
                            info.id = dir
                                .file_name()
                                .map(|n| n.to_string_lossy().trim_start_matches('-').to_owned())
                                .unwrap_or_default();
                        }
                        info
                    }
                    Err(e) => {
                        eprintln!("Skipping mod {}: {e:#}", dir.display());
                        continue;
                    }
                };
                if out.iter().any(|m| m.info.id == info.id) {
                    continue;
                }
                out.push(Installed {
                    info,
                    dir,
                    source: *source,
                    enabled,
                });
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_from_names_are_folder_safe() {
        assert_eq!(id_from_name("  My Islands!! "), "my-islands");
        assert_eq!(id_from_name("Ölberg Isles"), "lberg-isles");
        assert!(valid_id("harbour-pack_2.1"));
        for bad in ["", ".git", "-off", "a/b", "a b"] {
            assert!(!valid_id(bad), "{bad:?}");
        }
    }
}
