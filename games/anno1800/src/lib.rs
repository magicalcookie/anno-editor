// Anno 1800. Owns what the editor knows about this game: its `Profile`, its
// wording (`text`), its auto cliff pieces (`cliffs`), its harbour pieces
// and mine rails (`harbour`), its sky (`sky`) and sea (`sea`), read from
// its ambient presets, the water pass that draws the sea (`water`), and its
// prop converter (`model`).

mod assets;
mod cliffs;
mod files;
mod harbour;
mod model;
mod project;
mod sea;
mod sky;
mod text;
mod ui;
mod water;

use anno_core::ambient::Ambient;
use anno_core::game::{
    Dependencies, Environments, Game, GroundNames, IslandKinds, IslandRules, Islands, Module,
    Packaging, PlayerColor, Profile, Province, ResourceSlots, Role, SceneParts, SizeClass,
    UiScreens,
};
use anno_core::harbour::{Piece, RoadShape};
use anno_core::text::GameText;
use anno_formats::vfs::Install;

/// What the editor asks of this game.
pub static MODULE: Anno1800 = Anno1800;

pub struct Anno1800;

impl Module for Anno1800 {
    fn game(&self) -> Game {
        Game::Anno1800
    }
    fn profile(&self) -> &'static Profile {
        &PROFILE
    }
    fn text(&self) -> &'static GameText {
        &text::TEXT
    }
    // Steam's library hero image (1920×620), re-encoded at JPEG quality 82;
    // launcher cards are at most 280 px tall, 560 on a 2× display.
    fn card_art(&self) -> &'static [u8] {
        include_bytes!("../assets/card.jpg")
    }
    fn read_model(
        &self,
        install: &Install,
        source: &str,
    ) -> Option<anyhow::Result<anno_core::neutral::Model>> {
        Some(model::read(install, source))
    }
    fn write_model(
        &self,
        model: &anno_core::neutral::Model,
        from: &dyn anno_core::neutral::Source,
        to: &Install,
    ) -> Option<anyhow::Result<anno_core::neutral::Ported>> {
        Some(model::write(model, from, to))
    }
    // Industrial steel.
    fn accent(&self) -> [u32; 2] {
        [0xadc9d1, 0x3b6a7b]
    }
    fn project_roots(&self) -> &'static [&'static str] {
        &["data"]
    }
    fn files(&self) -> &'static [anno_core::files::Kind] {
        files::KINDS
    }
    // In the first player's colour, at the default preset's `GlowBrightness`.
    fn ports(&self) -> anno_core::files::Ports {
        anno_core::files::Ports {
            props_from: &[Game::Anno117, Game::Anno2070],
            prop_ext: ".prp",
            mesh_import: false,
        }
    }
    fn model_look(&self) -> anno_core::files::ModelLook {
        anno_core::files::ModelLook {
            dye: PLAYER_COLORS[0].dye(),
            glow: Ambient::default().glow,
            deferred_glow: false,
            window_interior: None,
            // Dye and night masks and window rooms (anno-render).
            surfaces: Some(anno_core::files::SurfacesOf(
                &anno_render::a7m_models::Models,
            )),
            player_colors: &PLAYER_COLORS,
            // Anno1800.exe, Steam build 16367353: the frame plan
            // (sub_141C389B0) draws Terrain, Decals before Streets,
            // Streets, Decals after Streets, then Models. Box decals move
            // 0.8 * cDepthBiasFactor along the line of sight
            // (decal_pbr.fx ApplyDepthBias); the factor is a per-frame
            // constant filed under first person tweaks (constants.fxh),
            // 1 outside it as far as the shaders show: its writer was not
            // found. Mesh decals move 0.01 the same way (model_decal.fx).
            decals: anno_core::files::DecalLook {
                box_bias: 0.8,
                grazing: false,
                box_on_ground: true,
                mesh_as_ground: false,
            },
        }
    }
    fn islands(&self) -> Option<&dyn Islands> {
        Some(self)
    }
    fn scene(&self) -> Option<&dyn SceneParts> {
        Some(self)
    }
    fn ui(&self) -> Option<&dyn UiScreens> {
        Some(self)
    }
    fn database(&self) -> Option<&'static anno_formats::assets::Schema> {
        Some(&assets::SCHEMA)
    }
    fn packaging(&self) -> Option<&'static Packaging> {
        Some(&project::PACKAGING)
    }
}
impl Islands for Anno1800 {
    fn rules(&self) -> &'static IslandRules {
        &RULES
    }
    fn codec(&self) -> anno_core::game::Codec<'_> {
        anno_core::game::Codec::A7m(&A7M)
    }
    fn maps(&self) -> Option<&dyn anno_core::map::Maps> {
        Some(&project::Redirect)
    }
    // Pool islands are the `Normal` ones in assets.xml, so the listing
    // reads the database when the caller has none.
    fn shipped(
        &self,
        install: &Install,
        assets: Option<&anno_formats::assets::Assets>,
    ) -> Vec<anno_core::game::ShippedIsland> {
        let loaded;
        let assets = match assets {
            Some(assets) => Some(assets),
            None => {
                loaded = anno_formats::assets::Assets::load(install, &assets::SCHEMA)
                    .map_err(|e| eprintln!("1800 islands listed without assets: {e:#}"))
                    .ok();
                loaded.as_ref()
            }
        };
        anno_core::game::a7m_islands(self, install, assets)
    }
    fn piece_of(&self, template: &str, name: &str) -> Option<Piece> {
        harbour::piece_of(template, name)
    }
    fn harbours(&self) -> Option<&dyn anno_core::game::Harbours> {
        Some(self)
    }
    // 1800's vegetation knows Grass and Bushes; 117's Soil, Marsh,
    // Bigbushes and other grass kinds become None.
    fn ground_type<'a>(&self, ground_type: &'a str) -> &'a str {
        match ground_type {
            "" => "",
            "Grass" | "GrassGreenHigher" | "GrassDry" => "Grass",
            "Bushes" => "Bushes",
            _ => "None",
        }
    }
}

