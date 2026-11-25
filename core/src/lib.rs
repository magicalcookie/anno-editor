// What the editor shares across Anno games: which game is in use, the shape
// of the facts that differ between them and the `Module` a game crate
// provides (`game`), how a preset lights an island (`ambient`) and the sky
// cube maps it names (`cube`), the wording
// that differs (`text`), the harbour pieces and work-area roads it copies
// and lays (`harbour`), the island's water handed to a game's water pass
// (`water`), finding a game's installs (`discovery`) and the mods a player
// has installed (`modinfo`), the model every game's converter reads
// and writes (`neutral`), the kinds of file a game holds (`files`), map layouts
// (`map`), what its projects and mods need (`project`) and what its UI screens need
// (`ui`). Each game's own values and code live in its
// crate under games/.

pub mod ambient;
pub mod cube;
pub mod discovery;
pub mod fertility;
pub mod files;
pub mod game;
pub mod harbour;
pub mod island;
pub mod map;
pub mod modinfo;
pub mod neutral;
pub mod project;
pub mod text;
pub mod ui;
pub mod water;
