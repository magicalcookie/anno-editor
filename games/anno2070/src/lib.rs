// Anno 2070: the editor lists, previews and extracts its files, edits its
// islands through their own codec (`island`), lights them by the game's own
// light profiles and sky (`scene`, `sky`), draws their sea (`water`), and
// other games can bring its models in as props (`model`). No UI screens or
// packaging yet. Owns its `Profile`, its launcher wording and its archive
// mount order.

pub mod assets;
mod files;
pub mod harbour;
pub mod island;
mod model;
pub mod objects;
mod project;
pub mod scene;
pub mod sky;
pub mod underwater;
mod water;

use anno_core::game::{
    Game, IslandKind, IslandRules, Islands, Module, Profile, Role, SceneParts, SizeClass,
};
use anno_core::text::{GameText, Playtest};
use anno_formats::vfs::Install;

/// What full glow lights a texel to, times its albedo.
const GLOW_BASE: f32 = 8.;

/// What the editor asks of this game.
pub static MODULE: Anno2070 = Anno2070;

pub struct Anno2070;

impl Module for Anno2070 {
    fn game(&self) -> Game {
        Game::Anno2070
    }
    fn profile(&self) -> &'static Profile {
        &PROFILE
    }
    fn text(&self) -> &'static GameText {
        &TEXT
    }
    // Steam's library hero image (1920×620), re-encoded at JPEG quality 82,
    // as the other games' cards are.
    fn card_art(&self) -> &'static [u8] {
        include_bytes!("../assets/card.jpg")
    }
    // The game reads base islands under `data/` and the add-on's under
    // `addondata/`; a project writes either.
    fn project_roots(&self) -> &'static [&'static str] {
        &["data", "addondata"]
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
        Some(model::read(install, source))
    }
    // 2070's sea-green, at the lightness of the other two cards.
    fn accent(&self) -> [u32; 2] {
        [0x9fd2c3, 0x2f6e60]
    }
    fn files(&self) -> &'static [anno_core::files::Kind] {
        files::KINDS
    }
    fn database(&self) -> Option<&'static anno_formats::assets::Schema> {
        Some(&assets::SCHEMA)
    }
    // Undyed, as no player owns a preview; on an island each object is dyed
    // by its owner (`owner_word`). 2070 lights from its G-buffer:
    // where the normal map's blue says, lighting turns toward GLOW_BASE
    // times the albedo (deferredshading.fx:44-48, :105-114; 4 under
    // low-quality HDR, which the editor doesn't model). Its models draw in
    // its building family; the glow here is for a renderer without it.
    fn ports(&self) -> anno_core::files::Ports {
        anno_core::files::Ports {
            props_from: &[],
            // Its cfgs port into the other games as props.
            prop_ext: ".cfg",
            mesh_import: false,
        }
    }
    fn model_look(&self) -> anno_core::files::ModelLook {
        anno_core::files::ModelLook {
            dye: [1.; 3],
            glow: GLOW_BASE,
            deferred_glow: true,
            window_interior: None,
            surfaces: Some(anno_core::files::SurfacesOf(&scene::Look)),
            player_colors: &PLAYER_COLORS,
            // Model terrain decals draw instanced (terraindecal.fx
            // vs_Decal2/3), moved 0.004 m along the view direction in depth
            // alone. When Anno5.exe draws CTerrainDecalRenderer
            // (build 11514956 sub_140881540) against the models was not traced: they stay
            // over everything opaque.
            decals: anno_core::files::DecalLook {
                box_bias: 0.004,
                grazing: false,
                box_on_ground: false,
                mesh_as_ground: false,
            },
        }
    }
}

// Islands edit their terrain, paint, blockers and objects, and place models
// as objects that copy shipped records. No `harbours()`: the shared coast,
// slot and work-area pieces are the later games' object-store records,
// while 2070's harbours are `Harbour` handles (12,900 wall records) laid by
// its codec (`harbour_along`), and its resources are handles too, with no
// slot records to collect. Ground words brought over keep their own.
impl Islands for Anno2070 {
    fn rules(&self) -> &'static IslandRules {
        &RULES
    }
    fn maps(&self) -> Option<&dyn anno_core::map::Maps> {
        Some(&project::World)
    }
    fn codec(&self) -> anno_core::game::Codec<'_> {
        anno_core::game::Codec::Own(self)
    }
    // 2070 has no island assets, and a world's `Type` is per placement: the
    // shipped worlds place one `normal/` island as regular (0), start (2)
    // or decoration (5). The folder is what stays: `normal/` and
    // `underwater/` hold the islands a map plays, `third parties/` the
    // islands worlds place as third parties (3, 4), and the rest are
    // campaign, scenario, world event and test islands.
    // Buildable ground is where the gameplay heights the codec derives
    // from the terrain (`isd::grids::gameplay_height`) fall in a band the
    // game builds on, the land's or the Deep Ocean plateau's
    // (`island::buildable`), so an island with land that maps settle
    // needs a cell in one. Islands under water don't: u_d09 and u_d11 ship
    // in `underwater/`, worlds place them, and neither has ground in the
    // plateau's band (u_d11's lies below it, at the -31 m floor), so a Deep
    // Ocean island may hold resources and no building ground. Start coasts
    // aren't checked: 2070 lays them as harbour tiles by rules the data
    // doesn't state.
    fn validate(
        &self,
        island: &anno_core::island::Island,
        refs: &anno_core::game::Refs,
        use_: anno_core::game::Use,
    ) -> Vec<anno_core::island::Problem> {
        use anno_formats::isd::grids::gameplay_height;
        use island::buildable;
        let use_ = anno_core::game::Use {
            start: false,
            ..use_
        };
        let mut out = anno_core::game::broken(self, island, refs, use_);
        let f = &island.doc.field;
        let (w, d) = (f.grid.width, f.grid.depth);
        let at = |x: usize, z: usize| f.heights[z * w + x];
        let land = f.heights.iter().any(|&h| h > 0.);
        let buildable = (1..d).any(|z| {
            (1..w).any(|x| {
                let corners = [at(x - 1, z - 1), at(x, z - 1), at(x - 1, z), at(x, z)];
                buildable(gameplay_height(corners))
            })
        });
        if use_.playable && land && !buildable {
            out.push(anno_core::island::Problem {
                level: anno_core::island::Level::Broken,
                what: "No buildable ground: no terrain at the height the game builds on".into(),
                count: 1,
                items: Vec::new(),
                names: Default::default(),
            });
        }
        out
    }
    fn kind_of(&self, path: &str, _: Option<&str>) -> IslandKind {
        let path = path.replace('\\', "/").to_ascii_lowercase();
        match path.split('/').nth(3) {
            Some("normal" | "underwater") => IslandKind::Playable,
            _ => IslandKind::Other,
        }
    }
    // Every `.isd`, grouped by its folder; the maps' own (`normal/`,
    // `underwater/`) are the ones New island copies. Reads each file's
    // `Width` and `Height`: 384 files in about 1 s in release, off the UI
    // thread.
    fn shipped(
        &self,
        install: &Install,
        _: Option<&anno_formats::assets::Assets>,
    ) -> Vec<anno_core::game::ShippedIsland> {
        let mut out = Vec::new();
        for id in install.search(".isd", 100_000) {
            let path = install.file_path(id).to_owned();
            let world = match install.read_file(id).map(|b| project::sides(&b)) {
                Ok(Ok(world)) => world,
                Ok(Err(e)) | Err(e) => {
                    eprintln!("{path}: left out of the island list: {e:#}");
                    continue;
                }
            };
            let kind = self.kind_of(&path, None);
            out.push(anno_core::game::ShippedIsland {
                region: folder(&path).to_owned(),
                world,
                kind,
                pool: kind == IslandKind::Playable,
                path,
            });
        }
        out.sort_by(|a, b| a.path.cmp(&b.path));
        out
    }
}

// `.isd` files through the codec in formats and `island`; models place
// as objects of their asset, but trees, rocks and grass as props.
impl anno_core::game::IslandCodec for Anno2070 {
    fn read(
        &self,
        install: &Install,
        path: &str,
        assets: Option<&anno_formats::assets::Assets>,
    ) -> anyhow::Result<anno_core::island::Island> {
        island::read(install, path, assets)
    }
    fn write(
        &self,
        island: &anno_core::island::Island,
        path: &str,
        assets: Option<&anno_formats::assets::Assets>,
    ) -> anyhow::Result<Vec<u8>> {
        island::write(island, path, assets)
    }
    fn places_as_prop(&self, assets: &anno_formats::assets::Assets, cfg: &str) -> bool {
        objects::prop_kind(assets, cfg).is_some()
    }
    fn object_for(
        &self,
        install: &Install,
        assets: &anno_formats::assets::Assets,
        island: &anno_core::island::Island,
        cfg: &str,
    ) -> Option<anyhow::Result<anno_core::island::objects::Object>> {
        Some(objects::template(install, assets, &island.doc.objects, cfg))
    }
    // Islands of a kind sit side by side in one folder, each one file.
    fn island_path(&self, region: &str, name: &str) -> Option<String> {
        Some(format!("data/levels/islands/{region}/{name}.isd"))
    }
    // Mines and river slots are handles of their own assets (mine_base 335,
    // river_slot 350; 1,220 and 444 shipped). 2070 has no clay, gas or marsh
    // slots, and its oil fields lie underwater (underwater_oil_field 349), so
    // a land oil slot has no stand-in.
    fn slot_model(&self, resource: anno_core::harbour::Resource) -> Option<&'static str> {
        use anno_core::harbour::Resource;
        match resource {
            Resource::Mine | Resource::NewWorldMine => {
                Some("data/graphics/landscape/mines/mine_base_object01/mine_base_object01.cfg")
            }
            Resource::River => Some("data/graphics/landscape/river/river_slot.cfg"),
            Resource::Clay | Resource::Oil | Resource::Gas | Resource::Marsh => None,
        }
    }
    fn top_view(
        &self,
        install: &Install,
        path: &str,
        edge: u32,
    ) -> Option<gpui_editor_kit::texture::Texture> {
        island::top_view(install, path, edge)
    }
    fn harbour_along(
        &self,
        install: &Install,
        island: &anno_core::island::Island,
        coast: &[[f32; 2]],
    ) -> Option<anyhow::Result<Vec<anno_core::island::objects::Object>>> {
        Some(harbour::along(
            install,
            &island.doc.field,
            &island.doc.objects,
            coast,
        ))
    }
    // A quarter turn for an asset whose shipped records stand on them
    // (`objects::Shipped::quarter_turns`): buildings and harbour walls.
    // Mines, oil rigs and quarries turned to their deposit, units, trees,
    // rocks and grass turn freely.
    // Never waits for the index: the first drag after an island opened
    // spent 1.2 s (release) on the UI thread while it read every island.
    fn turn_step(&self, install: &Install, guid: u32) -> Option<f32> {
        Some(match objects::Shipped::ready(install)? {
            Ok(index) if index.quarter_turns(guid) => std::f32::consts::FRAC_PI_2,
            _ => 0.,
        })
    }
}

