// Local launcher metadata and install identity. Discovery reads registry
// keys and small metadata files; it never starts a launcher or indexes RDAs.
use std::path::{Component, Path, PathBuf};

use crate::game::Game;

pub(crate) fn has_archives(dir: &Path) -> bool {
    std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .any(|entry| {
            entry.file_type().is_ok_and(|t| t.is_file())
                && entry
                    .path()
                    .extension()
                    .is_some_and(|ext| ext.eq_ignore_ascii_case("rda"))
                && crate::rda::has_header(&entry.path())
        })
}

pub(crate) fn installations(game: Game, roots: &[PathBuf], ubisoft: &[PathBuf]) -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    let mut libraries = Vec::new();
    for root in roots {
        libraries.push(root.clone());
        if let Ok(text) = std::fs::read_to_string(root.join("steamapps/libraryfolders.vdf")) {
            libraries.extend(library_paths(&text));
        }
    }
    for library in unique(libraries) {
        let apps = library.join("steamapps");
        let app = game.profile().steam_app_id;
        let manifest = apps.join(format!("appmanifest_{app}.acf"));
        if let Ok(text) = std::fs::read_to_string(manifest)
            && let Some(pairs) = pairs(&text)
            && value(&pairs, "appid") == Some(app)
            && let Some(name) = value(&pairs, "installdir").filter(|s| install_name(s))
        {
            candidates.push(apps.join("common").join(name));
        }
        // Retain support for copied installs or stale/missing manifests.
        candidates.push(apps.join("common").join(game.profile().steam_folder));
    }
    candidates.extend_from_slice(ubisoft);
    unique(
        candidates
            .into_iter()
            .filter(|p| Game::detect(p) == Some(game)),
    )
}

fn install_name(name: &str) -> bool {
    !name.contains(['/', '\\'])
        && !name.is_empty()
        && matches!(
            Path::new(name).components().next(),
            Some(Component::Normal(_))
        )
}

fn path_key(path: &Path) -> PathBuf {
    let path = path.canonicalize().unwrap_or_else(|_| path.to_owned());
    #[cfg(windows)]
    let path = PathBuf::from(path.to_string_lossy().to_lowercase());
    path
}

fn unique(paths: impl IntoIterator<Item = PathBuf>) -> Vec<PathBuf> {
    let mut seen = std::collections::HashSet::new();
    paths
        .into_iter()
        .filter(|p| seen.insert(path_key(p)))
        .collect()
}

/// Steam client roots, also used by the picker to find cached artwork.
pub fn steam_roots() -> Vec<PathBuf> {
    #[cfg(windows)]
    {
        let mut roots = windows::steam_roots();
        for var in ["ProgramFiles(x86)", "ProgramFiles"] {
            if let Some(dir) = std::env::var_os(var) {
                roots.push(PathBuf::from(dir).join("Steam"));
            }
        }
        unique(roots)
    }
    #[cfg(not(windows))]
    {
        let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
            return Vec::new();
        };
        unique(
            [
                ".local/share/Steam",
                ".steam/steam",
                ".steam/root",
                ".var/app/com.valvesoftware.Steam/.local/share/Steam",
            ]
            .map(|p| home.join(p)),
        )
    }
}

pub(crate) fn ubisoft_installs() -> Vec<PathBuf> {
    #[cfg(windows)]
    {
        windows::ubisoft_installs()
    }
    #[cfg(not(windows))]
    {
        Vec::new()
    }
}

#[cfg(windows)]
mod windows {
    use super::*;
    use winreg::{RegKey, enums::*};

