// What a game's UI screens need from the editor: where the compiled
// screens and their textures live, the HUD the game view stacks, and the
// values the game's own assets give the preview's sample data. Which files
// open in the UI editor is the game's file kinds' (`files::View::Screen`).

use anno_formats::assets::Assets;
use anno_formats::phx::{DataFields, DefaultValue, SourceDirs};
use std::sync::Arc;

pub trait UiScreens: Sync {
    fn screens(&self) -> &'static Screens;
}

/// One game's UI facts, as plain data.
pub struct Screens {
    /// Compiled screens, widget templates and image assets, each file
    /// named by its GUID; ends in `/`.
    pub generated: &'static str,
    /// Where an image asset a GUID names is looked for after `generated`.
    /// Neither 117 nor 1800 ships files there today.
    pub images: &'static str,
    pub textures: Textures,
    /// Screens (file names in `generated`) the game shows on top of an
    /// island, bottom to top: what the game view's HUD button stacks.
    pub hud: &'static [&'static str],
    /// An in-game shot of that HUD at 1920×1080, a PNG outside shipped
    /// builds (it is game art), for the game view to compare against.
    pub hud_reference: Option<&'static str>,
    /// The ComponentTemplate of an infotip's hover zone, whose images only
    /// show in the designer.
    pub infotip_area: Option<&'static str>,
    /// The ComponentTemplate of a drag-and-drop target, shown in game only
    /// while something is dragged.
    pub drop_zone: Option<&'static str>,
    /// Where the install keeps its Phoenix sources: enum entry names and
    /// what widget types, components and effects are for.
    pub sources: SourceDirs,
    /// The game-object field ids its screens' data paths name.
    pub data_fields: &'static DataFields,
    /// UI assets without type information are editable: the editor knows
    /// 117's whole Phoenix schema, 1800 assets only when typed.
    pub untyped_editable: bool,
    /// Reads what the game's own assets put where the preview has only
    /// sample data; `None` for a game whose assets give none.
    pub sample_data: Option<ReadSamples>,
    /// The image game-object icon fields (`Data.Icon`) show with sample
    /// data, so icon slots aren't empty; none leaves them empty.
    pub sample_icon: Option<&'static str>,
    /// What player-colour layers show with sample data, sRGB.
    pub sample_player_color: [u8; 3],
}

/// How an image asset's source path names the texture the game ships.
pub struct Textures {
    /// What replaces the source's extension (`.png` → `_0.dds`).
    pub suffix: &'static str,
    /// Folder swaps tried after the path as named, each on its first
    /// match: assets are authored against one folder, the game ships
    /// another (117: `/fhd/` sources, `/4k/` textures with the same UVs).
    pub swaps: &'static [(&'static str, &'static str)],
}

impl Textures {
    /// The texture paths `source` may be shipped under, in the order to
    /// try: as named, then lower case, each before its swaps.
    pub fn candidates(&self, source: &str) -> Vec<String> {
        let source = source.replace('\\', "/");
        let stem = source.rsplit_once('.').map_or(source.as_str(), |(s, _)| s);
        let lower = stem.to_ascii_lowercase();
        let mut out = Vec::new();
        for stem in [stem, lower.as_str()] {
            out.push(format!("{stem}{}", self.suffix));
            for (from, to) in self.swaps {
                if stem.contains(from) {
                    out.push(format!("{}{}", stem.replacen(from, to, 1), self.suffix));
                }
            }
        }
        out
    }
}

/// Builds a game's [`SampleData`] from its assets.
pub type ReadSamples = fn(&Assets) -> Option<Arc<dyn SampleData>>;

/// Values a game's own assets give where a screen's layout has only sample
/// data (117's minimap ring: its buttons, lodes and fertilities).
pub trait SampleData: Send + Sync {
    /// The value for property path `name` (lower case; `last` is its last
    /// segment), under list item `item` (the list's path and the item's
    /// index) when it is in one; `None` leaves it to the generic rules.
    fn sample(&self, item: Option<(&str, usize)>, name: &str, last: &str) -> Option<DefaultValue>;
}