impl SceneParts for Anno2070 {
    // A record's `m_PlayerID` is its owner; 15 is nature's (trees, rocks,
    // grass: a million shipped records), and players 1 to 12 own the rest.
    // The game dyes each player's buildings in their profile's colour; an
    // island in the editor has no players, so player 1 takes the colour
    // View settings picks and the others the profile list's next ones.
    fn owner_word(&self, owner: u32, player: usize) -> u32 {
        if !(1..15).contains(&owner) {
            return 0;
        }
        let c = &PLAYER_COLORS[(player + owner as usize - 1) % PLAYER_COLORS.len()];
        anno_render::a2070_buildings::owner_word(c.rgb)
    }
    fn ambients(
        &self,
        install: &Install,
        names: &[&str],
    ) -> Vec<(String, anno_core::ambient::Ambient)> {
        scene::ambients(install, names)
    }
    fn island_ambient(&self, install: &Install, path: &str) -> Option<String> {
        let map = scene::AmbientMap::read(install)
            .map_err(|e| eprintln!("2070 light profiles unavailable: {e:#}"))
            .ok()?;
        let underwater = project::underwater(install, path).unwrap_or(false);
        scene::island_profile(install, &map, path, underwater)
    }
    // The game shows its Deep Ocean islands from below the surface, where
    // its camera dives to build on them (underwater.rs).
    fn island_view(&self, install: &Install, path: &str) -> Option<anno_core::ambient::View> {
        (project::underwater(install, path).ok()?).then(|| underwater::view(install))
    }
    fn sky(&self, install: &Install) -> Option<std::sync::Arc<dyn anno_core::ambient::SkyPass>> {
        Some(std::sync::Arc::new(sky::SkyPasses::new(install)))
    }
    fn water(&self, install: &Install) -> Option<std::sync::Arc<dyn anno_core::water::Water>> {
        Some(std::sync::Arc::new(water::Water::new(install)))
    }
    fn ground(
        &self,
        install: &Install,
        paint: &anno_core::island::Paint,
    ) -> Option<anyhow::Result<Box<dyn anno_core::game::IslandGround>>> {
        let anno_core::island::Paint::Layers(paint) = paint else {
            return None;
        };
        Some(island::Ground::new(install, paint).map(|g| Box::new(g) as _))
    }
    fn warm(&self, _install: &Install, gpu: &gpui_editor_kit::gpu::Warm<'_>) {
        sky::warm(gpu);
        water::warm(gpu);
        underwater::warm(gpu);
        let families = scene::families();
        gpu.families(&families);
        anno_render::layers::warm(gpu, &families);
    }
}

/// The player colours a profile can pick at no cost, in the game's order:
/// `PlayerColors` in data/config/game/properties.xml, ARGB as signed
/// integers there. The names are the editor's; the game's text keys
/// (122564 on) aren't read.
const PLAYER_COLORS: [anno_core::game::PlayerColor; 8] = {
    use anno_core::game::PlayerColor;
    [
        PlayerColor {
            name: "Yellow",
            rgb: [0xf9, 0xe2, 0x10],
        },
        PlayerColor {
            name: "Green",
            rgb: [0x53, 0x7d, 0x0c],
        },
        PlayerColor {
            name: "Orange",
            rgb: [0xff, 0x88, 0x00],
        },
        PlayerColor {
            name: "Mint",
            rgb: [0x8a, 0xd1, 0xa0],
        },
        PlayerColor {
            name: "Red",
            rgb: [0xcf, 0x35, 0x35],
        },
        PlayerColor {
            name: "Blue",
            rgb: [0x31, 0x85, 0xa3],
        },
        PlayerColor {
            name: "Pink",
            rgb: [0xee, 0x79, 0xb0],
        },
        PlayerColor {
            name: "Purple",
            rgb: [0x9d, 0x6b, 0xa7],
        },
    ]
};

/// The order Anno5.exe mounts `maindata`, by the names it builds
/// (`data%d`, `%s%d` for the language, `patch%d`, `addon%d`,
/// `addon_%s%d`, `exedata`) and checked against the archives: where two
/// hold the same path, the later one here has the newer file (patches over
/// every data and language archive, addon0 over patch7, addon_eng over
/// eng7, exedata over patch1-3). Every language mounts: their files sit
/// under `data/loca/<language>/`, apart from each other.
pub(crate) const ARCHIVE_FAMILIES: &[&str] = &[
    "data",
    "cze",
    "eng",
    "esp",
    "fra",
    "ger",
    "ita",
    "pol",
    "rus",
    "patch",
    "addon",
    "addon_cze",
    "addon_eng",
    "addon_esp",
    "addon_fra",
    "addon_ger",
    "addon_ita",
    "addon_pol",
    "addon_rus",
    "exedata",
];

// No mod loader: the manifest fields stay BARE's.
static PROFILE: Profile = Profile {
    title: "Anno 2070",
    short_name: "2070",
    env_var: "ANNO2070_DIR",
    config_folder: "anno-editor/2070",
    steam_app_id: "48240",
    steam_folder: "Anno 2070",
    documents_folder: "Anno 2070",
    exe: "Anno5.exe",
    archive_families: ARCHIVE_FAMILIES,
    ..Profile::BARE
};

// Islands go through their own codec (`island`), so of these rules 2070
// states its size classes, roles, slot yaw and import source; the rest stay BARE's,
// so a stray use finds nothing of 117's or 1800's.
// From the shipped islands' names and sides: n_s 128-160 m, n_m
// 176-208 m, n_l 224-256 m. Start slots in the shipped worlds are
// 192-256 m, nearly all 240 or 256. Underwater (u_*, 80-144 m) and
// decoration islands (64-176 m) fall in Small.
pub(crate) const SIZE_CLASSES: &[SizeClass] = &[
    SizeClass {
        name: "Small",
        slot: 160,
        start: false,
    },
    SizeClass {
        name: "Medium",
        slot: 208,
        start: false,
    },
    SizeClass {
        name: "Large",
        slot: 256,
        start: true,
    },
];

static RULES: IslandRules = IslandRules {
    size_classes: SIZE_CLASSES,
    environment_types: &[("None", 0)],
    // The island list's names for the folders `shipped` groups by.
    provinces: &[
        province("normal", "Surface"),
        province("underwater", "Deep Ocean"),
        province("third parties", "Third parties"),
        province("campaign", "Campaign"),
        province("scenarios", "Scenarios"),
        province("world events", "World events"),
        province("benchmark", "Benchmark"),
        province("testworlds", "Test worlds"),
        province("dlc_01", "DLC 1"),
    ],
    // 2070 has no `IslandType`: a project keeps the later games' values,
    // and the test map reads only whether an island is regular or a start.
    roles: &[
        Role {
            label: "Regular",
            value: "Normal",
            regular: true,
            start: false,
        },
        Role {
            label: "Start",
            value: "Starter",
            regular: false,
            start: true,
        },
        Role {
            label: "Regular or start",
            value: "Normal;Starter",
            regular: true,
            start: true,
        },
    ],
    import_from: &[Game::Anno117, Game::Anno1800],
    slot_yaw_step: std::f32::consts::FRAC_PI_2,
    underwater_islands: true,
    ..IslandRules::BARE
};

const fn province(region: &'static str, name: &'static str) -> anno_core::game::Province {
    anno_core::game::Province {
        region,
        name,
        prop_sets: "",
    }
}

/// The folder the island list groups island `path` under: the one under
/// `levels/islands/` (`normal`, `underwater`, `campaign`), else the one
/// under `levels/` (`benchmark`, `testworlds`).
fn folder(path: &str) -> &str {
    let mut parts = path.split(['/', '\\']).skip(2);
    match parts.next() {
        Some(f) if f.eq_ignore_ascii_case("islands") => parts.next(),
        f => f,
    }
    .filter(|f| !f.ends_with(".isd"))
    .unwrap_or("")
}

