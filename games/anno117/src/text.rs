// Anno 117’s own wording in the editor.
use anno_core::text::{GameText, Playtest};

pub static TEXT: GameText = GameText {
    subtitle: "Pax Romana",
    era: "BUILD YOUR ROMAN LEGACY",
    regions: "Latium, Albion and Aegyptus islands",
    add_model: "A .cfg or .prp with its meshes and textures, or a bare .rdm with its textures beside it",
    model_formats: "A .cfg or .prp model, or an .rdm mesh with textures",
    slot_resource: "Resources belong to the island, not the slot: the test map gives the island this deposit and keeps this slot active",
    slot_roads: "Moves the selected slots' work areas 3 m down the slope and links each to its slot, like shipped islands. Ctrl-click adds another work area",
    playtest: Playtest {
        intro: "Adds an \"Editor islands\" map type to New game. Its maps are the Default maps with your islands in fixed slots, so every seed shows them.",
        steps: "1. Install  2. Start the game, New game → map type \"Editor islands\"  3. If the island is missing, check mods/mod-loader.log",
        in_game: "New game → map type Editor islands, Large world",
    },
    playtest_old_world: None,
    asset_words: anno_core::game::AssetWords {
        proper: &[
            "Albion", "Latium", "Celtic", "Roman", "Prydein", "Egypt", "Egyptian",
        ],
        joined: &[
            ("pirateoutpost", "pirate outpost"),
            ("legioneagle", "legion eagle"),
            ("workareaslot", "work area slot"),
            ("3rdparty", "third-party"),
            ("marshveg", "marsh"),
            ("noskirt", "no skirt"),
            ("simplecfg", "model object"),
            ("questobject", "quest object"),
            ("stonepine", "stone pine"),
            ("waterplant", "water plant"),
            ("watergrass", "water grass"),
            ("fishshoal", "fish shoal"),
            ("wildhorse", "wild horse"),
        ],
    },
};
