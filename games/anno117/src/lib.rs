// Anno 117: Pax Romana. Owns what the editor knows about this game: its
// `Profile`, its wording (`text`), its auto cliff pieces (`cliffs`), its
// ambient presets (`ambient`), its physical sky (`sky`), its sea (`sea`),
// the pass that draws its sea, rivers and lakes (`water`) and its model
// converter (`model`).

mod ambient;
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
use anno_formats::assets::Assets;
use anno_formats::vfs::Install;

/// What the editor asks of this game.
pub static MODULE: Anno117 = Anno117;

pub struct Anno117;

impl Module for Anno117 {
    fn game(&self) -> Game {
        Game::Anno117
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
    // Roman gold.
    fn accent(&self) -> [u32; 2] {
        [0xe0c49c, 0x94702f]
    }
    fn files(&self) -> &'static [anno_core::files::Kind] {
        files::KINDS
    }
    // In the first player's colour, at the default preset's `GlowBrightness`.
    fn ports(&self) -> anno_core::files::Ports {
        anno_core::files::Ports {
            props_from: &[Game::Anno1800, Game::Anno2070],
            prop_ext: ".cfg",
            mesh_import: true,
        }
    }
    fn model_look(&self) -> anno_core::files::ModelLook {
        anno_core::files::ModelLook {
            dye: PLAYER_COLORS[0].dye(),
            glow: Ambient::default().glow,
            deferred_glow: true,
            // The only wall texture under engine/interior/; which one the
            // engine binds is not traced.
            window_interior: Some(
                "data/base/graphics/engine/interior/window_interior_wall_diff_0.dds",
            ),
            // Dye and night masks and window rooms (anno-render).
            surfaces: Some(anno_core::files::SurfacesOf(
                &anno_render::a7m_models::Models,
            )),
            player_colors: &PLAYER_COLORS,
            // Anno117.exe build 26043928's frame plan (0x1422A9FA0) draws
            // Model Depth, Terrain, "Model before decals"
            // (RENDER_BEFORE_DECALS), Decals before Streets, Model Streets,
            // Decals after Streets, then Model and Model decals. The mesh
            // decals it flags draw with the ground (models.rs).
            // TODO(alex): the 26 flagged opaque materials (ShaderID 8)
            // should take box decals too; they draw with the models here.
            decals: anno_core::files::DecalLook {
                box_bias: 0.2,
                grazing: true,
                box_on_ground: true,
                mesh_as_ground: true,
            },
        }
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
    fn project_roots(&self) -> &'static [&'static str] {
        &["data"]
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
impl Islands for Anno117 {
    fn rules(&self) -> &'static IslandRules {
        &RULES
    }
    fn codec(&self) -> anno_core::game::Codec<'_> {
        anno_core::game::Codec::A7m(&A7M)
    }
    fn maps(&self) -> Option<&dyn anno_core::map::Maps> {
        Some(&project::MapType)
    }
    fn shipped(
        &self,
        install: &Install,
        assets: Option<&anno_formats::assets::Assets>,
    ) -> Vec<anno_core::game::ShippedIsland> {
        anno_core::game::a7m_islands(self, install, assets)
    }

    fn piece_of(&self, template: &str, name: &str) -> Option<Piece> {
        harbour::piece_of(template, name)
    }
    // 117 has every 1800 kind but calls Bigbushes Bushes.
    fn ground_type<'a>(&self, ground_type: &'a str) -> &'a str {
        match ground_type {
            "Bigbushes" => "Bushes",
            g => g,
        }
    }
    fn harbours(&self) -> Option<&dyn anno_core::game::Harbours> {
        Some(self)
    }
}

