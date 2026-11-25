// Owns which game is in use and the shape of every fact that differs
// between the games: the `Module` a game's crate implements, its identity
// (`Profile`) and the capabilities it has, each with its own data
// (`IslandRules` behind `islands()`). Code that behaves differently per
// game asks the module, never matches on `Game`.

use crate::ambient::Ambient;
use crate::harbour::{Piece, RoadShape};
use crate::island::{Island, Level, Problem};
use crate::text::GameText;
use anno_formats::vfs::Install;

pub use crate::project::Packaging;
pub use crate::ui::UiScreens;

/// The game whose install and format rules are in use.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Game {
    #[serde(rename = "117")]
    Anno117,
    #[serde(rename = "1800")]
    Anno1800,
    #[serde(rename = "2070")]
    Anno2070,
}

impl Game {
    pub const ALL: [Self; 3] = [Self::Anno1800, Self::Anno117, Self::Anno2070];
}

/// A game as its crate under games/ provides it: the app shell holds one
/// per `Game` and asks it for whatever the game does its own way.
pub trait Module: Sync {
    fn game(&self) -> Game;
    fn profile(&self) -> &'static Profile;
    /// The editor's wording that differs for this game.
    fn text(&self) -> &'static GameText;
    /// The launcher card's key art, a JPEG.
    fn card_art(&self) -> &'static [u8];
    /// The launcher card's accent, 0xRRGGBB on the dark and on the light
    /// theme.
    fn accent(&self) -> [u32; 2];
    /// Reads `source`, one of this game's models in `install`, into the
    /// neutral model; `None` for a game the editor converts no models from.
    fn read_model(
        &self,
        _install: &Install,
        _source: &str,
    ) -> Option<anyhow::Result<crate::neutral::Model>> {
        None
    }
    /// Writes `model`, whose files `from` reads, as this game's models,
    /// copying the shipped templates of `to`; `None` for a game the editor
    /// writes no models for.
    fn write_model(
        &self,
        _model: &crate::neutral::Model,
        _from: &dyn crate::neutral::Source,
        _to: &Install,
    ) -> Option<anyhow::Result<crate::neutral::Ported>> {
        None
    }
    /// The island editor's rules and island format; `None` for a game
    /// whose islands the editor doesn't open.
    fn islands(&self) -> Option<&dyn Islands> {
        None
    }
    /// What an island view draws beside the kit's own: sky, water, ground,
    /// families and view modes.
    fn scene(&self) -> Option<&dyn SceneParts> {
        None
    }
    /// The UI screens the editor opens for this game.
    fn ui(&self) -> Option<&dyn UiScreens> {
        None
    }
    /// How a project installs into the game and runs Test in game; `None`
    /// keeps both off.
    fn packaging(&self) -> Option<&'static Packaging> {
        None
    }
    /// The folders a project of this game writes under its mod folder, the
    /// first the one a new project starts with; empty for a game without
    /// projects. Outside every capability: a game without packaging, or
    /// with screens and no islands, still saves its projects.
    fn project_roots(&self) -> &'static [&'static str] {
        &[]
    }
    /// Which models this game brings in as its own props.
    fn ports(&self) -> crate::files::Ports {
        crate::files::Ports::NONE
    }
    /// How this game's models look in previews and thumbnails.
    fn model_look(&self) -> crate::files::ModelLook {
        crate::files::ModelLook::PLAIN
    }
    /// The file kinds this game reads beyond core's generic ones
    /// (`files::GENERIC`), tried first.
    fn files(&self) -> &'static [crate::files::Kind] {
        &[]
    }
    /// How this game's asset database and texts are laid out; `None` for a
    /// game without one the editor reads.
    fn database(&self) -> Option<&'static anno_formats::assets::Schema> {
        None
    }
}

pub trait Islands: Sync {
    /// How this game's islands are laid out, named and generated.
    fn rules(&self) -> &'static IslandRules;
    /// The game's map layouts: the map editor, and Test in game's test
    /// maps; none for a game whose maps the editor doesn't open.
    fn maps(&self) -> Option<&dyn crate::map::Maps> {
        None
    }
    /// What the shipped island at `path` is, for the island list's filter;
    /// `asset_kind` is its `RandomIsland/IslandType` where the game's assets
    /// give one. By default the rules' [`IslandKinds`] decide.
    fn kind_of(&self, path: &str, asset_kind: Option<&str>) -> IslandKind {
        self.rules().island_kinds.classify(path, asset_kind)
    }
    /// How this game's islands are read and saved.
    fn codec(&self) -> Codec<'_>;
    /// Every island `install` ships, by path: the Islands pane lists them,
    /// and New island and Import build on the `pool` ones. `assets` is the
    /// install's database when the caller holds it. None by default; 117
    /// and 1800 list theirs by `.a7minfo` ([`a7m_islands`]).
    fn shipped(
        &self,
        _install: &Install,
        _assets: Option<&anno_formats::assets::Assets>,
    ) -> Vec<ShippedIsland> {
        Vec::new()
    }
    /// What stops `island` leaving the editor (level 5, [`Policy::Block`]):
    /// references `refs` can't resolve, and what the game needs of an
    /// island used as `use_` that it lacks. By default the checks every
    /// game shares ([`broken`]).
    ///
    /// [`Policy::Block`]: crate::island::Policy::Block
    fn validate(&self, island: &Island, refs: &Refs, use_: Use) -> Vec<Problem> {
        broken(self, island, refs, use_)
    }
    /// Which piece an asset plays on this game's islands, by its template
    /// and internal name: what the harbour library copies and conversion
    /// recognises. None for a game whose harbours and slots the tools
    /// don't build (2070).
    fn piece_of(&self, _template: &str, _name: &str) -> Option<Piece> {
        None
    }
    /// What this game calls another game's ground material `<GroundType>`,
    /// for material sets brought over; by default the same word.
    fn ground_type<'a>(&self, ground_type: &'a str) -> &'a str {
        ground_type
    }
    /// Whether a saved island keeps objects placed, moved or removed in the
    /// editor. Off, models aren't offered for placing.
    fn places_objects(&self) -> bool {
        true
    }
    /// The shared coast pieces, slots and work areas the harbour and slot
    /// tools lay (117's and 1800's); `None` for a game without them.
    fn harbours(&self) -> Option<&dyn Harbours> {
        None
    }
}

