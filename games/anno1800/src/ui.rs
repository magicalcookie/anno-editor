// Anno 1800's UI screens: where they and their textures live. Its assets
// give the preview no sample data, and the game view has no HUD for it.

use anno_core::ui::{Screens, Textures, UiScreens};
use anno_formats::phx::{DataFields, SourceDirs};

impl UiScreens for super::Anno1800 {
    fn screens(&self) -> &'static Screens {
        &SCREENS
    }
}

static SCREENS: Screens = Screens {
    generated: "data/ui/studio/generated/",
    images: "data/ui/studio/images/",
    // 117's rule, which the editor applied to both games before; 1800's
    // textures sit beside their sources (`data/ui/2kimages/...`), so the
    // swap finds nothing to swap there.
    textures: Textures {
        suffix: "_0.dds",
        swaps: &[("/fhd/", "/4k/")],
    },
    // TODO(alex): an 1800 HUD list and reference shot.
    hud: &[],
    hud_reference: None,
    infotip_area: None,
    drop_zone: None,
    // Only typed assets: the editor doesn't know 1800's whole schema.
    // The editor reads its install's sources where 117 keeps them; 1800
    // ships none there, so it has no enum entry names or widget types.
    sources: SourceDirs {
        enums: "data/ui/pwl/enums",
        templates: &[
            "data/ui/pwl/widgettypes",
            "data/ui/pwl/components",
            "data/ui/pwl/effects",
        ],
    },
    data_fields: &DATA_FIELDS,
    untyped_editable: false,
    // No asset of 1800's holds 117's minimap ring.
    sample_data: None,
    // The gold coin, shipped by 117 and 1800 alike at this path; player
    // colour layers in player 1's red.
    sample_icon: Some("data/ui/2kimages/main/icons/icon_resource_money_4.png"),
    sample_player_color: [140, 28, 36],
};

/// Anno 117's field tables: the two games share one id space (175 of the
/// 188 ids both exes name agree), and 117's named ids keep the ones 1800's
/// start-up rows add (games/anno117/examples/phx_named_ids.rs).
// TODO(alex): 1800's own property rows from Anno1800.exe; until then a
// field 117 dropped or renamed shows by id or under 117's name.
static DATA_FIELDS: DataFields = DataFields::new(
    include_str!("../../anno117/data/data_fields.tsv"),
    include_str!("../../anno117/data/named_ids.tsv"),
    &[0x20e0_47c1],
);
