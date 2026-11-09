// Owns which game is in use and every fact that differs between the games:
// one `Profile` per game, as plain data. Code that behaves differently per
// game matches on a profile field, not on `Game`.

use std::path::{Path, PathBuf};

use crate::vfs::discovery;

/// The game whose install and format rules are in use.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Game {
    #[serde(rename = "117")]
    Anno117,
    #[serde(rename = "1800")]
    Anno1800,
}

impl Game {
    pub const ALL: [Self; 2] = [Self::Anno1800, Self::Anno117];

    pub fn profile(self) -> &'static Profile {
        match self {
            Self::Anno117 => &ANNO117,
            Self::Anno1800 => &ANNO1800,
        }
    }

    /// Region names used by the game's assets, independent of DLC folder.
    pub fn island_region(self, path: &str) -> &'static str {
        let profile = self.profile();
        (profile.regions.iter())
            .find(|(marker, _)| path.contains(marker))
            .map_or(profile.default_region, |&(_, region)| region)
    }

    /// Identifies an installation by its executable and readable RDA header.
    pub fn detect(dir: &Path) -> Option<Self> {
        Self::ALL.into_iter().find(|game| {
            dir.join("Bin/Win64").join(game.profile().exe).is_file()
                && discovery::has_archives(&dir.join("maindata"))
        })
    }

    pub fn environment_dir(self) -> Option<PathBuf> {
        std::env::var_os(self.profile().env_var)
            .map(PathBuf::from)
            .filter(|dir| Self::detect(dir) == Some(self))
    }

    /// Every distinct install identified by local launcher metadata.
    pub fn installations(self) -> Vec<PathBuf> {
        discovery::installations(
            self,
            &crate::vfs::steam_roots(),
            &discovery::ubisoft_installs(),
        )
    }

    /// Environment override, else the first validated launcher installation.
    pub fn locate(self) -> Option<PathBuf> {
        self.environment_dir()
            .or_else(|| self.installations().into_iter().next())
    }
}

/// What one game is: its install, files, formats and which editor features
/// apply. Paths are virtual (inside the mounted archives) unless they say
/// otherwise.
pub struct Profile {
    pub title: &'static str,
    /// The number players call the game by, `117` or `1800`.
    pub short_name: &'static str,
    /// Install folder override.
    pub env_var: &'static str,
    /// The editor's config folder for this game.
    pub config_folder: &'static str,
    pub steam_app_id: &'static str,
    /// The install's folder under `steamapps/common`.
    pub steam_folder: &'static str,
    /// The game's folder under the user's Documents, which holds its
    /// config, logs and a `mods` folder the loader also reads.
    pub documents_folder: &'static str,
    /// The executable under `Bin/Win64` that identifies an install.
    pub exe: &'static str,
    /// mod.io's game id, from the loader's search paths (mod-loader.log).
    pub modio_game: &'static str,
    pub archive_order: ArchiveOrder,

