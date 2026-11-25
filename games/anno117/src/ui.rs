// Anno 117's UI screens: where they and their textures live, the HUD the
// game view stacks, and the minimap ring its assets give the preview.

use anno_core::ui::{SampleData, Screens, Textures, UiScreens};
use anno_formats::assets::Assets;
use anno_formats::phx::{DataFields, DefaultValue, SourceDirs};
use std::sync::Arc;

impl UiScreens for super::Anno117 {
    fn screens(&self) -> &'static Screens {
        &SCREENS
    }
}

static SCREENS: Screens = Screens {
    generated: "data/ui/studio/generated/",
    images: "data/ui/studio/images/",
    textures: Textures {
        suffix: "_0.dds",
        // Assets are authored against data/ui/fhd/; the game ships only the
        // 4k set (twice the pixels, same UV rects).
        swaps: &[("/fhd/", "/4k/")],
    },
    // Screens on top of the island in the reference shot. Their contents
    // that game code spawns (resource and construction bar items,
    // notifications) are not in the files and stay empty. QuickNavigation
    // (the province overlay) and QuickTools (the gamepad radial) are not
    // open there.
    hud: &[
        "hudgovernorsbarscene",
        "resourcebarscene",
        "questtrackerscene",
        // The left button column (the archives' button panel).
        "hudarchivesscene",
        "minimapscene",
        "constructionbarscene",
        // The bottom bar: category tabs, building buttons and tools.
        "constructionmenuscene",
    ],
    // HUD only, in the source tree's docs/ beside the repository.
    hud_reference: Some(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../../docs/reference/ingame-hud-1920x1080.png"
    )),
    // Neither ships in 1800.
    infotip_area: Some("41f69505-a029-4bdb-bd17-6ab85420c93f"),
    // TODO(alex): which native call toggles them is not read from the exe.
    drop_zone: Some("7831a47e-2d9c-4945-afda-0ab73f55074b"),
    sources: SOURCES,
    data_fields: &DATA_FIELDS,
    untyped_editable: true,
    sample_data: Some(Minimap::read),
    // The gold coin, shipped by 117 and 1800 alike at this path; player
    // colour layers in player 1's red.
    sample_icon: Some("data/ui/2kimages/main/icons/icon_resource_money_4.png"),
    sample_player_color: [140, 28, 36],
    // 305 of the 310 shipped list templates stamp one child per element.
};

/// The Phoenix sources shipped beside the compiled UI. 1800's install
/// has none of these folders.
const SOURCES: SourceDirs = SourceDirs {
    enums: "data/ui/pwl/enums",
    templates: &[
        "data/ui/pwl/widgettypes",
        "data/ui/pwl/components",
        "data/ui/pwl/effects",
    ],
};

/// Anno117.exe's game-object fields (data/data_fields.tsv; kinds from the
/// shipped Lua stubs) and the other ids its screens use (data/named_ids.tsv,
/// from examples/phx_named_ids.rs).
static DATA_FIELDS: DataFields = DataFields::new(
    include_str!("../data/data_fields.tsv"),
    include_str!("../data/named_ids.tsv"),
    // Indentation is Boolean in the stubs, and the screens pass it on into
    // Float properties 15 times.
    &[0x20e0_47c1],
);

/// The minimap ring's entries from the `MinimapScene` and
/// `GameSpeedFeature` assets (kept whole by the schema).
struct Minimap {
    /// The ring list (`RadialButtonsBase`) item by item, in its groups'
    /// order (the wheel group components on the list: DayAndNight,
    /// GameSpeed, Controls, Lodes, Fertilities): icon path and badge text.
    ring: Vec<(String, String)>,
    /// `RadialMinimapData.MapsCount` / `LodesCount`: where the Lodes and
    /// Fertilities groups begin.
    controls: usize,
    lodes: usize,
    /// Where the fertilities begin in `ring`.
    fertilities: usize,
}

