// Editor wording that differs per game, one `GameText` per game. The facts
// the wording rests on (paths, size classes, features) live in
// `anno_formats::game`; sentences built from those facts stay at their use.
use anno_formats::game::Game;

pub struct GameText {
    /// Launcher card: the line under the number and the era line over it.
    /// The card's accent colour is `colors::Launcher::accent`.
    pub subtitle: &'static str,
    pub era: &'static str,
    /// What differs per game, not the same feature list twice: the regions
    /// its islands come from.
    pub regions: &'static str,
    /// The import command's one label, wherever it is offered.
    pub import_label: &'static str,
    /// Short on the project card, where the full label squeezed the title;
    /// the full wording on hover and in the File menu.
    pub import_short: &'static str,
    /// Add model's tooltip.
    pub add_model: &'static str,
    /// What Add model's file picker accepts, said under the Models page's
    /// Add model.
    pub model_formats: &'static str,
    /// The tooltip on a slot's test map resource choices.
    pub slot_resource: &'static str,
    pub playtest: Playtest,
    /// Test maps that include the Old World, whose Archipelago map type
    /// they relabel. `None`: the game has no such case.
    pub playtest_old_world: Option<Playtest>,
}

/// The Test in game dialog and the status line after installing.
pub struct Playtest {
    pub intro: &'static str,
    pub steps: &'static str,
    /// "In game: …" after a successful install.
    pub in_game: &'static str,
}

pub fn of(game: Game) -> &'static GameText {
    match game {
        Game::Anno117 => &ANNO117,
        Game::Anno1800 => &ANNO1800,
    }
}

static ANNO117: GameText = GameText {
    subtitle: "Pax Romana",
    era: "BUILD YOUR ROMAN LEGACY",
    regions: "Latium and Albion islands",
    import_label: "Import island from Anno 1800…",
    import_short: "Import from Anno 1800…",
    add_model: "A .cfg or .prp with its meshes and textures, or a bare .rdm with its textures beside it",
    model_formats: "A .cfg or .prp model, or an .rdm mesh with textures",
    slot_resource: "Resources belong to the island, not the slot: the test map gives the island this deposit and keeps this slot active",
    playtest: Playtest {
        intro: "Adds an \"Editor islands\" map type to New game. Its maps are the Default maps with your islands in fixed slots, so every seed shows them.",
        steps: "1. Install  2. Start the game, New game → map type \"Editor islands\"  3. If the island is missing, check mods/mod-loader.log",
        in_game: "New game → map type Editor islands, Large world",
    },
    playtest_old_world: None,
};

static ANNO1800: GameText = GameText {
    subtitle: "The Industrial Revolution",
    era: "AN AGE OF INVENTION",
    regions: "Old World, New World, Arctic and Enbesa islands",
    import_label: "Import island from Anno 117…",
    import_short: "Import from Anno 117…",
    add_model: "A .cfg or .prp with its meshes and textures",
    model_formats: "Model configurations (.cfg, .prp) with their meshes and textures",
    slot_resource: "The test map replaces this random slot with the chosen resource. Other random slots receive their first valid resource.",
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
};