    pub assets_path: &'static str,
    /// English texts; the other languages replace `english` in the name.
    pub texts_path: &'static str,
    pub prop_sets_path: &'static str,
    /// New islands go to `{0}{region}{1}/{name}/{name}.a7m`, next to the
    /// shipped ones of their region.
    pub island_folder: [&'static str; 2],
    /// Test maps go to `{0}{region}{1}/{name}/{name}.a7t` (and `.a7tinfo`,
    /// `.a7te`).
    pub custom_maps_folder: [&'static str; 2],

    /// `modinfo.json` `Anno`.
    pub modinfo_anno: u32,
    pub dependencies: Dependencies,
    /// `modinfo.json` has a `Difficulty` (see `modinfo::DIFFICULTIES`); the
    /// 117 loader refuses a mod without one.
    pub difficulty: bool,

    /// The element that keys a `<Text>` in `texts_<language>.xml`.
    pub text_id: &'static str,
    /// The shipped asset every pool island's `RandomIsland` entry is
    /// inserted after, so the ModOp has a stable anchor. 117's is a roman
    /// pool island.
    pub pool_anchor_guid: u32,
    /// Object templates that are only decoration: auto blockers cover them
    /// as they do rocks.
    pub decoration_templates: &'static [&'static str],

    /// `(path marker, region)`: an island path containing the marker is in
    /// that region, first match wins; others are in `default_region`.
    pub regions: &'static [(&'static str, &'static str)],
    pub default_region: &'static str,
    /// The map generator's island size classes, smallest first.
    pub size_classes: &'static [SizeClass],
    /// Ambience environment types in `EnvironmentGRid` value order, with the
    /// debug overlay colour (ARGB) from `assets.xml` `EnvironmentTypeMapping`.
    /// Index = stored value.
    pub environment_types: &'static [(&'static str, u32)],
    /// The types the landscape rules derive.
    pub environments: Environments,
    /// `(region, environment)` where island ground is not
    /// `environments.meadow`.
    pub land_environments: &'static [(&'static str, &'static str)],
    /// The tint level of `BaseTextureData`, the resident base texture. Both
    /// games stream 32 texels/m at level 0.
    pub tint_base_level: u32,
    /// Harvestable trees are the live ones under `vegetation/trees/` whose
    /// path contains none of these.
    pub tree_exclusions: &'static [&'static str],

    /// Shipped islands that can be a new island's skeleton are the
    /// `Normal` kind in `assets.xml`, so listing them reads the assets
    /// first. 117 lists every square island.
    pub islands_from_assets: bool,
    /// An island's `.a7minfo` is the placement header read with it,
    /// written back on every save. 117 writes a fresh one for new islands
    /// and leaves shipped ones as shipped.
    pub info_from_island: bool,
    /// Ground materials are `materialmaps/<level>/matid.dds` and
    /// `matalpha.dds` beside the island; 1800 embeds them in the island.
    pub material_files: bool,
    /// Ground material names are regional (`gras`, `snow_`) without 117's
    /// `grass_01`-style roles, so material picks fall back to looser names.
    pub regional_materials: bool,
    /// A new island takes its region's first named vegetation prop set
    /// rather than keeping the skeleton's.
    pub regional_prop_set: bool,
    /// The Trees scatter preset uses the harvestable tree rule. 117's preset
    /// skips only dead, stump, palm and log stems.
    pub harvestable_tree_preset: bool,
    pub ambients: Ambients,
    pub slots: ResourceSlots,
    /// A test map can fix an island's fertilities instead of rolling them.
    pub fertility_choice: bool,
    /// Pool entries state the island's size class
    /// (`RandomIsland/IslandPlaceholderSize`), and the inspector offers to
    /// override it. 1800's `RandomIsland` has no such field.
    pub placeholder_size: bool,
    pub test_maps: TestMaps,
    /// The game Import island reads islands from.
    pub import_from: Game,
    /// Add model takes a bare `.rdm` and dresses it in a shipped template
    /// cfg; 1800 props need their `.prp`.
    pub mesh_import: bool,
    /// The game whose `.prp` props this one takes as ported cfgs. Only
    /// props: 1800 cfgs (decoration objects) may use effects and shaders
    /// 117 lacks.
    pub ports_props_from: Option<Game>,
    /// A prop model's extension: what the Models page lists from
    /// dependency mods.
    pub prop_ext: &'static str,
    /// UI assets without type information are editable: the editor knows
    /// 117's whole Phoenix schema, 1800 assets only when typed.
    pub phx_untyped_editable: bool,
}

