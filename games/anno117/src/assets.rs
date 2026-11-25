// Anno 117's asset database: where `assets.xml` and its texts live, and
// the element paths it spells each field by.

use anno_formats::assets::{Field, Schema};

pub static SCHEMA: Schema = Schema {
    assets_path: "data/base/config/export/assets.xml",
    more_assets: &[],
    texts_path: "data/base/config/gui/texts_english.xml",
    templates_path: Some("data/base/config/export/templates.xml"),
    text_key: "LineId",
    languages: &[
        ("brazilian", "data/base/config/gui/texts_brazilian.xml"),
        ("english", "data/base/config/gui/texts_english.xml"),
        ("french", "data/base/config/gui/texts_french.xml"),
        ("german", "data/base/config/gui/texts_german.xml"),
        ("italian", "data/base/config/gui/texts_italian.xml"),
        ("japanese", "data/base/config/gui/texts_japanese.xml"),
        ("korean", "data/base/config/gui/texts_korean.xml"),
        ("polish", "data/base/config/gui/texts_polish.xml"),
        ("russian", "data/base/config/gui/texts_russian.xml"),
        (
            "simplified_chinese",
            "data/base/config/gui/texts_simplified_chinese.xml",
        ),
        ("spanish", "data/base/config/gui/texts_spanish.xml"),
        (
            "traditional_chinese",
            "data/base/config/gui/texts_traditional_chinese.xml",
        ),
    ],
    fields: &[
        ("Template", Field::Template),
        ("BaseAssetGUID", Field::Base),
        ("Values/Standard/GUID", Field::Guid),
        ("Values/Standard/Name", Field::Name),
        ("Values/Standard/IconFilename", Field::Icon),
        ("Values/Object/Variations/Item/Filename", Field::Cfg),
        ("Values/Mesh/OnlyVisibleInEditor", Field::EditorOnly),
        // Names are ids into the texts file (`LineId`).
        ("Values/Text/OasisId", Field::TextId),
        ("Values/Fertility/NeedsSlot", Field::NeedsSlot),
        ("Values/Slot/SlotType", Field::SlotType),
        ("Values/Slot/WorkArea", Field::WorkArea),
        (
            "Values/Building/AssociatedRegions",
            Field::AssociatedRegions,
        ),
        (
            "Values/ResourceSetCondition/AllowedRegion",
            Field::AllowedRegion,
        ),
        (
            "Values/FertilitySet/Fertilities/Item/Fertility",
            Field::Fertility,
        ),
        // Sets list pools, which list the fertilities.
        (
            "Values/FertilityPool/FertilityList/Item/Fertility",
            Field::Fertility,
        ),
        ("Values/MapTemplate/TemplateFilename", Field::MapPath),
        ("Values/MapTemplate/TemplateRegion", Field::MapRegion),
        ("Values/MapTemplate/TemplateSize", Field::MapSize),
        ("Values/MapTemplate/IslandSize", Field::MapIslandSize),
        ("Values/RandomIsland/FilePath", Field::IslandPath),
        ("Values/RandomIsland/IslandRegion", Field::IslandRegion),
        ("Values/RandomIsland/IslandType", Field::IslandKind),
    ],
    // The `WorkArea` template's own `SlotType`. 117 work areas lay no path
    // models, so nothing reads it yet.
    default_work_area_slot: "WorkArea",
    // The preview's sample data reads these whole (`ui`).
    kept: &["MinimapScene", "GameSpeedFeature", "Season"],
};

#[cfg(test)]
mod tests {
    use super::*;
    use anno_formats::assets::Assets;
    use anno_formats::vfs::Install;

    #[test]
    fn loads_installed_assets() {
        let Some(dir) = anno_formats::vfs::install_dir("ANNO117_DIR") else {
            eprintln!("skip: ANNO117_DIR unset");
            return;
        };
        let install = Install::open(dir, crate::ARCHIVE_FAMILIES).unwrap();
        let t = std::time::Instant::now();
        let db = Assets::load(&install, &SCHEMA).unwrap();
        eprintln!("Assets::load: {} assets in {:?}", db.len(), t.elapsed());
        assert!(db.len() > 30_000);

        // Egypt's seasons raise its rivers by these (the season pool's
        // RiverHeight), which the water reads from here.
        assert_eq!(crate::water::river_heights(&db), Some([0.01, 0.5]));

        let tower = db.get(87583).unwrap();
        assert_eq!(tower.name, "Harbor Heartlands Pirate01 Platform Scorpios");
        assert_eq!(tower.template, "MilitaryTowerUnit");
        assert_eq!(tower.text.as_deref(), Some("Local Raider Defences"));
        assert_eq!(
            tower.cfgs,
            [
                "data/base/graphics/library/modular_level_kits/generic/stone_construction_v03/stone_construction_semicircular.cfg"
            ]
        );

        let kontor = db.get(3402).unwrap();
        assert_eq!(kontor.template, "HarborWarehouse");
        assert_eq!(kontor.text.as_deref(), Some("Trading Post"));
        assert!(kontor.cfgs[0].ends_with("kontor_01.cfg"));

        let iron = db.get(4049).unwrap();
        assert_eq!(iron.template, "Fertility");
        assert_eq!(iron.needs_slot.as_deref(), Some("Mountain"));
        let pool = db.get(31349).unwrap();
        assert_eq!(pool.name, "Fertility Pool Roman Iron Marble");
        assert_eq!(pool.fertilities, [4049, 4062]);

        let worker = db.get(28639).unwrap();
        assert_eq!(worker.template, "Inhabitant");
        assert_eq!(worker.cfgs.len(), 2);

        // Some cfgs are dead references (dev placeholders, cut quest
        // props); about 17% of 3.4k in this build.
        let all: Vec<&String> = db.iter().flat_map(|a| &a.cfgs).collect();
        let missing = all.iter().filter(|c| install.file(c).is_none()).count();
        assert!(
            missing * 4 < all.len(),
            "{missing} of {} cfgs missing",
            all.len()
        );
        assert!(install.file(&tower.cfgs[0]).is_some());
    }

    // Installs carry only the languages a player picked; every texts file
    // one has must be a language the schema lists, so a mod's texts patch
    // reaches it.
    #[test]
    fn installed_texts_are_listed_languages() {
        let Some(dir) = anno_formats::vfs::install_dir("ANNO117_DIR") else {
            eprintln!("skip: ANNO117_DIR unset");
            return;
        };
        let install = Install::open(dir, crate::ARCHIVE_FAMILIES).unwrap();
        let folder = SCHEMA.texts_path.rsplit_once('/').unwrap().0;
        let texts: Vec<String> = (install.list(folder).into_iter())
            .filter(|p| {
                let name = p.rsplit('/').next().unwrap();
                // 117 keeps its text ids' metadata beside the languages.
                name.starts_with("texts_") && name != "texts_metadata.xml"
            })
            .collect();
        assert!(texts.iter().any(|p| p == SCHEMA.texts_path));
        for path in &texts {
            assert!(SCHEMA.languages.iter().any(|(_, p)| p == path), "{path}");
        }
        assert_eq!(SCHEMA.texts("english"), Some(SCHEMA.texts_path));
    }
}