    pub(super) fn steam_roots() -> Vec<PathBuf> {
        let mut roots = Vec::new();
        for (hive, name) in [
            (HKEY_CURRENT_USER, "SteamPath"),
            (HKEY_LOCAL_MACHINE, "InstallPath"),
        ] {
            for view in [KEY_WOW64_32KEY, KEY_WOW64_64KEY] {
                if let Ok(key) = RegKey::predef(hive)
                    .open_subkey_with_flags(r"Software\Valve\Steam", KEY_READ | view)
                    && let Ok(path) = key.get_value::<String, _>(name)
                    && !path.is_empty()
                {
                    roots.push(PathBuf::from(path));
                }
            }
        }
        roots
    }

    pub(super) fn ubisoft_installs() -> Vec<PathBuf> {
        let root = RegKey::predef(HKEY_LOCAL_MACHINE);
        let mut dirs = Vec::new();
        // Some installations register only in the 32-bit view, even when
        // the editor itself is 64-bit. Read both, then deduplicate paths.
        for view in [KEY_WOW64_32KEY, KEY_WOW64_64KEY] {
            let Ok(installs) =
                root.open_subkey_with_flags(r"SOFTWARE\Ubisoft\Launcher\Installs", KEY_READ | view)
            else {
                continue;
            };
            let mut names: Vec<_> = installs.enum_keys().filter_map(Result::ok).collect();
            names.sort();
            for name in names {
                if let Ok(key) = installs.open_subkey_with_flags(name, KEY_READ | view)
                    && let Ok(dir) = key.get_value::<String, _>("InstallDir")
                    && !dir.is_empty()
                {
                    dirs.push(PathBuf::from(dir));
                }
            }
        }
        dirs
    }
}

// Valve KeyValues uses quoted strings, braces, and // comments. Retaining
// braces prevents treating an object name as a string-valued property.
#[derive(PartialEq)]
enum Token {
    Text(String),
    Brace,
}

fn pairs(text: &str) -> Option<Vec<(String, String)>> {
    let mut tokens = Vec::new();
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '/' if chars.peek() == Some(&'/') => {
                for c in chars.by_ref() {
                    if c == '\n' {
                        break;
                    }
                }
            }
            '{' | '}' => tokens.push(Token::Brace),
            '"' => {
                let mut value = String::new();
                loop {
                    match chars.next()? {
                        '"' => break,
                        '\\' => match chars.next()? {
                            '\\' => value.push('\\'),
                            '"' => value.push('"'),
                            other => {
                                value.push('\\');
                                value.push(other);
                            }
                        },
                        c => value.push(c),
                    }
                }
                tokens.push(Token::Text(value));
            }
            c if c.is_whitespace() || c == '\u{feff}' => {}
            _ => return None,
        }
    }
    let mut pairs = Vec::new();
    let mut pos = 0;
    while pos < tokens.len() {
        if let (Token::Text(key), Some(Token::Text(value))) = (&tokens[pos], tokens.get(pos + 1)) {
            pairs.push((key.clone(), value.clone()));
            pos += 2;
        } else {
            pos += 1;
        }
    }
    Some(pairs)
}

fn value<'a>(pairs: &'a [(String, String)], key: &str) -> Option<&'a str> {
    pairs
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(key))
        .map(|(_, v)| v.as_str())
}