/// The harbour and slot tools' shared pieces: harbour coasts, resource
/// slots and the work areas laid from them, copied from records the
/// game's shipped islands hold (`harbour::Library`).
pub trait Harbours: Sync {
    /// The pieces a region's harbour library needs before it stops reading
    /// shipped islands.
    fn required_pieces(&self, region: &str) -> &'static [Piece];
    /// How a work area playing `kind` (None when unknown) is laid and
    /// pathed to its slot.
    fn road_shape(&self, kind: Option<Piece>) -> RoadShape;
}

/// How a game's islands are read and saved. The a7m archives 117 and 1800
/// share are read and written by the shell, which keeps every derived
/// file (material levels, terrain meshes, tint) beside them; any other
/// format is the game's own.
#[derive(Clone, Copy)]
pub enum Codec<'a> {
    /// a7m archives, each island in a folder of its own
    /// (`<name>/<name>.a7m`, `<name>.a7minfo`, `_gamedata/<name>/`).
    A7m(&'static A7mRules),
    /// The game's own format, one file per island.
    Own(&'a dyn IslandCodec),
}

impl<'a> Codec<'a> {
    /// The game's own codec; `None` for a7m islands.
    pub fn own(self) -> Option<&'a dyn IslandCodec> {
        match self {
            Codec::Own(codec) => Some(codec),
            Codec::A7m(_) => None,
        }
    }

    /// The a7m rules; `None` for a game with its own codec.
    pub fn a7m(self) -> Option<&'static A7mRules> {
        match self {
            Codec::A7m(rules) => Some(rules),
            Codec::Own(_) => None,
        }
    }

    /// Whether each island has a folder of its own that holds every file
    /// of it, rather than one file beside others of its kind.
    pub fn folders(self) -> bool {
        matches!(self, Codec::A7m(_))
    }

    /// The virtual path of a new island called `name` (file-safe already)
    /// in `region`, beside the shipped ones: an a7m island's folder under
    /// `rules.island_folder`, an own-format island where its codec puts it.
    /// `None` for a codec that makes no new islands.
    pub fn island_path(self, rules: &IslandRules, region: &str, name: &str) -> Option<String> {
        match self {
            Codec::A7m(_) => {
                let [before, after] = rules.island_folder;
                Some(format!("{before}{region}{after}/{name}/{name}.a7m"))
            }
            Codec::Own(codec) => codec.island_path(region, name),
        }
    }
}

/// How the a7m pipeline reads and bakes a game's islands.
pub struct A7mRules {
    /// How the game's terrain bake blends ground layers, which the splat
    /// copies.
    pub ground_bake: Bake,
    /// The tint level of `BaseTextureData`, the resident base texture. Both
    /// games stream 32 texels/m at level 0.
    pub tint_base_level: u32,
    /// Shipped islands that can be a new island's skeleton are the
    /// `Normal` kind in `assets.xml`, so listing them reads the assets
    /// first. 117 lists every square island.
    pub islands_from_assets: bool,
    /// An island's `.a7minfo` is the placement header read with it,
    /// written back on every save. 117 writes a fresh one for new islands
    /// and leaves shipped ones as shipped.
    pub info_from_island: bool,
    /// Ground materials are `materialmaps/<level>/matid.dds` and
    /// `matalpha.dds` beside the island; 1800 embeds them in the island.
    pub material_files: bool,
    /// The map generator's island types that random `.a7tinfo` slots name.
    pub pool_types: PoolTypes,
    /// The material set an island means when its rd3d names none; `None`
    /// for a game whose islands always name one.
    pub default_material_set: Option<&'static str>,
}

impl A7mRules {
    /// The set an island paints with: the one it names, else the game's
    /// default; empty when there is neither.
    pub fn material_set<'a>(&self, named: Option<&'a str>) -> &'a str {
        named.or(self.default_material_set).unwrap_or_default()
    }
}

/// `Config/Type/id` values of random map template slots, indices into the
/// pool's `IslandType` list (properties.xml `MapGenerator`). Test maps fill
/// these two kinds; decoration, third parties and volcanoes stay vanilla.
pub struct PoolTypes {
    pub normal: u16,
    pub starter: u16,
}

/// A game's own island format: the file into the neutral island and back.
pub trait IslandCodec: Sync {
    /// Reads island `path` from `install` into the neutral island, setting
    /// its `native` (`Island::set_native`) so `write` can carry what the
    /// document doesn't hold. `assets` is the install's database when the
    /// caller holds it, so the read needn't parse it again.
    fn read(
        &self,
        install: &Install,
        path: &str,
        assets: Option<&anno_formats::assets::Assets>,
    ) -> anyhow::Result<crate::island::Island>;
    /// The bytes of island file `path` with `island`'s edits, which `read`
    /// gave. `assets`, the install's database when the caller holds it,
    /// names the records of models the file never placed.
    fn write(
        &self,
        island: &crate::island::Island,
        path: &str,
        assets: Option<&anno_formats::assets::Assets>,
    ) -> anyhow::Result<Vec<u8>>;
    /// Whether model `cfg` places as a prop, not through [`object_for`]. By
    /// default every model does; on 2070 only the trees, rocks and grass,
    /// whose records the island's props save into.
    ///
    /// [`object_for`]: Self::object_for
    fn places_as_prop(&self, _assets: &anno_formats::assets::Assets, _cfg: &str) -> bool {
        true
    }
    /// A new object of the asset that draws model `cfg`, to place on
    /// `island`, copying a shipped record of its kind; `None` where the
    /// format places no objects.
    fn object_for(
        &self,
        _install: &Install,
        _assets: &anno_formats::assets::Assets,
        _island: &crate::island::Island,
        _cfg: &str,
    ) -> Option<anyhow::Result<crate::island::objects::Object>> {
        None
    }
    /// Island `path` seen from above, `edge` texels a side over its whole
    /// square, for the map view: land opaque, shallows faint, sea clear.
    /// `None` for a game whose islands ship a map picture (117's and
    /// 1800's, which the shell reads) or an island that can't be read.
    fn top_view(
        &self,
        _install: &Install,
        _path: &str,
        _edge: u32,
    ) -> Option<gpui_editor_kit::texture::Texture> {
        None
    }
    /// Where a new island called `name` (file-safe already) goes in
    /// `region`, the folder its shipped list groups it under; `None` for a
    /// format that makes no new islands.
    fn island_path(&self, _region: &str, _name: &str) -> Option<String> {
        None
    }
    /// The model whose object stands for a slot of `resource` on this
    /// game's islands, for a format without slot records (2070's, whose mines
    /// and river slots are handles of their own asset); `None` where it
    /// has no such resource.
    fn slot_model(&self, _resource: crate::harbour::Resource) -> Option<&'static str> {
        None
    }
    /// The turn, in radians, that objects of asset `guid` stand on on this
    /// game's islands, from what `install` ships: Place turns a new one by
    /// whole steps of it and the rotate gizmo snaps to it. 0 for a free
    /// turn. Called on the UI thread, so it never waits: `None` while the
    /// game is still reading what decides it, off that thread.
    fn turn_step(&self, _install: &Install, _guid: u32) -> Option<f32> {
        Some(0.)
    }
    /// New harbour objects along `coast`, points on `island`'s shore (x,
    /// z), for a game whose harbours are objects laid by its own rules
    /// (2070's tiles), copying records `install` ships. `None` where the
    /// harbour tool places the shared coast pieces (`Harbours`), or none.
    fn harbour_along(
        &self,
        _install: &Install,
        _island: &crate::island::Island,
        _coast: &[[f32; 2]],
    ) -> Option<anyhow::Result<Vec<crate::island::objects::Object>>> {
        None
    }
}

