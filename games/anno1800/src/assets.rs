// Anno 1800's asset database: where `assets.xml` and its texts live, and
// the element paths it spells each field by.

use anno_formats::assets::{Field, Schema};

pub static SCHEMA: Schema = Schema {
    assets_path: "data/config/export/main/asset/assets.xml",
    more_assets: &[],
    texts_path: "data/config/gui/texts_english.xml",
    templates_path: Some("data/config/export/main/asset/templates.xml"),
    text_key: "GUID",
    // 1800's own names: `chinese` and `taiwanese`, not 117's
    // `simplified_chinese` and `traditional_chinese`, and a `portuguese`.
    languages: &[
        ("brazilian", "data/config/gui/texts_brazilian.xml"),
        ("chinese", "data/config/gui/texts_chinese.xml"),
        ("english", "data/config/gui/texts_english.xml"),
        ("french", "data/config/gui/texts_french.xml"),
        ("german", "data/config/gui/texts_german.xml"),
        ("italian", "data/config/gui/texts_italian.xml"),
        ("japanese", "data/config/gui/texts_japanese.xml"),
        ("korean", "data/config/gui/texts_korean.xml"),
        ("polish", "data/config/gui/texts_polish.xml"),
        ("portuguese", "data/config/gui/texts_portuguese.xml"),
        ("russian", "data/config/gui/texts_russian.xml"),
        ("spanish", "data/config/gui/texts_spanish.xml"),
        ("taiwanese", "data/config/gui/texts_taiwanese.xml"),
    ],
    fields: &[
        ("Template", Field::Template),
        ("BaseAssetGUID", Field::Base),
        ("Values/Standard/GUID", Field::Guid),
        ("Values/Standard/Name", Field::Name),
        ("Values/Standard/IconFilename", Field::Icon),
        ("Values/Object/Variations/Item/Filename", Field::Cfg),
        ("Values/Mesh/OnlyVisibleInEditor", Field::EditorOnly),
        // Names are written inline, so the texts file isn't read.
        ("Values/Text/LocaText/English/Text", Field::Text),
        ("Values/Slot/SlotType", Field::SlotType),
        ("Values/Slot/WorkArea", Field::WorkArea),
        ("Values/WorkAreaPath/AreaPathConfig", Field::AreaPath),
        (
            "Values/Building/AssociatedRegions",
            Field::AssociatedRegions,
        ),
        (
            "Values/RandomMapObject/Replacements/Item/NewAsset",
            Field::Replacement,
        ),
        (
            "Values/RandomMapObject/DLCSlotReplacements/Item/Replacement",
            Field::Replacement,
        ),
        (
            "Values/ResourceSetCondition/AllowedRegion",
            Field::AllowedRegion,
        ),
        (
            "Values/FertilitySet/Fertilities/Item/Fertility",
            Field::Fertility,
        ),
        ("Values/MapTemplate/TemplateFilename", Field::MapPath),
        ("Values/MapTemplate/TemplateRegion", Field::MapRegion),
        ("Values/MapTemplate/TemplateSize", Field::MapSize),
        ("Values/MapTemplate/IslandSize", Field::MapIslandSize),
        ("Values/MapTemplate/TemplateMapType", Field::MapType),
        ("Values/RandomIsland/FilePath", Field::IslandPath),
        ("Values/RandomIsland/IslandRegion", Field::IslandRegion),
        ("Values/RandomIsland/IslandType", Field::IslandKind),
    ],
    // The `WorkArea` and `WorkAreaRiverBuilding` templates' default.
    default_work_area_slot: "WorkAreaMountain",
    kept: &[],
};

#[cfg(test)]
mod tests {
    use super::*;
    use anno_formats::assets::Assets;
    use anno_formats::vfs::Install;

    // Needs Anno 1800: shipped slot work areas resolve to the models their
    // buildings lay. Mine slots get the rail on both work areas (coal's
    // rail beats cement's none on the cave one: two scenario coal mines and
    // two cement mines tie, the lower GUID is coal's); a picked Cement
    // resource lays none; river slots get the Water Pump's channel, which
    // three buildings share.
    #[test]
    fn installed_1800_work_area_paths() {
        let Some(dir) = anno_formats::vfs::install_dir("ANNO1800_DIR") else {
            eprintln!("skip: ANNO1800_DIR unset");
            return;
        };
        let assets = Assets::load(
            &Install::open(dir, crate::ARCHIVE_FAMILIES).unwrap(),
            &SCHEMA,
        )
        .unwrap();
        let rail = Some("data/graphics/buildings/production/mining_01/mining_rail_01.cfg");
        // random slot mining; slot mining workarea mountain / cave.
        assert_eq!(
            assets.work_area_path(1000029, 1000253, "Moderate", None),
            rail
        );
        assert_eq!(
            assets.work_area_path(1000029, 100419, "Moderate", None),
            rail
        );
        let cement = (assets.slot_replacements(1000029, "Moderate").into_iter())
            .find(|r| r.slot_type.as_deref() == Some("Cement"))
            .map(|r| r.guid);
        assert!(cement.is_some());
        assert_eq!(
            assets.work_area_path(1000029, 100419, "Moderate", cement),
            None
        );
        // Riverslot; Riverslot_workarea.
        assert_eq!(
            assets.work_area_path(117741, 117742, "Africa", None),
            Some(
                "data/dlc06/graphics/buildings/special/river_mine_colony_02/riverslot_waterpump_channel_01.cfg"
            )
        );
        // New World islands carry the Old World mine slot too; its
        // replacements are all tagged Moderate, which must not empty it.
        assert_eq!(
            assets.work_area_path(1000029, 1000253, "Colony01", None),
            rail
        );
        assert!(!assets.slot_buildings(1000029, "Colony01", None).is_empty());
        // random slot clay: the clay pit lays nothing.
        assert_eq!(
            assets.work_area_path(100417, 100419, "Moderate", None),
            None
        );
    }

    // Installs carry only the languages a player picked; every texts file
    // one has must be a language the schema lists, so a mod's texts patch
    // reaches it.
    #[test]
    fn installed_texts_are_listed_languages() {
        let Some(dir) = anno_formats::vfs::install_dir("ANNO1800_DIR") else {
            eprintln!("skip: ANNO1800_DIR unset");
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
