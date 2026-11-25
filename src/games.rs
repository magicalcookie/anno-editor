// The games the editor knows, each from its own crate under games/: the one
// place that matches a `Game` to it. Adding a game is a variant in
// anno_core::game, a crate, and an arm here.
use anno_core::discovery;
use anno_core::game::{Game, Module, Profile};
use anno_core::text::GameText;
use anno_formats::{assets::Assets, vfs::Install};
use std::path::{Path, PathBuf};

/// `game`'s crate: the one match from a game to its code, so each game is
/// registered here at compile time.
pub fn module(game: Game) -> &'static dyn Module {
    match game {
        Game::Anno117 => &anno117::MODULE,
        Game::Anno1800 => &anno1800::MODULE,
        Game::Anno2070 => &anno2070::MODULE,
    }
}

pub fn profile(game: Game) -> &'static Profile {
    module(game).profile()
}

/// The editor's wording that differs for `game`.
pub fn text(game: Game) -> &'static GameText {
    module(game).text()
}

/// `module`'s asset database as `install` serves it, read by the game's
/// schema.
pub fn assets(module: &dyn Module, install: &Install) -> anyhow::Result<Assets> {
    let schema = (module.database())
        .ok_or_else(|| anyhow::anyhow!("{} has no asset database", module.profile().title))?;
    Assets::load(install, schema)
}

/// The game installed in `install`'s folder: for tests, which open the
/// installs their environment variables name.
#[cfg(test)]
pub fn of(install: &Install) -> Game {
    Game::detect(install.dir()).expect("a game install")
}

/// `game`'s island rules, for tests that name a game with islands.
#[cfg(test)]
pub fn rules(game: Game) -> &'static anno_core::game::IslandRules {
    module(game).islands().expect("a game with islands").rules()
}

/// `game`'s a7m rules, for tests that name a game whose islands are a7m.
#[cfg(test)]
pub fn a7m_rules(game: Game) -> &'static anno_core::game::A7mRules {
    let islands = module(game).islands().expect("a game with islands");
    islands.codec().a7m().expect("a7m islands")
}

/// What the editor asks of a game by its id; import it to call these on
/// `Game` as if they were its own.
pub trait GameExt: Sized {
    fn profile(self) -> &'static Profile;
    /// The game installed in `dir`, by its executable and readable archives.
    fn detect(dir: &Path) -> Option<Self>;
    /// The install the game's environment variable names, if it is one.
    fn environment_dir(self) -> Option<PathBuf>;
    /// Every distinct install identified by local launcher metadata.
    fn installations(self) -> Vec<PathBuf>;
    /// Environment override, else the first validated launcher installation.
    fn locate(self) -> Option<PathBuf>;
}