pub trait SceneParts: Sync {
    fn rain(&self) -> bool {
        false
    }
    /// For an island at `path` with a dry and a flood season the water
    /// draws (117's Egypt), how high each raises the rivers, dry then flood:
    /// its `Season` assets' `RiverHeight`, else the shipped values while
    /// `assets` are still loading. `None` for islands without seasons.
    fn seasons(
        &self,
        _path: &str,
        _assets: Option<&anno_formats::assets::Assets>,
    ) -> Option<[f32; 2]> {
        None
    }
    /// The instance word (`Renderer::set_instance_words`) an object owned
    /// by player `owner` draws with, which its surface family dyes by;
    /// `player` is the colour (`ModelLook::player_colors`) View settings
    /// picks for player 1. 0, undyed, by default and for no owner.
    fn owner_word(&self, _owner: u32, _player: usize) -> u32 {
        0
    }
    /// The folder of the engine's `default.xml` and `extended.xml` (its
    /// view distances for the normal and the extended camera), and the
    /// View Distance level the game starts with; `None` for a game whose
    /// foliage the editor draws at any distance.
    fn engine_settings(&self) -> Option<(&'static str, u32)> {
        None
    }
    /// The ambient presets an island of `region` can pick, in the order the
    /// picker offers them, by the names `ambients` takes; none where the
    /// island's own light is the only one (2070's, see `island_ambient`).
    fn ambient_names(&self, _install: &Install, _region: &str) -> Vec<String> {
        Vec::new()
    }
    fn volumetric_clouds(&self) -> bool {
        false
    }
    /// The ambient presets among `names` that `install` has, each read into
    /// the light, sky and sea it gives.
    fn ambients(&self, install: &Install, names: &[&str]) -> Vec<(String, Ambient)>;
    /// The ambient preset that lights island `path` when the island file
    /// names none of its own: 2070 finds it in the worlds that place the
    /// island. `None` leaves the island's session's.
    fn island_ambient(&self, _install: &Install, _path: &str) -> Option<String> {
        None
    }
    /// The view island `path` opens in when it isn't the plain one from
    /// the air: 2070's Deep Ocean islands are seen from under the water.
    /// `None` for the plain view.
    fn island_view(&self, _install: &Install, _path: &str) -> Option<crate::ambient::View> {
        None
    }
    /// The ground that draws `paint`, an island's paint in this game's own
    /// form, read from `install`; `None` where the a7m splat draws it, or
    /// the kit's terrain material.
    fn ground(
        &self,
        _install: &Install,
        _paint: &crate::island::Paint,
    ) -> Option<anyhow::Result<Box<dyn IslandGround>>> {
        None
    }
    /// The pass that draws this game's sea, rivers and lakes on one
    /// island, reading what it needs from `install`; none for games whose
    /// water the editor draws flat.
    fn water(&self, _install: &Install) -> Option<std::sync::Arc<dyn crate::water::Water>> {
        None
    }
    /// The passes that draw this game's sky and its haze on one island,
    /// reading what they need from `install`; none for games whose sky the
    /// kit's backdrop draws from the baked map.
    fn sky(&self, _install: &Install) -> Option<std::sync::Arc<dyn crate::ambient::SkyPass>> {
        None
    }
    /// Builds the pipelines this game's water and sky passes draw with, on
    /// `gpu`'s device and the calling thread, before an island needs them
    /// (`gpu::warm_passes`): the first island opened otherwise waited for the
    /// driver to compile them, 0.9 s for 117's sea.
    fn warm(&self, _install: &Install, _gpu: &gpui_editor_kit::gpu::Warm<'_>) {}
    /// The tone curve for presets whose sky brings none of its own
    /// (`ambient::Sky::curve`).
    fn tone_curve(&self) -> gpui_editor_kit::gpu::Curve {
        gpui_editor_kit::gpu::Curve::default()
    }
    /// How scenes under `tone_curve` expose themselves frame by frame, on
    /// top of the preset's exposure; `None` leaves the preset's alone.
    fn auto_exposure(&self) -> Option<gpui_editor_kit::gpu::AutoExposure> {
        None
    }
}

/// A game's ground for one island's paint, kept for the island's life so
/// the kit keeps it mounted across scenes.
pub trait IslandGround: Send + Sync {
    /// What scenes draw the ground with while it shows its paint.
    fn draw(&self) -> gpui_editor_kit::gpu::GroundDraw;
    /// Hands the ground `paint`, changed within world `area` (x0, z0, x1,
    /// z1 metres); it uploads only what changed there.
    fn update(&self, paint: &crate::island::Paint, area: [f32; 4]);
    /// A thumbnail per paint id for the paint palette, from `install`.
    fn thumbs(&self, install: &Install) -> Vec<Thumb>;
    /// What the status line says of how `paint` shows in game, such as
    /// layers the game draws as a stand-in.
    fn note(&self, _paint: &crate::island::Paint) -> Option<String> {
        None
    }
    /// Whether paint id `id` draws the developers' stand-in in game
    /// (2070's magenta `wiki_dummy_diff`).
    fn stand_in(&self, _id: u8) -> bool {
        false
    }
}

/// One paint id as the paint palette shows it.
pub struct Thumb {
    pub id: u8,
    /// What it is, by the path of its texture: conversion reads ground
    /// roles (rock, sand, sea floor) from it.
    pub name: String,
    pub image: gpui_editor_kit::texture::Texture,
    /// It draws colour, rather than scattering grass or bushes: conversion
    /// maps ground onto colour ids only.
    pub colour: bool,
}

/// Who a game is: what install discovery, the launcher and a project's
/// manifest need, whatever else the game does. Its island rules sit behind
/// `Islands::rules`, its scene's behind `scene()` and its models' in
/// `model_look()`, so a game without islands never holds another game's
/// values. Paths are virtual (inside the mounted archives) unless they say
/// otherwise.
pub struct Profile {
    pub title: &'static str,
    /// The number players call the game by, `117` or `1800`.
    pub short_name: &'static str,
    /// Install folder override.
    pub env_var: &'static str,
    /// The editor's config folder for this game.
    pub config_folder: &'static str,
    pub steam_app_id: &'static str,
    /// The install's folder under `steamapps/common`.
    pub steam_folder: &'static str,
    /// The game's folder under the user's Documents, which holds its
    /// config, logs and a `mods` folder the loader also reads.
    pub documents_folder: &'static str,
    /// The executable that identifies an install, relative to its folder.
    pub exe: &'static str,
    /// mod.io's game id, from the loader's search paths (mod-loader.log).
    pub modio_game: &'static str,
    /// The archive families `maindata` mounts first, in order
    /// (`Install::open`).
    pub archive_families: &'static [&'static str],
    /// `modinfo.json` `Anno`. Every project writes a manifest, packaged or
    /// not.
    pub modinfo_anno: u32,
    pub dependencies: Dependencies,
}