static TEXT: GameText = GameText {
    subtitle: "Islands",
    era: "THE FUTURE IS IN YOUR HANDS",
    regions: "Edit its islands; browse its archives, textures and models",
    add_model: "",
    model_formats: "",
    slot_resource: "",
    slot_roads: "",
    playtest: Playtest {
        intro: "",
        steps: "",
        in_game: "",
    },
    playtest_old_world: None,
    // The factions asset texts name, and the run-together words of the
    // model names the inspector shows.
    asset_words: anno_core::game::AssetWords {
        proper: &["Eden", "Ecos", "Tycoon", "Tycoons"],
        joined: &[
            ("harboursystem", "harbour system"),
            ("jungletree", "jungle tree"),
            ("parksystem", "park system"),
            ("glasssystem", "glass system"),
            ("powerplant", "power plant"),
            ("powerpole", "power pole"),
            ("markethouse", "market house"),
            ("startship", "start ship"),
            ("buildspace", "build space"),
            ("cargoprops", "cargo props"),
            ("rockbridge", "rock bridge"),
            ("smallmountain", "small mountain"),
            ("mediummountain", "medium mountain"),
            ("pathblocker", "path blocker"),
            ("buildblocker", "build blocker"),
            ("streetlamp", "street lamp"),
            ("lightbuoy", "light buoy"),
        ],
    },
};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::island::SCULPT;

    // An island's chunks as its bytes store them, read beside the codec so
    // tests check its writes against the file rather than against itself.
    pub(crate) struct Chunks<'a> {
        doc: &'a anno_formats::isd::Doc,
        elements: Vec<anno_formats::isd::NodeId>,
    }

    // A chunk's stored heights: `side` samples a row, little-endian f32
    // metres, row by row.
    #[derive(Clone, Copy)]
    pub(crate) struct Samples<'a> {
        pub(crate) side: usize,
        pub(crate) samples: &'a [u8],
    }

    impl Samples<'_> {
        pub(crate) fn at(&self, x: usize, z: usize) -> f32 {
            let i = (z * self.side + x) * 4;
            f32::from_le_bytes(self.samples[i..i + 4].try_into().unwrap())
        }
    }

    impl<'a> Chunks<'a> {
        pub(crate) fn read(doc: &'a anno_formats::isd::Doc) -> Chunks<'a> {
            let map = doc.child(doc.root("Terrain").unwrap(), "ChunkMap").unwrap();
            let elements = (doc.children(map))
                .filter(|&c| doc.name(c) == b"Element")
                .collect();
            Chunks { doc, elements }
        }

        fn value(&self, k: usize, path: &[&str]) -> Option<anno_formats::isd::Value<'a>> {
            let mut at = self.elements[k];
            for name in path {
                at = self.doc.child(at, name)?;
            }
            Some(self.doc.value(at))
        }

        // `None` for an empty ocean chunk.
        pub(crate) fn heights(&self, k: usize) -> Option<Samples<'a>> {
            use anno_formats::isd::Value;
            let Value::Text(side) = self.value(k, &["HeightMap", "Width"])? else {
                panic!("chunk {k}: a HeightMap Width that is no number");
            };
            let Some(Value::Blob(samples)) = self.value(k, &["HeightMap", "Data"]) else {
                panic!("chunk {k}: HeightMap without Data");
            };
            let side = std::str::from_utf8(side).unwrap().parse().unwrap();
            Some(Samples { side, samples })
        }

        pub(crate) fn flags(&self, k: usize) -> u32 {
            let Some(anno_formats::isd::Value::Text(t)) = self.value(k, &["Flags"]) else {
                panic!("chunk {k}: no Flags");
            };
            std::str::from_utf8(t).unwrap().parse().unwrap()
        }
    }

    // The shipped gameplay height rule over the 1 m cell at (x, z); `None`
    // where a corner is in empty ocean.
    pub(crate) fn gameplay_at(
        corners: &anno_formats::isd::terrain::Corners,
        x: usize,
        z: usize,
    ) -> Option<i16> {
        let c = [(x, z), (x + 1, z), (x, z + 1), (x + 1, z + 1)];
        Some(anno_formats::isd::grids::gameplay_height([
            corners.get(c[0].0, c[0].1)?,
            corners.get(c[1].0, c[1].1)?,
            corners.get(c[2].0, c[2].1)?,
            corners.get(c[3].0, c[3].1)?,
        ]))
    }

    // `island`'s paint, as the editor's brush edits it.
    fn paint_of(island: &mut anno_core::island::Island) -> &mut anno_core::island::Layers {
        match &mut island.doc.materials.as_mut().unwrap().paint {
            anno_core::island::Paint::Layers(l) => l,
            _ => panic!("2070 paint"),
        }
    }

    #[test]
    fn island_kinds_follow_the_island_folders() {
        let kind = |path| MODULE.kind_of(path, None);
        assert_eq!(
            kind("data/levels/islands/normal/n_m02.isd"),
            IslandKind::Playable
        );
        assert_eq!(
            kind("addondata/levels/islands/underwater/u_s21.isd"),
            IslandKind::Playable
        );
        assert_eq!(
            kind("data\\levels\\Islands\\Normal\\N_L01.isd"),
            IslandKind::Playable
        );
        assert_eq!(
            kind("addondata/levels/islands/third parties/u_x03_rift.isd"),
            IslandKind::Other
        );
        assert_eq!(
            kind("data/levels/islands/campaign/chap3_n_01.isd"),
            IslandKind::Other
        );
        assert_eq!(
            kind("data/levels/islands/dlc01_scenario01_n_l25.isd"),
            IslandKind::Other
        );
    }

    // Needs Anno 2070: none of the 384 islands it ships has a level-5
    // problem in 2070: every model and asset resolves, and the islands its
    // maps settle (`normal/`, `underwater/`, by `kind_of`) that have land
    // have buildable ground.
    #[test]
    fn every_shipped_island_passes_its_own_games_checks() {
        use anno_core::game::{Refs, Use};
        let Some(dir) = anno_formats::vfs::install_dir(PROFILE.env_var) else {
            return;
        };
        let install = Install::open(&dir, ARCHIVE_FAMILIES).unwrap();
        let assets = anno_formats::assets::Assets::load(&install, &assets::SCHEMA).unwrap();
        let paths: Vec<String> = (0..install.file_count())
            .map(|i| {
                install
                    .file_path(anno_formats::vfs::FileId(i as u32))
                    .to_owned()
            })
            .filter(|p| p.ends_with(".isd"))
            .collect();
        assert_eq!(paths.len(), 384);
        let none = |_: &str| false;
        let refs = Refs {
            install: &install,
            project: &none,
            assets: Some(&assets),
        };
        let mut failed = Vec::new();
        for path in &paths {
            let island = island::read(&install, path, Some(&assets)).unwrap();
            let use_ = Use {
                region: RULES.island_region(path),
                start: false,
                playable: MODULE.kind_of(path, None) == IslandKind::Playable,
            };
            let problems = MODULE.validate(&island, &refs, use_);
            if !problems.is_empty() {
                let said: Vec<String> = problems.iter().map(|p| p.said()).collect();
                failed.push(format!("{path}: {}", said.join("; ")));
            }
        }
        assert!(failed.is_empty(), "{failed:#?}");
    }

    // Needs Anno 2070: the folder rule agrees with how the 57 shipped worlds
    // place islands. Every island a world places as a third party (`Type` 3
    // or 4) is Other, and every placed `normal/` or `underwater/` island is
    // placed at least once as a regular, underwater or start island.
    #[test]
    fn island_kinds_agree_with_the_shipped_worlds() {
        use anno_formats::isd::world;
        let Some(dir) = anno_formats::vfs::install_dir(PROFILE.env_var) else {
            return;
        };
        let install = Install::open(&dir, ARCHIVE_FAMILIES).unwrap();
        let mut placed = std::collections::BTreeMap::<String, u32>::new();
        let mut worlds = 0;
        for i in 0..install.file_count() {
            let id = anno_formats::vfs::FileId(i as u32);
            if !install.file_path(id).ends_with(".www") {
                continue;
            }
            worlds += 1;
            let doc = anno_formats::isd::Doc::parse(&install.read_file(id).unwrap()).unwrap();
            for p in world::read(&doc).unwrap() {
                let file = p.file.replace('\\', "/").to_ascii_lowercase();
                *placed.entry(file).or_default() |= 1 << p.kind as u8;
            }
        }
        assert_eq!(worlds, 57);
        let bits = |kinds: &[world::Kind]| kinds.iter().fold(0u32, |b, &k| b | 1 << k as u8);
        let third_party = bits(&[
            world::Kind::ThirdPartyUnderwater,
            world::Kind::ThirdPartySurface,
        ]);
        let playable = bits(&[
            world::Kind::Regular,
            world::Kind::Underwater,
            world::Kind::Start,
        ]);
        let mut played = 0;
        for (file, kinds) in &placed {
            let kind = MODULE.kind_of(file, None);
            if kinds & third_party != 0 {
                assert_eq!(kind, IslandKind::Other, "{file} {kinds:b}");
            }
            if kind == IslandKind::Playable {
                played += 1;
                assert!(kinds & playable != 0, "{file} {kinds:b}");
            }
        }
        assert!(played > 100, "{played}");
    }

    // Needs Anno 2070 (ANNO2070_DIR): the whole install mounts in the game's
    // order, and where a patch replaced a file the patched one is served.
    #[test]
    fn the_install_mounts_with_patches_on_top() {
        let Some(dir) = anno_formats::vfs::install_dir(PROFILE.env_var) else {
            return;
        };
        let install = Install::open(&dir, ARCHIVE_FAMILIES).unwrap();
        assert!(install.file_count() > 30_000, "{}", install.file_count());
        let names = install.archives().iter().cloned();
        let names: Vec<String> = names.collect();
        let at = |n: &str| names.iter().position(|a| a.ends_with(n)).unwrap();
        assert!(at("Data5.rda") < at("eng0.rda"));
        assert!(at("eng8.rda") < at("patch1.rda"));
        assert!(at("patch9.rda") < at("addon0.rda"));
        assert!(at("addon_eng1.rda") < at("exedata.rda"));
        assert!(!names.iter().any(|n| n.ends_with("checksums.rda")));
        // patch9 carries the last credits; Data/eng archives the older ones.
        let layers = install.layers(install.file("data/loca/eng/txt/credits.txt").unwrap());
        assert!(layers.len() > 1);
    }

    // Needs Anno 2070: every copy of every island (`.isd`) and world
    // (`.www`) in the archives, the ones a later archive replaces included,
    // writes back byte for byte from its parsed tree. Prints how many each
    // archive holds (`--nocapture`). The counts are the Steam install's,
    // patch 9 with the Deep Ocean add-on.
    #[test]
    fn every_shipped_island_and_world_writes_back() {
        use anno_formats::isd::Doc;
        use anno_formats::vfs::FileId;

        let Some(dir) = anno_formats::vfs::install_dir(PROFILE.env_var) else {
            return;
        };
        let install = Install::open(&dir, ARCHIVE_FAMILIES).unwrap();
        let archives = install.archives();
        // Per archive: (islands, worlds, bytes, copies a later archive hides).
        let mut held = vec![(0, 0, 0u64, 0); archives.len()];
        let (mut islands, mut worlds) = (0, 0);
        let mut replaced = Vec::new();
        for i in 0..install.file_count() {
            let id = FileId(i as u32);
            let path = install.file_path(id);
            let island = path.ends_with(".isd");
            if !island && !path.ends_with(".www") {
                continue;
            }
            *(if island { &mut islands } else { &mut worlds }) += 1;
            let layers = install.layers(id);
            for (n, layer) in layers.iter().enumerate() {
                assert!(!layer.is_tombstone(), "{path}: deleted by an archive");
                let bytes = install.read_layer(id, layer).unwrap();
                let archive = &archives[layer.archive as usize];
                let doc =
                    Doc::parse(&bytes).unwrap_or_else(|e| panic!("{path} in {archive}: {e:#}"));
                assert!(
                    doc.encode() == bytes,
                    "{path} in {archive} writes back changed"
                );
                let row = &mut held[layer.archive as usize];
                *(if island { &mut row.0 } else { &mut row.1 }) += 1;
                row.2 += bytes.len() as u64;
                if n + 1 < layers.len() {
                    row.3 += 1;
                }
            }
            if layers.len() > 1 {
                let by: Vec<&str> = layers
                    .iter()
                    .map(|l| &*archives[l.archive as usize])
                    .collect();
                replaced.push(format!("{path}: {}", by.join(" < ")));
            }
        }
        for (name, (i, w, bytes, hidden)) in archives.iter().zip(&held) {
            if i + w > 0 {
                eprintln!(
                    "{name}: {i} islands, {w} worlds, {bytes} bytes, {hidden} replaced later"
                );
            }
        }
        for line in &replaced {
            eprintln!("replaced: {line}");
        }
        assert_eq!((islands, worlds), (384, 57));
    }

    // Needs Anno 2070: every shipped island reads through the terrain view,
    // which checks the facts the editor's 2070 ground paint and heights rest
    // on (the plan's `Layers` form). Each 16 m chunk lists its texture
    // layers by ascending `TextureIndex`, without repeats, starting with
    // layer 0 (the opaque base), at most 24 of them, each a 17×17 byte
    // coverage plane; chunks that share an edge hold the same coverage and
    // the same height bits along it. Every layer is in the ground texture
    // registry, where its draw rank comes from. The counts are the Steam
    // install's (`--nocapture` prints the flag census).
    #[test]
    fn shipped_terrain_chunks_keep_their_layer_and_border_rules() {
        use anno_formats::isd::Doc;
        use anno_formats::isd::terrain::{GroundTextures, Terrain};
        use anno_formats::vfs::FileId;

        let Some(dir) = anno_formats::vfs::install_dir(PROFILE.env_var) else {
            return;
        };
        let install = Install::open(&dir, ARCHIVE_FAMILIES).unwrap();
        let registry = install
            .read("data/config/engine/groundtextures.xml")
            .unwrap();
        let ground = GroundTextures::parse(&String::from_utf8_lossy(&registry)).unwrap();
        assert_eq!(ground.textures().len(), 40);
        // File order is not draw order: earth_01 lists before the grasses
        // and draws over them.
        assert!(ground.rank(18) > ground.rank(72) && ground.rank(72) > ground.rank(52));

        let (mut files, mut chunks, mut painted, mut planes) = (0, 0, 0, 0);
        let (mut most, mut base_below, mut edges) = (0, 0, 0);
        // Chunks without heights, and at 1 m, 0.5 m and 0.25 m.
        let mut resolutions = [0; 4];
        let mut flags_seen = std::collections::BTreeMap::new();
        for i in 0..install.file_count() {
            let id = FileId(i as u32);
            let path = install.file_path(id);
            if !path.ends_with(".isd") {
                continue;
            }
            files += 1;
            let bytes = install.read_file(id).unwrap();
            let doc = Doc::parse(&bytes).unwrap();
            let terrain = Terrain::read(&doc, &path).unwrap_or_else(|e| panic!("{e:#}"));
            let stored = Chunks::read(&doc);
            let (w, h) = terrain.size();
            for k in 0..w * h {
                chunks += 1;
                let heights = stored.heights(k);
                resolutions[heights.map_or(0, |h| match h.side {
                    17 => 1,
                    33 => 2,
                    _ => 3,
                })] += 1;
                let ids = terrain.ids(k);
                *flags_seen
                    .entry((stored.flags(k), heights.is_some(), !ids.is_empty()))
                    .or_insert(0) += 1;
                if ids.is_empty() {
                    continue;
                }
                painted += 1;
                planes += ids.len();
                most = most.max(ids.len());
                base_below += terrain.plane(k, 0).iter().any(|&a| a < 255) as usize;
                for &layer in ids {
                    assert!(ground.rank(layer).is_some(), "{path}: layer {layer}");
                }
                // Edges shared with a painted chunk to the right and below.
                edges += (k % w + 1 < w && !terrain.ids(k + 1).is_empty()) as usize;
                edges += (k / w + 1 < h && !terrain.ids(k + w).is_empty()) as usize;
            }
        }
        eprintln!("{chunks} chunks, {planes} planes");
        for ((flags, heights, layers), n) in &flags_seen {
            eprintln!("flags {flags:#x}: heights {heights}, layers {layers}: {n} chunks");
        }
        assert_eq!(files, 384);
        assert_eq!(resolutions, [9_367, 35_812, 827, 4]);
        assert_eq!((painted, most, edges), (36_643, 24, 65_757));
        assert_eq!(base_below, 713);
    }

    // Needs Anno 2070: on every shipped island the 1 m corner grid holds
    // each chunk's own samples at whole metres bit for bit, the 0.5 m and
    // 0.25 m chunks included, so a corner on a shared edge matches every
    // chunk around it. Corners that only empty ocean chunks touch hold
    // nothing. The counts are the Steam install's.
    #[test]
    fn corners_match_native_samples_on_shared_edges() {
        use anno_formats::isd::Doc;
        use anno_formats::isd::terrain::Terrain;
        use anno_formats::vfs::FileId;

        let Some(dir) = anno_formats::vfs::install_dir(PROFILE.env_var) else {
            return;
        };
        let install = Install::open(&dir, ARCHIVE_FAMILIES).unwrap();
        let (mut files, mut held, mut shared, mut none) = (0, 0u64, 0u64, 0u64);
        for i in 0..install.file_count() {
            let id = FileId(i as u32);
            let path = install.file_path(id);
            if !path.ends_with(".isd") {
                continue;
            }
            files += 1;
            let doc = Doc::parse(&install.read_file(id).unwrap()).unwrap();
            let terrain = Terrain::read(&doc, &path).unwrap();
            let stored = Chunks::read(&doc);
            let corners = terrain.corners();
            let (w, h) = terrain.size();
            let (side, rows) = corners.size();
            assert_eq!((side, rows), (w * 16 + 1, h * 16 + 1), "{path}");
            // How many chunks gave each corner a height.
            let mut given = vec![0u8; side * rows];
            for k in 0..w * h {
                let Some(native) = stored.heights(k) else {
                    continue;
                };
                let step = (native.side - 1) / 16;
                for n in 0..17 * 17 {
                    let (x, z) = (n % 17, n / 17);
                    let (cx, cz) = (k % w * 16 + x, k / w * 16 + z);
                    assert_eq!(
                        corners.get(cx, cz).map(f32::to_bits),
                        Some(native.at(x * step, z * step).to_bits()),
                        "{path}: corner {cx},{cz} from chunk {k}"
                    );
                    given[cz * side + cx] += 1;
                }
            }
            for (n, &chunks) in given.iter().enumerate() {
                match chunks {
                    0 => {
                        assert_eq!(corners.get(n % side, n / side), None, "{path}");
                        none += 1;
                    }
                    1 => held += 1,
                    _ => shared += 1,
                }
            }
        }
        assert_eq!(files, 384);
        // Corners one chunk holds, corners chunks share, and empty ocean.
        assert_eq!((held, shared, none), (8_474_326, 1_027_130, 2_402_224));
    }

    // Needs Anno 2070: the brush's border rule where it bites hardest, saved
    // through the codec. On n_m08, chunks 101, 102, 114 and 115 meet at
    // vertex (176, 128) holding 24, 8, 14 and 6 layers. A colour the full
    // chunk lacks goes into none of them, and the save gives the island
    // back byte for byte. One it holds goes into the others, and the four
    // agree on the vertex afterwards. Then strokes of every amount over the
    // whole island, scatter and empty ocean included: after each texture
    // the save reads back (reading checks every shared edge) with the
    // brush's ids and planes in every chunk.
    #[test]
    fn the_n_m08_corner_takes_a_layer_into_every_chunk_or_none_and_saves() {
        use anno_core::island::Skipped;
        use anno_formats::isd::Doc;
        use anno_formats::isd::terrain::Terrain;

        let Some(dir) = anno_formats::vfs::install_dir(PROFILE.env_var) else {
            return;
        };
        let install = Install::open(&dir, ARCHIVE_FAMILIES).unwrap();
        let registry = island::registry(&install).unwrap();
        let ground = registry.textures();
        let path = "data/levels/islands/normal/n_m08.isd";
        let source: std::sync::Arc<[u8]> = install.read(path).unwrap().into();
        let loaded = Doc::parse(&source).unwrap();
        let before = Terrain::read(&loaded, path).unwrap();
        assert_eq!(before.size(), (13, 13));
        let mut isle = island::from_bytes(source.clone(), path, registry.clone(), None).unwrap();
        let save = |isle: &anno_core::island::Island| island::write(isle, path, None).unwrap();
        // The chunks around the vertex, and where it sits in their planes.
        let corner = [(101, 16 * 17 + 16), (102, 16 * 17), (114, 16), (115, 0)];
        assert_eq!(corner.map(|(k, _)| before.ids(k).len()), [24, 8, 14, 6]);
        let colours: Vec<u32> = (ground.textures().iter())
            .filter(|t| t.id != 0 && !ground.scatter(t.id))
            .map(|t| t.id)
            .collect();

        let lacking = *colours
            .iter()
            .find(|id| !before.ids(101).contains(id))
            .unwrap();
        assert_eq!(
            paint_of(&mut isle).paint(176, 128, lacking as u8, 1.),
            Err(Skipped::Full { chunk: 101 })
        );
        assert!(save(&isle) == *source, "a refused dab changed the island");

        let held = *colours
            .iter()
            .find(|id| {
                before.ids(101).contains(id)
                    && !corner[1..].iter().any(|&(k, _)| before.ids(k).contains(id))
            })
            .unwrap();
        assert_eq!(
            paint_of(&mut isle).paint(176, 128, held as u8, 1.),
            Ok(true)
        );
        let written = save(&isle);
        let back = Doc::parse(&written).unwrap();
        assert!(back.encode() == written);
        let after = Terrain::read(&back, path).unwrap();
        assert_eq!(
            corner.map(|(k, _)| after.ids(k).len()),
            [24, 9, 15, 7],
            "layer {held}"
        );
        let (w, h) = after.size();
        for k in 0..w * h {
            let vertex = corner.iter().find(|c| c.0 == k).map(|c| c.1);
            for (layer, &id) in after.ids(k).iter().enumerate() {
                let was = before.ids(k).iter().position(|&b| b == id);
                assert!(
                    was.is_some() || (vertex.is_some() && id == held),
                    "chunk {k}"
                );
                let now = after.plane(k, layer);
                for i in 0..289 {
                    let old = was.map_or(0, |l| before.plane(k, l)[i]);
                    if Some(i) != vertex {
                        assert_eq!(now[i], old, "chunk {k} layer {id} at {i}");
                    } else if id == held {
                        assert_eq!(now[i], 255, "chunk {k}");
                    } else {
                        // Colour that drew over the new layer is gone here.
                        assert!(now[i] == old || now[i] == 0, "chunk {k} layer {id}");
                    }
                }
            }
            assert!(
                before.ids(k).iter().all(|id| after.ids(k).contains(id)),
                "chunk {k} lost a layer"
            );
        }

        // Strokes: each texture in turn, a diagonal line of dabs across the
        // island at a falling amount, so layers rise, fall, enter and leave.
        let side = w * 16 + 1;
        let mut step = 0usize;
        for t in ground.textures() {
            let paint = paint_of(&mut isle);
            for n in 0..side / 3 {
                step += 1;
                let (x, z) = ((n * 3 + step) % side, (n * 5 + step * 7) % side);
                let amount = [1., 0.6, 0.25, 0.03][step % 4];
                let _ = paint.paint(x, z, t.id as u8, amount);
            }
            let written = save(&isle);
            let doc = Doc::parse(&written).unwrap();
            let after = Terrain::read(&doc, path).unwrap();
            let paint = paint_of(&mut isle);
            for k in 0..w * h {
                let ids: Vec<u32> = paint.ids(k).iter().map(|&id| u32::from(id)).collect();
                if ids.is_empty() {
                    // Left as the file has it.
                    assert_eq!(after.ids(k), before.ids(k), "texture {}: chunk {k}", t.id);
                    continue;
                }
                assert_eq!(after.ids(k), ids, "texture {}: chunk {k}", t.id);
                for l in 0..ids.len() {
                    assert!(
                        after.plane(k, l) == paint.plane(k, l),
                        "texture {}: chunk {k} layer {l}",
                        t.id
                    );
                }
            }
        }
    }

    // Needs Anno 2070: every shipped island takes a height edit and a paint
    // edit and reads back with exactly those. Unedited heights and paint
    // write nothing. A one metre lift of one corner rewrites that chunk's
    // heights, its `Flags` bit 0 only if the 0.25 m predicate flipped, and
    // the four gameplay cells around the corner by the rule. The 44 islands
    // with v1 gameplay heights get v2 instead, every cell by the rule and
    // the floor where a cell reaches into empty ocean. A dab of the editor's
    // brush where up to four chunks meet lands in every one of them or in
    // none. Everything else in the file stays as loaded. The counts are the
    // Steam install's.
    #[test]
    fn every_shipped_island_takes_a_sculpt_and_a_dab() {
        use anno_core::island::Skipped;
        use anno_formats::isd::terrain::Terrain;
        use anno_formats::isd::{Doc, NodeId, Value};
        use anno_formats::vfs::FileId;

        // Whether two subtrees hold the same names and values.
        fn same(a: &Doc, x: NodeId, b: &Doc, y: NodeId) -> bool {
            let mut stack = vec![(x, y)];
            while let Some((x, y)) = stack.pop() {
                let (mut xs, mut ys) = (a.children(x), b.children(y));
                if a.name(x) != b.name(y) || a.value(x) != b.value(y) {
                    return false;
                }
                loop {
                    match (xs.next(), ys.next()) {
                        (Some(x), Some(y)) => stack.push((x, y)),
                        (None, None) => break,
                        _ => return false,
                    }
                }
            }
            true
        }
        fn gameplay(doc: &Doc) -> Option<&[u8]> {
            let grid = doc.child(doc.root("m_GOPManager")?, "m_GRIDManager")?;
            match doc.value(doc.child(grid, "m_HeightMap_v2")?) {
                Value::Blob(b) => Some(b),
                _ => None,
            }
        }

        let Some(dir) = anno_formats::vfs::install_dir(PROFILE.env_var) else {
            return;
        };
        let install = Install::open(&dir, ARCHIVE_FAMILIES).unwrap();
        let registry = island::registry(&install).unwrap();
        let ground = registry.textures();
        let (mut files, mut sculpted, mut v1, mut flipped) = (0, 0, 0, 0);
        let (mut dabbed, mut full, mut inserted) = (0, 0, 0);
        let (mut blocked, mut rebuilt, mut dropped) = (0, 0, 0);
        for i in 0..install.file_count() {
            let id = FileId(i as u32);
            let path = install.file_path(id);
            if !path.ends_with(".isd") {
                continue;
            }
            files += 1;
            let source = install.read_file(id).unwrap();
            let loaded = Doc::parse(&source).unwrap();
            let before = Terrain::read(&loaded, &path).unwrap();
            let stored = Chunks::read(&loaded);
            let (w, h) = before.size();
            let mut doc = Doc::parse(&source).unwrap();
            let mut corners = before.corners();
            corners.write(&mut doc, &path, SCULPT).unwrap();
            before.layers().write(&mut doc, &path, SCULPT).unwrap();
            // The editor's brush paints the dab below.
            let mut paint = anno_core::island::Layers::read(&before, registry.clone()).unwrap();
            assert!(
                doc.encode() == source,
                "{path}: an unedited write changed it"
            );

            // The sculpt: the middle corner of the first chunk stored at 1 m.
            let chunk = (0..w * h)
                .find(|&k| stored.heights(k).is_some_and(|h| h.side == 17))
                .unwrap_or_else(|| panic!("{path}: no chunk at 1 m"));
            let (x, z) = (chunk % w * 16 + 8, chunk / w * 16 + 8);
            let lifted = corners.get(x, z).unwrap() + 1.;
            corners.set(x, z, lifted);
            corners.write(&mut doc, &path, SCULPT).unwrap();
            let moved = true;
            sculpted += 1;

            // The dab: where four painted chunks meet, or else inside the
            // first painted chunk, with the first colour one of them lacks.
            let painted = |k: usize| !before.ids(k).is_empty();
            let vertex = (0..w * h)
                .find(|&k| {
                    k % w + 1 < w
                        && k / w + 1 < h
                        && [k, k + 1, k + w, k + w + 1].into_iter().all(painted)
                })
                .map(|k| ((k % w + 1) * 16, (k / w + 1) * 16))
                .or_else(|| {
                    let k = (0..w * h).find(|&k| painted(k))?;
                    Some((k % w * 16 + 8, k / w * 16 + 8))
                })
                .unwrap_or_else(|| panic!("{path}: no painted chunk"));
            // (chunk, index in its planes) for each chunk holding the vertex.
            let owners: Vec<(usize, usize)> = (0..w * h)
                .filter(|&k| painted(k))
                .filter_map(|k| {
                    let (lx, lz) = (
                        vertex.0.checked_sub(k % w * 16)?,
                        vertex.1.checked_sub(k / w * 16)?,
                    );
                    (lx <= 16 && lz <= 16).then_some((k, lz * 17 + lx))
                })
                .collect();
            let colour = (ground.textures().iter().map(|t| t.id))
                .filter(|&id| id != 0 && !ground.scatter(id))
                .find(|id| owners.iter().any(|&(k, _)| !before.ids(k).contains(id)))
                .unwrap();
            let lacking = |k: usize| !before.ids(k).contains(&colour);
            let dab = paint.paint(vertex.0, vertex.1, colour as u8, 1.).map(drop);
            match owners
                .iter()
                .find(|&&(k, _)| lacking(k) && before.ids(k).len() == 24)
            {
                Some(&(k, _)) => {
                    assert_eq!(dab, Err(Skipped::Full { chunk: k }), "{path}");
                    full += 1;
                }
                None => {
                    assert_eq!(dab, Ok(()), "{path}");
                    dabbed += 1;
                    inserted += owners.iter().filter(|&&(k, _)| lacking(k)).count();
                }
            }
            island::write_paint(&mut doc, &path, &paint).unwrap();

            // What was written, from its bytes.
            let written = doc.encode();
            let back = Doc::parse(&written).unwrap();
            assert!(
                back.encode() == written,
                "{path}: the edit does not re-encode"
            );
            let after = Terrain::read(&back, &path).unwrap();
            let written = Chunks::read(&back);
            for k in 0..w * h {
                let (was, now) = (stored.heights(k), written.heights(k));
                assert_eq!(was.map(|h| h.side), now.map(|h| h.side), "{path}");
                let above =
                    |h: &Samples| (0..h.side * h.side).all(|n| h.at(n % h.side, n / h.side) > 0.25);
                if let (Some(was), Some(now)) = (was, now) {
                    if moved && k == chunk {
                        let at = (8 * 17 + 8) * 4;
                        assert_eq!(now.samples[..at], was.samples[..at], "{path}");
                        assert_eq!(now.samples[at + 4..], was.samples[at + 4..], "{path}");
                        assert_eq!(now.at(8, 8).to_bits(), lifted.to_bits(), "{path}");
                        let flag = match above(&was) == above(&now) {
                            true => stored.flags(k),
                            false => {
                                flipped += 1;
                                (stored.flags(k) & !1) | above(&now) as u32
                            }
                        };
                        assert_eq!(written.flags(k), flag, "{path}");
                    } else {
                        assert!(now.samples == was.samples, "{path}: chunk {k} heights");
                    }
                }
                if !(moved && k == chunk) {
                    assert_eq!(written.flags(k), stored.flags(k), "{path}: chunk {k}");
                }
                let vertex = owners.iter().find(|o| o.0 == k).map(|o| o.1);
                for (layer, &id) in after.ids(k).iter().enumerate() {
                    let old = before.ids(k).iter().position(|&b| b == id);
                    assert!(
                        old.is_some() || (dab.is_ok() && vertex.is_some() && id == colour),
                        "{path}: chunk {k} gained layer {id}"
                    );
                    let now = after.plane(k, layer);
                    for n in 0..289 {
                        let was = old.map_or(0, |l| before.plane(k, l)[n]);
                        if dab.is_err() || Some(n) != vertex {
                            assert_eq!(now[n], was, "{path}: chunk {k} layer {id} at {n}");
                        } else if id == colour {
                            assert_eq!(now[n], 255, "{path}: chunk {k}");
                        } else {
                            assert!(now[n] == was || now[n] == 0, "{path}: chunk {k}");
                        }
                    }
                }
                // A layer goes only when the dab drew it down to nothing
                // everywhere: it covered the vertex alone.
                for (l, id) in before.ids(k).iter().enumerate() {
                    if !after.ids(k).contains(id) {
                        let alone =
                            (0..289).all(|n| Some(n) == vertex || before.plane(k, l)[n] == 0);
                        assert!(
                            dab.is_ok() && vertex.is_some() && alone,
                            "{path}: chunk {k} lost layer {id}"
                        );
                        dropped += 1;
                    }
                }
            }
            match (gameplay(&loaded), gameplay(&back)) {
                (Some(was), Some(now)) => {
                    let cells = w * 16;
                    let rule = after.corners();
                    assert_eq!(was.len(), now.len(), "{path}");
                    for n in 0..was.len() / 2 {
                        let (cx, cz) = (n % cells, n / cells);
                        let around = moved && (cx == x - 1 || cx == x) && (cz == z - 1 || cz == z);
                        let want = match around {
                            true => gameplay_at(&rule, cx, cz).unwrap().to_le_bytes(),
                            false => [was[n * 2], was[n * 2 + 1]],
                        };
                        assert_eq!(now[n * 2..n * 2 + 2], want, "{path}: cell {cx},{cz}");
                    }
                }
                // v1: every cell by the rule now, as v2.
                (None, Some(now)) => {
                    v1 += 1;
                    let cells = w * 16;
                    let rule = after.corners();
                    for n in 0..now.len() / 2 {
                        let (cx, cz) = (n % cells, n / cells);
                        let want = gameplay_at(&rule, cx, cz).unwrap_or(-31744);
                        assert_eq!(
                            now[n * 2..n * 2 + 2],
                            want.to_le_bytes(),
                            "{path}: cell {cx},{cz}"
                        );
                    }
                }
                _ => panic!("{path}: v2 gameplay heights went"),
            }
            // The rest of the file: every top-level element but the terrain
            // and the object manager, and in that everything but the grids.
            let pairs = |a: Option<NodeId>, b: Option<NodeId>| match (a, b) {
                (Some(a), Some(b)) => loaded.children(a).zip(back.children(b)),
                _ => panic!("{path}: no object manager"),
            };
            assert_eq!(loaded.roots().count(), back.roots().count(), "{path}");
            for (a, b) in loaded.roots().zip(back.roots()) {
                match loaded.name(a) {
                    b"Terrain" => assert_eq!(back.name(b), b"Terrain"),
                    b"m_GOPManager" => {
                        let (a, b) = (
                            loaded.child(a, "m_GRIDManager"),
                            back.child(b, "m_GRIDManager"),
                        );
                        for (a, b) in pairs(a, b) {
                            let name = loaded.name(a);
                            if name.starts_with(b"m_HeightMap") {
                                assert_eq!(back.name(b), b"m_HeightMap_v2", "{path}");
                                continue;
                            }
                            if name != b"m_PathBlockerLayer" && name != b"m_SubmarineBlockerLayer" {
                                assert!(same(&loaded, a, &back, b), "{path}: grids");
                                continue;
                            }
                            let ((cols, was), (_, now)) =
                                (bit_grid(&loaded, a), bit_grid(&back, b));
                            let rule = after.corners();
                            for (n, (&p, &q)) in was.iter().zip(&now).enumerate() {
                                if p == q {
                                    continue;
                                }
                                let (bx, bz) = (n % cols, n / cols);
                                if name == b"m_PathBlockerLayer" {
                                    // Only the four cells around the lifted
                                    // corner, when they left the band.
                                    let (cx, cz) = (bx / 2, bz / 2);
                                    assert!(
                                        (cx + 1 == x || cx == x) && (cz + 1 == z || cz == z),
                                        "{path}"
                                    );
                                    let old = match gameplay(&loaded) {
                                        Some(g) => i16::from_le_bytes([
                                            g[(cz * w * 16 + cx) * 2],
                                            g[(cz * w * 16 + cx) * 2 + 1],
                                        ]),
                                        None => {
                                            gameplay_at(&before.corners(), cx, cz).unwrap_or(-31744)
                                        }
                                    };
                                    let new = gameplay_at(&rule, cx, cz).unwrap();
                                    assert!(
                                        q && old <= 768 && new > 768,
                                        "{path}: path bit {bx},{bz}"
                                    );
                                    blocked += 1;
                                } else {
                                    // Within 4 m of the cells around the corner,
                                    // and by the rule there.
                                    assert!(
                                        bx + 5 >= x && bx <= x + 4 && bz + 5 >= z && bz <= z + 4,
                                        "{path}"
                                    );
                                    assert_eq!(
                                        q,
                                        submarine_rule(&rule, bx, bz),
                                        "{path}: submarine bit {bx},{bz}"
                                    );
                                    rebuilt += 1;
                                }
                            }
                        }
                    }
                    _ => assert!(same(&loaded, a, &back, b), "{path}: outside the terrain"),
                }
            }
            for (a, b) in loaded
                .children(loaded.root("m_GOPManager").unwrap())
                .zip(back.children(back.root("m_GOPManager").unwrap()))
            {
                let grids = loaded.name(a) == b"m_GRIDManager";
                assert!(grids || same(&loaded, a, &back, b), "{path}: objects");
            }
        }
        eprintln!(
            "{flipped} flags flipped, {blocked} path bits blocked, {rebuilt} submarine bits rebuilt, {dropped} layers dropped"
        );
        assert_eq!((files, sculpted, v1), (384, 384, 44));
        // The dab drew 17 layers that covered its vertex alone to nothing.
        assert_eq!(dropped, 17);
        // Every dab landed, most of them as a new layer in several chunks.
        assert_eq!((dabbed, full, inserted), (384, 0, 1_510));
    }

    // A grid manager bit grid's columns, and its bits row by row.
    fn bit_grid(
        doc: &anno_formats::isd::Doc,
        layer: anno_formats::isd::NodeId,
    ) -> (usize, Vec<bool>) {
        use anno_formats::isd::Value;
        let grid = doc.child(layer, "BitGrid").unwrap();
        let number = |name: &str| match doc.value(doc.child(grid, name).unwrap()) {
            Value::Text(t) => std::str::from_utf8(t).unwrap().parse::<usize>().unwrap(),
            _ => panic!("{name}"),
        };
        let (cols, rows, ints) = (number("m_XSize"), number("m_YSize"), number("m_IntXSize"));
        let Value::Blob(bytes) = doc.value(doc.child(grid, "m_BitGrid").unwrap()) else {
            panic!("no bits");
        };
        let bits = (0..cols * rows)
            .map(|n| {
                let (x, z) = (n % cols, n / cols);
                let word = (z * ints + x / 32) * 4;
                u32::from_le_bytes(bytes[word..word + 4].try_into().unwrap()) >> (x % 32) & 1 == 1
            })
            .collect();
        (cols, bits)
    }

    // The submarine rule, written out again: some cell within 4 m, as a
    // square, has corners averaging at or above -24 m.
    fn submarine_rule(corners: &anno_formats::isd::terrain::Corners, x: usize, z: usize) -> bool {
        let (side, rows) = corners.size();
        let high = |x: usize, z: usize| {
            let c =
                [(x, z), (x + 1, z), (x, z + 1), (x + 1, z + 1)].map(|(x, z)| corners.get(x, z));
            c.iter().all(Option::is_some)
                && c.iter().map(|h| h.unwrap() as f64).sum::<f64>() / 4. >= -24.
        };
        (z.saturating_sub(4)..(z + 5).min(rows - 1))
            .any(|hz| (x.saturating_sub(4)..(x + 5).min(side - 1)).any(|hx| high(hx, hz)))
    }

    // Needs Anno 2070: the rules a sculpt applies, run over every shipped
    // island as loaded, reproduce the counts the plan rests on. `Flags`
    // bit 0 disagrees with "has heights, all above 0.25 m" on 1,290 chunks,
    // so a sculpt flips it only when the predicate changes. Gameplay heights
    // follow trunc(1024 × corner mean) but on 9,778 v2 cells, which carry.
    // The submarine blocker re-derives exactly on 224 of 380 islands, so it
    // is rebuilt only around a sculpt. Path blockers cover nearly all
    // ground above the buildable band and leave nearly all of the band
    // open, which is why a lift above the band blocks by default. The
    // counts are the Steam install's.
    #[test]
    fn the_sculpt_rules_rederive_the_shipped_grids() {
        use anno_formats::isd::terrain::Terrain;
        use anno_formats::isd::{Doc, Value};
        use anno_formats::vfs::FileId;

        let Some(dir) = anno_formats::vfs::install_dir(PROFILE.env_var) else {
            return;
        };
        let install = Install::open(&dir, ARCHIVE_FAMILIES).unwrap();
        let (mut flags_off, mut cells_off, mut exact, mut subs) = (0, 0, 0, 0);
        let (mut above, mut above_blocked, mut band, mut band_open) = (0u64, 0u64, 0u64, 0u64);
        for i in 0..install.file_count() {
            let id = FileId(i as u32);
            let path = install.file_path(id);
            if !path.ends_with(".isd") {
                continue;
            }
            let doc = Doc::parse(&install.read_file(id).unwrap()).unwrap();
            let terrain = Terrain::read(&doc, &path).unwrap();
            let stored = Chunks::read(&doc);
            let (w, h) = terrain.size();
            for k in 0..w * h {
                if let Some(heights) = stored.heights(k) {
                    let all = (0..heights.side * heights.side)
                        .all(|n| heights.at(n % heights.side, n / heights.side) > 0.25);
                    flags_off += (all != (stored.flags(k) & 1 == 1)) as usize;
                }
            }
            let corners = terrain.corners();
            let cols = w * 16;
            let grid = doc
                .child(doc.root("m_GOPManager").unwrap(), "m_GRIDManager")
                .unwrap();
            if let Some(layer) = doc.child(grid, "m_SubmarineBlockerLayer") {
                subs += 1;
                let (_, bits) = bit_grid(&doc, layer);
                exact += (0..cols * cols)
                    .all(|n| bits[n] == submarine_rule(&corners, n % cols, n / cols))
                    as usize;
            }
            let Some(v2) = doc.child(grid, "m_HeightMap_v2") else {
                continue;
            };
            let Value::Blob(stored) = doc.value(v2) else {
                panic!("{path}");
            };
            let stored: Vec<i16> = stored
                .as_chunks::<2>()
                .0
                .iter()
                .map(|&v| i16::from_le_bytes(v))
                .collect();
            for n in 0..cols * cols {
                if let Some(v) = gameplay_at(&corners, n % cols, n / cols) {
                    cells_off += (v != stored[n]) as usize;
                }
            }
            let (pcols, path_bits) = bit_grid(&doc, doc.child(grid, "m_PathBlockerLayer").unwrap());
            for n in 0..cols * cols {
                let (x, z) = (n % cols, n / cols);
                let open = (0..4).all(|q| !path_bits[(z * 2 + q / 2) * pcols + x * 2 + q % 2]);
                match stored[n] {
                    v if v > 768 => {
                        above += 1;
                        above_blocked += !open as u64;
                    }
                    676..=768 => {
                        band += 1;
                        band_open += open as u64;
                    }
                    _ => {}
                }
            }
        }
        assert_eq!((flags_off, cells_off), (1_290, 9_778));
        assert_eq!((exact, subs), (224, 380));
        // On v2 islands: 99.3% and 97.5%.
        assert_eq!((above_blocked, above), (874_720, 880_997));
        assert_eq!((band_open, band), (1_769_469, 1_814_134));
    }

    // Needs Anno 2070: every chunk stored at 0.5 m or 0.25 m takes a 1 m
    // lift of its middle corner and keeps its native samples. Inside the
    // 2×2 m footprint around the corner a sample rises by the tent
    // (1 - |dx|)(1 - |dz|), the corner by exactly 1 m; every other sample of
    // the island keeps its bits. The chunks still agree with their
    // neighbours, which reading the result checks. The counts are the
    // Steam install's.
    #[test]
    fn every_fine_chunk_takes_a_sculpt_inside_its_footprint() {
        use anno_formats::isd::Doc;
        use anno_formats::isd::terrain::Terrain;
        use anno_formats::vfs::FileId;

        let Some(dir) = anno_formats::vfs::install_dir(PROFILE.env_var) else {
            return;
        };
        let install = Install::open(&dir, ARCHIVE_FAMILIES).unwrap();
        let (mut files, mut fine, mut inside) = (0, 0, 0u64);
        for i in 0..install.file_count() {
            let id = FileId(i as u32);
            let path = install.file_path(id);
            if !path.ends_with(".isd") {
                continue;
            }
            let source = install.read_file(id).unwrap();
            let loaded = Doc::parse(&source).unwrap();
            let before = Terrain::read(&loaded, &path).unwrap();
            let stored = Chunks::read(&loaded);
            let (w, h) = before.size();
            let chunks: Vec<usize> = (0..w * h)
                .filter(|&k| stored.heights(k).is_some_and(|h| h.side > 17))
                .collect();
            if chunks.is_empty() {
                continue;
            }
            files += 1;
            fine += chunks.len();
            let mut corners = before.corners();
            for &k in &chunks {
                let (x, z) = (k % w * 16 + 8, k / w * 16 + 8);
                corners.set(x, z, corners.get(x, z).unwrap() + 1.);
            }
            let mut doc = Doc::parse(&source).unwrap();
            corners.write(&mut doc, &path, SCULPT).unwrap();
            let back = Doc::parse(&doc.encode()).unwrap();
            Terrain::read(&back, &path).unwrap();
            let after = Chunks::read(&back);
            for k in 0..w * h {
                let (Some(was), Some(now)) = (stored.heights(k), after.heights(k)) else {
                    assert!(after.heights(k).is_none(), "{path}: chunk {k}");
                    continue;
                };
                assert_eq!(was.side, now.side, "{path}: chunk {k}");
                if !chunks.contains(&k) {
                    assert!(was.samples == now.samples, "{path}: chunk {k} changed");
                    continue;
                }
                let step = (was.side - 1) / 16;
                for n in 0..was.side * was.side {
                    let (sx, sz) = (n % was.side, n / was.side);
                    let (dx, dz) = (
                        (sx as f64 / step as f64 - 8.).abs(),
                        (sz as f64 / step as f64 - 8.).abs(),
                    );
                    let (a, b) = (was.at(sx, sz), now.at(sx, sz));
                    if dx >= 1. || dz >= 1. {
                        assert_eq!(a.to_bits(), b.to_bits(), "{path}: chunk {k} at {sx},{sz}");
                    } else if dx == 0. && dz == 0. {
                        assert_eq!(b, corners.get(k % w * 16 + 8, k / w * 16 + 8).unwrap());
                    } else {
                        let want = a as f64 + (1. - dx) * (1. - dz);
                        assert!((b as f64 - want).abs() < 1e-3, "{path}: chunk {k} {a} {b}");
                        inside += 1;
                    }
                }
            }
        }
        assert_eq!((files, fine), (143, 831));
        eprintln!("{inside} samples between corners moved");
    }

    // Needs Anno 2070: on every island with empty ocean chunks, a sculpt in
    // the first one and a dab of the editor's brush in the last give each
    // its records (17×17 heights, `Flags` 0x20000, paint from its base layer
    // up) and touch no other chunk; reading the result checks that the new
    // chunks agree with their neighbours. The counts are the Steam
    // install's.
    #[test]
    fn empty_ocean_chunks_take_a_first_sculpt_and_dab() {
        use anno_formats::isd::Doc;
        use anno_formats::isd::terrain::Terrain;
        use anno_formats::vfs::FileId;

        let Some(dir) = anno_formats::vfs::install_dir(PROFILE.env_var) else {
            return;
        };
        let install = Install::open(&dir, ARCHIVE_FAMILIES).unwrap();
        let registry = island::registry(&install).unwrap();
        let earth = 18;
        let (mut files, mut created) = (0, 0);
        for i in 0..install.file_count() {
            let id = FileId(i as u32);
            let path = install.file_path(id);
            if !path.ends_with(".isd") {
                continue;
            }
            let source = install.read_file(id).unwrap();
            let loaded = Doc::parse(&source).unwrap();
            let before = Terrain::read(&loaded, &path).unwrap();
            let stored = Chunks::read(&loaded);
            let (w, h) = before.size();
            let empty: Vec<usize> = (0..w * h)
                .filter(|&k| stored.heights(k).is_none())
                .collect();
            let (Some(&first), Some(&last)) = (empty.first(), empty.last()) else {
                continue;
            };
            files += 1;
            let mut corners = before.corners();
            let mut paint = anno_core::island::Layers::read(&before, registry.clone()).unwrap();
            let (x, z) = (first % w * 16 + 8, first / w * 16 + 8);
            corners.set(x, z, -20.);
            let (x, z) = (last % w * 16 + 8, last / w * 16 + 8);
            assert_eq!(paint.paint(x, z, earth, 1.), Ok(true), "{path}");
            let mut doc = Doc::parse(&source).unwrap();
            corners.write(&mut doc, &path, SCULPT).unwrap();
            island::write_paint(&mut doc, &path, &paint).unwrap();
            let back = Doc::parse(&doc.encode()).unwrap();
            let after = Terrain::read(&back, &path).unwrap();
            let written = Chunks::read(&back);
            for k in 0..w * h {
                if k == first || k == last {
                    let heights = written.heights(k).unwrap();
                    assert_eq!(heights.side, 17, "{path}");
                    assert_eq!(written.flags(k), 0x20000, "{path}: chunk {k}");
                    assert_eq!(after.ids(k)[0], 0, "{path}");
                    created += 1;
                    continue;
                }
                assert_eq!(written.flags(k), stored.flags(k), "{path}: chunk {k}");
                assert_eq!(after.ids(k), before.ids(k), "{path}: chunk {k}");
                assert_eq!(
                    written.heights(k).map(|h| h.samples),
                    stored.heights(k).map(|h| h.samples),
                    "{path}: chunk {k}"
                );
                for layer in 0..after.ids(k).len() {
                    assert!(after.plane(k, layer) == before.plane(k, layer), "{path}");
                }
            }
            assert_eq!(written.heights(first).unwrap().at(8, 8), -20., "{path}");
            let dabbed = after
                .ids(last)
                .iter()
                .position(|&id| id == u32::from(earth))
                .unwrap();
            assert_eq!(after.plane(last, dabbed)[8 * 17 + 8], 255, "{path}");
        }
        assert_eq!(files, 346);
        eprintln!("{created} chunks created");
    }

    // Needs Anno 2070: every island's build-blocker polygons read with the
    // shipped winding, and take a new square, a moved point and a removed
    // polygon; its movement grids take a 3 m disc of blocked cells. Read back from the
    // written bytes, the polygons and cells are the edited ones, and every
    // other top-level element is as loaded. The counts are the Steam
    // install's.
    #[test]
    fn every_island_takes_polygon_and_path_brush_edits() {
        use anno_formats::isd::Doc;
        use anno_formats::isd::grids::{BuildBlockers, PathBlockers, PathLayer};
        use anno_formats::vfs::FileId;

        let Some(dir) = anno_formats::vfs::install_dir(PROFILE.env_var) else {
            return;
        };
        let install = Install::open(&dir, ARCHIVE_FAMILIES).unwrap();
        let (mut files, mut shapes, mut brushed) = (0, 0, 0);
        for i in 0..install.file_count() {
            let id = FileId(i as u32);
            let path = install.file_path(id);
            if !path.ends_with(".isd") {
                continue;
            }
            files += 1;
            let source = install.read_file(id).unwrap();
            let loaded = Doc::parse(&source).unwrap();
            let mut blockers = BuildBlockers::read(&loaded, &path).unwrap();
            let mut paths = PathBlockers::read(&loaded, &path).unwrap();
            shapes += blockers.len();
            let (cols, _) = paths.size();
            let middle = cols as f64 / 4.;
            if !blockers.is_empty() {
                let last = blockers.len() - 1;
                blockers.remove(last);
            }
            let square =
                [[0., 0.], [2., 0.], [2., 2.], [0., 2.]].map(|p: [f64; 2]| p.map(|c| c + middle));
            blockers.add(&square).unwrap();
            // Pull the new square's corner out: it stays the right way round.
            let new = blockers.len() - 1;
            blockers.move_point(new, 2, [middle + 3., middle + 2.5]);
            // Every 0.5 m cell whose centre lies within 3 m of the middle.
            let (cols, rows) = paths.size();
            for n in 0..cols * rows {
                let (i, j) = (n % cols, n / cols);
                let (dx, dz) = (
                    (i as f64 + 0.5) / 2. - middle,
                    (j as f64 + 0.5) / 2. - middle,
                );
                if dx * dx + dz * dz <= 9. && !paths.get(PathLayer::Blocked, i, j) {
                    paths.set(PathLayer::Blocked, i, j, true);
                    brushed += 1;
                }
            }

            let mut doc = Doc::parse(&source).unwrap();
            blockers.write(&mut doc, &path).unwrap();
            paths.write(&mut doc, &path).unwrap();
            let written = doc.encode();
            let back = Doc::parse(&written).unwrap();
            assert!(back.encode() == written, "{path}");
            let (b2, p2) = (
                BuildBlockers::read(&back, &path).unwrap(),
                PathBlockers::read(&back, &path).unwrap(),
            );
            assert_eq!(b2.len(), blockers.len(), "{path}");
            for s in 0..b2.len() {
                assert!(b2.points(s).eq(blockers.points(s)), "{path}: shape {s}");
            }
            let (c, r) = p2.size();
            for layer in [PathLayer::Blocked, PathLayer::HighGround] {
                for n in 0..c * r {
                    assert_eq!(
                        p2.get(layer, n % c, n / c),
                        paths.get(layer, n % c, n / c),
                        "{path}"
                    );
                }
            }
            for (a, b) in loaded.roots().zip(back.roots()) {
                let name = loaded.name(a);
                if name != b"BuildBlockerShapes" && name != b"m_GOPManager" {
                    assert!(
                        loaded.value(a) == back.value(b) && name == back.name(b),
                        "{path}"
                    );
                }
            }
        }
        assert_eq!((files, shapes), (384, 1_525));
        eprintln!("{brushed} path cells blocked");
    }

    // Needs Anno 2070: a big-endian rock reads with its normals facing out
    // of its triangles, its box from its vertices (the file stores zeros)
    // and its texture coordinates inside the texture.
    #[test]
    fn big_endian_meshes_read_like_little_endian_ones() {
        let Some(dir) = anno_formats::vfs::install_dir(PROFILE.env_var) else {
            return;
        };
        let install = Install::open(&dir, ARCHIVE_FAMILIES).unwrap();
        let path =
            "data/graphics/landscape/stones/north/nature_stones/rdm/n_nature_big_stone_01_lod1.rdm";
        let bytes = install.read(path).unwrap();
        assert_eq!(&bytes[..4], b"RDM\0");
        let m = anno_formats::rdm::Rdm::parse(&bytes).unwrap();
        let facing = (m.indices.chunks(3))
            .filter(|t| {
                let [a, b, c] = [t[0], t[1], t[2]].map(|i| m.positions[i as usize]);
                let (u, v) = (
                    [0, 1, 2].map(|k| b[k] - a[k]),
                    [0, 1, 2].map(|k| c[k] - a[k]),
                );
                let face = [
                    u[1] * v[2] - u[2] * v[1],
                    u[2] * v[0] - u[0] * v[2],
                    u[0] * v[1] - u[1] * v[0],
                ];
                let n = m.normals[t[0] as usize];
                face[0] * n[0] + face[1] * n[1] + face[2] * n[2] > 0.
            })
            .count();
        assert!(facing * 100 > m.indices.len() / 3 * 95, "{facing}");
        // The stored box is the positions' own, unquantised.
        let lo = m
            .positions
            .iter()
            .fold([f32::INFINITY; 3], |b, p| [0, 1, 2].map(|k| b[k].min(p[k])));
        let hi = m.positions.iter().fold([f32::NEG_INFINITY; 3], |b, p| {
            [0, 1, 2].map(|k| b[k].max(p[k]))
        });
        assert_eq!(m.bounds, (lo, hi));
        assert!(m.bounds.1[1] - m.bounds.0[1] > 1.);
        let inside = m
            .uvs
            .iter()
            .filter(|uv| uv.iter().all(|c| (-0.01..=1.01).contains(c)));
        assert_eq!(inside.count(), m.uvs.len());
    }

    // Needs Anno 2070: a shipped building's cfg loads with every part's
    // finest mesh (2070's highest `_lod<N>`), its diffuse, normal and mask
    // textures shipped as DDS, its ground decal and its flame props.
    #[test]
    fn shipped_models_resolve_to_meshes_and_textures() {
        let Some(dir) = anno_formats::vfs::install_dir(PROFILE.env_var) else {
            return;
        };
        let install = Install::open(&dir, ARCHIVE_FAMILIES).unwrap();
        let cfg = "data/graphics/buildings/ecos/production/bakery_ecos/bakery_ecos.cfg";
        let model = anno_formats::model::Model::load(&install, cfg).unwrap();
        assert!(model.problems.is_empty(), "{:?}", model.problems);
        let [part] = &model.parts[..] else {
            panic!("{:?}", model.parts);
        };
        assert!(part.rdm.ends_with("bakery_ecos_lod2.rdm"), "{}", part.rdm);
        let finest = part.mesh.as_ref().unwrap();
        let coarsest = anno_formats::rdm::Rdm::parse(
            &install.read(&part.rdm.replace("_lod2", "_lod0")).unwrap(),
        )
        .unwrap();
        assert!(finest.indices.len() > coarsest.indices.len());
        let m = &part.materials[0];
        for texture in [&m.diffuse, &m.normal, &m.metallic] {
            let texture = texture.as_deref().unwrap();
            assert!(
                install.read(texture).unwrap().starts_with(b"DDS "),
                "{texture}"
            );
        }
        assert_eq!(model.decals.len(), 1);
        assert_eq!(model.decals[0].extents, [2., 1., 1.5]);
        assert_eq!(model.props.len(), 3);
        assert!(
            model
                .props
                .iter()
                .all(|p| p.cfg.ends_with("building_flame.cfg"))
        );
        assert_eq!(model.particles.len(), 2);
    }
}