pub(super) fn library_paths(text: &str) -> Vec<PathBuf> {
    let Some(pairs) = pairs(text) else {
        return Vec::new();
    };
    let modern: Vec<_> = pairs
        .iter()
        .filter(|(k, _)| k.eq_ignore_ascii_case("path"))
        .map(|(_, p)| PathBuf::from(p))
        .collect();
    if !modern.is_empty() {
        return modern;
    }
    // Older Steam clients stored "1" "D:\\SteamLibrary" directly.
    pairs
        .iter()
        .filter(|(k, v)| k.parse::<u32>().is_ok() && v.contains(['/', '\\']))
        .map(|(_, v)| PathBuf::from(v))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rda::{PackOptions, Packer};

    fn game(dir: &Path, game: Game) {
        std::fs::create_dir_all(dir.join("Bin/Win64")).unwrap();
        std::fs::create_dir_all(dir.join("maindata")).unwrap();
        let exe = match game {
            Game::Anno117 => "Anno117.exe",
            Game::Anno1800 => "Anno1800.exe",
        };
        std::fs::write(dir.join("Bin/Win64").join(exe), []).unwrap();
        let mut archive = Packer::new(PackOptions::default());
        archive.add("data/fixture.txt", b"fixture").unwrap();
        archive.write(&dir.join("maindata/data0.rda")).unwrap();
    }

    #[test]
    fn manifests_resolve_custom_install_names_and_external_libraries() {
        let root = tempfile::tempdir().unwrap();
        let library = root.path().join("Other drive ü/SteamLibrary");
        std::fs::create_dir_all(root.path().join("steamapps")).unwrap();
        std::fs::create_dir_all(library.join("steamapps/common")).unwrap();
        let install = library.join("steamapps/common/My custom Anno folder");
        game(&install, Game::Anno1800);
        let lib = serde_json::to_string(&library).unwrap();
        std::fs::write(
            root.path().join("steamapps/libraryfolders.vdf"),
            format!(r#""libraryfolders" {{ "1" {{ "path" {lib} }} }}"#),
        )
        .unwrap();
        std::fs::write(
            library.join("steamapps/appmanifest_916440.acf"),
            r#""AppState" { "appid" "916440" "installdir" "My custom Anno folder" }"#,
        )
        .unwrap();
        assert_eq!(
            installations(Game::Anno1800, &[root.path().to_owned()], &[]),
            [install]
        );
    }

    #[test]
    fn duplicate_launcher_paths_collapse_and_wrong_games_are_filtered() {
        let dir = tempfile::tempdir().unwrap();
        let steam = dir.path().join("steamapps/common/Anno 1800");
        let ubi = dir.path().join("Ubisoft Games/Anno 1800");
        let other = dir.path().join("Ubisoft Games/Anno 117");
        game(&steam, Game::Anno1800);
        game(&ubi, Game::Anno1800);
        game(&other, Game::Anno117);
        let dirs = installations(
            Game::Anno1800,
            &[dir.path().to_owned()],
            &[steam.clone(), ubi.clone(), other],
        );
        assert_eq!(dirs, [steam, ubi]);
    }

    #[test]
    fn invalid_or_traversing_manifest_names_are_ignored() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("steamapps")).unwrap();
        for name in ["..", "../elsewhere", "D:\\elsewhere", "/elsewhere", ""] {
            let name = serde_json::to_string(name).unwrap();
            std::fs::write(
                dir.path().join("steamapps/appmanifest_916440.acf"),
                format!(r#""AppState" {{ "appid" "916440" "installdir" {name} }}"#),
            )
            .unwrap();
            assert!(installations(Game::Anno1800, &[dir.path().to_owned()], &[]).is_empty());
        }
    }

    #[test]
    fn vdf_escapes_legacy_paths_and_truncation() {
        assert_eq!(
            library_paths(r#""libraryfolders" { "1" "D:\\Steam Games" }"#),
            [PathBuf::from(r"D:\Steam Games")]
        );
        assert_eq!(
            library_paths(r#""libraryfolders" { "1" { "path" "D:\Steam Games" } }"#),
            [PathBuf::from(r"D:\Steam Games")]
        );
        assert!(library_paths("\"path\" \"unfinished").is_empty());
        assert_eq!(
            value(
                &pairs(r#""installdir" "Anno \"custom\"" "apps" { "916440" "1234" }"#).unwrap(),
                "installdir"
            ),
            Some("Anno \"custom\"")
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_installations_are_deduplicated() {
        let root = tempfile::tempdir().unwrap();
        let install = root.path().join("real");
        let link = root.path().join("alias");
        game(&install, Game::Anno117);
        std::os::unix::fs::symlink(&install, &link).unwrap();
        assert_eq!(
            installations(Game::Anno117, &[], &[install.clone(), link]),
            [install]
        );
    }
}
