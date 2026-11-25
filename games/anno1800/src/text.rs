// Anno 1800’s own wording in the editor.
use anno_core::text::{GameText, Playtest};

pub static TEXT: GameText = GameText {
    subtitle: "The Industrial Revolution",
    era: "AN AGE OF INVENTION",
    regions: "Old World, New World, Arctic and Enbesa islands",
    add_model: "A .cfg or .prp with its meshes and textures",
    model_formats: "Model configurations (.cfg, .prp) with their meshes and textures",
    slot_resource: "The test map replaces this random slot with the chosen resource. Other random slots receive their first valid resource.",
    slot_roads: "Lays the selected slots' work areas and rails anew like shipped islands (mines: mountain 11 m up the slope, cave 5 m), adding missing ones. Drag road points to bend them; Ctrl-click adds another work area",
    playtest: Playtest {
        intro: "Redirects the chosen regions' map templates to test maps with your islands in fixed slots. Enable the relevant DLC when starting the game.",
        steps: "1. Install  2. Start a new Large world with the relevant DLC enabled, then enter your island’s region  3. Disable this test mod to restore the original maps.",
        in_game: "Start a new Large world with the relevant DLC enabled, then enter your island’s region",
    },
    playtest_old_world: Some(Playtest {
        intro: "Redirects the chosen regions' map templates to test maps with your islands in fixed slots. In the Old World, Archipelago is labelled Editor islands while this mod is active.",
        steps: "1. Install  2. Start a new game with a Large world and map type Editor islands (Archipelago)  3. Disable this test mod to restore the original maps.",
        in_game: "New game → map type Editor islands, Large world",
    }),
    asset_words: anno_core::game::AssetWords {
        proper: &[
            "Enbesa",
            "Africa",
            "African",
            "Arctic",
            "Docklands",
            "Botanica",
        ],
        joined: &[
            ("3rdparty", "third-party"),
            ("questobject", "quest object"),
            ("questgiver", "quest giver"),
            ("groundplate", "ground plate"),
            ("nogroundplate", "no ground plate"),
            ("multifactory", "multi factory"),
            ("riverslot", "river slot"),
            ("hedgesystem", "hedge system"),
            ("wallsystem", "wall system"),
            ("waterpump", "water pump"),
            ("lightpost", "light post"),
            ("steampipe", "steam pipe"),
            ("winefestival", "wine festival"),
            ("goatenclosure", "goat enclosure"),
        ],
    },
};