impl anno_core::game::Harbours for Anno1800 {
    fn required_pieces(&self, region: &str) -> &'static [Piece] {
        harbour::required_pieces(region)
    }
    fn road_shape(&self, kind: Option<Piece>) -> RoadShape {
        harbour::road_shape(kind)
    }
}
impl SceneParts for Anno1800 {
    // 1800 ships no engine.ini and picks a preset from the hardware at
    // first start: Ultra High, View Distance 4 of 0 to 4, on an RTX 4080
    // SUPER.
    fn engine_settings(&self) -> Option<(&'static str, u32)> {
        Some(("data/config/engine/enginesettings/", 4))
    }
    fn ambient_names(&self, install: &Install, region: &str) -> Vec<String> {
        sky::ambient_names(install, region)
    }
    fn ambients(&self, install: &Install, names: &[&str]) -> Vec<(String, Ambient)> {
        sky::ambients(install, names)
    }
    fn water(&self, install: &Install) -> Option<std::sync::Arc<dyn anno_core::water::Water>> {
        Some(std::sync::Arc::new(water::Water::new(install)?))
    }
    // The sea (0.7 s cold, reading its waves first) beside the sky and the
    // placements' pipelines for the models' families (1.4 s cold, else
    // compiled when the first island's view built its device).
    fn warm(&self, install: &Install, gpu: &gpui_editor_kit::gpu::Warm<'_>) {
        std::thread::scope(|s| {
            s.spawn(|| sky::warm(gpu));
            s.spawn(|| gpu.families(&anno_render::a7m_models::families()));
            if let Some(water) = water::Water::new(install) {
                water.warm(gpu);
            }
        });
    }

    fn sky(&self, install: &Install) -> Option<std::sync::Arc<dyn anno_core::ambient::SkyPass>> {
        Some(std::sync::Arc::new(sky::SkyPasses::new(install)))
    }
}

/// The order Anno1800.exe mounts `maindata`: `data<N>.rda` by number, then
/// the rest by name. file.db's ArchiveFiles list runs data0..data33, then
/// the languages; lexical order would let data9 override data33's updated
/// assets.
pub(crate) const ARCHIVE_FAMILIES: &[&str] = &["data"];

static PROFILE: Profile = Profile {
    title: "Anno 1800",
    short_name: "1800",
    env_var: "ANNO1800_DIR",
    config_folder: "anno-editor/1800",
    steam_app_id: "916440",
    steam_folder: "Anno 1800",
    documents_folder: "Anno 1800",
    exe: "Bin/Win64/Anno1800.exe",
    modio_game: "4169",
    archive_families: ARCHIVE_FAMILIES,

    modinfo_anno: 7,
    dependencies: Dependencies::ModDependencies,
};