impl GameExt for Game {
    fn profile(self) -> &'static Profile {
        profile(self)
    }

    fn detect(dir: &Path) -> Option<Self> {
        Game::ALL
            .into_iter()
            .find(|game| discovery::is_install(dir, game.profile().exe))
    }

    fn environment_dir(self) -> Option<PathBuf> {
        std::env::var_os(self.profile().env_var)
            .map(PathBuf::from)
            .filter(|dir| Self::detect(dir) == Some(self))
    }

    fn installations(self) -> Vec<PathBuf> {
        discovery::installations(
            self.profile(),
            &discovery::steam_roots(),
            &discovery::ubisoft_installs(),
        )
    }

    fn locate(self) -> Option<PathBuf> {
        self.environment_dir()
            .or_else(|| self.installations().into_iter().next())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anno_core::modinfo::*;
    use anno_formats::rda::{PackOptions, Packer};
    use serde_json::Value;

    #[test]
    fn every_game_answers_its_capabilities() {
        for game in Game::ALL {
            let m = module(game);
            let edits = game != Game::Anno2070;
            assert_eq!(m.game(), game);
            // 2070 edits islands, plans test maps, lights them and draws its sea, with no
            // screens or packaging yet.
            assert!(m.islands().and_then(|i| i.maps()).is_some());
            assert!(m.scene().is_some());
            assert_eq!(m.ui().is_some(), edits);
            assert_eq!(
                m.packaging().map(|p| p.mods_folder),
                edits.then_some("mods")
            );
        }
    }

    // The values the landscape rules store, as fitted on shipped islands. A
    // misspelt name would silently derive `None` cells.
    #[test]
    fn derived_environments_store_the_shipped_values() {
        let values = |game: Game| {
            let p = rules(game);
            let e = &p.environments;
            [
                e.coast_rock,
                e.coast_sand,
                e.forest,
                e.marsh,
                e.meadow,
                e.mountain,
                e.mountain_top,
                e.sea,
                e.sea_close,
            ]
            .map(|name| p.environment_value(name))
        };
        assert_eq!(values(Game::Anno117), [1, 2, 4, 7, 8, 9, 10, 20, 21]);
        assert_eq!(values(Game::Anno1800), [3, 2, 5, 4, 4, 6, 6, 1, 1]);
        let p = rules(Game::Anno1800);
        assert_eq!(p.environment_value(p.land_environments[0].1), 19);
    }

    #[test]
    fn size_classes_follow_the_slot_limits() {
        let p = rules(Game::Anno117);
        let classes: Vec<_> = [0, 256, 257, 320, 321, 432, 433, 4096]
            .map(|w| p.size_class(w))
            .into();
        assert_eq!(
            classes,
            [
                "Small", "Small", "Medium", "Medium", "Large", "Large", "XL", "XL"
            ]
        );
        assert_eq!(p.start_classes(), "Large or XL");
        assert_eq!((p.min_start_extent(), p.max_slot()), (321, 432));
        let p = rules(Game::Anno1800);
        let classes: Vec<_> = [192, 193, 272, 273, 900].map(|w| p.size_class(w)).into();
        assert_eq!(classes, ["Small", "Medium", "Medium", "Large", "Large"]);
        assert_eq!(p.start_classes(), "Large");
        assert_eq!((p.min_start_extent(), p.max_slot()), (273, 384));
    }

    #[test]
    fn island_regions_follow_path_markers() {
        let region = |game: Game, path| rules(game).island_region(path);
        assert_eq!(
            region(Game::Anno117, "data/base/provinces/celtic/x.a7m"),
            "Celtic"
        );
        assert_eq!(
            region(Game::Anno117, "data/base/provinces/roman/x.a7m"),
            "Roman"
        );
        assert_eq!(
            region(Game::Anno1800, "data/sessions/islands/colony01/x.a7m"),
            "Colony01"
        );
        assert_eq!(
            region(Game::Anno1800, "data/dlc06/x/africa/x.a7m"),
            "Africa"
        );
        assert_eq!(
            region(Game::Anno1800, "data/dlc03/x/colony_03/x.a7m"),
            "Arctic"
        );
        assert_eq!(
            region(Game::Anno1800, "data/sessions/islands/pool/x.a7m"),
            "Moderate"
        );
    }

    // An assets document in `schema`'s spelling: one `<Values>` chain per
    // leaf, which reads as one asset. `Field::Text` is the display name,
    // written inline or as a `TextId` into the returned texts file,
    // whichever the schema reads.
    fn fixture(
        schema: &anno_formats::assets::Schema,
        assets: &[&[(anno_formats::assets::Field, &str)]],
    ) -> (String, String) {
        use anno_formats::assets::Field;
        let path = |f: Field| {
            (schema.fields.iter())
                .find(|&&(_, g)| g == f)
                .map(|&(p, _)| p)
        };
        let escape = |v: &str| {
            v.replace('&', "&amp;")
                .replace('<', "&lt;")
                .replace('>', "&gt;")
        };
        let chain = |path: &str, value: &str| {
            let names: Vec<&str> = path.split('/').collect();
            let open: String = names.iter().map(|n| format!("<{n}>")).collect();
            let close: String = names.iter().rev().map(|n| format!("</{n}>")).collect();
            format!("{open}{}{close}", escape(value))
        };
        let mut xml = String::from("<AssetList><Groups><Group><Assets>\n");
        let mut texts = String::from("<TextExport><Texts>\n");
        for (i, fields) in assets.iter().enumerate() {
            xml += "<Asset>";
            for &(field, value) in *fields {
                if field == Field::Text && path(Field::Text).is_none() {
                    let id = 7000 + i;
                    let key = schema.text_key;
                    xml += &chain(path(Field::TextId).unwrap(), &id.to_string());
                    texts += &format!(
                        "<Text><{key}>{id}</{key}><Text>{}</Text></Text>\n",
                        escape(value)
                    );
                } else {
                    xml += &chain(path(field).expect("the schema reads the field"), value);
                }
            }
            xml += "</Asset>\n";
        }
        xml += "</Assets></Group></Groups></AssetList>";
        texts += "</Texts></TextExport>";
        (xml, texts)
    }

    fn schemas() -> impl Iterator<Item = (Game, &'static anno_formats::assets::Schema)> {
        Game::ALL
            .into_iter()
            .filter_map(|g| Some((g, module(g).database()?)))
    }

    // Islands name their objects by the asset database.
    #[test]
    fn every_game_with_islands_reads_a_database() {
        for game in Game::ALL {
            let m = module(game);
            assert_eq!(m.database().is_some(), m.islands().is_some(), "{game:?}");
        }
    }

    // 2070's assets have no bases or icons, and their texts aren't XML; its
    // own crate tests its schema.
    #[test]
    fn parses_assets_with_inheritance_and_texts() {
        use anno_formats::assets::Field::*;
        for (game, schema) in schemas().filter(|(g, _)| *g != Game::Anno2070) {
            let (xml, texts) = fixture(
                schema,
                &[
                    &[
                        (Template, "Collectable"),
                        (Cfg, "data/a_01.cfg"),
                        (Cfg, "data/a_02.cfg"),
                        (Guid, "1000212"),
                        (Name, "Flotsam & Jetsam"),
                        (Icon, "data/ui/icon.png"),
                        (Text, "Flotsam <sea>"),
                    ],
                    &[(Base, "1000212"), (Guid, "-11342281"), (Name, "Derived")],
                ],
            );
            // A GUID inside a value list is a reference, not an asset.
            let xml = xml.replacen(
                "</Asset>",
                "<Values><AssetPool><AssetList><Item><Asset>42</Asset></Item></AssetList></AssetPool></Values></Asset>",
                1,
            );
            let db = Assets::parse(xml.as_bytes(), Some(texts.as_bytes()), schema).unwrap();
            assert_eq!(db.len(), 2, "{game:?}");
            let a = db.get(1000212).unwrap();
            assert_eq!(a.name, "Flotsam & Jetsam");
            assert_eq!(a.template, "Collectable");
            assert_eq!(a.cfgs, ["data/a_01.cfg", "data/a_02.cfg"]);
            assert_eq!(a.icon.as_deref(), Some("data/ui/icon.png"));
            assert_eq!(a.text.as_deref(), Some("Flotsam <sea>"), "{game:?}");
            assert!(db.get(42).is_none(), "{game:?}");

            let d = db.get(-11342281i32 as u32).unwrap();
            assert_eq!(d.base, Some(1000212));
            assert_eq!(d.template, "Collectable");
            assert_eq!(d.cfgs, a.cfgs);
            assert_eq!(d.text, a.text, "{game:?}");
            assert_eq!(
                db.search("jetsam", 10)
                    .iter()
                    .map(|a| a.guid)
                    .collect::<Vec<_>>(),
                [1000212]
            );
        }
    }

    // 1800 writes names inline under `Text/LocaText/English`; to 117 that
    // path is just an unknown leaf, and the other way round 117's text id
    // means nothing to 1800.
    #[test]
    fn an_1800_text_path_means_nothing_to_117s_schema() {
        use anno_formats::assets::Field::*;
        let [s117, s1800] = [Game::Anno117, Game::Anno1800].map(|g| module(g).database().unwrap());
        let asset: &[_] = &[(Template, "Collectable"), (Guid, "5"), (Text, "Crate")];
        let (inline, _) = fixture(s1800, &[asset]);
        let text = |xml: &str, texts: &str, schema| {
            let db = Assets::parse(xml.as_bytes(), Some(texts.as_bytes()), schema).unwrap();
            db.get(5).unwrap().text.clone()
        };
        assert_eq!(text(&inline, "", s1800).as_deref(), Some("Crate"));
        assert_eq!(text(&inline, "", s117), None);
        let (by_id, texts) = fixture(s117, &[asset]);
        assert_eq!(text(&by_id, &texts, s117).as_deref(), Some("Crate"));
        assert_eq!(text(&by_id, &texts, s1800), None);
    }

    // A game the registry doesn't know reads its own library by its own
    // spelling: its paths, leaf names and caption key. 117's schema finds
    // nothing in that install, and the asset written in 117's spelling is
    // no asset to the skeleton.
    #[test]
    fn the_database_reads_a_skeleton_schema() {
        use super::fixtures::{SKELETON, SKELETON_FILES, install};
        let dir = tempfile::tempdir().unwrap();
        let install = install(dir.path(), SKELETON_FILES);
        let db = assets(&SKELETON, &install).unwrap();
        assert_eq!(db.len(), 3);
        let rock = db.get(910001).unwrap();
        assert_eq!(
            (rock.name.as_str(), rock.template.as_str()),
            ("skel_rock", "Rock")
        );
        assert_eq!(rock.text.as_deref(), Some("Grey rock"));
        assert_eq!(rock.cfgs, ["content/meshes/rock.skm"]);
        let mossy = db.get(910002).unwrap();
        assert_eq!(
            (mossy.base, mossy.template.as_str()),
            (Some(910001), "Rock")
        );
        assert_eq!(mossy.text.as_deref(), Some("Grey rock"));
        let found: Vec<u32> = db.search("survey", 5).iter().map(|a| a.guid).collect();
        assert_eq!(found, [910003]);
        assert!(db.get(5).is_none());
        assert!(assets(&anno117::MODULE, &install).is_err());
    }

    // Recorded at d51799a with the union parser, before the
    // schemas: asset, map, fertility, slot and text counts, and an FNV-1a
    // digest over every asset's `Debug` text in file order. A schema that
    // drops or misreads a path changes one of them. 117's digest is from
    // Steam build 26783132 (2026-11-20), whose config.rda changed assets
    // but none of the counts.
    #[test]
    fn each_schema_reads_the_recorded_assets() {
        let recorded = [
            (
                Game::Anno117,
                [42413, 38, 57, 44, 7784],
                0x0c15b74294ef3164u64,
            ),
            (
                Game::Anno1800,
                [96102, 67, 121, 57, 57776],
                0x29157244a5c5b120,
            ),
        ];
        for (game, counts, digest) in recorded {
            let Some(dir) = game.environment_dir() else {
                eprintln!("skip: {} unset", game.profile().env_var);
                continue;
            };
            let install = Install::open(dir, game.profile().archive_families).unwrap();
            let t = std::time::Instant::now();
            let db = assets(module(game), &install).unwrap();
            eprintln!("{game:?}: {} assets in {:?}", db.len(), t.elapsed());
            let count =
                |f: fn(&anno_formats::assets::Asset) -> bool| db.iter().filter(|a| f(a)).count();
            let got = [
                db.len(),
                count(|a| a.map.is_some()),
                count(|a| !a.fertilities.is_empty()),
                count(|a| a.slot_type.is_some()),
                count(|a| a.text.is_some()),
            ];
            assert_eq!(
                got, counts,
                "{game:?}: assets, maps, fertility, slots, texts"
            );
            let mut h = 0xcbf29ce484222325u64;
            for a in db.iter() {
                for b in format!("{a:?}").bytes() {
                    h ^= b as u64;
                    h = h.wrapping_mul(0x100000001b3);
                }
            }
            assert_eq!(h, digest, "{game:?} digest {h:016x}");
        }
    }

    #[test]
    fn game_detection_uses_executable_and_keeps_storage_separate() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(Game::detect(dir.path()), None);
        std::fs::create_dir_all(dir.path().join("Bin/Win64")).unwrap();
        std::fs::create_dir(dir.path().join("maindata")).unwrap();
        let mut archive = Packer::new(PackOptions::default());
        archive.add("data/fixture.txt", b"fixture").unwrap();
        std::fs::write(
            dir.path().join("maindata/data0.rda"),
            archive.encode().unwrap(),
        )
        .unwrap();
        for (game, exe) in [
            (Game::Anno1800, "Anno1800.exe"),
            (Game::Anno117, "Anno117.exe"),
        ] {
            let exe = dir.path().join("Bin/Win64").join(exe);
            std::fs::write(&exe, []).unwrap();
            assert_eq!(Game::detect(dir.path()), Some(game));
            std::fs::remove_file(exe).unwrap();
        }
        assert_eq!(Game::Anno117.profile().config_folder, "anno-editor");
        assert_ne!(
            Game::Anno117.profile().config_folder,
            Game::Anno1800.profile().config_folder
        );
    }

    // The landscape rules write the values each game stores.
    #[test]
    fn landscape_environments_are_each_games_own() {
        let e = rules(Game::Anno117).environment_values();
        assert_eq!((e.coast_rock, e.meadow, e.sea, e.sea_close), (1, 8, 20, 21));
        let e = rules(Game::Anno1800).environment_values();
        assert_eq!((e.coast_rock, e.marsh, e.sea), (3, 4, 1));
    }

    // Needs the game: the environment variable names the install found.
    #[test]
    fn the_environment_variable_locates_the_install() {
        let Some(dir) = anno_formats::vfs::install_dir("ANNO117_DIR").map(PathBuf::from) else {
            return;
        };
        assert_eq!(Game::Anno117.locate().as_deref(), Some(dir.as_path()));
    }

    #[test]
    fn unknown_fields_and_languages_survive_an_edit() {
        let src = br#"{
            "Anno": 8, "ModID": "harbour-pack", "Version": "1.2.0",
            "ModName": { "English": "Harbour Pack", "German": "Hafenpaket" },
            "Category": "Islands", "Difficulty": "easier",
            "GameSetup": { "RequiresNewGame": true },
            "Dependencies": { "Require": ["shared-lib"], "LoadAfter": ["x"] }
        }"#;
        let mut info = ModInfo::parse(src, &anno117::MODULE).unwrap();
        assert_eq!(
            (info.id.as_str(), info.name.as_str(), info.category.as_str()),
            ("harbour-pack", "Harbour Pack", "Islands")
        );
        assert_eq!(info.requires, ["shared-lib"]);
        info.name = "Harbours".into();
        info.requires.push("trees".into());
        let back: Value = serde_json::from_str(&info.encode(&anno117::MODULE)).unwrap();
        assert_eq!(back["ModName"]["English"], "Harbours");
        assert_eq!(back["ModName"]["German"], "Hafenpaket");
        assert_eq!(back["GameSetup"]["RequiresNewGame"], true);
        assert_eq!(back["Dependencies"]["LoadAfter"][0], "x");
        assert_eq!(back["Dependencies"]["Require"][1], "trees");
        assert_eq!(back["Difficulty"], "easier");
    }

    #[test]
    fn each_game_gets_its_own_manifest_shape() {
        let mut info = ModInfo::new("isles", "Isles");
        let v: Value = serde_json::from_str(&info.encode(&anno117::MODULE)).unwrap();
        assert_eq!(v["Anno"], 8);
        // The 117 loader logs an error and refuses the mod without it.
        assert_eq!(v["Difficulty"], "unchanged");
        assert!(v.get("Dependencies").is_none());
        info.requires = vec!["dep".into()];
        let v: Value = serde_json::from_str(&info.encode(&anno1800::MODULE)).unwrap();
        assert_eq!(v["Anno"], 7);
        assert_eq!(v["ModDependencies"][0], "dep");
        assert!(v.get("Difficulty").is_none());
        assert_eq!(
            ModInfo::parse(info.encode(&anno1800::MODULE).as_bytes(), &anno1800::MODULE)
                .unwrap()
                .requires,
            ["dep"]
        );
    }

    #[test]
    fn installed_mods_are_found_nested_and_deduplicated() {
        let root = tempfile::tempdir().unwrap();
        let put = |rel: &str, json: &str| {
            let dir = root.path().join(rel);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("modinfo.json"), json).unwrap();
        };
        put("a", r#"{"ModID":"a","ModName":"A"}"#);
        put("a/sub", r#"{"ModID":"a-sub"}"#);
        put("-b", r#"{"ModID":"b"}"#);
        put("c", r#"{"ModID":"a"}"#);
        put("broken", "{");
        std::fs::create_dir_all(root.path().join(".git")).unwrap();
        let found = installed(&anno117::MODULE, &[(Source::Game, root.path().to_owned())]);
        let ids: Vec<(&str, bool)> = found
            .iter()
            .map(|m| (m.info.id.as_str(), m.enabled))
            .collect();
        assert_eq!(ids, [("b", false), ("a", true), ("a-sub", true)]);
    }

    #[test]
    fn proton_roots_sit_in_the_game_prefix() {
        let install = Path::new("/lib/steamapps/common/Anno 1800");
        let roots = roots(&anno1800::MODULE, install);
        let has = |p: &str| roots.iter().any(|(_, r)| r == Path::new(p));
        assert!(has("/lib/steamapps/common/Anno 1800/mods"));
        if !cfg!(windows) {
            assert!(has(
                "/lib/steamapps/compatdata/916440/pfx/drive_c/users/steamuser/Documents/Anno 1800/mods"
            ));
            assert!(has(
                "/lib/steamapps/compatdata/916440/pfx/drive_c/users/Public/mod.io/4169/mods"
            ));
        }
    }

    #[cfg(not(windows))]
    #[test]
    fn install_roots_use_the_games_mods_folder() {
        use super::fixtures::PartialGame;
        let game = &PartialGame::Packaged;
        let install = Path::new("/lib/steamapps/common/Game");
        let found: Vec<PathBuf> = roots(game, install).into_iter().map(|(_, r)| r).collect();
        let documents = "/lib/steamapps/compatdata/3274580/pfx/drive_c/users/steamuser/Documents/Anno 117 - Pax Romana/addons";
        assert_eq!(
            found[..3],
            [
                install.join("addons"),
                install.join("addons/.ubi"),
                PathBuf::from(documents)
            ]
        );
        assert_eq!(install_root(game, install).unwrap(), Path::new(documents));
        let loose = Path::new("/games/Game");
        assert_eq!(install_root(game, loose).unwrap(), loose.join("addons"));
        // A game without a loader has no folders to search or install to.
        let bare = &PartialGame::Islands;
        assert!(roots(bare, install).is_empty());
        assert_eq!(install_root(bare, install), None);
    }

    #[cfg(not(windows))]
    #[test]
    fn installs_go_to_the_user_folder_even_before_it_exists() {
        let lib = Path::new("/lib/steamapps/common/Anno 117 - Pax Romana");
        assert_eq!(
            install_root(&anno117::MODULE, lib).unwrap(),
            Path::new(
                "/lib/steamapps/compatdata/3274580/pfx/drive_c/users/steamuser/Documents/Anno 117 - Pax Romana/mods"
            )
        );
        // Outside a Steam library there is no prefix to name.
        let loose = Path::new("/games/Anno 117 - Pax Romana");
        assert_eq!(
            install_root(&anno117::MODULE, loose).unwrap(),
            loose.join("mods")
        );
    }
}

#[cfg(test)]
pub(crate) mod fixtures {
    use super::*;
    use anno_core::files::{Group, Kind, View};
    use anno_core::game::{Islands, Packaging, SceneParts, UiScreens};
    use anno_core::harbour::Piece;
    use anno_core::map::{Fill, MapRules, Maps, Registers, Setup};
    use anno_core::project::{GuidRange, MapEntry, MapIsland, MapPlan, PlanSlot};

    /// A game with islands or with screens, never both, and two project
    /// roots; `Bare` has neither and no projects. `Packaged` is `Islands`
    /// with a loader of its own, none of whose values are 117's.
    pub enum PartialGame {
        Islands,
        Screens,
        Bare,
        Packaged,
    }

    pub static PACKAGING: Packaging = Packaging {
        mods_folder: "addons",
        difficulties: &["calm", "wild"],
        guid_range: GuidRange {
            first: 7_000_000,
            size: 50,
            islands: 40,
            count: 10,
            registry: "https://example.invalid/guids",
        },
        loader_log: "addons/loader.txt",
    };
    impl Module for PartialGame {
        fn game(&self) -> Game {
            Game::Anno117
        }
        fn profile(&self) -> &'static Profile {
            anno117::MODULE.profile()
        }
        fn card_art(&self) -> &'static [u8] {
            &[]
        }
        fn text(&self) -> &'static GameText {
            anno117::MODULE.text()
        }
        fn accent(&self) -> [u32; 2] {
            [0; 2]
        }
        fn project_roots(&self) -> &'static [&'static str] {
            match self {
                Self::Islands | Self::Screens | Self::Packaged => &["data", "addondata"],
                Self::Bare => &[],
            }
        }
        fn islands(&self) -> Option<&dyn Islands> {
            matches!(self, Self::Islands | Self::Packaged).then_some(self)
        }
        fn packaging(&self) -> Option<&'static Packaging> {
            matches!(self, Self::Packaged).then_some(&PACKAGING)
        }
        fn ui(&self) -> Option<&dyn UiScreens> {
            matches!(self, Self::Screens).then_some(self)
        }
        // Both variants list both kinds: the capability decides whether a
        // file opens in its editor or shows in Assets.
        fn files(&self) -> &'static [Kind] {
            static KINDS: &[Kind] = &[
                Kind::ext(
                    "fixture",
                    "Fixture island",
                    Group::Islands,
                    &["fixture"],
                    Some(View::Island),
                ),
                Kind::ext(
                    "screen",
                    "Fixture screen",
                    Group::Ui,
                    &["screen"],
                    Some(View::Screen),
                ),
            ];
            KINDS
        }
    }
    impl Islands for PartialGame {
        fn rules(&self) -> &'static anno_core::game::IslandRules {
            super::rules(Game::Anno117)
        }
        fn maps(&self) -> Option<&dyn Maps> {
            Some(&OneWorld)
        }
        fn codec(&self) -> anno_core::game::Codec<'_> {
            anno117::MODULE.islands().unwrap().codec()
        }
    }
    // Borrows 117's folders and data, as it borrows its profile.
    impl UiScreens for PartialGame {
        fn screens(&self) -> &'static anno_core::ui::Screens {
            anno117::MODULE.ui().unwrap().screens()
        }
    }

    /// A fourth Anno-engine game: its own identity, one project root
    /// (`content`), file kinds of its own (a text mesh, `.skm`, and a text
    /// picture, `.skt`), a model reader and writer for its meshes, an asset
    /// database in its own spelling, and islands with rules, a format
    /// (`.ski`), paint (`Dots`) and a ground (`Sand`) of their own. No
    /// packaging and no screens; its scene is the kit's but for the ground. `game()` has to name one of the closed enum's
    /// games and names 117, so a tool that looks the module up again by
    /// `Game` runs 117's rules and fails the checks, each of which looks
    /// for a value only the skeleton has.
    pub struct Skeleton;
    pub static SKELETON: Skeleton = Skeleton;

    static SKELETON_PROFILE: Profile = Profile {
        title: "Skeleton",
        short_name: "skel",
        env_var: "SKELETON_DIR",
        config_folder: "anno-editor/skeleton",
        exe: "Bin/Win64/Skeleton.exe",
        modinfo_anno: 99,
        ..Profile::BARE
    };

    /// The skeleton's islands: a reef region beside its default shoal, two
    /// size classes, its own folders, pool anchor and environment types.
    pub static SKELETON_RULES: anno_core::game::IslandRules = anno_core::game::IslandRules {
        prop_sets_path: "skeleton/props.xml",
        island_folder: ["content/isles/", "/made"],
        pool_anchor_guid: 910_100,
        regions: &[("/reef/", "Reef")],
        default_region: "Shoal",
        asset_regions: &[],
        provinces: &[anno_core::game::Province {
            region: "Reef",
            name: "Coral Sea (Reef)",
            prop_sets: "reefgrass",
        }],
        roles: &[
            anno_core::game::Role {
                label: "Lagoon",
                value: "Lagoon",
                regular: true,
                start: false,
            },
            anno_core::game::Role {
                label: "Landing",
                value: "Landing",
                regular: false,
                start: true,
            },
        ],
        size_classes: &[
            anno_core::game::SizeClass {
                name: "Islet",
                slot: 96,
                start: false,
            },
            anno_core::game::SizeClass {
                name: "Atoll",
                slot: 192,
                start: true,
            },
        ],
        environment_types: &[("Nothing", 0), ("Kelp", 0xff20_a040)],
        placeholder_size: true,
        slot_yaw_step: 0.25,
        ..anno_core::game::IslandRules::BARE
    };

    static SKELETON_TEXT: GameText = GameText {
        subtitle: "Test game",
        era: "",
        regions: "",
        add_model: "",
        model_formats: ".skm",
        slot_resource: "",
        slot_roads: "",
        playtest: anno_core::text::Playtest {
            intro: "",
            steps: "",
            in_game: "",
        },
        playtest_old_world: None,
        asset_words: anno_core::game::AssetWords::NONE,
    };

    static SKELETON_KINDS: &[Kind] = &[
        Kind::ext(
            "ski",
            "Skeleton island",
            Group::Islands,
            &["ski"],
            Some(View::Island),
        ),
        // Text, so it previews as text: the 3D viewer draws cfgs.
        Kind {
            deps: Some(skm_deps),
            ..Kind::ext("skm", "Skeleton mesh", Group::Models, &["skm"], None)
        },
        Kind {
            decode: Some(skt_decode),
            ..Kind::ext(
                "skt",
                "Skeleton picture",
                Group::Textures,
                &["skt"],
                Some(View::Picture),
            )
        },
    ];

    /// The skeleton's asset database: ten lines of XML under its own root
    /// element, with its own leaf names and a texts file keyed by `Key`.
    pub static SKELETON_SCHEMA: anno_formats::assets::Schema = {
        use anno_formats::assets::Field;
        anno_formats::assets::Schema {
            assets_path: "skeleton/library.xml",
            more_assets: &[],
            texts_path: "skeleton/captions.xml",
            templates_path: None,
            text_key: "Key",
            languages: &[("english", "skeleton/captions.xml")],
            fields: &[
                ("Kind", Field::Template),
                ("Base", Field::Base),
                ("Data/Id", Field::Guid),
                ("Data/Label", Field::Name),
                ("Data/Mesh", Field::Cfg),
                ("Data/Caption", Field::TextId),
            ],
            default_work_area_slot: "",
            kept: &[],
        }
    };

    impl Module for Skeleton {
        fn game(&self) -> Game {
            Game::Anno117
        }
        fn profile(&self) -> &'static Profile {
            &SKELETON_PROFILE
        }
        fn text(&self) -> &'static GameText {
            &SKELETON_TEXT
        }
        fn card_art(&self) -> &'static [u8] {
            &[]
        }
        fn accent(&self) -> [u32; 2] {
            [0x5a7a5a; 2]
        }
        fn project_roots(&self) -> &'static [&'static str] {
            &["content"]
        }
        fn files(&self) -> &'static [Kind] {
            SKELETON_KINDS
        }
        fn ports(&self) -> anno_core::files::Ports {
            anno_core::files::Ports {
                props_from: &[Game::Anno117],
                prop_ext: ".skm",
                mesh_import: false,
            }
        }
        fn database(&self) -> Option<&'static anno_formats::assets::Schema> {
            Some(&SKELETON_SCHEMA)
        }
        fn islands(&self) -> Option<&dyn Islands> {
            Some(self)
        }
        fn scene(&self) -> Option<&dyn SceneParts> {
            Some(self)
        }
        fn read_model(
            &self,
            install: &Install,
            source: &str,
        ) -> Option<anyhow::Result<anno_core::neutral::Model>> {
            Some((|| {
                let text = String::from_utf8(install.read(source)?)?;
                let stem = source.strip_suffix(".skm").unwrap_or(source);
                let name = source.rsplit('/').next().unwrap_or(source);
                let (shape, materials, tints) = skm_parse(&text)?;
                Ok(anno_core::neutral::Model {
                    pieces: vec![anno_core::neutral::Piece {
                        name: stem.to_owned(),
                        label: name.to_owned(),
                        at: anno_formats::model::Transform::IDENTITY,
                        shape,
                        materials,
                        tints,
                    }],
                    ..Default::default()
                })
            })())
        }
        fn write_model(
            &self,
            model: &anno_core::neutral::Model,
            from: &dyn anno_core::neutral::Source,
            _to: &Install,
        ) -> Option<anyhow::Result<anno_core::neutral::Ported>> {
            Some(skm_write(model, from))
        }
    }

    // No harbours or slot tools to feed; its survey markers are the
    // islands' marker piece.
    impl Islands for Skeleton {
        fn rules(&self) -> &'static anno_core::game::IslandRules {
            &SKELETON_RULES
        }
        fn codec(&self) -> anno_core::game::Codec<'_> {
            anno_core::game::Codec::Own(self)
        }
        fn piece_of(&self, template: &str, _: &str) -> Option<Piece> {
            (template == "Marker").then_some(Piece::Marker)
        }
    }

    // `.ski` text: `ski 1`, `size <w> <d>` in metre corners, then a
    // `h` line of heights and a `p` line of ground ids, both row by row.
    impl anno_core::game::IslandCodec for Skeleton {
        fn read(
            &self,
            install: &Install,
            path: &str,
            _: Option<&Assets>,
        ) -> anyhow::Result<anno_core::island::Island> {
            use anno_core::island::{Doc, Island, Materials, Native, Own, Paint};
            use anyhow::{Context, ensure};
            use gpui_editor_kit::heightfield::{Grid, Heightfield};
            use std::sync::Arc;
            let bytes: Arc<[u8]> = install.read(path)?.into();
            let text = std::str::from_utf8(&bytes)?;
            let mut lines = text.lines();
            ensure!(
                lines.next() == Some("ski 1"),
                "{path}: not a skeleton island"
            );
            let mut field = |key: &str| -> anyhow::Result<Vec<&str>> {
                let line = lines.next().with_context(|| format!("{path}: no {key}"))?;
                let mut words = line.split_whitespace();
                ensure!(words.next() == Some(key), "{path}: {line:?} is not {key}");
                Ok(words.collect())
            };
            let size: Vec<usize> =
                (field("size")?.iter().map(|w| w.parse())).collect::<Result<_, _>>()?;
            ensure!(size.len() == 2, "{path}: size");
            let (width, depth) = (size[0], size[1]);
            let heights: Vec<f32> =
                (field("h")?.iter().map(|w| w.parse())).collect::<Result<_, _>>()?;
            let ids: Vec<u8> = (field("p")?.iter().map(|w| w.parse())).collect::<Result<_, _>>()?;
            ensure!(
                heights.len() == width * depth && ids.len() == width * depth,
                "{path}: {} heights and {} ids for {width}×{depth}",
                heights.len(),
                ids.len()
            );
            let grid = Grid {
                width,
                depth,
                spacing: 1.,
                origin: [0.; 2],
            };
            let field = Arc::new(Heightfield::new(grid, heights));
            let paint = Paint::Own(Own::new(Dots { width, ids }));
            let doc = Doc {
                land_heights: field.clone(),
                field,
                materials: Some(Materials::from_paint(paint, 1.)),
                meshes: Arc::default(),
                props: Default::default(),
                objects: Arc::default(),
                grids: None,
                tint: None,
                session: Default::default(),
                heights_patch: None,
            };
            let mut island = Island::new(doc, None, String::new());
            island.set_native(Native::File(bytes));
            Ok(island)
        }
        fn write(
            &self,
            island: &anno_core::island::Island,
            path: &str,
            _: Option<&Assets>,
        ) -> anyhow::Result<Vec<u8>> {
            use anyhow::Context;
            use std::fmt::Write;
            let doc = &island.doc;
            let m = doc.materials.as_ref().context("no paint")?;
            let anno_core::island::Paint::Own(own) = &m.paint else {
                anyhow::bail!("{path}: a skeleton island holds skeleton paint");
            };
            let dots = own.get::<Dots>().context("skeleton paint")?;
            let g = doc.field.grid;
            let mut out = format!("ski 1\nsize {} {}\nh", g.width, g.depth);
            for h in doc.field.heights.iter() {
                write!(out, " {h}")?;
            }
            out += "\np";
            for id in &dots.ids {
                write!(out, " {id}")?;
            }
            out += "\n";
            Ok(out.into_bytes())
        }
        // One file per island, beside the others of its region.
        fn island_path(&self, region: &str, name: &str) -> Option<String> {
            Some(format!("content/isles/{region}/made/{name}.ski"))
        }
    }

    /// The skeleton's paint: one ground id a metre corner, the whole
    /// corner's. A dab sets the corners it weighs at half or more.
    #[derive(Clone)]
    pub struct Dots {
        width: usize,
        ids: Vec<u8>,
    }

    impl Dots {
        pub fn id(&self, x: usize, z: usize) -> u8 {
            self.ids[z * self.width + x]
        }
    }

    impl anno_core::island::Form for Dots {
        fn size(&self) -> [usize; 2] {
            [self.width, self.ids.len() / self.width]
        }
        fn corners(&self) -> bool {
            true
        }
        fn cover(&self, x: usize, y: usize) -> anno_core::island::Cover {
            let mut out = anno_core::island::Cover::default();
            out.add(self.id(x, y), 1.);
            out
        }
        fn paint_with(
            &mut self,
            per_metre: f32,
            lo: [f32; 2],
            hi: [f32; 2],
            id: u8,
            weight: &(dyn Fn([f32; 2]) -> f32 + Sync),
        ) -> bool {
            let [w, d] = self.size();
            let span = |a: f32, b: f32, n: usize| {
                ((a * per_metre).ceil().max(0.) as usize)
                    ..((b * per_metre).floor() as usize + 1).min(n)
            };
            let mut changed = false;
            for z in span(lo[1], hi[1], d) {
                for x in span(lo[0], hi[0], w) {
                    let at = [x as f32 / per_metre, z as f32 / per_metre];
                    let cell = &mut self.ids[z * w + x];
                    if weight(at) >= 0.5 && *cell != id {
                        *cell = id;
                        changed = true;
                    }
                }
            }
            changed
        }
        fn same_shape(&self, other: &dyn anno_core::island::Form) -> bool {
            (other.as_any().downcast_ref::<Dots>())
                .is_some_and(|o| o.width == self.width && o.ids.len() == self.ids.len())
        }
        fn changed(
            &self,
            other: &dyn anno_core::island::Form,
        ) -> Option<gpui_editor_kit::heightfield::Rect> {
            let other = other.as_any().downcast_ref::<Dots>()?;
            let mut rect: Option<gpui_editor_kit::heightfield::Rect> = None;
            for (i, _) in (self.ids.iter().zip(&other.ids).enumerate()).filter(|(_, (a, b))| a != b)
            {
                let (x, z) = (i % self.width, i / self.width);
                let r = rect.get_or_insert(gpui_editor_kit::heightfield::Rect {
                    x0: x,
                    z0: z,
                    x1: x + 1,
                    z1: z + 1,
                });
                *r = r.union(gpui_editor_kit::heightfield::Rect {
                    x0: x,
                    z0: z,
                    x1: x + 1,
                    z1: z + 1,
                });
            }
            rect
        }
        fn duplicate(&self) -> Box<dyn anno_core::island::Form> {
            Box::new(self.clone())
        }
        fn bytes(&self) -> usize {
            self.ids.len()
        }
        fn as_any(&self) -> &dyn std::any::Any {
            self
        }
    }

    /// The skeleton's ground: flat sand.
    pub struct Sand {
        factory: gpui_editor_kit::gpu::terrain::GroundFactory,
    }

    /// Paint updates the skeleton's grounds were handed, for tests.
    pub static SAND_UPDATES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    /// The ground ids the skeleton's palette offers, with their colours.
    pub const SKELETON_GROUNDS: [(u8, [u8; 3]); 3] =
        [(1, [200, 180, 120]), (2, [60, 140, 60]), (3, [90, 90, 100])];

    impl Sand {
        fn new() -> Self {
            Self {
                factory: std::sync::Arc::new(|init| Box::new(SandGround::new(init.device))),
            }
        }
    }

    impl anno_core::game::IslandGround for Sand {
        fn draw(&self) -> gpui_editor_kit::gpu::GroundDraw {
            gpui_editor_kit::gpu::GroundDraw::Surface(self.factory.clone())
        }
        fn update(&self, paint: &anno_core::island::Paint, _area: [f32; 4]) {
            if let anno_core::island::Paint::Own(own) = paint
                && own.get::<Dots>().is_some()
            {
                SAND_UPDATES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
        }
        fn thumbs(&self, _: &Install) -> Vec<anno_core::game::Thumb> {
            (SKELETON_GROUNDS.iter())
                .map(|&(id, [r, g, b])| anno_core::game::Thumb {
                    id,
                    name: format!("content/ground/ground_{id}.skt"),
                    image: gpui_editor_kit::texture::Texture::from_rgba(1, 1, vec![r, g, b, 255])
                        .unwrap(),
                    colour: true,
                })
                .collect()
        }
    }

    // A flat sand surface over the kit's patches: group 3 holds one unused
    // uniform, as a ground must bind a group of its own.
    struct SandGround {
        layout: wgpu::BindGroupLayout,
        group: wgpu::BindGroup,
    }

    impl SandGround {
        fn new(d: &wgpu::Device) -> Self {
            let layout = d.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("skeleton sand"),
                entries: &[wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                }],
            });
            let uniform = d.create_buffer(&wgpu::BufferDescriptor {
                label: Some("skeleton sand"),
                size: 16,
                usage: wgpu::BufferUsages::UNIFORM,
                mapped_at_creation: false,
            });
            let group = d.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("skeleton sand"),
                layout: &layout,
                entries: &[wgpu::BindGroupEntry {
                    binding: 0,
                    resource: uniform.as_entire_binding(),
                }],
            });
            Self { layout, group }
        }
    }

    impl gpui_editor_kit::gpu::terrain::Ground for SandGround {
        fn wgsl(&self) -> &str {
            r#"
@group(3) @binding(0) var<uniform> sand: vec4<u32>;
fn ground_displacement(uv: vec2<f32>, world: vec3<f32>, n: vec3<f32>, dw: array<vec3<f32>, 2>) -> f32 {
    return 0.0;
}
fn ground(uv: vec2<f32>, world: vec3<f32>, n: vec3<f32>, dw: array<vec3<f32>, 2>) -> GroundSample {
    return GroundSample(vec4<f32>(0.78, 0.7, 0.47, 1.0), n, 0.0, 0.0, 0.0);
}
"#
        }
        fn layout(&self) -> &wgpu::BindGroupLayout {
            &self.layout
        }
        fn bind_group(&self) -> Option<&wgpu::BindGroup> {
            Some(&self.group)
        }
    }

    // No light, sky or sea of its own: the kit's. Its ground is its own.
    impl SceneParts for Skeleton {
        fn ambients(&self, _: &Install, _: &[&str]) -> Vec<(String, anno_core::ambient::Ambient)> {
            Vec::new()
        }
        fn ground(
            &self,
            _: &Install,
            paint: &anno_core::island::Paint,
        ) -> Option<anyhow::Result<Box<dyn anno_core::game::IslandGround>>> {
            let anno_core::island::Paint::Own(own) = paint else {
                return None;
            };
            own.get::<Dots>()?;
            Some(Ok(Box::new(Sand::new())))
        }
    }

    /// The archive `files` packed as `maindata/data0.rda` under `dir`: an
    /// install as `Install::open` reads one.
    pub fn install(dir: &Path, files: &[(&str, &[u8])]) -> Install {
        let archives = dir.join("maindata");
        std::fs::create_dir_all(&archives).unwrap();
        let mut pack = anno_formats::rda::Packer::new(anno_formats::rda::PackOptions::default());
        for (path, bytes) in files {
            pack.add(path, bytes).unwrap();
        }
        std::fs::write(archives.join("data0.rda"), pack.encode().unwrap()).unwrap();
        Install::open(dir, &[]).unwrap()
    }

    /// A skeleton install's library and captions, a mesh and a picture,
    /// beside a 117 model and screen the skeleton has no kind for.
    pub const SKELETON_FILES: &[(&str, &[u8])] = &[
        (
            "skeleton/library.xml",
            b"<Library>
  <Asset><Kind>Rock</Kind><Data><Id>910001</Id><Label>skel_rock</Label>
    <Mesh>content/meshes/rock.skm</Mesh><Caption>41</Caption></Data></Asset>
  <Asset><Base>910001</Base><Data><Id>910002</Id><Label>skel_rock_mossy</Label></Data></Asset>
  <Asset><Kind>Marker</Kind><Data><Id>910003</Id><Label>skel_marker</Label>
    <Caption>42</Caption></Data></Asset>
  <!-- 117's spelling: no field the skeleton reads, so no asset. -->
  <Asset><Template>Collectable</Template><Values><Standard><GUID>5</GUID>
    <Name>Crate</Name></Standard></Values></Asset>
</Library>",
        ),
        (
            "skeleton/captions.xml",
            b"<TextExport><Texts>
<Text><Key>41</Key><Text>Grey rock</Text></Text>
<Text><Key>42</Key><Text>Survey marker</Text></Text>
</Texts></TextExport>",
        ),
        (
            "content/meshes/rock.skm",
            b"skm 1
mesh content/meshes/rock.rdm
at 0 0 0 0 0 0 1 1 1 1
material content/textures/rock.psd - - 1 1 1 opaque 0 0 0
",
        ),
        ("content/textures/rock.skt", b"skt 2 1\nff0000ff 0000ff80\n"),
        ("data/base/graphics/rock.cfg", b"<Config></Config>"),
        ("data/ui/studio/generated/00214ce2-8775", b"PHX"),
    ];

    /// What an `.skm` names: its mesh with the distance levels the games
    /// find by name, and each texture as its shipped mips.
    fn skm_deps(bytes: &[u8]) -> Vec<String> {
        let text = String::from_utf8_lossy(bytes);
        let mut out: Vec<String> = Vec::new();
        for line in text.lines() {
            let refs: Vec<&str> = match line.split_whitespace().collect::<Vec<_>>()[..] {
                ["mesh", rdm] => vec![rdm],
                ["material", diffuse, normal, metallic, ..] => vec![diffuse, normal, metallic],
                _ => continue,
            };
            for r in refs.into_iter().filter(|r| *r != "-") {
                for f in anno_core::files::siblings(&anno_core::files::shipped_path(r)) {
                    if !out.contains(&f) {
                        out.push(f);
                    }
                }
            }
        }
        out
    }

    /// An `.skt`: `skt <width> <height>`, then the texels as RGBA hex words,
    /// row by row.
    fn skt_decode(bytes: &[u8]) -> anyhow::Result<(gpui_editor_kit::texture::Texture, String)> {
        use anyhow::Context;
        let text = std::str::from_utf8(bytes)?;
        let mut words = text.split_whitespace();
        anyhow::ensure!(words.next() == Some("skt"), "not a skeleton picture");
        let mut size = || -> anyhow::Result<u32> { Ok(words.next().context("no size")?.parse()?) };
        let (w, h) = (size()?, size()?);
        let rgba: Vec<u8> = (words.map(|w| u32::from_str_radix(w, 16)))
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .flat_map(u32::to_be_bytes)
            .collect();
        anyhow::ensure!(
            rgba.len() == (w * h * 4) as usize,
            "{w}×{h} wants {} texels",
            w * h
        );
        let texture = gpui_editor_kit::texture::Texture::from_rgba(w, h, rgba)?;
        Ok((texture, "skeleton text picture".to_owned()))
    }

    fn skm_parse(
        text: &str,
    ) -> anyhow::Result<(
        anno_core::neutral::Shape,
        Vec<anno_core::neutral::Material>,
        Vec<[f32; 3]>,
    )> {
        use anno_formats::model::{Alpha, Transform};
        use anyhow::{Context, bail, ensure};
        let mut lines = text
            .lines()
            .filter(|l| !l.is_empty() && !l.starts_with('#'));
        ensure!(lines.next() == Some("skm 1"), "not a skeleton mesh");
        let (mut rdm, mut transform) = (None, Transform::IDENTITY);
        let (mut materials, mut tints) = (Vec::new(), Vec::new());
        let path = |p: &str| (p != "-").then(|| p.to_owned());
        for line in lines {
            let words: Vec<&str> = line.split_whitespace().collect();
            let floats = |words: &[&str]| -> anyhow::Result<Vec<f32>> {
                Ok(words.iter().map(|w| w.parse()).collect::<Result<_, _>>()?)
            };
            match words[..] {
                ["mesh", p] => rdm = Some(p.to_owned()),
                ["at", ref rest @ ..] => {
                    let v = floats(rest)?;
                    ensure!(v.len() == 10, "at: {line}");
                    transform = Transform {
                        position: [v[0], v[1], v[2]],
                        rotation: [v[3], v[4], v[5], v[6]],
                        scale: [v[7], v[8], v[9]],
                    };
                }
                ["material", d, n, m, r, g, b, alpha, two, ground, tint] => {
                    let alpha = match alpha.split_once(':') {
                        Some(("cutout", v)) => Alpha::Cutout(v.parse()?),
                        Some(("blend", v)) => Alpha::Blend(v.parse()?),
                        Some(("tested", v)) => Alpha::Tested(v.parse()?),
                        None if alpha == "opaque" => Alpha::Opaque,
                        None if alpha == "decal" => Alpha::Decal,
                        None if alpha == "hidden" => Alpha::Hidden,
                        _ => bail!("alpha {alpha}"),
                    };
                    let c = floats(&[r, g, b])?;
                    materials.push(anno_core::neutral::Material {
                        diffuse: path(d),
                        normal: path(n),
                        metallic: path(m),
                        color: [c[0], c[1], c[2]],
                        double_sided: two == "1",
                        alpha,
                        ground_shaded: ground == "1",
                        terrain_tint: tint.parse()?,
                    });
                }
                ["tint", r, g, b] => {
                    let c = floats(&[r, g, b])?;
                    tints.push([c[0], c[1], c[2]]);
                }
                _ => bail!("unknown line: {line}"),
            }
        }
        let rdm = rdm.context("no mesh line")?;
        Ok((
            anno_core::neutral::Shape::Mesh { rdm, transform },
            materials,
            tints,
        ))
    }

    /// Each mesh piece as an `.skm` named after it, with its mesh levels and
    /// textures copied as they are: the skeleton keeps part placements, so
    /// nothing is baked. It has no decals and no effects.
    fn skm_write(
        model: &anno_core::neutral::Model,
        from: &dyn anno_core::neutral::Source,
    ) -> anyhow::Result<anno_core::neutral::Ported> {
        use anno_core::neutral::{Shape, texture_files, texture_source};
        use anno_formats::model::Alpha;
        use std::fmt::Write;
        let mut out = anno_core::neutral::Ported {
            left_out: model.left_out.clone(),
            ..Default::default()
        };
        // A path is one word of a line.
        let word = |p: &str| -> anyhow::Result<String> {
            anyhow::ensure!(!p.contains(char::is_whitespace), "{p:?} has a space");
            Ok(p.to_owned())
        };
        for piece in &model.pieces {
            let Shape::Mesh { rdm, transform } = &piece.shape else {
                out.left_out
                    .push(format!("{}: the skeleton has no decals", piece.label));
                continue;
            };
            let path = format!("{}.skm", from.place(&piece.name));
            let mut skm = format!("skm 1\nmesh {}\n", word(&from.place(rdm))?);
            for (level, f) in anno_core::files::siblings(rdm).iter().enumerate() {
                match from.read(f) {
                    Ok(bytes) => out.files.push((from.place(f), bytes)),
                    Err(e) if level == 0 => return Err(e.context(format!("mesh {rdm}"))),
                    Err(_) => {}
                }
            }
            let t = transform;
            let at: Vec<String> = (t.position.iter().chain(&t.rotation).chain(&t.scale))
                .map(f32::to_string)
                .collect();
            writeln!(skm, "at {}", at.join(" "))?;
            for m in &piece.materials {
                let mut texture = |named: &Option<String>| -> anyhow::Result<String> {
                    let Some(named) = named else {
                        return Ok("-".to_owned());
                    };
                    let source = texture_source(named);
                    out.files.extend(texture_files(from, &source));
                    word(&from.place(&source))
                };
                let (d, n, me) = (
                    texture(&m.diffuse)?,
                    texture(&m.normal)?,
                    texture(&m.metallic)?,
                );
                let alpha = match m.alpha {
                    Alpha::Opaque => "opaque".to_owned(),
                    Alpha::Cutout(v) => format!("cutout:{v}"),
                    Alpha::Blend(v) => format!("blend:{v}"),
                    Alpha::Tested(v) => format!("tested:{v}"),
                    Alpha::Decal => "decal".to_owned(),
                    Alpha::Hidden => "hidden".to_owned(),
                };
                let [r, g, b] = m.color;
                writeln!(
                    skm,
                    "material {d} {n} {me} {r} {g} {b} {alpha} {} {} {}",
                    m.double_sided as u8, m.ground_shaded as u8, m.terrain_tint
                )?;
            }
            for [r, g, b] in &piece.tints {
                writeln!(skm, "tint {r} {g} {b}")?;
            }
            out.files.push((path.clone(), skm.into_bytes()));
            out.pieces.push((path, piece.at));
        }
        out.left_out
            .extend(model.effects.iter().map(|e| e.missing.clone()));
        let mut seen = std::collections::HashSet::new();
        out.files.retain(|(p, _)| seen.insert(p.clone()));
        Ok(out)
    }

    /// Test maps as 2070's will be: one world file that places every island
    /// on the test map, listed by path.
    pub struct OneWorld;
    static ONE_WORLD: MapRules = MapRules {
        setups: &[Setup::NONE],
        roles: &[],
        size_classes: &[],
        grid: 16,
        fixed_turns: true,
        pool_slots: false,
        free_starts: false,
        min_starts: 0,
        min_gap: 0,
        min_edge: 0,
        layout_folder: ["data/maps/", ""],
        registers: Registers::None,
    };
    impl Maps for OneWorld {
        fn rules(&self) -> &'static MapRules {
            &ONE_WORLD
        }
        fn plan(
            &self,
            _: &Install,
            _: Option<&Assets>,
            islands: &[MapIsland],
            mod_id: &str,
            _: Option<&anno_core::map::Map>,
        ) -> anyhow::Result<Vec<MapPlan>> {
            let path = format!("data/maps/{mod_id}.world");
            let placed: Vec<usize> = (0..islands.len())
                .filter(|&i| islands[i].on_test_map)
                .collect();
            let listed: Vec<&str> = placed.iter().map(|&i| islands[i].path).collect();
            Ok(vec![MapPlan {
                region: String::new(),
                island_size: "Medium",
                size: [1024; 2],
                slots: (placed.iter())
                    .map(|&i| PlanSlot {
                        position: [0; 2],
                        class: 1,
                        starter: false,
                        fill: Fill::Ours(i),
                    })
                    .collect(),
                relabels_map_type: false,
                entry: MapEntry {
                    path: path.clone(),
                    region: String::new(),
                    island_size: "Medium".into(),
                    source_guids: Vec::new(),
                },
                files: vec![(path, listed.join("\n").into_bytes())],
            }])
        }
    }
}
