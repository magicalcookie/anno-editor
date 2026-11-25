// Anno 2070's asset database: `assets.xml`, the add-on's own assets file
// and the four DLC folders' (`data/config/dlc_0N/assets.xml`), the element
// paths it spells each field by, and its texts, which are `guid=text`
// lines keyed by the asset's own GUID. The editor reads it once per
// install and hands it to island opens (`Islands::read`).
//
// An island object's GUID names an asset whose `Object/Variations` list
// its models, picked by the object's `m_Variation`, as in the later games.
// The DLC assets are scenario buildings no shipped island places; their
// display names live in the DLCs' own texts.xml, which aren't read.

use std::cell::Cell;

use anno_formats::assets::{Assets, Field, Schema};
use anno_formats::vfs::Install;

pub static SCHEMA: Schema = Schema {
    assets_path: "data/config/game/assets.xml",
    // Later files win a GUID defined twice; no DLC redefines a shipped one.
    more_assets: &[
        "addondata/config/balancing/addon_01_assets.xml",
        "data/config/dlc_01/assets.xml",
        "data/config/dlc_02/assets.xml",
        "data/config/dlc_03/assets.xml",
        "data/config/dlc_04/assets.xml",
    ],
    // The add-on's language archives replace these files whole.
    texts_path: "data/loca/eng/txt/guids.txt",
    // No `<Template>` default 2070 reads (it has no editor-only meshes).
    templates_path: None,
    text_key: "",
    languages: &[
        ("czech", "data/loca/cze/txt/guids.txt"),
        ("english", "data/loca/eng/txt/guids.txt"),
        ("french", "data/loca/fra/txt/guids.txt"),
        ("german", "data/loca/ger/txt/guids.txt"),
        ("italian", "data/loca/ita/txt/guids.txt"),
        ("polish", "data/loca/pol/txt/guids.txt"),
        ("russian", "data/loca/rus/txt/guids.txt"),
        ("spanish", "data/loca/esp/txt/guids.txt"),
    ],
    fields: &[
        ("Template", Field::Template),
        ("Values/Standard/GUID", Field::GuidText),
        ("Values/Standard/Name", Field::Name),
        ("Values/Object/Variations/Item/Filename", Field::Cfg),
    ],
    // 2070 has none of the later games' slot work areas, fertility sets or
    // random slots; these names match nothing it ships.
    default_work_area_slot: "",
    kept: &[],
};

thread_local! {
    static PARSES: Cell<usize> = const { Cell::new(0) };
}

/// `install`'s database, read whole: 3.8 MB of XML, about a second in a
/// debug build. For an island open without the editor's (`island::read`).
pub fn load(install: &Install) -> anyhow::Result<Assets> {
    PARSES.with(|p| p.set(p.get() + 1));
    Assets::load(install, &SCHEMA)
}

/// How often this thread has read the database through [`load`]: an
/// island opened with the editor's database reads it no more.
pub fn parses() -> usize {
    PARSES.with(Cell::get)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn install() -> Option<Install> {
        let dir = anno_formats::vfs::install_dir(crate::PROFILE.env_var)?;
        Some(Install::open(&dir, crate::ARCHIVE_FAMILIES).unwrap())
    }

    // Needs Anno 2070: the database holds the DLC folders' assets too.
    // Every object n_m02 places resolves through the
    // database to a model the install holds; over every shipped island,
    // the share of objects that do stays at what was recorded. A tree
    // named in the base file and a street lamp only in the add-on's
    // both resolve, and the road takes its display name from `guids.txt`.
    #[test]
    fn shipped_island_objects_resolve_to_models() {
        let Some(install) = install() else {
            return;
        };
        let before = parses();
        let assets = load(&install).unwrap();
        assert_eq!(parses(), before + 1);
        // Recorded 2026-11-19 on the Steam install, the DLC folders' with.
        assert_eq!(assets.iter().count(), 1_359);
        // Each DLC folder's assets are in, by the first GUID its file
        // spells, found without the parser.
        for dlc in &SCHEMA.more_assets[1..] {
            let xml = String::from_utf8(install.read(dlc).unwrap()).unwrap();
            let guid: u32 = xml
                .split("<GUID>")
                .nth(1)
                .unwrap()
                .split('<')
                .next()
                .unwrap()
                .trim()
                .parse()
                .unwrap();
            assert!(assets.get(guid).is_some(), "{dlc}: {guid}");
        }
        let tree = assets.get(313).unwrap();
        assert_eq!(tree.name, "jungletree_medium");
        assert!(tree.cfgs.len() > 1, "{:?}", tree.cfgs);
        assert!(install.exists(&tree.cfgs[0]), "{}", tree.cfgs[0]);
        assert_eq!(assets.get(10).unwrap().text.as_deref(), Some("Road"));
        let lamp = assets.get(40_000_130).unwrap();
        assert_eq!(lamp.name, "science_line_streetlamp");
        assert!(install.exists(&lamp.cfgs[0]), "{}", lamp.cfgs[0]);

        let path = "data/levels/islands/normal/n_m02.isd";
        let doc = anno_formats::isd::Doc::parse(&install.read(path).unwrap()).unwrap();
        let objects = anno_formats::isd::objects::read(&doc, path).unwrap();
        assert_eq!(objects.len(), 5_368);
        let resolved = |o: &anno_formats::isd::objects::Object| {
            crate::objects::model(&assets, o).is_some_and(|c| install.exists(c))
        };
        let missing: Vec<u32> = (objects.iter())
            .filter(|o| !resolved(o))
            .map(|o| o.guid)
            .collect();
        assert!(missing.is_empty(), "n_m02: {missing:?}");

        let (mut all, mut drawn) = (0usize, 0usize);
        let mut unknown = std::collections::BTreeMap::<u32, usize>::new();
        for i in 0..install.file_count() {
            let id = anno_formats::vfs::FileId(i as u32);
            let path = install.file_path(id).to_owned();
            if !path.ends_with(".isd") {
                continue;
            }
            let doc = anno_formats::isd::Doc::parse(&install.read_file(id).unwrap()).unwrap();
            for o in anno_formats::isd::objects::read(&doc, &path).unwrap() {
                all += 1;
                if resolved(&o) {
                    drawn += 1;
                } else {
                    *unknown.entry(o.guid).or_default() += 1;
                }
            }
        }
        eprintln!("{drawn} of {all} objects resolve to models; the rest by GUID: {unknown:?}");
        // Recorded 2026-11-19 on the Steam install. The 87 left are test
        // assets without a model (`tree` 40, `Sebs tree` 307,
        // `dummy_stefan` 1006) and GUIDs no assets file defines (343, 348,
        // 10042, 10043, 10072, 20004).
        assert_eq!((drawn, all), (1_045_598, 1_045_685));
    }
}