static A7M: anno_core::game::A7mRules = anno_core::game::A7mRules {
    ground_bake: anno_core::game::Bake::Plain,
    // 4 texels/m.
    tint_base_level: 3,
    islands_from_assets: true,
    info_from_island: true,
    material_files: false,
    // Absent `Config/Type/id` is 0.
    pool_types: anno_core::game::PoolTypes {
        normal: 0,
        starter: 1,
    },
    // The Old World set, which shipped islands elide.
    default_material_set: Some("data/config/engine/material_sets/terra_01.xml"),
};

pub(crate) const SIZE_CLASSES: &[SizeClass] = &[
    SizeClass {
        name: "Small",
        slot: 192,
        start: false,
    },
    SizeClass {
        name: "Medium",
        slot: 272,
        start: false,
    },
    SizeClass {
        name: "Large",
        slot: 384,
        start: true,
    },
];

static RULES: IslandRules = IslandRules {
    prop_sets_path: "data/config/engine/prop_sets/prop_sets.xml",
    island_folder: ["data/sessions/islands/custom/", ""],

    pool_anchor_guid: 140363,
    decoration_templates: &["TestData_Simple", "VisualObject"],
    pool_folder: "/pool/",
    large_islands: &["large", "_l_"],

    regions: &[
        ("colony01", "Colony01"),
        ("colony_01", "Colony01"),
        ("colony02", "Africa"),
        ("colony_02", "Africa"),
        ("/africa/", "Africa"),
        ("colony03", "Arctic"),
        ("colony_03", "Arctic"),
        ("/arctic/", "Arctic"),
    ],
    default_region: "Moderate",
    asset_regions: &[],
    provinces: &[
        Province {
            region: "Moderate",
            name: "Old World",
            prop_sets: "Moderate",
        },
        Province {
            region: "Colony01",
            name: "New World",
            prop_sets: "Colony",
        },
        Province {
            region: "Arctic",
            name: "Arctic",
            prop_sets: "Arctic",
        },
        Province {
            region: "Africa",
            name: "Enbesa",
            prop_sets: "Colony02",
        },
    ],
    // `IslandType`: a regular island, a player start, or both (every
    // shipped start island is both).
    roles: &[
        Role {
            label: "Regular",
            value: "Normal",
            regular: true,
            start: false,
        },
        // Parallel with its neighbours; "island" is said once after the
        // role where it is shown in a sentence.
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
    size_classes: SIZE_CLASSES,
    environment_types: &ENVIRONMENT_TYPES_1800,
    marsh_tile: 0,
    environments: Environments {
        coast_rock: "Coast_Cliff",
        coast_sand: "Coast_Sand",
        forest: "Forest",
        marsh: "Meadow",
        meadow: "Meadow",
        mountain: "Mountain",
        mountain_top: "Mountain",
        sea: "Ocean",
        sea_close: "Ocean",
    },
    land_environments: &[("Arctic", "Tundra")],
    tree_exclusions: &["dead", "stump", "/logs/"],

    regional_prop_set: true,
    harvestable_tree_preset: true,
    slots: ResourceSlots::Random,
    fertility_choice: true,
    placeholder_size: false,
    import_from: &[Game::Anno117, Game::Anno2070],
    // psBakeSplat: GetMaterialCoverage's bilinear coverage, BlendSplat's
    // plain cuts, layers blended as stored over the white the bake clears
    // to, which the terrain shader reads with GammaToLinear.
    // Slots ship at multiples of 90°.
    slot_yaw_step: std::f32::consts::FRAC_PI_2,
    cliffs: &[
        ("Moderate", &cliffs::MODERATE),
        ("Colony01", &cliffs::COLONY),
        ("Arctic", &cliffs::ARCTIC),
        ("Africa", &cliffs::AFRICA),
    ],
    // Top, curtain and foot emitters for each size.
    waterfall_spots: [
        &[102191, 102192, 102193],
        &[102194, 102195, 102196],
        &[102197, 102198, 102199],
    ],
    cliff_spot: 102416,
    // `StaticBlockingObject` "CampaignBuildBlocker". `EditorBlocking`
    // (92981, Voada Wall Blocking) is a destructible palisade, not a
    // footprint.
    build_blocker: 101128,
    // Materials carry a region prefix (`m_gras_02`, `c_gravle_01`) and fewer
    // roles than 117's, so World Machine channels take the nearest word,
    // and every channel finds some material. The leading names are those of
    // 117's that 1800's shipped sets also hold. Conversion keeps id order.
    ground_names: GroundNames {
        preferred: &[],
        channels: &[
            ("cliff", &["mountain", "rock", "stone"]),
            ("sharp cliff", &["mountain", "rock", "stone"]),
            ("coastal rock", &["rock", "stone", "mountain"]),
            ("gravel", &["gravle", "stone", "beach"]),
            (
                "seagrass",
                &["mud", "underwater", "snow", "stone", "rock", "beach"],
            ),
            ("beach & shelf", &["beach_01.", "beach", "snow", "stone"]),
            ("waterline", &["beach_02", "beach", "snow", "stone"]),
            ("shore sand", &["beach", "snow", "stone"]),
            ("river bed", &["gravle", "stone", "beach"]),
            (
                "river banks",
                &["mud_01.", "mud", "roots", "snow", "stone", "gras"],
            ),
            (
                "river shore",
                &["beach_05", "mud", "roots", "snow", "stone", "gras"],
            ),
            ("pebble beach", &["beach_03", "beach", "snow", "stone"]),
            ("dirt", &["mud", "roots", "snow", "stone", "gras"]),
            ("dry grass", &["grass", "gras", "snow", "mud"]),
            ("lush grass", &["grass", "gras", "snow", "mud"]),
            ("grass", &["grass", "gras", "snow", "mud"]),
            ("upland grass", &["grass", "gras", "snow", "mud"]),
            ("flowers", &["grass", "gras", "snow", "mud"]),
            (
                "marsh",
                &["mud_01.", "mud", "roots", "snow", "stone", "gras"],
            ),
            ("bare earth", &["mud", "roots", "snow", "stone", "gras"]),
            // World Machine's default export: R eroded ridges, G flat
            // rock, B scree, A the base cover.
            ("eroded ridges", &["cliff", "mountain"]),
            ("flat rock", &["rock", "stone"]),
            ("scree", &["gravel", "gravle", "beach"]),
            ("base cover", &["grass", "gras", "snow"]),
        ],
        // A new island starts on the set's first grass ("gras" names both
        // spellings), the arctic's on snow. No river bed roles: the River
        // tool says so and paints none.
        painted: &[("island base", &["gras", "snow_"])],
        prefixes: &["tropical", "colony02"],
    },
    region_words: &[
        ("moderate", &["moderate"]),
        ("colony01", &["jungle", "colony01", "south_america", "palm"]),
        ("arctic", &["arctic", "snow", "tundra"]),
        (
            "africa",
            &["africa", "desert", "enbesa", "colony02", "savanna"],
        ),
    ],
    // The `IslandType`s assets.xml ships. Story islands without an asset
    // (Crown Falls `moderate_c_01`, Manola `colony01_c_01`) match no name
    // and stay playable; decorations are named `colony03_d_01`.
    island_kinds: IslandKinds {
        asset_decoration: &["Decoration"],
        asset_other: &["ThirdParty", "PirateIsland"],
        asset_playable: &["Normal", "Starter"],
        other_names: &[
            "campaign",
            "3rdparty",
            "trader",
            "battlesite",
            "encounter",
            "strandedsailor",
            "storyisland",
            "settlement",
            "blocker",
            "scenario",
        ],
        decoration_names: &["deco", "_d_"],
    },
    // Names that keep their capital, and the ids' run-together words, from
    // the asset texts and model names the inspector shows. Docklands and
    // Botanica are DLC names asset texts carry.
    underwater_islands: false,
    // 117's islands land in the Old World.
    import_regions: &[
        ("roman", "moderate"),
        ("celtic", "moderate"),
        ("egypt", "moderate"),
        ("normal", "moderate"),
    ],
};

/// assets.xml's `ParticipantRepresentationFeature` `ParticipantColors`,
/// Color1 to Color16; the names are the editor's.
const PLAYER_COLORS: [PlayerColor; 16] = [
    PlayerColor {
        name: "Blue",
        rgb: [0x2d, 0x50, 0xc8],
    },
    PlayerColor {
        name: "Magenta",
        rgb: [0xcd, 0x0a, 0x87],
    },
    PlayerColor {
        name: "Yellow",
        rgb: [0xe1, 0xc8, 0x37],
    },
    PlayerColor {
        name: "Green",
        rgb: [0x00, 0x64, 0x00],
    },
    PlayerColor {
        name: "Cyan",
        rgb: [0x23, 0xcd, 0xf5],
    },
    PlayerColor {
        name: "Dark grey",
        rgb: [0x32, 0x32, 0x32],
    },
    PlayerColor {
        name: "Purple",
        rgb: [0x5f, 0x37, 0x82],
    },
    PlayerColor {
        name: "Teal",
        rgb: [0x00, 0x87, 0x87],
    },
    PlayerColor {
        name: "Red",
        rgb: [0xe1, 0x1e, 0x1e],
    },
    PlayerColor {
        name: "Brown",
        rgb: [0x8c, 0x4f, 0x20],
    },
    PlayerColor {
        name: "Grey",
        rgb: [0xaa, 0xaa, 0xaa],
    },
    PlayerColor {
        name: "Sea green",
        rgb: [0x48, 0x90, 0x65],
    },
    PlayerColor {
        name: "Wine",
        rgb: [0x73, 0x13, 0x36],
    },
    PlayerColor {
        name: "Orange",
        rgb: [0xf5, 0x7c, 0x00],
    },
    PlayerColor {
        name: "White",
        rgb: [0xff, 0xff, 0xff],
    },
    PlayerColor {
        name: "Pure red",
        rgb: [0xff, 0x00, 0x00],
    },
];

// Enum names from Anno1800.exe (build 16367353).
// Colors come from assets.xml's EnvironmentTypeMapping.
const ENVIRONMENT_TYPES_1800: [(&str, u32); 23] = [
    ("None", 0x00000000),
    ("Ocean", 0x5002e0d9),
    ("Coast_Sand", 0x50f5e102),
    ("Coast_Cliff", 0x50f00000),
    ("Meadow", 0x5083e305),
    ("Forest", 0x50009e10),
    ("Mountain", 0x5a16effa),
    ("Building_Residence", 0x464f4d4d),
    ("Building_Industry", 0x5027d9f5),
    ("Field", 0x50d7de10),
    ("Harbor", 0x50862ef2),
    ("Mountain_Side", 0x80f50796),
    ("Fog", 0x5a1a1818),
    ("ResidenceT1", 0x5a708c9c),
    ("ResidenceT2", 0x5a708c9c),
    ("ResidenceT3", 0x5a708c9c),
    ("ResidenceT4", 0x5a708c9c),
    ("ResidenceT5", 0x5a708c9c),
    ("Lake", 0x5f687ccc),
    ("Tundra", 0x8a51c3c9),
    ("MeadowIrrigated", 0xad308033),
    ("ResidenceScholars", 0x00000000),
    ("LakeCosmetic", 0x00000000),
];

#[cfg(test)]
mod tests {
    use super::*;
    use anno_core::game::IslandKind;

    #[test]
    fn island_kind_lists_1800_story_islands_as_playable() {
        let at =
            |dir: &str, name: &str| format!("data/{dir}/sessions/islands/pool/{name}/{name}.a7m");
        let kind = |dir, name, asset| MODULE.kind_of(&at(dir, name), asset);
        // No island asset: Crown Falls and Manola.
        assert_eq!(kind("dlc01", "moderate_c_01", None), IslandKind::Playable);
        assert_eq!(kind("dlc12", "colony01_c_01", None), IslandKind::Playable);
        assert_eq!(kind("dlc03", "colony03_d_04", None), IslandKind::Decoration);
        assert_eq!(
            kind("dlc01", "moderate_battlesite_01", None),
            IslandKind::Other
        );
        // Starter-only islands are start islands, still playable.
        let l01 = |asset| kind("base", "moderate_l_01", Some(asset));
        assert_eq!(l01("Starter"), IslandKind::Playable);
        assert_eq!(l01("Decoration"), IslandKind::Decoration);
        assert_eq!(l01("Normal;Starter"), IslandKind::Playable);
        assert_eq!(
            kind("base", "moderate_3rdparty03_01", Some("PirateIsland")),
            IslandKind::Other
        );
    }

    // 117 material sets brought into 1800 keep what 1800's vegetation knows
    // and drop the rest.
    #[test]
    fn ground_types_from_117_become_1800_ones() {
        let g = |t| MODULE.ground_type(t);
        assert_eq!(g("GrassDry"), "Grass");
        assert_eq!(g("Bushes"), "Bushes");
        assert_eq!(g("Soil"), "None");
        assert_eq!(g(""), "");
    }
}