/// How a game's islands are laid out, named and generated: what the island
/// editor, its tools and Test in game read through `Islands::rules`.
pub struct IslandRules {
    pub prop_sets_path: &'static str,
    /// New islands go to `{0}{region}{1}/{name}/{name}.a7m`, next to the
    /// shipped ones of their region.
    pub island_folder: [&'static str; 2],
    /// The shipped asset every pool island's `RandomIsland` entry is
    /// inserted after, so the ModOp has a stable anchor. 117's is a roman
    /// pool island.
    pub pool_anchor_guid: u32,
    /// Object templates that are only decoration: auto blockers cover them
    /// as they do rocks.
    pub decoration_templates: &'static [&'static str],
    /// The path marker of the map generator's pool islands, whose slots
    /// and harbours the harbour library copies; empty for none.
    pub pool_folder: &'static str,
    /// Path words of large islands, which carry the start coast: record
    /// searches read them first.
    pub large_islands: &'static [&'static str],

    /// `(path marker, region)`: an island path containing the marker is in
    /// that region, first match wins; others are in `default_region`.
    pub regions: &'static [(&'static str, &'static str)],
    pub default_region: &'static str,
    /// `(region, name)` where the assets name a region otherwise than its
    /// folder (117's `Egypt` islands are `Egyptian` in AssociatedRegions
    /// and fertility sets). Regions not listed are named alike.
    pub asset_regions: &'static [(&'static str, &'static str)],
    /// What the game calls each region's province, and its shipped prop
    /// sets. Regions not listed show as named.
    pub provinces: &'static [Province],
    /// The roles an island can play, the first the one a new island takes.
    pub roles: &'static [Role],
    /// The map generator's island size classes, smallest first.
    pub size_classes: &'static [SizeClass],
    /// Ambience environment types in `EnvironmentGRid` value order, with the
    /// debug overlay colour (ARGB) from `assets.xml` `EnvironmentTypeMapping`.
    /// Index = stored value.
    pub environment_types: &'static [(&'static str, u32)],
    /// What the marsh grid (`IrrigationManager/m_StaticTileGrid`) holds on
    /// marsh ground; 0 for a game whose islands have none.
    pub marsh_tile: u32,
    /// The types the landscape rules derive.
    pub environments: Environments,
    /// `(region, environment)` where island ground is not
    /// `environments.meadow`.
    pub land_environments: &'static [(&'static str, &'static str)],
    /// Harvestable trees are the live ones under `vegetation/trees/` whose
    /// path contains none of these.
    pub tree_exclusions: &'static [&'static str],

    /// A new island takes its region's first named vegetation prop set
    /// rather than keeping the skeleton's.
    pub regional_prop_set: bool,
    /// The Trees scatter preset uses the harvestable tree rule. 117's preset
    /// skips only dead, stump, palm and log stems.
    pub harvestable_tree_preset: bool,
    pub slots: ResourceSlots,
    /// A test map can fix an island's fertilities instead of rolling them.
    pub fertility_choice: bool,
    /// Pool entries state the island's size class
    /// (`RandomIsland/IslandPlaceholderSize`), and the inspector offers to
    /// override it. 1800's `RandomIsland` has no such field.
    pub placeholder_size: bool,
    /// The games Import island reads islands from, the first offered
    /// first; none for a game without it.
    pub import_from: &'static [Game],
    /// The yaw step (radians) shipped resource slots snap to.
    pub slot_yaw_step: f32,
    /// Auto cliffs' pieces by region name (matched ignoring case); `""`
    /// stands for every region not listed. Regions without a set get none.
    pub cliffs: &'static [(&'static str, &'static CliffSet)],
    /// The waterfall sound spots (`AudioSpots` assets) auto placement
    /// writes per fall size, small to big: one at the fall's foot, or one
    /// each at its top, curtain and foot.
    pub waterfall_spots: [&'static [u32]; 3],
    /// The sound spot auto placement writes along steep coast.
    pub cliff_spot: u32,
    /// The asset whose cubes (one variation per side, `blockers::SIZES`)
    /// Auto blockers lays over rocks and decoration; 0 for none.
    pub build_blocker: u32,
    /// How shipped islands say what they are, for the island list.
    pub island_kinds: IslandKinds,
    /// Islands may lie wholly under the sea (2070's underwater islands).
    /// Elsewhere an island with no ground above it is a kind the game
    /// lacks.
    pub underwater_islands: bool,
    /// `(source region, province)`: the province (both by folder name) an
    /// island of an `import_from` game's region starts on in Import, the
    /// one closest in climate; regions not listed suggest none. Sources
    /// name regions as their island lists group them (2070's `normal`).
    pub import_regions: &'static [(&'static str, &'static str)],
    /// Path words that tie a model to one of the game's regions (by the
    /// region's lowercase name), for conversion. Models carrying none of
    /// them are shared between its regions.
    pub region_words: &'static [(&'static str, &'static [&'static str])],
    /// The game's ground material names for the editor's ground roles.
    pub ground_names: GroundNames,
}