impl anno_core::game::Harbours for Anno117 {
    // Every province reads islands until it has every piece.
    fn required_pieces(&self, _region: &str) -> &'static [Piece] {
        &Piece::ALL
    }
    fn road_shape(&self, kind: Option<Piece>) -> RoadShape {
        harbour::road_shape(kind)
    }
}
impl SceneParts for Anno117 {
    fn rain(&self) -> bool {
        true
    }
    fn seasons(&self, path: &str, assets: Option<&Assets>) -> Option<[f32; 2]> {
        path.contains("/provinces/egypt/").then(|| {
            assets
                .and_then(water::river_heights)
                .unwrap_or(water::RIVER_HEIGHTS)
        })
    }
    fn volumetric_clouds(&self) -> bool {
        true
    }
    // The engine.ini the game ships starts at View Distance 3 of 0 to 4.
    fn engine_settings(&self) -> Option<(&'static str, u32)> {
        Some(("data/base/config/engine/enginesettings/", 3))
    }
    fn ambient_names(&self, install: &Install, region: &str) -> Vec<String> {
        ambient::names(install, region)
    }
    fn ambients(&self, install: &Install, names: &[&str]) -> Vec<(String, Ambient)> {
        ambient::ambients(install, names)
    }
    fn water(&self, install: &Install) -> Option<std::sync::Arc<dyn anno_core::water::Water>> {
        Some(std::sync::Arc::new(water::Water::new(install)))
    }
    fn sky(&self, install: &Install) -> Option<std::sync::Arc<dyn anno_core::ambient::SkyPass>> {
        Some(std::sync::Arc::new(sky::SkyPasses::new(install)))
    }
    // The sea, the sky and the placements' pipelines for the models'
    // families side by side: 0.9, 0.7 and 1.4 s cold. Unwarmed, the last
    // compiled when the first island's view built its device.
    fn warm(&self, _install: &Install, gpu: &gpui_editor_kit::gpu::Warm<'_>) {
        std::thread::scope(|s| {
            s.spawn(|| sky::warm(gpu));
            s.spawn(|| gpu.families(&anno_render::a7m_models::families()));
            water::warm(gpu);
        });
    }
    // tonemap.fxh Tonemap: FixLows lifts exposed values under
    // globals.xml's TonemapLowEdge along its TonemapLowPower (the same for
    // every preset), then Narkowicz's ACES fit of the value times 0.6.
    fn tone_curve(&self) -> gpui_editor_kit::gpu::Curve {
        gpui_editor_kit::gpu::Curve::Aces {
            lift_edge: 0.1,
            lift_power: 0.7,
            scale: 0.6,
        }
    }
    // eye_adaptation.fx: a 64-bin histogram of each frame's log luminance,
    // exposed toward a key (cExposureTarget: 0.18 times a blend of three
    // engine values by the ambient's state, Anno117.exe build 24988830
    // 0x1423AEF67), on top of the preset's Exposure; globals.xml's
    // EyeAdaptionMaxExposure caps it at 12. TODO: the histogram's range,
    // its filter, the blend's values, the lowest exposure, the tolerance
    // and the speeds are set in obfuscated code. These are estimates, the
    // key (0.32, the blend at 1.8) set so noon frames average 99 to 105
    // in sRGB, where retail screenshots average 86 to 114.
    fn auto_exposure(&self) -> Option<gpui_editor_kit::gpu::AutoExposure> {
        Some(gpui_editor_kit::gpu::AutoExposure {
            log_min: -14.,
            log_max: 6.,
            low: 0.3,
            count: 0.6,
            target: 0.32,
            min: 0.05,
            max: 12.,
            tolerance: 0.1,
            speed_up: 1.,
            speed_down: 1.,
        })
    }
}

/// `maindata`'s mount order. 117 ships no numbered archives, so every one
/// mounts by name; `data` first is 1800's rule and changes nothing here.
pub(crate) const ARCHIVE_FAMILIES: &[&str] = &["data"];

static PROFILE: Profile = Profile {
    title: "Anno 117: Pax Romana",
    short_name: "117",
    env_var: "ANNO117_DIR",
    // The original config folder, so existing projects still open.
    config_folder: "anno-editor",
    steam_app_id: "3274580",
    steam_folder: "Anno 117 - Pax Romana",
    documents_folder: "Anno 117 - Pax Romana",
    exe: "Bin/Win64/Anno117.exe",
    modio_game: "11358",
    archive_families: ARCHIVE_FAMILIES,

    modinfo_anno: 8,
    dependencies: Dependencies::Require,
};

static A7M: anno_core::game::A7mRules = anno_core::game::A7mRules {
    ground_bake: anno_core::game::Bake::Smooth,
    // 1 texel/m.
    tint_base_level: 5,
    islands_from_assets: false,
    info_from_island: false,
    material_files: true,
    // Absent `Config/Type/id` is 0.
    pool_types: anno_core::game::PoolTypes {
        normal: 0,
        starter: 1,
    },
    // Every shipped island names its set.
    default_material_set: None,
};

// `MapGenerator` asset 2002538. Every shipped island fits its class's
// slot; shipped starters are all Large or XL.
pub(crate) const SIZE_CLASSES: &[SizeClass] = &[
    SizeClass {
        name: "Small",
        slot: 256,
        start: false,
    },
    SizeClass {
        name: "Medium",
        slot: 320,
        start: false,
    },
    SizeClass {
        name: "Large",
        slot: 432,
        start: true,
    },
    SizeClass {
        name: "XL",
        slot: 432,
        start: true,
    },
];

