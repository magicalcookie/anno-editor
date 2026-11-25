// The shape of the editor wording that differs per game; each game's crate
// writes its own `GameText`. The facts the wording rests on (paths, size
// classes, features) live in its `Profile`; sentences built from those
// facts stay at their use.

pub struct GameText {
    /// Launcher card: the line under the number and the era line over it.
    /// The card's accent colour is `colors::Launcher::accent`.
    pub subtitle: &'static str,
    pub era: &'static str,
    /// What differs per game, not the same feature list twice: the regions
    /// its islands come from.
    pub regions: &'static str,
    /// Add model's tooltip.
    pub add_model: &'static str,
    /// What Add model's file picker accepts, said under the Models page's
    /// Add model.
    pub model_formats: &'static str,
    /// The tooltip on a slot's test map resource choices.
    pub slot_resource: &'static str,
    /// The Slot tool's Road button tooltip: how this game lays work areas.
    pub slot_roads: &'static str,
    pub playtest: Playtest,
    /// Test maps that include the Old World, whose Archipelago map type
    /// they relabel. `None`: the game has no such case.
    pub playtest_old_world: Option<Playtest>,
    /// Proper names and run-together words in the game's asset ids and
    /// texts, for showing them as words.
    pub asset_words: crate::game::AssetWords,
}

/// The Test in game dialog and the status line after installing.
pub struct Playtest {
    pub intro: &'static str,
    pub steps: &'static str,
    /// "In game: …" after a successful install.
    pub in_game: &'static str,
}