impl Minimap {
    fn read(assets: &Assets) -> Option<Arc<dyn SampleData>> {
        let values = |template: &str| {
            let a = assets.iter().find(|a| a.template == template)?;
            Some(assets.values(a.guid))
        };
        let minimap = values("MinimapScene")?;
        let speeds = values("GameSpeedFeature")?;
        fn leaf<'a>(values: &'a [(String, String)], path: &str) -> Option<&'a str> {
            (values.iter())
                .find(|(p, _)| p == path)
                .map(|(_, t)| t.as_str())
        }
        // An Icon (or Fertility, Slot) asset's texture.
        let icon = |guid: &str| -> Option<String> {
            let icon = assets.get(guid.parse().ok()?)?.icon.as_ref()?;
            Some(icon.replace('\\', "/"))
        };
        let button = |name: &str| {
            icon(leaf(
                minimap,
                &format!("Values/MinimapScene/MinimapButtons/{name}/Icon"),
            )?)
        };
        let listed = |path: &str| -> Vec<String> {
            (minimap.iter())
                .filter(|(p, _)| p == path)
                .filter_map(|(_, g)| icon(g))
                .collect()
        };
        let none = String::new();
        let mut ring: Vec<(String, String)> = Vec::new();
        ring.extend(button("DayNight").map(|i| (i, none.clone())));
        // The game lays the speeds and controls out clockwise in this order
        // (the 2026-11-04 shots: fastest next to day and night, pause next
        // to the notes button).
        for speed in ["Fastest", "Fast", "Normal", "Paused"] {
            let path = format!("Values/GameSpeedFeature/GameSpeedVisualization/{speed}/Icon");
            ring.push((icon(leaf(speeds, &path)?)?, none.clone()));
        }
        // CoopPing only shows in a co-op game.
        let controls: Vec<String> = ["NotesTool", "QuickNavigation", "SwitchSession"]
            .iter()
            .filter_map(|b| button(b))
            .collect();
        let lodes = listed("Values/MinimapScene/LodesPerRegion/Roman/Order/Item/Slot");
        let fertilities = listed("Values/MinimapScene/FertilityOrder/Item/GUID");
        if lodes.is_empty() || fertilities.is_empty() {
            return None;
        }
        let (n_controls, n_lodes) = (controls.len(), lodes.len());
        let first_fertility = ring.len() + n_controls + n_lodes;
        ring.extend(controls.into_iter().map(|i| (i, none.clone())));
        // A lode shows how many deposits are left.
        ring.extend(lodes.into_iter().map(|i| (i, "5".to_owned())));
        // An island has a handful: the ring's Fertilities group keeps room
        // for 8 (its MinGroupItemCount).
        ring.extend(fertilities.into_iter().take(8).map(|i| (i, none.clone())));
        Some(Arc::new(Minimap {
            ring,
            controls: n_controls,
            lodes: n_lodes,
            fertilities: first_fertility,
        }))
    }
}