/// A game's ground material names for the editor's ground roles, each list
/// best first and matched by prefix. A role a game doesn't list has no
/// preference: conversion then keeps id order among a kind's materials,
/// and a World Machine channel or a tool's role paints nothing.
#[derive(Clone, Copy, Debug)]
pub struct GroundNames {
    /// By ground kind (the shell's `Ground::label`: "rock", "sea floor"),
    /// which of the kind's materials conversion takes when their colours
    /// don't decide, matched against the file's stem.
    pub preferred: &'static [(&'static str, &'static [&'static str])],
    /// By World Machine splatmap channel, the palette material it paints
    /// (`pick`): the shell's template `CHANNELS` ("cliff", "beach &
    /// shelf") and its default export's four (worldmachine.rs
    /// `DEFAULT_CHANNELS`: "eroded ridges", "base cover").
    pub channels: &'static [(&'static str, &'static [&'static str])],
    /// By what an editor tool paints, the palette material (`pick`): a
    /// river's or lake's "bed sand", a river's "bed edge", "bed pebbles"
    /// and "bed rock", and a new island's "island base".
    pub painted: &'static [(&'static str, &'static [&'static str])],
    /// Region prefixes a material file's name may carry before its own,
    /// past the one-letter ones every game's may (`m_gras_02`).
    pub prefixes: &'static [&'static str],
}

impl GroundNames {
    pub const NONE: GroundNames = GroundNames {
        preferred: &[],
        channels: &[],
        painted: &[],
        prefixes: &[],
    };

    pub fn preferred(&self, kind: &str) -> &'static [&'static str] {
        Self::find(self.preferred, kind)
    }

    pub fn channel(&self, role: &str) -> &'static [&'static str] {
        Self::find(self.channels, role)
    }

    pub fn painted(&self, role: &str) -> &'static [&'static str] {
        Self::find(self.painted, role)
    }

    /// A material file's name without its folder and region prefix.
    pub fn name<'f>(&self, file: &'f str) -> &'f str {
        let file = file.rsplit('/').next().unwrap_or(file);
        file.split_once('_')
            .filter(|(prefix, _)| prefix.len() == 1 || self.prefixes.contains(prefix))
            .map_or(file, |(_, name)| name)
    }

    /// The id of the first of `files` whose `name` starts with `wants`'
    /// first name, else its second, and so on. Names match from the start:
    /// "grass_01" must not pick "underwatergrass_01".
    pub fn pick<'f, I>(&self, wants: &[&str], files: I) -> Option<u8>
    where
        I: IntoIterator<Item = (u8, &'f str)> + Clone,
    {
        wants.iter().find_map(|want| {
            (files.clone().into_iter())
                .find_map(|(id, f)| self.name(f).starts_with(want).then_some(id))
        })
    }

    fn find(
        table: &'static [(&'static str, &'static [&'static str])],
        key: &str,
    ) -> &'static [&'static str] {
        table
            .iter()
            .find(|(k, _)| *k == key)
            .map_or(&[], |(_, names)| names)
    }
}

/// What a shipped island is, as the island list filters it. `All` is the
/// filter's "any kind"; no island classifies as it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum IslandKind {
    All,
    /// The default: under All, decoration and campaign islands filled the
    /// first screen before any island one would start from.
    #[default]
    Playable,
    Decoration,
    Other,
}

impl IslandKind {
    pub const ALL: [IslandKind; 4] = [Self::All, Self::Playable, Self::Decoration, Self::Other];

    pub fn label(self) -> &'static str {
        match self {
            Self::All => "All",
            Self::Playable => "Playable",
            Self::Decoration => "Decoration",
            Self::Other => "Campaign & NPC",
        }
    }

    pub fn admits(self, kind: IslandKind) -> bool {
        self == Self::All || kind == self
    }
}

/// How a game's shipped islands say what they are: the `IslandType` words of
/// their `RandomIsland` asset (`;`-separated there), else words in the file
/// name. Asset words are checked decoration, then other, then playable.
#[derive(Clone, Copy, Debug)]
pub struct IslandKinds {
    pub asset_decoration: &'static [&'static str],
    pub asset_other: &'static [&'static str],
    pub asset_playable: &'static [&'static str],
    /// File-name words of campaign and NPC islands. They win over
    /// `decoration_names`, so a campaign decoration counts as Other.
    pub other_names: &'static [&'static str],
    pub decoration_names: &'static [&'static str],
}

impl IslandKinds {
    /// The kind of the island at `path`; one that neither its asset nor its
    /// name places is playable.
    pub fn classify(&self, path: &str, asset_kind: Option<&str>) -> IslandKind {
        let has =
            |words: &[&str]| asset_kind.is_some_and(|a| a.split(';').any(|x| words.contains(&x)));
        if has(self.asset_decoration) {
            return IslandKind::Decoration;
        } else if has(self.asset_other) {
            return IslandKind::Other;
        } else if has(self.asset_playable) {
            return IslandKind::Playable;
        }
        let file = path.rsplit('/').next().unwrap_or(path).to_ascii_lowercase();
        let name = file
            .rsplit_once('.')
            .map_or(file.as_str(), |(stem, _)| stem);
        if self.other_names.iter().any(|w| name.contains(w)) {
            IslandKind::Other
        } else if self.decoration_names.iter().any(|w| name.contains(w)) {
            IslandKind::Decoration
        } else {
            IslandKind::Playable
        }
    }
}

/// A shipped island as the island list and New island see it.
#[derive(Clone, Debug)]
pub struct ShippedIsland {
    pub path: String,
    /// Map side, metres.
    pub world: [u32; 2],
    /// What the list groups it under and a new island built on it takes:
    /// 117's and 1800's province folder (`roman`), 2070's island folder
    /// (`normal`).
    pub region: String,
    pub kind: IslandKind,
    /// A size the game's maps place, so New island may build on it:
    /// 1800's `Normal` islands, every listed 117 island, 2070's `normal/`
    /// and `underwater/` ones. Story islands like Crown Falls (768) have
    /// sizes no map uses.
    pub pool: bool,
}