static RULES: IslandRules = IslandRules {
    prop_sets_path: "data/base/config/engine/prop_sets.xml",
    island_folder: ["data/base/provinces/", "/islands/custom"],

    pool_anchor_guid: 42366,
    decoration_templates: &["TestData_Simple"],
    pool_folder: "/pool/",
    large_islands: &["large", "_l_"],

    regions: &[("/celtic/", "Celtic"), ("/egypt/", "Egypt")],
    default_region: "Roman",
    asset_regions: &[("Egypt", "Egyptian")],
    provinces: &[
        Province {
            region: "Roman",
            name: "Latium (Roman)",
            prop_sets: "ls_heartlands",
        },
        Province {
            region: "Celtic",
            name: "Albion (Celtic)",
            prop_sets: "ls_wetlands",
        },
        Province {
            region: "Egypt",
            name: "Aegyptus (Egyptian)",
            prop_sets: "ls_egypt",
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
    environment_types: &ENVIRONMENT_TYPES_117,
    // Celtic islands: all but 17 of 128k set cells; the rest hold 3.
    marsh_tile: 10,
    environments: Environments {
        coast_rock: "CoastRock",
        coast_sand: "CoastSand",
        forest: "Forest",
        marsh: "Marsh",
        meadow: "Meadow",
        mountain: "Mountain",
        mountain_top: "MountainTop",
        sea: "Sea",
        sea_close: "SeaClose",
    },
    land_environments: &[],
    // The ones shipped islands never put on a `TreeMap` cell: olives, palms,
    // the ornamental `tree_cypress_01_a/b/c` folders, dead trees and stumps.
    tree_exclusions: &[
        "dead",
        "olive",
        "palm",
        "juniper",
        "strawberry",
        "/tree_cypress_01_a/",
        "/tree_cypress_01_b/",
        "/tree_cypress_01_c/",
        "/tree_stonepine_02",
        "/tree_ash_01",
    ],

    regional_prop_set: false,
    harvestable_tree_preset: false,
    slots: ResourceSlots::Typed,
    fertility_choice: false,
    placeholder_size: true,
    import_from: &[Game::Anno1800, Game::Anno2070],
    // terrain_baking.fx psBakeSplatsBulk: bicubic coverage, cuts smoothed
    // (BlendSplatSmooth), layers sampled through sRGB views over linear
    // 0.2 grey.
    // Slots ship at multiples of 45°.
    slot_yaw_step: std::f32::consts::FRAC_PI_4,
    cliffs: &[
        ("Celtic", &cliffs::CELTIC),
        ("Egypt", &cliffs::EGYPT),
        ("", &cliffs::ROMAN),
    ],
    // Spot_Waterfall_Small, _Medium and _Big at the fall's foot.
    waterfall_spots: [&[69133], &[69134], &[69135]],
    cliff_spot: 102416,
    // `StaticBlockingObject` "CampaignBuildBlocker". `EditorBlocking`
    // (92981, Voada Wall Blocking) is a destructible palisade, not a
    // footprint.
    build_blocker: 101128,
    // Kinds: conversion's picks were the World Machine channels' first
    // choices. Channels: where retail islands put each material, measured
    // over the roman and celtic pools; the names are palette files, roman
    // then celtic. 1-16 are ground channels sharing one weightmap, 17-20
    // overlay patches.
    ground_names: GroundNames {
        preferred: &[
            (
                "rock",
                &["cliff_limestone", "celtic_cliffs_05", "rock_limestone"],
            ),
            ("sand", &["beach_04", "beach_01", "sand_01"]),
            ("gravel", &["gravel_01", "river_pebbles", "gravel_round"]),
            (
                "stony",
                &["pavement_02", "pavement_01", "square_stone_floor"],
            ),
            ("sea floor", &["underwatergrass", "beach_04"]),
            ("mud", &["dirt_01", "mud_01"]),
            ("grass", &["grass_01_e", "grass_01.", "grass_01_f_dark"]),
            ("dry grass", &["grass_01_h_dry", "grass_01_c_dry"]),
        ],
        channels: &[
            ("cliff", &["cliff_limestone", "celtic_cliffs_05", "cliff"]),
            (
                "sharp cliff",
                &[
                    "roman_cliff_sharp",
                    "celtic_cliffs_06",
                    "cliff_blocky",
                    "cliff",
                ],
            ),
            ("coastal rock", &["rock_limestone", "rocks_sharp", "rock"]),
            ("gravel", &["gravel_01", "gravel_round", "gravel"]),
            (
                "seagrass",
                &[
                    "underwatergrass",
                    "mud_01_darker.",
                    "mud_01_darker_dummy",
                    "mud",
                ],
            ),
            (
                "beach & shelf",
                &["beach_04", "beach_01.", "beach_01_dummy", "beach"],
            ),
            ("waterline", &["beach_02", "beach_01_b", "beach"]),
            ("shore sand", &["sand_01", "gravel_round", "sand", "beach"]),
            ("river bed", &["river_pebbles", "gravel_round", "gravel"]),
            (
                "river banks",
                &["mud_01.", "mud_01_dummy", "mud_01_darker_wet", "mud"],
            ),
            (
                "river shore",
                &["beach_05", "mud_01_darker.", "mud_01_darker_dummy", "mud"],
            ),
            ("pebble beach", &["beach_03", "gravel_round", "beach"]),
            ("dirt", &["dirt_01", "mud_01_darker_wet", "dirt", "mud"]),
            ("dry grass", &["grass_01_h_dry", "grass_01_c_dry", "grass"]),
            (
                "lush grass",
                &["grass_01_f_dark", "grass_01.", "grass_01_dummy", "grass"],
            ),
            (
                "grass",
                &["grass_01_e", "grass_01.", "grass_01_dummy", "grass"],
            ),
            (
                "upland grass",
                &["grass_01_g_dry", "grass_01_c_dry", "grass"],
            ),
            (
                "flowers",
                &["grass_01_d_lavender", "grass_01_b_light", "grass"],
            ),
            ("marsh", &["marshfloor", "mud_01.", "mud_01_dummy", "mud"]),
            (
                "bare earth",
                &["dirt_01", "mud_01_darker.", "mud_01_darker_dummy", "mud"],
            ),
            // World Machine's default export: R eroded ridges, G flat
            // rock, B scree, A the base cover.
            (
                "eroded ridges",
                &["cliff_limestone", "cliff_blocky", "cliff_sharp", "cliff"],
            ),
            (
                "flat rock",
                &["layeredstone", "granite", "rocks_sharp", "rock", "stone"],
            ),
            ("scree", &["gravel", "beach"]),
            (
                "base cover",
                &["grass_01_dummy", "grass_01.", "grass_01", "grass"],
            ),
        ],
        // Rivers: where shipped beds put each material, measured on five
        // roman pool islands (river/bed.rs); celtic sets take the nearest.
        // Lake shores take the bed's sand. A new island starts on plain
        // grass: id 0 is beach on celtic sets and mud on roman ones, which
        // read as snow and dirt in game.
        painted: &[
            (
                "bed sand",
                &[
                    "beach_04",
                    "beach_01",
                    "gravel_round",
                    "beach",
                    "gravel",
                    "mud",
                ],
            ),
            ("bed edge", &["beach_05", "beach_02", "beach_03"]),
            ("bed pebbles", &["river_pebbles", "gravel_round", "gravel"]),
            ("bed rock", &["cliff_limestone", "cliff"]),
            ("island base", &["grass_01"]),
        ],
        prefixes: &[],
    },
    region_words: &[
        ("roman", &["/roman/", "roman_"]),
        ("celtic", &["/celtic/", "celtic_"]),
        ("egypt", &["/egypt/", "egypt_"]),
    ],
    // The `IslandType`s assets.xml ships; volcanic islands are DLC 1's pool
    // islands. Names cover islands without an asset (campaign, deco).
    island_kinds: IslandKinds {
        asset_decoration: &["Decoration"],
        asset_other: &["ThirdParty", "PirateIsland"],
        asset_playable: &["Normal", "Starter", "VolcanicIsland"],
        other_names: &["campaign", "pirate", "3rdparty", "trader"],
        decoration_names: &["deco"],
    },
    // Names that keep their capital, and the ids' run-together words, from
    // the asset texts and model names the inspector shows.
    underwater_islands: false,
    // Albion's temperate north for the Old World and the Arctic, Latium
    // for the warm ones, Aegyptus for Africa's desert.
    import_regions: &[
        ("moderate", "celtic"),
        ("arctic", "celtic"),
        ("colony01", "roman"),
        ("africa", "egypt"),
        // 2070's surface islands are green and temperate.
        ("normal", "celtic"),
    ],
};

/// assets.xml's `ParticipantColor`s with `IsPlayerColor`, their
/// `ParticipantInGameColor` (colour mode None), in file order: 91304 to
/// 91308 and 91312. The Hall of Fame colours unlocked later are left out.
const PLAYER_COLORS: [PlayerColor; 6] = [
    PlayerColor {
        name: "Lapis Lazuli",
        rgb: [0x39, 0x56, 0x73],
    },
    PlayerColor {
        name: "Purpura",
        rgb: [0x6d, 0x49, 0x7a],
    },
    PlayerColor {
        name: "Saffron",
        rgb: [0x99, 0x81, 0x3a],
    },
    PlayerColor {
        name: "Celtic Green",
        rgb: [0x20, 0x66, 0x32],
    },
    PlayerColor {
        name: "Delphin",
        rgb: [0x50, 0x93, 0x94],
    },
    PlayerColor {
        name: "Imperial Red",
        rgb: [0x7f, 0x33, 0x33],
    },
];

/// The order is `EnvironmentTypePriorities`.
const ENVIRONMENT_TYPES_117: [(&str, u32); 38] = [
    ("None", 0xc826_2626),
    ("CoastRock", 0x50f0_0000),
    ("CoastSand", 0x50f5_e102),
    ("FieldGeneric", 0x50d7_de10),
    ("Forest", 0x5000_9e10),
    ("Lake", 0x5f68_7ccc),
    ("LakeShore", 0x5f68_7ccc),
    ("Marsh", 0xbf2a_4a00),
    ("Meadow", 0x5083_e305),
    ("Mountain", 0x80f5_0796),
    ("MountainTop", 0x7df2_f2f2),
    ("ResidenceRomanT1", 0xa987_2600),
    ("ResidenceRomanT2", 0x98ed_8d3e),
    ("ResidenceRomanT3", 0x82ff_ff00),
    ("ResidenceRomanT4", 0x8da5_89c7),
    ("ResidenceCelticT1", 0xff87_2600),
    ("ResidenceCelticT2", 0xffee_8d3d),
    ("ResidenceCelticT3", 0xffff_ff00),
    ("ResidenceCelticRomanizedT2", 0xffcb_afed),
    ("ResidenceCelticRomanizedT3", 0xff53_3775),
    ("Sea", 0x5002_75e0),
    ("SeaClose", 0x5002_e0d9),
    ("BuildingResidence", 0x464f_4d4d),
    ("BuildingIndustry", 0x5027_d9f5),
    ("Harbor", 0x5086_2ef2),
    ("Fog", 0x5a1a_1818),
    ("IncidentFire", 0xffff_0000),
    ("IncidentIllness", 0xff00_8000),
    ("IncidentRiot", 0xffff_ff00),
    ("RiverCrossing", 0x7d80_0080),
    // No overlay colour in assets.xml for the rest.
    ("Embankment", 0),
    ("Desert", 0),
    ("SeaCalm", 0),
    ("ResidenceEgyptT1", 0),
    ("ResidenceEgyptT2", 0),
    ("ResidenceEgyptT3", 0),
    ("ResidenceEgyptRomanizedT2", 0),
    ("ResidenceEgyptRomanizedT3", 0),
];

#[cfg(test)]
mod tests {
    use super::*;
    use anno_core::game::IslandKind;

    #[test]
    fn island_kind_reads_the_shipped_file_names() {
        let at = |name: &str| format!("data/base/provinces/celtic/islands/{name}.a7m");
        let kind = |name| MODULE.kind_of(&at(name), None);
        assert_eq!(kind("celtic_island_medium_01"), IslandKind::Playable);
        assert_eq!(kind("celtic_island_deco_08"), IslandKind::Decoration);
        // Campaign wins over deco: a campaign decoration is not a free one.
        assert_eq!(kind("celtic_island_campaign_deco_01"), IslandKind::Other);
        assert_eq!(kind("celtic_island_3rdparty_trader_01"), IslandKind::Other);
        // DLC 1's volcanic pool islands are playable by their asset.
        let volcanic = "data/dlc01/provinces/roman/islands/pool/roman_dlc01_island_medium_01/roman_dlc01_island_medium_01.a7m";
        assert_eq!(
            MODULE.kind_of(volcanic, Some("VolcanicIsland")),
            IslandKind::Playable
        );
    }

    #[test]
    fn ground_types_from_1800_keep_their_names() {
        assert_eq!(MODULE.ground_type("Bigbushes"), "Bushes");
        assert_eq!(MODULE.ground_type("Grass"), "Grass");
    }
}