impl Profile {
    /// The size class land `extent` metres wide falls in: the first whose
    /// slots take it, else the largest.
    pub fn size_class(&self, extent: u32) -> &'static str {
        (self.size_classes.iter())
            .find(|c| extent <= c.slot)
            .or(self.size_classes.last())
            .map_or("", |c| c.name)
    }

    /// The narrowest land a start class takes: one metre past the slots of
    /// the class below the first start class.
    pub fn min_start_extent(&self) -> u32 {
        let first = self.size_classes.iter().position(|c| c.start).unwrap_or(0);
        first
            .checked_sub(1)
            .map_or(0, |below| self.size_classes[below].slot + 1)
    }

    /// The widest land any slot takes.
    pub fn max_slot(&self) -> u32 {
        self.size_classes.iter().map(|c| c.slot).max().unwrap_or(0)
    }

    /// The stored `EnvironmentGRid` value of a type; 0 (None) for names the
    /// game lacks.
    pub fn environment_value(&self, name: &str) -> u32 {
        (self.environment_types.iter())
            .position(|&(n, _)| n == name)
            .unwrap_or(0) as u32
    }

    /// The classes start islands come in, for sentences: "Large or XL".
    pub fn start_classes(&self) -> String {
        let names: Vec<&str> = (self.size_classes.iter())
            .filter(|c| c.start)
            .map(|c| c.name)
            .collect();
        names.join(" or ")
    }
}

/// One island size class of the map generator.
pub struct SizeClass {
    pub name: &'static str,
    /// The widest land, in metres, the class's slots take
    /// (`IslandSizes/*/MaxWidth`); also the placeholder footprint on the
    /// test map preview. Land wider than every slot is the largest class.
    pub slot: u32,
    /// Shipped start islands come in this class.
    pub start: bool,
}

/// The order `maindata/*.rda` mount in; later archives win.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArchiveOrder {
    /// By file name.
    Name,
    /// `data<N>.rda` by number, then the rest by name. 1800's file.db
    /// ArchiveFiles list runs data0..data33, then languages; lexical order
    /// would let data9 override data33's updated assets.
    Numbered,
}

/// Where `modinfo.json` lists the mods a mod needs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dependencies {
    /// `Dependencies.Require`.
    Require,
    /// `ModDependencies`.
    ModDependencies,
}

/// The `EnvironmentGRid` types the landscape rules derive, by name in
/// [`Profile::environment_types`]. 1800 has fewer kinds: several rules
/// share one.
pub struct Environments {
    pub coast_rock: &'static str,
    pub coast_sand: &'static str,
    pub forest: &'static str,
    pub marsh: &'static str,
    pub meadow: &'static str,
    pub mountain: &'static str,
    pub mountain_top: &'static str,
    pub sea: &'static str,
    pub sea_close: &'static str,
}

/// How the game's ambient presets (`config/engine/ambientsettings/`) are
/// named and lit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ambients {
    /// `amb_<region>_*.xml`, named by file, lit by constants the island
    /// preview reads.
    Files,
    /// Files under `local/` with a prefix per region, named by their
    /// `<Name>`, lit by time of day curves the preview does not read.
    Named,
}

/// What a resource slot on an island is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResourceSlots {
    /// Typed (Mountain, River, Marsh). Resources belong to the island, not
    /// the slot: a test map gives the island its deposits and keeps the
    /// slot active.
    Typed,
    /// A `Random` slot object whose asset lists the deposits that may
    /// replace it. A test map replaces it with the chosen one, or nothing.
    Random,
}

/// How Test in game puts the project's islands on a map.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TestMaps {
    /// A new "Editor islands" map type whose templates are copies of the
    /// province Default templates.
    MapType,
    /// The shipped map assets' templates redirected to copies, by the GUIDs
    /// of the assets replaced.
    Redirect,
}