/// Every square island `install` ships with an `.a7minfo` beside it, the
/// way 117 and 1800 list theirs. With `A7mRules::islands_from_assets` an island
/// takes its kind and pool size from its `assets.xml` entry, so `assets`
/// must be the install's database; story islands such as Crown Falls and
/// Manola have no island asset and fall back to their file names.
pub fn a7m_islands<G: Islands + ?Sized>(
    game: &G,
    install: &Install,
    assets: Option<&anno_formats::assets::Assets>,
) -> Vec<ShippedIsland> {
    let rules = game.rules();
    let from_assets = game.codec().a7m().is_some_and(|a| a.islands_from_assets);
    let kinds: std::collections::HashMap<&str, &str> = assets
        .into_iter()
        .flat_map(|a| a.iter())
        .filter_map(|a| a.island.as_ref())
        .map(|i| (i.path.as_str(), i.kind.as_str()))
        .collect();
    let mut out = Vec::new();
    for id in install.search(".a7minfo", 100_000) {
        let info = install.file_path(id);
        let Some(a7m) = info
            .strip_suffix("info")
            .filter(|p| install.file(p).is_some())
        else {
            continue;
        };
        let Ok(bytes) = install.read_file(id) else {
            continue;
        };
        let Ok(info) = anno_formats::a7m::a7minfo::A7mInfo::parse(&bytes) else {
            continue;
        };
        let world = info.map_size;
        if !(64..=4096).contains(&world[0]) || world[0] != world[1] {
            continue;
        }
        let asset_kind = kinds.get(a7m).copied();
        let pool = !from_assets
            || asset_kind.is_some_and(|k| k.is_empty() || k.split(';').any(|k| k == "Normal"));
        out.push(ShippedIsland {
            path: a7m.to_owned(),
            world,
            region: rules.island_region(a7m).to_ascii_lowercase(),
            kind: game.kind_of(a7m, asset_kind),
            pool,
        });
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    out
}

/// What level-5 checks resolve an island's references against.
pub struct Refs<'a> {
    /// The install, with the project and the mods it requires mounted.
    pub install: &'a Install,
    /// Files the project holds that the mounted install doesn't serve yet:
    /// staged by the save that checks.
    pub project: &'a dyn Fn(&str) -> bool,
    /// The install's asset database; without it objects aren't checked.
    pub assets: Option<&'a anno_formats::assets::Assets>,
}

impl Refs<'_> {
    pub fn has(&self, path: &str) -> bool {
        self.install.file(path).is_some() || (self.project)(path)
    }
}

/// What an island is checked for: how its project offers it.
#[derive(Clone, Copy, Debug)]
pub struct Use<'a> {
    /// Its region, for the pieces the region's islands need.
    pub region: &'a str,
    /// A player starts on it.
    pub start: bool,
    /// Maps place it to settle (regular or start), so it needs ground to
    /// build on; decoration doesn't.
    pub playable: bool,
}

/// The level-5 checks every game shares, as far as its data tells:
/// - props whose model file and objects whose asset the install lacks, of
///   those the editor added, and a material set it can't find
/// - for an island maps settle, no ground above the sea, where the game has
///   no underwater islands
/// - a start island without a harbour coast
/// - for an island maps settle that its codec reads as an a7m, no buildable
///   ground: an empty or missing `AreaIDs` grid
///
/// Every game ships islands that name models or assets it lacks (1800's
/// pool island `colony01_m_06`, 117's `celtic_island_3rdparty_trader_02`,
/// 2070's campaign `n_s01`), and plays them, so what the island's file held
/// when loaded (`Island::loaded_models` and the loaded objects) isn't
/// counted.
pub fn broken<G: Islands + ?Sized>(
    game: &G,
    island: &Island,
    refs: &Refs,
    use_: Use,
) -> Vec<Problem> {
    let rules = game.rules();
    let doc = &island.doc;
    let mut out = Vec::new();
    let mut push = |what: String, count: usize, items: Vec<u64>| {
        out.push(Problem {
            level: Level::Broken,
            what,
            count,
            items,
            names: Default::default(),
        })
    };

    // One lookup per model file, not per prop: the continental island
    // places 45k props of a few hundred files.
    let mut missing = vec![0u64; doc.meshes.len().div_ceil(64)];
    for (k, mesh) in doc.meshes.iter().enumerate() {
        if !refs.has(mesh) && !island.loaded_models.contains(mesh) {
            missing[k / 64] |= 1 << (k % 64);
        }
    }
    let props: Vec<u64> = (doc.props.iter())
        .filter(|p| {
            let k = p.source.file as usize;
            k < doc.meshes.len() && missing[k / 64] >> (k % 64) & 1 == 1
        })
        .map(|p| p.id)
        .collect();
    if !props.is_empty() {
        push(
            "Props whose model the install lacks".into(),
            props.len(),
            props,
        );
    }
    if let Some(assets) = refs.assets {
        let objects: Vec<u64> = (doc.objects.iter())
            .filter(|o| o.is_new() && assets.get(o.guid).is_none())
            .map(|o| o.id)
            .collect();
        if !objects.is_empty() {
            let n = objects.len();
            push("Objects of assets the install lacks".into(), n, objects);
        }
    }
    if !island.material_set.is_empty()
        && island.own_set.is_none()
        && !refs.has(&island.material_set)
    {
        push(
            format!("Material set {} is missing", island.material_set),
            1,
            Vec::new(),
        );
    }

    // 1800 ships decoration islands wholly under the sea (`moderate_dst_*`,
    // `colony03_d_*`): only islands maps settle need ground above it.
    let land = doc.field.heights.iter().any(|&h| h > 0.);
    if use_.playable && !land && !rules.underwater_islands {
        push(
            "No ground above the sea: an underwater island, which this game lacks".into(),
            1,
            Vec::new(),
        );
    }

    // Either coast serves: 117 ships start islands without a start coast
    // (`roman_island_extralarge_03`, `_04`), and 1800 names none.
    let pieces = game
        .harbours()
        .map_or(&[][..], |h| h.required_pieces(use_.region));
    let coasts = [Piece::StartCoast, Piece::Coast];
    if use_.start
        && coasts.iter().any(|p| pieces.contains(p))
        && let Some(assets) = refs.assets
    {
        let has = doc.objects.iter().any(|o| {
            assets.get(o.guid).is_some_and(|a| {
                let piece = game.piece_of(&crate::harbour::template_of(assets, o.guid), &a.name);
                piece.is_some_and(|p| coasts.contains(&p))
            })
        });
        if !has {
            push("Start island without a harbour coast".into(), 1, Vec::new());
        }
    }

    if use_.playable && game.codec().a7m().is_some() {
        let areas =
            (doc.grids.as_ref()).and_then(|g| g.layer(anno_formats::a7m::grids::Kind::AreaIds));
        let buildable =
            areas.is_some_and(|l| (0..l.height).any(|y| (0..l.width).any(|x| l.get(x, y) != 0)));
        if !buildable {
            push(
                "No buildable ground: the AreaIDs grid is empty".into(),
                1,
                Vec::new(),
            );
        }
    }
    out
}

/// The game's words the editor needs to show its ids and texts as words:
/// `ls_wetlands_01`, `Mountain Slot` and `harboursystem` are ids and texts
/// as the game ships them.
#[derive(Clone, Copy, Debug)]
pub struct AssetWords {
    /// Proper names (places, provinces, factions): they keep their capital
    /// in a Title Case text whose other words go lower case, and take it
    /// wherever they fall in an id.
    pub proper: &'static [&'static str],
    /// Words the game's ids run together, spelled out; matched per word
    /// without case.
    pub joined: &'static [(&'static str, &'static str)],
}