impl SampleData for Minimap {
    fn sample(&self, item: Option<(&str, usize)>, name: &str, last: &str) -> Option<DefaultValue> {
        match item {
            // How many items the list stamps (see `layout`'s sample count).
            None if name.ends_with("minimapradialwheel.radialbuttonsbase#len") => {
                Some(DefaultValue::S32(self.ring.len() as i32))
            }
            // Radial menus all name their list `RadialButtonsBase`; the
            // minimap's is under `MinimapRadialWheel`.
            Some((list, n)) if list.ends_with("MinimapRadialWheel.RadialButtonsBase") => {
                let (icon, badge) = self.ring.get(n)?;
                if last == "icon" {
                    Some(DefaultValue::Str(icon.clone()))
                } else if last == "text" && name.ends_with("textdata.text") {
                    Some(DefaultValue::Str(badge.clone()))
                } else if name.ends_with("states.context.index") {
                    // Context 1 picks the sector style without a button
                    // tile: fertilities are information, not buttons (they
                    // float on the band in the 2026-11-04 shots).
                    Some(DefaultValue::S32(i32::from(n >= self.fertilities)))
                } else {
                    None
                }
            }
            None if name.ends_with("radialminimapdata.mapscount") => {
                Some(DefaultValue::S32(self.controls as i32))
            }
            None if name.ends_with("radialminimapdata.lodescount") => {
                Some(DefaultValue::S32(self.lodes as i32))
            }
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anno_formats::vfs::Install;

    // Recorded at d51799a from the preview's old string scan of
    // assets.xml; read through `Assets` now.
    #[test]
    fn the_117_minimap_ring_comes_from_its_assets() {
        let Some(dir) = anno_formats::vfs::install_dir("ANNO117_DIR") else {
            eprintln!("skip: ANNO117_DIR unset");
            return;
        };
        let assets = Assets::load(
            &Install::open(dir, crate::ARCHIVE_FAMILIES).unwrap(),
            &crate::assets::SCHEMA,
        )
        .unwrap();
        let read = SCREENS.sample_data.unwrap();
        let ring = read(&assets).expect("the minimap ring");
        let ring = ring.sample(None, "x.minimapradialwheel.radialbuttonsbase#len", "");
        assert_eq!(ring, Some(DefaultValue::S32(18)));
        let minimap = Minimap::read(&assets).unwrap();
        let entry = |n: usize, last: &str| {
            minimap.sample(
                Some(("Hud.MinimapRadialWheel.RadialButtonsBase", n)),
                "x.textdata.text",
                last,
            )
        };
        let str = |s: &str| Some(DefaultValue::Str(s.into()));
        let base = "data/ui/fhd/base/icon_content";
        assert_eq!(
            entry(0, "icon"),
            str(&format!("{base}/hud_minimap/icon_2d_minimap_day_night.png"))
        );
        assert_eq!(
            entry(1, "icon"),
            str(&format!("{base}/generic/icon_2d_speed_veryfast.png"))
        );
        assert_eq!(
            entry(4, "icon"),
            str(&format!("{base}/generic/icon_2d_speed_pause.png"))
        );
        assert_eq!(
            entry(5, "icon"),
            str(&format!("{base}/generic/icon_2d_notes.png"))
        );
        assert_eq!(
            entry(7, "icon"),
            str(&format!("{base}/hud_minimap/icon_2d_minimap_world_map.png"))
        );
        assert_eq!(
            entry(8, "icon"),
            str(&format!(
                "{base}/infrastructure/icon_3d_resource_mountain.png"
            ))
        );
        assert_eq!(entry(8, "text"), str("5"));
        assert_eq!(
            entry(9, "icon"),
            str(&format!("{base}/infrastructure/icon_3d_resource_river.png"))
        );
        assert_eq!(
            entry(10, "icon"),
            str(&format!(
                "{base}/production_goods/icon_3d_mackerels_goods.png"
            ))
        );
        assert_eq!(
            entry(16, "icon"),
            str("data/ui/fhd/dlc03/icon_content/production_goods/icon_3d_sesame_goods.png")
        );
        assert_eq!(
            entry(17, "icon"),
            str(&format!("{base}/production_goods/icon_3d_olives_goods.png"))
        );
        let count = |name: &str| minimap.sample(None, name, "");
        assert_eq!(
            count("x.radialminimapdata.mapscount"),
            Some(DefaultValue::S32(3))
        );
        assert_eq!(
            count("x.radialminimapdata.lodescount"),
            Some(DefaultValue::S32(2))
        );
        let context = |n| {
            minimap.sample(
                Some(("MinimapRadialWheel.RadialButtonsBase", n)),
                "x.states.context.index",
                "index",
            )
        };
        assert_eq!(context(9), Some(DefaultValue::S32(0)));
        assert_eq!(context(10), Some(DefaultValue::S32(1)));
    }

    // Rows of data/*.tsv as the exe and the Lua stubs have them; parsing
    // checks that each list is sorted for the lookups' binary search.
    #[test]
    fn field_tables_read_known_rows() {
        let f = &DATA_FIELDS;
        assert_eq!(f.field(0x2001_e6c7), Some(("AlreadyAdded", 0)));
        assert_eq!(f.kind(0x2001_e6c7), Some(0));
        // An object field: the type column is empty.
        assert_eq!(f.field(0x20bd_1a05), Some(("DisplayedSize", u32::MAX)));
        assert_eq!(f.kind(0x20bd_1a05), None);
        // Indentation is typed, but the screens use it as a Float too.
        assert_eq!(f.field(0x20e0_47c1).map(|r| r.1), Some(0));
        assert_eq!(f.kind(0x20e0_47c1), None);
        // Click is an event row, past the property rows.
        assert_eq!(f.field(0x2003_2db3), None);
        assert_eq!(f.name(0x2003_2db3), Some("Click"));
        assert_eq!(f.component_property(0x7c10_1933), Some(("Tween", "Looped")));
        assert_eq!(f.name(0x1234_5678), None);
    }
}