static ANNO117: Profile = Profile {
    title: "Anno 117: Pax Romana",
    short_name: "117",
    env_var: "ANNO117_DIR",
    // The original config folder, so existing projects still open.
    config_folder: "anno-editor",
    steam_app_id: "3274580",
    steam_folder: "Anno 117 - Pax Romana",
    documents_folder: "Anno 117 - Pax Romana",
    exe: "Anno117.exe",
    modio_game: "11358",
    archive_order: ArchiveOrder::Name,

    assets_path: "data/base/config/export/assets.xml",
    texts_path: "data/base/config/gui/texts_english.xml",
    prop_sets_path: "data/base/config/engine/prop_sets.xml",
    island_folder: ["data/base/provinces/", "/islands/custom"],
    custom_maps_folder: ["data/base/provinces/", "/templates/custom"],

    modinfo_anno: 8,
    dependencies: Dependencies::Require,
    difficulty: true,

    text_id: "LineId",
    pool_anchor_guid: 42366,
    decoration_templates: &["TestData_Simple"],

    regions: &[("/celtic/", "Celtic")],
    default_region: "Roman",
    // `MapGenerator` asset 2002538. Every shipped island fits its class's
    // slot; shipped starters are all Large or XL.
    size_classes: &[
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
    ],
    environment_types: &ENVIRONMENT_TYPES_117,
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
    // 1 texel/m.
    tint_base_level: 5,
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

    islands_from_assets: false,
    info_from_island: false,
    material_files: true,
    regional_materials: false,
    regional_prop_set: false,
    harvestable_tree_preset: false,
    ambients: Ambients::Files,
    slots: ResourceSlots::Typed,
    fertility_choice: false,
    placeholder_size: true,
    test_maps: TestMaps::MapType,
    import_from: Game::Anno1800,
    mesh_import: true,
    ports_props_from: Some(Game::Anno1800),
    prop_ext: ".cfg",
    phx_untyped_editable: true,
};

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

static ANNO1800: Profile = Profile {
    title: "Anno 1800",
    short_name: "1800",
    env_var: "ANNO1800_DIR",
    config_folder: "anno-editor/1800",
    steam_app_id: "916440",
    steam_folder: "Anno 1800",
    documents_folder: "Anno 1800",
    exe: "Anno1800.exe",
    modio_game: "4169",
    archive_order: ArchiveOrder::Numbered,

    assets_path: "data/config/export/main/asset/assets.xml",
    texts_path: "data/config/gui/texts_english.xml",
    prop_sets_path: "data/config/engine/prop_sets/prop_sets.xml",
    island_folder: ["data/sessions/islands/custom/", ""],
    custom_maps_folder: ["data/sessions/maps/custom/", ""],

    modinfo_anno: 7,
    dependencies: Dependencies::ModDependencies,
    difficulty: false,

    text_id: "GUID",
    pool_anchor_guid: 140363,
    decoration_templates: &["TestData_Simple", "VisualObject"],

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
    size_classes: &[
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
    ],
    environment_types: &ENVIRONMENT_TYPES_1800,
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
    // 4 texels/m.
    tint_base_level: 3,
    tree_exclusions: &["dead", "stump", "/logs/"],

    islands_from_assets: true,
    info_from_island: true,
    material_files: false,
    regional_materials: true,
    regional_prop_set: true,
    harvestable_tree_preset: true,
    ambients: Ambients::Named,
    slots: ResourceSlots::Random,
    fertility_choice: true,
    placeholder_size: false,
    test_maps: TestMaps::Redirect,
    import_from: Game::Anno117,
    mesh_import: false,
    ports_props_from: None,
    prop_ext: ".prp",
    phx_untyped_editable: false,
};

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

    // The values the landscape rules store, as fitted on shipped islands. A
    // misspelt name would silently derive `None` cells.
    #[test]
    fn derived_environments_store_the_shipped_values() {
        let values = |game: Game| {
            let p = game.profile();
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
        let p = Game::Anno1800.profile();
        assert_eq!(p.environment_value(p.land_environments[0].1), 19);
    }

    #[test]
    fn size_classes_follow_the_slot_limits() {
        let p = Game::Anno117.profile();
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
        let p = Game::Anno1800.profile();
        let classes: Vec<_> = [192, 193, 272, 273, 900].map(|w| p.size_class(w)).into();
        assert_eq!(classes, ["Small", "Medium", "Medium", "Large", "Large"]);
        assert_eq!(p.start_classes(), "Large");
        assert_eq!((p.min_start_extent(), p.max_slot()), (273, 384));
    }

    #[test]
    fn island_regions_follow_path_markers() {
        let region = |game: Game, path| game.island_region(path);
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
}