/// A player colour as the game's assets give it, sRGB.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PlayerColor {
    pub name: &'static str,
    pub rgb: [u8; 3],
}

impl PlayerColor {
    /// The colour dye masks blend toward, linear: the engine packs it into
    /// four bits a channel and squares it back (`GetObjectDyeColor`,
    /// `FastGammaToLinear`). Rounding to the nearest step is an estimate.
    pub fn dye(&self) -> [f32; 3] {
        self.rgb.map(|c| {
            let v = (f32::from(c) * 15. / 255.).round() / 15.;
            v * v
        })
    }
}

impl Profile {
    /// Every field empty, zero or none: spell the identity and let the rest come
    /// from here (`Profile { title, short_name, .., ..Profile::BARE }`).
    pub const BARE: Profile = Profile {
        title: "",
        short_name: "",
        env_var: "",
        config_folder: "",
        steam_app_id: "",
        steam_folder: "",
        documents_folder: "",
        exe: "",
        modio_game: "",
        archive_families: &[],
        modinfo_anno: 0,
        dependencies: Dependencies::None,
    };
}

impl AssetWords {
    /// No proper names or run-together words.
    pub const NONE: AssetWords = AssetWords {
        proper: &[],
        joined: &[],
    };
}

impl IslandRules {
    /// Every field empty, off or zero: a game's islands with no region, size
    /// class, environment, cliff or sound rule, so nothing of 117's or
    /// 1800's reaches them (`IslandRules { size_classes, ..IslandRules::BARE }`).
    pub const BARE: IslandRules = IslandRules {
        prop_sets_path: "",
        island_folder: ["", ""],
        pool_anchor_guid: 0,
        decoration_templates: &[],
        pool_folder: "",
        large_islands: &[],
        regions: &[],
        default_region: "",
        asset_regions: &[],
        provinces: &[],
        roles: &[],
        size_classes: &[],
        environment_types: &[],
        marsh_tile: 0,
        environments: Environments {
            coast_rock: "",
            coast_sand: "",
            forest: "",
            marsh: "",
            meadow: "",
            mountain: "",
            mountain_top: "",
            sea: "",
            sea_close: "",
        },
        land_environments: &[],
        tree_exclusions: &[],
        regional_prop_set: false,
        harvestable_tree_preset: false,
        slots: ResourceSlots::None,
        fertility_choice: false,
        placeholder_size: false,
        import_from: &[],
        slot_yaw_step: 0.,
        cliffs: &[],
        waterfall_spots: [&[], &[], &[]],
        cliff_spot: 0,
        build_blocker: 0,
        region_words: &[],
        island_kinds: IslandKinds {
            asset_decoration: &[],
            asset_other: &[],
            asset_playable: &[],
            other_names: &[],
            decoration_names: &[],
        },
        underwater_islands: false,
        import_regions: &[],
        ground_names: GroundNames::NONE,
    };

    /// The region an island at `path` belongs to, by its folder markers.
    pub fn island_region(&self, path: &str) -> &'static str {
        (self.regions.iter())
            .find(|(marker, _)| path.contains(marker))
            .map_or(self.default_region, |&(_, region)| region)
    }

    /// `region` as the assets name it (AssociatedRegions, fertility sets).
    pub fn asset_region<'a>(&self, region: &'a str) -> &'a str {
        (self.asset_regions.iter())
            .find(|(r, _)| r.eq_ignore_ascii_case(region))
            .map_or(region, |&(_, name)| name)
    }

    /// What the game calls `region`'s province (`Latium (Roman)`), else the
    /// region as given.
    pub fn province(&self, region: &str) -> String {
        (self.provinces.iter())
            .find(|p| p.region.eq_ignore_ascii_case(region) && !p.name.is_empty())
            .map_or_else(|| region.to_owned(), |p| p.name.to_owned())
    }

    /// The province alone (`Latium`), for compact places: rows, chips and
    /// meta lines.
    pub fn province_short(&self, region: &str) -> String {
        let name = self.province(region);
        name.split(" (").next().unwrap_or(&name).to_owned()
    }

    /// The family of `region`'s shipped prop sets; none for a region
    /// without.
    pub fn prop_sets(&self, region: &str) -> Option<&'static str> {
        (self.provinces.iter())
            .find(|p| p.region.eq_ignore_ascii_case(region))
            .map(|p| p.prop_sets)
            .filter(|f| !f.is_empty())
    }

    /// Whether the island at `path` is one of the map generator's pool
    /// islands.
    pub fn is_pool(&self, path: &str) -> bool {
        !self.pool_folder.is_empty() && path.contains(self.pool_folder)
    }

    /// Whether `path` names a large island.
    pub fn is_large(&self, path: &str) -> bool {
        self.large_islands.iter().any(|w| path.contains(w))
    }

    /// The role a project stores as `value`.
    pub fn role(&self, value: &str) -> Option<&'static Role> {
        self.roles.iter().find(|r| r.value == value)
    }

    /// The size class land `extent` metres wide falls in: the first whose
    /// slots take it, else the largest.
    pub fn size_class(&self, extent: u32) -> &'static str {
        (self.size_classes.iter())
            .find(|c| extent <= c.slot)
            .or(self.size_classes.last())
            .map_or("", |c| c.name)
    }

    /// The narrowest land a start class takes: one metre past the slots of
    /// the class below the first start class.
    pub fn min_start_extent(&self) -> u32 {
        let first = self.size_classes.iter().position(|c| c.start).unwrap_or(0);
        first
            .checked_sub(1)
            .map_or(0, |below| self.size_classes[below].slot + 1)
    }

    /// The widest land any slot takes.
    pub fn max_slot(&self) -> u32 {
        self.size_classes.iter().map(|c| c.slot).max().unwrap_or(0)
    }

    /// The stored `EnvironmentGRid` value of a type; 0 (None) for names the
    /// game lacks.
    pub fn environment_value(&self, name: &str) -> u32 {
        (self.environment_types.iter())
            .position(|&(n, _)| n == name)
            .unwrap_or(0) as u32
    }

    /// The `EnvironmentGRid` values the landscape rules write.
    pub fn environment_values(&self) -> crate::island::landscape::Environments {
        let e = &self.environments;
        let code = |name| self.environment_value(name);
        crate::island::landscape::Environments {
            coast_rock: code(e.coast_rock),
            coast_sand: code(e.coast_sand),
            forest: code(e.forest),
            marsh: code(e.marsh),
            meadow: code(e.meadow),
            mountain: code(e.mountain),
            mountain_top: code(e.mountain_top),
            sea: code(e.sea),
            sea_close: code(e.sea_close),
        }
    }

    /// The classes start islands come in, for sentences: "Large or XL".
    pub fn start_classes(&self) -> String {
        let names: Vec<&str> = (self.size_classes.iter())
            .filter(|c| c.start)
            .map(|c| c.name)
            .collect();
        names.join(" or ")
    }
}

/// A region as the game presents it.
pub struct Province {
    /// The region, as `IslandRules::regions` names it (any case).
    pub region: &'static str,
    /// What the game calls its province, `Latium (Roman)`; empty shows the
    /// region.
    pub name: &'static str,
    /// The family of the region's shipped vegetation prop sets: `<family>`
    /// and `<family>_*`.
    pub prop_sets: &'static str,
}

/// A role an island can play on the game's maps.
pub struct Role {
    /// What the editor calls it.
    pub label: &'static str,
    /// What a project stores and the pool entry writes (`IslandType` on
    /// 117 and 1800).
    pub value: &'static str,
    /// It takes the map's other island slots.
    pub regular: bool,
    /// It takes a player's start slot.
    pub start: bool,
}

/// One island size class of the map generator.
pub struct SizeClass {
    pub name: &'static str,
    /// The widest land, in metres, the class's slots take
    /// (`IslandSizes/*/MaxWidth`); also the placeholder footprint on the
    /// test map preview. Land wider than every slot is the largest class.
    pub slot: u32,
    /// Shipped start islands come in this class.
    pub start: bool,
}

/// Where `modinfo.json` lists the mods a mod needs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dependencies {
    /// `Dependencies.Require`.
    Require,
    /// `ModDependencies`.
    ModDependencies,
    /// No mod loader reads them: the manifest keeps what it holds.
    None,
}

/// The `EnvironmentGRid` types the landscape rules derive, by name in
/// [`IslandRules::environment_types`]. 1800 has fewer kinds: several rules
/// share one.
pub struct Environments {
    pub coast_rock: &'static str,
    pub coast_sand: &'static str,
    pub forest: &'static str,
    pub marsh: &'static str,
    pub meadow: &'static str,
    pub mountain: &'static str,
    pub mountain_top: &'static str,
    pub sea: &'static str,
    pub sea_close: &'static str,
}

/// What a resource slot on an island is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResourceSlots {
    /// No resource slot records: the slot tool offers nothing to pick.
    None,
    /// Typed (Mountain, River, Marsh). Resources belong to the island, not
    /// the slot: a test map gives the island its deposits and keeps the
    /// slot active.
    Typed,
    /// A `Random` slot object whose asset lists the deposits that may
    /// replace it. A test map replaces it with the chosen one, or nothing.
    Random,
}

/// What an auto cliff piece is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CliffClass {
    /// `*_cliff_*_noskirt`: 13–28 m wide, 16–19 m tall wall segments whose
    /// box reaches ~12 m uphill (+Z) from the origin.
    Cliff,
    /// Big rocks (`roman_rock_02_*`, `celtic_rock_01_*`): 4–12 m, used on
    /// shorter or broken slopes.
    Boulder,
}

/// One region's auto cliffs (island::cliffs): cfg, class and its share of retail placements (the
/// census count, so the mix matches shipped islands).
pub struct CliffSet {
    pub pieces: &'static [(&'static str, CliffClass, u32)],
    /// Face height (metres of drop over ±8 m across the slope) from which a
    /// cliff wall is used instead of rocks. Celtic islands cover their
    /// steep ground mostly with big rocks (cliff walls on ~12 % of it,
    /// roman ~25 %), so their walls need taller faces.
    pub cliff_relief: f32,
    /// Spacing multiplier: shipped celtic islands put rock on 65 % of their
    /// steep ground (p10–p90 51–80 %), roman ones on 45 % (33–66 %).
    pub spacing: f32,
}

/// How a game's terrain bake blends its splat's layers (render's a7m
/// ground draws the one the game's bake matches).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Bake {
    /// Coverage under a 4x4 bicubic, cuts eased, layers blended decoded
    /// (linear) over a grey base, triplanar weights |n|^(16 max(a, 0.1))
    /// less 0.001.
    #[default]
    Smooth,
    /// Coverage bilinear over the 2x2 texels at and after, plain cuts,
    /// layers blended as stored over white and decoded with gamma 2.2
    /// after, triplanar weights |n|^(16 a).
    Plain,
}

#[cfg(test)]
mod tests {
    use super::{IslandKind, IslandKinds, PlayerColor};

    // Asset words decide before the name; without them, campaign words
    // win over decoration words. The filter's All admits every kind.
    #[test]
    fn island_kinds_read_the_asset_then_the_name() {
        let kinds = IslandKinds {
            asset_decoration: &["Deco"],
            asset_other: &["Npc"],
            asset_playable: &["Open"],
            other_names: &["story"],
            decoration_names: &["rock"],
        };
        let kind = |path, asset| kinds.classify(path, asset);
        assert_eq!(kind("a/story_rock.a7m", None), IslandKind::Other);
        assert_eq!(kind("a/b_rock_1.a7m", None), IslandKind::Decoration);
        assert_eq!(
            kind("a/story_1.a7m", Some("Open;Big")),
            IslandKind::Playable
        );
        assert_eq!(
            kind("a/plain.a7m", Some("Open;Deco")),
            IslandKind::Decoration
        );
        assert_eq!(kind("a/plain.a7m", Some("Npc")), IslandKind::Other);
        // An unknown asset word falls through to the name.
        assert_eq!(
            kind("a/b_rock.a7m", Some("Volcano")),
            IslandKind::Decoration
        );
        assert_eq!(kind("a/plain.a7m", None), IslandKind::Playable);
        assert!(IslandKind::All.admits(IslandKind::Decoration));
        assert!(!IslandKind::Playable.admits(IslandKind::Decoration));
    }

    // The engine keeps a player colour in four bits a channel and squares
    // it to linear: 117's Lapis Lazuli, 0x39 0x56 0x73, is 3, 5 and 7 of 15.
    #[test]
    fn player_colours_dye_in_twelve_bits() {
        let lapis = PlayerColor {
            name: "Lapis Lazuli",
            rgb: [0x39, 0x56, 0x73],
        };
        let want = [3f32, 5., 7.].map(|v| (v / 15.) * (v / 15.));
        assert_eq!(lapis.dye(), want);
        let white = PlayerColor {
            name: "White",
            rgb: [255; 3],
        };
        assert_eq!(white.dye(), [1.; 3]);
    }
}
